//! Bounded, deterministic query planning over an immutable CCIR graph.

use ccir_core::{Entity, EntityRef, Graph, Quantity};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::cmp::Ordering;
use std::collections::{BTreeMap, BTreeSet, VecDeque};
use thiserror::Error;

#[derive(Debug, Error)]
pub enum QueryError {
    #[error("query revision {requested} does not match graph revision {actual}")]
    RevisionMismatch { requested: String, actual: String },
    #[error("query cost {actual} exceeds budget {budget}")]
    CostExceeded { actual: u64, budget: u64 },
    #[error("invalid continuation token")]
    InvalidContinuation,
    #[error("query depth {0} exceeds the maximum of 8")]
    DepthExceeded(usize),
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct QueryRequest {
    pub revision: String,
    pub from: QueryScope,
    pub kind: Vec<String>,
    #[serde(rename = "where")]
    pub predicate: Value,
    pub traverse: Vec<TraversalStep>,
    pub select: Vec<String>,
    pub order_by: Vec<OrderBy>,
    pub limit: usize,
    pub continuation: Option<String>,
    pub cost_budget: Option<u64>,
    pub tolerance: Option<Quantity>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum QueryScope {
    #[default]
    Document,
    Selection,
    Refs(Vec<EntityRef>),
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct TraversalStep {
    pub relation: String,
    pub max_depth: usize,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct OrderBy {
    pub field: String,
    pub direction: SortDirection,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SortDirection {
    Asc,
    Desc,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct QueryResult {
    pub query_hash: String,
    pub observed_revision: String,
    pub complete: bool,
    pub matches: Vec<QueryMatch>,
    pub omitted_counts: BTreeMap<String, u64>,
    pub continuation: Option<String>,
    pub tolerances: BTreeMap<String, Value>,
    pub cost: QueryCost,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct QueryMatch {
    #[serde(rename = "ref")]
    pub ref_: EntityRef,
    pub projection: Value,
    pub evidence_path: Vec<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct QueryCost {
    pub estimated: u64,
    pub executed: u64,
    pub indexed: bool,
    pub exact_callbacks: u64,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct DependencyExplanation {
    pub reference: EntityRef,
    pub paths: Vec<Vec<DependencyHop>>,
    pub controlling_properties: Vec<Value>,
    pub cycles: Vec<Vec<EntityRef>>,
    pub health: Vec<ccir_core::Diagnostic>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct DependencyHop {
    pub from: EntityRef,
    pub relation: String,
    pub to: EntityRef,
}

pub fn execute(
    graph: &Graph,
    request: &QueryRequest,
    selection: &[EntityRef],
) -> Result<QueryResult, QueryError> {
    let actual_revision = graph.revision.id();
    if request.revision != actual_revision && request.revision != graph.revision.geometry_id() {
        return Err(QueryError::RevisionMismatch {
            requested: request.revision.clone(),
            actual: actual_revision,
        });
    }
    let depth = request
        .traverse
        .iter()
        .map(|step| step.max_depth)
        .max()
        .unwrap_or(0);
    if depth > 8 {
        return Err(QueryError::DepthExceeded(depth));
    }
    let query_hash = hash_request(request);
    let estimated = estimate_cost(graph, request);
    let budget = request.cost_budget.unwrap_or(10_000);
    if estimated > budget {
        return Err(QueryError::CostExceeded {
            actual: estimated,
            budget,
        });
    }
    let mut refs = match &request.from {
        QueryScope::Document => graph.entities.keys().cloned().collect::<Vec<_>>(),
        QueryScope::Selection => selection.to_vec(),
        QueryScope::Refs(refs) => refs.clone(),
    };
    if !request.traverse.is_empty() {
        refs = traverse(graph, refs, &request.traverse);
    }
    refs.retain(|reference| {
        graph.entity(reference).is_some_and(|entity| {
            (request.kind.is_empty() || request.kind.iter().any(|kind| kind == &entity.kind))
                && matches_predicate(entity, &request.predicate, graph)
        })
    });
    refs.sort_by(|left, right| {
        compare_entities(graph.entity(left), graph.entity(right), &request.order_by)
    });
    refs.dedup();

    let start = continuation_offset(&request.continuation, &query_hash)?;
    let limit = request.limit.clamp(1, 1_000);
    let end = (start + limit).min(refs.len());
    let matches = refs[start..end]
        .iter()
        .filter_map(|reference| {
            graph.entity(reference).map(|entity| QueryMatch {
                ref_: reference.clone(),
                projection: project(entity, &request.select),
                evidence_path: evidence_path(graph, reference, &request.from),
            })
        })
        .collect::<Vec<_>>();
    let mut omitted_counts = BTreeMap::new();
    if end < refs.len() {
        omitted_counts.insert("matches_after_limit".into(), (refs.len() - end) as u64);
    }
    let mut tolerances = BTreeMap::new();
    if let Some(tolerance) = &request.tolerance {
        tolerances.insert("linear".into(), serde_json::json!(tolerance));
    }
    tolerances.insert("angular_deg".into(), Value::from(0.1));
    Ok(QueryResult {
        query_hash: query_hash.clone(),
        observed_revision: actual_revision,
        complete: end == refs.len(),
        matches,
        omitted_counts,
        continuation: (end < refs.len()).then(|| format!("qcont:{query_hash}:{end}")),
        tolerances,
        cost: QueryCost {
            estimated,
            executed: (end - start) as u64,
            indexed: true,
            exact_callbacks: 0,
        },
    })
}

pub fn explain_dependencies(
    graph: &Graph,
    reference: &EntityRef,
    direction: &str,
    depth: usize,
) -> Option<DependencyExplanation> {
    if depth > 8 || graph.entity(reference).is_none() {
        return None;
    }
    let mut paths = Vec::new();
    let mut queue = VecDeque::from([(reference.clone(), Vec::<DependencyHop>::new(), 0usize)]);
    let mut visited = BTreeSet::new();
    let mut cycles = Vec::new();
    while let Some((current, path, current_depth)) = queue.pop_front() {
        if current_depth >= depth {
            continue;
        }
        let neighbors = if direction == "upstream" {
            graph
                .entities
                .values()
                .flat_map(|entity| {
                    entity
                        .links
                        .iter()
                        .filter(|link| link.target == current)
                        .map(|link| (entity.ref_.clone(), link.relation.clone()))
                })
                .collect::<Vec<_>>()
        } else {
            graph
                .entity(&current)
                .into_iter()
                .flat_map(|entity| {
                    entity
                        .links
                        .iter()
                        .map(|link| (link.target.clone(), link.relation.clone()))
                })
                .collect::<Vec<_>>()
        };
        for (neighbor, relation) in neighbors {
            if path
                .iter()
                .any(|hop| hop.from == neighbor || hop.to == neighbor)
            {
                cycles.push(path.iter().map(|hop| hop.from.clone()).collect());
                continue;
            }
            let hop = if direction == "upstream" {
                DependencyHop {
                    from: neighbor.clone(),
                    relation,
                    to: current.clone(),
                }
            } else {
                DependencyHop {
                    from: current.clone(),
                    relation,
                    to: neighbor.clone(),
                }
            };
            let mut extended = path.clone();
            extended.push(hop);
            let key = serde_json::to_string(&extended).unwrap_or_default();
            if visited.insert(key) {
                paths.push(extended.clone());
                queue.push_back((neighbor, extended, current_depth + 1));
            }
        }
    }
    let controlling_properties = graph
        .entity(reference)
        .and_then(|entity| entity.properties.get("controlling_properties"))
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    Some(DependencyExplanation {
        reference: reference.clone(),
        paths,
        controlling_properties,
        cycles,
        health: graph.diagnostics.clone(),
    })
}

fn estimate_cost(graph: &Graph, request: &QueryRequest) -> u64 {
    let scope = match &request.from {
        QueryScope::Document => graph.entities.len(),
        QueryScope::Selection => 8,
        QueryScope::Refs(ref_list) => ref_list.len(),
    } as u64;
    let traversal = request
        .traverse
        .iter()
        .map(|step| (step.max_depth as u64 + 1) * 4)
        .sum::<u64>();
    scope.saturating_mul(1 + traversal + request.select.len() as u64)
}

fn traverse(graph: &Graph, starts: Vec<EntityRef>, steps: &[TraversalStep]) -> Vec<EntityRef> {
    let mut current = starts;
    for step in steps {
        let mut next = Vec::new();
        for start in &current {
            let mut frontier = vec![start.clone()];
            let mut seen = BTreeSet::from([start.clone()]);
            for _ in 0..step.max_depth {
                let mut following = Vec::new();
                for reference in &frontier {
                    if let Some(entity) = graph.entity(reference) {
                        for link in &entity.links {
                            if link.relation == step.relation && seen.insert(link.target.clone()) {
                                following.push(link.target.clone());
                            }
                        }
                    }
                }
                next.extend(following.iter().cloned());
                frontier = following;
                if frontier.is_empty() {
                    break;
                }
            }
        }
        current = next;
    }
    current.sort();
    current.dedup();
    current
}

fn matches_predicate(entity: &Entity, predicate: &Value, graph: &Graph) -> bool {
    let Some(object) = predicate.as_object() else {
        return true;
    };
    if let Some(all) = object.get("all").and_then(Value::as_array) {
        return all
            .iter()
            .all(|child| matches_predicate(entity, child, graph));
    }
    if let Some(any) = object.get("any").and_then(Value::as_array) {
        return any
            .iter()
            .any(|child| matches_predicate(entity, child, graph));
    }
    if let Some(not) = object.get("not") {
        return !matches_predicate(entity, not, graph);
    }
    object
        .iter()
        .all(|(field, expected)| predicate_field(entity, field, expected, graph))
}

fn predicate_field(entity: &Entity, field: &str, expected: &Value, graph: &Graph) -> bool {
    if field == "topology" || field == "relation" {
        return expected.as_object().is_some_and(|conditions| {
            conditions.iter().all(|(relation, value)| {
                entity.links.iter().any(|link| {
                    link.relation == relation.as_str()
                        && (value.as_bool().unwrap_or(true)
                            || graph.entity(&link.target).is_some_and(|target| {
                                target.kind == value.as_str().unwrap_or_default()
                            }))
                })
            })
        });
    }
    let actual = field_value(entity, field);
    let Some(conditions) = expected.as_object() else {
        return actual == Some(expected.clone());
    };
    conditions
        .iter()
        .all(|(operator, wanted)| match operator.as_str() {
            "eq" => actual == Some(wanted.clone()),
            "ne" => actual != Some(wanted.clone()),
            "between_mm" => {
                let Some(value) = actual.as_ref().and_then(Value::as_f64) else {
                    return false;
                };
                let Some(bounds) = wanted.as_array() else {
                    return false;
                };
                bounds.len() == 2
                    && bounds[0].as_f64().is_some_and(|min| value >= min)
                    && bounds[1].as_f64().is_some_and(|max| value <= max)
            }
            "gte" => actual
                .as_ref()
                .and_then(Value::as_f64)
                .zip(wanted.as_f64())
                .is_some_and(|(a, b)| a >= b),
            "lte" => actual
                .as_ref()
                .and_then(Value::as_f64)
                .zip(wanted.as_f64())
                .is_some_and(|(a, b)| a <= b),
            "contains" => actual
                .as_ref()
                .and_then(Value::as_str)
                .is_some_and(|text| wanted.as_str().is_some_and(|part| text.contains(part))),
            "present" => actual.is_some() == wanted.as_bool().unwrap_or(false),
            _ => false,
        })
}

fn field_value(entity: &Entity, field: &str) -> Option<Value> {
    match field {
        "ref" => Some(Value::String(entity.ref_.clone())),
        "kind" => Some(Value::String(entity.kind.clone())),
        "name" => entity.name.clone().map(Value::String),
        "label" => entity.label.clone().map(Value::String),
        "identity.state" => {
            Some(serde_json::to_value(&entity.identity.state).unwrap_or(Value::Null))
        }
        "bounds.volume" => entity
            .bounds
            .as_ref()
            .map(|bounds| Value::from(bounds.volume())),
        _ => field
            .split('.')
            .try_fold(&entity.properties, |value, key| value.get(key))
            .cloned(),
    }
}

fn project(entity: &Entity, fields: &[String]) -> Value {
    if fields.is_empty() {
        return entity.summary();
    }
    let mut projection = serde_json::Map::new();
    for field in fields {
        if let Some(value) = field_value(entity, field) {
            projection.insert(field.clone(), value);
        }
    }
    Value::Object(projection)
}

fn evidence_path(_graph: &Graph, reference: &str, scope: &QueryScope) -> Vec<String> {
    match scope {
        QueryScope::Selection => vec!["selection".into(), "contains".into(), reference.into()],
        QueryScope::Refs(_) => vec!["explicit_scope".into(), reference.into()],
        QueryScope::Document => vec!["document".into(), "contains".into(), reference.into()],
    }
}

fn compare_entities(left: Option<&Entity>, right: Option<&Entity>, order: &[OrderBy]) -> Ordering {
    for item in order {
        let left_value = left.and_then(|entity| field_value(entity, &item.field));
        let right_value = right.and_then(|entity| field_value(entity, &item.field));
        let ordering = compare_values(left_value.as_ref(), right_value.as_ref());
        if ordering != Ordering::Equal {
            return if matches!(item.direction, SortDirection::Desc) {
                ordering.reverse()
            } else {
                ordering
            };
        }
    }
    left.map(|entity| (&entity.kind, &entity.name, &entity.ref_))
        .cmp(&right.map(|entity| (&entity.kind, &entity.name, &entity.ref_)))
}

fn compare_values(left: Option<&Value>, right: Option<&Value>) -> Ordering {
    match (left, right) {
        (Some(Value::String(left)), Some(Value::String(right))) => left.cmp(right),
        (Some(Value::Number(left)), Some(Value::Number(right))) => left
            .as_f64()
            .partial_cmp(&right.as_f64())
            .unwrap_or(Ordering::Equal),
        (Some(left), Some(right)) => left.to_string().cmp(&right.to_string()),
        (None, None) => Ordering::Equal,
        (None, Some(_)) => Ordering::Less,
        (Some(_), None) => Ordering::Greater,
    }
}

fn hash_request(request: &QueryRequest) -> String {
    let mut normalized = request.clone();
    normalized.continuation = None;
    let bytes = serde_json::to_vec(&normalized).unwrap_or_default();
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    format!("sha256:{}", hex::encode(hasher.finalize()))
}

fn continuation_offset(token: &Option<String>, query_hash: &str) -> Result<usize, QueryError> {
    let Some(token) = token else { return Ok(0) };
    let Some(rest) = token.strip_prefix("qcont:") else {
        return Err(QueryError::InvalidContinuation);
    };
    let Some((token_hash, offset)) = rest.rsplit_once(':') else {
        return Err(QueryError::InvalidContinuation);
    };
    if token_hash != query_hash {
        return Err(QueryError::InvalidContinuation);
    }
    offset.parse().map_err(|_| QueryError::InvalidContinuation)
}

#[cfg(test)]
mod tests {
    use super::*;
    use ccir_core::{Entity, Graph, Identity, Revision};

    fn graph() -> Graph {
        let mut graph = Graph::new("s", "d", Revision::new("e"));
        for (index, kind) in [("1", "cad.face"), ("2", "cad.face"), ("3", "cad.edge")] {
            let reference = format!("fc://s/d/{kind}/{index}@{}", graph.revision.id());
            graph.entities.insert(reference.clone(), Entity {
                ref_: reference,
                kind: kind.into(),
                revision: graph.revision.id(),
                properties: serde_json::json!({"geometry": {"surface_type": if index == "3" {"line"} else {"cylinder"}, "radius": 3.0}}),
                identity: Identity::default(),
                ..Entity::default()
            });
        }
        graph
    }

    #[test]
    fn bounded_query_returns_projection_and_continuation() {
        let graph = graph();
        let request = QueryRequest {
            revision: graph.revision.id(),
            from: QueryScope::Document,
            kind: vec!["cad.face".into()],
            predicate: serde_json::json!({"geometry.surface_type": {"eq": "cylinder"}}),
            select: vec!["ref".into(), "geometry.radius".into()],
            limit: 1,
            ..QueryRequest::default()
        };
        let result = execute(&graph, &request, &[]).unwrap();
        assert_eq!(result.matches.len(), 1);
        assert!(!result.complete);
        assert!(result.continuation.is_some());
        let next_request = QueryRequest {
            continuation: result.continuation,
            ..request
        };
        let next = execute(&graph, &next_request, &[]).unwrap();
        assert_eq!(next.matches.len(), 1);
        assert!(next.complete);
    }

    #[test]
    fn dependency_explanation_has_path() {
        let mut graph = graph();
        let refs: Vec<_> = graph.entities.keys().cloned().collect();
        graph
            .entities
            .get_mut(&refs[0])
            .unwrap()
            .links
            .push(ccir_core::Link {
                relation: "adjacent_to".into(),
                target: refs[1].clone(),
            });
        let explanation = explain_dependencies(&graph, &refs[0], "downstream", 1).unwrap();
        assert_eq!(explanation.paths.len(), 1);
    }
}
