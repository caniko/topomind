"""Main-thread FreeCAD/OCCT extraction into bridge DTO JSON."""

from __future__ import annotations

import hashlib
import time
from dataclasses import dataclass
from typing import Any
from urllib.parse import quote

from ..bridge.protocol import sha256_json


def _ref(session: str, document: str, path: str, revision: str) -> str:
    path = "/".join(quote(part, safe="._-~") for part in path.split("/"))
    return f"fc://session/{quote(session, safe='._-~')}/document/{quote(document, safe='._-~')}/{path}@{revision}"


@dataclass
class RevisionBook:
    epoch: str
    geometry: int = 0
    metadata: int = 0
    focus: int = 0
    view: int = 0

    def bump(self, event_class: str) -> None:
        if event_class in {"geometry", "parametric", "transaction"}:
            self.geometry += 1
        elif event_class == "metadata":
            self.metadata += 1
        elif event_class == "focus":
            self.focus += 1
        elif event_class == "view":
            self.view += 1

    def id(self) -> str:
        return f"g{self.geometry}.m{self.metadata}.f{self.focus}.v{self.view}.{self.epoch}"

    def as_dict(self) -> dict[str, Any]:
        return {"geometry": self.geometry, "metadata": self.metadata, "focus": self.focus, "view": self.view, "epoch": self.epoch}


class ExtractionError(RuntimeError):
    """An extraction stage failed without authorizing a guess."""


class FreeCADExtractor:
    def __init__(self, app: Any, gui: Any = None, part: Any = None) -> None:
        self.app = app
        self.gui = gui
        self.part = part

    def snapshot(self, session: str, book: RevisionBook, document_name: str | None = None) -> dict[str, Any]:
        document = self._document(document_name)
        revision = book.id()
        if document is None:
            return {
                "schema_version": "bridge-dto/1.0",
                "session": session,
                "session_epoch": book.epoch,
                "document": "",
                "document_label": None,
                "revision": book.as_dict(),
                "active_workbench": self._active_workbench(),
                "gui_mode": self.gui is not None,
                "active_object": None,
                "active_edit": None,
                "selection": [],
                "view": None,
                "entities": [],
                "diagnostics": [{"code": "document_not_found", "severity": "warning", "message": "No open FreeCAD document", "entity": None, "retryable": True, "evidence": []}],
                "metadata": {},
            }
        entities = self._document_entities(session, document, revision)
        selection = self._selection(session, document.Name, revision)
        return {
            "schema_version": "bridge-dto/1.0",
            "session": session,
            "session_epoch": book.epoch,
            "document": str(document.Name),
            "document_label": str(getattr(document, "Label", document.Name)),
            "revision": book.as_dict(),
            "active_workbench": self._active_workbench(),
            "gui_mode": self.gui is not None,
            "active_object": selection[0]["ref"] if selection else None,
            "active_edit": self._active_edit(session, document.Name, revision),
            "selection": selection,
            "view": self._view(session, document.Name, revision),
            "entities": entities,
            "diagnostics": self._document_diagnostics(document, entities),
            "metadata": {"source": "FreeCAD main thread", "extractor": "freecad.core/1.0"},
        }

    def _document(self, name: str | None) -> Any:
        if name:
            documents = getattr(self.app, "Documents", {})
            if isinstance(documents, dict):
                return documents.get(name)
            get_document = getattr(self.app, "getDocument", None)
            return get_document(name) if get_document else None
        return getattr(self.app, "ActiveDocument", None)

    def _active_workbench(self) -> str | None:
        getter = getattr(self.gui, "activeWorkbench", None) if self.gui is not None else None
        try:
            return str(getter()) if getter else None
        except Exception:
            return None

    def _document_entities(self, session: str, document: Any, revision: str) -> list[dict[str, Any]]:
        document_ref = _ref(session, str(document.Name), "document", revision)
        entities: list[dict[str, Any]] = [{
            "ref": document_ref,
            "kind": "cad.document",
            "name": str(document.Name),
            "label": str(getattr(document, "Label", document.Name)),
            "frame": "world",
            "units": {},
            "source": {"object": None, "subelement": None, "extractor": "freecad.core/1.0", "kernel_tolerance": None, "fingerprint": None},
            "identity": {"state": "exact", "confidence": 1.0, "signals": ["document_name"]},
            "properties": {"recompute": "ok", "object_count": len(getattr(document, "Objects", []) or [])},
            "links": [],
            "bounds": None,
            "shape": None,
        }]
        objects = list(getattr(document, "Objects", []) or [])
        object_refs = {getattr(obj, "Name", str(index)): _ref(session, str(document.Name), f"object/{getattr(obj, 'Name', index)}", revision) for index, obj in enumerate(objects)}
        for obj in objects:
            name = str(getattr(obj, "Name", "Object"))
            reference = object_refs[name]
            links = [{"relation": "contained_by", "target": document_ref}]
            for dependency in list(getattr(obj, "OutList", []) or []):
                dependency_ref = object_refs.get(getattr(dependency, "Name", ""))
                if dependency_ref:
                    links.append({"relation": "depends_on", "target": dependency_ref})
            properties = self._properties(obj)
            shape = getattr(obj, "Shape", None)
            summary = self._shape_summary(shape)
            if summary:
                properties.update(summary.get("properties", {}))
            entities.append({
                "ref": reference,
                "kind": str(getattr(obj, "TypeId", "cad.object")),
                "name": name,
                "label": str(getattr(obj, "Label", name)),
                "frame": "world",
                "units": {},
                "source": {"object": name, "subelement": None, "extractor": "freecad.core/1.0", "kernel_tolerance": None, "fingerprint": sha256_json({"name": name, "type": str(getattr(obj, "TypeId", "")), "shape": summary})},
                "identity": {"state": "exact", "confidence": 1.0, "signals": ["freecad_internal_name"]},
                "properties": properties,
                "links": links,
                "bounds": summary.get("bounds") if summary else None,
                "shape": summary.get("shape") if summary else None,
            })
            entities.extend(self._shape_entities(session, str(document.Name), obj, shape, revision))
            entities.extend(self._sketch_entities(session, str(document.Name), obj, revision))
        self._add_adjacency(entities)
        return entities

    def _properties(self, obj: Any) -> dict[str, Any]:
        properties: dict[str, Any] = {"type_id": str(getattr(obj, "TypeId", "")), "visible": bool(getattr(getattr(obj, "ViewObject", None), "Visibility", True))}
        for name in list(getattr(obj, "PropertiesList", []) or []):
            if name.startswith("_") or name in {"Shape", "Proxy"}:
                continue
            try:
                properties[name] = safe_value(getattr(obj, name))
            except Exception as error:
                properties[name] = {"unsupported": type(error).__name__}
        return properties

    def _shape_summary(self, shape: Any) -> dict[str, Any] | None:
        if shape is None or bool(getattr(shape, "isNull", lambda: True)()):
            return None
        bounds = _bounds(getattr(shape, "BoundBox", None))
        topology = {key.lower(): len(getattr(shape, key, []) or []) for key in ("Vertexes", "Edges", "Wires", "Faces", "Shells", "Solids", "CompSolids")}
        properties: dict[str, Any] = {"valid": bool(getattr(shape, "isValid", lambda: True)()), "shape_type": str(getattr(shape, "ShapeType", "")), "topology": topology}
        for name in ("Volume", "Area", "Length"):
            value = getattr(shape, name, None)
            if isinstance(value, (int, float)):
                properties[name.lower()] = float(value)
        center = getattr(shape, "CenterOfMass", None)
        if center is not None:
            properties["center_of_mass"] = _vector(center)
        return {"bounds": bounds, "properties": properties, "shape": {"shape_type": str(getattr(shape, "ShapeType", "")), "topology": topology, "geometry_type": None, "exact": True, "properties": properties}}

    def _shape_entities(self, session: str, document: str, obj: Any, shape: Any, revision: str) -> list[dict[str, Any]]:
        if shape is None or bool(getattr(shape, "isNull", lambda: True)()):
            return []
        name = str(getattr(obj, "Name", "Object"))
        entities: list[dict[str, Any]] = []
        edges = list(getattr(shape, "Edges", []) or [])
        for index, edge in enumerate(edges, 1):
            ref = _ref(session, document, f"object/{name}/edge/{index}", revision)
            entities.append({"ref": ref, "kind": "cad.edge", "name": f"Edge{index}", "label": None, "frame": "world", "units": {}, "source": {"object": name, "subelement": f"Edge{index}", "extractor": "freecad.part/1.0", "kernel_tolerance": None, "fingerprint": sha256_json({"object": name, "edge": index, "length": getattr(edge, "Length", None)})}, "identity": {"state": "exact", "confidence": 1.0, "signals": ["object_and_subelement"]}, "properties": {"length": float(getattr(edge, "Length", 0.0) or 0.0), "curve_type": type(getattr(edge, "Curve", None)).__name__}, "links": [], "bounds": _bounds(getattr(edge, "BoundBox", None)), "shape": None})
        edge_refs = {index: _ref(session, document, f"object/{name}/edge/{index}", revision) for index in range(1, len(edges) + 1)}
        for index, face in enumerate(list(getattr(shape, "Faces", []) or []), 1):
            ref = _ref(session, document, f"object/{name}/face/{index}", revision)
            surface = getattr(face, "Surface", None)
            surface_type = _surface_type(surface)
            geometry: dict[str, Any] = {"surface_type": surface_type}
            if surface_type == "cylinder":
                if hasattr(surface, "Radius"):
                    geometry["radius"] = float(surface.Radius)
                if hasattr(surface, "Axis"):
                    geometry["axis"] = _vector(surface.Axis)
                if hasattr(surface, "Center"):
                    geometry["origin"] = _vector(surface.Center)
            orientation = str(getattr(face, "Orientation", "Forward"))
            links = [{"relation": "generated_by", "target": _ref(session, document, f"object/{name}", revision)}]
            for edge_index, candidate in enumerate(edges, 1):
                try:
                    if any(candidate.isSame(face_edge) for face_edge in list(getattr(face, "Edges", []) or [])):
                        links.append({"relation": "bounded_by", "target": edge_refs[edge_index]})
                except Exception:
                    pass
            bounds = _bounds(getattr(face, "BoundBox", None))
            depth = max(_extent(bounds)) if bounds else None
            edge_count = len(getattr(face, "Edges", []) or [])
            properties = {"geometry": geometry, "semantic": {"orientation": "interior" if orientation.lower() == "reversed" else "exterior", "openings": float(min(2, max(1, edge_count)))}, "orientation": orientation, "area": float(getattr(face, "Area", 0.0) or 0.0)}
            if depth is not None:
                properties["geometry"]["depth"] = depth
            entities.append({"ref": ref, "kind": "cad.face", "name": f"Face{index}", "label": None, "frame": "world", "units": {"length": "mm", "area": "mm²"}, "source": {"object": name, "subelement": f"Face{index}", "extractor": "freecad.part/1.0", "kernel_tolerance": None, "fingerprint": sha256_json({"object": name, "face": index, "surface": surface_type, "bounds": bounds})}, "identity": {"state": "exact", "confidence": 1.0, "signals": ["object_and_subelement", "shape_fingerprint"]}, "properties": properties, "links": links, "bounds": bounds, "shape": {"shape_type": "face", "topology": {"edges": len(getattr(face, "Edges", []) or [])}, "geometry_type": surface_type, "exact": True, "properties": geometry}})
        return entities

    def _sketch_entities(self, session: str, document: str, obj: Any, revision: str) -> list[dict[str, Any]]:
        if "Sketcher::SketchObject" not in str(getattr(obj, "TypeId", "")):
            return []
        entities: list[dict[str, Any]] = []
        constraints = list(getattr(obj, "Constraints", []) or [])
        for index, constraint in enumerate(constraints):
            ref = _ref(session, document, f"object/{obj.Name}/constraint/{index}", revision)
            datum = None
            getter = getattr(obj, "Datum", None)
            if callable(getter):
                try:
                    datum = safe_value(getter(index))
                except Exception:
                    datum = {"unsupported": "datum_unavailable"}
            entities.append({"ref": ref, "kind": "sketch.constraint", "name": f"Constraint{index}", "label": None, "frame": "sketch_local", "units": {}, "source": {"object": str(obj.Name), "subelement": f"Constraint{index}", "extractor": "freecad.sketcher/1.0", "kernel_tolerance": None, "fingerprint": sha256_json({"object": obj.Name, "constraint": index, "value": safe_value(constraint)})}, "identity": {"state": "exact", "confidence": 1.0, "signals": ["constraint_index_and_sketch"]}, "properties": {"constraint_index": index, "constraint": safe_value(constraint), "datum": datum}, "links": [{"relation": "constrains", "target": _ref(session, document, f"object/{obj.Name}", revision)}], "bounds": None, "shape": None})
        solver = safe_value(getattr(obj, "SolverMessages", None))
        sketch_ref = _ref(session, document, f"object/{obj.Name}", revision)
        entities.append({"ref": sketch_ref + "/sketch", "kind": "sketch.sketch", "name": str(obj.Name), "label": str(getattr(obj, "Label", obj.Name)), "frame": "sketch_local", "units": {}, "source": {"object": str(obj.Name), "subelement": None, "extractor": "freecad.sketcher/1.0", "kernel_tolerance": None, "fingerprint": None}, "identity": {"state": "exact", "confidence": 1.0, "signals": ["sketch_internal_name"]}, "properties": {"solver": solver, "constraint_count": len(constraints), "geometry_count": len(getattr(obj, "Geometry", []) or [])}, "links": [{"relation": "contained_by", "target": _ref(session, document, "document", revision)}], "bounds": None, "shape": None})
        return entities

    def _add_adjacency(self, entities: list[dict[str, Any]]) -> None:
        edge_faces: dict[str, list[str]] = {}
        for entity in entities:
            if entity["kind"] != "cad.face":
                continue
            for link in entity["links"]:
                if link["relation"] == "bounded_by":
                    edge_faces.setdefault(link["target"], []).append(entity["ref"])
        by_ref = {entity["ref"]: entity for entity in entities}
        for faces in edge_faces.values():
            for source in faces:
                for target in faces:
                    if source != target:
                        by_ref[source]["links"].append({"relation": "adjacent_to", "target": target})

    def _document_diagnostics(self, document: Any, entities: list[dict[str, Any]]) -> list[dict[str, Any]]:
        errors = []
        for obj in list(getattr(document, "Objects", []) or []):
            error = getattr(obj, "State", None)
            if error and "Error" in [str(value) for value in error]:
                errors.append({"code": "recompute_error", "severity": "error", "message": f"{getattr(obj, 'Name', 'object')} reports an error", "entity": next((entity["ref"] for entity in entities if entity.get("name") == getattr(obj, "Name", None)), None), "retryable": False, "evidence": []})
        return errors

    def _selection(self, session: str, document: str, revision: str) -> list[dict[str, Any]]:
        if self.gui is None:
            return []
        selection_api = getattr(self.gui, "Selection", None)
        selected = list(getattr(selection_api, "getSelectionEx", lambda: [])() or [])
        items = []
        for index, item in enumerate(selected):
            obj = getattr(item, "Object", None)
            if obj is None:
                continue
            subelements = list(getattr(item, "SubElementNames", []) or []) or [None]
            for subelement in subelements:
                prefix = next((candidate for candidate in ("Face", "Edge", "Vertex") if subelement and subelement.startswith(candidate)), None)
                path = f"object/{obj.Name}" + (f"/{prefix.lower()}/{subelement[len(prefix):]}" if prefix else "")
                items.append({"ref": _ref(session, document, path, revision), "selection_index": index, "subelement": subelement, "source": "Gui.Selection"})
        return items

    def _active_edit(self, session: str, document: str, revision: str) -> str | None:
        if self.gui is None:
            return None
        active = getattr(self.gui, "ActiveDocument", None)
        edit = getattr(active, "getInEdit", lambda: None)() if active else None
        return _ref(session, document, f"object/{edit.Name}", revision) if edit else None

    def _view(self, session: str, document: str, revision: str) -> dict[str, Any] | None:
        if self.gui is None:
            return None
        active = getattr(self.gui, "ActiveDocument", None)
        view = getattr(active, "ActiveView", None) if active else None
        if view is None:
            return None
        camera_node = _call(view, "getCameraNode")
        position_node = getattr(camera_node, "position", None) if camera_node else None
        orientation_node = getattr(camera_node, "orientation", None) if camera_node else None
        camera_position = _vector(_call(position_node, "getValue")) if position_node else None
        camera_orientation = safe_value(_call(orientation_node, "getValue")) if orientation_node else None
        projection = str(_call(view, "getCameraType") or "perspective").lower()
        document_object = self._document(document)
        objects = document_object.Objects if document_object is not None else []
        visible = [_ref(session, document, f"object/{obj.Name}", revision) for obj in objects if bool(getattr(getattr(obj, "ViewObject", None), "Visibility", False))]
        hidden = [_ref(session, document, f"object/{obj.Name}", revision) for obj in objects if not bool(getattr(getattr(obj, "ViewObject", None), "Visibility", False))]
        return {"view_id": f"{session}:{document}:view", "projection": projection, "camera_position": camera_position, "camera_orientation": camera_orientation, "look_direction": None, "up_direction": None, "target": None, "field_of_view_deg": _number_or_none(_call(view, "getCameraFOV")), "orthographic_scale": _number_or_none(_call(view, "getCameraHeight")), "clipping": None, "viewport": None, "visible_objects": visible, "hidden_objects": hidden, "section_planes": [], "timestamp_ms": int(time.time() * 1000), "revision": revision}


def safe_value(value: Any) -> Any:
    if value is None or isinstance(value, (bool, int, float, str)):
        return value
    if isinstance(value, (list, tuple)):
        return [safe_value(item) for item in value]
    if isinstance(value, dict):
        return {str(key): safe_value(item) for key, item in value.items()}
    vector = _vector(value)
    if vector is not None:
        return vector
    for attribute in ("Value", "UserString"):
        if hasattr(value, attribute):
            try:
                return {"value": safe_value(getattr(value, attribute)), "type": type(value).__name__}
            except Exception:
                pass
    return {"type": type(value).__name__, "repr": repr(value)[:256]}


def _vector(value: Any) -> dict[str, float] | list[float] | None:
    if all(hasattr(value, axis) for axis in ("x", "y", "z")):
        return {"x": float(value.x), "y": float(value.y), "z": float(value.z)}
    if all(hasattr(value, axis) for axis in ("X", "Y", "Z")):
        return {"x": float(value.X), "y": float(value.Y), "z": float(value.Z)}
    return None


def _bounds(box: Any) -> dict[str, Any] | None:
    if box is None or not all(hasattr(box, name) for name in ("XMin", "YMin", "ZMin", "XMax", "YMax", "ZMax")):
        return None
    return {"min": {"x": float(box.XMin), "y": float(box.YMin), "z": float(box.ZMin)}, "max": {"x": float(box.XMax), "y": float(box.YMax), "z": float(box.ZMax)}, "frame": "world"}


def _extent(bounds: dict[str, Any] | None) -> tuple[float, float, float]:
    if not bounds:
        return (0.0, 0.0, 0.0)
    return (abs(bounds["max"]["x"] - bounds["min"]["x"]), abs(bounds["max"]["y"] - bounds["min"]["y"]), abs(bounds["max"]["z"] - bounds["min"]["z"]))


def _surface_type(surface: Any) -> str | None:
    name = type(surface).__name__.lower() if surface is not None else ""
    for token in ("cylinder", "plane", "sphere", "cone", "torus", "bspline", "bezier"):
        if token in name:
            return token
    return name or None


def _call(value: Any, method: str) -> Any:
    function = getattr(value, method, None) if value is not None else None
    try:
        return function() if callable(function) else None
    except Exception:
        return None


def _number_or_none(value: Any) -> float | None:
    return float(value) if isinstance(value, (int, float)) else None
