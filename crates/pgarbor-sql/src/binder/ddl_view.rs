//! CREATE [OR REPLACE] [MATERIALIZED] VIEW and REFRESH MATERIALIZED VIEW (SQL-36, sonnet).
//! PG references: `view.c` (`DefineView`, `DefineVirtualRelation`, `checkViewColumns`), `matview.c`
//! (`ExecCreateTableAs` for matviews, `ExecRefreshMatView`), `ruleutils.c` (`pg_get_viewdef` format),
//! `parse_utilcmd.c` (`transformCreateTableAsStmt`).
//!
//! `bind_create_view(b, stmt)` (`Statement::CreateView { or_replace, materialized, name, columns,
//! query, options, with_no_schema_binding, if_not_exists, temporary, to, params, .. }`):
//! * name/namespace as CREATE TABLE (`creation_namespace_for`); `TEMP` views 0A000; `WITH
//!   NO SCHEMA BINDING`, `TO` (ClickHouse), `ON CLUSTER`, `cluster_by`, `secure`: 42601.
//! * The query is bound with `select::bind_query` (top_level false): parameters in a view are an error
//!   (42P02 `there is no parameter $1`, oracle); unknown-typed output columns resolve to `text`
//!   (PG 10+); column names = the optional column list (`CREATE VIEW v(a,b)`: more names than columns
//!   42601 `CREATE VIEW specifies more column names than columns`; FEWER names than columns is accepted and
//!   the remaining columns keep their query names - oracle) else the query's output names;
//!   duplicate output names: 42701 `column "x" specified more than once`; a view must have >= 1 column.
//! * `ViewSpec.columns` = `ColumnSpec::new(name, ty)` for each output (collation from the query output
//!   type's default), `ViewSpec.query` = `StoredExpr { sql: deparse::query::deparse_viewdef(&bound),
//!   depends_on: <relations, columns, functions, types referenced> }` in `pg_get_viewdef` format
//!   (` SELECT a,\n    b\n   FROM t;` — see deparse docs), `or_replace`, `materialized`, `populated =
//!   !WITH NO DATA`, `options` from `WITH (security_barrier=true, check_option=..)`, `check_option` from
//!   `WITH [LOCAL|CASCADED] CHECK OPTION` (not valid on non-updatable views: 0A000).
//! * `OR REPLACE` compatibility with the existing view is checked by the CATALOG (42P16 `cannot change
//!   name of view column "a" to "b"` ...).
//! * Dependencies: every `RangeKind::Relation` OID, referenced columns (`Column` nodes mapped through
//!   range entries; `whole-row` references depend on the relation), user functions, user types; views
//!   referencing views depend on the inner view (not its base tables).
//!
//! `bind_refresh_matview(b, name, concurrently, with_data)` -> `RefreshMatView { view, with_data,
//! concurrently }`: relation must be a matview (42809 `"v" is not a materialized view`);
//! `CONCURRENTLY` requires a unique index (55000 `cannot refresh materialized view "public.v"
//! concurrently` HINT `Create a unique index with no WHERE clause on one or more columns of the
//! materialized view.`) — checked by the executor; `WITH NO DATA` + CONCURRENTLY 0A000.
use pgarbor_common::{PgError, Result};
use sqlparser::ast::{ObjectName, Statement};

use super::context::Binder;
use crate::ir::BoundStatement;

/// `Statement::CreateView`.
#[allow(unused_variables)]
pub fn bind_create_view(b: &mut Binder, stmt: &Statement) -> Result<BoundStatement> {
    Err(PgError::unimplemented("SQL-36", "CREATE VIEW binding (ddl_view::bind_create_view)"))
}

/// `REFRESH MATERIALIZED VIEW`.
#[allow(unused_variables)]
pub fn bind_refresh_matview(b: &mut Binder, name: &ObjectName, concurrently: bool, with_data: bool) -> Result<BoundStatement> {
    Err(PgError::unimplemented("SQL-36", "REFRESH MATERIALIZED VIEW binding (ddl_view::bind_refresh_matview)"))
}
