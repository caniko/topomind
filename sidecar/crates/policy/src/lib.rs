//! Capability policy, approval proofs, idempotency, and local audit records.

use ccir_core::{ChangeSet, Operation, RiskClass};
use hmac::{Hmac, Mac};
use serde::{Deserialize, Serialize};
use sha2::Sha256;
use std::collections::BTreeSet;
use thiserror::Error;
use uuid::Uuid;

type HmacSha256 = Hmac<Sha256>;

#[derive(Debug, Error)]
pub enum PolicyError {
    #[error("capability denied: {0}")]
    CapabilityDenied(String),
    #[error("approval required")]
    ApprovalRequired,
    #[error("approval token is invalid: {0}")]
    InvalidApproval(String),
    #[error("approval token expired")]
    ExpiredApproval,
    #[error("idempotency key has already committed")]
    DuplicateIdempotency,
    #[error("workspace policy is unavailable or invalid: {0}")]
    WorkspacePolicy(String),
    #[error("serialization failed: {0}")]
    Serialization(#[from] serde_json::Error),
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PolicyProfile {
    NeverWrite,
    ApproveEach,
    ApproveHighRisk,
    WorkspacePolicy,
    Developer,
}

#[derive(Clone, Debug, Eq, PartialEq, Ord, PartialOrd, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Capability {
    ReadDocumentStructure,
    ReadExactGeometry,
    ReadAnnotationsText,
    ReadFilePaths,
    WriteSelection,
    WriteView,
    WriteModelLowRisk,
    WriteModelHighRisk,
    ExportArtifacts,
    FilesystemOutsideArtifactRoot,
    NetworkAccess,
    ArbitraryCode,
    ReadAudit,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ApprovalToken {
    pub version: String,
    pub client_id: String,
    pub session: String,
    pub document: String,
    pub base_revision: String,
    pub changeset_hash: String,
    pub preview_hash: String,
    pub risk: RiskClass,
    pub expires_at_ms: u128,
    pub nonce: String,
    pub mac: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct AuditEvent {
    pub id: String,
    pub timestamp_ms: u128,
    pub client_id: String,
    pub policy: PolicyProfile,
    pub session: String,
    pub document: String,
    pub base_revision: String,
    pub resulting_revision: Option<String>,
    pub changeset_hash: Option<String>,
    pub preview_hash: Option<String>,
    pub operation_summary: Vec<String>,
    pub decision: String,
    pub status: String,
    pub error_category: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct WorkspaceRules {
    pub schema_version: String,
    pub workspace_id: String,
    pub document_prefix: String,
    pub allowed_operations: BTreeSet<String>,
    pub max_risk: RiskClass,
    pub signature: String,
}

impl WorkspaceRules {
    pub fn signed(mut self, secret: impl AsRef<[u8]>) -> Result<Self, PolicyError> {
        self.signature.clear();
        self.signature = workspace_mac(&self, secret.as_ref())?;
        Ok(self)
    }
}

#[derive(Clone, Debug)]
pub struct Policy {
    profile: PolicyProfile,
    client_id: String,
    secret: Vec<u8>,
    used_idempotency: BTreeSet<String>,
    audit: Vec<AuditEvent>,
    workspace: Option<WorkspaceRules>,
}

impl Policy {
    pub fn new(
        profile: PolicyProfile,
        client_id: impl Into<String>,
        secret: impl AsRef<[u8]>,
    ) -> Self {
        Self {
            profile,
            client_id: client_id.into(),
            secret: secret.as_ref().to_vec(),
            used_idempotency: BTreeSet::new(),
            audit: Vec::new(),
            workspace: None,
        }
    }

    pub fn with_workspace_rules(mut self, rules: WorkspaceRules) -> Result<Self, PolicyError> {
        if rules.schema_version != "policy/1.0" {
            return Err(PolicyError::WorkspacePolicy(
                "unsupported schema version".into(),
            ));
        }
        let signature = rules.signature.clone();
        let expected = workspace_mac(&rules, &self.secret)?;
        if signature != expected {
            return Err(PolicyError::WorkspacePolicy("signature mismatch".into()));
        }
        self.workspace = Some(rules);
        Ok(self)
    }

    pub fn read_only_default() -> Self {
        Self::new(
            PolicyProfile::NeverWrite,
            "stdio-client",
            Uuid::new_v4().as_bytes(),
        )
    }

    pub fn profile(&self) -> &PolicyProfile {
        &self.profile
    }

    pub fn capabilities(&self) -> BTreeSet<Capability> {
        let mut capabilities = BTreeSet::from([
            Capability::ReadDocumentStructure,
            Capability::ReadExactGeometry,
        ]);
        match self.profile {
            PolicyProfile::NeverWrite => {}
            PolicyProfile::ApproveEach
            | PolicyProfile::ApproveHighRisk
            | PolicyProfile::WorkspacePolicy => {
                capabilities.insert(Capability::WriteSelection);
                capabilities.insert(Capability::WriteView);
                capabilities.insert(Capability::WriteModelLowRisk);
                capabilities.insert(Capability::WriteModelHighRisk);
            }
            PolicyProfile::Developer => {
                capabilities.extend([
                    Capability::WriteSelection,
                    Capability::WriteView,
                    Capability::WriteModelLowRisk,
                    Capability::WriteModelHighRisk,
                    Capability::ExportArtifacts,
                    Capability::ReadAudit,
                ]);
            }
        }
        capabilities
    }

    pub fn authorize_capability(&self, capability: Capability) -> Result<(), PolicyError> {
        self.capabilities()
            .contains(&capability)
            .then_some(())
            .ok_or_else(|| PolicyError::CapabilityDenied(format!("{capability:?}")))
    }

    pub fn authorize_changeset(&self, changeset: &ChangeSet) -> Result<(), PolicyError> {
        let risk = Self::risk(changeset);
        self.authorize_workspace(changeset, &risk)?;
        self.authorize_capability(capability_for(changeset, &risk))
    }

    pub fn risk(changeset: &ChangeSet) -> RiskClass {
        changeset
            .operations
            .iter()
            .map(Operation::risk_class)
            .max()
            .unwrap_or(RiskClass::Low)
    }

    pub fn approval_required(&self, risk: &RiskClass) -> bool {
        match self.profile {
            PolicyProfile::NeverWrite => true,
            PolicyProfile::ApproveEach => true,
            PolicyProfile::ApproveHighRisk | PolicyProfile::WorkspacePolicy => {
                matches!(risk, RiskClass::Medium | RiskClass::High)
            }
            PolicyProfile::Developer => false,
        }
    }

    pub fn issue_approval(
        &self,
        changeset: &ChangeSet,
        preview_hash: &str,
        now_ms: u128,
        ttl_ms: u128,
    ) -> Result<String, PolicyError> {
        let risk = Self::risk(changeset);
        if matches!(self.profile, PolicyProfile::NeverWrite) {
            return Err(PolicyError::CapabilityDenied(
                "model writes are disabled by never_write".into(),
            ));
        }
        self.authorize_changeset(changeset)?;
        let changeset_hash = changeset
            .hash()
            .map_err(|error| PolicyError::InvalidApproval(error.to_string()))?;
        let mut token = ApprovalToken {
            version: "approval/1.0".into(),
            client_id: self.client_id.clone(),
            session: changeset.session.clone(),
            document: changeset.document.clone(),
            base_revision: changeset.base_revision.clone(),
            changeset_hash,
            preview_hash: preview_hash.into(),
            risk,
            expires_at_ms: now_ms.saturating_add(ttl_ms),
            nonce: Uuid::new_v4().to_string(),
            mac: String::new(),
        };
        token.mac = self.mac(&token)?;
        Ok(serde_json::to_string(&token)?)
    }

    pub fn authorize_commit(
        &mut self,
        changeset: &ChangeSet,
        preview_hash: &str,
        now_ms: u128,
    ) -> Result<(), PolicyError> {
        let risk = Self::risk(changeset);
        self.authorize_changeset(changeset)?;
        let required = if matches!(risk, RiskClass::Ui) {
            false
        } else {
            self.approval_required(&risk)
        };
        if required {
            let raw = changeset
                .approval_token
                .as_deref()
                .ok_or(PolicyError::ApprovalRequired)?;
            self.verify_approval(raw, changeset, preview_hash, now_ms)?;
        }
        if !self
            .used_idempotency
            .insert(changeset.idempotency_key.clone())
        {
            return Err(PolicyError::DuplicateIdempotency);
        }
        Ok(())
    }

    pub fn record(&mut self, event: AuditEvent) {
        self.audit.push(event);
    }

    pub fn audit(&self) -> &[AuditEvent] {
        &self.audit
    }

    fn verify_approval(
        &self,
        raw: &str,
        changeset: &ChangeSet,
        preview_hash: &str,
        now_ms: u128,
    ) -> Result<(), PolicyError> {
        let token: ApprovalToken = serde_json::from_str(raw)
            .map_err(|error| PolicyError::InvalidApproval(error.to_string()))?;
        if token.expires_at_ms < now_ms {
            return Err(PolicyError::ExpiredApproval);
        }
        if token.client_id != self.client_id
            || token.session != changeset.session
            || token.document != changeset.document
            || token.base_revision != changeset.base_revision
            || token.preview_hash != preview_hash
        {
            return Err(PolicyError::InvalidApproval(
                "binding does not match the reviewed ChangeSet".into(),
            ));
        }
        let expected_hash = changeset
            .hash()
            .map_err(|error| PolicyError::InvalidApproval(error.to_string()))?;
        if token.changeset_hash != expected_hash {
            return Err(PolicyError::InvalidApproval(
                "ChangeSet hash differs".into(),
            ));
        }
        if token.mac
            != self.mac(&ApprovalToken {
                mac: String::new(),
                ..token.clone()
            })?
        {
            return Err(PolicyError::InvalidApproval("MAC mismatch".into()));
        }
        Ok(())
    }

    fn mac(&self, token: &ApprovalToken) -> Result<String, PolicyError> {
        let mut unsigned = token.clone();
        unsigned.mac.clear();
        let bytes = serde_json::to_vec(&unsigned)?;
        let mut mac = HmacSha256::new_from_slice(&self.secret)
            .map_err(|error| PolicyError::InvalidApproval(error.to_string()))?;
        mac.update(&bytes);
        Ok(hex::encode(mac.finalize().into_bytes()))
    }

    fn authorize_workspace(
        &self,
        changeset: &ChangeSet,
        risk: &RiskClass,
    ) -> Result<(), PolicyError> {
        if !matches!(self.profile, PolicyProfile::WorkspacePolicy) {
            return Ok(());
        }
        let rules = self.workspace.as_ref().ok_or_else(|| {
            PolicyError::WorkspacePolicy("a signed workspace policy is required".into())
        })?;
        if !changeset.document.starts_with(&rules.document_prefix) {
            return Err(PolicyError::WorkspacePolicy(
                "document is outside the signed workspace scope".into(),
            ));
        }
        if risk_rank(risk) > risk_rank(&rules.max_risk) {
            return Err(PolicyError::WorkspacePolicy(
                "ChangeSet risk exceeds workspace policy".into(),
            ));
        }
        for operation in &changeset.operations {
            let name = operation_name(operation);
            if !rules.allowed_operations.contains(name) {
                return Err(PolicyError::WorkspacePolicy(format!(
                    "operation is not allowed by workspace policy: {name}"
                )));
            }
        }
        Ok(())
    }
}

fn capability_for(changeset: &ChangeSet, risk: &RiskClass) -> Capability {
    if matches!(risk, RiskClass::Ui) {
        if changeset
            .operations
            .iter()
            .any(|operation| matches!(operation, Operation::SetSelection { .. }))
        {
            Capability::WriteSelection
        } else {
            Capability::WriteView
        }
    } else if matches!(risk, RiskClass::High) {
        Capability::WriteModelHighRisk
    } else {
        Capability::WriteModelLowRisk
    }
}

fn operation_name(operation: &Operation) -> &'static str {
    match operation {
        Operation::SetProperty { .. } => "set_property",
        Operation::SetExpression { .. } => "set_expression",
        Operation::SketchSetDatum { .. } => "sketch_set_datum",
        Operation::CreatePrimitive { .. } => "create_primitive",
        Operation::Boolean { .. } => "boolean",
        Operation::SetVisibility { .. } => "set_visibility",
        Operation::SetSelection { .. } => "set_selection",
        Operation::SetView { .. } => "set_view",
        Operation::CreateObject { .. } => "create_object",
        Operation::DeleteObject { .. } => "delete_object",
        Operation::Undo => "undo",
        Operation::Redo => "redo",
    }
}

fn risk_rank(risk: &RiskClass) -> u8 {
    match risk {
        RiskClass::Ui => 0,
        RiskClass::Low => 1,
        RiskClass::Medium => 2,
        RiskClass::High => 3,
    }
}

fn workspace_mac(rules: &WorkspaceRules, secret: &[u8]) -> Result<String, PolicyError> {
    let mut unsigned = rules.clone();
    unsigned.signature.clear();
    let bytes = serde_json::to_vec(&unsigned)?;
    let mut mac = HmacSha256::new_from_slice(secret)
        .map_err(|error| PolicyError::WorkspacePolicy(error.to_string()))?;
    mac.update(&bytes);
    Ok(hex::encode(mac.finalize().into_bytes()))
}

pub fn now_ms() -> u128 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |duration| duration.as_millis())
}

#[cfg(test)]
mod tests {
    use super::*;
    use ccir_core::ChangeSet;

    #[test]
    fn approval_is_bound_to_exact_preview_and_changeset() {
        let mut policy = Policy::new(PolicyProfile::ApproveEach, "client", b"secret");
        let changeset = ChangeSet::new("s", "d", "g1", Vec::new());
        let token = policy
            .issue_approval(&changeset, "preview", 100, 1_000)
            .unwrap();
        let mut approved = changeset.clone();
        approved.approval_token = Some(token);
        policy.authorize_commit(&approved, "preview", 200).unwrap();
        assert!(matches!(
            policy.authorize_commit(&approved, "preview", 200),
            Err(PolicyError::DuplicateIdempotency)
        ));
    }

    #[test]
    fn read_only_policy_denies_mutation() {
        let mut policy = Policy::read_only_default();
        let changeset = ChangeSet::new("s", "d", "g1", Vec::new());
        assert!(matches!(
            policy.authorize_commit(&changeset.clone(), "p", 0),
            Err(PolicyError::CapabilityDenied(_))
        ));
    }

    #[test]
    fn signed_workspace_policy_limits_operation_scope() {
        let secret = b"workspace-secret";
        let rules = WorkspaceRules {
            schema_version: "policy/1.0".into(),
            workspace_id: "demo".into(),
            document_prefix: "allowed/".into(),
            allowed_operations: BTreeSet::from(["set_property".into()]),
            max_risk: RiskClass::Low,
            signature: String::new(),
        }
        .signed(secret)
        .unwrap();
        let mut policy = Policy::new(PolicyProfile::WorkspacePolicy, "client", secret)
            .with_workspace_rules(rules)
            .unwrap();
        let allowed = ChangeSet::new(
            "s",
            "allowed/doc",
            "g1",
            vec![Operation::SetProperty {
                target: "fc://s/d/o@r".into(),
                property: "Length".into(),
                value: 1.into(),
            }],
        );
        assert!(policy.authorize_changeset(&allowed).is_ok());
        assert!(policy.authorize_commit(&allowed, "p", 0).is_ok());
        let denied = ChangeSet::new(
            "s",
            "other/doc",
            "g1",
            vec![Operation::SetProperty {
                target: "fc://s/d/o@r".into(),
                property: "Length".into(),
                value: 1.into(),
            }],
        );
        assert!(matches!(
            policy.authorize_changeset(&denied),
            Err(PolicyError::WorkspacePolicy(_))
        ));
        assert!(matches!(
            policy.authorize_commit(&denied, "p", 0),
            Err(PolicyError::WorkspacePolicy(_))
        ));
    }
}
