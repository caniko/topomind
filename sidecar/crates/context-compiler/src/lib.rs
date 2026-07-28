//! CCIR compilation and bounded context assembly.

use bridge_dto::{BridgeEntity, BridgeSnapshot, BridgeSource, ViewState};
use ccir_core::{
    CCIR_SCHEMA_VERSION, Diagnostic, Entity, EntityRef, Graph, Omission, Revision, SemanticFact,
    Source, Value,
};
use geometry_analysis::{recognize_holes, validate_shapes};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeSet, VecDeque};
use thiserror::Error;

#[derive(Debug, Error)]
pub enum ContextError {
    #[error("entity not found: {0}")]
    EntityNotFound(String),
    #[error("context budget must be greater than zero")]
    InvalidBudget,
    #[error("serialization failed: {0}")]
    Serialization(#[from] serde_json::Error),
    #[error("revision error: {0}")]
    Revision(#[from] revision_store::RevisionError),
    #[error("geometry error: {0}")]
    Geometry(#[from] geometry_analysis::GeometryError),
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ContextRequest {
    pub detail: DetailTier,
    pub focus: FocusSelector,
    pub budget: Budget,
    pub freshness: String,
    pub include_recent_diff: bool,
    pub task_profile: Option<String>,
}

#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum DetailTier {
    #[default]
    Compact,
    Focused,
    Analytical,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct FocusSelector {
    pub explicit_refs: Vec<EntityRef>,
    pub use_selection: bool,
    pub dependency_depth: usize,
    pub topology_depth: usize,
    pub view_relevance_threshold: Option<f64>,
    pub semantic_kinds: Vec<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Budget {
    pub max_inline_bytes: usize,
    pub max_entities: usize,
}

impl Default for Budget {
    fn default() -> Self {
        Self {
            max_inline_bytes: 131_072,
            max_entities: 80,
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ContextResult {
    pub schema_version: String,
    pub observed_revision: Revision,
    pub complete: bool,
    pub context: Value,
    pub diagnostics: Vec<Diagnostic>,
    pub omissions: Vec<Omission>,
    pub suggested_calls: Vec<SuggestedCall>,
    pub cache_status: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SuggestedCall {
    pub tool: String,
    pub reason: String,
    pub arguments: Value,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct InspectRequest {
    #[serde(rename = "ref")]
    pub ref_: EntityRef,
    pub profile: String,
    pub neighborhood_depth: usize,
    pub topology_depth: usize,
    pub budget: Budget,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct EntityResult {
    #[serde(rename = "ref")]
    pub ref_: EntityRef,
    pub observed_revision: Revision,
    pub entity: Value,
    pub links: Vec<Value>,
    pub facts: Vec<SemanticFact>,
    pub complete: bool,
    pub omissions: Vec<Omission>,
}

pub fn compile_snapshot(snapshot: &BridgeSnapshot) -> Result<Graph, ContextError> {
    let mut graph = Graph::new(
        snapshot.session.clone(),
        snapshot.document.clone(),
        snapshot.revision.clone(),
    );
    graph.metadata.insert(
        "document_label".into(),
        snapshot
            .document_label
            .clone()
            .map(Value::String)
            .unwrap_or(Value::Null),
    );
    graph.metadata.insert(
        "active_workbench".into(),
        snapshot
            .active_workbench
            .clone()
            .map(Value::String)
            .unwrap_or(Value::Null),
    );
    graph
        .metadata
        .insert("gui_mode".into(), Value::Bool(snapshot.gui_mode));
    graph.metadata.insert(
        "active_object".into(),
        snapshot
            .active_object
            .clone()
            .map(Value::String)
            .unwrap_or(Value::Null),
    );
    graph.metadata.insert(
        "active_edit".into(),
        snapshot
            .active_edit
            .clone()
            .map(Value::String)
            .unwrap_or(Value::Null),
    );
    graph.metadata.extend(snapshot.metadata.clone());
    graph.metadata.insert(
        "selection".into(),
        serde_json::to_value(&snapshot.selection)?,
    );
    if let Some(view) = &snapshot.view {
        graph
            .metadata
            .insert("view".into(), serde_json::to_value(view)?);
    }
    for dto in &snapshot.entities {
        graph
            .entities
            .insert(dto.ref_.clone(), entity_from_dto(dto));
    }
    graph.diagnostics.extend(snapshot.diagnostics.clone());
    graph.diagnostics.extend(validate_shapes(&graph));
    graph.diagnostics.extend(graph.validate_invariants());
    graph.facts = recognize_holes(&graph)?;
    Ok(graph)
}

pub fn assemble(
    graph: &Graph,
    request: &ContextRequest,
    selection: &[EntityRef],
    view: Option<&ViewState>,
    recent_diff: Option<&Value>,
) -> Result<ContextResult, ContextError> {
    if request.budget.max_inline_bytes == 0 || request.budget.max_entities == 0 {
        return Err(ContextError::InvalidBudget);
    }
    let mut focus = request.focus.explicit_refs.clone();
    if request.focus.use_selection {
        focus.extend(selection.iter().cloned());
    }
    focus.retain(|reference| graph.entity(reference).is_some());
    focus.sort();
    focus.dedup();
    let candidates = ranked_entities(graph, &focus, request);
    let mut selected = Vec::new();
    let mut omissions = Vec::new();
    let mut complete = true;
    for reference in candidates {
        if selected.len() >= request.budget.max_entities {
            complete = false;
            continue;
        }
        let Some(entity) = graph.entity(&reference) else {
            continue;
        };
        selected.push(entity.summary());
    }
    if selected.len() < graph.entities.len() && request.detail != DetailTier::Analytical {
        complete = false;
        omissions.push(Omission {
            kind: "entities".into(),
            count: graph.entities.len().saturating_sub(selected.len()) as u64,
            reason: "budget".into(),
            retrieve_with: Some("cad.query_entities".into()),
            profile: Some("focused".into()),
        });
    }
    let mut context = serde_json::json!({
        "session": {
            "id": graph.session,
            "epoch": graph.revision.epoch,
            "state": if graph.diagnostics.iter().any(|diagnostic| diagnostic.severity == ccir_core::Severity::Error) { "degraded" } else { "ready_read_only" }
        },
        "document": {
            "ref": format!("fc://session/{}/document/{}@{}", graph.session, graph.document, graph.revision.id()),
            "id": graph.document,
            "label": graph.metadata.get("document_label"),
            "revision": graph.revision,
            "health": {
                "recompute": if graph.diagnostics.iter().any(|diagnostic| diagnostic.code == "recompute_error") { "error" } else { "ok" },
                "invalid_shapes": graph.diagnostics.iter().filter(|diagnostic| diagnostic.code == "invalid_shape").count()
            }
        },
        "selection": selection.iter().filter_map(|reference| graph.entity(reference).map(Entity::summary)).collect::<Vec<_>>(),
        "active_edit": graph.metadata.get("active_edit"),
        "view": view.map(view_summary),
        "resources": {
            "current": format!("freecad://session/{}/document/{}/current", graph.session, graph.document),
            "revision": format!("freecad://session/{}/document/{}/revision/{}", graph.session, graph.document, graph.revision.id()),
        },
        "entities": selected,
        "facts": graph.facts.iter().filter(|fact| focus.iter().any(|reference| fact.evidence.contains(reference))).collect::<Vec<_>>(),
        "recent_diff": recent_diff,
        "omissions": omissions,
    });
    trim_to_budget(&mut context, &request.budget, &mut omissions, &mut complete)?;
    let suggested_calls = suggestions(graph, &omissions, request);
    let diagnostics = graph.diagnostics.clone();
    Ok(ContextResult {
        schema_version: "ccir.context/1.0".into(),
        observed_revision: graph.revision.clone(),
        complete,
        context,
        diagnostics,
        omissions,
        suggested_calls,
        cache_status: "cold".into(),
    })
}

pub fn inspect_entity(
    graph: &Graph,
    request: &InspectRequest,
) -> Result<EntityResult, ContextError> {
    let entity = graph
        .entity(&request.ref_)
        .ok_or_else(|| ContextError::EntityNotFound(request.ref_.clone()))?;
    let mut omissions = Vec::new();
    let mut links = Vec::new();
    let depth = request.neighborhood_depth.min(8);
    let mut queue = VecDeque::from([(entity.ref_.clone(), 0usize)]);
    let mut visited = BTreeSet::from([entity.ref_.clone()]);
    while let Some((reference, current_depth)) = queue.pop_front() {
        let Some(current) = graph.entity(&reference) else {
            continue;
        };
        if current_depth >= depth {
            continue;
        }
        for link in &current.links {
            if let Some(target) = graph.entity(&link.target) {
                links.push(
                    serde_json::json!({"relation": link.relation, "target": target.summary()}),
                );
                if visited.insert(target.ref_.clone()) {
                    queue.push_back((target.ref_.clone(), current_depth + 1));
                }
            }
        }
    }
    if request.profile == "topology" && request.topology_depth > 0 {
        let topology = links
            .iter()
            .filter(|link| {
                link.get("relation")
                    .and_then(Value::as_str)
                    .is_some_and(|relation| {
                        matches!(
                            relation,
                            "adjacent_to" | "bounds" | "bounded_by" | "contains"
                        )
                    })
            })
            .count();
        if topology == 0 {
            omissions.push(Omission {
                kind: "topology".into(),
                count: 1,
                reason: "unsupported_or_absent".into(),
                retrieve_with: None,
                profile: Some("topology".into()),
            });
        }
    }
    let facts = graph
        .facts
        .iter()
        .filter(|fact| fact.evidence.contains(&entity.ref_))
        .cloned()
        .collect();
    let value = match request.profile.as_str() {
        "summary" | "view" => entity.summary(),
        "parametric" | "sketch_diagnostic" | "topology" | "analytical" => serde_json::json!({
            "entity": entity,
            "properties": entity.properties,
            "links": entity.links,
            "source": entity.source,
            "bounds": entity.bounds,
            "identity": entity.identity,
        }),
        _ => entity.summary(),
    };
    Ok(EntityResult {
        ref_: entity.ref_.clone(),
        observed_revision: graph.revision.clone(),
        entity: value,
        links,
        facts,
        complete: omissions.is_empty(),
        omissions,
    })
}

pub fn explain_view(graph: &Graph, view: Option<&ViewState>, selected: &[EntityRef]) -> Value {
    let Some(view) = view else {
        return serde_json::json!({"supported": false, "reason": "headless_or_view_api_unavailable"});
    };
    let mut relevance = graph
        .entities
        .values()
        .filter_map(|entity| {
            let bounds = entity.bounds.as_ref()?;
            let projected_area = bounds.volume().sqrt();
            let selected_bonus = if selected.contains(&entity.ref_) {
                1.0
            } else {
                0.0
            };
            Some(serde_json::json!({
                "ref": entity.ref_,
                "projected_area_estimate": projected_area,
                "selected": selected_bonus > 0.0,
                "components": {"projected_area": projected_area, "selection": selected_bonus}
            }))
        })
        .collect::<Vec<_>>();
    relevance.sort_by(|left, right| {
        right
            .get("projected_area_estimate")
            .and_then(Value::as_f64)
            .partial_cmp(&left.get("projected_area_estimate").and_then(Value::as_f64))
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    serde_json::json!({
        "view": view,
        "visibility_qualification": "projected_bounds",
        "relevance": relevance,
    })
}

fn entity_from_dto(dto: &BridgeEntity) -> Entity {
    Entity {
        ref_: dto.ref_.clone(),
        kind: dto.kind.clone(),
        schema_version: CCIR_SCHEMA_VERSION.into(),
        revision: dto
            .ref_
            .rsplit_once('@')
            .map(|(_, revision)| revision.to_owned())
            .unwrap_or_default(),
        name: dto.name.clone(),
        label: dto.label.clone(),
        frame: dto.frame.clone(),
        units: dto.units.clone(),
        source: dto.source.as_ref().map(source_from_dto),
        identity: dto.identity.clone(),
        properties: dto
            .shape
            .as_ref()
            .map(|shape| merge_shape(&dto.properties, shape))
            .unwrap_or_else(|| dto.properties.clone()),
        links: dto.links.clone(),
        omissions: Vec::new(),
        bounds: dto.bounds.clone(),
    }
}

fn source_from_dto(source: &BridgeSource) -> Source {
    Source {
        object: source.object.clone(),
        subelement: source.subelement.clone(),
        extractor: source.extractor.clone(),
        kernel_tolerance: source.kernel_tolerance.clone(),
        fingerprint: source.fingerprint.clone(),
    }
}

fn merge_shape(properties: &Value, shape: &bridge_dto::ShapeSummary) -> Value {
    let mut root = properties.as_object().cloned().unwrap_or_default();
    root.insert(
        "shape".into(),
        serde_json::to_value(shape).unwrap_or(Value::Null),
    );
    if let Some(geometry_type) = &shape.geometry_type {
        let geometry = root
            .entry("geometry")
            .or_insert_with(|| serde_json::json!({}));
        if let Some(object) = geometry.as_object_mut() {
            object
                .entry("surface_type")
                .or_insert_with(|| Value::String(geometry_type.clone()));
        }
    }
    Value::Object(root)
}

fn ranked_entities(graph: &Graph, focus: &[EntityRef], request: &ContextRequest) -> Vec<EntityRef> {
    let mut scores = graph
        .entities
        .values()
        .map(|entity| {
            let explicit = focus.contains(&entity.ref_);
            let diagnostic = graph
                .diagnostics
                .iter()
                .any(|diagnostic| diagnostic.entity.as_ref() == Some(&entity.ref_));
            let semantic = request
                .focus
                .semantic_kinds
                .iter()
                .any(|kind| kind == &entity.kind);
            let score = (
                explicit as u8,
                diagnostic as u8,
                semantic as u8,
                entity.kind.as_str(),
                entity.ref_.as_str(),
            );
            (score, entity.ref_.clone())
        })
        .collect::<Vec<_>>();
    scores.sort_by(|left, right| right.0.cmp(&left.0));
    scores.into_iter().map(|(_, reference)| reference).collect()
}

fn trim_to_budget(
    context: &mut Value,
    budget: &Budget,
    omissions: &mut Vec<Omission>,
    complete: &mut bool,
) -> Result<(), ContextError> {
    let encoded = serde_json::to_vec(context)?;
    if encoded.len() <= budget.max_inline_bytes {
        return Ok(());
    }
    *complete = false;
    loop {
        if serde_json::to_vec(context)?.len() <= budget.max_inline_bytes {
            break;
        }
        let can_pop = context
            .get("entities")
            .and_then(Value::as_array)
            .is_some_and(|entities| !entities.is_empty());
        if !can_pop {
            break;
        }
        context
            .get_mut("entities")
            .and_then(Value::as_array_mut)
            .expect("array checked above")
            .pop();
    }
    omissions.push(Omission {
        kind: "serialized_entity_detail".into(),
        count: 1,
        reason: "budget".into(),
        retrieve_with: Some("cad.inspect_entity".into()),
        profile: Some("analytical".into()),
    });
    Ok(())
}

fn view_summary(view: &ViewState) -> Value {
    serde_json::json!({
        "id": view.view_id,
        "projection": view.projection,
        "camera_position": view.camera_position,
        "look_direction": view.look_direction,
        "up_direction": view.up_direction,
        "viewport": view.viewport,
        "visible_objects": view.visible_objects.len(),
        "hidden_objects": view.hidden_objects.len(),
        "timestamp_ms": view.timestamp_ms,
    })
}

fn suggestions(
    graph: &Graph,
    omissions: &[Omission],
    request: &ContextRequest,
) -> Vec<SuggestedCall> {
    let mut calls = Vec::new();
    if omissions
        .iter()
        .any(|omission| omission.kind.contains("entity"))
    {
        calls.push(SuggestedCall { tool: "cad.query_entities".into(), reason: "context was bounded; query the exact entity set".into(), arguments: serde_json::json!({"revision": graph.revision.id(), "limit": request.budget.max_entities}) });
    }
    if omissions
        .iter()
        .any(|omission| omission.kind.contains("topology"))
    {
        calls.push(SuggestedCall {
            tool: "cad.inspect_entity".into(),
            reason: "topology detail was omitted".into(),
            arguments: serde_json::json!({"profile": "topology"}),
        });
    }
    if graph
        .diagnostics
        .iter()
        .any(|diagnostic| diagnostic.severity == ccir_core::Severity::Error)
    {
        calls.push(SuggestedCall {
            tool: "cad.validate".into(),
            reason: "current diagnostics include errors".into(),
            arguments: serde_json::json!({"revision": graph.revision.id()}),
        });
    }
    calls
}

#[cfg(test)]
mod tests {
    use super::*;
    use bridge_dto::{BridgeEntity, BridgeSource, ShapeSummary};
    use ccir_core::{Identity, Revision};
    use std::collections::BTreeMap;

    fn snapshot() -> BridgeSnapshot {
        let revision = Revision::new("e");
        BridgeSnapshot {
            schema_version: bridge_dto::DTO_VERSION.into(),
            session: "s".into(),
            session_epoch: "e".into(),
            document: "d".into(),
            document_label: Some("Demo".into()),
            revision: revision.clone(),
            active_workbench: Some("PartDesignWorkbench".into()),
            gui_mode: false,
            active_object: None,
            active_edit: None,
            selection: Vec::new(),
            view: None,
            entities: vec![BridgeEntity {
                ref_: format!("fc://session/s/document/d/face/1@{}", revision.id()),
                kind: "cad.face".into(),
                name: Some("Face1".into()),
                label: None,
                frame: Some("world".into()),
                units: BTreeMap::new(),
                source: Some(BridgeSource {
                    object: Some("Box".into()),
                    subelement: Some("Face1".into()),
                    extractor: "test".into(),
                    kernel_tolerance: None,
                    fingerprint: None,
                }),
                identity: Identity::default(),
                properties: serde_json::json!({"geometry": {"surface_type": "cylinder", "radius": 3.0}, "semantic": {"orientation": "interior", "openings": 2.0}}),
                links: Vec::new(),
                bounds: None,
                shape: Some(ShapeSummary {
                    geometry_type: Some("cylinder".into()),
                    ..ShapeSummary::default()
                }),
            }],
            diagnostics: Vec::new(),
            metadata: BTreeMap::new(),
        }
    }

    #[test]
    fn compiles_and_assembles_bounded_context() {
        let graph = compile_snapshot(&snapshot()).unwrap();
        assert_eq!(graph.facts.len(), 1);
        let result = assemble(
            &graph,
            &ContextRequest {
                detail: DetailTier::Compact,
                focus: FocusSelector::default(),
                budget: Budget {
                    max_inline_bytes: 10_000,
                    max_entities: 1,
                },
                freshness: "synchronized".into(),
                include_recent_diff: false,
                task_profile: None,
            },
            &[],
            None,
            None,
        )
        .unwrap();
        assert!(result.context.get("document").is_some());
        assert!(result.complete);
    }
}
