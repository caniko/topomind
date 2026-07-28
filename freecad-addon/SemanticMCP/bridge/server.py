"""FreeCAD-side authenticated bridge and transaction coordinator."""

from __future__ import annotations

import os
import queue
import socket
import threading
import time
import uuid
from dataclasses import asdict
from typing import Any

from ..adapters.core import core_registry
from ..extractors.snapshot import FreeCADExtractor, RevisionBook
from ..observers.events import EventEnvelope, ObserverRegistry
from ..operations.typed import OperationError, TypedExecutor
from .auth import Rendezvous
from .protocol import DTO_VERSION, IPC_VERSION, ProtocolError, canonical_json, read_frame, sha256_json, write_frame


class BridgeError(RuntimeError):
    """Safe bridge error; kernel/Python traces remain local."""


class FreeCADBridge:
    def __init__(self, app: Any = None, gui: Any = None, part: Any = None, runtime_root: str | None = None) -> None:
        self.app, self.gui, self.part = app or _import("FreeCAD"), gui if gui is not None else _optional_import("FreeCADGui"), part if part is not None else _optional_import("Part")
        self.rendezvous = Rendezvous(runtime_root)
        self.book = RevisionBook(self.rendezvous.epoch)
        self.extractor = FreeCADExtractor(self.app, self.gui, self.part)
        self.executor = TypedExecutor(self.app, self.gui, self.part)
        self.adapters = core_registry()
        self.observers = ObserverRegistry(self._on_event)
        self.observers.install(self.app, self.gui)
        self.preview_records: dict[str, dict[str, Any]] = {}
        self._main_thread_id = threading.get_ident()
        self._pending: queue.Queue[tuple[str, dict[str, Any], threading.Event, dict[str, Any]]] = queue.Queue()
        self._pending_schedule_lock = threading.Lock()
        self._pending_scheduled = False
        self._stop = threading.Event()
        self._server: socket.socket | None = None
        self._write_blocked = False
        self.started = False

    def hello(self) -> dict[str, Any]:
        freecad_version = _freecad_version(self.app)
        write_capabilities = ["write_selection", "write_view", "write_model_low_risk"] if _at_least(freecad_version, (1, 1, 3)) else []
        return {
            "message": "bridge.hello",
            "ipc_versions": [IPC_VERSION],
            "dto_versions": [DTO_VERSION],
            "freecad": {"version": freecad_version, "python": _python_version(), "occt": _occt_version(self.app), "mode": "gui" if self.gui is not None else "headless", "active_workbench": self.extractor._active_workbench()},
            "extractors": ["core.document/1", "sketcher/1", "part/1", "partdesign/1", "view/1"],
            "operations": sorted({"set_property", "set_expression", "sketch_set_datum", "create_primitive", "boolean", "set_visibility", "set_selection", "set_view", "create_object", "delete_object"} | set(self.adapters.capabilities())),
            "transaction_safety": {"core": "verified", "unknown_third_party_objects": "read_only"},
            "limits": {"max_control_message_bytes": 4 * 1024 * 1024, "max_pending_requests": 64, "max_entities_per_snapshot": 50_000},
            "capabilities": ["read_document_structure", "read_exact_geometry", *write_capabilities],
            "compatibility": "supported" if write_capabilities else "read_only_below_design_baseline",
        }

    def serve_forever(self) -> None:
        use_unix = hasattr(socket, "AF_UNIX") and not os.environ.get("TOPOMIND_FORCE_TCP")
        if use_unix:
            self.rendezvous.socket.unlink(missing_ok=True)
            server = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
            server.bind(str(self.rendezvous.socket))
            os.chmod(self.rendezvous.socket, 0o600)
            endpoint = str(self.rendezvous.socket)
        else:
            server = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
            server.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
            server.bind(("127.0.0.1", 0))
            host, port = server.getsockname()
            endpoint = f"tcp://{host}:{port}"
        server.listen(4)
        server.settimeout(0.5)
        self._server = server
        self.started = True
        self.rendezvous.publish(self.hello(), self._documents(), endpoint)
        try:
            while not self._stop.is_set():
                try:
                    connection, _ = server.accept()
                except socket.timeout:
                    continue
                except OSError:
                    if self._stop.is_set():
                        break
                    raise
                with connection:
                    self._serve_connection(connection)
        finally:
            server.close()
            self._server = None
            self.observers.remove(self.app, self.gui)
            self.rendezvous.close()

    def start_background(self) -> threading.Thread:
        thread = threading.Thread(target=self.serve_forever, name="topomind-freecad-bridge", daemon=True)
        thread.start()
        return thread

    def stop(self) -> None:
        self._stop.set()
        if self._server is not None:
            try:
                self._server.close()
            except OSError:
                pass

    def handle_request(self, request: dict[str, Any], authenticated: bool) -> tuple[dict[str, Any], bool]:
        message = request.get("message")
        request_id = request.get("request_id", "unknown")
        if request.get("protocol_version") not in {None, IPC_VERSION}:
            return self._response(request_id, "error", {"code": "invalid_request", "message": "unsupported IPC protocol version"}), False
        if message == "bridge.authenticate":
            payload = request.get("payload") or {}
            if not self.rendezvous.pairing.verify(str(payload.get("nonce", "")), str(payload.get("proof", ""))):
                return self._response(request_id, "error", {"code": "permission_denied", "message": "bridge authentication failed"}), False
            return self._response(request_id, "ok", {"session": self.rendezvous.session, "hello": self.hello(), "documents": self._documents()}), True
        if not authenticated:
            return self._response(request_id, "error", {"code": "permission_denied", "message": "bridge must be authenticated"}), False
        if request.get("session_epoch") != self.rendezvous.epoch:
            return self._response(request_id, "error", {"code": "session_unavailable", "message": "session epoch is stale"}), False
        try:
            payload = self._dispatch_on_main(message, request.get("payload") or {})
            return self._response(request_id, "ok", payload), True
        except (BridgeError, OperationError, KeyError, ValueError) as error:
            return self._response(request_id, "error", {"code": _error_code(error), "message": str(error)}), True
        except Exception:
            return self._response(request_id, "error", {"code": "internal_inconsistency", "message": "bridge operation failed; inspect local diagnostics"}), True

    def _serve_connection(self, connection: socket.socket) -> None:
        authenticated = False
        stream = connection.makefile("rwb", buffering=0)
        while True:
            try:
                request = read_frame(stream)
            except EOFError:
                return
            except (ProtocolError, ValueError) as error:
                write_frame(stream, self._response("unknown", "error", {"code": "invalid_request", "message": str(error)}))
                return
            response, authenticated = self.handle_request(request, authenticated)
            write_frame(stream, response)

    def _dispatch_on_main(self, message: str, payload: dict[str, Any]) -> dict[str, Any]:
        if threading.get_ident() == self._main_thread_id:
            return self._dispatch(message, payload)
        done = threading.Event()
        result: dict[str, Any] = {}
        self._pending.put((message, payload, done, result))
        self._schedule_pending()
        if not done.wait(10.0):
            raise BridgeError("bridge_busy")
        if "error" in result:
            raise result["error"]
        return result["value"]

    def _schedule_pending(self) -> None:
        with self._pending_schedule_lock:
            if self._pending_scheduled:
                return
            self._pending_scheduled = True
        try:
            qtcore = _qt_core()
            if qtcore is None:
                raise BridgeError("bridge_busy: FreeCAD main-thread scheduler unavailable")
            qtcore.QTimer.singleShot(0, self.process_pending)
        except Exception as error:
            with self._pending_schedule_lock:
                self._pending_scheduled = False
            while True:
                try:
                    _, _, done, result = self._pending.get_nowait()
                except queue.Empty:
                    break
                result["error"] = error
                done.set()

    def process_pending(self) -> None:
        if threading.get_ident() != self._main_thread_id:
            return
        try:
            while True:
                message, payload, done, result = self._pending.get_nowait()
                try:
                    result["value"] = self._dispatch(message, payload)
                except Exception as error:
                    result["error"] = error
                finally:
                    done.set()
        except queue.Empty:
            pass
        finally:
            with self._pending_schedule_lock:
                self._pending_scheduled = False
            if not self._pending.empty():
                self._schedule_pending()

    def _dispatch(self, message: str, payload: dict[str, Any]) -> dict[str, Any]:
        self.observers.flush(force=payload.get("freshness") == "synchronized" or message in {"bridge.inspect", "bridge.preview", "bridge.commit", "bridge.undo", "bridge.redo"})
        if message == "bridge.snapshot":
            return self.extractor.snapshot(self.rendezvous.session, self.book, payload.get("document"))
        if message == "bridge.inspect":
            snapshot = self.extractor.snapshot(self.rendezvous.session, self.book, payload.get("document"))
            reference = payload.get("ref")
            entity = next((entity for entity in snapshot["entities"] if entity["ref"] == reference), None)
            if entity is None:
                raise BridgeError("entity is not present in the current snapshot")
            return {"entity": entity, "revision": snapshot["revision"]}
        if message == "bridge.preview":
            return self._preview(payload)
        if message == "bridge.commit":
            return self._commit(payload)
        if message == "bridge.undo":
            self.executor.undo(payload.get("document"))
            self.book.bump("transaction")
            return {"resulting_revision": self.book.id(), "validation": {"status": "pass", "checks": [{"id": "undo", "status": "pass", "message": "FreeCAD undo completed", "evidence": []}]}}
        if message == "bridge.redo":
            self.executor.redo(payload.get("document"))
            self.book.bump("transaction")
            return {"resulting_revision": self.book.id(), "validation": {"status": "pass", "checks": [{"id": "redo", "status": "pass", "message": "FreeCAD redo completed", "evidence": []}]}}
        raise BridgeError(f"unsupported bridge message: {message}")

    def _preview(self, payload: dict[str, Any]) -> dict[str, Any]:
        changeset = payload.get("changeset")
        if not isinstance(changeset, dict):
            raise BridgeError("changeset is required")
        before = self.extractor.snapshot(self.rendezvous.session, self.book, changeset.get("document"))
        self._validate_changeset(changeset, before)
        base_revision = _revision_id(before["revision"])
        if changeset.get("base_revision") not in {base_revision, f"g{before['revision']['geometry']}.{before['revision']['epoch']}"}:
            raise BridgeError("revision_conflict")
        base_fingerprint = sha256_json(before)
        changeset_hash = _changeset_hash(changeset)
        counters = (self.book.geometry, self.book.metadata, self.book.focus, self.book.view)
        operation_result = self.executor.apply(changeset, commit=False)
        self._bump_for_changeset(changeset)
        preview_snapshot = self.extractor.snapshot(self.rendezvous.session, self.book, changeset.get("document"))
        preview_fingerprint = sha256_json(preview_snapshot)
        self.book.geometry, self.book.metadata, self.book.focus, self.book.view = counters
        after_abort = self.extractor.snapshot(self.rendezvous.session, self.book, changeset.get("document"))
        rollback_verified = sha256_json(after_abort) == base_fingerprint
        if not rollback_verified:
            self._write_blocked = True
            raise BridgeError("rollback_verification_failed")
        preview_hash = sha256_json({"base_revision": base_revision, "base_fingerprint": base_fingerprint, "preview_fingerprint": preview_fingerprint, "changeset_hash": changeset_hash})
        self.preview_records[changeset_hash] = {"base_revision": base_revision, "base_fingerprint": base_fingerprint, "preview_hash": preview_hash}
        return {"base_revision": base_revision, "resulting_revision": _revision_id(preview_snapshot["revision"]), "changeset_hash": changeset_hash, "preview_hash": preview_hash, "rolled_back": True, "rollback_fingerprint_verified": rollback_verified, "diff": {"changed_entities": operation_result.changed, "objects_recomputed": len(operation_result.changed)}, "validation": {"status": "pass", "checks": operation_result.checks}}

    def _commit(self, payload: dict[str, Any]) -> dict[str, Any]:
        changeset = payload.get("changeset")
        if not isinstance(changeset, dict):
            raise BridgeError("changeset is required")
        changeset_hash = _changeset_hash(changeset)
        record = self.preview_records.get(changeset_hash)
        if record is None or record["preview_hash"] != payload.get("preview_hash"):
            raise BridgeError("preview hash is not known or does not match")
        before = self.extractor.snapshot(self.rendezvous.session, self.book, changeset.get("document"))
        self._validate_changeset(changeset, before)
        if sha256_json(before) != record["base_fingerprint"]:
            raise BridgeError("revision_conflict")
        operation_result = self.executor.apply(changeset, commit=True)
        self._bump_for_changeset(changeset)
        after = self.extractor.snapshot(self.rendezvous.session, self.book, changeset.get("document"))
        self.preview_records.pop(changeset_hash, None)
        return {"base_revision": record["base_revision"], "resulting_revision": _revision_id(after["revision"]), "changeset_hash": changeset_hash, "preview_hash": record["preview_hash"], "rolled_back": False, "rollback_fingerprint_verified": True, "diff": {"changed_entities": operation_result.changed}, "validation": {"status": "pass", "checks": operation_result.checks}}

    def _on_event(self, event: EventEnvelope) -> None:
        self.book.bump(event.event_class)

    def _validate_changeset(self, changeset: dict[str, Any], snapshot: dict[str, Any]) -> None:
        if self._write_blocked:
            raise BridgeError("writes are blocked after an unverified rollback; restart the bridge")
        if not _at_least(_freecad_version(self.app), (1, 1, 3)):
            raise BridgeError("write support requires FreeCAD 1.1.3+")
        if changeset.get("schema_version") != "changeset/1.0":
            raise BridgeError("unsupported changeset schema")
        if changeset.get("session") not in {None, "", self.rendezvous.session}:
            raise BridgeError("session does not belong to this bridge")
        if not isinstance(changeset.get("document"), str) or changeset["document"] != snapshot.get("document"):
            raise BridgeError("document is not the active bridge document")
        if not isinstance(changeset.get("operations"), list) or not changeset["operations"]:
            raise BridgeError("changeset must contain operations")
        current = _revision_id(snapshot["revision"])
        if changeset.get("base_revision") not in {current, f"g{snapshot['revision']['geometry']}.{snapshot['revision']['epoch']}"}:
            raise BridgeError("revision_conflict")
        entities = {entity["ref"]: entity for entity in snapshot.get("entities", [])}
        for precondition in changeset.get("preconditions", []):
            if not isinstance(precondition, dict):
                raise BridgeError("invalid precondition")
            kind = precondition.get("kind")
            if kind == "revision_equals" and precondition.get("revision") not in {current, f"g{snapshot['revision']['geometry']}.{snapshot['revision']['epoch']}"}:
                raise BridgeError("revision_conflict")
            if kind in {"entity_exists", "entity_kind", "property_equals"}:
                target = precondition.get("target")
                entity = entities.get(target)
                if entity is None:
                    raise BridgeError("stale_reference")
                if kind == "entity_kind" and entity.get("kind") != precondition.get("expected_kind"):
                    raise BridgeError("precondition_failed")
                if kind == "property_equals" and _property_value(entity.get("properties", {}), precondition.get("property", "")) != precondition.get("value"):
                    raise BridgeError("precondition_failed")
            elif kind not in {"revision_equals", "entity_exists", "entity_kind", "property_equals"}:
                raise BridgeError("unsupported precondition")
        for operation in changeset["operations"]:
            if not isinstance(operation, dict) or not isinstance(operation.get("op"), str):
                raise BridgeError("invalid typed operation")
            for key in ("target", "constraint", "left", "right"):
                reference = operation.get(key)
                if reference is None:
                    continue
                entity = entities.get(reference)
                if entity is None or entity.get("identity", {}).get("state") not in {"exact", "mapped_exact"}:
                    raise BridgeError("stale_reference")
            if operation.get("op") in {"set_property", "set_expression", "set_visibility", "delete_object"} and any(part in str(operation.get("target", "")).split("@", 1)[0].split("/") for part in ("face", "edge", "vertex", "constraint")):
                raise BridgeError("operation target must be an object, not a sub-element")
            if operation.get("op") == "boolean" and any(any(part in str(operation.get(key, "")).split("@", 1)[0].split("/") for part in ("face", "edge", "vertex", "constraint")) for key in ("left", "right")):
                raise BridgeError("boolean operands must be objects, not sub-elements")

    def _bump_for_changeset(self, changeset: dict[str, Any]) -> None:
        classes = set()
        for operation in changeset.get("operations", []):
            name = operation.get("op") if isinstance(operation, dict) else None
            classes.add({"set_selection": "focus", "set_view": "view", "set_visibility": "metadata"}.get(name, "geometry"))
        for event_class in sorted(classes):
            self.book.bump(event_class)

    def _response(self, request_id: str, status: str, payload: dict[str, Any]) -> dict[str, Any]:
        diagnostics = []
        if status != "ok":
            diagnostics.append({"code": payload.get("code", "internal_inconsistency"), "severity": "error", "message": payload.get("message", "bridge error"), "entity": None, "retryable": payload.get("code") in {"bridge_busy", "revision_conflict"}, "evidence": []})
        return {"protocol_version": IPC_VERSION, "message": "bridge.response", "request_id": request_id, "status": status, "session_epoch": self.rendezvous.epoch, "payload": payload if status == "ok" else None, "diagnostics": diagnostics}

    def _documents(self) -> list[str]:
        documents = getattr(self.app, "Documents", {})
        if isinstance(documents, dict):
            return sorted(str(name) for name in documents)
        return [str(getattr(document, "Name", "")) for document in list(getattr(self.app, "listDocuments", lambda: {})().values())]


def _import(name: str) -> Any:
    module = __import__(name)
    return module


def _optional_import(name: str) -> Any:
    try:
        return _import(name)
    except ImportError:
        return None


def _qt_core() -> Any:
    for module_name in ("PySide.QtCore", "PySide6.QtCore"):
        try:
            return __import__(module_name, fromlist=["QtCore"])
        except ImportError:
            continue
    return None


def _freecad_version(app: Any) -> str:
    version = getattr(app, "Version", None)
    try:
        value = version() if callable(version) else version
        return ".".join(str(part) for part in value) if isinstance(value, (tuple, list)) else str(value or "unknown")
    except Exception:
        return "unknown"


def _occt_version(app: Any) -> str:
    getter = getattr(app, "getOpenSCADVersion", None)
    try:
        return str(getter()) if getter else "reported-at-runtime"
    except Exception:
        return "reported-at-runtime"


def _python_version() -> str:
    import platform

    return platform.python_version()


def _revision_id(revision: dict[str, Any]) -> str:
    return f"g{revision['geometry']}.m{revision['metadata']}.f{revision['focus']}.v{revision['view']}.{revision['epoch']}"


def _changeset_hash(changeset: dict[str, Any]) -> str:
    normalized = dict(changeset)
    normalized.pop("approval_token", None)
    return sha256_json(normalized)


def _property_value(value: Any, path: str) -> Any:
    current = value
    for part in path.split("."):
        if not isinstance(current, dict) or part not in current:
            return None
        current = current[part]
    return current


def _error_code(error: Exception) -> str:
    message = str(error)
    if "bridge_busy" in message:
        return "bridge_busy"
    if "rollback_verification_failed" in message:
        return "rollback_verification_failed"
    if "revision_conflict" in message:
        return "revision_conflict"
    if "validation" in message:
        return "validation_failed"
    return "invalid_request"


def _at_least(version: str, required: tuple[int, ...]) -> bool:
    try:
        actual = tuple(int(part) for part in version.split(".")[:3])
    except (AttributeError, TypeError, ValueError):
        return False
    return actual >= required
