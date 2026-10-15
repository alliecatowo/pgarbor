# Amendment request pgarbor-types-02: `PgError.context` field (CONTEXT error field)

Status: PROPOSED (architect, pgarbor-types). The v1 skeleton omits CONTEXT.

## Problem

Several PostgreSQL errors carry a CONTEXT line that clients display and differential tests may compare.
- JSON input errors: `CONTEXT:  JSON data, line 1: {"a":}`.
- PL/pgSQL errors and SQL-function errors.
- COPY errors: `COPY t, line 3, column a: "x"`.

`pgarbor_common::PgError` has no `context` field. The wire protocol field is `W` (Where).

## Proposed change

Add `pub context: Option<String>` to `PgError` (frozen common crate), with a `with_context(..)` builder.
The wire layer sends it as field `W`, and the oracle comparator ignores it unless a case opts in.

## Affected crates

pgarbor-common (struct field + builder), pgarbor-wire (emit `W`), pgarbor-types (json::parse errors),
pgarbor-exec (COPY, function-call contexts). Adding a field breaks every struct-literal construction of
`PgError`. All current code uses `PgError::new`, so the change is source-compatible in practice.
