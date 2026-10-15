//! Query expressions: `WITH ... SELECT/VALUES/set-operation ... ORDER BY ... LIMIT ...` (SQL-16).
//! PG references: `analyze.c` (`transformSelectStmt`, `transformSetOperationStmt`,
//! `transformValuesClause`, `transformStmt`), `parse_clause.c` (`transformFromClause`,
//! `transformWhereClause`, `transformLimitClause`, `transformLockingClause`), `parse_cte.c`.
//!
//! ## `bind_query(b, q, qctx)` algorithm (sqlparser `Query { with, body, order_by, limit_clause,
//! fetch, locks, .. }`)
//! 1. `b.enter()`. Reject non-PostgreSQL clauses: `for_clause`, `settings`, `format_clause`,
//!    `pipe_operators`, `limit_by`, MySQL `LIMIT a, b` (`OffsetCommaLimit`): 42601 at their position.
//! 2. `WITH`: `cte::bind_with(b, with, qctx.top_level)` -> `ctes` (frame popped at the end).
//! 3. Body (`SetExpr`):
//!    * `Select(sel)` -> [`bind_select_block`] with the query's ORDER BY (it needs the select scope
//!      for junk target entries), result gives `BoundSelect`, `Vec<SortKey>` and output columns.
//!    * `Values(v)` -> `values::bind_values_body` (ORDER BY by output ordinal/name only).
//!    * `SetOperation{..}` -> `setop::bind_set_operation`.
//!    * `Query(q2)` (parenthesized) -> `bind_query(q2)` and combine ORDER BY/LIMIT of the outer
//!      query (inner ORDER BY with outer LIMIT keeps both: wrap by making the inner the body of a
//!      `SetOp`-free passthrough: bind the inner query, then apply the outer ORDER BY by output
//!      column; if the inner already has order/limit and the outer has its own, nest through a
//!      subquery `RangeKind::Subquery` of a synthetic SELECT * — see PG which does the same via
//!      `transformSelectStmt` on a sub-select).
//!    * `Table(t)` (`TABLE name`) -> equivalent to `SELECT * FROM name`.
//!    * `Insert/Update/Delete/Merge` as a query body only occur inside CTEs (handled by `cte`); here
//!      -> 0A000/42601.
//! 4. ORDER BY on VALUES/set-operation bodies: `order::bind_order_by_output`.
//! 5. `LIMIT`/`OFFSET`/`FETCH`: `order::bind_limit_clause` (also decides `limit_with_ties`; `WITH TIES`
//!    requires ORDER BY: 42601 `WITH TIES cannot be specified without ORDER BY clause`).
//! 6. Locking: `FOR UPDATE|NO KEY UPDATE|SHARE|KEY SHARE [OF t,..] [NOWAIT|SKIP LOCKED]` ->
//!    `LockingClause`; errors 0A000 `FOR UPDATE is not allowed with DISTINCT clause` / `GROUP BY` /
//!    `HAVING` / `aggregate functions` / `window functions` / `set-returning functions in the target
//!    list` / `UNION/INTERSECT/EXCEPT` / `VALUES`? (`diag::locking_not_allowed`), `relation "x" in FOR
//!    UPDATE clause not found in FROM clause` (42P01, `diag::locking_relation_missing`).
//! 7. Pop the CTE frame; compute `BoundQuery.output`; `b.leave()`.
//!
//! ## Select block ([`bind_select_block`])
//! Order of work (matches `transformSelectStmt`, which fixes error precedence):
//! 1. Push `QueryScope::new(ScopeKind::Select)` onto `b.scopes`.
//! 2. FROM: `from::bind_from_clause` (builds range table, namespace, join tree).
//! 3. Follow PG's `transformSelectStmt` order exactly (it fixes which error wins when several apply):
//!    FROM -> target list -> WHERE (`expr::bind_boolean(.., ExprCtx::where_(), "WHERE")`) -> HAVING
//!    (`ExprCtx::having()`, "HAVING") -> `window::bind_window_clause` -> `group::bind_group_by` ->
//!    `group::resolve_pending_groupings` -> DISTINCT / DISTINCT ON + ORDER BY via
//!    `order::bind_order_by` (with `order::bind_distinct_on`) -> `group::check_grouping` (the 42803
//!    ungrouped-column check over target list, HAVING and ORDER BY expressions) -> unknown-type
//!    resolution of output entries (`coerce::resolve_unknown_output`, only for non-junk entries and
//!    only when `!qctx.keep_unknown_outputs`; `bind_select_block` therefore takes the flag).
//! 4. `SELECT ... INTO` is handled by [`bind_query_statement`], not here.
//! 5. Unsupported select-level syntax (all 42601 or 0A000 with the clause named): `TOP`,
//!    `PREWHERE`, `CONNECT BY`, `QUALIFY`, `CLUSTER BY`, `DISTRIBUTE BY`, `SORT BY`, `LATERAL VIEW`,
//!    `EXCLUDE`, `SELECT AS STRUCT`, optimizer hints, `select_modifiers`.
//! 6. Pop the scope into `BoundSelect { range_table, from, where_, target_list, group_by, having,
//!    distinct, windows, has_aggs, has_windows, has_target_srfs, has_sublinks }` and return it with
//!    the sort keys and `OutputColumn`s (name/type/origin of non-junk targets).
//!
//! ## Statement level
//! [`bind_query_statement`] wraps `bind_query` in `BoundStatement::Query`, and turns
//! `SELECT ... INTO [TEMP] t` into `BoundStatement::CreateTableAs` via
//! `ddl_table::bind_select_into` (TEMP: 0A000).
use pgarbor_common::{PgError, Result};
use sqlparser::ast::{OrderByExpr, Query, Select};

use super::context::Binder;
use crate::ir::{BoundQuery, BoundSelect, BoundStatement, OutputColumn, SortKey};

/// Options for [`bind_query`].
// CONTRACT
#[derive(Clone, Debug, Default)]
pub struct QueryCtx {
    /// Column alias list applied to the output names (`WITH x(a,b) AS (..)`, `(select ..) s(a,b)`,
    /// `CREATE VIEW v(a,b) AS`): more aliases than columns -> 42P10 (INVALID_COLUMN_REFERENCE) `WITH query "x" has N
    /// columns available but M columns specified` (`diag::too_many_column_aliases`).
    pub column_aliases: Vec<String>,
    /// The outermost statement query (data-modifying CTEs allowed).
    pub top_level: bool,
    /// Leave `unknown`-typed output columns (bare string literals / NULL / parameters) unresolved
    /// instead of coercing them to `text`; the caller coerces them to a known target type
    /// (INSERT ... SELECT, `UPDATE .. SET (a,b) = (SELECT ..)`). Normal queries: false.
    pub keep_unknown_outputs: bool,
}

/// Result of [`bind_select_block`].
// CONTRACT
#[derive(Clone, Debug, PartialEq)]
pub struct SelectBlock {
    pub select: BoundSelect,
    pub order_by: Vec<SortKey>,
    pub output: Vec<OutputColumn>,
}

/// Bind a complete query expression (module docs).
#[allow(unused_variables)]
pub fn bind_query(b: &mut Binder, q: &Query, qctx: &QueryCtx) -> Result<BoundQuery> {
    Err(PgError::unimplemented("SQL-16", "query binding (select::bind_query)"))
}

/// Bind one simple SELECT block together with the ORDER BY that belongs to it (module docs).
#[allow(unused_variables)]
pub fn bind_select_block(b: &mut Binder, sel: &Select, order_by: &[OrderByExpr], keep_unknown_outputs: bool) -> Result<SelectBlock> {
    Err(PgError::unimplemented("SQL-16", "SELECT block binding (select::bind_select_block)"))
}

/// Top-level `Statement::Query`: query or `SELECT INTO`.
#[allow(unused_variables)]
pub fn bind_query_statement(b: &mut Binder, q: &Query) -> Result<BoundStatement> {
    Err(PgError::unimplemented("SQL-16", "query statement binding (select::bind_query_statement)"))
}

/// Bind a query that is used as a sub-select source (derived table, view body, CTE body) with
/// optional column aliases; thin wrapper over [`bind_query`] that sets `top_level = false`.
#[allow(unused_variables)]
pub fn bind_subselect(b: &mut Binder, q: &Query, column_aliases: &[String]) -> Result<BoundQuery> {
    Err(PgError::unimplemented("SQL-16", "sub-select binding (select::bind_subselect)"))
}
