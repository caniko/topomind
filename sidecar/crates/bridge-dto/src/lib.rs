//! Schema-versioned DTOs exchanged with the in-process FreeCAD bridge.

use ccir_core::{Bounds, Diagnostic, EntityRef, Identity, Link, Quantity, Revision, Value};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::io::{Read, Write};
use thiserror::Error;

use hmac::{Hmac, Mac};
use sha2::Sha256;

pub const IPC_VERSION: &str = "ipc/1.0";
pub const DTO_VERSION: &str = "bridge-dto/1.0";
pub const MAX_FRAME: usize = 4 * 1024 * 1024;

type HmacSha256 = Hmac<Sha256>;

#[derive(Debug, Error)]
pub enum WireError {
    #[error("wire I/O failed: {0}")]
    Io(#[from] std::io::Error),
    #[error("wire serialization failed: {0}")]
    Serialization(#[from] serde_json::Error),
    #[error("frame is too large: {0} bytes")]
    FrameTooLarge(usize),
    #[error("pairing proof is invalid")]
    InvalidProof,
}

pub fn write_frame<T: Serialize>(stream: &mut impl Write, value: &T) -> Result<(), WireError> {
    let bytes = serde_json::to_vec(value)?;
    if bytes.len() > MAX_FRAME {
        return Err(WireError::FrameTooLarge(bytes.len()));
    }
    stream.write_all(&(bytes.len() as u32).to_be_bytes())?;
    stream.write_all(&bytes)?;
    stream.flush()?;
    Ok(())
}

pub fn read_frame<T: for<'de> Deserialize<'de>>(stream: &mut impl Read) -> Result<T, WireError> {
    let mut length = [0_u8; 4];
    stream.read_exact(&mut length)?;
    let size = u32::from_be_bytes(length) as usize;
    if size > MAX_FRAME {
        return Err(WireError::FrameTooLarge(size));
    }
    let mut bytes = vec![0_u8; size];
    stream.read_exact(&mut bytes)?;
    Ok(serde_json::from_slice(&bytes)?)
}

pub fn pairing_proof(secret: &[u8], nonce: &str) -> Result<String, WireError> {
    let mut mac = HmacSha256::new_from_slice(secret).map_err(|_| WireError::InvalidProof)?;
    mac.update(nonce.as_bytes());
    Ok(hex::encode(mac.finalize().into_bytes()))
}

pub fn verify_pairing(secret: &[u8], nonce: &str, proof: &str) -> bool {
    let Ok(expected) = pairing_proof(secret, nonce) else {
        return false;
    };
    expected.len() == proof.len()
        && expected
            .as_bytes()
            .iter()
            .zip(proof.as_bytes())
            .fold(0_u8, |difference, (left, right)| {
                difference | (left ^ right)
            })
            == 0
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct IpcRequest {
    pub protocol_version: String,
    pub message: String,
    pub request_id: String,
    pub session_epoch: String,
    pub deadline_ms: u64,
    pub payload: Value,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct IpcResponse {
    pub protocol_version: String,
    pub message: String,
    pub request_id: String,
    pub status: ResponseStatus,
    pub session_epoch: String,
    pub payload: Option<Value>,
    pub diagnostics: Vec<Diagnostic>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ResponseStatus {
    Ok,
    Error,
    Cancelled,
    Busy,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct BridgeHello {
    pub message: String,
    pub ipc_versions: Vec<String>,
    pub dto_versions: Vec<String>,
    pub freecad: FreeCadInfo,
    pub extractors: Vec<String>,
    pub operations: Vec<String>,
    pub transaction_safety: BTreeMap<String, String>,
    pub limits: BridgeLimits,
    pub capabilities: Vec<String>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct FreeCadInfo {
    pub version: String,
    pub python: String,
    pub occt: String,
    pub mode: String,
    pub active_workbench: Option<String>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct BridgeLimits {
    pub max_control_message_bytes: usize,
    pub max_pending_requests: usize,
    pub max_entities_per_snapshot: usize,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct BridgeSnapshot {
    pub schema_version: String,
    pub session: String,
    pub session_epoch: String,
    pub document: String,
    pub document_label: Option<String>,
    pub revision: Revision,
    pub active_workbench: Option<String>,
    pub gui_mode: bool,
    pub active_object: Option<EntityRef>,
    pub active_edit: Option<EntityRef>,
    pub selection: Vec<SelectionItem>,
    pub view: Option<ViewState>,
    pub entities: Vec<BridgeEntity>,
    pub diagnostics: Vec<Diagnostic>,
    pub metadata: BTreeMap<String, Value>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct BridgeEntity {
    #[serde(rename = "ref")]
    pub ref_: EntityRef,
    pub kind: String,
    pub name: Option<String>,
    pub label: Option<String>,
    pub frame: Option<String>,
    pub units: BTreeMap<String, String>,
    pub source: Option<BridgeSource>,
    pub identity: Identity,
    pub properties: Value,
    pub links: Vec<Link>,
    pub bounds: Option<Bounds>,
    pub shape: Option<ShapeSummary>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct BridgeSource {
    pub object: Option<String>,
    pub subelement: Option<String>,
    pub extractor: String,
    pub kernel_tolerance: Option<Quantity>,
    pub fingerprint: Option<String>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct ShapeSummary {
    pub shape_type: String,
    pub topology: BTreeMap<String, u64>,
    pub geometry_type: Option<String>,
    pub exact: bool,
    pub properties: Value,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SelectionItem {
    #[serde(rename = "ref")]
    pub ref_: EntityRef,
    pub selection_index: usize,
    pub subelement: Option<String>,
    pub source: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ViewState {
    pub view_id: String,
    pub projection: String,
    pub camera_position: Option<ccir_core::Vec3>,
    pub camera_orientation: Option<Value>,
    pub look_direction: Option<ccir_core::Vec3>,
    pub up_direction: Option<ccir_core::Vec3>,
    pub target: Option<ccir_core::Vec3>,
    pub field_of_view_deg: Option<f64>,
    pub orthographic_scale: Option<f64>,
    pub clipping: Option<[f64; 2]>,
    pub viewport: Option<[u32; 3]>,
    pub visible_objects: Vec<EntityRef>,
    pub hidden_objects: Vec<EntityRef>,
    pub section_planes: Vec<Value>,
    pub timestamp_ms: u128,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SnapshotRequest {
    pub session: Option<String>,
    pub document: Option<String>,
    pub freshness: String,
    pub entity_refs: Vec<EntityRef>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct EntityRequest {
    #[serde(rename = "ref")]
    pub ref_: EntityRef,
    pub detail: String,
    pub topology_depth: usize,
    pub dependency_depth: usize,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ChangeRequest {
    pub changeset: ccir_core::ChangeSet,
    pub preview_hash: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ChangeResponse {
    pub base_revision: String,
    pub resulting_revision: Option<String>,
    pub changeset_hash: String,
    pub preview_hash: Option<String>,
    pub rolled_back: bool,
    pub rollback_fingerprint_verified: bool,
    pub diff: Option<Value>,
    pub validation: ValidationResult,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ValidationResult {
    pub status: String,
    pub checks: Vec<ValidationCheck>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ValidationCheck {
    pub id: String,
    pub status: String,
    pub message: String,
    pub evidence: Vec<EntityRef>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SessionAdvertisement {
    pub session: String,
    pub session_epoch: String,
    pub endpoint: String,
    pub pid: u32,
    pub started_at_ms: u128,
    pub hello: BridgeHello,
    pub documents: Vec<String>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    #[test]
    fn snapshot_round_trips() {
        let snapshot = BridgeSnapshot {
            schema_version: DTO_VERSION.into(),
            session: "s".into(),
            session_epoch: "e".into(),
            document: "d".into(),
            document_label: None,
            revision: Revision::new("e"),
            active_workbench: None,
            gui_mode: false,
            active_object: None,
            active_edit: None,
            selection: Vec::new(),
            view: None,
            entities: Vec::new(),
            diagnostics: Vec::new(),
            metadata: BTreeMap::new(),
        };
        let encoded = serde_json::to_string(&snapshot).unwrap();
        let decoded: BridgeSnapshot = serde_json::from_str(&encoded).unwrap();
        assert_eq!(decoded.session, "s");
    }

    #[test]
    fn bounded_frames_and_pairing_are_shared() {
        let mut bytes = Vec::new();
        write_frame(&mut bytes, &serde_json::json!({"ok": true})).unwrap();
        let decoded: Value = read_frame(&mut Cursor::new(bytes)).unwrap();
        assert_eq!(decoded["ok"], true);
        assert!(verify_pairing(
            b"secret",
            "nonce",
            &pairing_proof(b"secret", "nonce").unwrap()
        ));
        assert!(!verify_pairing(b"secret", "nonce", "bad"));
    }

    #[test]
    fn oversized_frames_fail_before_allocation() {
        let error = write_frame(&mut Vec::new(), &"x".repeat(MAX_FRAME + 1)).unwrap_err();
        assert!(matches!(error, WireError::FrameTooLarge(_)));
    }
}
