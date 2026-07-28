//! Session discovery and the bridge/fixture boundary.

use bridge_dto::{BridgeSnapshot, SessionAdvertisement, SnapshotRequest, ViewState};
use ccir_core::{ChangeSet, EntityRef, Graph, Value};
use context_compiler::{ContextRequest, ContextResult, assemble, compile_snapshot};
use ipc_client::IpcClient;
use revision_store::{Preview, RevisionRecord, RevisionStore, preview_hash};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::fs;
use std::path::Path;
use thiserror::Error;

#[derive(Debug, Error)]
pub enum SessionError {
    #[error("session not found: {0}")]
    NotFound(String),
    #[error("document not found: {0}")]
    DocumentNotFound(String),
    #[error("session is disconnected")]
    Disconnected,
    #[error("fixture error: {0}")]
    Fixture(#[from] revision_store::RevisionError),
    #[error("context error: {0}")]
    Context(#[from] context_compiler::ContextError),
    #[error("IPC error: {0}")]
    Ipc(#[from] ipc_client::IpcError),
    #[error("serialization failed: {0}")]
    Serialization(#[from] serde_json::Error),
    #[error("core error: {0}")]
    Core(#[from] ccir_core::CcirError),
    #[error("filesystem error: {0}")]
    Io(#[from] std::io::Error),
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SessionSummary {
    pub id: String,
    pub epoch: String,
    pub state: String,
    pub freecad_version: String,
    pub python: String,
    pub occt: String,
    pub mode: String,
    pub active_workbench: Option<String>,
    pub documents: Vec<String>,
    pub capabilities: Vec<String>,
    pub source: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct PreviewEnvelope {
    pub base_revision: String,
    pub preview_revision: String,
    pub changeset_hash: String,
    pub preview_hash: String,
    pub diff: Value,
    pub validation: Value,
    pub rolled_back: bool,
    pub rollback_fingerprint_verified: bool,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct CommitEnvelope {
    pub resulting_revision: Option<String>,
    pub changeset_hash: String,
    pub audit_id: String,
    pub diff: Option<Value>,
    pub validation: Value,
    pub undo_available: bool,
}

enum SessionBackend {
    Fixture(RevisionStore),
    Ipc {
        client: IpcClient,
        store: Option<RevisionStore>,
    },
}

struct SessionEntry {
    summary: SessionSummary,
    backend: SessionBackend,
}

#[derive(Default)]
pub struct SessionManager {
    sessions: BTreeMap<String, SessionEntry>,
}

impl SessionManager {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn register_fixture(&mut self, snapshot: BridgeSnapshot) -> Result<String, SessionError> {
        let graph = compile_snapshot(&snapshot)?;
        let id = snapshot.session.clone();
        let summary = SessionSummary {
            id: id.clone(),
            epoch: snapshot.session_epoch,
            state: "ready_read_only".into(),
            freecad_version: "fixture".into(),
            python: "fixture".into(),
            occt: "fixture".into(),
            mode: if snapshot.gui_mode { "gui" } else { "headless" }.into(),
            active_workbench: snapshot.active_workbench,
            documents: vec![snapshot.document],
            capabilities: vec![
                "read_document_structure".into(),
                "read_exact_geometry".into(),
            ],
            source: "fixture".into(),
        };
        self.sessions.insert(
            id.clone(),
            SessionEntry {
                summary,
                backend: SessionBackend::Fixture(RevisionStore::new(graph)?),
            },
        );
        Ok(id)
    }

    pub fn register_ipc(&mut self, advertisement: SessionAdvertisement, client: IpcClient) {
        let hello = &advertisement.hello;
        let summary = SessionSummary {
            id: advertisement.session.clone(),
            epoch: advertisement.session_epoch,
            state: "ready_read_only".into(),
            freecad_version: hello.freecad.version.clone(),
            python: hello.freecad.python.clone(),
            occt: hello.freecad.occt.clone(),
            mode: hello.freecad.mode.clone(),
            active_workbench: hello.freecad.active_workbench.clone(),
            documents: advertisement.documents,
            capabilities: hello.capabilities.clone(),
            source: "ipc".into(),
        };
        self.sessions.insert(
            advertisement.session,
            SessionEntry {
                summary,
                backend: SessionBackend::Ipc {
                    client,
                    store: None,
                },
            },
        );
    }

    pub fn load_fixture_file(&mut self, path: impl AsRef<Path>) -> Result<String, SessionError> {
        let snapshot: BridgeSnapshot = serde_json::from_slice(&fs::read(path)?)?;
        self.register_fixture(snapshot)
    }

    pub fn list(&self) -> Vec<SessionSummary> {
        self.sessions
            .values()
            .map(|entry| entry.summary.clone())
            .collect()
    }

    pub fn summary(&self, session: &str) -> Result<SessionSummary, SessionError> {
        self.sessions
            .get(session)
            .map(|entry| entry.summary.clone())
            .ok_or_else(|| SessionError::NotFound(session.into()))
    }

    /// Synchronize one live bridge into the immutable sidecar revision store.
    /// FreeCAD objects never escape the bridge; only a validated DTO is stored.
    pub fn refresh(&mut self, session: &str, document: &str) -> Result<String, SessionError> {
        let entry = self
            .sessions
            .get_mut(session)
            .ok_or_else(|| SessionError::NotFound(session.into()))?;
        match &mut entry.backend {
            SessionBackend::Fixture(store) => {
                if store.current().document != document {
                    return Err(SessionError::DocumentNotFound(document.into()));
                }
                Ok(store.current().revision.id())
            }
            SessionBackend::Ipc { client, store } => {
                let snapshot: BridgeSnapshot =
                    client.snapshot(serde_json::to_value(SnapshotRequest {
                        session: Some(session.into()),
                        document: Some(document.into()),
                        freshness: "synchronized".into(),
                        entity_refs: Vec::new(),
                    })?)?;
                if snapshot.session != session || snapshot.document != document {
                    return Err(SessionError::DocumentNotFound(format!(
                        "bridge returned {}/{}",
                        snapshot.session, snapshot.document
                    )));
                }
                let graph = compile_snapshot(&snapshot)?;
                if let Some(existing) = store {
                    if existing.current_fingerprint()? != graph.fingerprint()? {
                        existing.observe(graph, "bridge_observe")?;
                    }
                    entry.summary.epoch = snapshot.session_epoch;
                    entry.summary.state = "ready".into();
                    Ok(existing.current().revision.id())
                } else {
                    let revision = graph.revision.id();
                    *store = Some(RevisionStore::new(graph)?);
                    entry.summary.epoch = snapshot.session_epoch;
                    entry.summary.state = "ready".into();
                    Ok(revision)
                }
            }
        }
    }

    pub fn graph(&self, session: &str, document: &str) -> Result<&Graph, SessionError> {
        let entry = self
            .sessions
            .get(session)
            .ok_or_else(|| SessionError::NotFound(session.into()))?;
        match &entry.backend {
            SessionBackend::Fixture(store) => (store.current().document == document)
                .then_some(store.current())
                .ok_or_else(|| SessionError::DocumentNotFound(document.into())),
            SessionBackend::Ipc {
                store: Some(store), ..
            } => (store.current().document == document)
                .then_some(store.current())
                .ok_or_else(|| SessionError::DocumentNotFound(document.into())),
            SessionBackend::Ipc { store: None, .. } => Err(SessionError::Disconnected),
        }
    }

    pub fn revision_record(
        &self,
        session: &str,
        document: &str,
        revision: &str,
    ) -> Result<&RevisionRecord, SessionError> {
        let entry = self
            .sessions
            .get(session)
            .ok_or_else(|| SessionError::NotFound(session.into()))?;
        match &entry.backend {
            SessionBackend::Fixture(store) => {
                if store.current().document != document {
                    return Err(SessionError::DocumentNotFound(document.into()));
                }
                store
                    .get(revision)
                    .ok_or_else(|| SessionError::DocumentNotFound(format!("revision {revision}")))
            }
            SessionBackend::Ipc {
                store: Some(store), ..
            } => {
                if store.current().document != document {
                    return Err(SessionError::DocumentNotFound(document.into()));
                }
                store
                    .get(revision)
                    .ok_or_else(|| SessionError::DocumentNotFound(format!("revision {revision}")))
            }
            SessionBackend::Ipc { store: None, .. } => Err(SessionError::Disconnected),
        }
    }

    pub fn context(
        &self,
        session: &str,
        document: &str,
        request: &ContextRequest,
    ) -> Result<ContextResult, SessionError> {
        let graph = self.graph(session, document)?;
        let selection = current_selection(graph);
        let view = graph
            .metadata
            .get("view")
            .and_then(|value| serde_json::from_value::<ViewState>(value.clone()).ok());
        Ok(assemble(graph, request, &selection, view.as_ref(), None)?)
    }

    pub fn inspect(
        &self,
        session: &str,
        document: &str,
        request: &context_compiler::InspectRequest,
    ) -> Result<context_compiler::EntityResult, SessionError> {
        Ok(context_compiler::inspect_entity(
            self.graph(session, document)?,
            request,
        )?)
    }

    pub fn preview(&mut self, changeset: &ChangeSet) -> Result<PreviewEnvelope, SessionError> {
        let entry = self
            .sessions
            .get_mut(&changeset.session)
            .ok_or_else(|| SessionError::NotFound(changeset.session.clone()))?;
        match &mut entry.backend {
            SessionBackend::Fixture(store) => {
                if store.current().document != changeset.document {
                    return Err(SessionError::DocumentNotFound(changeset.document.clone()));
                }
                let preview = store.preview(changeset)?;
                let preview_hash = preview_hash(&preview)?;
                Ok(preview_envelope(&preview, preview_hash)?)
            }
            SessionBackend::Ipc { client, .. } => {
                let response: bridge_dto::ChangeResponse = client.call(
                    "bridge.preview",
                    serde_json::to_value(bridge_dto::ChangeRequest {
                        changeset: changeset.clone(),
                        preview_hash: None,
                    })?,
                )?;
                let changeset_hash = changeset.hash()?;
                let preview_hash = response
                    .preview_hash
                    .clone()
                    .unwrap_or_else(|| "unavailable".into());
                Ok(PreviewEnvelope {
                    base_revision: response.base_revision,
                    preview_revision: response.resulting_revision.unwrap_or_default(),
                    changeset_hash,
                    preview_hash,
                    diff: response.diff.unwrap_or(Value::Null),
                    validation: serde_json::to_value(response.validation)?,
                    rolled_back: response.rolled_back,
                    rollback_fingerprint_verified: response.rollback_fingerprint_verified,
                })
            }
        }
    }

    pub fn commit(
        &mut self,
        changeset: &ChangeSet,
        preview_hash: &str,
    ) -> Result<CommitEnvelope, SessionError> {
        let entry = self
            .sessions
            .get_mut(&changeset.session)
            .ok_or_else(|| SessionError::NotFound(changeset.session.clone()))?;
        let (result, refresh_live) = match &mut entry.backend {
            SessionBackend::Fixture(store) => {
                let record = store.commit(changeset, preview_hash)?;
                (
                    CommitEnvelope {
                        resulting_revision: Some(record.revision.id()),
                        changeset_hash: changeset.hash()?,
                        audit_id: format!("audit-{}", uuid::Uuid::new_v4()),
                        diff: None,
                        validation: serde_json::json!({"status": "pass", "checks": changeset.validate}),
                        undo_available: true,
                    },
                    false,
                )
            }
            SessionBackend::Ipc { client, .. } => {
                let response: bridge_dto::ChangeResponse = client.call(
                    "bridge.commit",
                    serde_json::to_value(bridge_dto::ChangeRequest {
                        changeset: changeset.clone(),
                        preview_hash: Some(preview_hash.into()),
                    })?,
                )?;
                (
                    CommitEnvelope {
                        resulting_revision: response.resulting_revision,
                        changeset_hash: changeset.hash()?,
                        audit_id: format!("audit-{}", uuid::Uuid::new_v4()),
                        diff: response.diff,
                        validation: serde_json::to_value(response.validation)?,
                        undo_available: true,
                    },
                    true,
                )
            }
        };
        if refresh_live {
            self.refresh(&changeset.session, &changeset.document)?;
        }
        Ok(result)
    }

    pub fn undo_redo(
        &mut self,
        session: &str,
        document: &str,
        redo: bool,
    ) -> Result<CommitEnvelope, SessionError> {
        let entry = self
            .sessions
            .get_mut(session)
            .ok_or_else(|| SessionError::NotFound(session.into()))?;
        let (result, refresh_live) = match &mut entry.backend {
            SessionBackend::Fixture(store) => {
                if store.current().document != document {
                    return Err(SessionError::DocumentNotFound(document.into()));
                }
                let record = if redo { store.redo()? } else { store.undo()? };
                (
                    CommitEnvelope {
                        resulting_revision: Some(record.revision.id()),
                        changeset_hash: format!("navigation-{}", uuid::Uuid::new_v4()),
                        audit_id: format!("audit-{}", uuid::Uuid::new_v4()),
                        diff: None,
                        validation: serde_json::json!({"status": "pass", "checks": [if redo { "redo" } else { "undo" }]}),
                        undo_available: true,
                    },
                    false,
                )
            }
            SessionBackend::Ipc { client, .. } => {
                let response: bridge_dto::ChangeResponse = client.call(
                    if redo { "bridge.redo" } else { "bridge.undo" },
                    serde_json::json!({"document": document}),
                )?;
                (
                    CommitEnvelope {
                        resulting_revision: response.resulting_revision,
                        changeset_hash: format!("navigation-{}", uuid::Uuid::new_v4()),
                        audit_id: format!("audit-{}", uuid::Uuid::new_v4()),
                        diff: response.diff,
                        validation: serde_json::to_value(response.validation)?,
                        undo_available: true,
                    },
                    true,
                )
            }
        };
        if refresh_live {
            self.refresh(session, document)?;
        }
        Ok(result)
    }

    pub fn raw_bridge_call<T: serde::de::DeserializeOwned>(
        &mut self,
        session: &str,
        message: &str,
        payload: Value,
    ) -> Result<T, SessionError> {
        let entry = self
            .sessions
            .get_mut(session)
            .ok_or_else(|| SessionError::NotFound(session.into()))?;
        match &mut entry.backend {
            SessionBackend::Fixture(_) => Err(SessionError::Disconnected),
            SessionBackend::Ipc { client, .. } => Ok(client.call(message, payload)?),
        }
    }
}

fn current_selection(graph: &Graph) -> Vec<EntityRef> {
    graph
        .metadata
        .get("selection")
        .and_then(Value::as_array)
        .map(|items| {
            items
                .iter()
                .filter_map(|item| item.get("ref").or_else(|| item.get("ref_")))
                .filter_map(Value::as_str)
                .map(str::to_owned)
                .collect()
        })
        .unwrap_or_default()
}

fn preview_envelope(
    preview: &Preview,
    preview_hash: String,
) -> Result<PreviewEnvelope, SessionError> {
    Ok(PreviewEnvelope {
        base_revision: preview.base_revision.clone(),
        preview_revision: preview.preview_revision.clone(),
        changeset_hash: preview.changeset_hash.clone(),
        preview_hash,
        diff: serde_json::to_value(&preview.diff)?,
        validation: serde_json::json!({"status": "pass", "checks": []}),
        rolled_back: preview.rolled_back,
        rollback_fingerprint_verified: preview.rollback_fingerprint_verified,
    })
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
            document_label: Some("D".into()),
            revision: revision.clone(),
            active_workbench: None,
            gui_mode: false,
            active_object: None,
            active_edit: None,
            selection: Vec::new(),
            view: None,
            entities: vec![BridgeEntity {
                ref_: format!("fc://session/s/document/d/object/O@{}", revision.id()),
                kind: "cad.object".into(),
                name: Some("O".into()),
                label: None,
                frame: None,
                units: BTreeMap::new(),
                source: Some(BridgeSource {
                    object: Some("O".into()),
                    subelement: None,
                    extractor: "test".into(),
                    kernel_tolerance: None,
                    fingerprint: None,
                }),
                identity: Identity::default(),
                properties: serde_json::json!({"valid": true}),
                links: Vec::new(),
                bounds: None,
                shape: Some(ShapeSummary::default()),
            }],
            diagnostics: Vec::new(),
            metadata: BTreeMap::new(),
        }
    }

    #[test]
    fn fixture_session_is_listed_and_context_is_bounded() {
        let mut manager = SessionManager::new();
        manager.register_fixture(snapshot()).unwrap();
        assert_eq!(manager.list().len(), 1);
        let context = manager
            .context(
                "s",
                "d",
                &ContextRequest {
                    detail: Default::default(),
                    focus: Default::default(),
                    budget: Default::default(),
                    freshness: "synchronized".into(),
                    include_recent_diff: false,
                    task_profile: None,
                },
            )
            .unwrap();
        assert_eq!(context.observed_revision.geometry, 0);
    }
}
