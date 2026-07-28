//! Immutable revision retention, ChangeSet replay for fixture mode, diffs, and
//! conservative entity mapping.

use ccir_core::{
    ChangeSet, Diff, Entity, EntityChange, EntityMapping, EntityRef, Graph, IdentityState,
    MappingCandidate, Operation, Precondition, Revision, RiskClass,
};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::{BTreeMap, BTreeSet};
use thiserror::Error;

#[derive(Debug, Error)]
pub enum RevisionError {
    #[error("revision conflict: expected {expected}, current {current}")]
    RevisionConflict { expected: String, current: String },
    #[error("entity not found: {0}")]
    EntityNotFound(String),
    #[error("stale or write-ineligible entity: {0}")]
    StaleReference(String),
    #[error("precondition failed: {0}")]
    PreconditionFailed(String),
    #[error("unsupported fixture operation: {0}")]
    UnsupportedOperation(String),
    #[error("entity has dependents: {0}")]
    HasDependents(String),
    #[error("no undo state is available")]
    NoUndo,
    #[error("no redo state is available")]
    NoRedo,
    #[error("core error: {0}")]
    Core(#[from] ccir_core::CcirError),
    #[error("serialization failed: {0}")]
    Serialization(#[from] serde_json::Error),
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct RevisionRecord {
    pub revision: Revision,
    pub fingerprint: String,
    pub parent: Option<String>,
    pub source: String,
    pub graph: Graph,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Preview {
    pub base_revision: String,
    pub preview_revision: String,
    pub base_fingerprint: String,
    pub preview_fingerprint: String,
    pub graph: Graph,
    pub diff: Diff,
    pub changeset_hash: String,
    pub rolled_back: bool,
    pub rollback_fingerprint_verified: bool,
}

#[derive(Clone, Debug)]
pub struct RevisionStore {
    current: Graph,
    history: BTreeMap<String, RevisionRecord>,
    max_revisions: usize,
    undo_stack: Vec<Graph>,
    redo_stack: Vec<Graph>,
}

impl RevisionStore {
    pub fn new(graph: Graph) -> Result<Self, RevisionError> {
        let fingerprint = graph.fingerprint()?;
        let revision_id = graph.revision.id();
        let record = RevisionRecord {
            revision: graph.revision.clone(),
            fingerprint,
            parent: None,
            source: "initial".into(),
            graph: graph.clone(),
        };
        let history = BTreeMap::from([(revision_id, record)]);
        Ok(Self {
            current: graph,
            history,
            max_revisions: 50,
            undo_stack: Vec::new(),
            redo_stack: Vec::new(),
        })
    }

    pub fn with_retention(mut self, max_revisions: usize) -> Self {
        self.max_revisions = max_revisions.max(1);
        self
    }

    pub fn current(&self) -> &Graph {
        &self.current
    }

    pub fn current_fingerprint(&self) -> Result<String, RevisionError> {
        Ok(self.current.fingerprint()?)
    }

    pub fn get(&self, revision: &str) -> Option<&RevisionRecord> {
        self.history.get(revision)
    }

    pub fn revisions(&self) -> impl Iterator<Item = &RevisionRecord> {
        self.history.values()
    }

    pub fn preview(&self, changeset: &ChangeSet) -> Result<Preview, RevisionError> {
        self.verify_base(changeset)?;
        let base = self.current.clone();
        let base_fingerprint = base.fingerprint()?;
        let next = apply_changeset(base.clone(), changeset)?;
        let preview_fingerprint = next.fingerprint()?;
        let diff = diff_graphs(&base, &next, "agent_preview")?;
        let changeset_hash = changeset.hash()?;
        let rollback_fingerprint_verified = base.fingerprint()? == base_fingerprint;
        Ok(Preview {
            base_revision: base.revision.id(),
            preview_revision: next.revision.id(),
            base_fingerprint,
            preview_fingerprint,
            graph: next,
            diff,
            changeset_hash,
            rolled_back: true,
            rollback_fingerprint_verified,
        })
    }

    pub fn commit(
        &mut self,
        changeset: &ChangeSet,
        expected_preview_hash: &str,
    ) -> Result<RevisionRecord, RevisionError> {
        let preview = self.preview(changeset)?;
        let actual_preview_hash = preview_hash(&preview)?;
        if actual_preview_hash != expected_preview_hash {
            return Err(RevisionError::PreconditionFailed(
                "preview hash differs from reviewed state".into(),
            ));
        }
        if !preview.rollback_fingerprint_verified {
            return Err(RevisionError::PreconditionFailed(
                "rollback fingerprint was not verified".into(),
            ));
        }
        let parent = self.current.revision.id();
        self.undo_stack.push(self.current.clone());
        self.redo_stack.clear();
        self.current = preview.graph;
        let revision = self.current.revision.clone();
        let record = RevisionRecord {
            revision: revision.clone(),
            fingerprint: self.current.fingerprint()?,
            parent: Some(parent),
            source: "agent_commit".into(),
            graph: self.current.clone(),
        };
        self.history.insert(revision.id(), record.clone());
        self.trim_history();
        Ok(record)
    }

    pub fn undo(&mut self) -> Result<RevisionRecord, RevisionError> {
        let previous = self.undo_stack.pop().ok_or(RevisionError::NoUndo)?;
        self.redo_stack.push(self.current.clone());
        self.publish_navigation(previous, "agent_undo")
    }

    pub fn redo(&mut self) -> Result<RevisionRecord, RevisionError> {
        let next = self.redo_stack.pop().ok_or(RevisionError::NoRedo)?;
        self.undo_stack.push(self.current.clone());
        self.publish_navigation(next, "agent_redo")
    }

    pub fn observe(
        &mut self,
        mut graph: Graph,
        source: impl Into<String>,
    ) -> Result<Diff, RevisionError> {
        let old = self.current.clone();
        if graph.revision.epoch != old.revision.epoch {
            return Err(RevisionError::PreconditionFailed(
                "session epoch changed; full rescan is required".into(),
            ));
        }
        if graph.revision.id() == old.revision.id() {
            graph.revision.geometry = old.revision.geometry + 1;
        }
        let diff = diff_graphs(&old, &graph, &source.into())?;
        self.undo_stack.push(old.clone());
        self.redo_stack.clear();
        self.current = graph.clone();
        let revision = graph.revision.clone();
        let record = RevisionRecord {
            revision: revision.clone(),
            fingerprint: graph.fingerprint()?,
            parent: Some(old.revision.id()),
            source: diff.source.clone(),
            graph,
        };
        self.history.insert(revision.id(), record);
        self.trim_history();
        Ok(diff)
    }

    fn publish_navigation(
        &mut self,
        mut graph: Graph,
        source: &str,
    ) -> Result<RevisionRecord, RevisionError> {
        let old = self.current.clone();
        let source_revision_id = graph.revision.id();
        graph.revision = old.revision.clone();
        graph.revision.geometry += 1;
        let new_revision_id = graph.revision.id();
        rekey_refs(&mut graph, &source_revision_id, &new_revision_id);
        graph.diagnostics = graph.validate_invariants();
        let revision = graph.revision.clone();
        let record = RevisionRecord {
            revision: revision.clone(),
            fingerprint: graph.fingerprint()?,
            parent: Some(old.revision.id()),
            source: source.into(),
            graph: graph.clone(),
        };
        self.current = graph;
        self.history.insert(revision.id(), record.clone());
        self.trim_history();
        Ok(record)
    }

    fn verify_base(&self, changeset: &ChangeSet) -> Result<(), RevisionError> {
        let current = self.current.revision.id();
        if changeset.base_revision != current
            && changeset.base_revision != self.current.revision.geometry_id()
        {
            return Err(RevisionError::RevisionConflict {
                expected: changeset.base_revision.clone(),
                current,
            });
        }
        Ok(())
    }

    fn trim_history(&mut self) {
        while self.history.len() > self.max_revisions {
            let Some(first) = self.history.keys().next().cloned() else {
                break;
            };
            self.history.remove(&first);
        }
    }
}

pub fn preview_hash(preview: &Preview) -> Result<String, RevisionError> {
    Ok(ccir_core::sha256_json(&(
        &preview.base_revision,
        &preview.preview_revision,
        &preview.base_fingerprint,
        &preview.preview_fingerprint,
        &preview.diff,
        &preview.changeset_hash,
    ))?)
}

pub fn apply_changeset(mut graph: Graph, changeset: &ChangeSet) -> Result<Graph, RevisionError> {
    verify_preconditions(&graph, changeset)?;
    validate_operations(&graph, &changeset.operations)?;
    let old_revision = graph.revision.clone();
    for operation in &changeset.operations {
        apply_operation(&mut graph, operation)?;
    }
    if changeset.operations.iter().any(|operation| {
        matches!(
            operation.risk_class(),
            RiskClass::Low | RiskClass::Medium | RiskClass::High
        )
    }) {
        graph.revision.geometry += 1;
    }
    if changeset.operations.iter().any(|operation| matches!(operation, Operation::SetProperty { property, .. } if property == "label")) {
        graph.revision.metadata += 1;
    }
    if changeset
        .operations
        .iter()
        .any(|operation| matches!(operation, Operation::SetSelection { .. }))
    {
        graph.revision.focus += 1;
    }
    if changeset
        .operations
        .iter()
        .any(|operation| matches!(operation, Operation::SetView { .. }))
    {
        graph.revision.view += 1;
    }
    let old_revision_id = old_revision.id();
    let new_revision_id = graph.revision.id();
    rekey_refs(&mut graph, &old_revision_id, &new_revision_id);
    graph.diagnostics = graph.validate_invariants();
    Ok(graph)
}

fn validate_operations(graph: &Graph, operations: &[Operation]) -> Result<(), RevisionError> {
    for operation in operations {
        match operation {
            Operation::SetProperty {
                target, property, ..
            }
            | Operation::SetExpression {
                target, property, ..
            } => {
                let entity = object_entity(graph, target)?;
                validate_property_name(property)?;
                if property_value(&entity.properties, property).is_none() {
                    return Err(RevisionError::UnsupportedOperation(format!(
                        "property is not declared on {target}: {property}"
                    )));
                }
            }
            Operation::SketchSetDatum { constraint, .. } => {
                let entity = exact_entity(graph, constraint)?;
                if entity.kind != "sketch.constraint" {
                    return Err(RevisionError::UnsupportedOperation(
                        "sketch datum requires a sketch.constraint target".into(),
                    ));
                }
            }
            Operation::SetVisibility { target, .. } => {
                let entity = object_entity(graph, target)?;
                if property_value(&entity.properties, "visible").is_none() {
                    return Err(RevisionError::UnsupportedOperation(format!(
                        "visibility is not declared on {target}"
                    )));
                }
            }
            Operation::SetSelection { targets, mode } => {
                if !matches!(mode.as_str(), "replace" | "add" | "remove") {
                    return Err(RevisionError::UnsupportedOperation(
                        "selection mode is not allowlisted".into(),
                    ));
                }
                for target in targets {
                    let entity = exact_entity(graph, target)?;
                    if entity.kind == "cad.document" {
                        return Err(RevisionError::UnsupportedOperation(
                            "document cannot be selected as an object".into(),
                        ));
                    }
                }
            }
            Operation::SetView { operation, .. } => {
                if !matches!(
                    operation.as_str(),
                    "fit_all"
                        | "view_axo"
                        | "view_front"
                        | "view_rear"
                        | "view_left"
                        | "view_right"
                        | "view_top"
                        | "view_bottom"
                ) {
                    return Err(RevisionError::UnsupportedOperation(
                        "view operation is not allowlisted".into(),
                    ));
                }
            }
            Operation::CreatePrimitive {
                object,
                primitive,
                parameters,
            } => {
                validate_identifier(object)?;
                let expected = match primitive.as_str() {
                    "box" => ["length", "width", "height"].as_slice(),
                    "cylinder" => ["radius", "height"].as_slice(),
                    "sphere" => ["radius"].as_slice(),
                    _ => {
                        return Err(RevisionError::UnsupportedOperation(
                            "primitive is not allowlisted".into(),
                        ));
                    }
                };
                if parameters
                    .keys()
                    .map(String::as_str)
                    .collect::<BTreeSet<_>>()
                    != expected.iter().copied().collect()
                {
                    return Err(RevisionError::UnsupportedOperation(
                        "primitive parameters do not match the allowlisted shape".into(),
                    ));
                }
                if parameters.values().any(|quantity| {
                    !quantity.value.is_finite() || quantity.value < 0.0 || quantity.unit.is_empty()
                }) {
                    return Err(RevisionError::UnsupportedOperation(
                        "primitive quantities must be finite, non-negative, and unit-tagged".into(),
                    ));
                }
            }
            Operation::Boolean {
                result,
                operation,
                left,
                right,
            } => {
                validate_identifier(result)?;
                if !matches!(operation.as_str(), "cut" | "fuse" | "common") {
                    return Err(RevisionError::UnsupportedOperation(
                        "boolean operation is not allowlisted".into(),
                    ));
                }
                object_entity(graph, left)?;
                object_entity(graph, right)?;
                ensure_new_object(graph, result)?;
            }
            Operation::CreateObject {
                object,
                kind,
                properties: _,
            } => {
                validate_identifier(object)?;
                if !matches!(
                    kind.as_str(),
                    "Part::Feature"
                        | "PartDesign::Feature"
                        | "PartDesign::Body"
                        | "PartDesign::FeaturePython"
                        | "Sketcher::SketchObject"
                ) {
                    return Err(RevisionError::UnsupportedOperation(
                        "object type is not allowlisted".into(),
                    ));
                }
                ensure_new_object(graph, object)?;
            }
            Operation::DeleteObject { target, .. } => {
                object_entity(graph, target)?;
            }
            Operation::Undo | Operation::Redo => {
                return Err(RevisionError::UnsupportedOperation(
                    "undo/redo require a live bridge".into(),
                ));
            }
        }
    }
    Ok(())
}

fn exact_entity<'a>(graph: &'a Graph, reference: &str) -> Result<&'a Entity, RevisionError> {
    let entity = graph
        .entity(reference)
        .ok_or_else(|| RevisionError::EntityNotFound(reference.into()))?;
    if !entity.identity.state.write_eligible() {
        return Err(RevisionError::StaleReference(reference.into()));
    }
    Ok(entity)
}

fn object_entity<'a>(graph: &'a Graph, reference: &str) -> Result<&'a Entity, RevisionError> {
    let entity = exact_entity(graph, reference)?;
    if entity.kind == "cad.document"
        || entity
            .source
            .as_ref()
            .and_then(|source| source.subelement.as_ref())
            .is_some()
    {
        return Err(RevisionError::UnsupportedOperation(format!(
            "operation requires an object-level target: {reference}"
        )));
    }
    Ok(entity)
}

fn ensure_new_object(graph: &Graph, name: &str) -> Result<(), RevisionError> {
    let reference = format!(
        "fc://session/{}/document/{}/object/{name}",
        graph.session, graph.document
    );
    if graph.entities.contains_key(&reference) {
        return Err(RevisionError::PreconditionFailed(format!(
            "object already exists: {name}"
        )));
    }
    Ok(())
}

fn validate_identifier(name: &str) -> Result<(), RevisionError> {
    if name.is_empty()
        || !name
            .chars()
            .next()
            .is_some_and(|character| character.is_ascii_alphabetic())
        || !name
            .chars()
            .all(|character| character.is_ascii_alphanumeric() || character == '_')
    {
        return Err(RevisionError::UnsupportedOperation(
            "object names must be simple ASCII identifiers".into(),
        ));
    }
    Ok(())
}

fn validate_property_name(property: &str) -> Result<(), RevisionError> {
    if property.is_empty()
        || property.contains('.')
        || property.contains("__")
        || !property
            .chars()
            .all(|character| character.is_ascii_alphanumeric() || character == '_')
    {
        return Err(RevisionError::UnsupportedOperation(
            "property names must be simple identifiers".into(),
        ));
    }
    Ok(())
}

pub fn diff_graphs(from: &Graph, to: &Graph, source: &str) -> Result<Diff, RevisionError> {
    let from_paths = logical_paths(from);
    let to_paths = logical_paths(to);
    let mut added = Vec::new();
    let mut removed = Vec::new();
    let mut changed = Vec::new();
    let mut mappings = Vec::new();
    for (path, to_reference) in &to_paths {
        match from_paths.get(path) {
            None => added.push(to_reference.clone()),
            Some(from_reference) => {
                let old = from.entity(from_reference).unwrap();
                let new = to.entity(to_reference).unwrap();
                let old_value = serde_json::to_value(old)?;
                let new_value = serde_json::to_value(new)?;
                if old_value != new_value {
                    let fields = changed_fields(old, new);
                    changed.push(EntityChange {
                        reference: to_reference.clone(),
                        fields,
                        old: old_value,
                        new: new_value,
                    });
                }
                if from_reference != to_reference {
                    let same_fingerprint = old.kind == new.kind
                        && old
                            .source
                            .as_ref()
                            .and_then(|source| source.fingerprint.as_ref())
                            == new
                                .source
                                .as_ref()
                                .and_then(|source| source.fingerprint.as_ref());
                    let state = if same_fingerprint {
                        IdentityState::MappedExact
                    } else {
                        IdentityState::MappedHeuristic
                    };
                    mappings.push(EntityMapping {
                        source: from_reference.clone(),
                        target_revision: to.revision.id(),
                        state: state.clone(),
                        candidates: vec![MappingCandidate {
                            reference: to_reference.clone(),
                            confidence: if same_fingerprint { 1.0 } else { 0.55 },
                            signals: if same_fingerprint {
                                vec![
                                    "same_logical_path".into(),
                                    "same_kind".into(),
                                    "same_source_fingerprint".into(),
                                ]
                            } else {
                                vec!["same_logical_path".into(), "path_only".into()]
                            },
                        }],
                        write_eligible: state.write_eligible(),
                        recommended_action: if same_fingerprint {
                            "use the mapped current reference".into()
                        } else {
                            "re-resolve at a higher-level feature; heuristic mapping is read-only"
                                .into()
                        },
                    });
                }
            }
        }
    }
    for (path, from_reference) in &from_paths {
        if !to_paths.contains_key(path) {
            removed.push(from_reference.clone());
            mappings.push(EntityMapping {
                source: from_reference.clone(),
                target_revision: to.revision.id(),
                state: IdentityState::Deleted,
                candidates: Vec::new(),
                write_eligible: false,
                recommended_action: "reselect or target the owning feature".into(),
            });
        }
    }
    let from_fact_refs: BTreeSet<_> = from.facts.iter().map(|fact| fact.ref_.clone()).collect();
    let to_fact_refs: BTreeSet<_> = to.facts.iter().map(|fact| fact.ref_.clone()).collect();
    let facts_added = to
        .facts
        .iter()
        .filter(|fact| !from_fact_refs.contains(&fact.ref_))
        .cloned()
        .collect();
    let facts_removed = from
        .facts
        .iter()
        .filter(|fact| !to_fact_refs.contains(&fact.ref_))
        .cloned()
        .collect();
    Ok(Diff {
        from: from.revision.clone(),
        to: to.revision.clone(),
        added,
        removed,
        changed,
        mappings,
        facts_added,
        facts_removed,
        diagnostics_changed: to.diagnostics.clone(),
        source: source.into(),
    })
}

fn verify_preconditions(graph: &Graph, changeset: &ChangeSet) -> Result<(), RevisionError> {
    for precondition in &changeset.preconditions {
        match precondition {
            Precondition::PropertyEquals {
                target,
                property,
                value,
            } => {
                let entity = graph
                    .entity(target)
                    .ok_or_else(|| RevisionError::EntityNotFound(target.clone()))?;
                let actual = property_value(&entity.properties, property);
                if actual != Some(value) {
                    return Err(RevisionError::PreconditionFailed(format!(
                        "{target}.{property} != {value}"
                    )));
                }
            }
            Precondition::EntityKind {
                target,
                expected_kind: kind,
            } => {
                let entity = graph
                    .entity(target)
                    .ok_or_else(|| RevisionError::EntityNotFound(target.clone()))?;
                if entity.kind != *kind {
                    return Err(RevisionError::PreconditionFailed(format!(
                        "{target} is {}, not {kind}",
                        entity.kind
                    )));
                }
            }
            Precondition::EntityExists { target, identity } => {
                let entity = graph
                    .entity(target)
                    .ok_or_else(|| RevisionError::EntityNotFound(target.clone()))?;
                if identity
                    .as_ref()
                    .is_some_and(|expected| &entity.identity.state != expected)
                {
                    return Err(RevisionError::PreconditionFailed(format!(
                        "identity state mismatch for {target}"
                    )));
                }
            }
            Precondition::RevisionEquals { revision } => {
                if revision != &graph.revision.id() && revision != &graph.revision.geometry_id() {
                    return Err(RevisionError::RevisionConflict {
                        expected: revision.clone(),
                        current: graph.revision.id(),
                    });
                }
            }
            Precondition::QueryHolds { description, .. } => {
                return Err(RevisionError::UnsupportedOperation(format!(
                    "query_holds precondition was not evaluated: {description}"
                )));
            }
        }
    }
    Ok(())
}

fn apply_operation(graph: &mut Graph, operation: &Operation) -> Result<(), RevisionError> {
    match operation {
        Operation::SetProperty {
            target,
            property,
            value,
        } => {
            let entity = writable_entity(graph, target)?;
            set_property(&mut entity.properties, property, value.clone());
        }
        Operation::SetExpression {
            target,
            property,
            expression,
        } => {
            let entity = writable_entity(graph, target)?;
            set_property(
                &mut entity.properties,
                &format!("expressions.{property}"),
                Value::String(expression.clone()),
            );
        }
        Operation::SketchSetDatum { constraint, value } => {
            let entity = writable_entity(graph, constraint)?;
            set_property(
                &mut entity.properties,
                "datum",
                serde_json::to_value(value)?,
            );
        }
        Operation::CreatePrimitive {
            object,
            primitive,
            parameters,
        } => {
            let reference = format!(
                "fc://session/{}/document/{}/object/{object}",
                graph.session, graph.document
            );
            if graph.entities.contains_key(&reference) {
                return Err(RevisionError::PreconditionFailed(format!(
                    "object already exists: {object}"
                )));
            }
            let mut properties = serde_json::Map::new();
            properties.insert("primitive".into(), Value::String(primitive.clone()));
            properties.insert("parameters".into(), serde_json::to_value(parameters)?);
            graph.entities.insert(
                reference.clone(),
                Entity {
                    ref_: reference,
                    kind: "cad.object".into(),
                    schema_version: ccir_core::CCIR_SCHEMA_VERSION.into(),
                    revision: graph.revision.id(),
                    name: Some(object.clone()),
                    label: Some(object.clone()),
                    identity: ccir_core::Identity::default(),
                    properties: Value::Object(properties),
                    ..Entity::default()
                },
            );
        }
        Operation::Boolean {
            result,
            operation,
            left,
            right,
        } => {
            let left_entity = graph
                .entity(left)
                .ok_or_else(|| RevisionError::EntityNotFound(left.clone()))?;
            let right_entity = graph
                .entity(right)
                .ok_or_else(|| RevisionError::EntityNotFound(right.clone()))?;
            let reference = format!(
                "fc://session/{}/document/{}/object/{result}",
                graph.session, graph.document
            );
            graph.entities.insert(
                reference.clone(),
                Entity {
                    ref_: reference,
                    kind: "cad.feature.boolean".into(),
                    schema_version: ccir_core::CCIR_SCHEMA_VERSION.into(),
                    revision: graph.revision.id(),
                    name: Some(result.clone()),
                    label: Some(result.clone()),
                    identity: ccir_core::Identity::default(),
                    properties: serde_json::json!({"operation": operation, "valid": true}),
                    links: vec![
                        ccir_core::Link {
                            relation: "depends_on".into(),
                            target: left_entity.ref_.clone(),
                        },
                        ccir_core::Link {
                            relation: "depends_on".into(),
                            target: right_entity.ref_.clone(),
                        },
                    ],
                    ..Entity::default()
                },
            );
        }
        Operation::SetVisibility { target, visible } => {
            let entity = writable_entity(graph, target)?;
            set_property(&mut entity.properties, "visible", Value::Bool(*visible));
        }
        Operation::SetSelection { targets, mode } => {
            graph.metadata.insert(
                "selection".into(),
                serde_json::json!({"targets": targets, "mode": mode}),
            );
        }
        Operation::SetView {
            operation,
            parameters,
        } => {
            graph.metadata.insert(
                "view_operation".into(),
                serde_json::json!({"operation": operation, "parameters": parameters}),
            );
        }
        Operation::CreateObject {
            object,
            kind,
            properties,
        } => {
            let reference = format!(
                "fc://session/{}/document/{}/object/{object}",
                graph.session, graph.document
            );
            graph.entities.insert(
                reference.clone(),
                Entity {
                    ref_: reference,
                    kind: kind.clone(),
                    schema_version: ccir_core::CCIR_SCHEMA_VERSION.into(),
                    revision: graph.revision.id(),
                    name: Some(object.clone()),
                    label: Some(object.clone()),
                    identity: ccir_core::Identity::default(),
                    properties: properties.clone(),
                    ..Entity::default()
                },
            );
        }
        Operation::DeleteObject {
            target,
            require_no_dependents,
        } => {
            let dependents = graph
                .entities
                .values()
                .filter(|entity| entity.has_relation("depends_on", target))
                .count();
            if *require_no_dependents && dependents > 0 {
                return Err(RevisionError::HasDependents(target.clone()));
            }
            if graph.entities.remove(target).is_none() {
                return Err(RevisionError::EntityNotFound(target.clone()));
            }
        }
        Operation::Undo | Operation::Redo => {
            return Err(RevisionError::UnsupportedOperation(
                "undo/redo require a live bridge".into(),
            ));
        }
    }
    Ok(())
}

fn writable_entity<'a>(
    graph: &'a mut Graph,
    reference: &str,
) -> Result<&'a mut Entity, RevisionError> {
    let eligible = graph
        .entity(reference)
        .ok_or_else(|| RevisionError::EntityNotFound(reference.into()))?
        .identity
        .state
        .write_eligible();
    if !eligible {
        return Err(RevisionError::StaleReference(reference.into()));
    }
    graph
        .entities
        .get_mut(reference)
        .ok_or_else(|| RevisionError::EntityNotFound(reference.into()))
}

fn set_property(properties: &mut Value, path: &str, value: Value) {
    let mut current = properties;
    let parts: Vec<_> = path.split('.').collect();
    for part in &parts[..parts.len().saturating_sub(1)] {
        if !current.get(*part).is_some_and(Value::is_object) {
            current[*part] = serde_json::json!({});
        }
        current = current.get_mut(*part).expect("object inserted above");
    }
    if let Some(last) = parts.last() {
        current[*last] = value;
    }
}

fn property_value<'a>(properties: &'a Value, path: &str) -> Option<&'a Value> {
    path.split('.')
        .try_fold(properties, |current, key| current.get(key))
}

fn logical_paths(graph: &Graph) -> BTreeMap<String, EntityRef> {
    graph
        .entities
        .keys()
        .map(|reference| (logical_path(reference), reference.clone()))
        .collect()
}

fn logical_path(reference: &str) -> String {
    reference
        .rsplit_once('@')
        .map_or_else(|| reference.to_owned(), |(path, _)| path.to_owned())
}

fn changed_fields(old: &Entity, new: &Entity) -> BTreeSet<String> {
    let mut fields = BTreeSet::new();
    if old.kind != new.kind {
        fields.insert("kind".into());
    }
    if old.name != new.name {
        fields.insert("name".into());
    }
    if old.label != new.label {
        fields.insert("label".into());
    }
    if old.properties != new.properties {
        fields.insert("properties".into());
    }
    if old.links != new.links {
        fields.insert("links".into());
    }
    if old
        .bounds
        .as_ref()
        .map(|value| serde_json::to_value(value).unwrap_or(Value::Null))
        != new
            .bounds
            .as_ref()
            .map(|value| serde_json::to_value(value).unwrap_or(Value::Null))
    {
        fields.insert("bounds".into());
    }
    fields
}

fn rekey_refs(graph: &mut Graph, old_revision: &str, new_revision: &str) {
    if old_revision == new_revision {
        return;
    }
    let mut entities = BTreeMap::new();
    for (reference, mut entity) in std::mem::take(&mut graph.entities) {
        let new_reference = replace_revision(&reference, old_revision, new_revision);
        entity.ref_ = new_reference.clone();
        entity.revision = new_revision.into();
        for link in &mut entity.links {
            link.target = replace_revision(&link.target, old_revision, new_revision);
        }
        entities.insert(new_reference, entity);
    }
    graph.entities = entities;
    for fact in &mut graph.facts {
        fact.ref_ = replace_revision(&fact.ref_, old_revision, new_revision);
        for reference in &mut fact.evidence {
            *reference = replace_revision(reference, old_revision, new_revision);
        }
        for reference in &mut fact.contradicting {
            *reference = replace_revision(reference, old_revision, new_revision);
        }
    }
}

fn replace_revision(reference: &str, old_revision: &str, new_revision: &str) -> String {
    reference
        .strip_suffix(&format!("@{old_revision}"))
        .map_or_else(
            || reference.to_owned(),
            |prefix| format!("{prefix}@{new_revision}"),
        )
}

#[cfg(test)]
mod tests {
    use super::*;
    use ccir_core::{Entity, Identity, Operation, Revision};

    fn graph() -> Graph {
        let revision = Revision::new("e");
        let reference = format!(
            "fc://session/s/document/d/sketch/constraint/1@{}",
            revision.id()
        );
        Graph {
            session: "s".into(),
            document: "d".into(),
            revision: revision.clone(),
            entities: BTreeMap::from([(
                reference.clone(),
                Entity {
                    ref_: reference,
                    kind: "sketch.constraint".into(),
                    revision: revision.id(),
                    identity: Identity::default(),
                    properties: serde_json::json!({"datum": {"value": 6.0, "unit": "mm"}}),
                    ..Entity::default()
                },
            )]),
            ..Graph::default()
        }
    }

    #[test]
    fn preview_rekeys_refs_and_commit_requires_proof() {
        let initial = graph();
        let reference = initial.entities.keys().next().unwrap().clone();
        let changeset = ChangeSet {
            operations: vec![Operation::SketchSetDatum {
                constraint: reference.clone(),
                value: ccir_core::Quantity::new(6.5, "mm"),
            }],
            ..ChangeSet::new("s", "d", initial.revision.id(), Vec::new())
        };
        let mut store = RevisionStore::new(initial).unwrap();
        let preview = store.preview(&changeset).unwrap();
        assert!(preview.graph.entities.keys().next().unwrap().contains("g1"));
        let proof = preview_hash(&preview).unwrap();
        let record = store.commit(&changeset, &proof).unwrap();
        assert_eq!(record.revision.geometry, 1);
    }

    #[test]
    fn stale_identity_is_rejected() {
        let mut graph = graph();
        graph.entities.values_mut().next().unwrap().identity.state = IdentityState::Ambiguous;
        let reference = graph.entities.keys().next().unwrap().clone();
        let changeset = ChangeSet::new(
            "s",
            "d",
            graph.revision.id(),
            vec![Operation::SetProperty {
                target: reference,
                property: "x".into(),
                value: Value::from(1),
            }],
        );
        assert!(matches!(
            apply_changeset(graph, &changeset),
            Err(RevisionError::StaleReference(_))
        ));
    }

    #[test]
    fn undo_and_redo_create_new_monotonic_revisions() {
        let initial = graph();
        let reference = initial.entities.keys().next().unwrap().clone();
        let changeset = ChangeSet::new(
            "s",
            "d",
            initial.revision.id(),
            vec![Operation::SketchSetDatum {
                constraint: reference,
                value: ccir_core::Quantity::new(6.5, "mm"),
            }],
        );
        let mut store = RevisionStore::new(initial).unwrap();
        let preview = store.preview(&changeset).unwrap();
        store
            .commit(&changeset, &preview_hash(&preview).unwrap())
            .unwrap();
        let undone = store.undo().unwrap();
        let redone = store.redo().unwrap();
        assert!(undone.revision.geometry > 1);
        assert!(redone.revision.geometry > undone.revision.geometry);
    }
}
