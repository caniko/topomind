# Topomind

Topomind is a local FreeCAD Semantic Context MCP server. Its FreeCAD workbench,
extractors, observers, typed executor, authenticated bridge, and sidecar are
Rust. FreeCAD's required `Init.py`/`InitGui.py` files only load the native
extension.

The default policy is `never_write`. Model changes are typed ChangeSets and
must pass revision, identity, precondition, preview-fingerprint, validation,
and policy checks before a bridge can commit them. Document text is data; it
cannot select capabilities, paths, endpoints, or approvals.

## Repository layout

* `freecad-addon/SemanticMCP` — the FreeCAD loader and packaged native
  extension.
* `freecad-extension` — PyO3 workbench, main-thread extractor, observer layer,
  authenticated bridge, typed operations, and explicit workbench commands.
* `sidecar/crates` — CCIR, bridge DTOs, compiler, geometry, query, revisions,
  policy, artifacts, IPC, sessions, MCP adapter, and the `topomind` binary.
* `schemas` — language-neutral JSON contracts and generated schema manifests.
* `fixtures` — portable bridge snapshots, CCIR golden assertions, queries, and
  ChangeSets used without a FreeCAD installation.
* `docs` — architecture, schema, adapter, operation, and threat-model notes.

## Local development

```sh
nix develop
cargo test --workspace
cargo clippy --workspace --all-targets -- -D warnings
cargo fmt --all -- --check
cargo run -p topomind -- --validate-schemas --check --root .
cargo build -p topomind-freecad-extension
```

Run the fixture MCP server:

```sh
cargo run -p topomind -- --fixture fixtures/bridge-dto/simple_document.json --no-discover
```

The server speaks MCP JSON-RPC over stdio. The addon publishes an authenticated
rendezvous record below `$XDG_RUNTIME_DIR/topomind` and does not open a socket
when imported. The sidecar discovers those records automatically, or accepts
`--bridge-socket ENDPOINT --bridge-secret PATH` explicitly.

Build the FreeCAD addon archive:

```sh
cargo run -p topomind -- --package-addon --native target/release/libSemanticMCP_native.so --output dist/topomind-freecad-addon.zip
```

The bridge probes the running FreeCAD APIs and advertises capabilities at
runtime. Unsupported APIs fail closed as typed read-only or unavailable
operations rather than relying on a guessed version threshold.
