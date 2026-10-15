//! Cost model and cardinality estimates (PLN-12). PGArbor keeps NO table statistics in v1
//! (`relpages`/`reltuples` are static), so estimates are rule based; they only steer join order,
//! hash build side and strategy choices and fill the `(cost=.. rows=.. width=..)` EXPLAIN annotations.
//! EXPLAIN costs are NOT expected to equal PostgreSQL's (differential tests run with `COSTS OFF`).
//! PG references for the formulas: `optimizer/path/costsize.c` (`cost_seqscan`, `cost_index`,
//! `cost_sort`, `cost_agg`, `final_cost_hashjoin`, ...), `utils/adt/selfuncs.c` default selectivities
//! (`DEFAULT_EQ_SEL` 0.005, `DEFAULT_INEQ_SEL` 0.3333333333333333, `DEFAULT_RANGE_INEQ_SEL` 0.005,
//! `DEFAULT_MATCH_SEL` 0.005, `DEFAULT_NUM_DISTINCT` 200, `DEFAULT_UNK_SEL` 0.005).
//!
//! * [`table_rows`]: `PlanContext::table_rows[oid]` when present, else `options.default_table_rows` (1000); virtual catalog relations 100;
//!   `VALUES` the row count; function scans `1000` for SRFs (`BuiltinProc`/`prorows` if known), 1 for
//!   scalar functions; sub-queries their plan's estimate; CTE scans the CTE estimate.
//! * [`selectivity`]: `=` against a constant 0.005 (full-key match of a unique index: `1/rows`), `<>`
//!   0.995, `<`,`<=`,`>`,`>=` 1/3, a pair of opposing inequalities on one column 0.005, `LIKE`/`~~`/regex
//!   match operators 0.005, `IS NULL` 0.005, `IS NOT NULL` 0.995, `x = ANY(array of n)` `min(1, 0.005 n)`,
//!   `AND` multiplies, `OR` is `s1 + s2 - s1*s2`, `NOT` is `1 - s`, constant `true` 1 and `false`/NULL 0,
//!   anything else 0.005. Clamp to [0, 1].
//! * Costs (arbitrary but monotone): `cpu_tuple_cost 0.01`, `cpu_operator_cost 0.0025`, `seq_page_cost 1.0`
//!   (pages = rows * width / 8192), `random_page_cost 4.0`, `cpu_index_tuple_cost 0.005`; sort
//!   `2 * cpu_operator_cost * n * log2(n)`; hash join build/probe linear; nested loop `outer_rows *
//!   inner_cost`.
//! All functions are pure over their arguments.
use pgarbor_common::{PgError, Result};
use pgarbor_sql::ir::BoundExpr;

use crate::context::PlannerState;
use crate::logical::LogicalScan;
use crate::plan::Estimate;

/// Estimated row count of a scan before its quals.
#[allow(unused_variables)]
pub fn table_rows(st: &PlannerState, scan: &LogicalScan) -> Result<f64> {
    Err(PgError::unimplemented("PLN-12", "base cardinality (cost::table_rows)"))
}

/// Estimated rows of a scan after its own quals (`table_rows * selectivity(quals)`, at least 1).
#[allow(unused_variables)]
pub fn estimate_scan_rows(st: &PlannerState, scan: &LogicalScan) -> Result<f64> {
    Err(PgError::unimplemented("PLN-12", "scan cardinality (cost::estimate_scan_rows)"))
}

/// Selectivity of one boolean expression in [0, 1] (module docs).
#[allow(unused_variables)]
pub fn selectivity(e: &BoundExpr) -> Result<f64> {
    Err(PgError::unimplemented("PLN-12", "qual selectivity (cost::selectivity)"))
}

/// Estimated number of groups for `n_keys` grouping keys over `input_rows` rows.
#[allow(unused_variables)]
pub fn estimate_groups(input_rows: f64, n_keys: usize) -> Result<f64> {
    Err(PgError::unimplemented("PLN-12", "group cardinality (cost::estimate_groups)"))
}

/// Estimated join output rows (`inner join: l*r*sel`, outer joins at least the preserved side).
#[allow(unused_variables)]
pub fn join_rows(left_rows: f64, right_rows: f64, join_type: crate::plan::JoinType, equi_keys: usize, other_quals: usize) -> Result<f64> {
    Err(PgError::unimplemented("PLN-12", "join cardinality (cost::join_rows)"))
}

/// Cost estimate of common node shapes: returns `Estimate` with startup/total cost filled from the
/// child estimates. `shape` names the node: "seqscan", "indexscan", "filter", "project", "sort",
/// "hashagg", "groupagg", "hashjoin", "mergejoin", "nestloop", "limit", "append", "unique", "setop",
/// "window", "materialize", "values", "function", "result".
#[allow(unused_variables)]
pub fn cost_node(shape: &str, rows: f64, width: u32, children: &[Estimate]) -> Result<Estimate> {
    Err(PgError::unimplemented("PLN-12", "node costing (cost::cost_node)"))
}
