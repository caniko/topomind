use crate::executor::{Executor, OperationResult};
use crate::extractor::Extractor;
use crate::pyutil::{attr, call0, import, optional_import, py_value, string_attr, version_string};
use bridge_dto::{
    DTO_VERSION, IPC_VERSION, IpcRequest, IpcResponse, ResponseStatus, WireError, read_frame,
    verify_pairing, write_frame,
};
use ccir_core::{Diagnostic, Revision, Severity, Value, sha256_json};
use pyo3::prelude::*;
use rand::random;
use serde_json::json;
use std::collections::{HashMap, VecDeque};
use std::fs::{self, OpenOptions};
use std::io::{self, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock, mpsc};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use uuid::Uuid;

#[cfg(unix)]
use std::os::unix::fs::OpenOptionsExt;
#[cfg(unix)]
use std::os::unix::net::{UnixListener, UnixStream};

static BRIDGE: OnceLock<Mutex<Option<Arc<BridgeState>>>> = OnceLock::new();

fn bridge_slot() -> &'static Mutex<Option<Arc<BridgeState>>> {
    BRIDGE.get_or_init(|| Mutex::new(None))
}

pub struct BridgeState {
    pub app: Py<PyAny>,
    pub gui: Option<Py<PyAny>>,
    pub part: Option<Py<PyAny>>,
    pub session: String,
    pub epoch: String,
    rendezvous: Rendezvous,
    revision: Mutex<Revision>,
    pending: Mutex<VecDeque<PendingRequest>>,
    previews: Mutex<HashMap<String, PreviewRecord>>,
    write_blocked: AtomicBool,
    stop: AtomicBool,
    worker: Mutex<Option<JoinHandle<()>>>,
    timer: Mutex<Option<Py<PyAny>>>,
    observers: Mutex<Vec<Py<PyAny>>>,
    events: EventCoalescer,
}

struct PendingRequest {
    request: IpcRequest,
    authenticated: bool,
    response: mpsc::Sender<DispatchResult>,
}

struct DispatchResult {
    response: IpcResponse,
    authenticated: bool,
}

#[derive(Clone)]
struct PreviewRecord {
    base_revision: String,
    base_fingerprint: String,
    preview_hash: String,
}

struct EventCoalescer {
    sequence: AtomicU64,
    pending: Mutex<HashMap<String, PendingEvent>>,
}

struct PendingEvent {
    sequence: u64,
    object_name: Option<String>,
    at: Instant,
}

impl EventCoalescer {
    fn new() -> Self {
        Self {
            sequence: AtomicU64::new(0),
            pending: Mutex::new(HashMap::new()),
        }
    }

    fn emit(&self, event_class: &str, object_name: Option<String>) {
        let sequence = self.sequence.fetch_add(1, Ordering::Relaxed) + 1;
        self.pending.lock().expect("event lock poisoned").insert(
            event_class.into(),
            PendingEvent {
                sequence,
                object_name,
                at: Instant::now(),
            },
        );
    }

    fn flush(&self, force: bool) -> Vec<String> {
        let now = Instant::now();
        let mut pending = self.pending.lock().expect("event lock poisoned");
        let mut ready = pending
            .iter()
            .filter(|(event_class, event)| {
                force
                    || event_class.as_str() == "transaction"
                    || now.duration_since(event.at) >= Duration::from_millis(100)
            })
            .map(|(event_class, event)| {
                (
                    event.sequence,
                    event_class.clone(),
                    event.object_name.clone(),
                )
            })
            .collect::<Vec<_>>();
        for (_, event_class, _) in &ready {
            pending.remove(event_class);
        }
        ready.sort_by_key(|event| event.0);
        ready
            .into_iter()
            .map(|(_, event_class, _)| event_class)
            .collect()
    }
}

impl BridgeState {
    fn new(
        _py: Python<'_>,
        app: &Bound<'_, PyAny>,
        gui: Option<&Bound<'_, PyAny>>,
        part: Option<&Bound<'_, PyAny>>,
    ) -> Result<Arc<Self>, String> {
        let rendezvous = Rendezvous::new().map_err(|error| error.to_string())?;
        let session = rendezvous.session.clone();
        let epoch = rendezvous.epoch.clone();
        Ok(Arc::new(Self {
            app: app.clone().unbind(),
            gui: gui.map(|value| value.clone().unbind()),
            part: part.map(|value| value.clone().unbind()),
            session,
            epoch: epoch.clone(),
            rendezvous,
            revision: Mutex::new(Revision::new(epoch)),
            pending: Mutex::new(VecDeque::new()),
            previews: Mutex::new(HashMap::new()),
            write_blocked: AtomicBool::new(false),
            stop: AtomicBool::new(false),
            worker: Mutex::new(None),
            timer: Mutex::new(None),
            observers: Mutex::new(Vec::new()),
            events: EventCoalescer::new(),
        }))
    }

    fn start(self: &Arc<Self>, py: Python<'_>) -> Result<String, String> {
        self.install_observers(py)?;
        let timer = install_timer(py)?;
        *self.timer.lock().expect("timer lock poisoned") = Some(timer);
        let hello = self.hello(py);
        let documents = self.documents(py);
        let state = Arc::clone(self);
        let worker = thread::Builder::new()
            .name("topomind-freecad-bridge".into())
            .spawn(move || server_loop(state, hello, documents))
            .map_err(|error| error.to_string())?;
        *self.worker.lock().expect("worker lock poisoned") = Some(worker);
        Ok(self.rendezvous.record_path.to_string_lossy().into_owned())
    }

    fn stop(&self, py: Python<'_>) {
        self.stop.store(true, Ordering::Release);
        if let Some(timer) = self.timer.lock().expect("timer lock poisoned").take() {
            let _ = timer.bind(py).call_method0("stop");
        }
        if let Some(worker) = self.worker.lock().expect("worker lock poisoned").take() {
            let _ = worker.join();
        }
        let app = self.app.bind(py);
        let gui = self.gui.as_ref().map(|value| value.bind(py));
        for observer in self
            .observers
            .lock()
            .expect("observer lock poisoned")
            .drain(..)
        {
            if let Some(remove) = attr(app, "removeDocumentObserver") {
                let _ = remove.call1((observer.bind(py),));
            }
            if let Some(selection) = gui.as_ref().and_then(|gui| attr(gui, "Selection")) {
                if let Some(remove) = attr(&selection, "removeObserver") {
                    let _ = remove.call1((observer.bind(py),));
                }
            }
        }
        self.rendezvous.close();
    }

    fn install_observers(self: &Arc<Self>, py: Python<'_>) -> Result<(), String> {
        let app = self.app.bind(py);
        let observer = Py::new(
            py,
            DocumentObserver {
                state: Arc::clone(self),
            },
        )
        .map_err(|error| error.to_string())?;
        if let Some(add) = attr(app, "addDocumentObserver") {
            add.call1((observer.clone_ref(py),))
                .map_err(|error| error.to_string())?;
            self.observers
                .lock()
                .expect("observer lock poisoned")
                .push(observer.clone_ref(py).into_any());
        }
        if let Some(gui) = self.gui.as_ref().map(|value| value.bind(py)) {
            if let Some(selection) = attr(gui, "Selection") {
                if let Some(add) = attr(&selection, "addObserver") {
                    let selection_observer = Py::new(
                        py,
                        SelectionObserver {
                            state: Arc::clone(self),
                        },
                    )
                    .map_err(|error| error.to_string())?;
                    add.call1((selection_observer.clone_ref(py),))
                        .map_err(|error| error.to_string())?;
                    self.observers
                        .lock()
                        .expect("observer lock poisoned")
                        .push(selection_observer.into_any());
                }
            }
        }
        Ok(())
    }

    pub fn process_pending(&self, py: Python<'_>) {
        let pending = {
            let mut pending = self.pending.lock().expect("pending lock poisoned");
            pending.drain(..).collect::<Vec<_>>()
        };
        for item in pending {
            let result = self.handle(py, &item.request, item.authenticated);
            let _ = item.response.send(result);
        }
    }

    fn enqueue(&self, request: IpcRequest, authenticated: bool) -> DispatchResult {
        let (sender, receiver) = mpsc::channel();
        self.pending
            .lock()
            .expect("pending lock poisoned")
            .push_back(PendingRequest {
                request,
                authenticated,
                response: sender,
            });
        receiver
            .recv_timeout(Duration::from_secs(10))
            .unwrap_or_else(|_| DispatchResult {
                response: self.error_response(
                    "unknown",
                    "bridge_busy",
                    "bridge main thread did not respond",
                    true,
                ),
                authenticated,
            })
    }

    fn handle(&self, py: Python<'_>, request: &IpcRequest, authenticated: bool) -> DispatchResult {
        if request.protocol_version != IPC_VERSION {
            return DispatchResult {
                response: self.error_response(
                    &request.request_id,
                    "invalid_request",
                    "unsupported IPC protocol version",
                    false,
                ),
                authenticated,
            };
        }
        if request.message == "bridge.authenticate" {
            let nonce = request
                .payload
                .get("nonce")
                .and_then(Value::as_str)
                .unwrap_or_default();
            let proof = request
                .payload
                .get("proof")
                .and_then(Value::as_str)
                .unwrap_or_default();
            if !verify_pairing(&self.rendezvous.secret, nonce, proof) {
                return DispatchResult {
                    response: self.error_response(
                        &request.request_id,
                        "permission_denied",
                        "bridge authentication failed",
                        false,
                    ),
                    authenticated: false,
                };
            }
            let payload = json!({"session": self.session, "hello": self.hello(py), "documents": self.documents(py)});
            return DispatchResult {
                response: self.ok_response(&request.request_id, payload),
                authenticated: true,
            };
        }
        if !authenticated {
            return DispatchResult {
                response: self.error_response(
                    &request.request_id,
                    "permission_denied",
                    "bridge must be authenticated",
                    false,
                ),
                authenticated: false,
            };
        }
        if request.session_epoch != self.epoch {
            return DispatchResult {
                response: self.error_response(
                    &request.request_id,
                    "session_unavailable",
                    "session epoch is stale",
                    true,
                ),
                authenticated,
            };
        }
        let force = request.payload.get("freshness").and_then(Value::as_str)
            == Some("synchronized")
            || matches!(
                request.message.as_str(),
                "bridge.inspect"
                    | "bridge.preview"
                    | "bridge.commit"
                    | "bridge.undo"
                    | "bridge.redo"
            );
        self.flush_events(force);
        let result = match request.message.as_str() {
            "bridge.snapshot" => {
                self.snapshot(py, request.payload.get("document").and_then(Value::as_str))
            }
            "bridge.inspect" => self.inspect(py, &request.payload),
            "bridge.preview" => self.preview(py, &request.payload),
            "bridge.commit" => self.commit(py, &request.payload),
            "bridge.undo" => self.undo(py, request.payload.get("document").and_then(Value::as_str)),
            "bridge.redo" => self.redo(py, request.payload.get("document").and_then(Value::as_str)),
            _ => Err(BridgeFault::new(
                "invalid_request",
                format!("unsupported bridge message: {}", request.message),
                false,
            )),
        };
        let response = match result {
            Ok(payload) => self.ok_response(&request.request_id, payload),
            Err(error) => self.error_response(
                &request.request_id,
                &error.code,
                &error.message,
                error.retryable,
            ),
        };
        DispatchResult {
            response,
            authenticated,
        }
    }

    fn snapshot(&self, py: Python<'_>, document: Option<&str>) -> Result<Value, BridgeFault> {
        let revision = self
            .revision
            .lock()
            .expect("revision lock poisoned")
            .clone();
        let app = self.app.bind(py);
        let gui = self.gui.as_ref().map(|value| value.bind(py));
        Ok(Extractor.snapshot(app, gui, &self.session, &revision, document))
    }

    fn inspect(&self, py: Python<'_>, payload: &Value) -> Result<Value, BridgeFault> {
        let snapshot = self.snapshot(py, payload.get("document").and_then(Value::as_str))?;
        let reference = payload
            .get("ref")
            .and_then(Value::as_str)
            .ok_or_else(|| BridgeFault::new("invalid_request", "ref is required", false))?;
        let entity = snapshot
            .get("entities")
            .and_then(Value::as_array)
            .and_then(|entities| {
                entities
                    .iter()
                    .find(|entity| entity.get("ref").and_then(Value::as_str) == Some(reference))
            })
            .cloned()
            .ok_or_else(|| {
                BridgeFault::new(
                    "stale_reference",
                    "entity is not present in the current snapshot",
                    true,
                )
            })?;
        Ok(
            json!({"entity": entity, "revision": snapshot.get("revision").cloned().unwrap_or(Value::Null)}),
        )
    }

    fn preview(&self, py: Python<'_>, payload: &Value) -> Result<Value, BridgeFault> {
        let changeset = payload
            .get("changeset")
            .filter(|value| value.is_object())
            .ok_or_else(|| BridgeFault::new("invalid_request", "changeset is required", false))?;
        let before = self.snapshot(py, changeset.get("document").and_then(Value::as_str))?;
        self.validate_changeset(changeset, &before)?;
        let base_revision = revision_id(&before);
        let changeset_hash = changeset_hash(changeset)?;
        let base_fingerprint = snapshot_fingerprint(&before)?;
        let counters = self
            .revision
            .lock()
            .expect("revision lock poisoned")
            .clone();
        let result = self.apply(py, changeset, false)?;
        self.bump_for_changeset(changeset);
        let preview_snapshot =
            self.snapshot(py, changeset.get("document").and_then(Value::as_str))?;
        *self.revision.lock().expect("revision lock poisoned") = counters;
        let after_abort = self.snapshot(py, changeset.get("document").and_then(Value::as_str))?;
        let rollback_verified = snapshot_fingerprint(&after_abort)
            .map(|fingerprint| fingerprint == base_fingerprint)
            .unwrap_or(false);
        if !rollback_verified {
            self.write_blocked.store(true, Ordering::Release);
            return Err(BridgeFault::new(
                "rollback_verification_failed",
                "rollback verification failed",
                false,
            ));
        }
        let preview_fingerprint = snapshot_fingerprint(&preview_snapshot)?;
        let preview_hash = sha256_json(&json!({"base_revision": base_revision, "base_fingerprint": base_fingerprint, "preview_fingerprint": preview_fingerprint, "changeset_hash": changeset_hash})).map_err(|error| BridgeFault::new("internal_inconsistency", error.to_string(), false))?;
        self.previews.lock().expect("preview lock poisoned").insert(
            changeset_hash.clone(),
            PreviewRecord {
                base_revision: base_revision.clone(),
                base_fingerprint,
                preview_hash: preview_hash.clone(),
            },
        );
        Ok(change_response(
            &base_revision,
            Some(&revision_id(&preview_snapshot)),
            &changeset_hash,
            Some(&preview_hash),
            true,
            rollback_verified,
            json!({"changed_entities": result.changed, "objects_recomputed": result.changed.len()}),
            result.checks,
        ))
    }

    fn commit(&self, py: Python<'_>, payload: &Value) -> Result<Value, BridgeFault> {
        let changeset = payload
            .get("changeset")
            .filter(|value| value.is_object())
            .ok_or_else(|| BridgeFault::new("invalid_request", "changeset is required", false))?;
        let changeset_hash = changeset_hash(changeset)?;
        let preview_hash = payload
            .get("preview_hash")
            .and_then(Value::as_str)
            .unwrap_or_default();
        let record = self
            .previews
            .lock()
            .expect("preview lock poisoned")
            .get(&changeset_hash)
            .cloned()
            .ok_or_else(|| {
                BridgeFault::new(
                    "invalid_request",
                    "preview hash is not known or does not match",
                    false,
                )
            })?;
        if record.preview_hash != preview_hash {
            return Err(BridgeFault::new(
                "invalid_request",
                "preview hash is not known or does not match",
                false,
            ));
        }
        let before = self.snapshot(py, changeset.get("document").and_then(Value::as_str))?;
        self.validate_changeset(changeset, &before)?;
        if snapshot_fingerprint(&before).ok().as_deref() != Some(record.base_fingerprint.as_str()) {
            return Err(BridgeFault::new(
                "revision_conflict",
                "revision conflict",
                true,
            ));
        }
        let result = self.apply(py, changeset, true)?;
        self.bump_for_changeset(changeset);
        let after = self.snapshot(py, changeset.get("document").and_then(Value::as_str))?;
        self.previews
            .lock()
            .expect("preview lock poisoned")
            .remove(&changeset_hash);
        Ok(change_response(
            &record.base_revision,
            Some(&revision_id(&after)),
            &changeset_hash,
            Some(&record.preview_hash),
            false,
            true,
            json!({"changed_entities": result.changed}),
            result.checks,
        ))
    }

    fn undo(&self, py: Python<'_>, document: Option<&str>) -> Result<Value, BridgeFault> {
        let app = self.app.bind(py);
        Executor
            .undo(app, document)
            .map_err(|message| BridgeFault::new(error_code(&message), message, false))?;
        self.bump("transaction");
        Ok(
            json!({"resulting_revision": self.current_revision(), "validation": {"status": "pass", "checks": [{"id": "undo", "status": "pass", "message": "FreeCAD undo completed", "evidence": []}]}}),
        )
    }

    fn redo(&self, py: Python<'_>, document: Option<&str>) -> Result<Value, BridgeFault> {
        let app = self.app.bind(py);
        Executor
            .redo(app, document)
            .map_err(|message| BridgeFault::new(error_code(&message), message, false))?;
        self.bump("transaction");
        Ok(
            json!({"resulting_revision": self.current_revision(), "validation": {"status": "pass", "checks": [{"id": "redo", "status": "pass", "message": "FreeCAD redo completed", "evidence": []}]}}),
        )
    }

    fn apply(
        &self,
        py: Python<'_>,
        changeset: &Value,
        commit: bool,
    ) -> Result<OperationResult, BridgeFault> {
        let app = self.app.bind(py);
        let gui = self.gui.as_ref().map(|value| value.bind(py));
        let part = self.part.as_ref().map(|value| value.bind(py));
        Executor
            .apply(py, app, gui, part, changeset, commit)
            .map_err(|message| BridgeFault::new(error_code(&message), message, false))
    }

    fn validate_changeset(&self, changeset: &Value, snapshot: &Value) -> Result<(), BridgeFault> {
        if self.write_blocked.load(Ordering::Acquire) {
            return Err(BridgeFault::new(
                "write_blocked",
                "writes are blocked after an unverified rollback; restart the bridge",
                false,
            ));
        }
        if changeset.get("schema_version").and_then(Value::as_str) != Some("changeset/1.0") {
            return Err(BridgeFault::new(
                "invalid_request",
                "unsupported changeset schema",
                false,
            ));
        }
        if let Some(session) = changeset.get("session").and_then(Value::as_str) {
            if !session.is_empty() && session != self.session {
                return Err(BridgeFault::new(
                    "permission_denied",
                    "session does not belong to this bridge",
                    false,
                ));
            }
        }
        if changeset.get("document").and_then(Value::as_str)
            != snapshot.get("document").and_then(Value::as_str)
        {
            return Err(BridgeFault::new(
                "invalid_request",
                "document is not the active bridge document",
                false,
            ));
        }
        let operations = changeset
            .get("operations")
            .and_then(Value::as_array)
            .filter(|operations| !operations.is_empty())
            .ok_or_else(|| {
                BridgeFault::new(
                    "invalid_request",
                    "changeset must contain operations",
                    false,
                )
            })?;
        let current = revision_id(snapshot);
        if !revision_matches(
            changeset.get("base_revision").and_then(Value::as_str),
            snapshot,
        ) {
            return Err(BridgeFault::new(
                "revision_conflict",
                "revision conflict",
                true,
            ));
        }
        let entities = snapshot
            .get("entities")
            .and_then(Value::as_array)
            .map(|entities| {
                entities
                    .iter()
                    .filter_map(|entity| {
                        entity
                            .get("ref")
                            .and_then(Value::as_str)
                            .map(|reference| (reference, entity))
                    })
                    .collect::<HashMap<_, _>>()
            })
            .unwrap_or_default();
        for precondition in changeset
            .get("preconditions")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
        {
            let kind = precondition
                .get("kind")
                .and_then(Value::as_str)
                .ok_or_else(|| {
                    BridgeFault::new("invalid_request", "invalid precondition", false)
                })?;
            match kind {
                "revision_equals" => {
                    if !revision_matches(
                        precondition.get("revision").and_then(Value::as_str),
                        snapshot,
                    ) {
                        return Err(BridgeFault::new(
                            "revision_conflict",
                            "revision conflict",
                            true,
                        ));
                    }
                }
                "entity_exists" | "entity_kind" | "property_equals" => {
                    let target = precondition
                        .get("target")
                        .and_then(Value::as_str)
                        .ok_or_else(|| {
                            BridgeFault::new(
                                "invalid_request",
                                "precondition target is required",
                                false,
                            )
                        })?;
                    let entity = entities.get(target).ok_or_else(|| {
                        BridgeFault::new("stale_reference", "stale reference", true)
                    })?;
                    if kind == "entity_kind"
                        && entity.get("kind").and_then(Value::as_str)
                            != precondition.get("expected_kind").and_then(Value::as_str)
                    {
                        return Err(BridgeFault::new(
                            "precondition_failed",
                            "entity kind precondition failed",
                            false,
                        ));
                    }
                    if kind == "property_equals"
                        && property_value(
                            entity.get("properties").unwrap_or(&Value::Null),
                            precondition
                                .get("property")
                                .and_then(Value::as_str)
                                .unwrap_or_default(),
                        ) != precondition.get("value").cloned().unwrap_or(Value::Null)
                    {
                        return Err(BridgeFault::new(
                            "precondition_failed",
                            "property precondition failed",
                            false,
                        ));
                    }
                }
                _ => {
                    return Err(BridgeFault::new(
                        "invalid_request",
                        format!("unsupported precondition: {kind}"),
                        false,
                    ));
                }
            }
        }
        for operation in operations {
            let name = operation.get("op").and_then(Value::as_str).ok_or_else(|| {
                BridgeFault::new("invalid_request", "invalid typed operation", false)
            })?;
            for key in ["target", "constraint", "left", "right"] {
                if let Some(reference) = operation.get(key).and_then(Value::as_str) {
                    let entity = entities.get(reference).ok_or_else(|| {
                        BridgeFault::new("stale_reference", "stale reference", true)
                    })?;
                    if !matches!(
                        entity
                            .get("identity")
                            .and_then(|identity| identity.get("state"))
                            .and_then(Value::as_str),
                        Some("exact") | Some("mapped_exact")
                    ) {
                        return Err(BridgeFault::new("stale_reference", "stale reference", true));
                    }
                }
            }
            if [
                "set_property",
                "set_expression",
                "set_visibility",
                "delete_object",
            ]
            .contains(&name)
                && operation
                    .get("target")
                    .and_then(Value::as_str)
                    .map(has_subelement)
                    .unwrap_or(false)
            {
                return Err(BridgeFault::new(
                    "invalid_request",
                    "operation target must be an object, not a sub-element",
                    false,
                ));
            }
            if name == "boolean"
                && ["left", "right"].iter().any(|key| {
                    operation
                        .get(*key)
                        .and_then(Value::as_str)
                        .map(has_subelement)
                        .unwrap_or(false)
                })
            {
                return Err(BridgeFault::new(
                    "invalid_request",
                    "boolean operands must be objects, not sub-elements",
                    false,
                ));
            }
        }
        let _ = current;
        Ok(())
    }

    fn bump_for_changeset(&self, changeset: &Value) {
        let mut classes = Vec::new();
        for operation in changeset
            .get("operations")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
        {
            classes.push(match operation.get("op").and_then(Value::as_str) {
                Some("set_selection") => "focus",
                Some("set_view") => "view",
                Some("set_visibility") => "metadata",
                _ => "geometry",
            });
        }
        classes.sort_unstable();
        classes.dedup();
        for class in classes {
            self.bump(class);
        }
    }

    fn bump(&self, event_class: &str) {
        let mut revision = self.revision.lock().expect("revision lock poisoned");
        match event_class {
            "geometry" | "parametric" | "transaction" => revision.geometry += 1,
            "metadata" => revision.metadata += 1,
            "focus" => revision.focus += 1,
            "view" => revision.view += 1,
            _ => {}
        }
    }

    fn flush_events(&self, force: bool) {
        for event in self.events.flush(force) {
            self.bump(&event);
        }
    }

    fn current_revision(&self) -> String {
        self.revision.lock().expect("revision lock poisoned").id()
    }

    fn hello(&self, py: Python<'_>) -> Value {
        let app = self.app.bind(py);
        let gui = self.gui.as_ref().map(|value| value.bind(py));
        let version = call0(app, "Version")
            .map(|value| version_string(&value))
            .unwrap_or_else(|| "unknown".into());
        let active_workbench = gui
            .as_ref()
            .and_then(|gui| call0(gui, "activeWorkbench"))
            .and_then(|value| value.extract::<String>().ok());
        let operations = vec![
            "boolean",
            "create_object",
            "create_primitive",
            "delete_object",
            "set_expression",
            "set_property",
            "set_selection",
            "set_view",
            "set_visibility",
            "sketch_set_datum",
        ]
        .into_iter()
        .map(String::from)
        .collect::<Vec<_>>();
        json!({
            "message": "bridge.hello",
            "ipc_versions": [IPC_VERSION],
            "dto_versions": [DTO_VERSION],
            "freecad": {"version": version, "python": python_version(py), "occt": "reported-at-runtime", "mode": if gui.is_some() {"gui"} else {"headless"}, "active_workbench": active_workbench},
            "extractors": ["core.document/1", "sketcher/1", "part/1", "partdesign/1", "view/1"],
            "operations": operations,
            "transaction_safety": {"core": "verified", "unknown_third_party_objects": "read_only"},
            "limits": {"max_control_message_bytes": bridge_dto::MAX_FRAME, "max_pending_requests": 64, "max_entities_per_snapshot": 50_000},
            "capabilities": ["read_document_structure", "read_exact_geometry", "write_model_low_risk", if gui.is_some() {"write_selection"} else {"write_selection_unavailable"}, if gui.is_some() {"write_view"} else {"write_view_unavailable"}],
            "compatibility": "capability_probe"
        })
    }

    fn documents(&self, py: Python<'_>) -> Vec<String> {
        let app = self.app.bind(py);
        let mut documents = Extractor.documents(app);
        documents.sort();
        documents
    }

    fn ok_response(&self, request_id: &str, payload: Value) -> IpcResponse {
        IpcResponse {
            protocol_version: IPC_VERSION.into(),
            message: "bridge.response".into(),
            request_id: request_id.into(),
            status: ResponseStatus::Ok,
            session_epoch: self.epoch.clone(),
            payload: Some(payload),
            diagnostics: Vec::new(),
        }
    }

    fn error_response(
        &self,
        request_id: &str,
        code: &str,
        message: &str,
        retryable: bool,
    ) -> IpcResponse {
        IpcResponse {
            protocol_version: IPC_VERSION.into(),
            message: "bridge.response".into(),
            request_id: request_id.into(),
            status: ResponseStatus::Error,
            session_epoch: self.epoch.clone(),
            payload: None,
            diagnostics: vec![Diagnostic {
                code: code.into(),
                severity: Severity::Error,
                message: message.into(),
                entity: None,
                retryable,
                evidence: Vec::new(),
            }],
        }
    }

    pub fn pairing_status(&self) -> Value {
        if !self.rendezvous.record_path.exists() {
            return json!({"state": "starting", "record": self.rendezvous.record_path});
        }
        match fs::read(&self.rendezvous.record_path)
            .ok()
            .and_then(|bytes| serde_json::from_slice::<Value>(&bytes).ok())
        {
            Some(record) => {
                let mut result = json!({"state": "ready"});
                if let (Some(result), Value::Object(record)) = (result.as_object_mut(), record) {
                    result.extend(record);
                }
                result
            }
            None => json!({"state": "starting", "record": self.rendezvous.record_path}),
        }
    }
}

#[derive(Debug)]
struct BridgeFault {
    code: String,
    message: String,
    retryable: bool,
}

impl BridgeFault {
    fn new(code: impl Into<String>, message: impl Into<String>, retryable: bool) -> Self {
        Self {
            code: code.into(),
            message: message.into(),
            retryable,
        }
    }
}

struct Rendezvous {
    root: PathBuf,
    session: String,
    epoch: String,
    socket_path: PathBuf,
    secret_path: PathBuf,
    record_path: PathBuf,
    secret: Vec<u8>,
}

impl Rendezvous {
    fn new() -> io::Result<Self> {
        let root = std::env::var_os("XDG_RUNTIME_DIR")
            .map(PathBuf::from)
            .unwrap_or_else(std::env::temp_dir)
            .join("topomind");
        fs::create_dir_all(&root)?;
        set_mode(&root, 0o700)?;
        let session = format!("s_{}", Uuid::new_v4().simple());
        let epoch = format!("e_{}", Uuid::new_v4().simple());
        Ok(Self {
            socket_path: root.join(format!("bridge-{session}.sock")),
            secret_path: root.join(format!("bridge-{session}.secret")),
            record_path: root.join(format!("bridge-{session}.json")),
            root,
            session,
            epoch,
            secret: (0..32).map(|_| random::<u8>()).collect(),
        })
    }

    fn publish(&self, hello: Value, documents: Vec<String>, endpoint: &str) -> io::Result<()> {
        atomic_secret(&self.secret_path, &self.secret)?;
        atomic_json(
            &self.record_path,
            &json!({"session": self.session, "session_epoch": self.epoch, "endpoint": endpoint, "secret_path": self.secret_path, "pid": std::process::id(), "started_at_ms": now_ms(), "hello": hello, "documents": documents}),
        )
    }

    fn close(&self) {
        for path in [&self.socket_path, &self.record_path, &self.secret_path] {
            let _ = fs::remove_file(path);
        }
        let _ = &self.root;
    }
}

fn atomic_secret(path: &Path, secret: &[u8]) -> io::Result<()> {
    let temporary = path.with_extension(format!("tmp.{}", std::process::id()));
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    options.mode(0o600);
    let mut file = options.open(&temporary)?;
    file.write_all(secret)?;
    file.sync_all()?;
    drop(file);
    fs::rename(&temporary, path)?;
    set_mode(path, 0o600)?;
    Ok(())
}

fn atomic_json(path: &Path, value: &Value) -> io::Result<()> {
    let temporary = path.with_extension(format!("tmp.{}", std::process::id()));
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    options.mode(0o600);
    let mut file = options.open(&temporary)?;
    file.write_all(&serde_json::to_vec(value).map_err(io::Error::other)?)?;
    file.sync_all()?;
    drop(file);
    fs::rename(&temporary, path)?;
    set_mode(path, 0o600)?;
    Ok(())
}

fn set_mode(path: &Path, mode: u32) -> io::Result<()> {
    #[cfg(unix)]
    {
        fs::set_permissions(path, std::os::unix::fs::PermissionsExt::from_mode(mode))
    }
    #[cfg(not(unix))]
    {
        let _ = (path, mode);
        Ok(())
    }
}

enum Listener {
    #[cfg(unix)]
    Unix(UnixListener),
    Tcp(TcpListener),
}

enum Connection {
    #[cfg(unix)]
    Unix(UnixStream),
    Tcp(TcpStream),
}

impl Read for Connection {
    fn read(&mut self, bytes: &mut [u8]) -> io::Result<usize> {
        match self {
            #[cfg(unix)]
            Self::Unix(stream) => stream.read(bytes),
            Self::Tcp(stream) => stream.read(bytes),
        }
    }
}
impl Write for Connection {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        match self {
            #[cfg(unix)]
            Self::Unix(stream) => stream.write(bytes),
            Self::Tcp(stream) => stream.write(bytes),
        }
    }
    fn flush(&mut self) -> io::Result<()> {
        match self {
            #[cfg(unix)]
            Self::Unix(stream) => stream.flush(),
            Self::Tcp(stream) => stream.flush(),
        }
    }
}

fn server_loop(state: Arc<BridgeState>, hello: Value, documents: Vec<String>) {
    let (listener, endpoint) = match bind_listener(&state) {
        Ok(value) => value,
        Err(_) => return,
    };
    if state
        .rendezvous
        .publish(hello, documents, &endpoint)
        .is_err()
    {
        state.rendezvous.close();
        return;
    }
    loop {
        if state.stop.load(Ordering::Acquire) {
            break;
        }
        let accepted = match &listener {
            #[cfg(unix)]
            Listener::Unix(listener) => listener
                .accept()
                .map(|(stream, _)| Connection::Unix(stream)),
            Listener::Tcp(listener) => listener.accept().map(|(stream, _)| Connection::Tcp(stream)),
        };
        match accepted {
            Ok(connection) => serve_connection(&state, connection),
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                thread::sleep(Duration::from_millis(20))
            }
            Err(_) => break,
        }
    }
    state.rendezvous.close();
}

fn bind_listener(state: &BridgeState) -> io::Result<(Listener, String)> {
    #[cfg(unix)]
    if std::env::var_os("TOPOMIND_FORCE_TCP").is_none() {
        let _ = fs::remove_file(&state.rendezvous.socket_path);
        let listener = UnixListener::bind(&state.rendezvous.socket_path)?;
        listener.set_nonblocking(true)?;
        set_mode(&state.rendezvous.socket_path, 0o600)?;
        return Ok((
            Listener::Unix(listener),
            state.rendezvous.socket_path.to_string_lossy().into_owned(),
        ));
    }
    let listener = TcpListener::bind("127.0.0.1:0")?;
    listener.set_nonblocking(true)?;
    let address = listener.local_addr()?;
    Ok((Listener::Tcp(listener), format!("tcp://{address}")))
}

fn serve_connection(state: &Arc<BridgeState>, mut connection: Connection) {
    let mut authenticated = false;
    loop {
        let request: IpcRequest = match read_frame(&mut connection) {
            Ok(request) => request,
            Err(WireError::Io(error)) if error.kind() == io::ErrorKind::UnexpectedEof => return,
            Err(_) => return,
        };
        let result = state.enqueue(request, authenticated);
        authenticated = result.authenticated;
        if write_frame(&mut connection, &result.response).is_err() {
            return;
        }
    }
}

fn install_timer(py: Python<'_>) -> Result<Py<PyAny>, String> {
    let qtcore = optional_import(py, "PySide.QtCore")
        .or_else(|| optional_import(py, "PySide6.QtCore"))
        .ok_or_else(|| "FreeCAD QtCore module is unavailable".to_string())?;
    let timer = qtcore
        .getattr("QTimer")
        .map_err(|error| error.to_string())?
        .call0()
        .map_err(|error| error.to_string())?;
    timer
        .call_method1("setInterval", (10_i32,))
        .map_err(|error| error.to_string())?;
    let poller = Py::new(py, Poller).map_err(|error| error.to_string())?;
    timer
        .getattr("timeout")
        .map_err(|error| error.to_string())?
        .call_method1("connect", (poller,))
        .map_err(|error| error.to_string())?;
    timer
        .call_method0("start")
        .map_err(|error| error.to_string())?;
    Ok(timer.unbind())
}

pub fn start_bridge(py: Python<'_>) -> PyResult<String> {
    if let Some(state) = bridge_slot().lock().expect("bridge lock poisoned").as_ref() {
        return Ok(state.rendezvous.record_path.to_string_lossy().into_owned());
    }
    let app = import(py, "FreeCAD")?;
    let gui = optional_import(py, "FreeCADGui");
    let part = optional_import(py, "Part");
    let state = BridgeState::new(
        py,
        app.as_any(),
        gui.as_ref().map(|value| value.as_any()),
        part.as_ref().map(|value| value.as_any()),
    )
    .map_err(pyo3::exceptions::PyRuntimeError::new_err)?;
    state
        .start(py)
        .map_err(pyo3::exceptions::PyRuntimeError::new_err)?;
    let record = state.rendezvous.record_path.to_string_lossy().into_owned();
    *bridge_slot().lock().expect("bridge lock poisoned") = Some(state);
    Ok(record)
}

pub fn stop_bridge(py: Python<'_>) {
    if let Some(state) = bridge_slot().lock().expect("bridge lock poisoned").take() {
        state.stop(py);
    }
}

pub fn pairing_status(py: Python<'_>) -> PyResult<Py<PyAny>> {
    let value = bridge_slot()
        .lock()
        .expect("bridge lock poisoned")
        .as_ref()
        .map(|state| state.pairing_status())
        .unwrap_or_else(|| json!({"state": "stopped"}));
    py_value(py, &value)
}

pub fn show_pairing(py: Python<'_>) -> PyResult<Py<PyAny>> {
    let value = pairing_status(py)?;
    if let Some(gui) = optional_import(py, "FreeCADGui") {
        if let Some(widgets) = optional_import(py, "PySide.QtWidgets")
            .or_else(|| optional_import(py, "PySide6.QtWidgets"))
        {
            if let (Ok(dialog), Ok(window)) = (
                widgets.getattr("QMessageBox"),
                gui.getattr("getMainWindow")
                    .and_then(|method| method.call0()),
            ) {
                let text = value.bind(py).str()?.to_string();
                let _ = dialog
                    .getattr("information")
                    .and_then(|method| method.call1((window, "Topomind pairing", text)));
            }
        }
    }
    Ok(value)
}

pub fn poll(py: Python<'_>) {
    if let Some(state) = bridge_slot()
        .lock()
        .expect("bridge lock poisoned")
        .as_ref()
        .cloned()
    {
        state.process_pending(py);
    }
}

#[pyclass]
pub struct Poller;

#[pymethods]
impl Poller {
    fn poll(&self, py: Python<'_>) {
        poll(py);
    }
}

#[pyclass]
struct DocumentObserver {
    state: Arc<BridgeState>,
}

#[pymethods]
impl DocumentObserver {
    #[pyo3(name = "slotCreatedObject")]
    fn slot_created_object(&self, _document: &Bound<'_, PyAny>, object: &Bound<'_, PyAny>) {
        self.state
            .events
            .emit("geometry", string_attr(object, "Name"));
        self.state.flush_events(false);
    }
    #[pyo3(name = "slotDeletedObject")]
    fn slot_deleted_object(&self, _document: &Bound<'_, PyAny>, name: String) {
        self.state.events.emit("geometry", Some(name));
        self.state.flush_events(false);
    }
    #[pyo3(name = "slotChangedObject")]
    fn slot_changed_object(
        &self,
        _document: &Bound<'_, PyAny>,
        object: &Bound<'_, PyAny>,
        property: String,
    ) {
        let class = if property == "Label" || property == "Label2" {
            "metadata"
        } else {
            "parametric"
        };
        self.state.events.emit(class, string_attr(object, "Name"));
        self.state.flush_events(false);
    }
    #[pyo3(name = "slotRecomputedDocument")]
    fn slot_recomputed_document(&self, _document: &Bound<'_, PyAny>) {
        self.state.events.emit("geometry", None);
        self.state.flush_events(false);
    }
    #[pyo3(name = "transactionOpened")]
    fn transaction_opened(&self, _document: &Bound<'_, PyAny>) {
        self.state.events.emit("transaction", None);
        self.state.flush_events(true);
    }
    #[pyo3(name = "transactionCommitted")]
    fn transaction_committed(&self, _document: &Bound<'_, PyAny>) {
        self.state.events.emit("transaction", None);
        self.state.flush_events(true);
    }
    #[pyo3(name = "transactionAborted")]
    fn transaction_aborted(&self, _document: &Bound<'_, PyAny>) {
        self.state.events.emit("transaction", None);
        self.state.flush_events(true);
    }
}

#[pyclass]
struct SelectionObserver {
    state: Arc<BridgeState>,
}

#[pymethods]
impl SelectionObserver {
    #[pyo3(name = "addSelection")]
    fn add_selection(
        &self,
        _document: &Bound<'_, PyAny>,
        object_name: String,
        _subelement: String,
        _position: &Bound<'_, PyAny>,
    ) {
        self.state.events.emit("focus", Some(object_name));
        self.state.flush_events(false);
    }
    #[pyo3(name = "clearSelection")]
    fn clear_selection(&self, _document: &Bound<'_, PyAny>) {
        self.state.events.emit("focus", None);
        self.state.flush_events(false);
    }
    #[pyo3(name = "setPreselection")]
    fn set_preselection(
        &self,
        _document: &Bound<'_, PyAny>,
        object_name: String,
        _subelement: String,
    ) {
        self.state.events.emit("focus", Some(object_name));
        self.state.flush_events(false);
    }
    #[pyo3(name = "removeSelection")]
    fn remove_selection(
        &self,
        _document: &Bound<'_, PyAny>,
        object_name: String,
        _subelement: String,
    ) {
        self.state.events.emit("focus", Some(object_name));
        self.state.flush_events(false);
    }
}

fn python_version(py: Python<'_>) -> String {
    import(py, "platform")
        .ok()
        .and_then(|platform| platform.getattr("python_version").ok())
        .and_then(|method| method.call0().ok())
        .and_then(|value| value.extract().ok())
        .unwrap_or_else(|| "unknown".into())
}

fn revision_id(snapshot: &Value) -> String {
    snapshot
        .get("revision")
        .map(|revision| {
            format!(
                "g{}.m{}.f{}.v{}.{}",
                revision
                    .get("geometry")
                    .and_then(Value::as_u64)
                    .unwrap_or_default(),
                revision
                    .get("metadata")
                    .and_then(Value::as_u64)
                    .unwrap_or_default(),
                revision
                    .get("focus")
                    .and_then(Value::as_u64)
                    .unwrap_or_default(),
                revision
                    .get("view")
                    .and_then(Value::as_u64)
                    .unwrap_or_default(),
                revision
                    .get("epoch")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
            )
        })
        .unwrap_or_default()
}
fn revision_matches(value: Option<&str>, snapshot: &Value) -> bool {
    let Some(value) = value else {
        return false;
    };
    value == revision_id(snapshot)
        || value
            == format!(
                "g{}.{}",
                snapshot["revision"]["geometry"]
                    .as_u64()
                    .unwrap_or_default(),
                snapshot["revision"]["epoch"].as_str().unwrap_or_default()
            )
}
fn changeset_hash(changeset: &Value) -> Result<String, BridgeFault> {
    let mut normalized = changeset.clone();
    if let Some(object) = normalized.as_object_mut() {
        object.remove("approval_token");
    }
    sha256_json(&normalized)
        .map_err(|error| BridgeFault::new("internal_inconsistency", error.to_string(), false))
}
fn snapshot_fingerprint(snapshot: &Value) -> Result<String, BridgeFault> {
    let mut normalized = snapshot.clone();
    if let Some(snapshot) = normalized.as_object_mut() {
        snapshot.remove("revision");
        if let Some(view) = snapshot.get_mut("view").and_then(Value::as_object_mut) {
            view.remove("revision");
            view.remove("timestamp_ms");
        }
    }
    sha256_json(&normalized)
        .map_err(|error| BridgeFault::new("internal_inconsistency", error.to_string(), false))
}
fn property_value(value: &Value, path: &str) -> Value {
    path.split('.')
        .fold(value, |current, part| {
            current.get(part).unwrap_or(&Value::Null)
        })
        .clone()
}
fn has_subelement(reference: &str) -> bool {
    reference
        .split('@')
        .next()
        .unwrap_or_default()
        .split('/')
        .any(|part| ["face", "edge", "vertex", "constraint"].contains(&part))
}
fn error_code(message: &str) -> String {
    if message.contains("busy") {
        "bridge_busy"
    } else if message.contains("revision") {
        "revision_conflict"
    } else if message.contains("validation") {
        "validation_failed"
    } else {
        "invalid_request"
    }
    .into()
}
#[allow(clippy::too_many_arguments)]
fn change_response(
    base: &str,
    resulting: Option<&str>,
    changeset_hash: &str,
    preview_hash: Option<&str>,
    rolled_back: bool,
    rollback_verified: bool,
    diff: Value,
    checks: Vec<Value>,
) -> Value {
    json!({"base_revision": base, "resulting_revision": resulting, "changeset_hash": changeset_hash, "preview_hash": preview_hash, "rolled_back": rolled_back, "rollback_fingerprint_verified": rollback_verified, "diff": diff, "validation": {"status": if checks.iter().any(|check| check.get("status").and_then(Value::as_str) == Some("fail")) {"fail"} else {"pass"}, "checks": checks}})
}
fn now_ms() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis())
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn event_coalescer_keeps_the_latest_event_per_class() {
        let events = EventCoalescer::new();
        events.emit("view", Some("one".into()));
        events.emit("view", Some("two".into()));
        events.emit("transaction", None);
        assert_eq!(events.flush(true), vec!["view", "transaction"]);
    }

    #[test]
    fn revision_matching_accepts_full_and_geometry_revisions() {
        let revision = json!({"geometry": 4, "metadata": 2, "focus": 1, "view": 3, "epoch": "e"});
        let snapshot = json!({"revision": revision});
        assert!(revision_matches(Some("g4.m2.f1.v3.e"), &snapshot));
        assert!(revision_matches(Some("g4.e"), &snapshot));
        assert!(!revision_matches(Some("g3.e"), &snapshot));
    }

    #[test]
    fn snapshot_fingerprint_ignores_revision_and_view_clock() {
        let first = json!({
            "revision": {"geometry": 1},
            "view": {"revision": "g1", "timestamp_ms": 10},
            "entities": []
        });
        let second = json!({
            "revision": {"geometry": 2},
            "view": {"revision": "g2", "timestamp_ms": 20},
            "entities": []
        });
        assert_eq!(
            snapshot_fingerprint(&first).unwrap(),
            snapshot_fingerprint(&second).unwrap()
        );
    }
}
