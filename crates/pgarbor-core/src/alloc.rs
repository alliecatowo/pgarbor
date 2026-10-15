//! Non-transactional id allocation: row ids per table and OIDs per branch (TXN-01 support; see
//! amendment `pgarbor-core-01-id-allocation`). Lease CORE-14 (sonnet).
//!
//! Why not a counter row in the transaction overlay: every writer would key-lock the same counter
//! and two concurrent INSERTs (or CREATE TABLEs) would serialize until commit, where PostgreSQL
//! proceeds concurrently. PG's OID counter and heap TIDs are likewise non-transactional.
//!
//! Design: in-memory monotonic counters, seeded lazily from the LATEST committed head, never
//! persisted. Safe because (a) every id handed out in this process comes from the counter, so no
//! in-flight overlay holds an id above it; (b) after a restart, ids of uncommitted work are gone, so
//! "max committed + 1" never collides; (c) gaps are allowed (as in PG). Counters are per
//! (branch, database[, relid]); different branches have independent keyspaces.
//! Branch reset can install a head with LARGER ids (reset to another branch), so
//! `Engine::reset_branch` calls `forget_branch` and counters re-seed.
//!
//! - Row ids: table row keys are `[relid u32 BE][rowid u64 BE]` (ARCHITECTURE §4). Seed =
//!   `max_rowid(head tree, relid).map_or(1, |m| m + 1)`. Row ids start at 1; u64 exhaustion →
//!   54000 `row id space of relation <oid> is exhausted`.
//! - OIDs: seed supplied by the caller (`seed(&dyn KvRead) -> Result<Oid>` — pgarbor-catalog knows
//!   its key layout; core does not), clamped to >= `FIRST_NORMAL_OID`. Exhaustion (u32::MAX) →
//!   54000 `OID counter exhausted in branch "<name>"` (PG wraps around and probes for unused OIDs;
//!   documented gap).
use crate::txn::Txn;
use pgarbor_common::{BranchId, KvRead, Oid, PgError, RelId, Result, SqlState, FIRST_NORMAL_OID};
use std::collections::HashMap;
use std::ops::{Bound, Range};
use std::sync::Mutex;

// CONTRACT
#[derive(Debug, Default)]
pub struct IdAllocator {
    /// (branch, database, relid) → next row id to hand out.
    pub(crate) rowids: Mutex<HashMap<(BranchId, Oid, RelId), u64>>,
    /// (branch, database) → next OID to hand out.
    pub(crate) oids: Mutex<HashMap<(BranchId, Oid), Oid>>,
}

impl IdAllocator {
    pub fn new() -> IdAllocator {
        IdAllocator::default()
    }

    /// Forget every counter of `branch` (after reset/delete/restore).
    pub fn forget_branch(&self, branch: &BranchId) {
        self.rowids.lock().unwrap_or_else(|e| e.into_inner()).retain(|k, _| k.0 != *branch);
        self.oids.lock().unwrap_or_else(|e| e.into_inner()).retain(|k, _| k.0 != *branch);
    }
}

/// Next row id for `relid` in the txn's branch/database (seeding as in the module docs, using
/// `txn.engine().branch_head(..)` — the latest head, not the txn snapshot).
pub fn next_rowid(txn: &Txn, relid: RelId) -> Result<u64> {
    Ok(next_rowids(txn, relid, 1)?.start)
}

/// `n` consecutive row ids (bulk INSERT / COPY); `n == 0` → empty range.
pub fn next_rowids(txn: &Txn, relid: RelId, n: u64) -> Result<Range<u64>> {
    if n == 0 {
        return Ok(0..0);
    }
    let engine = txn.engine();
    let ids = engine.ids();
    let key = (txn.branch(), txn.database(), relid);
    // The mutex is held while seeding so two sessions never seed the same counter twice.
    let mut map = ids.rowids.lock().unwrap_or_else(|e| e.into_inner());
    let start = match map.get(&key) {
        Some(v) => *v,
        None => {
            let head = engine.branch_head(&key.0, key.1)?;
            match max_rowid(&head.tree, relid)? {
                None => 1,
                Some(m) => m.checked_add(1).ok_or_else(|| exhausted_rowids(relid))?,
            }
        }
    };
    let end = start.checked_add(n).ok_or_else(|| exhausted_rowids(relid))?;
    map.insert(key, end);
    Ok(start..end)
}

fn exhausted_rowids(relid: RelId) -> PgError {
    PgError::new(
        SqlState::PROGRAM_LIMIT_EXCEEDED,
        format!("row id space of relation {relid} is exhausted"),
    )
}

/// Next OID for the txn's branch/database; `seed` is called once per (branch, database) per
/// process with the latest head tree and must return the smallest OID not used by any catalog row.
pub fn next_oid(txn: &Txn, seed: &dyn Fn(&dyn KvRead) -> Result<Oid>) -> Result<Oid> {
    let engine = txn.engine();
    let ids = engine.ids();
    let key = (txn.branch(), txn.database());
    let mut map = ids.oids.lock().unwrap_or_else(|e| e.into_inner());
    let cur = match map.get(&key) {
        Some(v) => *v,
        None => {
            let head = engine.branch_head(&key.0, key.1)?;
            let tree: &dyn KvRead = &head.tree;
            seed(tree)?.max(FIRST_NORMAL_OID)
        }
    };
    if cur == Oid::MAX {
        return Err(PgError::new(
            SqlState::PROGRAM_LIMIT_EXCEEDED,
            format!("OID counter exhausted in branch \"{}\"", txn.branch_name()),
        ));
    }
    map.insert(key, cur + 1);
    Ok(cur)
}

/// Largest row id stored under table `relid` in `tree`, or None. Without reverse iteration: binary
/// search over the 64-bit row id space with probes `tree.scan(Included(key(relid, mid)),
/// Excluded(key(relid+1, 0)))` testing whether ANY key >= mid exists (monotone predicate) — at most
/// 64 probes of O(log n) pages. `relid == u32::MAX` → upper bound `Unbounded`.
pub fn max_rowid(tree: &dyn KvRead, relid: RelId) -> Result<Option<u64>> {
    let upper_key = relid.checked_add(1).map(|r| row_key(r, 0));
    // First key >= row_key(relid, x) within this relation, if any.
    let first_at_or_after = |x: u64| -> Result<Option<Vec<u8>>> {
        let lo = row_key(relid, x);
        let upper = match &upper_key {
            Some(k) => Bound::Excluded(&k[..]),
            None => Bound::Unbounded,
        };
        let mut it = tree.scan(Bound::Included(&lo[..]), upper)?;
        match it.next() {
            Some(r) => Ok(Some(r?.0)),
            None => Ok(None),
        }
    };
    if first_at_or_after(0)?.is_none() {
        return Ok(None);
    }
    // exists(x) is true iff x <= max; find the largest such x.
    let (mut lo, mut hi) = (0u64, u64::MAX);
    while lo < hi {
        let mid = lo + (hi - lo) / 2 + 1;
        if first_at_or_after(mid)?.is_some() {
            lo = mid;
        } else {
            hi = mid - 1;
        }
    }
    match first_at_or_after(lo)? {
        Some(k) if k.len() >= 12 => {
            let mut b = [0u8; 8];
            b.copy_from_slice(&k[4..12]);
            Ok(Some(u64::from_be_bytes(b)))
        }
        _ => Ok(Some(lo)),
    }
}

/// Row key `[relid BE][rowid BE]`.
pub fn row_key(relid: RelId, rowid: u64) -> [u8; 12] {
    let mut k = [0u8; 12];
    k[0..4].copy_from_slice(&relid.to_be_bytes());
    k[4..12].copy_from_slice(&rowid.to_be_bytes());
    k
}

#[cfg(test)]
mod tests {
    use super::*;
    use pgarbor_common::{KvWrite, MemKv};

    fn put(kv: &mut MemKv, relid: RelId, rowid: u64) {
        kv.put(&row_key(relid, rowid), b"v").unwrap();
    }

    #[test]
    fn max_rowid_empty() {
        let kv = MemKv::default();
        assert_eq!(max_rowid(&kv, 16384).unwrap(), None);
    }

    #[test]
    fn max_rowid_neighbors_not_confused() {
        let mut kv = MemKv::default();
        for i in 1..=5 {
            put(&mut kv, 16384, i);
        }
        for i in 1..=100 {
            put(&mut kv, 16385, i);
        }
        assert_eq!(max_rowid(&kv, 16384).unwrap(), Some(5));
        assert_eq!(max_rowid(&kv, 16385).unwrap(), Some(100));
        assert_eq!(max_rowid(&kv, 16383).unwrap(), None);
        assert_eq!(max_rowid(&kv, 16386).unwrap(), None);
    }

    #[test]
    fn max_rowid_extremes() {
        let mut kv = MemKv::default();
        put(&mut kv, u32::MAX, 0);
        assert_eq!(max_rowid(&kv, u32::MAX).unwrap(), Some(0));
        put(&mut kv, u32::MAX, u64::MAX);
        assert_eq!(max_rowid(&kv, u32::MAX).unwrap(), Some(u64::MAX));
        put(&mut kv, 20000, 1000);
        assert_eq!(max_rowid(&kv, 20000).unwrap(), Some(1000));
    }

    #[test]
    fn row_key_layout() {
        let k = row_key(0x01020304, 0x0a0b0c0d0e0f1011);
        assert_eq!(&k[..4], &[1, 2, 3, 4]);
        assert_eq!(&k[4..], &[0x0a, 0x0b, 0x0c, 0x0d, 0x0e, 0x0f, 0x10, 0x11]);
    }

    #[test]
    fn forget_branch_clears_only_that_branch() {
        let a = IdAllocator::new();
        let b1 = BranchId([1; 16]);
        let b2 = BranchId([2; 16]);
        a.rowids.lock().unwrap().insert((b1, 1, 16384), 7);
        a.rowids.lock().unwrap().insert((b2, 1, 16384), 9);
        a.oids.lock().unwrap().insert((b1, 1), 20000);
        a.forget_branch(&b1);
        assert_eq!(a.rowids.lock().unwrap().len(), 1);
        assert!(a.oids.lock().unwrap().is_empty());
    }
}
