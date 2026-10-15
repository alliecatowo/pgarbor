//! Sub-links -> sub-plans (PLN-04). PG references: `optimizer/plan/subselect.c`
//! (`SS_process_sublinks`, `make_subplan`, `build_subplan`, `convert_ANY_sublink_to_join`,
//! `convert_EXISTS_sublink_to_join`, `SS_replace_correlation_vars`, `replace_correlation_vars_mutator`,
//! `SS_make_initplan_output_param`, `hash_ok_operator`), `prepjointree.c` (`pull_up_sublinks`).
//!
//! ## `process_sublinks(st, e, parent_ranges)`
//! Walks `e` (bind-time expression of the current query level) and replaces every `SubLink`
//! with an `ExprKind::SubPlan(SubPlanRef)`:
//! 1. Plan the sub-query: `build::build_query(st, &sublink.subquery)` -> `planner::plan_logical` ->
//!    `PlanNode` (nested planning; the nested level pushes its range names on `st.range_names`).
//! 2. **Correlation**: inside the sub-query, `Column{levels_up >= 1}` refer to enclosing levels. For
//!    references with `levels_up == 1` (the level that owns the SubPlan expression): allocate a parameter
//!    `i` per distinct column, rewrite the reference inside the sub-plan to `OuterRef{levels_up: 0,
//!    index: i}` and add the column (as `Column{levels_up: 0, ..}` of the owning level) to
//!    `SubPlanRef::args`. References with `levels_up >= 2` become `Column{levels_up - 1}` in the
//!    sub-plan, to be resolved when the enclosing sub-plan is itself processed (nested sub-plans pass
//!    values down through successive `args`). A sub-plan with no `OuterRef` is uncorrelated
//!    (`is_initplan = true`, executed once).
//! 3. Kind specifics (`SubLinkKind`): `Exists` (`SubPlanDef.plan` may be wrapped in `Limit 1`), `Scalar`
//!    (`Limit 2`? no: executor raises 21000; leave unwrapped), `Array`, `Row`, `Any`/`All`: the `test`
//!    expression (also processed recursively) goes into `SubPlanRef::test`, `ops` copied. `Any` over an
//!    uncorrelated sub-query with hashable `=` operator(s) (all ops are `=` and every tested type is hashable: orderable types per
//!    `compare::is_orderable`; `json`, `xml`-like and other unorderable types are not) sets `use_hash = true`.
//! 4. Each created `SubPlanDef` gets `id = st.subplans.len()`; the numbering is the order of creation
//!    (EXPLAIN prints `SubPlan {id+1}` / `InitPlan {id+1}`; PG creates inner plans first).
//!
//! ## Pull-up of EXISTS / IN into semi-joins (`pull_up_sublinks`) - optional optimization
//! For AND-ed WHERE conjuncts at the top level of a query block: `EXISTS (correlated simple SELECT ...)` ->
//! `Semi` join, `NOT EXISTS` -> `Anti` join, `x IN (SELECT y ...)` (uncorrelated or correlated, no
//! aggregates/limit) -> `Semi` join with qual `x = y`. NOT IN is never converted (NULL semantics).
//! The function may return the plan unchanged; correctness never depends on it.
use pgarbor_common::{PgError, Result};
use pgarbor_sql::ir::BoundExpr;

use crate::context::PlannerState;
use crate::logical::LogicalPlan;

/// Replace every `SubLink` inside `e` by a `SubPlan` (module docs, steps 1-4).
#[allow(unused_variables)]
pub fn process_sublinks(st: &mut PlannerState, e: BoundExpr) -> Result<BoundExpr> {
    Err(PgError::unimplemented("PLN-04", "sub-link planning (subplan::process_sublinks)"))
}

/// Process all sub-links of a list of expressions (target list, quals, join conditions...).
#[allow(unused_variables)]
pub fn process_sublinks_in(st: &mut PlannerState, exprs: Vec<BoundExpr>) -> Result<Vec<BoundExpr>> {
    Err(PgError::unimplemented("PLN-04", "sub-link list planning (subplan::process_sublinks_in)"))
}

/// Pull up top-level EXISTS/IN sub-links of `quals` into semi/anti joins on `plan` (module docs).
/// Returns the new plan and the quals that remain (including converted-away ones removed).
#[allow(unused_variables)]
pub fn pull_up_sublinks(st: &mut PlannerState, plan: LogicalPlan, quals: Vec<BoundExpr>) -> Result<(LogicalPlan, Vec<BoundExpr>)> {
    Err(PgError::unimplemented("PLN-04", "sub-link pull-up (subplan::pull_up_sublinks)"))
}
