# pgarbor-catalog-06: validate recursive type metadata before snapshot publication

Status: ACCEPTED (2026-10-15, lead engineering safety review).

Decision: `CatalogSnapshot::build` validates every user type's metadata-resolution
path before publishing the snapshot. Missing base types, repeated OIDs on a path,
and paths exceeding 64 user metadata nodes return XX001. `load` uses the same
validation after decoding rows. Domain base edges and implicit array element
edges both count, since resolving an array of a domain resolves its element.
Builtin types, enum types and relation rowtypes terminate metadata resolution.

The bound contains recursive `CatalogEnv` consumers on ordinary thread stacks.
It is a conservative compatibility limit rather than PostgreSQL's configurable
stack-depth behavior. Snapshot validation remains iterative, with O(64*n) time
and bounded auxiliary memory. No model layout or public signature changes.

Regressions cover direct construction and encoded catalog rows: self-cycles,
mutual cycles, domain/array cycles, dangling bases, valid chains and excessive
chains. A domain referring to its own array is invalid even though its direct
base OID differs from its own OID.

Affected contract: pgarbor-catalog key invariant 6 and CAT-02 validation.
