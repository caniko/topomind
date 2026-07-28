use crate::pyutil::{
    attr, bool_attr, call0, call1, list_attr, number_attr, percent_encode, safe_value, string_attr,
    type_name, vector,
};
use ccir_core::{Revision, sha256_json};
use pyo3::prelude::*;
use pyo3::types::PyDict;
use serde_json::{Map, Value, json};
use std::collections::HashMap;
use std::time::{SystemTime, UNIX_EPOCH};

pub struct Extractor;

impl Extractor {
    pub fn snapshot<'py>(
        &self,
        app: &Bound<'py, PyAny>,
        gui: Option<&Bound<'py, PyAny>>,
        session: &str,
        revision: &Revision,
        document_name: Option<&str>,
    ) -> Value {
        let revision_id = revision.id();
        let document = self.document(app, document_name);
        let Some(document) = document else {
            return json!({
                "schema_version": "bridge-dto/1.0",
                "session": session,
                "session_epoch": revision.epoch,
                "document": "",
                "document_label": null,
                "revision": revision,
                "active_workbench": self.active_workbench(gui),
                "gui_mode": gui.is_some(),
                "active_object": null,
                "active_edit": null,
                "selection": [],
                "view": null,
                "entities": [],
                "diagnostics": [{
                    "code": "document_not_found",
                    "severity": "warning",
                    "message": "No open FreeCAD document",
                    "entity": null,
                    "retryable": true,
                    "evidence": []
                }],
                "metadata": {}
            });
        };

        let name = string_attr(&document, "Name").unwrap_or_default();
        let label = string_attr(&document, "Label").unwrap_or_else(|| name.clone());
        let entities = self.document_entities(session, &document, &name, &revision_id);
        let selection = self.selection(gui, session, &name, &revision_id);
        json!({
            "schema_version": "bridge-dto/1.0",
            "session": session,
            "session_epoch": revision.epoch,
            "document": name,
            "document_label": label,
            "revision": revision,
            "active_workbench": self.active_workbench(gui),
            "gui_mode": gui.is_some(),
            "active_object": selection.first().and_then(|item| item.get("ref")).cloned(),
            "active_edit": self.active_edit(gui, session, &name, &revision_id),
            "selection": selection,
            "view": self.view(app, gui, session, &name, &revision_id),
            "diagnostics": self.document_diagnostics(&document, &entities),
            "entities": entities,
            "metadata": {"source": "FreeCAD main thread", "extractor": "freecad.rust/1.0"}
        })
    }

    pub fn document<'py>(
        &self,
        app: &Bound<'py, PyAny>,
        name: Option<&str>,
    ) -> Option<Bound<'py, PyAny>> {
        if let Some(name) = name {
            if let Some(documents) = attr(app, "Documents") {
                if let Ok(documents) = documents.cast::<PyDict>() {
                    if let Ok(Some(document)) = documents.get_item(name) {
                        if !document.is_none() {
                            return Some(document);
                        }
                    }
                }
            }
            return call1(app, "getDocument", name).filter(|document| !document.is_none());
        }
        attr(app, "ActiveDocument").filter(|document| !document.is_none())
    }

    pub fn documents<'py>(&self, app: &Bound<'py, PyAny>) -> Vec<String> {
        if let Some(documents) = attr(app, "Documents") {
            if let Ok(documents) = documents.cast::<PyDict>() {
                return documents
                    .keys()
                    .into_iter()
                    .filter_map(|key| key.extract::<String>().ok())
                    .collect();
            }
        }
        call0(app, "listDocuments")
            .map(|documents| {
                if let Ok(documents) = documents.cast::<PyDict>() {
                    return documents
                        .keys()
                        .into_iter()
                        .filter_map(|key| key.extract::<String>().ok())
                        .collect();
                }
                list_from_value(&documents)
                    .into_iter()
                    .filter_map(|document| {
                        document
                            .extract::<String>()
                            .ok()
                            .or_else(|| string_attr(&document, "Name"))
                    })
                    .collect()
            })
            .unwrap_or_default()
    }

    fn active_workbench(&self, gui: Option<&Bound<'_, PyAny>>) -> Option<String> {
        gui.and_then(|gui| call0(gui, "activeWorkbench"))
            .and_then(|value| value.extract::<String>().ok())
    }

    fn document_entities(
        &self,
        session: &str,
        document: &Bound<'_, PyAny>,
        document_name: &str,
        revision: &str,
    ) -> Vec<Value> {
        let document_ref = entity_ref(session, document_name, "document", revision);
        let objects = list_attr(document, "Objects");
        let mut object_refs = HashMap::new();
        for (index, object) in objects.iter().enumerate() {
            let name = string_attr(object, "Name").unwrap_or_else(|| format!("Object{index}"));
            object_refs.insert(
                name.clone(),
                entity_ref(session, document_name, &format!("object/{name}"), revision),
            );
        }

        let mut entities = vec![json!({
            "ref": document_ref,
            "kind": "cad.document",
            "name": document_name,
            "label": string_attr(document, "Label").unwrap_or_else(|| document_name.into()),
            "frame": "world",
            "units": {},
            "source": {"object": null, "subelement": null, "extractor": "freecad.core/1.0", "kernel_tolerance": null, "fingerprint": null},
            "identity": {"state": "exact", "confidence": 1.0, "signals": ["document_name"]},
            "properties": {"recompute": "ok", "object_count": objects.len()},
            "links": [],
            "bounds": null,
            "shape": null
        })];

        for object in &objects {
            let name = string_attr(object, "Name").unwrap_or_else(|| "Object".into());
            let reference = object_refs.get(&name).cloned().unwrap_or_else(|| {
                entity_ref(session, document_name, &format!("object/{name}"), revision)
            });
            let mut links = vec![json!({"relation": "contained_by", "target": document_ref})];
            for dependency in list_attr(object, "OutList") {
                if let Some(dependency_name) = string_attr(&dependency, "Name") {
                    if let Some(dependency_ref) = object_refs.get(&dependency_name) {
                        links.push(json!({"relation": "depends_on", "target": dependency_ref}));
                    }
                }
            }
            let mut properties = self.properties(object);
            let shape = attr(object, "Shape");
            let summary = shape.as_ref().and_then(|shape| self.shape_summary(shape));
            if let Some(summary) = &summary {
                if let Some(summary_properties) =
                    summary.get("properties").and_then(Value::as_object)
                {
                    if let Some(properties_map) = properties.as_object_mut() {
                        properties_map.extend(summary_properties.clone());
                    }
                }
            }
            let fingerprint = sha256_json(
                &json!({"name": name, "type": string_attr(object, "TypeId"), "shape": summary}),
            )
            .ok();
            entities.push(json!({
                "ref": reference,
                "kind": string_attr(object, "TypeId").unwrap_or_else(|| "cad.object".into()),
                "name": name,
                "label": string_attr(object, "Label"),
                "frame": "world",
                "units": {},
                "source": {"object": string_attr(object, "Name"), "subelement": null, "extractor": "freecad.core/1.0", "kernel_tolerance": null, "fingerprint": fingerprint},
                "identity": {"state": "exact", "confidence": 1.0, "signals": ["freecad_internal_name"]},
                "properties": properties,
                "links": links,
                "bounds": summary.as_ref().and_then(|value| value.get("bounds")).cloned().unwrap_or(Value::Null),
                "shape": summary.as_ref().and_then(|value| value.get("shape")).cloned().unwrap_or(Value::Null)
            }));
            if let Some(shape) = shape {
                entities.extend(self.shape_entities(
                    session,
                    document_name,
                    &name,
                    object,
                    &shape,
                    revision,
                ));
            }
            entities.extend(self.sketch_entities(session, document_name, object, revision));
        }
        add_adjacency(&mut entities);
        entities
    }

    fn properties(&self, object: &Bound<'_, PyAny>) -> Value {
        let mut properties = Map::new();
        properties.insert(
            "type_id".into(),
            json!(string_attr(object, "TypeId").unwrap_or_default()),
        );
        let visible = attr(object, "ViewObject")
            .and_then(|view| bool_attr(&view, "Visibility"))
            .unwrap_or(true);
        properties.insert("visible".into(), json!(visible));
        for property_name in list_attr(object, "PropertiesList") {
            let Some(property_name) = property_name.extract::<String>().ok() else {
                continue;
            };
            if property_name.starts_with('_') || matches!(property_name.as_str(), "Shape" | "Proxy")
            {
                continue;
            }
            let value = attr(object, &property_name)
                .map(|value| safe_value(&value))
                .unwrap_or_else(|| json!({"unsupported": "unavailable"}));
            properties.insert(property_name, value);
        }
        Value::Object(properties)
    }

    fn shape_summary(&self, shape: &Bound<'_, PyAny>) -> Option<Value> {
        if call0(shape, "isNull")
            .and_then(|value| value.extract::<bool>().ok())
            .unwrap_or(false)
        {
            return None;
        }
        let bounds = attr(shape, "BoundBox").and_then(bounds);
        let mut topology = Map::new();
        for (attribute_name, output_name) in [
            ("Vertexes", "vertexes"),
            ("Edges", "edges"),
            ("Wires", "wires"),
            ("Faces", "faces"),
            ("Shells", "shells"),
            ("Solids", "solids"),
            ("CompSolids", "compsolids"),
        ] {
            topology.insert(
                output_name.into(),
                json!(list_attr(shape, attribute_name).len()),
            );
        }
        let mut properties = Map::new();
        properties.insert(
            "valid".into(),
            json!(
                call0(shape, "isValid")
                    .and_then(|value| value.extract::<bool>().ok())
                    .unwrap_or(true)
            ),
        );
        properties.insert(
            "shape_type".into(),
            json!(string_attr(shape, "ShapeType").unwrap_or_default()),
        );
        properties.insert("topology".into(), Value::Object(topology.clone()));
        for name in ["Volume", "Area", "Length"] {
            if let Some(value) = number_attr(shape, name) {
                properties.insert(name.to_lowercase(), json!(value));
            }
        }
        if let Some(center) = attr(shape, "CenterOfMass").and_then(|value| vector(&value)) {
            properties.insert("center_of_mass".into(), center);
        }
        Some(json!({
            "bounds": bounds,
            "properties": properties,
            "shape": {"shape_type": string_attr(shape, "ShapeType").unwrap_or_default(), "topology": topology, "geometry_type": null, "exact": true, "properties": properties}
        }))
    }

    fn shape_entities(
        &self,
        session: &str,
        document: &str,
        object_name: &str,
        object: &Bound<'_, PyAny>,
        shape: &Bound<'_, PyAny>,
        revision: &str,
    ) -> Vec<Value> {
        let edges = list_attr(shape, "Edges");
        let mut entities = Vec::new();
        let edge_refs: HashMap<usize, String> = (1..=edges.len())
            .map(|index| {
                (
                    index,
                    entity_ref(
                        session,
                        document,
                        &format!("object/{object_name}/edge/{index}"),
                        revision,
                    ),
                )
            })
            .collect();
        for (index, edge) in edges.iter().enumerate() {
            let index = index + 1;
            let fingerprint = sha256_json(&json!({"object": object_name, "edge": index, "length": number_attr(edge, "Length")})).ok();
            entities.push(json!({
                "ref": edge_refs.get(&index),
                "kind": "cad.edge",
                "name": format!("Edge{index}"),
                "label": null,
                "frame": "world",
                "units": {},
                "source": {"object": object_name, "subelement": format!("Edge{index}"), "extractor": "freecad.part/1.0", "kernel_tolerance": null, "fingerprint": fingerprint},
                "identity": {"state": "exact", "confidence": 1.0, "signals": ["object_and_subelement"]},
                "properties": {"length": number_attr(edge, "Length").unwrap_or(0.0), "curve_type": attr(edge, "Curve").map(|curve| type_name(&curve))},
                "links": [],
                "bounds": attr(edge, "BoundBox").and_then(bounds),
                "shape": null
            }));
        }
        for (index, face) in list_attr(shape, "Faces").iter().enumerate() {
            let index = index + 1;
            let reference = entity_ref(
                session,
                document,
                &format!("object/{object_name}/face/{index}"),
                revision,
            );
            let surface = attr(face, "Surface");
            let surface_type = surface.as_ref().map(type_name);
            let mut geometry = Map::new();
            geometry.insert("surface_type".into(), json!(surface_type));
            if surface_type.as_deref() == Some("Cylinder")
                || surface_type.as_deref() == Some("cylinder")
            {
                if let Some(surface) = &surface {
                    if let Some(radius) = number_attr(surface, "Radius") {
                        geometry.insert("radius".into(), json!(radius));
                    }
                    if let Some(axis) = attr(surface, "Axis").and_then(|value| vector(&value)) {
                        geometry.insert("axis".into(), axis);
                    }
                    if let Some(origin) = attr(surface, "Center").and_then(|value| vector(&value)) {
                        geometry.insert("origin".into(), origin);
                    }
                }
            }
            let orientation = string_attr(face, "Orientation").unwrap_or_else(|| "Forward".into());
            let face_edges = list_attr(face, "Edges");
            let mut links = vec![
                json!({"relation": "generated_by", "target": entity_ref(session, document, &format!("object/{object_name}"), revision)}),
            ];
            for (edge_index, candidate) in edges.iter().enumerate() {
                if face_edges.iter().any(|face_edge| {
                    call1(candidate, "isSame", face_edge.clone())
                        .and_then(|value| value.extract::<bool>().ok())
                        .unwrap_or(false)
                }) {
                    if let Some(edge_ref) = edge_refs.get(&(edge_index + 1)) {
                        links.push(json!({"relation": "bounded_by", "target": edge_ref}));
                    }
                }
            }
            let face_bounds = attr(face, "BoundBox").and_then(bounds);
            let depth = face_bounds.as_ref().map(|value| {
                let dx = (value["max"]["x"].as_f64().unwrap_or(0.0)
                    - value["min"]["x"].as_f64().unwrap_or(0.0))
                .abs();
                let dy = (value["max"]["y"].as_f64().unwrap_or(0.0)
                    - value["min"]["y"].as_f64().unwrap_or(0.0))
                .abs();
                let dz = (value["max"]["z"].as_f64().unwrap_or(0.0)
                    - value["min"]["z"].as_f64().unwrap_or(0.0))
                .abs();
                dx.max(dy).max(dz)
            });
            if let Some(depth) = depth {
                geometry.insert("depth".into(), json!(depth));
            }
            let fingerprint = sha256_json(&json!({"object": object_name, "face": index, "surface": surface_type, "bounds": face_bounds})).ok();
            entities.push(json!({
                "ref": reference,
                "kind": "cad.face",
                "name": format!("Face{index}"),
                "label": null,
                "frame": "world",
                "units": {"length": "mm", "area": "mm²"},
                "source": {"object": object_name, "subelement": format!("Face{index}"), "extractor": "freecad.part/1.0", "kernel_tolerance": null, "fingerprint": fingerprint},
                "identity": {"state": "exact", "confidence": 1.0, "signals": ["object_and_subelement", "shape_fingerprint"]},
                "properties": {"geometry": geometry, "semantic": {"orientation": if orientation.eq_ignore_ascii_case("reversed") {"interior"} else {"exterior"}, "openings": (face_edges.len().clamp(1, 2)) as f64}, "orientation": orientation, "area": number_attr(face, "Area").unwrap_or(0.0)},
                "links": links,
                "bounds": face_bounds,
                "shape": {"shape_type": "face", "topology": {"edges": face_edges.len()}, "geometry_type": surface_type, "exact": true, "properties": geometry}
            }));
        }
        let _ = object;
        entities
    }

    fn sketch_entities(
        &self,
        session: &str,
        document: &str,
        object: &Bound<'_, PyAny>,
        revision: &str,
    ) -> Vec<Value> {
        if !string_attr(object, "TypeId")
            .unwrap_or_default()
            .contains("Sketcher::SketchObject")
        {
            return Vec::new();
        }
        let object_name = string_attr(object, "Name").unwrap_or_else(|| "Sketch".into());
        let constraints = list_attr(object, "Constraints");
        let mut entities = Vec::new();
        for (index, constraint) in constraints.iter().enumerate() {
            let reference = entity_ref(
                session,
                document,
                &format!("object/{object_name}/constraint/{index}"),
                revision,
            );
            let datum = call1(object, "Datum", index as i64).map(|value| safe_value(&value));
            entities.push(json!({
                "ref": reference,
                "kind": "sketch.constraint",
                "name": format!("Constraint{index}"),
                "label": null,
                "frame": "sketch_local",
                "units": {},
                "source": {"object": object_name, "subelement": format!("Constraint{index}"), "extractor": "freecad.sketcher/1.0", "kernel_tolerance": null, "fingerprint": sha256_json(&json!({"object": object_name, "constraint": safe_value(constraint)})).ok()},
                "identity": {"state": "exact", "confidence": 1.0, "signals": ["constraint_index_and_sketch"]},
                "properties": {"constraint_index": index, "constraint": safe_value(constraint), "datum": datum},
                "links": [{"relation": "constrains", "target": entity_ref(session, document, &format!("object/{object_name}"), revision)}],
                "bounds": null,
                "shape": null
            }));
        }
        entities.push(json!({
            "ref": format!("{}/sketch", entity_ref(session, document, &format!("object/{object_name}"), revision)),
            "kind": "sketch.sketch",
            "name": object_name,
            "label": string_attr(object, "Label"),
            "frame": "sketch_local",
            "units": {},
            "source": {"object": string_attr(object, "Name"), "subelement": null, "extractor": "freecad.sketcher/1.0", "kernel_tolerance": null, "fingerprint": null},
            "identity": {"state": "exact", "confidence": 1.0, "signals": ["sketch_internal_name"]},
            "properties": {"solver": attr(object, "SolverMessages").map(|value| safe_value(&value)), "constraint_count": constraints.len(), "geometry_count": list_attr(object, "Geometry").len()},
            "links": [{"relation": "contained_by", "target": entity_ref(session, document, "document", revision)}],
            "bounds": null,
            "shape": null
        }));
        entities
    }

    fn document_diagnostics(&self, document: &Bound<'_, PyAny>, entities: &[Value]) -> Vec<Value> {
        list_attr(document, "Objects")
            .into_iter()
            .filter_map(|object| {
                let states = list_attr(&object, "State");
                let has_error = states.iter().any(|state| state.extract::<String>().map(|value| value == "Error").unwrap_or(false));
                has_error.then(|| {
                    let name = string_attr(&object, "Name").unwrap_or_else(|| "object".into());
                    json!({
                        "code": "recompute_error",
                        "severity": "error",
                        "message": format!("{name} reports an error"),
                        "entity": entities.iter().find(|entity| entity.get("name").and_then(Value::as_str) == Some(name.as_str())).and_then(|entity| entity.get("ref")),
                        "retryable": false,
                        "evidence": []
                    })
                })
            })
            .collect()
    }

    fn selection(
        &self,
        gui: Option<&Bound<'_, PyAny>>,
        session: &str,
        document: &str,
        revision: &str,
    ) -> Vec<Value> {
        let Some(selection) = gui.and_then(|gui| attr(gui, "Selection")) else {
            return Vec::new();
        };
        let selected = call0(&selection, "getSelectionEx")
            .map(|value| list_from_value(&value))
            .unwrap_or_default();
        let mut items = Vec::new();
        for (index, item) in selected.iter().enumerate() {
            let Some(object) = attr(item, "Object") else {
                continue;
            };
            let Some(name) = string_attr(&object, "Name") else {
                continue;
            };
            let mut subelements: Vec<String> = list_attr(item, "SubElementNames")
                .into_iter()
                .filter_map(|value| value.extract().ok())
                .collect();
            if subelements.is_empty() {
                subelements.push(String::new());
            }
            for subelement in subelements {
                let path = if let Some((kind, number)) = subelement_kind(&subelement) {
                    format!("object/{name}/{kind}/{number}")
                } else {
                    format!("object/{name}")
                };
                items.push(json!({"ref": entity_ref(session, document, &path, revision), "selection_index": index, "subelement": if subelement.is_empty() {Value::Null} else {json!(subelement)}, "source": "Gui.Selection"}));
            }
        }
        items
    }

    fn active_edit(
        &self,
        gui: Option<&Bound<'_, PyAny>>,
        session: &str,
        document: &str,
        revision: &str,
    ) -> Option<Value> {
        let active = gui.and_then(|gui| attr(gui, "ActiveDocument"));
        let edit = active
            .as_ref()
            .and_then(|active| call0(active, "getInEdit"));
        edit.filter(|edit| !edit.is_none())
            .and_then(|edit| string_attr(&edit, "Name"))
            .map(|name| {
                json!(entity_ref(
                    session,
                    document,
                    &format!("object/{name}"),
                    revision
                ))
            })
    }

    fn view(
        &self,
        app: &Bound<'_, PyAny>,
        gui: Option<&Bound<'_, PyAny>>,
        session: &str,
        document: &str,
        revision: &str,
    ) -> Option<Value> {
        let active = gui.and_then(|gui| attr(gui, "ActiveDocument"));
        let view = active
            .as_ref()
            .and_then(|active| attr(active, "ActiveView"));
        let view = view?;
        let camera = call0(&view, "getCameraNode");
        let camera_position = camera
            .as_ref()
            .and_then(|camera| attr(camera, "position"))
            .and_then(|position| call0(&position, "getValue"))
            .and_then(|value| vector(&value));
        let camera_orientation = camera
            .as_ref()
            .and_then(|camera| attr(camera, "orientation"))
            .and_then(|orientation| call0(&orientation, "getValue"))
            .map(|value| safe_value(&value));
        let projection = call0(&view, "getCameraType")
            .and_then(|value| value.extract::<String>().ok())
            .unwrap_or_else(|| "perspective".into())
            .to_lowercase();
        let document_object = self.document(app, Some(document));
        let objects = document_object
            .as_ref()
            .map(|document| list_attr(document, "Objects"))
            .unwrap_or_default();
        let mut visible = Vec::new();
        let mut hidden = Vec::new();
        for object in objects {
            let Some(name) = string_attr(&object, "Name") else {
                continue;
            };
            let reference = json!(entity_ref(
                session,
                document,
                &format!("object/{name}"),
                revision
            ));
            if attr(&object, "ViewObject")
                .and_then(|view| bool_attr(&view, "Visibility"))
                .unwrap_or(false)
            {
                visible.push(reference);
            } else {
                hidden.push(reference);
            }
        }
        Some(json!({
            "view_id": format!("{session}:{document}:view"),
            "projection": projection,
            "camera_position": camera_position,
            "camera_orientation": camera_orientation,
            "look_direction": null,
            "up_direction": null,
            "target": null,
            "field_of_view_deg": call0(&view, "getCameraFOV").and_then(|value| value.extract::<f64>().ok()),
            "orthographic_scale": call0(&view, "getCameraHeight").and_then(|value| value.extract::<f64>().ok()),
            "clipping": null,
            "viewport": null,
            "visible_objects": visible,
            "hidden_objects": hidden,
            "section_planes": [],
            "timestamp_ms": SystemTime::now().duration_since(UNIX_EPOCH).map(|value| value.as_millis()).unwrap_or_default(),
            "revision": revision
        }))
    }
}

fn entity_ref(session: &str, document: &str, path: &str, revision: &str) -> String {
    let path = path
        .split('/')
        .map(percent_encode)
        .collect::<Vec<_>>()
        .join("/");
    format!(
        "fc://session/{}/document/{}/{path}@{revision}",
        percent_encode(session),
        percent_encode(document)
    )
}

fn bounds(value: Bound<'_, PyAny>) -> Option<Value> {
    let min = json!({"x": number_attr(&value, "XMin")?, "y": number_attr(&value, "YMin")?, "z": number_attr(&value, "ZMin")?});
    let max = json!({"x": number_attr(&value, "XMax")?, "y": number_attr(&value, "YMax")?, "z": number_attr(&value, "ZMax")?});
    Some(json!({"min": min, "max": max, "frame": "world"}))
}

fn add_adjacency(entities: &mut [Value]) {
    let mut edge_faces: HashMap<String, Vec<String>> = HashMap::new();
    for entity in entities.iter() {
        if entity.get("kind").and_then(Value::as_str) != Some("cad.face") {
            continue;
        }
        let Some(reference) = entity.get("ref").and_then(Value::as_str) else {
            continue;
        };
        if let Some(links) = entity.get("links").and_then(Value::as_array) {
            for link in links
                .iter()
                .filter(|link| link.get("relation").and_then(Value::as_str) == Some("bounded_by"))
            {
                if let Some(target) = link.get("target").and_then(Value::as_str) {
                    edge_faces
                        .entry(target.into())
                        .or_default()
                        .push(reference.into());
                }
            }
        }
    }
    let mut adjacency: HashMap<String, Vec<String>> = HashMap::new();
    for faces in edge_faces.values() {
        for source in faces {
            for target in faces {
                if source != target {
                    adjacency
                        .entry(source.clone())
                        .or_default()
                        .push(target.clone());
                }
            }
        }
    }
    for entity in entities.iter_mut() {
        let Some(reference) = entity.get("ref").and_then(Value::as_str) else {
            continue;
        };
        let Some(targets) = adjacency.get(reference) else {
            continue;
        };
        if let Some(links) = entity.get_mut("links").and_then(Value::as_array_mut) {
            links.extend(
                targets
                    .iter()
                    .map(|target| json!({"relation": "adjacent_to", "target": target})),
            );
        }
    }
}

fn subelement_kind(value: &str) -> Option<(&'static str, &str)> {
    [
        ("Face", "face"),
        ("Edge", "edge"),
        ("Vertex", "vertex"),
        ("Constraint", "constraint"),
    ]
    .iter()
    .find_map(|(prefix, kind)| value.strip_prefix(prefix).map(|number| (*kind, number)))
}

fn list_from_value<'py>(value: &Bound<'py, PyAny>) -> Vec<Bound<'py, PyAny>> {
    value
        .try_iter()
        .map(|iterator| iterator.filter_map(Result::ok).collect())
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn entity_refs_escape_document_names_without_losing_paths() {
        let reference = entity_ref("session", "Demo Document", "object/Body/face/1", "g1");
        assert_eq!(
            reference,
            "fc://session/session/document/Demo%20Document/object/Body/face/1@g1"
        );
    }

    #[test]
    fn selection_names_are_limited_to_known_subelements() {
        assert_eq!(subelement_kind("Face12"), Some(("face", "12")));
        assert_eq!(subelement_kind("Random12"), None);
    }
}
