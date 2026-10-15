//! SELECT / RETURNING target lists (SQL-19). PG references: `parse_target.c`
//! (`transformTargetList`, `transformTargetEntry`, `FigureColname`, `FigureColnameInternal`,
//! `ExpandColumnRefStar`, `ExpandAllTables`, `ExpandSingleTable`, `markTargetListOrigin`,
//! `resolveTargetListUnknowns` in `parse_clause`/`parse_target`).
//!
//! `bind_target_list(b, items, ec)` converts `Vec<SelectItem>` into `Vec<TargetEntry>` (never junk;
//! junk entries are added later by `order`/`group`):
//! * `UnnamedExpr(e)` -> `expr::bind_expr(e, ec)`; name from [`figure_colname`].
//! * `ExprWithAlias { expr, alias }` -> alias folded with `Binder::ident` (AS keyword optional).
//! * `Wildcard(opts)` -> `scope::expand_star(None)`; `QualifiedWildcard(name, opts)` ->
//!   `scope::expand_star(Some(name))`, or `expr_misc::expand_composite` when `name` is not a range
//!   entry but a composite column (`(t.c).*` is `Expr::...Wildcard`); options `EXCLUDE`, `EXCEPT`,
//!   `REPLACE`, `RENAME`, `ILIKE` (BigQuery/Snowflake): 42601. Each expansion yields one entry per
//!   visible column with the column's own name and origin.
//! * Set-returning functions in the list set `scope.has_target_srfs`; `SELECT` list items of
//!   composite type are NOT expanded unless written with `.*`.
//!
//! ## Output names ([`figure_colname`], PG `FigureColname`)
//! column ref -> column name (last identifier; `t.c` -> `c`); `t.*`/`*` n/a; function call -> function
//! name (last part; `count(*)` -> `count`); `CAST(x AS t)`/`x::t` -> the operand's name if it has
//! one, else the TYPE name (`select 1::int` -> `int4`; `select 'a'::text` -> `text`); `CASE` ->
//! `case`; `COALESCE` -> `coalesce`; `GREATEST` -> `greatest`; `LEAST` -> `least`; `NULLIF` ->
//! `nullif`; `EXISTS(..)` -> `exists`; `ARRAY[..]` and `ARRAY(select)` -> `array`; `ROW(..)` ->
//! `row`; `(x).field` -> `field`; subscript `a[1]` -> `a` (the root's name); `x IS NULL` and other
//! operators / literals / parameters -> `?column?`; `CURRENT_DATE` & co. -> their keyword lower-case
//! (`current_date`, `current_timestamp`, `current_user`->`current_user`, `session_user`, `user` ->
//! `user`, `current_catalog`, `current_schema`, `localtime`, `localtimestamp`); `EXTRACT(..)` ->
//! `extract`; `SUBSTRING` -> `substring`; `POSITION` -> `position`; `OVERLAY` -> `overlay`; `TRIM` ->
//! `btrim`/`ltrim`/`rtrim`; `x AT TIME ZONE z` -> `timezone`; `COLLATE` -> operand's name; sub-select
//! scalar `(select a)` -> the sub-select's FIRST output column name; `interval '1 day'` ->
//! `interval`; a typed literal `date '2020-01-01'` -> `date`; `GROUPING(..)` -> `grouping`; `x ->>
//! 'a'` -> `?column?`; boolean tests -> `?column?`; `NOT x` -> `?column?`; `TRUE` -> `bool`;
//! numeric/string literals -> `?column?`. Verify each against the oracle with `select <expr>` and
//! `\gdesc`-style column names before relying on a rule.
//!
//! ## Column origin (`markTargetListOrigin`)
//! If the bound expression (after stripping implicit relabel casts) is a `Column{levels_up: 0}`
//! referencing a plain relation range entry, `origin = ColumnOrigin{table: oid, attnum}`; a column of
//! a sub-select/view/CTE range entry resolves recursively to the underlying base column when that
//! output is itself a plain column reference (via [`origin_of`]); anything else `None`.
use pgarbor_common::{PgError, Result};
use sqlparser::ast::{Expr, SelectItem};

use super::context::{Binder, ExprCtx};
use crate::ir::{BoundExpr, ColumnOrigin, TargetEntry};

/// Bind a SELECT list (or RETURNING list when `ec` = `ExprCtx::returning()`).
#[allow(unused_variables)]
pub fn bind_target_list(b: &mut Binder, items: &[SelectItem], ec: ExprCtx) -> Result<Vec<TargetEntry>> {
    Err(PgError::unimplemented("SQL-19", "target list binding (target::bind_target_list)"))
}

/// Default output column name of an expression (table above). Pure over the AST; `Binder` is only
/// needed to fold identifiers. Returns `"?column?"` when the rules give no name.
#[allow(unused_variables)]
pub fn figure_colname(b: &mut Binder, e: &Expr) -> Result<String> {
    Err(PgError::unimplemented("SQL-19", "output column naming (target::figure_colname)"))
}

/// Origin of a bound expression (module docs).
#[allow(unused_variables)]
pub fn origin_of(b: &Binder, e: &BoundExpr) -> Result<Option<ColumnOrigin>> {
    Err(PgError::unimplemented("SQL-19", "column origin lookup (target::origin_of)"))
}
