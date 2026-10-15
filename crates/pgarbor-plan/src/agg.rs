//! Aggregation planning (PLN-07). PG references: `optimizer/plan/planner.c`
//! (`create_grouping_paths`, `consider_groupingsets_paths`, `make_partial_grouping_target`),
//! `plan/createplan.c` (`create_agg_plan`, `create_groupingsets_plan`), `util/clauses.c`
//! (`contain_agg_clause`), `plan/setrefs.c` (`set_upper_references` for aggregates),
//! `parse_agg.c`.
//!
//! ## Logical build (`build_aggregate`)
//! Called by `build::build_select` when `sel.has_aggs` or GROUP BY / HAVING is present:
//! * `group_exprs` = the grouping expressions: `sel.target_list[i].expr` for `i in group_by.exprs`
//!   (in that order); `grouping_sets` = `group_by.sets`.
//! * `aggrefs` = every distinct (structurally equal) `Aggref` with `levels_up == 0` found in the target
//!   list, HAVING and window definitions' expressions ([`collect_aggrefs`]).
//! * `having` = HAVING conjuncts; conjuncts of HAVING that reference NO aggregate and only grouping
//!   expressions are moved BELOW the aggregate as filters on the input (PG does this; result-equivalent
//!   for plain GROUP BY, NOT for grouping sets or `Plain` aggregation without GROUP BY... only when
//!   there is a GROUP BY and no grouping sets).
//! * Output `vars = group_exprs ++ aggrefs` (+ `Const` mask placeholder `Column`-free var for the mask
//!   when grouping sets: a `Grouping`-less `Const(Int8(0))` marker is NOT used: setrefs finds the mask
//!   slot by position `group_exprs.len() + aggrefs.len()` from `AggregateNode`).
//! * The query's target list is evaluated above (`Project`), where `Aggref` nodes and group-expression
//!   subtrees are matched to aggregate output slots by `setrefs`.
//!
//! ## Physical choice (`plan_aggregate`)
//! * No `group_exprs` and no sets -> `AggStrategy::Plain`.
//! * Otherwise `Hashed` if `enable_hashagg`, every group key type is hashable (`compare::hash_datum`
//!   supports it) and there are no sets requiring sorted input; `Sorted` if the input is already ordered
//!   by the group keys (`PlanNode::ordering` prefix match), or hashing is disabled/impossible (insert a
//!   `Sort` on the keys; EXPLAIN shows `GroupAggregate`). Grouping sets: `Hashed`.
//! * `AggCall` construction from each `Aggref`: copy args/filter/order/distinct/star, `result_type = ty`,
//!   `arg_types` from the args.
//! * `Plain` aggregation over an empty input still yields one row; planner must not add a one-time
//!   false filter below it. A `Plain` aggregate with `LIMIT 0`... normal.
//! * Output `out`: names `group key` / aggregate names; estimates: groups = min(input rows,
//!   `cost::estimate_groups`).
//! * `ordering`: `Sorted` -> the group keys; `Hashed` -> none.
use pgarbor_common::{PgError, Result};
use pgarbor_sql::ir::{BoundExpr, BoundSelect};

use crate::context::PlannerState;
use crate::logical::{LogicalAggregate, LogicalPlan};
use crate::plan::PlanNode;

/// Distinct `Aggref` (level 0) expressions found in `exprs`, in first-appearance order.
#[allow(unused_variables)]
pub fn collect_aggrefs(exprs: &[&BoundExpr]) -> Result<Vec<BoundExpr>> {
    Err(PgError::unimplemented("PLN-07", "aggregate collection (agg::collect_aggrefs)"))
}

/// Wrap `input` in a `LogicalAggregate` for `sel` (module docs "Logical build").
#[allow(unused_variables)]
pub fn build_aggregate(st: &mut PlannerState, input: LogicalPlan, sel: &BoundSelect) -> Result<LogicalPlan> {
    Err(PgError::unimplemented("PLN-07", "aggregate logical plan (agg::build_aggregate)"))
}

/// Physical plan of a `LogicalAggregate` (module docs "Physical choice").
#[allow(unused_variables)]
pub fn plan_aggregate(st: &mut PlannerState, a: &LogicalAggregate) -> Result<PlanNode> {
    Err(PgError::unimplemented("PLN-07", "aggregate planning (agg::plan_aggregate)"))
}
