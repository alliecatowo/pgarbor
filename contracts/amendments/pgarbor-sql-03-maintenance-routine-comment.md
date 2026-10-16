# Amendment request pgarbor-sql-03: maintenance option lists, COMMENT on routines, MERGE RETURNING WITH

Status: PROPOSED (implemented in pgarbor-sql; pending review).

## Problem

sqlparser 0.63 has no option list for `VACUUM` / `ANALYZE` and no argument list for `COMMENT ON FUNCTION`,
and it has no `ROUTINE` object kind for COMMENT. `MERGE ... RETURNING WITH (OLD AS o, NEW AS n)` has no alias
slot. All of these failed with 42601 `syntax error at or near "("`, which is wrong for PostgreSQL 18.6.

## Proposed change

1. `extra_stmt::ExtraStatement` gains two variants (the enum is `CONTRACT`):
   * `Maintenance { vacuum, options: Vec<MaintenanceOption>, table, columns }`: `ANALYZE (opts) [VERBOSE] [t [(cols)]]`
     and `VACUUM (opts) | FULL|FREEZE|VERBOSE|ANALYZE ... [t [(cols)]]`.
   * `CommentRoutine { kind: RoutineKind, name, args: Option<Vec<OperateFunctionArg>>, comment }`:
     `COMMENT ON {FUNCTION|PROCEDURE|ROUTINE} name [(args)] IS {'text'|NULL}`.
   New types `MaintenanceOption` and `RoutineKind` are added beside it.
2. `parser_extra` routes these heads (`COMMENT ON FUNCTION|PROCEDURE|ROUTINE`, and the `VACUUM`/`ANALYZE` forms
   with `(` or a legacy keyword). `MERGE` chunks containing `RETURNING WITH (` become
   `Unsupported("MERGE ... RETURNING WITH (OLD/NEW aliases)")` (0A000). Lease: SQL-02.
3. Binder semantics:
   * `VACUUM` / `ANALYZE` options are checked as PG `ExecVacuum` checks them (names 42601, `BUFFER_USAGE_LIMIT`
     22023 with the 128 kB .. 16 GB range, boolean values 42601, column list without ANALYZE 0A000).
   * `ANALYZE` statistics collection (VACUUM ANALYZE), `VERBOSE` output and `SKIP_LOCKED` are 0A000 with lease
     EXE-39, since PGArbor keeps no statistics and emits no INFO output.
   * COMMENT on a routine with an argument list resolves by input argument types only (OUT, names and defaults
     ignored), 42883 when nothing matches, 42725 when ambiguous without a list, 42809 for the wrong kind. Lease
     for the binder: SQL-38.

## Affected crates

pgarbor-sql only (`extra_stmt.rs`, `parser_extra.rs`, `binder/utility.rs`, `binder/ddl_drop.rs`,
`binder/dispatch.rs`). No change to catalog, exec or wire.
