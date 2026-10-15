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
use pgarbor_common::{BranchId, KvRead, Oid, PgError, RelId, Result};
use std::collections::HashMap;
use std::ops::Range;
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
#[allow(unused_variables)]
pub fn next_rowid(txn: &Txn, relid: RelId) -> Result<u64> {
    Err(PgError::unimplemented("CORE-14", "row id allocation"))
}

/// `n` consecutive row ids (bulk INSERT / COPY); `n == 0` → empty range.
#[allow(unused_variables)]
pub fn next_rowids(txn: &Txn, relid: RelId, n: u64) -> Result<Range<u64>> {
    Err(PgError::unimplemented("CORE-14", "bulk row id allocation"))
}

/// Next OID for the txn's branch/database; `seed` is called once per (branch, database) per
/// process with the latest head tree and must return the smallest OID not used by any catalog row.
#[allow(unused_variables)]
pub fn next_oid(txn: &Txn, seed: &dyn Fn(&dyn KvRead) -> Result<Oid>) -> Result<Oid> {
    Err(PgError::unimplemented("CORE-14", "OID allocation"))
}

/// Largest row id stored under table `relid` in `tree`, or None. Without reverse iteration: binary
/// search over the 64-bit row id space with probes `tree.scan(Included(key(relid, mid)),
/// Excluded(key(relid+1, 0)))` testing whether ANY key >= mid exists (monotone predicate) — at most
/// 64 probes of O(log n) pages. `relid == u32::MAX` → upper bound `Unbounded`.
#[allow(unused_variables)]
pub fn max_rowid(tree: &dyn KvRead, relid: RelId) -> Result<Option<u64>> {
    Err(PgError::unimplemented("CORE-14", "max row id probe"))
}

/// Row key `[relid BE][rowid BE]`.
pub fn row_key(relid: RelId, rowid: u64) -> [u8; 12] {
    let mut k = [0u8; 12];
    k[0..4].copy_from_slice(&relid.to_be_bytes());
    k[4..12].copy_from_slice(&rowid.to_be_bytes());
    k
}
