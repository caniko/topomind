use bridge_dto::BridgeSnapshot;
use context_compiler::compile_snapshot;
use serde::Deserialize;
use serde_json::Value;

#[derive(Deserialize)]
struct Golden {
    schema_version: String,
    session: String,
    document: String,
    revision: String,
    required_entity_kinds: Vec<String>,
    required_facts: Vec<ExpectedFact>,
    invariants: Invariants,
}

#[derive(Deserialize)]
struct ExpectedFact {
    kind: String,
    classification: String,
    parameters: Value,
    exactness: String,
    confidence_category: String,
    evidence_kind: String,
}

#[derive(Deserialize)]
struct Invariants {
    missing_link_targets: usize,
    all_geometric_quantities_have_units: bool,
    imported_feature_history_claimed: bool,
}

#[test]
fn simple_document_ccir_matches_golden_contract() {
    let snapshot: BridgeSnapshot = serde_json::from_str(include_str!(
        "../../../../fixtures/bridge-dto/simple_document.json"
    ))
    .unwrap();
    let golden: Golden = serde_json::from_str(include_str!(
        "../../../../fixtures/ccir-golden/simple_document.json"
    ))
    .unwrap();
    let graph = compile_snapshot(&snapshot).unwrap();
    assert_eq!(graph.schema_version, golden.schema_version);
    assert_eq!(graph.session, golden.session);
    assert_eq!(graph.document, golden.document);
    assert_eq!(graph.revision.id(), golden.revision);
    for kind in golden.required_entity_kinds {
        assert!(
            graph.entities.values().any(|entity| entity.kind == kind),
            "missing entity kind"
        );
    }
    assert_eq!(
        graph
            .diagnostics
            .iter()
            .filter(|diagnostic| diagnostic.code == "missing_link_target")
            .count(),
        golden.invariants.missing_link_targets
    );
    assert!(golden.invariants.all_geometric_quantities_have_units);
    assert!(!golden.invariants.imported_feature_history_claimed);
    for expected in golden.required_facts {
        let fact = graph
            .facts
            .iter()
            .find(|fact| {
                fact.kind == expected.kind && fact.classification == expected.classification
            })
            .expect("missing golden semantic fact");
        assert_eq!(
            fact.parameters.get("radius_mm"),
            expected.parameters.get("radius_mm")
        );
        assert_eq!(
            fact.parameters.get("diameter_mm"),
            expected.parameters.get("diameter_mm")
        );
        assert_eq!(
            fact.parameters.get("depth_mm"),
            expected.parameters.get("depth_mm")
        );
        assert_eq!(
            serde_json::to_value(&fact.exactness).unwrap(),
            Value::String(expected.exactness)
        );
        assert_eq!(fact.confidence.category, expected.confidence_category);
        assert!(fact.evidence.iter().any(|reference| {
            graph
                .entity(reference)
                .is_some_and(|entity| entity.kind == expected.evidence_kind)
        }));
    }
}
