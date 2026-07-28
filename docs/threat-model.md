# Threat model

## Trust boundaries

* FreeCAD documents are untrusted input. Labels, expressions, annotations,
  imported metadata, and property strings are quoted data.
* The native Rust extension has FreeCAD process authority. The only Python
  source is the loader required by FreeCAD's workbench discovery.
  It exposes no generic code-execution tool.
* The sidecar is the policy and MCP authority. It writes only inside the
  private artifact root unless a separately granted capability is added.
* The MCP host/LLM is not trusted to authorize itself. Tool arguments are
  validated again at runtime.

## Controls

* User-private rendezvous directory, strict secret/record/socket permissions,
  HMAC challenge-response, session-epoch binding, and bounded frames.
* Revision-qualified opaque references, exact identity requirements,
  preconditions, immutable preview hashes, rollback fingerprints, and
  single-use idempotency keys.
* Capability checks occur before bridge calls and again at commit. Audit events
  record denied and committed decisions without returning kernel traces.
* Artifacts are content-addressed, quota-limited, mode 0600, and named by
  hashes rather than document labels.
* Query recursion, traversal depth, result count, byte budgets, numeric input,
  and artifact size are bounded.

## Deliberate degraded behavior

Missing view APIs, unsupported adapters, unavailable recognizers, and bridge
busy states degrade to typed omissions or read-only errors. A rollback or
revision invariant failure blocks further writes until a coherent rescan is
available. A heuristic identity mapping is never silently used for a write.
