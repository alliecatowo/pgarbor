# Amendment request pgarbor-sql-04: ALTER INDEX / ALTER VIEW option forms, ALTER INDEX TABLESPACE, ALTER TYPE SET SCHEMA

Status: PROPOSED (implemented in pgarbor-sql, pgarbor-catalog; pending review).

## Problem

sqlparser 0.63 represents only `ALTER INDEX i RENAME TO j`, `ALTER VIEW v AS query` and the
RENAME/ADD VALUE/RENAME VALUE forms of `ALTER TYPE`. PostgreSQL 18.6 also accepts
`ALTER INDEX i SET (..) | RESET (..) | SET TABLESPACE t`, `ALTER INDEX ALL IN TABLESPACE t SET
TABLESPACE u`, `ALTER VIEW v SET (..) | RESET (..)` and `ALTER TYPE t SET SCHEMA s`. These failed with
42601 `syntax error at or near "SET"`, which is wrong for PostgreSQL 18.6. The catalog and executor
already implemented the option and type work (`alter_index_options`, `set_view_options`,
`set_type_schema`).

## Proposed change

1. `ExtraStatement` gains four variants (`crates/pgarbor-sql/src/extra_stmt.rs`):
   - `AlterIndex { if_exists, name, actions: Vec<RelOptionAction> }`
   - `AlterIndexAllInTablespace { from: Ident, to: Ident }`
   - `AlterView { if_exists, name, actions: Vec<RelOptionAction> }`
   - `AlterTypeSetSchema { name: ObjectName, schema: Ident }`
2. New enum `RelOptionAction { SetOptions(Vec<String>), ResetOptions(Vec<String>), SetTablespace(Ident) }`
   holds one comma-separated subcommand. Option values are kept as text (`name=value`, bare names as
   `name`), following PG `def_arg`.
3. `parser_extra` routes these forms by the token after the (possibly qualified) name, because the
   fixed-prefix table cannot see past a qualified name. RENAME and the other forms still go to sqlparser.
4. Binder: `ddl_alter::bind_alter_index_options`, `bind_alter_index_all_tablespace`,
   `bind_alter_view_options`, and `ddl_alter_other::bind_alter_type_set_schema`. Subcommands fold into
   one `AlterAction::IndexOptions` or `ViewOptions` call (catalog applies resets before sets). A RESET
   after a SET in the same statement is 0A000. Only `pg_default` is a usable index tablespace.
   `pg_global` is 22023, and any other name is 42704.
5. `ddl_view::check_alter_view_check_option`: `SET (check_option = local|cascaded)` on a view that is
   not automatically updatable is 0A000 `WITH CHECK OPTION is supported only on automatically updatable
   views`. The stored definition is re-bound as FROM expansion does.
6. Catalog (`pgarbor-catalog/src/ddl/index.rs`, `alter_index_options`): RESET names are not validated.
   PG drops them as given, so `RESET (fillfactor, no_such_option)` is accepted. SET validation is unchanged.

## Affected crates

- pgarbor-sql: `extra_stmt.rs`, `parser_extra.rs`, `binder/dispatch.rs`, `binder/ddl_alter.rs`,
  `binder/ddl_alter_other.rs`, `binder/ddl_view.rs`.
- pgarbor-catalog: `ddl/index.rs` (RESET validation only).

## Not in this amendment

- `ALTER VIEW` forms other than SET/RESET (RENAME, OWNER, column defaults) stay syntax errors, as before.
- `ALTER VIEW ... RESET` of unknown names still returns 22023 in the catalog. PG ignores them.
  This is left for a separate change.
- `INSERT` into a view is still 0A000, so `WITH CHECK OPTION` takes effect only through the
  updatable-view DML work.
