//! `EXPLAIN [ANALYZE]` execution (lease EXE-40). PG: `commands/explain.c`. Plan rendering is
//! `pgarbor_plan::explain_plan` / `explain_fmt::render_analyze`.
//!
//! `exec_explain(cx, explain)`:
//! 1. The inner statement must be plannable (`BoundStatement::is_plannable`: SELECT, INSERT, UPDATE,
//!    DELETE, CREATE TABLE AS); `EXECUTE name` is resolved by the SESSION before calling (it substitutes
//!    the prepared statement); anything else -> 0A000 `EXPLAIN of this statement is not supported`.
//! 2. Plan it with `pgarbor_plan::plan(stmt, &PlanContext{env: &cx.se.env, param_types:
//!    cx.se.param_types.clone(), param_values: Some(cx.se.params.clone()) when params exist else
//!    None, options: PlanOptions{for_explain: true, verbose: opts.verbose, ..Default::default()},
//!    table_rows: Default::default()})`.
//! 3. `analyze == false`: lines = `pgarbor_plan::explain_plan(&plan, &opts)`.
//!    `analyze == true`: set `cx.se.options.collect_stats = true`, `open_plan`, run to completion
//!    discarding result rows (DML inside EXPLAIN ANALYZE REALLY modifies data; the session wraps it in
//!    the caller's transaction), take `PlanStream::actuals()` (pre-order per `build.rs` numbering: main
//!    tree, then sub-plans in id order, then CTEs), measure `execution_ms` for the run and
//!    `planning_ms` for step 2, and render with `explain_fmt::render_analyze(&plan, &opts,
//!    &ActualStats{nodes, planning_ms, execution_ms})`. Restore `collect_stats = false` afterwards.
//! 4. Result: `ExecResult::Rows` with column `QUERY PLAN` (`ColumnDesc` name `QUERY PLAN`, text, no
//!    table); TEXT and YAML: one row per output line; JSON: ONE row whose value is
//!    `Datum::Json(lines.join("\n"))` with column type `json`; XML: whatever the planner returns (0A000).
//!    Tag `EXPLAIN`.
use pgarbor_common::{PgError, Result};
use pgarbor_sql::ir::BoundExplain;

use crate::context::ExecContext;
use crate::result::ExecResult;

#[allow(unused_variables)]
pub fn exec_explain(cx: &mut ExecContext, explain: &BoundExplain) -> Result<ExecResult> {
    Err(PgError::unimplemented("EXE-40", "EXPLAIN (explain_exec::exec_explain)"))
}
