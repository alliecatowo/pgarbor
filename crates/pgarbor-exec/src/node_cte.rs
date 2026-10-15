//! CTE execution (lease EXE-15, sonnet): `CteScan`, `WorkTableScan`, the CTE runtime and eager
//! data-modifying CTEs. PG: `nodeCtescan.c`, `nodeRecursiveunion.c`, `nodeWorktablescan.c`,
//! `ExecPostprocessPlan` (data-modifying CTEs run to completion).
//!
//! ## Runtime state (`runtime::CteRt`)
//! One `CteRt` per `Plan::ctes[id]`, built by [`build_cte_rt`]: the body operators are built with
//! `b.child(..)` (pre-order numbering continues after sub-plans), `state = Pending`, `rows` empty,
//! `worktable` empty.
//!
//! ## `ensure_cte(ex, id)` (lazy evaluation)
//! * `Done`: nothing. `Running` (a non-recursive CTE referencing itself cannot happen): XX000.
//! * `CteBodyRt::Query(op)`: take the operator out (see runtime module docs), set `Running`, drain it
//!   into a `Vec<Row>`, store `rows = Rc::new(..)`, `Done`.
//! * `CteBodyRt::Recursive{non_recursive, recursive, union_all}` (WITH RECURSIVE; PG semantics):
//!   `result = []`; run the non-recursive term fully: its rows (de-duplicated against everything so far
//!   when `!union_all`, using `keycodec` keys of ALL columns with NULLs equal) become both `result`
//!   and the first working table. Loop: while the working table is non-empty: set
//!   `worktable = previous iteration's rows`, `recursive.rescan(ex, &[])`, drain it into `new_rows`
//!   (de-duplicate against `result` and itself when `!union_all`), append to `result`,
//!   `worktable = new_rows`. Check `ex.check_interrupts()` per iteration and per 1024 rows. Finally
//!   `rows = result`.
//! * `CteBodyRt::Modify(op)`: data-modifying CTE; normally executed by [`run_modify_ctes`]; if reached
//!   here `Pending` it is executed the same way.
//!
//! ## [`run_modify_ctes`]
//! Called once by `build::open_plan`: for every CTE whose body is `Modify`, run `ensure_cte`
//! (executes the INSERT/UPDATE/DELETE ... RETURNING to completion and buffers its RETURNING rows),
//! in `id` order, BEFORE the main query starts.
//!
//! ## CteScan / WorkTableScan operators
//! * `CteScan{cte_id, filter}`: on first `next` after (re)start call `ensure_cte`, then iterate a
//!   shared `Rc<Vec<Row>>` clone of `rows` (`rescan` restarts the position only). Output row = CTE
//!   columns; `filter` over it.
//! * `WorkTableScan{cte_id, filter}`: yields the rows currently in `ctes[cte_id].worktable` (a snapshot
//!   cloned at (re)scan time); `rescan` re-reads the work table. Output row = CTE columns.
use pgarbor_common::{PgError, Result};
use pgarbor_plan::plan::{CteDef, CteScan, PlanNode, WorkTableScan};

use crate::build::BuildCtx;
use crate::runtime::{CteRt, Executor, RowSource};

#[allow(unused_variables)]
pub fn build_cte_scan(node: &PlanNode, n: &CteScan, b: &mut BuildCtx) -> Result<Box<dyn RowSource>> {
    Err(PgError::unimplemented("EXE-15", "CteScan node (node_cte::build_cte_scan)"))
}

#[allow(unused_variables)]
pub fn build_worktable_scan(node: &PlanNode, n: &WorkTableScan, b: &mut BuildCtx) -> Result<Box<dyn RowSource>> {
    Err(PgError::unimplemented("EXE-15", "WorkTableScan node (node_cte::build_worktable_scan)"))
}

/// Runtime form of one CTE definition (module docs).
#[allow(unused_variables)]
pub fn build_cte_rt(def: &CteDef, b: &mut BuildCtx) -> Result<CteRt> {
    Err(PgError::unimplemented("EXE-15", "CTE runtime construction (node_cte::build_cte_rt)"))
}

/// Execute CTE `id` if still pending (module docs).
#[allow(unused_variables)]
pub fn ensure_cte(ex: &mut Executor, id: u32) -> Result<()> {
    Err(PgError::unimplemented("EXE-15", "CTE evaluation (node_cte::ensure_cte)"))
}

/// Run all data-modifying CTEs to completion (module docs).
#[allow(unused_variables)]
pub fn run_modify_ctes(ex: &mut Executor) -> Result<()> {
    Err(PgError::unimplemented("EXE-15", "data-modifying CTE execution (node_cte::run_modify_ctes)"))
}
