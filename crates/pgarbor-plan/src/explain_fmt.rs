//! EXPLAIN support, part 2: output formats (PLN-16). PG reference: `commands/explain.c`
//! (`ExplainPrintPlan`, `ExplainNode`, `ExplainProperty*`, `ExplainOpenGroup`, `ExplainBeginOutput`,
//! `ExplainEndOutput`) and `explain_format.c`.
//!
//! [`render`] turns a FINALIZED `Plan` whose nodes carry `props` (the plan was made with
//! `PlanOptions::for_explain`) into lines. The `costs`, `verbose`, `analyze`, `summary`, `timing`,
//! `buffers`, `settings`, `wal` options come from `ExplainOptions` (only `costs` and `verbose` change
//! output in v1: `ANALYZE` is accepted by the executor, which supplies actual rows/time separately
//! through [`ActualStats`]).
//!
//! ## Text format
//! * Root line has no prefix; child lines are indented by 6 spaces per level and start with `->  `;
//!   node property lines are indented 2 spaces deeper than their node label (`  Filter: (a = 1)` under
//!   the root; `        Filter:` under a `->  ` child at depth 1: the property indent is `indent + 2`
//!   where `indent` is the column of the `->` plus 4 for non-root nodes; see fixtures in `explain.rs`).
//!   Exact rule (PG): a non-root node's label line is `"{pad}->  {label}"` with `pad = " " * (6 * depth - 4)`
//!   (depth 1 -> 2 spaces); its property lines use `pad = " " * (6 * depth)`; the root's properties use 2
//!   spaces.
//! * With `COSTS` on, the label line ends with `  (cost=0.00..2.25 rows=1 width=12)` using
//!   `Estimate` (2-decimal costs, integer rows, width bytes). With `COSTS OFF` nothing is appended.
//! * Sub-plan / CTE blocks as described in `explain.rs`: `"{pad}{Name}"` then the block's plan indented
//!   with `->  `.
//! * Hash joins print the inner child under an extra `Hash` node (`->  Hash` with its own indentation).
//! * Trailing lines (only with `ANALYZE`/`SUMMARY`): `Planning Time: 0.123 ms`, `Execution Time: ...`
//!   are the executor's job; v1 prints none.
//!
//! ## JSON format (`FORMAT JSON`), key order and exact layout (2-space indent, `"key": value`):
//! ```json
//! [
//!   {
//!     "Plan": {
//!       "Node Type": "Seq Scan",
//!       "Parallel Aware": false,
//!       "Async Capable": false,
//!       "Relation Name": "t",
//!       "Alias": "t",
//!       "Disabled": false,
//!       "Filter": "(a = 5)"
//!     }
//!   }
//! ]
//! ```
//! Node object key order: `Node Type`, (join nodes: `Join Type`, `Inner Unique`?), `Parent Relationship`
//! (`Outer`, `Inner`, `Member`, `InitPlan`, `SubPlan`, for non-root), `Subplan Name` (for sub-plans),
//! `Parallel Aware`, `Async Capable`, scan nodes: `Relation Name`, `Schema` (VERBOSE), `Alias`,
//! index scans: `Scan Direction`, `Index Name`; then costs (`Startup Cost`, `Total Cost`, `Plan Rows`,
//! `Plan Width`) when COSTS on, then `Disabled`, then the properties (`Index Cond`, `Filter`, `Hash Cond`,
//! `Sort Key` as arrays of strings, `Group Key` array...), then `Plans` (array of child nodes). Booleans
//! `true`/`false`, strings quoted and JSON-escaped. The top-level is always an array with one object
//! (`"Plan"`), plus `"Planning Time"` only with SUMMARY.
//! ## YAML (`FORMAT YAML`): same structure, block style (`- Plan: ` ... as in the oracle sample below).
//! ## XML (`FORMAT XML`): `<explain xmlns="http://www.postgresql.org/2009/explain">` with
//! `<Query><Plan><Node-Type>Seq Scan</Node-Type>...`; keys with spaces become dashes. v1 may report
//! 0A000 for XML (`EXPLAIN FORMAT XML`).
//! Oracle YAML sample:
//! ```text
//! - Plan: 
//!     Node Type: "Seq Scan"
//!     Parallel Aware: false
//!     Async Capable: false
//!     Relation Name: "t"
//!     Alias: "t"
//!     Disabled: false
//!     Filter: "(a = 5)"
//! ```
//! (note the trailing space after `Plan:`).
use pgarbor_common::{PgError, Result};
use pgarbor_sql::ir::ExplainOptions;

use crate::plan::Plan;

/// Actual execution statistics for `EXPLAIN ANALYZE`, provided by the executor, per plan node in
/// pre-order (root = 0).
// CONTRACT
#[derive(Clone, Debug, Default, PartialEq)]
pub struct ActualStats {
    pub nodes: Vec<NodeActual>,
    pub planning_ms: f64,
    pub execution_ms: f64,
}

/// Actuals of one node.
// CONTRACT
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct NodeActual {
    pub rows: f64,
    pub loops: u64,
    pub startup_ms: f64,
    pub total_ms: f64,
}

/// Render the plan (module docs). Dispatches on `options.format`.
#[allow(unused_variables)]
pub fn render(plan: &Plan, options: &ExplainOptions) -> Result<Vec<String>> {
    Err(PgError::unimplemented("PLN-16", "EXPLAIN rendering (explain_fmt::render)"))
}

/// Render with ANALYZE actuals (`(actual time=0.010..0.020 rows=5 loops=1)` appended after the cost
/// annotation; `Planning Time:` / `Execution Time:` footer lines).
#[allow(unused_variables)]
pub fn render_analyze(plan: &Plan, options: &ExplainOptions, actual: &ActualStats) -> Result<Vec<String>> {
    Err(PgError::unimplemented("PLN-16", "EXPLAIN ANALYZE rendering (explain_fmt::render_analyze)"))
}
