//! Simple pipeline operators (lease EXE-13, north-star): `Filter`, `Project`, `Append`,
//! `Materialize`. Layouts: plan.rs.
//!
//! * **Filter**: pulls rows from `input`, passes those for which `eval_qual(predicate, row)` is TRUE.
//!   Output row = input row.
//! * **Project**: output row = `eval_exprs(exprs, input_row)` (width = `exprs.len()`). Expressions
//!   may contain sub-plans and volatile functions: evaluate exactly once per input row, in order.
//! * **Append** (UNION ALL): drains `children` in order; a child exhausted moves to the next.
//!   `rescan(params)` rescans every child and restarts at the first. Children have identical
//!   slot types, so rows are forwarded unchanged.
//! * **Materialize**: buffers the input rows on first use and replays them from memory on later
//!   scans. `rescan(params)`: if `ex.epoch()` equals the epoch recorded when the buffer was filled
//!   (no `with_outer` frame was pushed since), restart the replay position at 0 WITHOUT touching the
//!   child; otherwise discard the buffer, `child.rescan(params)` and refill lazily. The buffer fills
//!   incrementally as rows are pulled (a partially consumed Materialize that is rescanned must not
//!   lose already buffered rows: finish filling from the child on the next pull).
//! All of them call `ex.check_interrupts()` at least every 1024 rows pulled in a loop that can skip
//! rows (Filter).
use pgarbor_common::{PgError, Result};
use pgarbor_plan::plan::{AppendNode, FilterNode, MaterializeNode, PlanNode, ProjectNode};

use crate::build::BuildCtx;
use crate::runtime::RowSource;

#[allow(unused_variables)]
pub fn build_filter(node: &PlanNode, n: &FilterNode, b: &mut BuildCtx) -> Result<Box<dyn RowSource>> {
    Err(PgError::unimplemented("EXE-13", "Filter node (node_basic::build_filter)"))
}

#[allow(unused_variables)]
pub fn build_project(node: &PlanNode, n: &ProjectNode, b: &mut BuildCtx) -> Result<Box<dyn RowSource>> {
    Err(PgError::unimplemented("EXE-13", "Project node (node_basic::build_project)"))
}

#[allow(unused_variables)]
pub fn build_append(node: &PlanNode, n: &AppendNode, b: &mut BuildCtx) -> Result<Box<dyn RowSource>> {
    Err(PgError::unimplemented("EXE-13", "Append node (node_basic::build_append)"))
}

#[allow(unused_variables)]
pub fn build_materialize(node: &PlanNode, n: &MaterializeNode, b: &mut BuildCtx) -> Result<Box<dyn RowSource>> {
    Err(PgError::unimplemented("EXE-13", "Materialize node (node_basic::build_materialize)"))
}
