//! The language-neutral CAD Context Intermediate Representation.
//!
//! The core crate intentionally contains data and deterministic hashing only. It
//! does not know about MCP, FreeCAD, sockets, or policy. That keeps historical
//! revisions safe to read after either process has restarted.

use serde::{Deserialize, Serialize};
pub use serde_json::Value;
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet};
use thiserror::Error;

pub const CCIR_SCHEMA_VERSION: &str = "ccir/1.0";
pub const CHANGESET_SCHEMA_VERSION: &str = "changeset/1.0";

pub type EntityRef = String;

#[derive(Debug, Error)]
pub enum CcirError {
    #[error("invalid entity reference: {0}")]
    InvalidEntityRef(String),
    #[error("serialization failed: {0}")]
    Serialization(#[from] serde_json::Error),
}

pub fn make_entity_ref(
    session: &str,
    document: &str,
    path: &str,
    revision: &str,
) -> Result<EntityRef, CcirError> {
    if session.is_empty() || document.is_empty() || path.is_empty() || revision.is_empty() {
        return Err(CcirError::InvalidEntityRef(
            "session, document, path and revision are required".into(),
        ));
    }
    Ok(format!(
        "fc://session/{session}/document/{document}/{path}@{revision}"
    ))
}

pub fn sha256_json<T: Serialize>(value: &T) -> Result<String, CcirError> {
    let bytes = serde_json::to_vec(value)?;
    Ok(sha256_bytes(&bytes))
}

pub fn sha256_bytes(bytes: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    format!("sha256:{}", hex::encode(hasher.finalize()))
}

#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
pub struct Revision {
    pub geometry: u64,
    pub metadata: u64,
    pub focus: u64,
    pub view: u64,
    pub epoch: String,
}

impl Revision {
    pub fn new(epoch: impl Into<String>) -> Self {
        Self {
            epoch: epoch.into(),
            ..Self::default()
        }
    }

    pub fn id(&self) -> String {
        format!(
            "g{}.m{}.f{}.v{}.{}",
            self.geometry, self.metadata, self.focus, self.view, self.epoch
        )
    }

    pub fn geometry_id(&self) -> String {
        format!("g{}.{}", self.geometry, self.epoch)
    }

    pub fn metadata_id(&self) -> String {
        format!("m{}.{}", self.metadata, self.epoch)
    }

    pub fn advances_from(&self, previous: &Self) -> bool {
        self.epoch == previous.epoch
            && self.geometry >= previous.geometry
            && self.metadata >= previous.metadata
            && self.focus >= previous.focus
            && self.view >= previous.view
            && self != previous
    }
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Quantity {
    pub value: f64,
    pub unit: String,
}

impl Quantity {
    pub fn new(value: f64, unit: impl Into<String>) -> Self {
        Self {
            value,
            unit: unit.into(),
        }
    }
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Vec3 {
    pub x: f64,
    pub y: f64,
    pub z: f64,
}

impl Vec3 {
    pub fn new(x: f64, y: f64, z: f64) -> Self {
        Self { x, y, z }
    }

    pub fn dot(&self, other: &Self) -> f64 {
        self.x * other.x + self.y * other.y + self.z * other.z
    }

    pub fn norm(&self) -> f64 {
        self.dot(self).sqrt()
    }

    pub fn distance(&self, other: &Self) -> f64 {
        Self::new(self.x - other.x, self.y - other.y, self.z - other.z).norm()
    }

    pub fn normalized(&self) -> Option<Self> {
        let length = self.norm();
        (length > f64::EPSILON)
            .then(|| Self::new(self.x / length, self.y / length, self.z / length))
    }
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Bounds {
    pub min: Vec3,
    pub max: Vec3,
    pub frame: String,
}

impl Bounds {
    pub fn center(&self) -> Vec3 {
        Vec3::new(
            (self.min.x + self.max.x) / 2.0,
            (self.min.y + self.max.y) / 2.0,
            (self.min.z + self.max.z) / 2.0,
        )
    }

    pub fn volume(&self) -> f64 {
        (self.max.x - self.min.x).abs()
            * (self.max.y - self.min.y).abs()
            * (self.max.z - self.min.z).abs()
    }

    pub fn intersects(&self, other: &Self) -> bool {
        self.min.x <= other.max.x
            && self.max.x >= other.min.x
            && self.min.y <= other.max.y
            && self.max.y >= other.min.y
            && self.min.z <= other.max.z
            && self.max.z >= other.min.z
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Source {
    pub object: Option<String>,
    pub subelement: Option<String>,
    pub extractor: String,
    pub kernel_tolerance: Option<Quantity>,
    pub fingerprint: Option<String>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Identity {
    pub state: IdentityState,
    pub confidence: f64,
    pub signals: Vec<String>,
}

#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum IdentityState {
    #[default]
    Exact,
    MappedExact,
    MappedHeuristic,
    Ambiguous,
    Deleted,
    Stale,
}

impl IdentityState {
    pub fn write_eligible(&self) -> bool {
        matches!(self, Self::Exact | Self::MappedExact)
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct Link {
    pub relation: String,
    pub target: EntityRef,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Omission {
    pub kind: String,
    pub count: u64,
    pub reason: String,
    pub retrieve_with: Option<String>,
    pub profile: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Diagnostic {
    pub code: String,
    pub severity: Severity,
    pub message: String,
    pub entity: Option<EntityRef>,
    pub retryable: bool,
    pub evidence: Vec<EntityRef>,
}

#[derive(Clone, Debug, Default, Eq, Ord, PartialEq, PartialOrd, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Severity {
    #[default]
    Info,
    Warning,
    Error,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Entity {
    #[serde(rename = "ref")]
    pub ref_: EntityRef,
    pub kind: String,
    pub schema_version: String,
    pub revision: String,
    pub name: Option<String>,
    pub label: Option<String>,
    pub frame: Option<String>,
    pub units: BTreeMap<String, String>,
    pub source: Option<Source>,
    pub identity: Identity,
    pub properties: Value,
    pub links: Vec<Link>,
    pub omissions: Vec<Omission>,
    pub bounds: Option<Bounds>,
}

impl Entity {
    pub fn summary(&self) -> Value {
        serde_json::json!({
            "ref": self.ref_,
            "kind": self.kind,
            "revision": self.revision,
            "name": self.name,
            "label": self.label,
            "identity": self.identity,
            "bounds": self.bounds,
            "links": self.links,
            "properties": self.properties,
            "omissions": self.omissions,
        })
    }

    pub fn has_relation(&self, relation: &str, target: &str) -> bool {
        self.links
            .iter()
            .any(|link| link.relation == relation && link.target == target)
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SemanticFact {
    pub ref_: String,
    pub kind: String,
    pub classification: String,
    pub parameters: Value,
    pub evidence: Vec<EntityRef>,
    pub contradicting: Vec<EntityRef>,
    pub algorithm: String,
    pub exactness: Exactness,
    pub tolerance: Option<Quantity>,
    pub confidence: Confidence,
    pub explanation: String,
}

#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Exactness {
    #[default]
    ObservedExact,
    ObservedApproximate,
    InferredSemantic,
    Mixed,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Confidence {
    pub category: String,
    pub score: Option<f64>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Graph {
    pub schema_version: String,
    pub session: String,
    pub document: String,
    pub revision: Revision,
    pub entities: BTreeMap<EntityRef, Entity>,
    pub facts: Vec<SemanticFact>,
    pub diagnostics: Vec<Diagnostic>,
    pub metadata: BTreeMap<String, Value>,
}

impl Graph {
    pub fn new(
        session: impl Into<String>,
        document: impl Into<String>,
        revision: Revision,
    ) -> Self {
        Self {
            schema_version: CCIR_SCHEMA_VERSION.into(),
            session: session.into(),
            document: document.into(),
            revision,
            ..Self::default()
        }
    }

    pub fn fingerprint(&self) -> Result<String, CcirError> {
        sha256_json(self)
    }

    pub fn entity(&self, reference: &str) -> Option<&Entity> {
        self.entities.get(reference)
    }

    pub fn entities_of_kind<'a>(&'a self, kind: &str) -> impl Iterator<Item = &'a Entity> {
        self.entities
            .values()
            .filter(move |entity| entity.kind == kind)
    }

    pub fn related(&self, reference: &str, relation: &str) -> Vec<&Entity> {
        self.entity(reference)
            .into_iter()
            .flat_map(|entity| {
                entity
                    .links
                    .iter()
                    .filter(move |link| link.relation == relation)
                    .filter_map(|link| self.entity(&link.target))
            })
            .collect()
    }

    pub fn validate_invariants(&self) -> Vec<Diagnostic> {
        let mut diagnostics = Vec::new();
        for entity in self.entities.values() {
            for link in &entity.links {
                if !self.entities.contains_key(&link.target)
                    && !self.facts.iter().any(|fact| fact.ref_ == link.target)
                {
                    diagnostics.push(Diagnostic {
                        code: "missing_link_target".into(),
                        severity: Severity::Warning,
                        message: format!("{} points to missing {}", entity.ref_, link.target),
                        entity: Some(entity.ref_.clone()),
                        retryable: true,
                        evidence: vec![entity.ref_.clone(), link.target.clone()],
                    });
                }
            }
        }
        diagnostics
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ChangeSet {
    pub schema_version: String,
    pub request_id: String,
    pub session: String,
    pub document: String,
    pub base_revision: String,
    pub description: Option<String>,
    pub preconditions: Vec<Precondition>,
    pub operations: Vec<Operation>,
    pub validate: Vec<String>,
    pub idempotency_key: String,
    pub approval_token: Option<String>,
}

impl ChangeSet {
    pub fn new(
        session: impl Into<String>,
        document: impl Into<String>,
        base_revision: impl Into<String>,
        operations: Vec<Operation>,
    ) -> Self {
        let request_id = uuid::Uuid::new_v4().to_string();
        Self {
            schema_version: CHANGESET_SCHEMA_VERSION.into(),
            idempotency_key: request_id.clone(),
            request_id,
            session: session.into(),
            document: document.into(),
            base_revision: base_revision.into(),
            description: None,
            preconditions: Vec::new(),
            operations,
            validate: vec!["recompute".into(), "shape_validity".into()],
            approval_token: None,
        }
    }

    pub fn hash(&self) -> Result<String, CcirError> {
        let mut normalized = self.clone();
        normalized.approval_token = None;
        sha256_json(&normalized)
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Precondition {
    PropertyEquals {
        target: EntityRef,
        property: String,
        value: Value,
    },
    EntityKind {
        target: EntityRef,
        expected_kind: String,
    },
    EntityExists {
        target: EntityRef,
        identity: Option<IdentityState>,
    },
    RevisionEquals {
        revision: String,
    },
    QueryHolds {
        description: String,
        query_hash: String,
    },
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum Operation {
    SetProperty {
        target: EntityRef,
        property: String,
        value: Value,
    },
    SetExpression {
        target: EntityRef,
        property: String,
        expression: String,
    },
    SketchSetDatum {
        constraint: EntityRef,
        value: Quantity,
    },
    CreatePrimitive {
        object: String,
        primitive: String,
        parameters: BTreeMap<String, Quantity>,
    },
    Boolean {
        result: String,
        operation: String,
        left: EntityRef,
        right: EntityRef,
    },
    SetVisibility {
        target: EntityRef,
        visible: bool,
    },
    SetSelection {
        targets: Vec<EntityRef>,
        mode: String,
    },
    SetView {
        operation: String,
        parameters: Value,
    },
    CreateObject {
        object: String,
        kind: String,
        properties: Value,
    },
    DeleteObject {
        target: EntityRef,
        require_no_dependents: bool,
    },
    Undo,
    Redo,
}

impl Operation {
    pub fn risk_class(&self) -> RiskClass {
        match self {
            Self::SetSelection { .. } | Self::SetView { .. } | Self::SetVisibility { .. } => {
                RiskClass::Ui
            }
            Self::SetProperty { .. } | Self::SetExpression { .. } | Self::SketchSetDatum { .. } => {
                RiskClass::Low
            }
            Self::CreatePrimitive { .. } | Self::CreateObject { .. } => RiskClass::Medium,
            Self::Boolean { .. } | Self::DeleteObject { .. } | Self::Undo | Self::Redo => {
                RiskClass::High
            }
        }
    }
}

#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RiskClass {
    Ui,
    Low,
    Medium,
    High,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct EntityMapping {
    pub source: EntityRef,
    pub target_revision: String,
    pub state: IdentityState,
    pub candidates: Vec<MappingCandidate>,
    pub write_eligible: bool,
    pub recommended_action: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct MappingCandidate {
    pub reference: EntityRef,
    pub confidence: f64,
    pub signals: Vec<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Diff {
    pub from: Revision,
    pub to: Revision,
    pub added: Vec<EntityRef>,
    pub removed: Vec<EntityRef>,
    pub changed: Vec<EntityChange>,
    pub mappings: Vec<EntityMapping>,
    pub facts_added: Vec<SemanticFact>,
    pub facts_removed: Vec<SemanticFact>,
    pub diagnostics_changed: Vec<Diagnostic>,
    pub source: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct EntityChange {
    pub reference: EntityRef,
    pub fields: BTreeSet<String>,
    pub old: Value,
    pub new: Value,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn revision_ids_are_stable_and_monotonic() {
        let mut revision = Revision::new("epoch");
        assert_eq!(revision.id(), "g0.m0.f0.v0.epoch");
        revision.geometry = 3;
        revision.focus = 2;
        assert!(revision.advances_from(&Revision::new("epoch")));
        assert_eq!(revision.geometry_id(), "g3.epoch");
    }

    #[test]
    fn graph_hash_is_deterministic() {
        let graph = Graph::new("s", "d", Revision::new("e"));
        assert_eq!(graph.fingerprint().unwrap(), graph.fingerprint().unwrap());
    }

    #[test]
    fn write_eligibility_rejects_heuristic_mappings() {
        assert!(!IdentityState::MappedHeuristic.write_eligible());
        assert!(IdentityState::MappedExact.write_eligible());
    }
}
