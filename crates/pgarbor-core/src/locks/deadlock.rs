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
use crate::locks::table::{LockScope, LockTable};
use crate::locks::tag::LockTag;
use crate::types::SessionId;
use pgarbor_common::{PgError, Result, TxnId};
use std::collections::{HashMap, HashSet};

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

fn scope_txn(scope: &LockScope) -> Option<TxnId> {
    match scope {
        LockScope::Transaction { txn, .. } => Some(*txn),
        LockScope::Session => None,
    }
}

/// All edges of the current table (module docs), ordered by (waiter, queue position, blocker).
pub fn wait_for_edges(table: &LockTable) -> Result<Vec<WaitForEdge>> {
    // (waiter, queue position, blocker, is_soft, edge)
    let mut keyed: Vec<(SessionId, usize, SessionId, bool, WaitForEdge)> = Vec::new();
    for (tag, entry) in &table.entries {
        for (i, w) in entry.waiters.iter().enumerate() {
            for g in &entry.granted {
                if g.session != w.session && w.mode.conflicts_with(g.mode)? {
                    keyed.push((
                        w.session,
                        i,
                        g.session,
                        false,
                        WaitForEdge {
                            waiter: w.session,
                            blocker: g.session,
                            tag: tag.clone(),
                            mode: w.mode,
                            blocker_txn: scope_txn(&g.scope),
                        },
                    ));
                }
            }
            for ahead in entry.waiters.iter().take(i) {
                if ahead.session != w.session && w.mode.conflicts_with(ahead.mode)? {
                    keyed.push((
                        w.session,
                        i,
                        ahead.session,
                        true,
                        WaitForEdge {
                            waiter: w.session,
                            blocker: ahead.session,
                            tag: tag.clone(),
                            mode: w.mode,
                            blocker_txn: scope_txn(&ahead.scope),
                        },
                    ));
                }
            }
        }
    }
    // Hard edges sort before soft ones for the same blocker; duplicates (same waiter, position,
    // blocker) are collapsed keeping the first.
    keyed.sort_by(|a, b| (a.0, a.1, a.2, a.3).cmp(&(b.0, b.1, b.2, b.3)));
    let mut out: Vec<WaitForEdge> = Vec::with_capacity(keyed.len());
    let mut last: Option<(SessionId, usize, SessionId)> = None;
    for (waiter, pos, blocker, _soft, edge) in keyed {
        if last == Some((waiter, pos, blocker)) {
            continue;
        }
        last = Some((waiter, pos, blocker));
        out.push(edge);
    }
    Ok(out)
}

/// Find a cycle through `start` by DFS over `edges` (iterative, visited set; graph size is bounded
/// by sessions). Returns the cycle's edges in order, beginning with an edge whose `waiter ==
/// start` and ending with an edge whose `blocker == start`; `None` if no cycle includes `start`.
/// Cycles not involving `start` are ignored (another waiter's check will find them).
pub fn find_cycle(edges: &[WaitForEdge], start: SessionId) -> Result<Option<Vec<WaitForEdge>>> {
    let mut adj: HashMap<SessionId, Vec<usize>> = HashMap::new();
    for (i, e) in edges.iter().enumerate() {
        adj.entry(e.waiter).or_default().push(i);
    }
    let mut visited: HashSet<SessionId> = HashSet::new();
    visited.insert(start);
    // Each frame: (node, next adjacency index). `path` holds the edge indices taken so far, so
    // path.len() == stack.len() - 1.
    let mut stack: Vec<(SessionId, usize)> = vec![(start, 0)];
    let mut path: Vec<usize> = Vec::new();
    while let Some(frame) = stack.last_mut() {
        let node = frame.0;
        let next = frame.1;
        frame.1 += 1;
        let out: &[usize] = adj.get(&node).map(|v| v.as_slice()).unwrap_or(&[]);
        if next >= out.len() {
            stack.pop();
            path.pop();
            continue;
        }
        let ei = out[next];
        let e = &edges[ei];
        if e.blocker == start {
            let mut cycle: Vec<WaitForEdge> = path.iter().map(|&p| edges[p].clone()).collect();
            cycle.push(e.clone());
            return Ok(Some(cycle));
        }
        if visited.insert(e.blocker) {
            path.push(ei);
            stack.push((e.blocker, 0));
        }
    }
    Ok(None)
}

/// Wait description used in the 40P01 detail (deadlock.c DeadLockReport, oracle-verified):
/// - Key tag with `blocker_txn = Some(x)` → `ShareLock on transaction <x>` (PG row-lock waits are
///   waits on the holder's transaction id);
/// - otherwise `<mode.pg_name()> on <tag.describe()>`, e.g.
///   `AccessExclusiveLock on relation 16384 of database 5`,
///   `ExclusiveLock on advisory lock [5,0,1,1]`.
pub fn describe_wait(edge: &WaitForEdge) -> Result<String> {
    if let (LockTag::Key { .. }, Some(txn)) = (&edge.tag, edge.blocker_txn) {
        return Ok(format!("ShareLock on transaction {}", txn.0));
    }
    Ok(format!("{} on {}", edge.mode.pg_name()?, edge.tag.describe()?))
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::locks::table::{Grant, LockEntry, WaitSlot, Waiter};
    use crate::types::SubXactId;
    use pgarbor_common::BranchId;
    use std::time::Instant;

    fn s(n: i32) -> SessionId {
        SessionId(n)
    }
    fn rel(r: u32) -> LockTag {
        LockTag::Relation { branch: BranchId([1; 16]), database: 5, relid: r }
    }
    fn key(k: u8) -> LockTag {
        LockTag::Key { branch: BranchId([1; 16]), database: 5, key: vec![0, 0, 0x40, 0, k] }
    }
    fn scope(t: u64) -> LockScope {
        LockScope::Transaction { txn: TxnId(t), subxact: SubXactId(0) }
    }
    fn grant(sess: i32, t: u64, mode: LockMode) -> Grant {
        Grant { session: s(sess), scope: scope(t), mode, count: 1, granted_unix_ms: 0 }
    }
    fn waiter(sess: i32, t: u64, mode: LockMode) -> Waiter {
        Waiter { session: s(sess), scope: scope(t), mode, slot: WaitSlot::new(), since: Instant::now(), since_unix_ms: 0 }
    }
    fn edge(w: i32, b: i32) -> WaitForEdge {
        WaitForEdge { waiter: s(w), blocker: s(b), tag: rel(16384), mode: LockMode::AccessExclusive, blocker_txn: None }
    }

    #[test]
    fn two_cycle_from_table() {
        let mut t = LockTable::default();
        let mut e1 = LockEntry::default();
        e1.granted.push(grant(1, 100, LockMode::ForUpdate));
        e1.waiters.push_back(waiter(2, 200, LockMode::ForUpdate));
        let mut e2 = LockEntry::default();
        e2.granted.push(grant(2, 200, LockMode::ForUpdate));
        e2.waiters.push_back(waiter(1, 100, LockMode::ForUpdate));
        t.entries.insert(key(1), e1);
        t.entries.insert(key(2), e2);
        let edges = wait_for_edges(&t).unwrap();
        assert_eq!(edges.len(), 2);
        assert_eq!(edges[0].waiter, s(1));
        assert_eq!(edges[0].blocker, s(2));
        assert_eq!(edges[0].blocker_txn, Some(TxnId(200)));
        let cyc = find_cycle(&edges, s(1)).unwrap().unwrap();
        assert_eq!(cyc.len(), 2);
    }

    #[test]
    fn three_cycle() {
        let edges = vec![edge(1, 2), edge(2, 3), edge(3, 1)];
        let c = find_cycle(&edges, s(1)).unwrap().unwrap();
        assert_eq!(c.len(), 3);
        assert_eq!(c[0].waiter, s(1));
        assert_eq!(c[2].blocker, s(1));
    }

    #[test]
    fn cycle_not_through_start() {
        let edges = vec![edge(1, 2), edge(2, 3), edge(3, 2)];
        assert!(find_cycle(&edges, s(1)).unwrap().is_none());
        assert!(find_cycle(&edges, s(2)).unwrap().is_some());
    }

    #[test]
    fn soft_edge_cycle() {
        // s3's AccessShare queued behind s2's AccessExclusive: soft edge 3 -> 2.
        let mut t = LockTable::default();
        let mut e = LockEntry::default();
        e.granted.push(grant(1, 100, LockMode::AccessShare));
        e.waiters.push_back(waiter(2, 200, LockMode::AccessExclusive));
        e.waiters.push_back(waiter(3, 300, LockMode::AccessShare));
        t.entries.insert(rel(16384), e);
        let mut e2 = LockEntry::default();
        e2.granted.push(grant(3, 300, LockMode::AccessExclusive));
        e2.waiters.push_back(waiter(1, 100, LockMode::AccessShare));
        t.entries.insert(rel(16385), e2);
        let edges = wait_for_edges(&t).unwrap();
        assert!(edges.iter().any(|e| e.waiter == s(3) && e.blocker == s(2)));
        // 1 -> 3 (hard on 16385), 3 -> 2 (soft), 2 -> 1 (hard)
        let c = find_cycle(&edges, s(1)).unwrap().unwrap();
        assert_eq!(c.len(), 3);
    }

    #[test]
    fn describe() {
        let mut e = edge(1, 2);
        e.tag = key(1);
        e.mode = LockMode::ForUpdate;
        e.blocker_txn = Some(TxnId(808));
        assert_eq!(describe_wait(&e).unwrap(), "ShareLock on transaction 808");
        let r = edge(1, 2);
        assert_eq!(describe_wait(&r).unwrap(), "AccessExclusiveLock on relation 16384 of database 5");
    }

    #[test]
    fn report_two_lines() {
        let mut a = edge(315, 313);
        a.tag = key(1);
        a.mode = LockMode::ForUpdate;
        a.blocker_txn = Some(TxnId(808));
        let mut b = edge(313, 315);
        b.tag = key(2);
        b.mode = LockMode::ForUpdate;
        b.blocker_txn = Some(TxnId(809));
        let err = deadlock_error(&[a, b]).unwrap();
        let text = format!("{:?}", err);
        assert!(text.contains("Process 315 waits for ShareLock on transaction 808; blocked by process 313."));
        assert!(text.contains("Process 313 waits for ShareLock on transaction 809; blocked by process 315."));
    }
}
