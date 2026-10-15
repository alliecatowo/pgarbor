//! Final expression translation: bind-time -> executable (PLN-14, sonnet). PG reference:
//! `optimizer/plan/setrefs.c` (`set_plan_references`, `set_plan_refs`, `set_upper_references`,
//! `set_join_references`, `fix_join_expr`, `fix_upper_expr`, `search_indexed_tlist_for_var`,
//! `search_indexed_tlist_for_non_var`, `build_tlist_index`).
//!
//! `finalize(plan)` walks the tree top-down (the root first, then children with their own `vars`) and
//! rewrites every expression stored in a node so it only contains executable forms (see `plan.rs` module
//! docs). The contract between planner and `setrefs` is the `PlanNode::vars` list: for each node, the
//! bind-time expression producing each output slot.
//!
//! ## Rules per node
//! * **Scans** (`SeqScan`, `IndexScan`, `VirtualScan`, `FunctionScan`, `ValuesScan`, `CteScan`,
//!   `WorkTableScan`): the scan's quals and other expressions are translated against the scan's OWN `vars`
//!   (the bind-time expressions it produces, `Column{0, rt_index, attnum}` in its own query level's terms),
//!   exactly like any other node translates against its child's vars: `Column` equal to `vars[i]` ->
//!   `InputRef(i)`. Scan nodes carry no rt_index: rt indexes restart in every sub-query, CTE and sub-plan
//!   and self-joins reuse one table OID, so NEVER key anything by rt_index across nodes. Whole-row
//!   `attnum 0` -> `Row` of the scan's column `InputRef`s; `tableoid` -> constant table OID; other system
//!   columns 0A000. Scan `needed` masks are NOT computed in v1: leave every entry `true`.
//! * **Project/ProjectSet/Filter/Sort/Limit/Distinct/Aggregate(input exprs)/WindowAgg/Materialize/LockRows/
//!   ModifyTable input exprs**: each `Column` or non-trivial subtree equal (`PartialEq`, ignoring
//!   `ColumnRef::pos`) to `child.vars[i]` becomes `InputRef(i)` (search the WHOLE subtree first, then
//!   recurse into children if no match - `search_indexed_tlist_for_non_var`), so shared subexpressions
//!   already computed by a lower node (grouping expressions, aggregates, window functions, SRF outputs) are
//!   referenced, not recomputed. A `Column` with no match in the child's vars is an internal error
//!   (XX000 `variable not found in subplan target list`).
//! * **Joins**: expressions over `outer ++ inner`: search the outer child's vars (slot `i`) then the inner
//!   child's vars (slot `outer.width() + i`). Join keys (`outer_keys`/`left_keys`) are translated against
//!   their own child only. `NestedLoop::inner_params` are translated against the OUTER child; inside the
//!   inner subtree `OuterRef{levels_up: 0, index}` is already in place (created by the join planner).
//! * **Aggregate**: `group_keys`, `aggs[*].args/filter/order_by/direct_args` against the input;
//!   `having` against the OUTPUT (`group_keys ++ aggs`): `Aggref` -> `InputRef(n_keys + k)` by structural
//!   match with the corresponding `AggCall`'s source `Aggref` (kept in `vars`); a group-key subexpression ->
//!   `InputRef(i)`. `Grouping{args}` -> `Grouping{args, mask_slot: Some(n_keys + n_aggs)}`.
//! * **WindowAgg**: partition/order/args/frame offsets against the input; the parent Project's
//!   `WindowFunc` -> `InputRef(input.width() + k)`.
//! * **Sub-plans**: `SubPlanRef::test` against the evaluating node's input; `SubPlanRef::args` are
//!   translated like any expression of that node; the `SubPlanDef.plan` trees are finalized
//!   independently with their own `vars` (their `OuterRef`s were created by `subplan`).
//! * **ModifyTable**: translate `insert.defaults/generated`, `update.assignments/generated`, `checks`
//!   (over the NEW row layout documented in `plan.rs`), `returning` (RETURNING input row layout),
//!   `ConflictPlanAction` expressions (existing ++ excluded layout).
//! * `Param`, `Const`, `OuterRef`, `SqlValue`, `CaseTestValue`, `SetToDefault` (error if left) unchanged.
//! * **Pass-through projections** (planner.rs sub-query boundaries): a `Project` whose translated `exprs`
//!   are exactly `InputRef(0)..InputRef(n-1)` with `n == child.width()` is removed (replaced by its child,
//!   keeping the Project's `out` names and `est`); do this bottom-up AFTER translating the node and its
//!   children so that parents' `InputRef` slots (which refer to the Project's slots) stay valid.
//! After translation clear every `PlanNode::vars`/`ordering` (set to empty, `level` to 0). Also re-derive each node's `out` types from the translated expressions when
//! the planner left them empty (`Project`, `Aggregate`, `WindowAgg`).
//!
//! Postconditions asserted in debug builds: no `Column`/`SubLink`/`Aggref`/`WindowFunc` anywhere in
//! `Plan` (use `BoundExpr::any`); `Grouping` only with `mask_slot: Some`.
use pgarbor_common::{PgError, Result};

use crate::plan::Plan;

/// Convert all expressions of `plan` (root, sub-plans and CTE bodies) to executable form.
#[allow(unused_variables)]
pub fn finalize(plan: &mut Plan) -> Result<()> {
    Err(PgError::unimplemented("PLN-14", "plan reference fixing (setrefs::finalize)"))
}

/// Check the executable-form postconditions (module docs); returns XX000 describing the first offending
/// node. Used by tests and `debug_assert`s.
#[allow(unused_variables)]
pub fn verify_executable(plan: &Plan) -> Result<()> {
    Err(PgError::unimplemented("PLN-14", "executable plan verification (setrefs::verify_executable)"))
}
