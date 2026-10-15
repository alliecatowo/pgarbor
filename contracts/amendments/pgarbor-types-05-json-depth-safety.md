# pgarbor-types-05: bound parsed JSON trees for safe consumption

Status: ACCEPTED (2026-10-15, lead engineering safety review).

Decision: replace the parser's 6400-container limit with 128 open containers for
`validate_json`, `parse_jsonb`, and `parse_json_value`; use the same conservative
limit for JSON set-returning traversal. Exceeding the limit returns 54001,
`stack depth limit exceeded`, before constructing a deeper tree. This intentionally
accepts less nesting than some PostgreSQL stack configurations, and is a known
compatibility limit; no oracle expectations are changed.

The old iterative parser admitted trees that overflowed recursive formatters,
comparison, cloning and destruction on the wire worker's 2 MiB thread stack.
Protecting only the formatter would leave abort paths. A subprocess regression
exercises both arrays and objects through parse, output, comparison, clone and
drop on a 2 MiB thread, plus rejection at depth 129 and the reported depth 6000.
Internal code creating deeper trees must apply the same resource bound; this
amendment does not claim arbitrary manually constructed Rust trees are safe.

Affected contract: pgarbor-types JSON input and traversal (TYP-32/TYP-36).
No public signatures or stored formats change.
