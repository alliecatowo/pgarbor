//! ORDER BY / LIMIT / DISTINCT planning and ordering reasoning (PLN-09). PG references:
//! `optimizer/path/pathkeys.c` (`pathkeys_contained_in`, `make_pathkeys_for_sortclauses`),
//! `plan/planner.c` (`create_ordered_paths`, `create_distinct_paths`, `create_limit_path`),
//! `plan/createplan.c` (`make_sort`, `create_limit_plan`, `make_unique_from_sortclauses`),
//! `nodeSort.c` (bounded sort), `nodeLimit.c` (WITH TIES).
//!
//! ## Logical build (`build_sort_limit_distinct`)
//! Applied to the SELECT block's projected plan (`LogicalProject` over all target entries incl. junk):
//! 1. DISTINCT / DISTINCT ON (`sel.distinct`): `LogicalDistinct`. Plain `DISTINCT`: key = all visible
//!    slots; the ORDER BY keys (which `bind` guaranteed are among the visible entries) sort afterwards.
//!    `DISTINCT ON (..)`: key = the ON expressions' slots; the input is sorted by the ORDER BY keys
//!    first (the binder guaranteed the ON keys are a prefix of them) and `on = true`.
//! 2. ORDER BY (`q.order_by: Vec<SortKey>` with `target_idx` into the target list): `LogicalSort` with
//!    [`sort_key_specs`] (expr = target entry expression, direction, NULLS, `using_op`).
//! 3. LIMIT/OFFSET (`q.limit`, `q.offset`, `q.limit_with_ties`) -> `LogicalLimit` (ties keys = the sort
//!    keys; `WITH TIES` without sort keys is a binder error already).
//! 4. A final projection removes junk entries (`n_visible`) when there are any.
//! Order of nodes bottom-up: Project -> Distinct -> Sort -> Limit -> (final Project).
//!
//! ## Physical
//! * `plan_sort`: skip the `Sort` if the child's `ordering` satisfies the requested keys
//!   ([`ordering_satisfies`]: requested keys are a PREFIX of the child's ordering, comparing structurally
//!   equal expressions, same direction and NULLS placement; keys constrained to a single constant by
//!   equality quals may be ignored - optional). Otherwise `Sort`; if the parent is a `Limit` with a
//!   constant count (and no ties) set `SortNode::limit = count + offset` (top-N).
//! * `plan_limit`: `LimitNode` with expressions simplified; `Limit 0` constants may be replaced by a `Result`
//!   with `one_time_filter = false`; a LIMIT over an `IndexScan` that provides the order lets the
//!   planner have asked `index::choose_scan` with `want_ordered_scan_for_limit`.
//! * `plan_distinct`: `Hashed` (HashAggregate-like, `enable_hashagg`, hashable types) for plain DISTINCT
//!   without an ORDER BY requirement; `Sorted` when the child is ordered by the key slots (sort
//!   added otherwise) or for DISTINCT ON; EXPLAIN labels: Hashed -> `HashAggregate` with `Group Key:`,
//!   Sorted -> `Unique`.
use pgarbor_common::{PgError, Result};
use pgarbor_sql::ir::{BoundQuery, BoundSelect, SortKey};

use crate::context::PlannerState;
use crate::logical::{LogicalDistinct, LogicalLimit, LogicalPlan, LogicalSort};
use crate::plan::{PlanNode, SortKeySpec};

/// Append Distinct/Sort/Limit (and the junk-stripping projection) above `projected` (module docs).
#[allow(unused_variables)]
pub fn build_sort_limit_distinct(st: &mut PlannerState, projected: LogicalPlan, sel: Option<&BoundSelect>, q: &BoundQuery) -> Result<LogicalPlan> {
    Err(PgError::unimplemented("PLN-09", "sort/limit/distinct logical plan (sort_limit::build_sort_limit_distinct)"))
}

/// Convert bound `SortKey`s (target-list indexes) to executable key specs over `vars` (the projected
/// plan's output expressions): `SortKeySpec { expr: vars[key.target_idx].clone(), descending,
/// nulls_first, collation: 0 (or the expression's collation), using_op }`.
#[allow(unused_variables)]
pub fn sort_key_specs(keys: &[SortKey], vars: &[pgarbor_sql::ir::BoundExpr]) -> Result<Vec<SortKeySpec>> {
    Err(PgError::unimplemented("PLN-09", "sort key construction (sort_limit::sort_key_specs)"))
}

/// Does an input with ordering `have` satisfy the requested ordering `want` (prefix semantics)?
#[allow(unused_variables)]
pub fn ordering_satisfies(have: &[SortKeySpec], want: &[SortKeySpec]) -> Result<bool> {
    Err(PgError::unimplemented("PLN-09", "ordering reasoning (sort_limit::ordering_satisfies)"))
}

/// Physical `Sort` (or the child itself when already ordered).
#[allow(unused_variables)]
pub fn plan_sort(st: &mut PlannerState, s: &LogicalSort, top_n: Option<u64>) -> Result<PlanNode> {
    Err(PgError::unimplemented("PLN-09", "sort planning (sort_limit::plan_sort)"))
}

/// Physical `Limit`.
#[allow(unused_variables)]
pub fn plan_limit(st: &mut PlannerState, l: &LogicalLimit) -> Result<PlanNode> {
    Err(PgError::unimplemented("PLN-09", "limit planning (sort_limit::plan_limit)"))
}

/// Physical `Distinct`.
#[allow(unused_variables)]
pub fn plan_distinct(st: &mut PlannerState, d: &LogicalDistinct) -> Result<PlanNode> {
    Err(PgError::unimplemented("PLN-09", "distinct planning (sort_limit::plan_distinct)"))
}
