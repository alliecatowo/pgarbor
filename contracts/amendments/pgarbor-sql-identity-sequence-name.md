# Amendment request: identity `SEQUENCE NAME` and order-free identity option lists

Status: APPLIED (2026-10-15).

## Problem

PG 18 `gram.y` accepts `SEQUENCE NAME any_name` (a `SeqOptElem`) inside `GENERATED ... AS IDENTITY (...)`
and in `ALTER TABLE ... ALTER COLUMN ... ADD GENERATED ... (...)`, in any order. PGArbor rejected it with
42601 (drizzle's migrations use it). The sqlparser 0.63 identity parser is fixed-order and has no name.

## Change

- `pgarbor-sql/src/identity_opts.rs` (new): before sqlparser parses a chunk, every identity option list is
  removed from the token stream and parsed (any order, `SEQUENCE NAME`). One `IdentityGroup` per
  `AS IDENTITY`, in source order, goes into `RawStatement::identity_groups`.
- `RawStatement` gains `identity_groups: Vec<IdentityGroup>`.
- `Binder` gains `identity_groups: VecDeque<IdentityGroup>`, filled by `bind_statement`; identity columns
  consume it in order (`ddl_table::identity_option_list`). Falls back to the sqlparser list when empty.
- `ddl::spec::IdentitySpec` gains `sequence_name: Option<String>`. `None` keeps the PG-chosen name.
  An explicit name must be unused in the namespace (42P07).

## Not implemented

- A schema-qualified `SEQUENCE NAME s.n` returns 0A000 (`SQL-30`).
- `SEQUENCE NAME ... AS type` inside identity lists is a 42601 syntax error for now.
