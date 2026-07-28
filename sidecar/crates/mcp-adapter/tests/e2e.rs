use artifact_store::ArtifactStore;
use bridge_dto::BridgeSnapshot;
use mcp_adapter::McpServer;
use policy::{Policy, PolicyProfile};
use serde_json::Value;
use session_manager::SessionManager;

fn server(profile: PolicyProfile) -> McpServer {
    let snapshot: BridgeSnapshot = serde_json::from_str(include_str!(
        "../../../../fixtures/bridge-dto/simple_document.json"
    ))
    .unwrap();
    let mut sessions = SessionManager::new();
    sessions.register_fixture(snapshot).unwrap();
    let root = std::env::temp_dir().join(format!(
        "topomind-e2e-{}-{}",
        std::process::id(),
        uuid::Uuid::new_v4()
    ));
    McpServer::new(
        sessions,
        Policy::new(profile, "e2e", b"fixture-secret"),
        ArtifactStore::new(root, 4 * 1024 * 1024).unwrap(),
    )
}

fn call(server: &mut McpServer, id: usize, name: &str, arguments: Value) -> Value {
    let request = serde_json::json!({"jsonrpc": "2.0", "id": id, "method": "tools/call", "params": {"name": name, "arguments": arguments}});
    serde_json::from_str(&server.handle_json(&request.to_string()).unwrap()).unwrap()
}

#[test]
fn fixture_mcp_reads_query_measurement_and_bounded_context() {
    let mut server = server(PolicyProfile::NeverWrite);
    let context = call(
        &mut server,
        1,
        "cad.get_context",
        serde_json::json!({"session": "fixture-session", "document": "simple-document", "budget": {"max_entities": 4, "max_inline_bytes": 32_000}}),
    );
    assert!(context["result"]["structuredContent"]["context"]["selection"].is_array());
    let face = "fc://session/fixture-session/document/simple-document/object/Body/face/1@g0.m0.f0.v0.fixture-epoch";
    let query = call(
        &mut server,
        2,
        "cad.query_entities",
        serde_json::json!({"session": "fixture-session", "document": "simple-document", "revision": "g0.m0.f0.v0.fixture-epoch", "from": "document", "kind": ["cad.face"], "where": {"geometry.surface_type": {"eq": "cylinder"}}, "select": ["ref", "geometry.radius"], "limit": 10}),
    );
    assert_eq!(
        query["result"]["structuredContent"]["matches"][0]["ref"],
        face
    );
    let measurement = call(
        &mut server,
        3,
        "cad.measure",
        serde_json::json!({"session": "fixture-session", "document": "simple-document", "kind": "diameter", "entities": [face]}),
    );
    assert_eq!(measurement["result"]["structuredContent"]["value"], 6.0);
    let denied = call(
        &mut server,
        4,
        "cad.preview_change",
        serde_json::json!({"changeset": serde_json::from_str::<Value>(include_str!("../../../../fixtures/changesets/set_datum.json")).unwrap()}),
    );
    assert_eq!(denied["error"]["data"]["type"], "permission_denied");
    assert_eq!(server.policy.audit().last().unwrap().status, "denied");
}

#[test]
fn developer_policy_requires_preview_hash_and_records_commit() {
    let mut server = server(PolicyProfile::Developer);
    let changeset: Value = serde_json::from_str(include_str!(
        "../../../../fixtures/changesets/set_datum.json"
    ))
    .unwrap();
    let preview = call(
        &mut server,
        10,
        "cad.preview_change",
        serde_json::json!({"changeset": changeset}),
    );
    let preview_value = &preview["result"]["structuredContent"];
    assert_eq!(preview_value["rolled_back"], true);
    assert_eq!(preview_value["rollback_fingerprint_verified"], true);
    let preview_hash = preview_value["preview_hash"].as_str().unwrap();
    let committed = call(
        &mut server,
        11,
        "cad.apply_change_set",
        serde_json::json!({"changeset": serde_json::from_str::<Value>(include_str!("../../../../fixtures/changesets/set_datum.json")).unwrap(), "preview_hash": preview_hash}),
    );
    assert_eq!(
        committed["result"]["structuredContent"]["validation"]["status"],
        "pass"
    );
    assert!(
        committed["result"]["structuredContent"]["resulting_revision"]
            .as_str()
            .unwrap()
            .starts_with("g1.")
    );
    let undone = call(
        &mut server,
        12,
        "cad.undo",
        serde_json::json!({"session": "fixture-session", "document": "simple-document"}),
    );
    assert!(
        undone["result"]["structuredContent"]["audit_id"]
            .as_str()
            .is_some()
    );
}
