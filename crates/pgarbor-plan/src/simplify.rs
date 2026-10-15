//! Expression simplification: constant folding, boolean normalization, qual splitting (PLN-02).
//! PG reference: `optimizer/util/clauses.c` (`eval_const_expressions`, `simplify_function`,
//! `simplify_op_expr`, `simplify_boolean_equality`, `negate_clause`, `make_ands_explicit`,
//! `make_ands_implicit`), `prepqual.c` (`canonicalize_qual`, `find_duplicate_ors`),
//! `optimizer/util/predtest.c` is out of scope.
//!
//! ## `simplify_expr(st, e)` rules (bottom-up; semantics preserving; evaluation errors propagate):
//! 1. **Folding**: a `FuncCall`/`Op`/`Cast`/`Coalesce`/`NullIf`/`MinMax`/`Case`/`Array`/`Row`/`FieldSelect`/
//!    `Subscript`/`NullTest`/`BooleanTest`/`DistinctFrom`/`ScalarArrayOp` node whose arguments are ALL
//!    `Const` and whose volatility is `Immutable` is replaced by the `Const` it evaluates to
//!    (`pgarbor_types::func::implementation(oid)?.call(..)` with a `FuncCtx` built from
//!    `TypeEnv`=`st.ctx.env`, default `IoConfig`, and a `TestSession`; casts via `cast::apply_cast`).
//!    Errors during folding (`1/0` -> 22012) are returned as-is (PG does the same at plan time).
//!    `Stable` and `Volatile` functions are NOT folded (`now()` stays; PG folds stable functions only
//!    for estimation). Strict functions with a NULL constant argument fold to NULL without calling.
//!    Never fold set-returning functions or anything containing `Param`/`Column`/`OuterRef`/sub-plans.
//!    `Param(n)` nodes become `Const` only when `st.ctx.param_values` is `Some` (custom plan).
//! 2. **Boolean**: flatten nested AND/OR; drop `true` conjuncts / `false` disjuncts; `x AND false` ->
//!    `false`; `x OR true` -> `true`; AND/OR with one remaining operand -> that operand; `NOT NOT x` ->
//!    `x`; `NOT (a = b)` -> `a <> b` and the other comparison negations via the operator's `negator`
//!    (PG `negate_clause` does this for btree comparison operators `=,<>,<,<=,>,>=` of the same type); `NOT (x IS NULL)` -> `x IS NOT NULL`;
//!    `Const(NULL)` operands: `NULL AND false` -> false, `NULL AND true` -> NULL, `NULL OR true` -> true.
//!    `CASE WHEN true THEN a ... END` -> `a`; leading `WHEN false` arms removed; `COALESCE(NULL, x)` ->
//!    `x`; `x IS NULL` on a non-nullable constant -> false.
//! 3. **Casts**: `Cast` of `Const` folded; stacked relabels collapsed; identity casts removed.
//!
//! `simplify_expr` must NOT change the result type of any node or reorder side-effecting operands.
//!
//! ## Qual helpers (`split_conjuncts`, `make_and`) are used by pushdown/index/join: fully pure.
use pgarbor_common::{PgError, Result};
use pgarbor_sql::ir::BoundExpr;

use crate::context::PlannerState;

/// Simplify one expression (module docs).
#[allow(unused_variables)]
pub fn simplify_expr(st: &mut PlannerState, e: BoundExpr) -> Result<BoundExpr> {
    Err(PgError::unimplemented("PLN-02", "expression simplification (simplify::simplify_expr)"))
}

/// Simplify a list of AND-ed quals: split top-level ANDs into conjuncts, simplify each, drop constant
/// TRUE conjuncts; a constant FALSE/NULL conjunct collapses the list to the single `false` constant
/// (callers then emit a `Result` with one-time filter false).
#[allow(unused_variables)]
pub fn simplify_quals(st: &mut PlannerState, quals: Vec<BoundExpr>) -> Result<Vec<BoundExpr>> {
    Err(PgError::unimplemented("PLN-02", "qual list simplification (simplify::simplify_quals)"))
}

/// Split an expression into its top-level AND conjuncts (`a AND (b AND c)` -> `[a, b, c]`; any other
/// expression -> one element). Fully implemented.
pub fn split_conjuncts(e: BoundExpr) -> Vec<BoundExpr> {
    use pgarbor_sql::ir::{BoolOp, ExprKind};
    match e.kind {
        ExprKind::Bool(b) if b.op == BoolOp::And => {
            let mut out = Vec::new();
            for a in b.args {
                out.extend(split_conjuncts(a));
            }
            out
        }
        _ => vec![e],
    }
}

/// AND a list back together (`BoundExpr::and_all`), `None` for the empty list.
pub fn make_and(quals: Vec<BoundExpr>) -> Option<BoundExpr> {
    if quals.is_empty() {
        None
    } else {
        Some(BoundExpr::and_all(quals))
    }
}
