//! UNION / INTERSECT / EXCEPT (SQL-23). PG references: `analyze.c` (`transformSetOperationStmt`,
//! `transformSetOperationTree`, `determineRecursiveColTypes`, `coerce_setop_columns`/
//! `select_common_type`/`select_common_typmod` use), `parse_coerce.c`.
//!
//! `bind_set_operation(b, op, quantifier, left, right, qctx)`:
//! 1. Bind both sides as independent queries (`select::bind_query` on a synthesized `Query`, or the
//!    block binder for `SetExpr::Select`); the sides see the OUTER scopes but not each other;
//!    parenthesized sides keep their own ORDER BY/LIMIT.
//! 2. Column counts must match: 42601 `each UNION query must have the same number of columns`
//!    (`INTERSECT` / `EXCEPT` likewise; `diag::setop_column_count`), position = the first column of
//!    the offending side.
//! 3. Per column: `coerce::coerce_to_common([l_i, r_i], "UNION"|"INTERSECT"|"EXCEPT")`
//!    (`UNION types integer and text cannot be matched`, 42804, `diag::setop_type_mismatch`);
//!    all-unknown columns resolve to text. Insert casts into each child's output: for a SELECT body
//!    wrap the target expression; for VALUES coerce the cells; for a nested set operation re-type its
//!    `types` and cast through an enclosing projection (build a `Subquery` SELECT over it if needed).
//!    Typmods: `select_common_typmod` keeps a typmod only when both sides agree.
//! 4. Result: `BoundQuery { body: SetOp(SetOpBody { op, all: quantifier == All, left, right, types }),
//!    output: one OutputColumn per common column named after the LEFT child's names (origin None),
//!    ctes: [], order_by: [] , limit: None }` — the caller (`select::bind_query`) attaches ORDER BY /
//!    LIMIT of the enclosing query.
//! 5. Quantifiers: `ALL` -> all = true; `DISTINCT`/none -> false; `BY NAME`/`ALL BY NAME` (DuckDB) ->
//!    42601. Operators: `Union`, `Intersect`, `Except`; `Minus` -> 42601.
//! 6. Recursive CTE support: `cte::bind_with` calls [`bind_recursive_union`] with the non-recursive
//!    term already typed; the recursive term's columns are coerced to the non-recursive term's types
//!    (not the other way): mismatch -> 42804 `recursive query "x" column 1 has type integer in
//!    non-recursive term but type numeric overall` HINT `Cast the output of the non-recursive term to
//!    the correct type.`
use pgarbor_common::{PgError, Result};
use sqlparser::ast::{SetExpr, SetOperator, SetQuantifier};

use super::context::Binder;
use super::select::QueryCtx;
use crate::ir::BoundQuery;

/// Bind `left <op> right` (module docs).
#[allow(unused_variables)]
pub fn bind_set_operation(
    b: &mut Binder,
    op: &SetOperator,
    quantifier: &SetQuantifier,
    left: &SetExpr,
    right: &SetExpr,
    qctx: &QueryCtx,
) -> Result<BoundQuery> {
    Err(PgError::unimplemented("SQL-23", "set operation binding (setop::bind_set_operation)"))
}

/// Combine an already bound non-recursive term with the recursive term for `WITH RECURSIVE`
/// (module docs item 6). `cte_name` is used in the error messages.
#[allow(unused_variables)]
pub fn bind_recursive_union(
    b: &mut Binder,
    cte_name: &str,
    non_recursive: BoundQuery,
    recursive: BoundQuery,
    all: bool,
) -> Result<BoundQuery> {
    Err(PgError::unimplemented("SQL-23", "recursive union typing (setop::bind_recursive_union)"))
}
