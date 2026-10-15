//! Sub-plan evaluation (lease EXE-09, sonnet). PG references: `executor/nodeSubplan.c`
//! (`ExecSubPlan`, `ExecScanSubPlan`, `ExecHashSubPlan`), plan.rs `SubPlanDef` docs.
//!
//! `eval_subplan(ex, sp, row)` evaluates `ExprKind::SubPlan(sp)` for the evaluating node's input
//! `row`. The runtime form `ex.rt.subplans[sp.plan_id]` (a `SubPlanRt` built by `build::open_plan`)
//! holds the operator tree (take it out while driving it, see the runtime module docs).
//!
//! ## Parameters and caching
//! * `sp.args` are evaluated over `row` (correlation) and pushed as ONE `OuterRef` frame with
//!   `Executor::with_outer` around the whole probe; then `op.rescan(ex, &args)` and `next()` calls.
//! * Uncorrelated sub-plans (`SubPlanRt::is_initplan`): run once, store the result in
//!   `SubPlanRt::cache` (`SubPlanCache`) and reuse it for later probes of the same execution.
//!   Volatile uncorrelated sub-plans (`volatility == Volatile`) are re-run for every probe.
//!
//! ## Result per kind (PG semantics)
//! * `Exists`: TRUE iff the sub-plan yields at least one row (stop after the first row); never NULL.
//! * `Scalar`: the first column of the single row; no rows -> NULL; a second row ->
//!   `error::scalar_subquery_multiple_rows()` (21000).
//! * `Array`: `Datum::Array` of the first column of all rows (empty -> empty array `{}`), element type
//!   from `SubPlanRt::out[0].ty`; NULL elements kept.
//! * `Row`: the single row as `Datum::Record` (no rows -> NULL; more than one row -> 21000).
//! * `Any` (`test op ANY (subquery)`, also `IN`): evaluate the left operand `sp.test` (a `Row` for
//!   multi-column tests) over `row`; for every sub-plan row compute the AND over columns of
//!   `ops[i](test_i, col_i)` (three-valued; column operators from `sp.ops`); result: TRUE on the
//!   first TRUE row; else NULL if any row result or the test value produced NULL; else FALSE. No
//!   rows -> FALSE regardless of the test value. With `sp.use_hash` (uncorrelated single-column
//!   equality IN): build once a `HashSet` of `keycodec::encode_key` bytes of the non-NULL sub-plan
//!   values (`SubPlanCache::Hashed`, remember "set contained NULL"); probe: NULL test value -> NULL if
//!   the set is non-empty, FALSE if empty; found -> TRUE; not found -> NULL if the set contained a
//!   NULL else FALSE. (The test value and set values are coerced to the same type by the binder, so
//!   equal values have equal keys.)
//! * `All` (`test op ALL (subquery)`): TRUE for no rows; FALSE on the first FALSE row; NULL if no FALSE
//!   but a NULL occurred; else TRUE.
//! A sub-plan that errors aborts the statement; the frame stack and the taken-out operator are
//! restored before returning the error.
use pgarbor_common::{PgError, Result};
use pgarbor_sql::ir::SubPlanRef;
use pgarbor_types::Datum;

use crate::runtime::Executor;

/// Evaluate a sub-plan reference (module docs).
#[allow(unused_variables)]
pub fn eval_subplan(ex: &mut Executor, sp: &SubPlanRef, row: &[Datum]) -> Result<Datum> {
    Err(PgError::unimplemented("EXE-09", "sub-plan evaluation (subplan::eval_subplan)"))
}
