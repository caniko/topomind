# Architecture

```text
MCP host --stdio/JSON-RPC--> Rust adapter
                              |-- policy + audit
                              |-- session/revision store
                              |-- CCIR compiler + facts
                              |-- bounded query + geometry services
                              |-- private artifacts
                              `-- authenticated local IPC
                                      |
                                FreeCAD Python addon
                                (main-thread bridge)
```

The bridge copies FreeCAD state into versioned DTOs. The sidecar never retains
FreeCAD Python objects. Immutable `Graph` values are keyed by a composite
revision (`geometry`, `metadata`, `focus`, `view`, `epoch`) and are safe to
query concurrently. A new process epoch makes old live handles stale.

## Read path

1. The sidecar discovers a private rendezvous record and authenticates with a
   256-bit secret using HMAC challenge-response.
2. A snapshot request is serialized and dispatched onto FreeCAD's main thread.
3. The compiler normalizes entities, links, units, bounds, view/selection
   metadata, diagnostics, and evidence-carrying semantic facts.
4. The revision store records the immutable graph and structured diff.
5. MCP tools assemble only the requested detail and budget; omitted data gets a
   typed retrieval suggestion or an artifact URI.

## Write path

Every write follows:

```text
typed ChangeSet
  -> policy/capability check
  -> exact revision and identity checks
  -> preconditions
  -> preview transaction
  -> recompute + validation
  -> abort + base fingerprint verification
  -> exact preview hash and approval check
  -> replayed commit
  -> observed new revision + audit event
```

The Python executor accepts only allowlisted operation names and declared
FreeCAD properties. There is no standard arbitrary-Python MCP tool. Unknown
objects remain readable but are not silently made write-eligible.

## Transport and lifecycle

MCP is stdio-only. Bridge IPC uses a length-prefixed JSON envelope with a 4 MiB
limit. Linux/macOS prefer a mode-0600 Unix socket; loopback TCP on an
ephemeral port is available as a fallback. Windows uses the same loopback
fallback in this implementation, with the private rendezvous record and HMAC
remaining mandatory. The bridge does not bind a fixed public port.

FreeCAD callbacks are coalesced by event class. The bridge worker never walks
the document directly: it queues requests and schedules `process_pending` on
the FreeCAD Qt main loop. If no scheduler is available, the call fails closed
as `bridge_busy`.
