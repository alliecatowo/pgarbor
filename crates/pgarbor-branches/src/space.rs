//! Per-branch space accounting for `pgarbor inspect --space` and `branch list --json`
//! (RFC 001 §6 "retained bytes pinned by branches"). Lease BR-07.
use pgarbor_common::{BranchId, PgError, Result};
use pgarbor_storage::{Container, SpaceReport};

// CONTRACT
/// Space retained by one branch (all of its live roots: head, extra databases, snapshots).
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize)]
pub struct BranchSpace {
    pub id: BranchId,
    pub name: String,
    /// "active" | "deleted".
    pub state: String,
    /// Pages reachable from any of the branch's roots (shared pages counted).
    pub pages: u64,
    /// Pages reachable ONLY from this branch's roots (what deleting+purging it would free after
    /// compaction).
    pub exclusive_pages: u64,
    pub bytes: u64,
    pub exclusive_bytes: u64,
    pub snapshot_count: usize,
}

/// Compute per-branch space.
/// 1. `snap = container.snapshot()`; `report = pgarbor_storage::doctor::space_report(&snap,
///    &BranchRootResolver)` and `records = directory::list(&snap.directory(), true)` — both from the
///    SAME snapshot so they agree.
/// 2. For each record with live roots (`record.live_roots()` non-empty; dropped tombstones get
///    zeros): `R_b = gc::trace(&snap, snap.page_count(), &roots_of_b, false)?.reachable`;
///    `R_o = trace(all live roots of every OTHER record)`; `pages = R_b.len()`;
///    `exclusive_pages = |{p in R_b : !R_o.contains(p)}|` (iterate pages 1..=page_count and test
///    both `ReachSet`s). Cost O(branches x pages) — documented v1 limit.
/// 3. `bytes = pages * page_size`, `exclusive_bytes` likewise; `state` = `BranchState::as_str`.
/// 4. Sort active branches by name, then deleted ones by name.
#[allow(unused_variables)]
pub fn branch_space(container: &Container) -> Result<(Vec<BranchSpace>, SpaceReport)> {
    Err(PgError::unimplemented("BR-07", "per-branch space accounting"))
}
