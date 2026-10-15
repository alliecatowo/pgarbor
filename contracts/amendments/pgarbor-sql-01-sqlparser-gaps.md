# Amendment request pgarbor-sql-01: sqlparser 0.63 gaps and how pgarbor-sql routes around them

Status: ACCEPTED (lead-engineer decision, 2026-10-15).
the conditions under which the dependency should be forked or upgraded.

## Problem

`sqlparser` 0.63.0 (dialect `PostgreSqlDialect`) cannot represent or parse a set of PostgreSQL constructs that
real clients send:

| construct | sqlparser behavior | PGArbor handling |
|---|---|---|
| `REFRESH MATERIALIZED VIEW`, `REINDEX`, `CLUSTER`, `CHECKPOINT`, `SET CONSTRAINTS`, `DO`, `DROP OWNED`, `REASSIGN OWNED`, `ALTER SEQUENCE`, `ALTER DOMAIN`, `ALTER DATABASE`, `ALTER MATERIALIZED VIEW` | parse error "Expected: an SQL statement" | hand parser `parser_extra.rs` + `extra_stmt.rs` (lease SQL-02) |
| `CREATE RULE/CAST/TABLESPACE/PUBLICATION/...`, `ALTER SYSTEM`, `ALTER DEFAULT PRIVILEGES`, `SECURITY LABEL`, `PREPARE TRANSACTION` | parse error | recognized by `parser_extra::is_extra_statement`, bound as `ExtraStatement::Unsupported` -> 0A000 |
| `ON CONFLICT (expr) WHERE pred` (inference with expressions/predicate) | `ConflictTarget::Columns(Vec<Ident>)` only | gap: 42601 / 0A000; documented in `binder/on_conflict.rs` |
| `UPDATE t SET a[1] = x`, `SET c.f = x` | `AssignmentTarget` has no indirection | gap: 0A000 |
| `CREATE TABLE ... LIKE`, `EXCLUDE` constraints, `INHERITS`, `PARTITION` | partly parsed | bound as 0A000 |
| `GROUPING SETS` inside `GROUP BY` mixes, `WITHIN GROUP`, `FILTER`, window `EXCLUDE` | mostly parsed; frame `EXCLUDE` may be missing | 0A000 where absent |
| statements of other databases (MySQL `SHOW TABLES`, Snowflake stages, T-SQL control flow) | parsed into dedicated `Statement` variants | `classify.rs` -> PostgreSQL's own 42601 |

## Proposed change

1. Keep `sqlparser = "=0.63.0"` pinned (root `Cargo.toml`). `classify.rs` has no wildcard arm: an upgrade fails
   the build until every new variant is classified.
2. If a later milestone needs the gaps above (ORMs: Prisma uses `ON CONFLICT` with column lists only;
   Drizzle `ON CONFLICT (cols)`; both fine today), vendor a patched sqlparser under `third_party/` via an
   amendment to the workspace `Cargo.toml` `[patch]` section. Not needed for the north-star slice.

## Affected crates

pgarbor-sql (parser_extra, classify, binder), workspace Cargo.toml (future).

## Steward decision (2026-10-15)

Keep sqlparser pinned at 0.63.0 and the explicit extras parser/classification. Unsupported constructs remain leased 0A000; no dependency fork or grammar-fidelity claim is approved.

Assembly follow-up (2026-10-15): the parser adapter now reads CREATE SEQUENCE option
lists in arbitrary PG order, retaining duplicate options for binder rejection. It folds
signed CONNECTION LIMIT number tokens for sqlparser's value-only role option parser,
retaining original source locations. Binder unit fixtures use the production adapter
rather than bypassing it with raw sqlparser. SQL case text and oracle files are unchanged.
