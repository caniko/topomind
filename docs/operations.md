# Safe operations

The Rust ChangeSet model and native Rust FreeCAD executor share the operation vocabulary in
`schemas/changeset/operation.schema.json`. Current operations cover declared
properties and expressions, sketch datums, common primitives and booleans,
visibility, selection, view actions, allowlisted object creation, dependent
deletion, and undo/redo.

`set_property` accepts only a simple declared property name. Quantity values
carry a numeric value and unit; non-finite numbers and dynamic property paths
are rejected. Primitive types, object types, boolean kinds, view actions, and
selection modes are allowlisted. Every model target must be exact or
mapped-exact at the reviewed revision.

`NeverWrite` denies preview itself. `ApproveEach` requires an HMAC approval
token for every model ChangeSet. `ApproveHighRisk` permits low-risk typed
changes after a valid preview and requires approval for medium/high risk.
`WorkspacePolicy` requires a signed JSON policy with an explicit document
prefix, operation allowlist, and maximum risk. `Developer` removes approval
prompts but preserves revision, precondition, validation, audit, and
idempotency guards.

Undo and redo are high-risk navigation operations. They use the same signed
workspace scope, capability check, approval-token binding, idempotency, and
audit path as model ChangeSets; the approval binding uses the navigation
intent hash because the bridge performs the native transaction directly.
