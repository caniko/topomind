//! Deterministic measurements and conservative semantic recognizers.

use ccir_core::{
    Bounds, Entity, EntityRef, Exactness, Graph, Quantity, SemanticFact, Vec3, sha256_json,
};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use thiserror::Error;

#[derive(Debug, Error)]
pub enum GeometryError {
    #[error("measurement needs {expected} entities, got {actual}")]
    WrongEntityCount { expected: String, actual: usize },
    #[error("entity {0} has no measurable geometry")]
    NotMeasurable(String),
    #[error("invalid geometry value at {0}")]
    InvalidValue(String),
    #[error("serialization failed: {0}")]
    Serialization(#[from] serde_json::Error),
    #[error("core error: {0}")]
    Core(#[from] ccir_core::CcirError),
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MeasurementKind {
    Distance,
    Angle,
    Radius,
    Diameter,
    Length,
    Area,
    Volume,
    CenterOfMass,
    Bounds,
    WallThickness,
    Clearance,
    Transform,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct MeasurementRequest {
    pub kind: MeasurementKind,
    pub entities: Vec<EntityRef>,
    pub frame: Option<String>,
    pub tolerance: Option<Quantity>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct MeasurementResult {
    pub kind: MeasurementKind,
    pub value: Value,
    pub unit: Option<String>,
    pub exactness: Exactness,
    pub method: String,
    pub tolerance: Option<Quantity>,
    pub evidence: Vec<EntityRef>,
    pub degeneracy: Option<String>,
}

pub fn measure(
    graph: &Graph,
    request: &MeasurementRequest,
) -> Result<MeasurementResult, GeometryError> {
    let entities: Vec<&Entity> = request
        .entities
        .iter()
        .map(|reference| {
            graph
                .entity(reference)
                .ok_or_else(|| GeometryError::NotMeasurable(reference.clone()))
        })
        .collect::<Result<_, _>>()?;
    let evidence = request.entities.clone();
    match request.kind {
        MeasurementKind::Distance => {
            require_count(&entities, 2, "two")?;
            let first = point_for(entities[0])?;
            let second = point_for(entities[1])?;
            Ok(result(
                request,
                serde_json::json!(first.distance(&second)),
                Some("mm"),
                Exactness::ObservedExact,
                "analytic point/center distance",
                evidence,
            ))
        }
        MeasurementKind::Angle => {
            require_count(&entities, 2, "two")?;
            let first = direction_for(entities[0])?;
            let second = direction_for(entities[1])?;
            let cosine = first.dot(&second).clamp(-1.0, 1.0);
            Ok(result(
                request,
                serde_json::json!(cosine.acos().to_degrees()),
                Some("deg"),
                Exactness::ObservedExact,
                "analytic direction angle",
                evidence,
            ))
        }
        MeasurementKind::Radius | MeasurementKind::Diameter => {
            require_count(&entities, 1, "one")?;
            let radius = number_at(&entities[0].properties, &["geometry", "radius"])
                .or_else(|| number_at(&entities[0].properties, &["radius"]))
                .ok_or_else(|| GeometryError::NotMeasurable(entities[0].ref_.clone()))?;
            let value = if matches!(request.kind, MeasurementKind::Diameter) {
                radius * 2.0
            } else {
                radius
            };
            Ok(result(
                request,
                serde_json::json!(value),
                Some("mm"),
                Exactness::ObservedExact,
                "OCCT analytic radius",
                evidence,
            ))
        }
        MeasurementKind::Length => {
            require_count(&entities, 1, "one")?;
            let value = number_at(&entities[0].properties, &["length"])
                .or_else(|| number_at(&entities[0].properties, &["geometry", "length"]))
                .ok_or_else(|| GeometryError::NotMeasurable(entities[0].ref_.clone()))?;
            Ok(result(
                request,
                serde_json::json!(value),
                Some("mm"),
                Exactness::ObservedExact,
                "OCCT curve length",
                evidence,
            ))
        }
        MeasurementKind::Area | MeasurementKind::Volume => {
            require_count(&entities, 1, "one")?;
            let key = if matches!(request.kind, MeasurementKind::Area) {
                "area"
            } else {
                "volume"
            };
            let value = number_at(&entities[0].properties, &[key])
                .or_else(|| number_at(&entities[0].properties, &["mass_properties", key]))
                .ok_or_else(|| GeometryError::NotMeasurable(entities[0].ref_.clone()))?;
            Ok(result(
                request,
                serde_json::json!(value),
                Some(if key == "area" { "mm²" } else { "mm³" }),
                Exactness::ObservedExact,
                "OCCT mass property",
                evidence,
            ))
        }
        MeasurementKind::CenterOfMass => {
            require_count(&entities, 1, "one")?;
            let value = value_at(&entities[0].properties, &["center_of_mass"])
                .or_else(|| {
                    value_at(
                        &entities[0].properties,
                        &["mass_properties", "center_of_mass"],
                    )
                })
                .ok_or_else(|| GeometryError::NotMeasurable(entities[0].ref_.clone()))?;
            Ok(result(
                request,
                value.clone(),
                Some("mm"),
                Exactness::ObservedExact,
                "OCCT center of mass",
                evidence,
            ))
        }
        MeasurementKind::Bounds => {
            require_count(&entities, 1, "one")?;
            let bounds = entities[0]
                .bounds
                .as_ref()
                .ok_or_else(|| GeometryError::NotMeasurable(entities[0].ref_.clone()))?;
            Ok(result(
                request,
                serde_json::to_value(bounds)?,
                None,
                Exactness::ObservedExact,
                "axis-aligned bounding box",
                evidence,
            ))
        }
        MeasurementKind::WallThickness => {
            require_count(&entities, 1, "one")?;
            let bounds = entities[0]
                .bounds
                .as_ref()
                .ok_or_else(|| GeometryError::NotMeasurable(entities[0].ref_.clone()))?;
            let extents = [
                (bounds.max.x - bounds.min.x).abs(),
                (bounds.max.y - bounds.min.y).abs(),
                (bounds.max.z - bounds.min.z).abs(),
            ];
            let value = extents.into_iter().fold(f64::INFINITY, f64::min);
            Ok(result(
                request,
                serde_json::json!(value),
                Some("mm"),
                Exactness::ObservedApproximate,
                "minimum axis-aligned extent; not an offset/raycast proof",
                evidence,
            ))
        }
        MeasurementKind::Clearance => {
            require_count(&entities, 2, "two")?;
            let first = entities[0]
                .bounds
                .as_ref()
                .ok_or_else(|| GeometryError::NotMeasurable(entities[0].ref_.clone()))?;
            let second = entities[1]
                .bounds
                .as_ref()
                .ok_or_else(|| GeometryError::NotMeasurable(entities[1].ref_.clone()))?;
            let gap = axis_gap(first, second);
            Ok(result(
                request,
                serde_json::json!(gap),
                Some("mm"),
                Exactness::ObservedApproximate,
                "axis-aligned bounds clearance",
                evidence,
            ))
        }
        MeasurementKind::Transform => {
            require_count(&entities, 1, "one")?;
            let value = value_at(&entities[0].properties, &["placement"])
                .ok_or_else(|| GeometryError::NotMeasurable(entities[0].ref_.clone()))?;
            Ok(result(
                request,
                value.clone(),
                None,
                Exactness::ObservedExact,
                "normalized placement",
                evidence,
            ))
        }
    }
}

pub fn recognize_holes(graph: &Graph) -> Result<Vec<SemanticFact>, GeometryError> {
    let mut facts = Vec::new();
    for entity in graph.entities.values() {
        let surface_type = string_at(&entity.properties, &["geometry", "surface_type"])
            .or_else(|| string_at(&entity.properties, &["surface_type"]));
        if surface_type.as_deref() != Some("cylinder") {
            continue;
        }
        let orientation = string_at(&entity.properties, &["semantic", "orientation"])
            .or_else(|| string_at(&entity.properties, &["orientation"]));
        if orientation.as_deref() != Some("interior") {
            continue;
        }
        let radius = number_at(&entity.properties, &["geometry", "radius"])
            .or_else(|| number_at(&entity.properties, &["radius"]));
        let Some(radius) = radius else { continue };
        let depth = number_at(&entity.properties, &["geometry", "depth"])
            .or_else(|| number_at(&entity.properties, &["depth"]))
            .or_else(|| {
                entity.bounds.as_ref().map(|bounds| {
                    let extent = [
                        (bounds.max.x - bounds.min.x).abs(),
                        (bounds.max.y - bounds.min.y).abs(),
                        (bounds.max.z - bounds.min.z).abs(),
                    ];
                    extent.into_iter().fold(0.0, f64::max)
                })
            });
        let openings = number_at(&entity.properties, &["semantic", "openings"])
            .or_else(|| number_at(&entity.properties, &["openings"]))
            .unwrap_or(1.0);
        let counterbore = entity
            .links
            .iter()
            .filter(|link| link.relation == "adjacent_to")
            .filter_map(|link| graph.entity(&link.target))
            .filter_map(|adjacent| {
                number_at(&adjacent.properties, &["geometry", "radius"])
                    .or_else(|| number_at(&adjacent.properties, &["radius"]))
            })
            .any(|adjacent_radius| (adjacent_radius - radius).abs() > 1e-6);
        let classification = if counterbore {
            "counterbored_blind"
        } else if openings >= 2.0 {
            "through"
        } else {
            "blind"
        };
        let axis = value_at(&entity.properties, &["geometry", "axis"])
            .or_else(|| value_at(&entity.properties, &["axis"]))
            .cloned()
            .unwrap_or_else(|| serde_json::json!([0.0, 0.0, 1.0]));
        let parameters = serde_json::json!({
            "axis": axis,
            "radius_mm": radius,
            "diameter_mm": radius * 2.0,
            "depth_mm": depth,
            "openings": openings,
            "inferred_from_final_geometry": entity.source.as_ref().and_then(|source| source.object.clone()).is_some(),
        });
        let fact_ref = format!(
            "{}/fact/hole/{}@{}",
            graph.document,
            short_hash(&parameters)?,
            graph.revision.id()
        );
        facts.push(SemanticFact {
            ref_: fact_ref,
            kind: "semantic.hole".into(),
            classification: classification.into(),
            parameters,
            evidence: vec![entity.ref_.clone()],
            contradicting: Vec::new(),
            algorithm: "core.holes/1.0.0".into(),
            exactness: Exactness::Mixed,
            tolerance: Some(Quantity::new(0.01, "mm")),
            confidence: ccir_core::Confidence {
                category: "high".into(),
                score: Some(0.95),
            },
            explanation: format!(
                "Interior cylindrical face with radius {radius:.3} mm; classification {classification} is derived from openings and adjacent coaxial geometry."
            ),
        });
    }
    Ok(facts)
}

pub fn validate_shapes(graph: &Graph) -> Vec<ccir_core::Diagnostic> {
    graph
        .entities
        .values()
        .filter_map(|entity| {
            let valid = entity
                .properties
                .get("valid")
                .and_then(Value::as_bool)
                .unwrap_or(true);
            (!valid).then(|| ccir_core::Diagnostic {
                code: "invalid_shape".into(),
                severity: ccir_core::Severity::Error,
                message: "shape reports invalid geometry".into(),
                entity: Some(entity.ref_.clone()),
                retryable: false,
                evidence: vec![entity.ref_.clone()],
            })
        })
        .collect()
}

fn result(
    request: &MeasurementRequest,
    value: Value,
    unit: Option<&str>,
    exactness: Exactness,
    method: &str,
    evidence: Vec<EntityRef>,
) -> MeasurementResult {
    MeasurementResult {
        kind: request.kind.clone(),
        value,
        unit: unit.map(str::to_owned),
        exactness,
        method: method.into(),
        tolerance: request.tolerance.clone(),
        evidence,
        degeneracy: None,
    }
}

fn require_count<T>(entities: &[T], expected: usize, name: &str) -> Result<(), GeometryError> {
    (entities.len() == expected)
        .then_some(())
        .ok_or_else(|| GeometryError::WrongEntityCount {
            expected: name.into(),
            actual: entities.len(),
        })
}

fn value_at<'a>(value: &'a Value, path: &[&str]) -> Option<&'a Value> {
    path.iter()
        .try_fold(value, |current, key| current.get(*key))
}

fn number_at(value: &Value, path: &[&str]) -> Option<f64> {
    value_at(value, path).and_then(Value::as_f64)
}

fn string_at(value: &Value, path: &[&str]) -> Option<String> {
    value_at(value, path)
        .and_then(Value::as_str)
        .map(str::to_owned)
}

fn point_for(entity: &Entity) -> Result<Vec3, GeometryError> {
    let value = value_at(&entity.properties, &["point"])
        .or_else(|| value_at(&entity.properties, &["center_of_mass"]))
        .map(parse_vec3)
        .transpose()?;
    Ok(value.unwrap_or_else(|| {
        entity
            .bounds
            .as_ref()
            .map(Bounds::center)
            .unwrap_or_default()
    }))
}

fn direction_for(entity: &Entity) -> Result<Vec3, GeometryError> {
    let value = value_at(&entity.properties, &["geometry", "axis"])
        .or_else(|| value_at(&entity.properties, &["axis"]))
        .or_else(|| value_at(&entity.properties, &["normal"]))
        .ok_or_else(|| GeometryError::NotMeasurable(entity.ref_.clone()))?;
    parse_vec3(value)?
        .normalized()
        .ok_or_else(|| GeometryError::InvalidValue(entity.ref_.clone()))
}

fn parse_vec3(value: &Value) -> Result<Vec3, GeometryError> {
    let values = value
        .as_array()
        .ok_or_else(|| GeometryError::InvalidValue(value.to_string()))?;
    if values.len() != 3 {
        return Err(GeometryError::InvalidValue(value.to_string()));
    }
    Ok(Vec3::new(
        values[0]
            .as_f64()
            .ok_or_else(|| GeometryError::InvalidValue(value.to_string()))?,
        values[1]
            .as_f64()
            .ok_or_else(|| GeometryError::InvalidValue(value.to_string()))?,
        values[2]
            .as_f64()
            .ok_or_else(|| GeometryError::InvalidValue(value.to_string()))?,
    ))
}

fn axis_gap(first: &Bounds, second: &Bounds) -> f64 {
    let gaps = [
        (first.min.x - second.max.x)
            .max(second.min.x - first.max.x)
            .max(0.0),
        (first.min.y - second.max.y)
            .max(second.min.y - first.max.y)
            .max(0.0),
        (first.min.z - second.max.z)
            .max(second.min.z - first.max.z)
            .max(0.0),
    ];
    gaps.into_iter().fold(0.0, f64::max)
}

fn short_hash<T: serde::Serialize>(value: &T) -> Result<String, GeometryError> {
    Ok(sha256_json(value)?.trim_start_matches("sha256:")[..16].to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;
    use ccir_core::{Graph, Identity, Revision, Source};
    use std::collections::BTreeMap;

    fn face() -> Entity {
        Entity {
            ref_: "fc://s/d/face/1@g".into(),
            kind: "cad.face".into(),
            revision: "g".into(),
            identity: Identity::default(),
            properties: serde_json::json!({
                "geometry": {"surface_type": "cylinder", "radius": 3.0, "axis": [0.0, 0.0, 1.0], "depth": 12.0},
                "semantic": {"orientation": "interior", "openings": 2.0},
                "area": 100.0
            }),
            source: Some(Source {
                object: Some("Body".into()),
                subelement: Some("Face1".into()),
                extractor: "test".into(),
                kernel_tolerance: None,
                fingerprint: None,
            }),
            ..Entity::default()
        }
    }

    #[test]
    fn recognizes_evidence_carrying_through_hole() {
        let mut graph = Graph::new("s", "d", Revision::new("e"));
        let entity = face();
        graph.entities.insert(entity.ref_.clone(), entity);
        let facts = recognize_holes(&graph).unwrap();
        assert_eq!(facts[0].classification, "through");
        assert_eq!(facts[0].evidence.len(), 1);
    }

    #[test]
    fn measures_exact_radius() {
        let mut graph = Graph::new("s", "d", Revision::new("e"));
        let entity = face();
        let reference = entity.ref_.clone();
        graph.entities.insert(reference.clone(), entity);
        let result = measure(
            &graph,
            &MeasurementRequest {
                kind: MeasurementKind::Diameter,
                entities: vec![reference],
                frame: None,
                tolerance: None,
            },
        )
        .unwrap();
        assert_eq!(result.value, serde_json::json!(6.0));
        assert_eq!(result.unit.as_deref(), Some("mm"));
    }

    #[test]
    fn validates_invalid_shape() {
        let mut graph = Graph::new("s", "d", Revision::new("e"));
        let mut entity = face();
        entity.properties = serde_json::json!({"valid": false});
        graph.entities.insert(entity.ref_.clone(), entity);
        assert_eq!(validate_shapes(&graph).len(), 1);
        let _ = BTreeMap::<String, String>::new();
    }
}
