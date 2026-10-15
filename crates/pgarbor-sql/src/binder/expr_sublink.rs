//! Sub-select expressions (SQL-15). PG references: `parse_expr.c` (`transformSubLink`),
//! `parse_clause.c`, `parse_oper.c` (`make_scalar_array_op`), `analyze.c` (`transformSelectStmt` for
//! the nested query), `parse_expr.c` (`transformArrayExpr` for `ARRAY(select)`).
//!
//! The sub-query is bound with `select::bind_query` WITHOUT popping the enclosing scopes: it opens a
//! new scope on `b.scopes`, so references to enclosing queries become `Column{levels_up >= 1}`
//! (correlation). Set `has_sublinks` on the enclosing scope. Sub-links are rejected where
//! `!ec.sublinks_ok` (0A000 `cannot use subquery in <clause>`; PG wording for DEFAULT/CHECK:
//! `cannot use subquery in DEFAULT expression` / `cannot use subquery in check constraint`).
//!
//! | SQL | `SubLinkKind` | rules |
//! |---|---|---|
//! | `EXISTS (q)` | `Exists` | `ty` boolean; the query's select list is irrelevant (not validated for type); `NOT EXISTS` -> `Bool(Not)` |
//! | `(q)` scalar | `Scalar` | exactly one output column else 42601 `subquery must return only one column` (`diag::subquery_one_column`); `ty` = that column's type (unknown -> text) |
//! | `x IN (q)` / `x = ANY (q)` / `x <op> ANY\|SOME (q)` | `Any` | `test` = x; per-column operator resolved with `expr_ops::resolve_named_op(op, test_type, column_type)`; IN uses `=`; `NOT IN` -> `Bool(Not)` over `Any(=)`; row tests `(a,b) IN (select x,y)` need matching width: 42601 `subquery has too many columns` / `subquery has too few columns` (`diag::subquery_column_count`) |
//! | `x <op> ALL (q)` | `All` | like ANY |
//! | `ARRAY(q)` | `Array` | one column; `ty` = array of the column type |
//! | `(a,b) = (q)` | `Row` | width must match; comparison expanded like row comparison |
//! The nested `BoundQuery` is stored in `SubLink::subquery`. LATERAL-ish references to the enclosing
//! FROM items of the SAME level are not visible (a sub-select in WHERE sees the enclosing query's
//! full namespace as outer scope, which is what `levels_up` means).
//! `InUnnest { expr, array_expr }` (BigQuery) -> 42601.
use pgarbor_common::{PgError, Result};
use sqlparser::ast::{BinaryOperator, Expr, Query};

use super::context::{Binder, ExprCtx};
use crate::ir::{BoundExpr, BoundQuery};

/// Bind a nested query as an expression sub-link body (opens/closes its scope; result type checks
/// are the callers').
#[allow(unused_variables)]
pub fn bind_nested_query(b: &mut Binder, q: &Query) -> Result<BoundQuery> {
    Err(PgError::unimplemented("SQL-15", "nested query binding (expr_sublink::bind_nested_query)"))
}

/// `Expr::Exists { subquery, negated }`.
#[allow(unused_variables)]
pub fn bind_exists(b: &mut Binder, q: &Query, negated: bool, ec: ExprCtx) -> Result<BoundExpr> {
    Err(PgError::unimplemented("SQL-15", "EXISTS (expr_sublink::bind_exists)"))
}

/// `Expr::Subquery(q)` scalar sub-select.
#[allow(unused_variables)]
pub fn bind_scalar_subquery(b: &mut Binder, q: &Query, ec: ExprCtx) -> Result<BoundExpr> {
    Err(PgError::unimplemented("SQL-15", "scalar subquery (expr_sublink::bind_scalar_subquery)"))
}

/// `Expr::InSubquery { expr, subquery, negated }`.
#[allow(unused_variables)]
pub fn bind_in_subquery(b: &mut Binder, e: &Expr, q: &Query, negated: bool, ec: ExprCtx) -> Result<BoundExpr> {
    Err(PgError::unimplemented("SQL-15", "IN (subquery) (expr_sublink::bind_in_subquery)"))
}

/// `AnyOp`/`AllOp` whose right side is `Expr::Subquery`.
#[allow(unused_variables)]
pub fn bind_any_all_subquery(b: &mut Binder, left: &Expr, op: &BinaryOperator, q: &Query, is_all: bool, op_pos: Option<u32>, ec: ExprCtx) -> Result<BoundExpr> {
    Err(PgError::unimplemented("SQL-15", "ANY/ALL (subquery) (expr_sublink::bind_any_all_subquery)"))
}

/// `ARRAY(select ...)`.
#[allow(unused_variables)]
pub fn bind_array_subquery(b: &mut Binder, q: &Query, ec: ExprCtx) -> Result<BoundExpr> {
    Err(PgError::unimplemented("SQL-15", "ARRAY(subquery) (expr_sublink::bind_array_subquery)"))
}
