//! Wait-for graph and deadlock detection (TXN-10). Lease CORE-10. `deadlock_error` is implemented.
//!
//! Called by `LockManager::acquire` with the table mutex held, after a waiter has slept
//! `deadlock_timeout`. Graph nodes are sessions. Edges (PG deadlock.c "hard" and "soft" edges):
//! - hard: waiter W on tag T → every OTHER session holding a granted mode on T that conflicts with
//!   W's requested mode;
//! - soft: W → every other session queued AHEAD of W on T whose requested mode conflicts with W's.
//! A deadlock exists iff a cycle passes through the checking session. PG's queue rearrangement to
//! resolve soft-edge-only cycles is NOT implemented (documented gap: we report 40P01 where PG might
//! reorder the queue instead).
use crate::error;
use crate::locks::mode::LockMode;
use crate::locks::table::LockTable;
use crate::locks::tag::LockTag;
use crate::types::SessionId;
use pgarbor_common::{PgError, Result, TxnId};

// CONTRACT
/// "waiter waits for `mode` on `tag`; blocked by `blocker`".
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WaitForEdge {
    pub waiter: SessionId,
    pub blocker: SessionId,
    pub tag: LockTag,
    pub mode: LockMode,
    /// The blocker's transaction, taken from the conflicting `Grant`'s (hard edge) or queued
    /// `Waiter`'s (soft edge) `LockScope::Transaction{txn, ..}`; None for Session-scope locks.
    /// (Do NOT use `LockTable::session_txn`, which only knows sessions that have waited.)
    pub blocker_txn: Option<TxnId>,
}

/// All edges of the current table (module docs), ordered by (waiter, queue position, blocker).
#[allow(unused_variables)]
pub fn wait_for_edges(table: &LockTable) -> Result<Vec<WaitForEdge>> {
    Err(PgError::unimplemented("CORE-10", "wait-for graph construction"))
}

/// Find a cycle through `start` by DFS over `edges` (iterative, visited set; graph size is bounded
/// by sessions). Returns the cycle's edges in order, beginning with an edge whose `waiter ==
/// start` and ending with an edge whose `blocker == start`; `None` if no cycle includes `start`.
/// Cycles not involving `start` are ignored (another waiter's check will find them).
#[allow(unused_variables)]
pub fn find_cycle(edges: &[WaitForEdge], start: SessionId) -> Result<Option<Vec<WaitForEdge>>> {
    Err(PgError::unimplemented("CORE-10", "deadlock cycle search"))
}

/// Wait description used in the 40P01 detail (deadlock.c DeadLockReport, oracle-verified):
/// - Key tag with `blocker_txn = Some(x)` → `ShareLock on transaction <x>` (PG row-lock waits are
///   waits on the holder's transaction id);
/// - otherwise `<mode.pg_name()> on <tag.describe()>`, e.g.
///   `AccessExclusiveLock on relation 16384 of database 5`,
///   `ExclusiveLock on advisory lock [5,0,1,1]`.
#[allow(unused_variables)]
pub fn describe_wait(edge: &WaitForEdge) -> Result<String> {
    Err(PgError::unimplemented("CORE-10", "deadlock wait description"))
}

/// Build the 40P01 error: detail lines `Process <waiter> waits for <describe_wait>; blocked by
/// process <blocker>.` joined by `\n`, in cycle order (oracle example:
/// `Process 315 waits for ShareLock on transaction 808; blocked by process 313.\nProcess 313 waits
/// for ShareLock on transaction 809; blocked by process 315.`).
pub fn deadlock_error(cycle: &[WaitForEdge]) -> Result<PgError> {
    let mut lines = Vec::with_capacity(cycle.len());
    for e in cycle {
        lines.push(format!("Process {} waits for {}; blocked by process {}.", e.waiter, describe_wait(e)?, e.blocker));
    }
    Ok(error::deadlock_detected(lines.join("\n")))
}
