# pgarbor-common amendment: PgError context fields

- `PgError` gains `context: Option<String>` (CONTEXT lines, newline separated, innermost first),
  `internal_query: Option<String>` and `internal_position: Option<u32>` (1-based character position in
  `internal_query`).
- `with_context_line` appends one CONTEXT line; `position` and `internal_position` are mutually exclusive when
  an error is raised inside a function body.
- Wire and embedded transports report the fields (`W`, `q`, `p` ErrorResponse fields).
