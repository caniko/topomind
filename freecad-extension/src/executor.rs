use crate::extractor::Extractor;
use crate::pyutil::{
    attr, call0, call1, list_attr, percent_decode, py_value, safe_value, string_attr,
};
use pyo3::prelude::*;
use serde_json::{Value, json};
use std::collections::HashSet;

pub struct OperationResult {
    pub changed: Vec<String>,
    pub checks: Vec<Value>,
}

pub struct Executor;

impl Executor {
    pub fn apply<'py>(
        &self,
        py: Python<'py>,
        app: &Bound<'py, PyAny>,
        gui: Option<&Bound<'py, PyAny>>,
        part: Option<&Bound<'py, PyAny>>,
        changeset: &Value,
        commit: bool,
    ) -> Result<OperationResult, String> {
        let document_name = changeset.get("document").and_then(Value::as_str);
        let document = Extractor
            .document(app, document_name)
            .ok_or_else(|| "document is not open".to_string())?;
        let operations = changeset
            .get("operations")
            .and_then(Value::as_array)
            .ok_or_else(|| "changeset must contain operations".to_string())?;
        if operations.is_empty() {
            return Err("changeset must contain at least one operation".into());
        }
        if let Some(open) = attr(&document, "openTransaction") {
            open.call1((format!(
                "Topomind {}",
                changeset
                    .get("request_id")
                    .and_then(Value::as_str)
                    .unwrap_or("change")
            ),))
                .map_err(py_error)?;
        }
        let mut changed = Vec::new();
        let result = (|| {
            for operation in operations {
                if !commit {
                    match operation.get("op").and_then(Value::as_str) {
                        Some("set_selection") => {
                            self.validate_selection(&document, gui, operation)?;
                            changed.extend(
                                operation
                                    .get("targets")
                                    .and_then(Value::as_array)
                                    .into_iter()
                                    .flatten()
                                    .filter_map(Value::as_str)
                                    .map(str::to_owned),
                            );
                            continue;
                        }
                        Some("set_view") => {
                            self.validate_view(gui, operation)?;
                            changed.push(
                                operation
                                    .get("operation")
                                    .and_then(Value::as_str)
                                    .unwrap_or_default()
                                    .into(),
                            );
                            continue;
                        }
                        Some("set_visibility") => {
                            self.validate_visibility(&document, operation)?;
                            changed.push(
                                operation
                                    .get("target")
                                    .and_then(Value::as_str)
                                    .unwrap_or_default()
                                    .into(),
                            );
                            continue;
                        }
                        _ => {}
                    }
                }
                changed.extend(self.apply_operation(py, &document, gui, part, operation)?);
            }
            let checks = self.validate(
                &document,
                changeset
                    .get("validate")
                    .and_then(Value::as_array)
                    .map(|values| values.iter().filter_map(Value::as_str).collect::<Vec<_>>())
                    .unwrap_or_default(),
            )?;
            if checks
                .iter()
                .any(|check| check.get("status").and_then(Value::as_str) == Some("fail"))
            {
                let message = checks
                    .iter()
                    .filter(|check| check.get("status").and_then(Value::as_str) == Some("fail"))
                    .map(|check| {
                        check
                            .get("message")
                            .and_then(Value::as_str)
                            .unwrap_or_default()
                    })
                    .collect::<Vec<_>>()
                    .join("; ");
                return Err(format!("validation failed: {message}"));
            }
            if commit {
                if let Some(commit_transaction) = attr(&document, "commitTransaction") {
                    commit_transaction.call0().map_err(py_error)?;
                }
            } else if let Some(abort_transaction) = attr(&document, "abortTransaction") {
                abort_transaction.call0().map_err(py_error)?;
            }
            Ok(OperationResult { changed, checks })
        })();
        if result.is_err() {
            if let Some(abort_transaction) = attr(&document, "abortTransaction") {
                let _ = abort_transaction.call0();
            }
        }
        result
    }

    fn validate(
        &self,
        document: &Bound<'_, PyAny>,
        requested: Vec<&str>,
    ) -> Result<Vec<Value>, String> {
        if let Some(recompute) = attr(document, "recompute") {
            recompute.call0().map_err(py_error)?;
        }
        let requested: HashSet<&str> = if requested.is_empty() {
            ["recompute", "shape_validity"].into_iter().collect()
        } else {
            requested.into_iter().collect()
        };
        let mut checks = Vec::new();
        if requested.contains("recompute") {
            let errors: Vec<String> = list_attr(document, "Objects")
                .into_iter()
                .filter_map(|object| {
                    let has_error = list_attr(&object, "State").iter().any(|state| {
                        state
                            .extract::<String>()
                            .map(|value| value == "Error")
                            .unwrap_or(false)
                    });
                    has_error
                        .then(|| string_attr(&object, "Name").unwrap_or_else(|| "object".into()))
                })
                .collect();
            checks.push(json!({"id": "recompute", "status": if errors.is_empty() {"pass"} else {"fail"}, "message": if errors.is_empty() {"recompute completed".into()} else {format!("objects with errors: {errors:?}")}, "evidence": errors}));
        }
        if requested.contains("shape_validity") {
            let invalid: Vec<String> = list_attr(document, "Objects")
                .into_iter()
                .filter_map(|object| {
                    let shape = attr(&object, "Shape")?;
                    let invalid = call0(&shape, "isValid")
                        .and_then(|value| value.extract::<bool>().ok())
                        .map(|value| !value)
                        .unwrap_or(false);
                    invalid.then(|| string_attr(&object, "Name").unwrap_or_else(|| "object".into()))
                })
                .collect();
            checks.push(json!({"id": "shape_validity", "status": if invalid.is_empty() {"pass"} else {"fail"}, "message": if invalid.is_empty() {"all reported shapes are valid".into()} else {format!("invalid shapes: {invalid:?}")}, "evidence": invalid}));
        }
        if requested.contains("sketch_solver") {
            let conflicting: Vec<String> = list_attr(document, "Objects")
                .into_iter()
                .filter_map(|object| {
                    let type_id = string_attr(&object, "TypeId").unwrap_or_default();
                    let messages = attr(&object, "SolverMessages")
                        .map(|value| safe_value(&value).to_string())
                        .unwrap_or_default();
                    (type_id.contains("Sketcher::SketchObject") && messages.contains("Conflict"))
                        .then(|| string_attr(&object, "Name").unwrap_or_else(|| "sketch".into()))
                })
                .collect();
            checks.push(json!({"id": "sketch_solver", "status": if conflicting.is_empty() {"pass"} else {"fail"}, "message": if conflicting.is_empty() {"sketch solver reports no conflicts".into()} else {format!("conflicting sketches: {conflicting:?}")}, "evidence": conflicting}));
        }
        Ok(checks)
    }

    pub fn undo<'py>(
        &self,
        app: &Bound<'py, PyAny>,
        document_name: Option<&str>,
    ) -> Result<(), String> {
        let document = Extractor
            .document(app, document_name)
            .ok_or_else(|| "undo is unavailable".to_string())?;
        attr(&document, "undo")
            .ok_or_else(|| "undo is unavailable".to_string())?
            .call0()
            .map_err(py_error)?;
        Ok(())
    }

    pub fn redo<'py>(
        &self,
        app: &Bound<'py, PyAny>,
        document_name: Option<&str>,
    ) -> Result<(), String> {
        let document = Extractor
            .document(app, document_name)
            .ok_or_else(|| "redo is unavailable".to_string())?;
        attr(&document, "redo")
            .ok_or_else(|| "redo is unavailable".to_string())?
            .call0()
            .map_err(py_error)?;
        Ok(())
    }

    fn apply_operation<'py>(
        &self,
        py: Python<'py>,
        document: &Bound<'py, PyAny>,
        gui: Option<&Bound<'py, PyAny>>,
        part: Option<&Bound<'py, PyAny>>,
        operation: &Value,
    ) -> Result<Vec<String>, String> {
        let name = operation
            .get("op")
            .and_then(Value::as_str)
            .ok_or_else(|| "typed operation requires op".to_string())?;
        match name {
            "set_property" => {
                let (object, _) = self.resolve(
                    document,
                    operation
                        .get("target")
                        .and_then(Value::as_str)
                        .ok_or_else(|| "target is required".to_string())?,
                )?;
                let property = property_name(
                    operation
                        .get("property")
                        .and_then(Value::as_str)
                        .unwrap_or_default(),
                )?;
                declared_property(&object, &property)?;
                let value = typed_value(py, operation.get("value").unwrap_or(&Value::Null))?;
                object
                    .setattr(property.as_str(), value.bind(py))
                    .map_err(py_error)?;
                Ok(vec![
                    operation
                        .get("target")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .into(),
                ])
            }
            "set_expression" => {
                let (object, _) = self.resolve(
                    document,
                    operation
                        .get("target")
                        .and_then(Value::as_str)
                        .ok_or_else(|| "target is required".to_string())?,
                )?;
                let property = property_name(
                    operation
                        .get("property")
                        .and_then(Value::as_str)
                        .unwrap_or_default(),
                )?;
                declared_property(&object, &property)?;
                let expression = operation
                    .get("expression")
                    .and_then(Value::as_str)
                    .ok_or_else(|| "set_expression requires a string expression".to_string())?;
                attr(&object, "setExpression")
                    .ok_or_else(|| "target does not support expressions".to_string())?
                    .call1((property, expression))
                    .map_err(py_error)?;
                Ok(vec![
                    operation
                        .get("target")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .into(),
                ])
            }
            "sketch_set_datum" => {
                let target = operation
                    .get("constraint")
                    .and_then(Value::as_str)
                    .ok_or_else(|| "constraint is required".to_string())?;
                let (object, parts) = self.resolve(document, target)?;
                if !string_attr(&object, "TypeId")
                    .unwrap_or_default()
                    .contains("Sketcher::SketchObject")
                {
                    return Err("sketch.set_datum target is not a Sketcher object".into());
                }
                let index = subelement_index(&parts, "constraint")?;
                let value = typed_value(py, operation.get("value").unwrap_or(&Value::Null))?;
                attr(&object, "setDatum")
                    .ok_or_else(|| "sketch does not expose setDatum".to_string())?
                    .call1((index, value.bind(py)))
                    .map_err(py_error)?;
                Ok(vec![target.into()])
            }
            "set_visibility" => {
                let target = operation
                    .get("target")
                    .and_then(Value::as_str)
                    .ok_or_else(|| "target is required".to_string())?;
                let (object, _) = self.resolve(document, target)?;
                let view_object = attr(&object, "ViewObject")
                    .ok_or_else(|| "target has no view object".to_string())?;
                view_object
                    .setattr(
                        "Visibility",
                        operation
                            .get("visible")
                            .and_then(Value::as_bool)
                            .ok_or_else(|| "visible must be boolean".to_string())?,
                    )
                    .map_err(py_error)?;
                Ok(vec![target.into()])
            }
            "set_selection" => {
                let gui = gui.ok_or_else(|| "selection requires GUI mode".to_string())?;
                let selection = attr(gui, "Selection")
                    .ok_or_else(|| "FreeCAD selection API is unavailable".to_string())?;
                let mode = operation
                    .get("mode")
                    .and_then(Value::as_str)
                    .unwrap_or("replace");
                if !["replace", "add", "remove"].contains(&mode) {
                    return Err(format!("selection mode is not allowlisted: {mode}"));
                }
                if mode == "replace" {
                    attr(&selection, "clearSelection")
                        .ok_or_else(|| "selection API is unavailable".to_string())?
                        .call0()
                        .map_err(py_error)?;
                }
                let targets = operation
                    .get("targets")
                    .and_then(Value::as_array)
                    .ok_or_else(|| "targets must be an array".to_string())?;
                for target in targets.iter().filter_map(Value::as_str) {
                    let (object, parts) = self.resolve(document, target)?;
                    let subelement = subelement_name(&parts);
                    let method = if mode == "remove" {
                        "removeSelection"
                    } else {
                        "addSelection"
                    };
                    if let Some(subelement) = subelement {
                        attr(&selection, method)
                            .ok_or_else(|| "selection API is unavailable".to_string())?
                            .call1((object, subelement))
                            .map_err(py_error)?;
                    } else {
                        attr(&selection, method)
                            .ok_or_else(|| "selection API is unavailable".to_string())?
                            .call1((object,))
                            .map_err(py_error)?;
                    }
                }
                Ok(targets
                    .iter()
                    .filter_map(Value::as_str)
                    .map(str::to_owned)
                    .collect())
            }
            "set_view" => {
                let gui = gui.ok_or_else(|| "view requires GUI mode".to_string())?;
                let active = attr(gui, "ActiveDocument")
                    .ok_or_else(|| "active view is unavailable".to_string())?;
                let view = attr(&active, "ActiveView")
                    .ok_or_else(|| "active view is unavailable".to_string())?;
                let operation = operation
                    .get("operation")
                    .and_then(Value::as_str)
                    .ok_or_else(|| "view operation is required".to_string())?;
                let method = match operation {
                    "fit_all" => "fitAll",
                    "view_axo" => "viewAxonometric",
                    "view_front" => "viewFront",
                    "view_rear" => "viewRear",
                    "view_left" => "viewLeft",
                    "view_right" => "viewRight",
                    "view_top" => "viewTop",
                    "view_bottom" => "viewBottom",
                    _ => return Err(format!("view operation is not allowlisted: {operation}")),
                };
                attr(&view, method)
                    .ok_or_else(|| "view operation is unavailable".to_string())?
                    .call0()
                    .map_err(py_error)?;
                Ok(vec![operation.into()])
            }
            "create_primitive" => {
                let part = part.ok_or_else(|| "Part module is unavailable".to_string())?;
                let object_name = operation
                    .get("object")
                    .and_then(Value::as_str)
                    .ok_or_else(|| "object is required".to_string())?;
                let primitive = operation
                    .get("primitive")
                    .and_then(Value::as_str)
                    .ok_or_else(|| "primitive is required".to_string())?;
                let parameters = operation
                    .get("parameters")
                    .and_then(Value::as_object)
                    .ok_or_else(|| "parameters are required".to_string())?;
                let number = |name: &str| {
                    number(
                        parameters
                            .get(name)
                            .ok_or_else(|| format!("{name} is required"))?,
                    )
                };
                let shape = match primitive {
                    "box" => attr(part, "makeBox")
                        .ok_or_else(|| "Part.makeBox is unavailable".to_string())?
                        .call1((number("length")?, number("width")?, number("height")?)),
                    "cylinder" => attr(part, "makeCylinder")
                        .ok_or_else(|| "Part.makeCylinder is unavailable".to_string())?
                        .call1((number("radius")?, number("height")?)),
                    "sphere" => attr(part, "makeSphere")
                        .ok_or_else(|| "Part.makeSphere is unavailable".to_string())?
                        .call1((number("radius")?,)),
                    _ => return Err(format!("primitive is not allowlisted: {primitive}")),
                }
                .map_err(py_error)?;
                let object = attr(document, "addObject")
                    .ok_or_else(|| "document object creation is unavailable".to_string())?
                    .call1(("Part::Feature", object_name))
                    .map_err(py_error)?;
                object.setattr("Shape", shape).map_err(py_error)?;
                Ok(vec![object_name.into()])
            }
            "create_object" => {
                let object_name = operation
                    .get("object")
                    .and_then(Value::as_str)
                    .ok_or_else(|| "object is required".to_string())?;
                let kind = operation
                    .get("kind")
                    .and_then(Value::as_str)
                    .ok_or_else(|| "kind is required".to_string())?;
                if ![
                    "Part::Feature",
                    "PartDesign::Feature",
                    "PartDesign::Body",
                    "PartDesign::FeaturePython",
                    "Sketcher::SketchObject",
                ]
                .contains(&kind)
                {
                    return Err(format!("object type is not allowlisted: {kind}"));
                }
                let object = attr(document, "addObject")
                    .ok_or_else(|| "document object creation is unavailable".to_string())?
                    .call1((kind, object_name))
                    .map_err(py_error)?;
                if let Some(properties) = operation.get("properties").and_then(Value::as_object) {
                    for (property_name, value) in properties {
                        declared_property(&object, property_name)?;
                        let value = typed_value(py, value)?;
                        object
                            .setattr(property_name.as_str(), value.bind(py))
                            .map_err(py_error)?;
                    }
                }
                Ok(vec![object_name.into()])
            }
            "delete_object" => {
                let target = operation
                    .get("target")
                    .and_then(Value::as_str)
                    .ok_or_else(|| "target is required".to_string())?;
                let (object, _) = self.resolve(document, target)?;
                if operation
                    .get("require_no_dependents")
                    .and_then(Value::as_bool)
                    .unwrap_or(true)
                    && !list_attr(&object, "InList").is_empty()
                {
                    return Err("delete would leave dependent objects".into());
                }
                let name = string_attr(&object, "Name")
                    .ok_or_else(|| "target has no internal name".to_string())?;
                attr(document, "removeObject")
                    .ok_or_else(|| "document deletion is unavailable".to_string())?
                    .call1((name,))
                    .map_err(py_error)?;
                Ok(vec![target.into()])
            }
            "boolean" => {
                let left = self
                    .resolve(
                        document,
                        operation
                            .get("left")
                            .and_then(Value::as_str)
                            .ok_or_else(|| "left is required".to_string())?,
                    )?
                    .0;
                let right = self
                    .resolve(
                        document,
                        operation
                            .get("right")
                            .and_then(Value::as_str)
                            .ok_or_else(|| "right is required".to_string())?,
                    )?
                    .0;
                let result_name = operation
                    .get("result")
                    .and_then(Value::as_str)
                    .ok_or_else(|| "result is required".to_string())?;
                let type_id = match operation.get("operation").and_then(Value::as_str) {
                    Some("cut") => "Part::Cut",
                    Some("fuse") => "Part::Fuse",
                    Some("common") => "Part::MultiCommon",
                    Some(name) => {
                        return Err(format!("boolean operation is not allowlisted: {name}"));
                    }
                    None => return Err("boolean operation is required".into()),
                };
                let result = attr(document, "addObject")
                    .ok_or_else(|| "document object creation is unavailable".to_string())?
                    .call1((type_id, result_name))
                    .map_err(py_error)?;
                if type_id == "Part::MultiCommon" {
                    let shapes = pyo3::types::PyList::new(py, [left.clone(), right.clone()])
                        .map_err(py_error)?;
                    result.setattr("Shapes", shapes).map_err(py_error)?;
                } else {
                    result.setattr("Base", left).map_err(py_error)?;
                    result.setattr("Tool", right).map_err(py_error)?;
                }
                Ok(vec![result_name.into()])
            }
            _ => Err(format!("operation is not supported: {name}")),
        }
    }

    fn validate_selection(
        &self,
        document: &Bound<'_, PyAny>,
        gui: Option<&Bound<'_, PyAny>>,
        operation: &Value,
    ) -> Result<(), String> {
        let gui = gui.ok_or_else(|| "selection requires GUI mode".to_string())?;
        let selection = attr(gui, "Selection")
            .ok_or_else(|| "FreeCAD selection API is unavailable".to_string())?;
        let mode = operation
            .get("mode")
            .and_then(Value::as_str)
            .unwrap_or("replace");
        if !["replace", "add", "remove"].contains(&mode) {
            return Err(format!("selection mode is not allowlisted: {mode}"));
        }
        let method = if mode == "remove" {
            "removeSelection"
        } else if mode == "replace" {
            "clearSelection"
        } else {
            "addSelection"
        };
        attr(&selection, method)
            .ok_or_else(|| "FreeCAD selection API is unavailable".to_string())?;
        let targets = operation
            .get("targets")
            .and_then(Value::as_array)
            .ok_or_else(|| "targets must be an array".to_string())?;
        for target in targets.iter().filter_map(Value::as_str) {
            self.resolve(document, target)?;
        }
        Ok(())
    }

    fn validate_view(
        &self,
        gui: Option<&Bound<'_, PyAny>>,
        operation: &Value,
    ) -> Result<(), String> {
        let gui = gui.ok_or_else(|| "view requires GUI mode".to_string())?;
        let active =
            attr(gui, "ActiveDocument").ok_or_else(|| "active view is unavailable".to_string())?;
        let view =
            attr(&active, "ActiveView").ok_or_else(|| "active view is unavailable".to_string())?;
        let operation = operation
            .get("operation")
            .and_then(Value::as_str)
            .ok_or_else(|| "view operation is required".to_string())?;
        let method = match operation {
            "fit_all" => "fitAll",
            "view_axo" => "viewAxonometric",
            "view_front" => "viewFront",
            "view_rear" => "viewRear",
            "view_left" => "viewLeft",
            "view_right" => "viewRight",
            "view_top" => "viewTop",
            "view_bottom" => "viewBottom",
            _ => return Err(format!("view operation is not allowlisted: {operation}")),
        };
        attr(&view, method).ok_or_else(|| "view operation is unavailable".to_string())?;
        Ok(())
    }

    fn validate_visibility(
        &self,
        document: &Bound<'_, PyAny>,
        operation: &Value,
    ) -> Result<(), String> {
        let target = operation
            .get("target")
            .and_then(Value::as_str)
            .ok_or_else(|| "target is required".to_string())?;
        let (object, _) = self.resolve(document, target)?;
        attr(&object, "ViewObject").ok_or_else(|| "target has no view object".to_string())?;
        operation
            .get("visible")
            .and_then(Value::as_bool)
            .ok_or_else(|| "visible must be boolean".to_string())?;
        Ok(())
    }

    fn resolve<'py>(
        &self,
        document: &Bound<'py, PyAny>,
        reference: &str,
    ) -> Result<(Bound<'py, PyAny>, Vec<String>), String> {
        if !reference.starts_with("fc://") {
            return Err("target must be an opaque fc:// reference".into());
        }
        let path = reference
            .split('@')
            .next()
            .unwrap_or_default()
            .split('/')
            .collect::<Vec<_>>();
        let index = path
            .iter()
            .position(|part| *part == "object")
            .ok_or_else(|| "reference does not identify an object".to_string())?;
        let name = percent_decode(
            path.get(index + 1)
                .ok_or_else(|| "reference has no object name".to_string())?,
        );
        let object = call1(document, "getObject", name.as_str())
            .filter(|object| !object.is_none())
            .ok_or_else(|| format!("object is not open: {name}"))?;
        Ok((
            object,
            path[index + 2..]
                .iter()
                .map(|part| percent_decode(part))
                .collect(),
        ))
    }
}

fn typed_value<'py>(py: Python<'py>, value: &Value) -> Result<Py<PyAny>, String> {
    if let Some(object) = value.as_object() {
        if let (Some(number), Some(unit)) = (
            object.get("value"),
            object.get("unit").and_then(Value::as_str),
        ) {
            let freecad = pyo3::types::PyModule::import(py, "FreeCAD").map_err(py_error)?;
            let units = attr(freecad.as_any(), "Units")
                .ok_or_else(|| "FreeCAD.Units is unavailable".to_string())?;
            let quantity = format!(
                "{} {unit}",
                number.as_f64().ok_or("quantity value must be numeric")?
            );
            return attr(&units, "Quantity")
                .ok_or_else(|| "FreeCAD.Units.Quantity is unavailable".to_string())?
                .call1((quantity,))
                .map(|value| value.unbind())
                .map_err(py_error);
        }
    }
    if value.is_array() || value.is_object() {
        return Err("complex property values require an adapter-specific operation".into());
    }
    if let Some(number) = value.as_f64() {
        if !number.is_finite() {
            return Err("numeric values must be finite".into());
        }
    }
    py_value(py, value).map_err(py_error)
}

fn declared_property(object: &Bound<'_, PyAny>, property: &str) -> Result<(), String> {
    if !list_attr(object, "PropertiesList")
        .iter()
        .filter_map(|value| value.extract::<String>().ok())
        .any(|value| value == property)
    {
        return Err(format!(
            "property is not declared on the target: {property}"
        ));
    }
    Ok(())
}

fn property_name(value: &str) -> Result<String, String> {
    if value.is_empty() || value.contains('.') || value.contains('(') || value.contains("__") {
        return Err("property names must be declared, simple identifiers".into());
    }
    Ok(value.into())
}

fn number(value: &Value) -> Result<f64, String> {
    let value = value.get("value").unwrap_or(value);
    let number = value
        .as_f64()
        .ok_or_else(|| "parameter must be a number".to_string())?;
    if !number.is_finite() || number < 0.0 {
        return Err("parameter must be a non-negative finite number".into());
    }
    Ok(number)
}

fn subelement_index(parts: &[String], kind: &str) -> Result<i32, String> {
    let index = parts
        .iter()
        .position(|part| part == kind)
        .ok_or_else(|| format!("reference has no numeric {kind} index"))?;
    parts
        .get(index + 1)
        .ok_or_else(|| format!("reference has no numeric {kind} index"))?
        .parse()
        .map_err(|_| format!("reference has no numeric {kind} index"))
}

fn subelement_name(parts: &[String]) -> Option<String> {
    if parts.len() >= 2 && ["face", "edge", "vertex", "constraint"].contains(&parts[0].as_str()) {
        Some(format!("{}{}", capitalize(&parts[0]), parts[1]))
    } else {
        None
    }
}

fn capitalize(value: &str) -> String {
    let mut chars = value.chars();
    chars
        .next()
        .map(|first| first.to_uppercase().collect::<String>() + chars.as_str())
        .unwrap_or_default()
}

fn py_error(error: PyErr) -> String {
    error.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn property_names_reject_attribute_traversal() {
        assert!(property_name("Length").is_ok());
        assert!(property_name("Proxy.__class__").is_err());
        assert!(property_name("setExpression()").is_err());
    }

    #[test]
    fn numbers_reject_negative_and_non_numeric_values() {
        assert_eq!(number(&json!(5.0)).unwrap(), 5.0);
        assert!(number(&json!(-1.0)).is_err());
        assert!(number(&json!("5")).is_err());
    }

    #[test]
    fn subelement_names_are_reconstructed_only_from_reference_parts() {
        assert_eq!(
            subelement_name(&["face".into(), "4".into()]),
            Some("Face4".into())
        );
        assert_eq!(subelement_name(&["property".into(), "4".into()]), None);
    }
}
