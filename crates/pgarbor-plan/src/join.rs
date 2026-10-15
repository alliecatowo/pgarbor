//! Join ordering and join algorithm selection (PLN-06). PG references:
//! `optimizer/path/joinpath.c` (`add_paths_to_joinrel`, `match_unsorted_outer`, `hash_inner_and_outer`,
//! `sort_inner_and_outer`), `joinrels.c` (`make_join_rel`), `allpaths.c` (`standard_join_search`
//! simplified), `util/restrictinfo.c`/`equivclass.c` (`select_equality_operators`),
//! `plan/createplan.c` (`create_nestloop_plan`, `create_hashjoin_plan`, `create_mergejoin_plan`,
//! `get_switched_clauses`).
//!
//! ## Join ordering (`order_join_tree`, called while building the logical plan)
//! Input: a list of FROM items (each a logical plan) plus the AND-ed WHERE conjuncts. Inner joins
//! commute: build a left-deep tree greedily: start from the item with the smallest estimated rows
//! (scan quals counted, `cost::estimate_scan_rows`); repeatedly add the remaining item that is
//! connected to the current set by an equality conjunct (prefer the one with a unique-index lookup
//! possibility on the new item), falling back to the smallest unconnected item (cross join, last).
//! LATERAL items keep their dependency order. Explicit outer joins are NOT reordered
//! (their syntactic nesting is preserved); only maximal inner-join groups are. Item count above 8: keep
//! syntactic order (PG `join_collapse_limit`). Join quals are attached by `pushdown`.
//!
//! ## `plan_join(st, j)` physical choice
//! 1. Plan both inputs (`planner::plan_logical`; for a nested-loop with an index-parameterized inner
//!    plan the inner side is planned by `plan_inner_with_params`).
//! 2. Split `j.quals` into **equi-join keys** and residual: a conjunct `L = R` (operator `=` resolved
//!    for the operands, hashable/mergeable type) where `expr_relids(L)` is within the left input's rels and
//!    `expr_relids(R)` within the right's (or swapped: swap them) and neither side is volatile; the rest
//!    is the residual `qual`.
//! 3. Strategy by join type (RIGHT is rewritten to LEFT by swapping inputs - keep the output column
//!    order via a final `Project` that restores `left ++ right` order):
//!    * keys non-empty, `enable_hashjoin`: **HashJoin** with the SMALLER estimated input as the build
//!      (inner) side when the join type allows (INNER/FULL/SEMI-with-swap rules; LEFT/SEMI/ANTI build on
//!      the nullable/inner side only: for LEFT the right input builds; swap an INNER join if the left is
//!      estimated smaller). FULL JOIN requires hashable or mergeable keys, else 0A000 `FULL JOIN is
//!      only supported with merge-joinable or hash-joinable join conditions` (PG message, SQLSTATE
//!      0A000).
//!    * both inputs already ordered by the keys (`PlanNode::ordering`, e.g. index scans) or
//!      `enable_hashjoin` off: **MergeJoin** (insert `Sort` below inputs that are not ordered).
//!    * no equi keys: **NestedLoop** (INNER/LEFT/SEMI/ANTI; the right side wrapped in `Materialize` when
//!      it is not a cheap rescannable scan and `enable_material`); FULL/RIGHT without keys: 0A000 as above.
//!    * **Index nested loop**: for INNER/LEFT/SEMI/ANTI, when the inner (right) input is a base table
//!      scan and `index::find_param_index` finds an index on its equi columns, and the outer is not
//!      estimated larger than ~1000 rows... (rule: always when the lookup is a unique point lookup; for
//!      non-unique indexes when the outer estimate < 1/10 of the inner estimate): build NestedLoop with
//!      `inner_params` = outer key expressions and the inner plan an `IndexScan` whose `bounds.eq`
//!      values are `OuterRef{0,i}`; remaining equi conjuncts and residual go to `qual`/filter.
//!    * LATERAL right side: NestedLoop with `inner_params` = the outer columns the right side
//!      references (rewritten to `OuterRef`).
//! 4. Output `vars = left.vars ++ right.vars` (Semi/Anti: left only), `out` accordingly, `ordering`:
//!    NestedLoop/HashJoin(probe order) keep the outer ordering; MergeJoin: its key order.
//! 5. `Plan` estimates via `cost::join_rows` etc.
use pgarbor_common::{PgError, Result};
use pgarbor_sql::ir::BoundExpr;

use crate::context::PlannerState;
use crate::logical::{LogicalJoin, LogicalPlan};
use crate::plan::PlanNode;

/// Equi-join key candidate extracted from a join qual.
// CONTRACT
#[derive(Clone, Debug, PartialEq)]
pub struct EquiKey {
    /// Expression over the left input only.
    pub left: BoundExpr,
    /// Expression over the right input only.
    pub right: BoundExpr,
    pub op: pgarbor_sql::ir::OpRef,
}

/// Combine independent FROM items and AND-ed quals into a join tree (module docs "Join ordering").
/// Returns the plan and the quals that were not attached (the caller hands them to pushdown).
#[allow(unused_variables)]
pub fn order_join_tree(st: &mut PlannerState, items: Vec<LogicalPlan>, quals: Vec<BoundExpr>) -> Result<(LogicalPlan, Vec<BoundExpr>)> {
    Err(PgError::unimplemented("PLN-06", "join ordering (join::order_join_tree)"))
}

/// Split join conjuncts into equi keys and the residual (module docs step 2).
#[allow(unused_variables)]
pub fn extract_equi_keys(st: &PlannerState, quals: &[BoundExpr], left_rels: &[u32], right_rels: &[u32]) -> Result<(Vec<EquiKey>, Vec<BoundExpr>)> {
    Err(PgError::unimplemented("PLN-06", "equi-join key extraction (join::extract_equi_keys)"))
}

/// Physical plan of a logical join (module docs step 1-5).
#[allow(unused_variables)]
pub fn plan_join(st: &mut PlannerState, j: &LogicalJoin) -> Result<PlanNode> {
    Err(PgError::unimplemented("PLN-06", "join planning (join::plan_join)"))
}
