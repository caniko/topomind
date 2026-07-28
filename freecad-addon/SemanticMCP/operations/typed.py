"""Typed ChangeSet execution; no arbitrary Python reaches this module."""

from __future__ import annotations

from dataclasses import dataclass
import math
from typing import Any
from urllib.parse import unquote


class OperationError(RuntimeError):
    """A ChangeSet operation is invalid or cannot be applied safely."""


@dataclass
class OperationResult:
    changed: list[str]
    checks: list[dict[str, Any]]


class TypedExecutor:
    ALLOWED_OBJECT_TYPES = {
        "Part::Feature",
        "PartDesign::Feature",
        "PartDesign::Body",
        "PartDesign::FeaturePython",
        "Sketcher::SketchObject",
    }

    def __init__(self, app: Any, gui: Any = None, part: Any = None) -> None:
        self.app = app
        self.gui = gui
        self.part = part

    def apply(self, changeset: dict[str, Any], *, commit: bool) -> OperationResult:
        document = self._document(changeset.get("document"))
        if document is None:
            raise OperationError("document is not open")
        operations = changeset.get("operations")
        if not isinstance(operations, list) or not operations:
            raise OperationError("changeset must contain at least one operation")
        open_transaction = getattr(document, "openTransaction", None)
        if open_transaction:
            open_transaction(f"Topomind {changeset.get('request_id', 'change')}")
        changed: list[str] = []
        try:
            for operation in operations:
                changed.extend(self._apply_operation(document, operation))
            checks = self.validate(document, changeset.get("validate", []))
            if any(check["status"] == "fail" for check in checks):
                raise OperationError("validation failed: " + "; ".join(check["message"] for check in checks if check["status"] == "fail"))
            if commit:
                if hasattr(document, "commitTransaction"):
                    document.commitTransaction()
            elif hasattr(document, "abortTransaction"):
                document.abortTransaction()
            return OperationResult(changed=changed, checks=checks)
        except Exception:
            if hasattr(document, "abortTransaction"):
                document.abortTransaction()
            raise

    def validate(self, document: Any, requested: list[str]) -> list[dict[str, Any]]:
        if hasattr(document, "recompute"):
            document.recompute()
        checks: list[dict[str, Any]] = []
        requested = requested or ["recompute", "shape_validity"]
        if "recompute" in requested:
            errors = [getattr(obj, "Name", "object") for obj in list(getattr(document, "Objects", []) or []) if "Error" in [str(state) for state in (getattr(obj, "State", []) or [])]]
            checks.append({"id": "recompute", "status": "fail" if errors else "pass", "message": f"objects with errors: {errors}" if errors else "recompute completed", "evidence": errors})
        if "shape_validity" in requested:
            invalid = []
            for obj in list(getattr(document, "Objects", []) or []):
                shape = getattr(obj, "Shape", None)
                if shape is not None and hasattr(shape, "isValid") and not shape.isValid():
                    invalid.append(getattr(obj, "Name", "object"))
            checks.append({"id": "shape_validity", "status": "fail" if invalid else "pass", "message": f"invalid shapes: {invalid}" if invalid else "all reported shapes are valid", "evidence": invalid})
        if "sketch_solver" in requested:
            conflicting = [getattr(obj, "Name", "sketch") for obj in list(getattr(document, "Objects", []) or []) if "Sketcher::SketchObject" in str(getattr(obj, "TypeId", "")) and "Conflict" in str(getattr(obj, "SolverMessages", ""))]
            checks.append({"id": "sketch_solver", "status": "fail" if conflicting else "pass", "message": f"conflicting sketches: {conflicting}" if conflicting else "sketch solver reports no conflicts", "evidence": conflicting})
        return checks

    def undo(self, document_name: str | None = None) -> None:
        document = self._document(document_name)
        if document is None or not hasattr(document, "undo"):
            raise OperationError("undo is unavailable")
        document.undo()

    def redo(self, document_name: str | None = None) -> None:
        document = self._document(document_name)
        if document is None or not hasattr(document, "redo"):
            raise OperationError("redo is unavailable")
        document.redo()

    def _document(self, name: str | None) -> Any:
        if name:
            documents = getattr(self.app, "Documents", {})
            if isinstance(documents, dict):
                return documents.get(name)
            getter = getattr(self.app, "getDocument", None)
            return getter(name) if getter else None
        return getattr(self.app, "ActiveDocument", None)

    def _apply_operation(self, document: Any, operation: dict[str, Any]) -> list[str]:
        name = operation.get("op")
        if name == "set_property":
            obj, _ = self._resolve(document, operation["target"])
            property_name = self._property_name(operation["property"])
            if property_name not in list(getattr(obj, "PropertiesList", []) or []):
                raise OperationError(f"property is not declared on the target: {property_name}")
            setattr(obj, property_name, _typed_value(operation["value"]))
            return [operation["target"]]
        if name == "set_expression":
            obj, _ = self._resolve(document, operation["target"])
            property_name = self._property_name(operation["property"])
            expression = operation.get("expression")
            if property_name not in list(getattr(obj, "PropertiesList", []) or []) or not isinstance(expression, str):
                raise OperationError("set_expression requires a declared property and string expression")
            setter = getattr(obj, "setExpression", None)
            if setter is None:
                raise OperationError("target does not support expressions")
            setter(property_name, expression)
            return [operation["target"]]
        if name == "sketch_set_datum":
            obj, parts = self._resolve(document, operation["constraint"])
            if "Sketcher::SketchObject" not in str(getattr(obj, "TypeId", "")):
                raise OperationError("sketch.set_datum target is not a Sketcher object")
            index = _subelement_index(parts, "constraint")
            setter = getattr(obj, "setDatum", None)
            if setter is None:
                raise OperationError("sketch does not expose setDatum")
            setter(index, _typed_value(operation["value"]))
            return [operation["constraint"]]
        if name == "set_visibility":
            obj, _ = self._resolve(document, operation["target"])
            view_object = getattr(obj, "ViewObject", None)
            if view_object is None:
                raise OperationError("target has no view object")
            view_object.Visibility = bool(operation["visible"])
            return [operation["target"]]
        if name == "set_selection":
            if self.gui is None:
                raise OperationError("selection requires GUI mode")
            selection = getattr(self.gui, "Selection", None)
            if selection is None:
                raise OperationError("FreeCAD selection API is unavailable")
            selection.clearSelection()
            for target in operation.get("targets", []):
                obj, parts = self._resolve(document, target)
                subelement = _subelement_name(parts)
                selection.addSelection(obj, subelement) if subelement else selection.addSelection(obj)
            return list(operation.get("targets", []))
        if name == "set_view":
            return self._set_view(operation)
        if name == "create_primitive":
            return [self._create_primitive(document, operation)]
        if name == "create_object":
            kind = operation.get("kind")
            if kind not in self.ALLOWED_OBJECT_TYPES:
                raise OperationError(f"object type is not allowlisted: {kind}")
            obj = document.addObject(kind, operation["object"])
            for property_name, value in (operation.get("properties") or {}).items():
                if property_name not in list(getattr(obj, "PropertiesList", []) or []):
                    raise OperationError(f"property is not declared on the created target: {property_name}")
                setattr(obj, property_name, _typed_value(value))
            return [operation["object"]]
        if name == "delete_object":
            obj, _ = self._resolve(document, operation["target"])
            if operation.get("require_no_dependents", True) and list(getattr(obj, "InList", []) or []):
                raise OperationError("delete would leave dependent objects")
            document.removeObject(obj.Name)
            return [operation["target"]]
        if name == "boolean":
            return [self._boolean(document, operation)]
        raise OperationError(f"operation is not supported: {name}")

    def _set_view(self, operation: dict[str, Any]) -> list[str]:
        if self.gui is None:
            raise OperationError("view requires GUI mode")
        view = getattr(getattr(self.gui, "ActiveDocument", None), "ActiveView", None)
        if view is None:
            raise OperationError("active view is unavailable")
        action = operation.get("operation")
        allowed = {"fit_all", "view_axo", "view_front", "view_rear", "view_left", "view_right", "view_top", "view_bottom"}
        if action not in allowed:
            raise OperationError(f"view operation is not allowlisted: {action}")
        methods = {"fit_all": "fitAll", "view_axo": "viewAxonometric", "view_front": "viewFront", "view_rear": "viewRear", "view_left": "viewLeft", "view_right": "viewRight", "view_top": "viewTop", "view_bottom": "viewBottom"}
        getattr(view, methods[action])()
        return [action]

    def _create_primitive(self, document: Any, operation: dict[str, Any]) -> str:
        if self.part is None:
            raise OperationError("Part module is unavailable")
        primitive = operation.get("primitive")
        parameters = operation.get("parameters", {})
        if primitive == "box":
            shape = self.part.makeBox(_number(parameters, "length"), _number(parameters, "width"), _number(parameters, "height"))
        elif primitive == "cylinder":
            shape = self.part.makeCylinder(_number(parameters, "radius"), _number(parameters, "height"))
        elif primitive == "sphere":
            shape = self.part.makeSphere(_number(parameters, "radius"))
        else:
            raise OperationError(f"primitive is not allowlisted: {primitive}")
        obj = document.addObject("Part::Feature", operation["object"])
        obj.Shape = shape
        return operation["object"]

    def _boolean(self, document: Any, operation: dict[str, Any]) -> str:
        left, _ = self._resolve(document, operation["left"])
        right, _ = self._resolve(document, operation["right"])
        operation_name = operation.get("operation")
        type_id = {"cut": "Part::Cut", "fuse": "Part::Fuse", "common": "Part::MultiCommon"}.get(operation_name)
        if type_id is None:
            raise OperationError(f"boolean operation is not allowlisted: {operation_name}")
        result = document.addObject(type_id, operation["result"])
        if type_id == "Part::MultiCommon":
            result.Shapes = [left, right]
        else:
            result.Base = left
            result.Tool = right
        return operation["result"]

    def _resolve(self, document: Any, reference: str) -> tuple[Any, list[str]]:
        if not isinstance(reference, str) or not reference.startswith("fc://"):
            raise OperationError("target must be an opaque fc:// reference")
        path = reference.split("@", 1)[0].split("/")
        try:
            object_index = path.index("object")
            object_name = unquote(path[object_index + 1])
        except (ValueError, IndexError) as error:
            raise OperationError("reference does not identify an object") from error
        getter = getattr(document, "getObject", None)
        obj = getter(object_name) if getter else None
        if obj is None:
            raise OperationError(f"object is not open: {object_name}")
        return obj, [unquote(part) for part in path[object_index + 2 :]]

    @staticmethod
    def _property_name(property_name: Any) -> str:
        if not isinstance(property_name, str) or not property_name or "." in property_name or "(" in property_name or "__" in property_name:
            raise OperationError("property names must be declared, simple identifiers")
        return property_name


def _typed_value(value: Any) -> Any:
    if isinstance(value, float) and not math.isfinite(value):
        raise OperationError("numeric values must be finite")
    if isinstance(value, dict) and "value" in value and "unit" in value:
        try:
            import FreeCAD  # type: ignore

            return FreeCAD.Units.Quantity(f"{value['value']} {value['unit']}")
        except Exception:
            return value
    if isinstance(value, (dict, list)):
        raise OperationError("complex property values require an adapter-specific operation")
    return value


def _number(parameters: dict[str, Any], key: str) -> float:
    value = parameters.get(key)
    if isinstance(value, dict):
        value = value.get("value")
    if not isinstance(value, (int, float)) or isinstance(value, bool) or not math.isfinite(float(value)) or value < 0:
        raise OperationError(f"{key} must be a non-negative number")
    return float(value)


def _subelement_index(parts: list[str], kind: str) -> int:
    try:
        index = parts.index(kind)
        return int(parts[index + 1])
    except (ValueError, IndexError, TypeError) as error:
        raise OperationError(f"reference has no numeric {kind} index") from error


def _subelement_name(parts: list[str]) -> str | None:
    if len(parts) >= 2 and parts[0] in {"face", "edge", "vertex", "constraint"}:
        return parts[0].title() + parts[1]
    return None
