use artifact_store::ArtifactStore;
use bridge_dto::{BridgeHello, SessionAdvertisement};
use mcp_adapter::McpServer;
use policy::{Policy, PolicyProfile, WorkspaceRules};
use session_manager::SessionManager;
use std::fs;
use std::io::{self, BufReader, BufWriter};
use std::path::PathBuf;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args: Vec<String> = std::env::args().collect();
    if args.iter().any(|arg| arg == "--help" || arg == "-h") {
        print_help();
        return Ok(());
    }
    if args.iter().any(|arg| arg == "--version") {
        println!("topomind 0.1.0");
        return Ok(());
    }

    let mut sessions = SessionManager::new();
    if let Some(path) =
        argument(&args, "--fixture").or_else(|| std::env::var("TOPOMIND_FIXTURE").ok())
    {
        sessions.load_fixture_file(path)?;
    }
    if let (Some(socket), Some(secret_file)) = (
        argument(&args, "--bridge-socket").or_else(|| std::env::var("TOPOMIND_BRIDGE_SOCKET").ok()),
        argument(&args, "--bridge-secret").or_else(|| std::env::var("TOPOMIND_BRIDGE_SECRET").ok()),
    ) {
        let secret = fs::read(secret_file)?;
        let mut client = ipc_client::IpcClient::connect_endpoint(&socket, &secret)?;
        let response = client.authenticate()?;
        let payload = response.payload.unwrap_or_default();
        let hello: BridgeHello =
            serde_json::from_value(payload.get("hello").cloned().unwrap_or_default())?;
        let session = payload
            .get("session")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("freecad")
            .to_owned();
        let epoch = response.session_epoch.clone();
        let documents = payload
            .get("documents")
            .and_then(serde_json::Value::as_array)
            .map(|values| {
                values
                    .iter()
                    .filter_map(serde_json::Value::as_str)
                    .map(str::to_owned)
                    .collect()
            })
            .unwrap_or_default();
        sessions.register_ipc(
            SessionAdvertisement {
                session,
                session_epoch: epoch,
                endpoint: socket,
                pid: 0,
                started_at_ms: 0,
                hello,
                documents,
            },
            client,
        );
    } else if !has_flag(&args, "--no-discover") {
        discover_sessions(&mut sessions)?;
    }

    let profile = match std::env::var("TOPOMIND_POLICY").as_deref() {
        Ok("approve_each") => PolicyProfile::ApproveEach,
        Ok("approve_high_risk") => PolicyProfile::ApproveHighRisk,
        Ok("workspace_policy") => PolicyProfile::WorkspacePolicy,
        Ok("developer") => PolicyProfile::Developer,
        _ => PolicyProfile::NeverWrite,
    };
    let policy_secret = std::env::var("TOPOMIND_POLICY_SECRET")
        .unwrap_or_else(|_| uuid::Uuid::new_v4().to_string());
    let policy = if matches!(profile, PolicyProfile::WorkspacePolicy) {
        let path = std::env::var("TOPOMIND_WORKSPACE_POLICY")
            .map_err(|_| "TOPOMIND_WORKSPACE_POLICY is required for workspace_policy")?;
        let rules: WorkspaceRules = serde_json::from_slice(&fs::read(path)?)?;
        Policy::new(profile, "stdio-client", &policy_secret).with_workspace_rules(rules)?
    } else {
        Policy::new(profile, "stdio-client", policy_secret)
    };
    let artifact_root = argument(&args, "--artifact-root")
        .map(PathBuf::from)
        .or_else(|| {
            std::env::var_os("XDG_RUNTIME_DIR")
                .map(PathBuf::from)
                .map(|path| path.join("topomind/artifacts"))
        })
        .unwrap_or_else(|| std::env::temp_dir().join("topomind-artifacts"));
    let artifacts = ArtifactStore::new(artifact_root, 256 * 1024 * 1024)?;
    let mut server = McpServer::new(sessions, policy, artifacts);
    let stdin = BufReader::new(io::stdin().lock());
    let stdout = BufWriter::new(io::stdout().lock());
    server.run_stdio(stdin, stdout)?;
    Ok(())
}

fn argument(args: &[String], name: &str) -> Option<String> {
    args.windows(2)
        .find(|pair| pair[0] == name)
        .map(|pair| pair[1].clone())
}

fn has_flag(args: &[String], name: &str) -> bool {
    args.iter().any(|argument| argument == name)
}

fn discover_sessions(sessions: &mut SessionManager) -> Result<(), Box<dyn std::error::Error>> {
    let root = std::env::var_os("XDG_RUNTIME_DIR")
        .map(PathBuf::from)
        .map(|path| path.join("topomind"))
        .unwrap_or_else(|| std::env::temp_dir().join("topomind"));
    let entries = match fs::read_dir(root) {
        Ok(entries) => entries,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error.into()),
    };
    for entry in entries.flatten().filter(|entry| {
        entry
            .path()
            .extension()
            .is_some_and(|extension| extension == "json")
    }) {
        let record: serde_json::Value = match serde_json::from_slice(&fs::read(entry.path())?) {
            Ok(record) => record,
            Err(_) => continue,
        };
        let endpoint = record
            .get("endpoint")
            .and_then(serde_json::Value::as_str)
            .unwrap_or_default();
        let secret_path = record
            .get("secret_path")
            .and_then(serde_json::Value::as_str)
            .unwrap_or_default();
        if endpoint.is_empty() || secret_path.is_empty() {
            continue;
        }
        let secret = match fs::read(secret_path) {
            Ok(secret) => secret,
            Err(_) => continue,
        };
        let mut client = match ipc_client::IpcClient::connect_endpoint(endpoint, &secret) {
            Ok(client) => client,
            Err(_) => continue,
        };
        let response = match client.authenticate() {
            Ok(response) => response,
            Err(_) => continue,
        };
        let payload = response.payload.unwrap_or_default();
        let hello: BridgeHello =
            match serde_json::from_value(payload.get("hello").cloned().unwrap_or_default()) {
                Ok(hello) => hello,
                Err(_) => continue,
            };
        let session = payload
            .get("session")
            .and_then(serde_json::Value::as_str)
            .unwrap_or_default()
            .to_owned();
        if session.is_empty() {
            continue;
        }
        let documents = payload
            .get("documents")
            .and_then(serde_json::Value::as_array)
            .map(|values| {
                values
                    .iter()
                    .filter_map(serde_json::Value::as_str)
                    .map(str::to_owned)
                    .collect()
            })
            .unwrap_or_default();
        sessions.register_ipc(
            SessionAdvertisement {
                session,
                session_epoch: response.session_epoch.clone(),
                endpoint: endpoint.into(),
                pid: record
                    .get("pid")
                    .and_then(serde_json::Value::as_u64)
                    .unwrap_or_default() as u32,
                started_at_ms: record
                    .get("started_at_ms")
                    .and_then(serde_json::Value::as_u64)
                    .unwrap_or_default() as u128,
                hello,
                documents,
            },
            client,
        );
    }
    Ok(())
}

fn print_help() {
    println!("topomind — FreeCAD Semantic Context MCP");
    println!();
    println!(
        "Usage: topomind [--fixture PATH] [--bridge-socket ENDPOINT --bridge-secret PATH] [--no-discover]"
    );
    println!();
    println!("MCP is served over stdio. The default policy is never_write.");
    println!(
        "TOPOMIND_POLICY may be never_write, approve_each, approve_high_risk, workspace_policy, or developer."
    );
    println!(
        "workspace_policy additionally requires TOPOMIND_WORKSPACE_POLICY and a matching TOPOMIND_POLICY_SECRET."
    );
}
