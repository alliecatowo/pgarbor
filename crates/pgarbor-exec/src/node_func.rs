//! Function-driven operators (lease EXE-14): `FunctionScan` and `ProjectSet`. Evaluation of a single
//! set-returning call is `eval_srf::open_srf`. PG: `nodeFunctionscan.c`, `nodeProjectSet.c`.
//!
//! ## FunctionScan (`FROM f(...)`, `ROWS FROM (f(), g())`, `unnest(a, b)`, `WITH ORDINALITY`)
//! * On each (re)start evaluate every `FunctionItem::expr` (a `FuncCall`; a scalar non-SRF call yields
//!   exactly one row) over the EMPTY row (arguments may use `Param`/`OuterRef`) with
//!   `eval_srf::open_srf`.
//! * Zip the functions: row i = the i-th row of every function, a function that ran out contributes
//!   NULLs for all its `columns`; stop when ALL functions are exhausted. Each function value is expanded
//!   with `eval_srf::expand_record(value, item.columns.len())` when `columns.len() > 1`.
//! * Output row = concatenated function columns; with `with_ordinality` one trailing `Int8` (1-based
//!   row number). `filter` over the produced row.
//! * Strict SRFs with a NULL argument produce zero rows (handled inside `open_srf`).
//!
//! ## ProjectSet (`SELECT generate_series(1,3), a FROM t`, `SELECT unnest(arr) ...`)
//! For each input row evaluate `exprs`. An expression is a *set expression* if it is a top-level
//! `FuncCall` with `retset` (nested SRFs inside other expressions are rejected by the binder).
//! Non-set expressions are evaluated once per input row and repeated. Set expressions advance in LOCK
//! STEP (PG >= 10 semantics): output row k contains the k-th value of every set expression, finished
//! ones contribute NULL, and rows are produced until ALL set expressions are exhausted. An input row
//! for which every set expression yields zero rows produces no output at all. Composite results are
//! NOT expanded here (the planner puts a `FunctionScan` for `SELECT * FROM f()`).
//! Output width = `exprs.len()`.
use pgarbor_common::{PgError, Result};
use pgarbor_plan::plan::{FunctionScan, PlanNode, ProjectSetNode};

use crate::build::BuildCtx;
use crate::runtime::RowSource;

#[allow(unused_variables)]
pub fn build_function_scan(node: &PlanNode, n: &FunctionScan, b: &mut BuildCtx) -> Result<Box<dyn RowSource>> {
    Err(PgError::unimplemented("EXE-14", "FunctionScan node (node_func::build_function_scan)"))
}

#[allow(unused_variables)]
pub fn build_project_set(node: &PlanNode, n: &ProjectSetNode, b: &mut BuildCtx) -> Result<Box<dyn RowSource>> {
    Err(PgError::unimplemented("EXE-14", "ProjectSet node (node_func::build_project_set)"))
}
