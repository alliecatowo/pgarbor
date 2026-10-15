//! `Distinct` and `SetOp` operators (lease EXE-24). PG: `nodeUnique.c`, `nodeSetOp.c`,
//! `nodeAgg.c` (hashed DISTINCT).
//!
//! ## Distinct (`DistinctNode{strategy, input, key_slots}`)
//! Emits the FIRST input row of every distinct key, where the key = the values at `key_slots` compared
//! with NULLs EQUAL (`sortutil::row_key_bytes` over the input column types and `key_slots`).
//! * `Hashed`: a `HashSet` of key bytes; rows stream out as they are first seen.
//! * `Sorted`: input is ordered so equal keys are adjacent: compare with the previous emitted key
//!   (`compare::not_distinct` per slot) and emit only changes.
//! Output row = input row (junk columns are stripped by a `Project` above).
//!
//! ## SetOp (`SetOpNode{cmd, left, right}`; all slots compared, NULLs equal)
//! Drain `right` into a multiset (`HashMap<key bytes, count>`), then stream `left`:
//! * `Intersect`: emit a left row the first time its key is seen if the key is present in `right`
//!   (distinct result); `IntersectAll`: each key is emitted `min(left_count, right_count)` times (emit
//!   while the remaining right count is > 0, decrementing it);
//! * `Except`: emit a left row the first time its key is seen if the key is ABSENT from `right`;
//!   `ExceptAll`: each key `max(left_count - right_count, 0)` times (skip the first `right_count`
//!   occurrences of the key, emit the rest, i.e. decrement a remaining-right counter per key).
//! Output order follows the left input (first occurrence order). Output row = left row layout.
//! `rescan(params)` restarts both children and clears state.
use pgarbor_common::{PgError, Result};
use pgarbor_plan::plan::{DistinctNode, PlanNode, SetOpNode};

use crate::build::BuildCtx;
use crate::runtime::RowSource;

#[allow(unused_variables)]
pub fn build_distinct(node: &PlanNode, n: &DistinctNode, b: &mut BuildCtx) -> Result<Box<dyn RowSource>> {
    Err(PgError::unimplemented("EXE-24", "Distinct node (node_setops::build_distinct)"))
}

#[allow(unused_variables)]
pub fn build_setop(node: &PlanNode, n: &SetOpNode, b: &mut BuildCtx) -> Result<Box<dyn RowSource>> {
    Err(PgError::unimplemented("EXE-24", "SetOp node (node_setops::build_setop)"))
}
