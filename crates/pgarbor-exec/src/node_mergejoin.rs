//! Merge join (lease EXE-18). PG: `nodeMergejoin.c`. Layout: plan.rs `MergeJoinNode`; both inputs
//! arrive sorted by their keys (`left_keys`/`right_keys`: expression, `descending`, `nulls_first`).
//!
//! * Keys are evaluated per row over the child's row and compared pairwise with
//!   `compare::cmp_sort(env, ty, a, b, descending, nulls_first)`. A row with ANY NULL key never matches
//!   (its relative position still follows the sort order); NULL-key rows are passed through
//!   unmatched for the outer-join types.
//! * Standard merge: advance the side with the smaller key; on equal keys collect the whole group of
//!   equal-key rows of the RIGHT side into a buffer, then for every left row of the equal group pair it
//!   with each buffered right row, evaluating the residual `qual` over `left ++ right`; (re)use the
//!   buffered group for all left rows with the same key.
//! * Join types: `Inner`; `Left` (unmatched left rows emit `left ++ NULLs`); `Right` (unmatched right rows
//!   emit `NULLs ++ right`); `Full` (both); `Semi` (left row once at its first qualifying right row);
//!   `Anti` (left rows without any qualifying right row). A left row whose group matches by key but
//!   whose residual `qual` fails for every right row counts as unmatched.
//! * Output `left ++ right` (Semi/Anti: left only). Emit lazily, one row per `next`.
//! * `rescan(params)` rescans both children and clears all buffers.
use pgarbor_common::{PgError, Result};
use pgarbor_plan::plan::{MergeJoinNode, PlanNode};

use crate::build::BuildCtx;
use crate::runtime::RowSource;

#[allow(unused_variables)]
pub fn build_merge_join(node: &PlanNode, n: &MergeJoinNode, b: &mut BuildCtx) -> Result<Box<dyn RowSource>> {
    Err(PgError::unimplemented("EXE-18", "MergeJoin node (node_mergejoin::build_merge_join)"))
}
