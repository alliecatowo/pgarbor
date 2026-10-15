//! Hash join (lease EXE-17, north-star). PG: `nodeHashjoin.c`, `nodeHash.c`. Layout: plan.rs
//! `HashJoinNode` (INNER child = build side).
//!
//! ## Build
//! On the first `next` (and again after `rescan` if `ex.epoch()` changed since the build, otherwise
//! the table is reused): drain the inner child; for each inner row evaluate `inner_keys` over it; a row
//! with ANY NULL key can never match (`=` semantics) and is not inserted (but it is remembered for
//! `Right`/`Full` unmatched output). Hash table: `HashMap<Vec<u8>, Vec<usize>>` from the key bytes to
//! indexes into `Vec<InnerEntry{row, matched: bool}>`, where the key bytes are
//! `keycodec::encode_key_vec(env, [KeyColumn::asc(ty_i)], key values)` (equal SQL values <=> equal
//! bytes by the keycodec contract, so no separate hash/eq callbacks are needed). `outer_keys[i].ty`
//! and `inner_keys[i].ty` must be identical (the planner inserts casts); if not, XX000
//! `hash join key types differ`.
//!
//! ## Probe, per outer row `o`
//! Evaluate `outer_keys` over `o`. NULL key (any column) -> no match. Candidates = the bucket; for each
//! candidate evaluate the residual `qual` over `o ++ inner` (`eval_opt_qual`); a candidate satisfying it
//! is a match (mark `matched`).
//! * `Inner`: emit `o ++ inner` per match.   * `Left`: matches, else `o ++ NULLs` if none.
//! * `Semi`: emit `o` at the first match.   * `Anti`: emit `o` if there was no match.
//! * `Right`: matches as Inner; after the outer side is exhausted emit `NULLs ++ inner` for every inner
//!   entry (including NULL-key rows) never matched.   * `Full`: Left behavior plus the Right tail.
//! Multiple matches produce multiple output rows; emit lazily (keep a pending-output queue so `next`
//! returns one row per call). Output row = `outer ++ inner` (Semi/Anti: outer only).
//! `rescan(params)`: rescan the outer child; the build side is reused when the epoch is unchanged,
//! otherwise rebuilt after rescanning the inner child.
use pgarbor_common::{PgError, Result};
use pgarbor_plan::plan::{HashJoinNode, PlanNode};

use crate::build::BuildCtx;
use crate::runtime::RowSource;

#[allow(unused_variables)]
pub fn build_hash_join(node: &PlanNode, n: &HashJoinNode, b: &mut BuildCtx) -> Result<Box<dyn RowSource>> {
    Err(PgError::unimplemented("EXE-17", "HashJoin node (node_hashjoin::build_hash_join)"))
}
