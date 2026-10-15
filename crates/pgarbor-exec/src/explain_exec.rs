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
use pgarbor_common::time::Instant;

use pgarbor_common::{PgError, Result, SqlState};
use pgarbor_plan::explain_fmt::{render_analyze, ActualStats, NodeActual};
use pgarbor_plan::plan::{ColumnDesc, Plan};
use pgarbor_plan::{PlanContext, PlanOptions};
use pgarbor_sql::ir::{BoundExplain, BoundStatement, ExplainFormat, ExplainOptions};
use pgarbor_types::{Datum, TypeId};

use crate::build::open_plan;
use crate::context::ExecContext;
use crate::result::ExecResult;

/// Rows pulled per `PlanStream::fetch` call while running an `EXPLAIN ANALYZE` (the rows are discarded,
/// so batches only bound the memory used by the stream's buffers).
const ANALYZE_FETCH_BATCH: usize = 1024;

/// Name of the single result column (PG: `ExplainResultDesc`).
const QUERY_PLAN_COLUMN: &str = "QUERY PLAN";

/// Command tag of EXPLAIN (PG: `CreateCommandTag` for `T_ExplainStmt`).
const EXPLAIN_TAG: &str = "EXPLAIN";

pub fn exec_explain(cx: &mut ExecContext, explain: &BoundExplain) -> Result<ExecResult> {
    ensure_plannable(&explain.statement)?;
    let opts = &explain.options;

    // Step 2: plan the inner statement, timing the planner for EXPLAIN ANALYZE.
    let t_plan = Instant::now();
    let plan = {
        let pctx = PlanContext {
            env: &cx.se.env,
            param_types: cx.se.param_types.clone(),
            param_values: if cx.se.params.is_empty() { None } else { Some(cx.se.params.clone()) },
            options: PlanOptions { for_explain: true, verbose: opts.verbose, ..Default::default() },
            table_rows: Default::default(),
        };
        pgarbor_plan::plan(&explain.statement, &pctx)?
    };
    let planning_ms = elapsed_ms(t_plan);

    // Step 3: render, running the plan first when ANALYZE was requested.
    let lines: Vec<String> = if opts.analyze {
        analyze_lines(cx, &plan, opts, planning_ms)?
    } else {
        pgarbor_plan::explain_plan(&plan, opts)?
    };

    // Step 4: shape the result.
    let desc = vec![output_column(opts.format)];
    let rows = output_rows(opts.format, lines);
    Ok(ExecResult::Rows { desc, rows, tag: EXPLAIN_TAG.to_string() })
}

/// Step 1: only statements that read or write relation data can be explained (PG: every statement
/// that is not plannable reaches `ExplainOneQuery`'s utility branch and fails there).
fn ensure_plannable(stmt: &BoundStatement) -> Result<()> {
    if stmt.is_plannable() {
        Ok(())
    } else {
        Err(PgError::new(SqlState::FEATURE_NOT_SUPPORTED, "EXPLAIN of this statement is not supported"))
    }
}

/// Step 3 with ANALYZE: execute the plan to completion with per-node instrumentation and render the
/// actual statistics. The instrumentation flag is restored on every path, including errors.
fn analyze_lines(cx: &mut ExecContext, plan: &Plan, opts: &ExplainOptions, planning_ms: f64) -> Result<Vec<String>> {
    cx.se.options.collect_stats = true;
    let run = run_instrumented(cx, plan);
    cx.se.options.collect_stats = false;
    let (nodes, execution_ms) = run?;
    let actual = ActualStats { nodes, planning_ms, execution_ms };
    render_analyze(plan, opts, &actual)
}

/// Run `plan` to completion, discarding its rows, and return the per-node actuals with the wall time of
/// the run. The caller's transaction owns the effects (DML really writes).
fn run_instrumented(cx: &mut ExecContext, plan: &Plan) -> Result<(Vec<NodeActual>, f64)> {
    let t_exec = Instant::now();
    let mut stream = open_plan(plan, cx)?;
    while !stream.is_done() {
        stream.fetch(cx, Some(ANALYZE_FETCH_BATCH))?;
    }
    let execution_ms = elapsed_ms(t_exec);
    let nodes = stream.actuals().ok_or_else(|| {
        PgError::internal("EXPLAIN ANALYZE: operator statistics were not collected for the plan")
    })?;
    Ok((nodes, execution_ms))
}

/// The single `QUERY PLAN` column: `json` for FORMAT JSON, `text` for every other format (PG:
/// `ExplainResultDesc`).
fn output_column(format: ExplainFormat) -> ColumnDesc {
    let type_id = match format {
        ExplainFormat::Json => TypeId::JSON,
        ExplainFormat::Text | ExplainFormat::Xml | ExplainFormat::Yaml => TypeId::TEXT,
    };
    ColumnDesc { name: QUERY_PLAN_COLUMN.to_string(), type_id, table_oid: 0, column_attnum: 0 }
}

/// Rows of the result: one text row per line, except JSON which is one row holding the whole document
/// (lines re-joined with `\n`, the inverse of the split done by the renderer).
fn output_rows(format: ExplainFormat, lines: Vec<String>) -> Vec<Vec<Datum>> {
    match format {
        ExplainFormat::Json => vec![vec![Datum::Json(lines.join("\n"))]],
        ExplainFormat::Text | ExplainFormat::Xml | ExplainFormat::Yaml => {
            lines.into_iter().map(|line| vec![Datum::Text(line)]).collect()
        }
    }
}

/// Wall-clock milliseconds since `start`.
fn elapsed_ms(start: Instant) -> f64 {
    start.elapsed().as_secs_f64() * 1000.0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn non_plannable_statement_is_rejected_with_0a000() {
        let err = ensure_plannable(&BoundStatement::Empty).expect_err("empty statement is not plannable");
        assert!(err.sqlstate == SqlState::FEATURE_NOT_SUPPORTED);
        assert_eq!(err.message, "EXPLAIN of this statement is not supported");
    }

    #[test]
    fn text_format_gives_one_row_per_line() {
        let lines = vec!["Result  (cost=0.00..0.01 rows=1 width=4)".to_string(), "  Output: 1".to_string()];
        let desc = output_column(ExplainFormat::Text);
        assert_eq!(desc.name, "QUERY PLAN");
        assert_eq!(desc.type_id, TypeId::TEXT);
        let rows = output_rows(ExplainFormat::Text, lines.clone());
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0], vec![Datum::Text(lines[0].clone())]);
        assert_eq!(rows[1], vec![Datum::Text(lines[1].clone())]);
    }

    #[test]
    fn json_format_gives_one_json_row() {
        let lines = vec!["[".to_string(), "  {".to_string(), "]".to_string()];
        let desc = output_column(ExplainFormat::Json);
        assert_eq!(desc.type_id, TypeId::JSON);
        let rows = output_rows(ExplainFormat::Json, lines);
        assert_eq!(rows, vec![vec![Datum::Json("[\n  {\n]".to_string())]]);
    }

    #[test]
    fn json_format_with_no_lines_is_one_empty_document_row() {
        let rows = output_rows(ExplainFormat::Json, Vec::new());
        assert_eq!(rows, vec![vec![Datum::Json(String::new())]]);
    }
}
