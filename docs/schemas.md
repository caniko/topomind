# Schemas and versioning

Canonical contracts live under `schemas/` and use additive `1.0` evolution.
The generated manifest records each schema's stable ID and SHA-256. Run:

```sh
cargo run -p topomind -- --validate-schemas
```

The same command refreshes `schemas/generated/manifest.json`. It intentionally
validates contract ownership and required
top-level fields without pretending to be a full JSON Schema evaluator.

The important boundaries are:

* `bridge-dto/1.0` — snapshots, hello, selection/view state, and change
  responses crossing the FreeCAD extension/sidecar boundary.
* `ccir/1.0` — normalized entities, links, identity, diagnostics, omissions,
  and semantic facts.
* `changeset/1.0` — revision-bound preconditions and typed operations.
* `query/1.0` — bounded query scope, predicates, traversal, ordering, and
  continuation tokens.
* `policy/1.0` — signed workspace operation scope and risk ceiling.

References are opaque revision-qualified `fc://` strings. A reference may be
returned as `exact`, `mapped_exact`, `mapped_heuristic`, `ambiguous`,
`deleted`, or `stale`; only exact and mapped-exact candidates can authorize a
write, and every write still checks the current revision.
