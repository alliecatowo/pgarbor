//! [`CatalogCache`]: shared cache of built [`CatalogSnapshot`]s keyed by [`CatalogVersion`]
//! (CAT-12 semantics; fully implemented, frozen v1).
//!
//! ## Why keying by version is correct
//! Every catalog-writing statement bumps `cat:version` = (counter, stamp) in the SAME transaction
//! that writes the rows (`CatalogTxn::finish_statement`), and the stamp is process-unique and
//! random. So the pair identifies catalog CONTENT, not a place: two branches forked at the same
//! state read the same pair and may share one snapshot; a branch reset or rollback can never
//! revisit a pair with different contents; an UNCOMMITTED transaction's catalog (its version
//! bump is in its overlay) has a pair no other transaction can see, so it never poisons the
//! shared entries (it merely occupies one slot until evicted).
//!
//! ## Protocol (what the `pgarbor` session does at every statement start)
//! 1. `let snap = cache.snapshot(&txn_kv)?` - one point read of `cat:version`; cache hit = `Arc`
//!    clone; miss = `CatalogSnapshot::load` (full scan of space 1) then insert.
//! 2. If `snap.ptr_eq(&session.snapshot)` nothing changed; otherwise rebuild the session's
//!    `CatalogEnv` (`env.with_snapshot(snap)`; re-resolve search_path when `Invalidation::schemas`),
//!    and revalidate prepared statements with `invalidation::plan_is_stale` (CAT-12).
//! 3. A session never swaps snapshots in the middle of a statement.
//! Version `ZERO` (branch not bootstrapped) is never cached.
use crate::persist::{self, CatalogVersion};
use crate::snapshot::CatalogSnapshot;
use pgarbor_common::{KvRead, Result};
use std::sync::Mutex;

/// Default number of cached snapshots (committed heads of live branches + a few in-flight txns).
pub const DEFAULT_CAPACITY: usize = 16;

/// Thread-safe LRU of snapshots by version. Cheap to share behind an `Arc`.
// CONTRACT
#[derive(Debug)]
pub struct CatalogCache {
    capacity: usize,
    /// Most recently used LAST.
    entries: Mutex<Vec<(CatalogVersion, CatalogSnapshot)>>,
}

impl Default for CatalogCache {
    fn default() -> CatalogCache {
        CatalogCache::with_capacity(DEFAULT_CAPACITY)
    }
}

impl CatalogCache {
    pub fn with_capacity(capacity: usize) -> CatalogCache {
        CatalogCache { capacity: capacity.max(1), entries: Mutex::new(Vec::new()) }
    }

    /// Snapshot of the catalog visible through `kv` (module docs, step 1).
    pub fn snapshot(&self, kv: &dyn KvRead) -> Result<CatalogSnapshot> {
        let version = persist::read_version(kv)?;
        if version != CatalogVersion::ZERO {
            if let Some(s) = self.get(version) {
                return Ok(s);
            }
        }
        let snap = CatalogSnapshot::load(kv)?;
        if snap.version() != CatalogVersion::ZERO {
            self.insert(snap.clone());
        }
        Ok(snap)
    }

    /// Cached snapshot for `version` (marks it most recently used).
    pub fn get(&self, version: CatalogVersion) -> Option<CatalogSnapshot> {
        let mut e = self.entries.lock().unwrap_or_else(|p| p.into_inner());
        let pos = e.iter().position(|(v, _)| *v == version)?;
        let item = e.remove(pos);
        let snap = item.1.clone();
        e.push(item);
        Some(snap)
    }

    /// Insert (or refresh) a snapshot, evicting the least recently used when full. Used by
    /// `CatalogTxn` consumers that already hold a freshly built snapshot after DDL commit.
    pub fn insert(&self, snap: CatalogSnapshot) {
        let v = snap.version();
        if v == CatalogVersion::ZERO {
            return;
        }
        let mut e = self.entries.lock().unwrap_or_else(|p| p.into_inner());
        e.retain(|(ev, _)| *ev != v);
        e.push((v, snap));
        while e.len() > self.capacity {
            e.remove(0);
        }
    }

    /// Drop everything (after `reset_branch`/restore the pairs stay valid, so this is only needed
    /// to release memory or in tests).
    pub fn clear(&self) {
        self.entries.lock().unwrap_or_else(|p| p.into_inner()).clear();
    }

    pub fn len(&self) -> usize {
        self.entries.lock().unwrap_or_else(|p| p.into_inner()).len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}
