//! INSERT (SQL-26). PG references: `analyze.c` (`transformInsertStmt`, `transformInsertRow`,
//! `checkInsertTargets`, `transformOnConflictClause` caller), `parse_target.c`
//! (`transformInsertRow`'s `transformAssignedExpr`), `rewriteHandler.c` (`rewriteTargetListIU`:
//! DEFAULT, identity and generated column rules), `parse_clause.c` (`transformOnConflictArbiter`).
//!
//! `bind_insert(b, ins)` (sqlparser `Insert`):
//! 1. Reject non-PostgreSQL options (42601/0A000 naming them): `or` (SQLite), `ignore`,
//!    `overwrite`, `partitioned`, `after_columns`, `priority`, `insert_alias`, `replace_into`,
//!    `output`, `settings`, `format_clause`, `multi_table_*`, `OnInsert::DuplicateKeyUpdate`,
//!    `TableObject::TableFunction`, `assignments` (MySQL `INSERT .. SET`).
//! 2. Push `ScopeKind::Insert`; open the target (`target_table::open_target_table`, `DmlKind::Insert`).
//!    Column list: each name must be a live column (42703 `column "x" of relation "t" does not exist`,
//!    `diag::column_of_relation_missing`); duplicates 42701 `column "x" specified more than once`
//!    (`column_specified_twice`); `INSERT INTO t(a.b)` subscripts/fields unsupported (0A000).
//!    No list -> all non-dropped columns in attnum order (the implicit list EXCLUDES stored generated
//!    columns - oracle: `insert into g values (1)` works for `g(a int, b int generated always as (a*2)
//!    stored)`, while `values (1,2)` fails with 428C9).
//! 3. Source:
//!    * `source: None` or `DEFAULT VALUES` -> `InsertSource::DefaultValues` (`columns` empty).
//!    * `SetExpr::Values(v)` -> `values::bind_values_body(b, v, Some(targets))` (the `DEFAULT` keyword
//!      allowed per cell). Cell count mismatch: 42601 `INSERT has more expressions than target columns`
//!      (position = first surplus cell) / `INSERT has more target columns than expressions`.
//!    * any other query -> `select::bind_query` with `QueryCtx { keep_unknown_outputs: true, .. }` while the
//!      INSERT target is hidden from the sub-select (`RangeEntry::visible = false`). Then the width check
//!      (`insert_too_many_expressions` / `insert_too_many_columns`), then per-column assignment
//!      coercion of the query's output: `unknown`-typed literal outputs are converted directly to the
//!      target column type (`insert into t(int_col) select 'x'` -> 22P02 at the literal, as PG);
//!      otherwise, when an output type differs from the column type, wrap the query in a coercing
//!      projection (a `BoundQuery` whose body is a SELECT over one `RangeKind::Subquery` entry with
//!      assignment-cast target expressions), as the `InsertSource::Query` IR doc promises.
//! 4. Per-column rules (PG `rewriteTargetListIU`), applied to the bound cells for VALUES sources and
//!    enforced by the executor for query sources through `TargetColumn` metadata:
//!    * identity `GENERATED ALWAYS`: a non-DEFAULT cell is 428C9 `cannot insert a non-DEFAULT value into
//!      column "id"` DETAIL `Column "id" is an identity column defined as GENERATED ALWAYS.` HINT `Use
//!      OVERRIDING SYSTEM VALUE to override.` unless `OVERRIDING SYSTEM VALUE`; with `OVERRIDING USER
//!      VALUE` the supplied value is ignored (default used);
//!    * stored generated column: any non-DEFAULT cell is 428C9 `cannot insert a non-DEFAULT value into
//!      column "g"` DETAIL `Column "g" is a generated column.`;
//!    * `DEFAULT` cell -> `SetToDefault{attnum}` (planner substitutes the default expression).
//!    For a query source the check applies to the column list (columns named in the list that are
//!    GENERATED ALWAYS identity or generated: same 428C9 error unless overriding).
//! 5. `ON CONFLICT`: `on_conflict::bind_on_conflict` (needs the target and column list).
//! 6. `RETURNING`: `update_delete::bind_returning` (columns of the target, plus `excluded` is NOT visible).
//! 7. Pop the scope; result `BoundStatement::Insert(Box<BoundInsert { target, range_table, ctes,
//!    columns, source, overriding, on_conflict, returning }>)`. `WITH` on the INSERT (`ins.source`'s
//!    own `with`, or the statement-level WITH in sqlparser's `Query` wrapping): `cte::bind_with` with
//!    `top_level = true`; the CTEs go to `BoundInsert::ctes`.
//! Privileges: `access.insert_cols` = target columns written; `select` for ON CONFLICT/RETURNING refs.
use pgarbor_common::{PgError, Result};
use sqlparser::ast::Insert;

use super::context::Binder;
use crate::ir::BoundStatement;

/// Bind an INSERT statement (module docs).
#[allow(unused_variables)]
pub fn bind_insert(b: &mut Binder, ins: &Insert) -> Result<BoundStatement> {
    Err(PgError::unimplemented("SQL-26", "INSERT binding (insert::bind_insert)"))
}
