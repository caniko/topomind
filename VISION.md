# Topomind vision

Topomind is a local FreeCAD Semantic Context MCP server. It lets an agent read
and, under policy, change a FreeCAD document through typed operations instead
of screen scraping or arbitrary Python. The FreeCAD workbench, extractors,
observers, executor, authenticated bridge and sidecar are Rust; the only Python
is the `Init.py`/`InitGui.py` loader FreeCAD requires. It speaks MCP JSON-RPC
over stdio and discovers a running FreeCAD addon through a private rendezvous
record.

## Problem

An LLM working on CAD is blind: it cannot see document structure, cannot trust
labels, and must not be handed a script runner with the user's model at stake.
Topomind turns a FreeCAD document into a revisioned, queryable semantic graph —
entities, links, units, bounds, view and selection state, diagnostics, and
evidence-carrying facts — and exposes that graph through bounded MCP tools. When
the agent wants to change something, the change becomes a typed ChangeSet that
is checked before anything is committed, rather than a script that either works
or corrupts the file.

## Principles

- **`never_write` by default.** The default policy denies writes; model changes
  are typed ChangeSets that pass revision, identity, precondition,
  preview-fingerprint, validation and policy checks before a bridge commits
  them.
- **Document text is data.** Labels, expressions, annotations, imported
  metadata and property strings can never select capabilities, paths,
  endpoints or approvals.
- **The MCP host is not trusted.** Tool arguments are validated again at
  runtime; the sidecar, not the caller, is the policy and MCP authority.
- **Revisions are explicit.** Immutable graphs are keyed by a composite
  revision (geometry, metadata, focus, view, epoch); references are
  revision-qualified and opaque, a new epoch makes old live handles stale, and
  a rollback or invariant failure blocks writes until a coherent rescan.
- **Nothing is silently guessed.** Unsupported APIs, missing view data and busy
  bridges degrade to typed read-only or unavailable results; a heuristic
  identity mapping is never used for a write.
- **Facts carry their evidence.** A recognizer emits a `SemanticFact` only with
  its evidence, algorithm version, exactness, tolerance, confidence and
  explanation; an inferred feature is not claimed to exist.
- **The boundary is small and authenticated.** HMAC challenge-response over a
  user-private socket, bounded frames, audit events for denied and committed
  decisions, and content-addressed artifacts at mode 0600 named by hash.
- **No arbitrary code tool.** The executor accepts only allowlisted operation
  names and declared properties; adapters enrich the DTO and never grant
  capabilities.

## Scope

- A FreeCAD Rust extension: PyO3 workbench, main-thread extractor, observer
  layer and typed executor.
- A sidecar with CCIR compiler, revision store, session manager, policy engine,
  bounded query and geometry services, private artifact store, and the
  `topomind` binary.
- MCP tools over stdio for document context, queries, previews, commits and
  undo/redo, with typed retrieval suggestions when data is omitted.
- Language-neutral JSON schemas under `schemas/` with validated manifests.
- Portable fixtures: bridge snapshots, CCIR golden assertions, queries and
  ChangeSets that run without FreeCAD installed.
- Workbench adapters for document objects, Part/Part Design and Sketcher, with
  generic preservation of unknown third-party objects.
- Policy levels from `NeverWrite` through `ApproveEach`, `ApproveHighRisk`,
  signed `WorkspacePolicy` and `Developer`, all retaining revision, validation,
  audit and idempotency guards.

## Non-goals

- General-purpose code execution or a Python scripting tool for the model.
- Operating a remote or multi-user CAD service; the bridge is local and
  user-private.
- Treating imported B-Rep as editable features, or upgrading inferred geometry
  into claimed parametric history.
- Editing a document it cannot authenticate to; there is no guessed-version
  compatibility mode.

## Direction

The current commit history is build work (harbor inputs, toolchain profiles,
FreeCAD bridge port), following the two feature commits that implemented the
semantic-context MCP and ported the bridge to Rust. The architecture docs define
the read and write paths and the adapter contract; adding an adapter requires a
fixture, golden assertions, declared operations and validators, and preview,
rollback, commit, stale-reference, validation, undo and idempotency tests before
its capability is exposed. No further roadmap is committed.

## Place in the fleet

The repository appears in atlas's Simit updater list, so canix tracks it for
update checks; no canix module imports it. It ships packaging and a FreeCAD
addon archive rather than a fleet service.
