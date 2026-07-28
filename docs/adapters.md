# Adapter and recognizer contract

The addon registry describes each workbench adapter with an object-type
pattern, extractor version, transaction-safety declaration, operations, and
validators. Adapters enrich the DTO; they do not grant capabilities and cannot
execute arbitrary code.

Core registrations cover document objects, Part/Part Design, and Sketcher.
Unknown third-party objects are preserved through generic type IDs, declared
properties where readable, dependencies, bounds, and final shape summaries.

A recognizer returns a `SemanticFact` only when it can list its evidence,
algorithm version, exactness, tolerance, confidence, and explanation. The
hole recognizer is intentionally conservative: an interior cylindrical face
can produce a through/blind/counterbore candidate, but the result is an
inferred semantic fact rather than a claim that an editable feature exists.
Imported B-Rep remains imported B-Rep.

To add an adapter:

1. Add its namespaced registration and extraction version.
2. Add a DTO/CCIR fixture and golden assertions.
3. Declare every supported operation and validator.
4. Add preview, rollback, commit, stale-reference, validation, undo, and
   idempotency tests before exposing the capability.
