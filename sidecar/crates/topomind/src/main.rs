use artifact_store::ArtifactStore;
use bridge_dto::{BridgeHello, SessionAdvertisement};
use mcp_adapter::McpServer;
use policy::{Policy, PolicyProfile, WorkspaceRules};
use session_manager::SessionManager;
use std::fs;
use std::io::{self, BufReader, BufWriter, Write};
use std::path::Path;
use std::path::PathBuf;
use zip::write::{SimpleFileOptions, ZipWriter};
use zip::{CompressionMethod, DateTime};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args: Vec<String> = std::env::args().collect();
    if has_flag(&args, "--validate-schemas") {
        return validate_schemas(
            argument(&args, "--root")
                .map(PathBuf::from)
                .unwrap_or(std::env::current_dir()?),
            has_flag(&args, "--check"),
        );
    }
    if has_flag(&args, "--package-addon") {
        let source = argument(&args, "--source")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from("freecad-addon/SemanticMCP"));
        let native = argument(&args, "--native")
            .map(PathBuf::from)
            .ok_or("--native is required")?;
        let output = argument(&args, "--output")
            .map(PathBuf::from)
            .ok_or("--output is required")?;
        return package_addon(&source, &native, &output);
    }
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
    println!();
    println!("Maintenance: topomind --validate-schemas [--check] [--root PATH]");
    println!("            topomind --package-addon --native PATH --output PATH [--source PATH]");
}

fn validate_schemas(root: PathBuf, check: bool) -> Result<(), Box<dyn std::error::Error>> {
    let schema_root = root.join("schemas");
    let mut entries = Vec::new();
    collect_schema_paths(&schema_root, &mut entries)?;
    entries.sort();
    let mut schemas = Vec::new();
    for path in entries {
        let value: serde_json::Value = serde_json::from_slice(&fs::read(&path)?)?;
        let object = value
            .as_object()
            .ok_or_else(|| format!("{}: top-level schema must be an object", path.display()))?;
        for required in ["$schema", "$id", "title", "type", "properties"] {
            if !object.contains_key(required) {
                return Err(format!("{}: missing schema key {required}", path.display()).into());
            }
        }
        if object.get("type").and_then(serde_json::Value::as_str) != Some("object") {
            return Err(format!("{}: top-level schema must be an object", path.display()).into());
        }
        if !object
            .get("$id")
            .and_then(serde_json::Value::as_str)
            .unwrap_or_default()
            .starts_with("https://github.com/caniko/topomind/")
        {
            return Err(format!(
                "{}: schema id is not owned by caniko/topomind",
                path.display()
            )
            .into());
        }
        if !object
            .get("properties")
            .is_some_and(serde_json::Value::is_object)
        {
            return Err(format!("{}: properties must be an object", path.display()).into());
        }
        if let Some(required) = object.get("required").and_then(serde_json::Value::as_array) {
            let properties = object
                .get("properties")
                .and_then(serde_json::Value::as_object)
                .unwrap();
            for field in required.iter().filter_map(serde_json::Value::as_str) {
                if !properties.contains_key(field) {
                    return Err(format!(
                        "{}: required field {field} is not declared",
                        path.display()
                    )
                    .into());
                }
            }
        }
        let relative = path
            .strip_prefix(&root)?
            .to_string_lossy()
            .replace(std::path::MAIN_SEPARATOR, "/");
        schemas.push(serde_json::json!({"path": relative, "id": object["$id"], "title": object["title"], "sha256": ccir_core::sha256_json(&value)?.trim_start_matches("sha256:")}));
    }
    let schema_count = schemas.len();
    let expected = serde_json::to_string_pretty(
        &serde_json::json!({"schema_version": "manifest/1.0", "schemas": schemas}),
    )? + "\n";
    let manifest_path = schema_root.join("generated/manifest.json");
    if check {
        let actual = fs::read_to_string(&manifest_path)?;
        if actual != expected {
            return Err(format!(
                "{} is stale; run topomind --validate-schemas",
                manifest_path.display()
            )
            .into());
        }
    } else {
        fs::create_dir_all(manifest_path.parent().unwrap())?;
        fs::write(&manifest_path, expected)?;
    }
    println!("validated {schema_count} schemas");
    Ok(())
}

fn collect_schema_paths(
    root: &Path,
    output: &mut Vec<PathBuf>,
) -> Result<(), Box<dyn std::error::Error>> {
    for entry in fs::read_dir(root)? {
        let path = entry?.path();
        if path.is_dir() {
            if path.file_name().and_then(|name| name.to_str()) != Some("generated") {
                collect_schema_paths(&path, output)?;
            }
        } else if path.extension().and_then(|extension| extension.to_str()) == Some("json") {
            output.push(path);
        }
    }
    Ok(())
}

fn package_addon(
    source: &Path,
    native: &Path,
    output: &Path,
) -> Result<(), Box<dyn std::error::Error>> {
    fs::create_dir_all(output.parent().unwrap_or_else(|| Path::new(".")))?;
    let file = fs::File::create(output)?;
    let mut archive = ZipWriter::new(file);
    let options = SimpleFileOptions::default()
        .compression_method(CompressionMethod::Deflated)
        .last_modified_time(DateTime::from_date_and_time(1980, 1, 1, 0, 0, 0)?)
        .unix_permissions(0o600);
    let mut files = fs::read_dir(source)?
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .filter(|path| {
            path.is_file()
                && path.extension().and_then(|extension| extension.to_str()) == Some("py")
        })
        .collect::<Vec<_>>();
    files.sort();
    for path in files {
        let name = path.file_name().unwrap().to_string_lossy();
        archive.start_file(format!("SemanticMCP/{name}"), options)?;
        archive.write_all(&fs::read(path)?)?;
    }
    let native_name = native
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or("native extension has no filename")?;
    let native_name = native_name.strip_prefix("lib").unwrap_or(native_name);
    archive.start_file(format!("SemanticMCP/{native_name}"), options)?;
    archive.write_all(&fs::read(native)?)?;
    archive.finish()?;
    println!("wrote {}", output.display());
    Ok(())
}
