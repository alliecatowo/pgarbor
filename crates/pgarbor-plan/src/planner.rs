//! Planner orchestration: `BoundStatement` -> `Plan` (PLN-13). PG reference: `planner.c`
//! (`standard_planner`, `subquery_planner`, `grouping_planner`), `createplan.c` (`create_plan`),
//! `setrefs.c` (`set_plan_references`), `plancat.c`.
//!
//! ## `plan_statement(stmt, ctx)`
//! 1. `PlannerState::new(ctx)`.
//! 2. By statement: `Query` -> `plan_query_plan`; `Insert`/`Update`/`Delete`/`CreateTableAs` ->
//!    `build_dml::plan_dml_statement` (PlanKind Insert/Update/Delete/Populate); anything else ->
//!    `PgError::internal("statement is not plannable")` (XX000).
//! 3. Query: `build::build_query` -> [`plan_logical`] -> root `PlanNode`.
//! 4. If `ctx.options.for_explain`: `explain::annotate_plan` (properties) BEFORE finalization.
//! 5. `setrefs::finalize` converts expressions to executable form (consumes `vars`/`ordering`).
//! 6. Assemble [`Plan`]: `subplans`/`ctes` from the state, `output` from [`output_columns`], `param_types`
//!    from the context, `deps` (`PlanDeps { relations, types, functions, uses_search_path: true }`),
//!    `catalog_version = ctx.env.snapshot().version()`.
//!
//! ## `plan_logical(st, lp)`: physical conversion of a logical node (recursive)
//! * `Scan`: `Table` -> `index::choose_scan` (virtual -> `VirtualScan` with `eq_filters`); `Subquery` ->
//!   `plan_logical` of the child, then a PASS-THROUGH `Project` on top: `exprs` = the child's `vars`
//!   (inner-level terms, unchanged), `vars` = `Column{0, rt_index, i+1}` for the parent's range entry,
//!   `out` from the range entry. NEVER mutate the child's own `vars` (its filters are still expressed in
//!   its own level's terms; `setrefs` removes identity projections later); keep the child's `ordering`
//!   translated through the projection when it is a plain column mapping. `Function` -> `FunctionScan`; `Values` -> `ValuesScan`
//!   (`subplan::process_sublinks_in` over cells); `Cte` -> `CteScan`; `WorkTable` -> `WorkTableScan`;
//!   `OneRow` -> `Result` with no exprs. Scan quals: sub-links processed first
//!   (`subplan::process_sublinks_in`), then given to the path chooser.
//! * `Join` -> `join::plan_join`; `Filter` -> `Filter` node (sub-links processed); `Aggregate` ->
//!   `agg::plan_aggregate`; `Window` -> `window::plan_window`; `Project` -> [`plan_project`]; `Sort` ->
//!   `sort_limit::plan_sort` (the parent `Limit` passes top-N); `Limit` -> `sort_limit::plan_limit`;
//!   `Distinct` -> `sort_limit::plan_distinct`; `SetOp` -> `setop::plan_setop`; `LockRows` ->
//!   `LockRows` node.
//! * Every node gets `out` (names/types), `vars`, `est` (via `cost::cost_node`), `ordering`, and
//!   `level`: `plan_logical` sets `st.cur_level = lp.level` on entry (restoring the previous value on exit)
//!   and every module creates nodes with `PlannerState::new_node`, so EXPLAIN can find the range table names
//!   of each node (`PlanNode::level`).
//!
//! ## `plan_project`
//! Evaluates the target list. Simplify every expression (`simplify::simplify_expr`), process sub-links,
//! build `Project` (or `ProjectSet` when `has_srf`). Do NOT elide identity projections here: `setrefs`
//! does it after translation (an identity can only be recognized in slot terms). `LIMIT` above a `Project` containing
//! expensive/volatile expressions is still applied after projection (semantics). `ordering` passes
//! through for expressions that merely rename columns.
//!
//! ## `output_columns(q)`
//! `ColumnDesc { name, type_id, table_oid, column_attnum }` from `BoundQuery::output` (`origin`).
use pgarbor_common::{PgError, Result};
use pgarbor_sql::ir::{BoundQuery, BoundStatement};

use crate::context::{PlanContext, PlannerState};
use crate::logical::{LogicalPlan, LogicalProject};
use crate::plan::{ColumnDesc, Plan, PlanNode};

/// Plan one statement (module docs).
#[allow(unused_variables)]
pub fn plan_statement(stmt: &BoundStatement, ctx: &PlanContext) -> Result<Plan> {
    Err(PgError::unimplemented("PLN-13", "statement planning entry (planner::plan_statement)"))
}

/// Plan a query (SELECT) into its physical root node (steps 3 of the module docs, without
/// finalization). Also used for sub-plans, CTAS sources and INSERT ... SELECT sources.
#[allow(unused_variables)]
pub fn plan_query_node(st: &mut PlannerState, q: &BoundQuery) -> Result<PlanNode> {
    Err(PgError::unimplemented("PLN-13", "query planning (planner::plan_query_node)"))
}

/// Physical conversion of a logical plan (module docs).
#[allow(unused_variables)]
pub fn plan_logical(st: &mut PlannerState, lp: &LogicalPlan) -> Result<PlanNode> {
    Err(PgError::unimplemented("PLN-13", "logical to physical conversion (planner::plan_logical)"))
}

/// Target-list projection (module docs).
#[allow(unused_variables)]
pub fn plan_project(st: &mut PlannerState, p: &LogicalProject) -> Result<PlanNode> {
    Err(PgError::unimplemented("PLN-13", "projection planning (planner::plan_project)"))
}

/// RowDescription columns of a query (module docs).
pub fn output_columns(q: &BoundQuery) -> Vec<ColumnDesc> {
    q.output
        .iter()
        .map(|c| ColumnDesc {
            name: c.name.clone(),
            type_id: c.ty,
            table_oid: c.origin.map(|o| o.table).unwrap_or(0),
            column_attnum: c.origin.map(|o| o.attnum).unwrap_or(0),
        })
        .collect()
}
