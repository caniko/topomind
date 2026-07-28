# Topomind

Topomind is a local FreeCAD Semantic Context MCP server. It keeps FreeCAD
integration small and Python-native while putting CCIR normalization,
revisioning, bounded queries, measurements, policy, audit, artifacts, and MCP
JSON-RPC in a Rust sidecar.

The default policy is `never_write`. Model changes are typed ChangeSets and
must pass revision, identity, precondition, preview-fingerprint, validation,
and policy checks before a bridge can commit them. Document text is data; it
cannot select capabilities, paths, endpoints, or approvals.

## Repository layout

* `freecad-addon/SemanticMCP` — FreeCAD main-thread extractor, observer layer,
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
python scripts/validate_schemas.py
python -m unittest discover -s tests -p 'test_*.py'
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
python scripts/package_addon.py --output dist/topomind-freecad-addon.zip
```

FreeCAD 1.1.3+ is the write-support baseline. Older versions may be read-only
when the bridge can extract safely; the bridge advertises that state explicitly.
