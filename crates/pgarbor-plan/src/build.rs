//! `BoundQuery` -> `LogicalPlan` for queries (PLN-01). PG references: `planner.c`
//! (`subquery_planner`, `grouping_planner`), `prepjointree.c` (`pull_up_subqueries`,
//! `deconstruct_jointree` concepts), `initsplan.c` (`distribute_qual_to_rels`), `planmain.c`
//! (`query_planner`).
//!
//! `build_query(st, q)` converts a bound query into a logical plan. Statement-level entry points
//! (`planner::plan_statement`) call it for SELECT, for the source of INSERT, for CTAS and for
//! sub-queries.
//!
//! ## Steps for a SELECT block (`QueryBody::Select`)
//! 1. **Range table**: for every range entry create the leaf [`LogicalScan`] (`build_range_scan`):
//!    `RangeKind::Relation` -> `ScanSource::Table` (TableRef from the catalog `Relation`, `is_virtual`
//!    if oid < 16384; note the dependency with `st.note_relation`); `Subquery` ->
//!    `ScanSource::Subquery(build_query(sub))` (a trivial sub-select - no aggregates/limit/DISTINCT, one
//!    FROM item - MAY be pulled up by substituting its target expressions for the outer `Column`s; skip in
//!    v1: keep the sub-plan); `Function` -> `ScanSource::Function`; `Values` -> `ScanSource::Values`
//!    (rows after `simplify`); `Cte` -> `cte::plan_cte_reference` (inline or `ScanSource::Cte`).
//!    `vars` = `Column{levels_up: 0, rt_index, attnum}` for every column (dropped columns too), `out` =
//!    names/types from the `RangeEntry`.
//! 2. **FROM tree**: `build_from_item` converts `FromItem::Join` recursively into [`LogicalJoin`]
//!    (JoinKind Inner/Left/Right/Full -> `JoinType`), keeping ON quals in `quals`; top-level comma items
//!    are combined with `join::order_join_tree` (inner joins may be reordered freely by it; outer
//!    joins keep their syntactic structure). LATERAL: a scan/subquery whose expressions reference
//!    earlier items gets `lateral = true` on the join that combines it.
//!    `FROM`-less select: `ScanSource::OneRow`.
//! 3. **WHERE**: `simplify::simplify_quals(split_conjuncts(where))`, then
//!    `pushdown::push_down_quals(plan, quals)` distributes them (including outer-join reductions).
//! 4. **Aggregation**: if `sel.has_aggs` or GROUP BY / HAVING exist: `agg::build_aggregate`.
//! 5. **Windows**: `window::build_windows` (one `LogicalWindow` per distinct `WindowDef`).
//! 6. **Target list**: `LogicalProject` over all target entries including junk ones (sublinks in
//!    expressions are handled by `subplan::process_sublinks` when the physical plan is made, not here);
//!    `has_srf` from `sel.has_target_srfs`.
//! 7. **DISTINCT / ORDER BY / LIMIT / locking**: `sort_limit::build_sort_limit_distinct` appends
//!    `Distinct`/`Sort`/`Limit` and a final projection dropping junk entries (`n_visible`), `LockRows`
//!    for `q.locking`.
//! `QueryBody::Values` -> a `Values` scan wrapped by sort/limit; `QueryBody::SetOp` ->
//! `setop::build_setop` then sort/limit. CTEs of the query are registered first with
//! `cte::register_ctes` (they must be available to every reference, including from sub-queries).
//!
//! ## Output
//! The returned plan's `vars` are the query's visible output expressions; `out` carries the final
//! names/types (`BoundQuery::output`).
use pgarbor_common::{PgError, Result};
use pgarbor_sql::ir::{BoundQuery, BoundSelect, FromItem, RangeEntry};

use crate::context::PlannerState;
use crate::logical::{LogicalPlan, LogicalScan};

/// Build the logical plan of a complete query (module docs).
#[allow(unused_variables)]
pub fn build_query(st: &mut PlannerState, q: &BoundQuery) -> Result<LogicalPlan> {
    Err(PgError::unimplemented("PLN-01", "query logical plan (build::build_query)"))
}

/// Build the part of a SELECT block below ORDER BY/LIMIT (steps 1-6).
#[allow(unused_variables)]
pub fn build_select(st: &mut PlannerState, sel: &BoundSelect, q: &BoundQuery) -> Result<LogicalPlan> {
    Err(PgError::unimplemented("PLN-01", "SELECT block logical plan (build::build_select)"))
}

/// Leaf scan for range entry `rt_index` (1-based) of `sel.range_table`.
#[allow(unused_variables)]
pub fn build_range_scan(st: &mut PlannerState, entries: &[RangeEntry], rt_index: u32, with_rowid: bool) -> Result<LogicalScan> {
    Err(PgError::unimplemented("PLN-01", "range entry scan (build::build_range_scan)"))
}

/// Convert one FROM tree item (leaf or explicit join) to a logical plan.
#[allow(unused_variables)]
pub fn build_from_item(st: &mut PlannerState, entries: &[RangeEntry], item: &FromItem, with_rowid: &[u32]) -> Result<LogicalPlan> {
    Err(PgError::unimplemented("PLN-01", "FROM item logical plan (build::build_from_item)"))
}

/// Wrap a scan into a [`LogicalPlan`] with `vars`/`out`/`rels` filled in.
#[allow(unused_variables)]
pub fn scan_to_plan(st: &mut PlannerState, entries: &[RangeEntry], scan: LogicalScan) -> Result<LogicalPlan> {
    Err(PgError::unimplemented("PLN-01", "scan plan wrapper (build::scan_to_plan)"))
}
