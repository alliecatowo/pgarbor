//! Shared page cache (sharded CLOCK). Lease STO-03.
//!
//! Committed pages are immutable for the life of a [`crate::Pager`] (append-only format; compaction
//! creates a new Pager with a new cache), so the cache NEVER needs invalidation. It must never hold
//! a private (uncommitted) page: `WriteTxn` keeps those in its own dirty map and only inserts pages
//! after its commit is durable.
//!
//! Design (CONTRACT for behavior, internals free):
//! - `SHARDS` = 16 shards, shard = `(id.0 as usize) % SHARDS`; each shard is a
//!   `Mutex<Shard>` holding `HashMap<u64, usize /*slot*/>` + a slot ring `Vec<Slot { id, page:
//!   PageRef, referenced: bool }>` with a clock hand. Per-shard capacity =
//!   `max(1, capacity_pages.div_ceil(SHARDS))`.
//! - `get`: hit → set `referenced = true`, clone the Arc, count a hit; miss → count a miss.
//! - `insert`: if present, replace page; else if shard not full push; else run CLOCK: advance the
//!   hand, clearing `referenced` bits, evict the first slot with `referenced == false`, count an
//!   eviction. New entries start with `referenced = false`.
//! - `capacity_pages == 0` disables caching (get always misses, insert drops).
//! - Poisoned mutexes: recover the inner value (`into_inner` on PoisonError) — a cache is never
//!   a correctness boundary.
//! - Stats counters are `AtomicU64` (Relaxed).
use crate::format::page::PageRef;
use pgarbor_common::{PageId, Result};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, MutexGuard};

pub const SHARDS: usize = 16;

// CONTRACT
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, serde::Serialize)]
pub struct CacheStats {
    pub hits: u64,
    pub misses: u64,
    pub inserts: u64,
    pub evictions: u64,
    pub resident: u64,
    pub capacity: u64,
}

/// One cache slot (internal; worker may restructure but must keep the struct names used here).
#[derive(Debug)]
pub(crate) struct Slot {
    pub(crate) id: u64,
    pub(crate) page: PageRef,
    pub(crate) referenced: bool,
}

#[derive(Debug, Default)]
pub(crate) struct Shard {
    pub(crate) index: std::collections::HashMap<u64, usize>,
    pub(crate) slots: Vec<Slot>,
    pub(crate) hand: usize,
    pub(crate) capacity: usize,
}

impl Shard {
    /// Insert `page` under `id` into this shard, evicting by CLOCK when full.
    /// Returns `true` if an existing entry was evicted to make room.
    fn insert(&mut self, id: u64, page: PageRef) -> bool {
        if self.capacity == 0 {
            return false;
        }
        if let Some(&slot_idx) = self.index.get(&id) {
            // Replace the page in place; the slot keeps its referenced bit.
            self.slots[slot_idx].page = page;
            return false;
        }
        if self.slots.len() < self.capacity {
            let idx = self.slots.len();
            self.slots.push(Slot { id, page, referenced: false });
            self.index.insert(id, idx);
            return false;
        }
        // CLOCK sweep. Every referenced slot is cleared at most once per sweep, so at most
        // `2 * capacity` steps are needed before an unreferenced victim is found.
        let len = self.slots.len();
        loop {
            let idx = self.hand;
            self.hand = if idx + 1 == len { 0 } else { idx + 1 };
            let slot = &mut self.slots[idx];
            if slot.referenced {
                slot.referenced = false;
                continue;
            }
            let old_id = slot.id;
            *slot = Slot { id, page, referenced: false };
            self.index.remove(&old_id);
            self.index.insert(id, idx);
            return true;
        }
    }
}

// CONTRACT
/// Thread-safe page cache. `Send + Sync`.
#[derive(Debug)]
pub struct PageCache {
    pub(crate) shards: Vec<Mutex<Shard>>,
    pub(crate) capacity: usize,
    pub(crate) hits: AtomicU64,
    pub(crate) misses: AtomicU64,
    pub(crate) inserts: AtomicU64,
    pub(crate) evictions: AtomicU64,
}

fn lock_shard(m: &Mutex<Shard>) -> MutexGuard<'_, Shard> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

impl PageCache {
    /// Cache holding about `capacity_pages` pages in total.
    pub fn new(capacity_pages: usize) -> PageCache {
        let per = if capacity_pages == 0 { 0 } else { capacity_pages.div_ceil(SHARDS).max(1) };
        PageCache {
            shards: (0..SHARDS).map(|_| Mutex::new(Shard { capacity: per, ..Shard::default() })).collect(),
            capacity: capacity_pages,
            hits: AtomicU64::new(0),
            misses: AtomicU64::new(0),
            inserts: AtomicU64::new(0),
            evictions: AtomicU64::new(0),
        }
    }

    pub fn capacity(&self) -> usize {
        self.capacity
    }

    #[inline]
    fn shard_for(&self, id: PageId) -> &Mutex<Shard> {
        &self.shards[(id.0 as usize) % SHARDS]
    }

    /// Look up a page. Returns `Ok(None)` on miss. `Err` never happens for valid use; it exists
    /// so the body can be leased.
    pub fn get(&self, id: PageId) -> Result<Option<PageRef>> {
        let mut shard = lock_shard(self.shard_for(id));
        let found = shard.index.get(&id.0).copied();
        if let Some(slot_idx) = found {
            let slot = &mut shard.slots[slot_idx];
            slot.referenced = true;
            let page = slot.page.clone();
            drop(shard);
            self.hits.fetch_add(1, Ordering::Relaxed);
            Ok(Some(page))
        } else {
            drop(shard);
            self.misses.fetch_add(1, Ordering::Relaxed);
            Ok(None)
        }
    }

    /// Insert (or replace) a committed page, evicting by CLOCK if the shard is full.
    pub fn insert(&self, page: PageRef) -> Result<()> {
        if self.capacity == 0 {
            // Caching is disabled: drop the page without touching any counter.
            return Ok(());
        }
        let id = page.id.0;
        let evicted = {
            let mut shard = lock_shard(self.shard_for(page.id));
            shard.insert(id, page)
        };
        self.inserts.fetch_add(1, Ordering::Relaxed);
        if evicted {
            self.evictions.fetch_add(1, Ordering::Relaxed);
        }
        Ok(())
    }

    /// Drop every entry (stats kept).
    pub fn clear(&self) -> Result<()> {
        for m in &self.shards {
            let mut shard = lock_shard(m);
            shard.index.clear();
            shard.slots.clear();
            shard.hand = 0;
        }
        Ok(())
    }

    /// Snapshot of counters; `resident` = total entries over all shards.
    pub fn stats(&self) -> Result<CacheStats> {
        let mut resident: u64 = 0;
        for m in &self.shards {
            resident += lock_shard(m).slots.len() as u64;
        }
        Ok(CacheStats {
            hits: self.hits.load(Ordering::Relaxed),
            misses: self.misses.load(Ordering::Relaxed),
            inserts: self.inserts.load(Ordering::Relaxed),
            evictions: self.evictions.load(Ordering::Relaxed),
            resident,
            capacity: self.capacity as u64,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::format::consts::PageKind;
    use crate::format::page::Page;
    use pgarbor_common::CommitSeq;
    use std::sync::Arc;

    fn page(id: u64) -> PageRef {
        Arc::new(Page {
            id: PageId(id),
            kind: PageKind::Leaf,
            commit_seq: CommitSeq(1),
            payload: id.to_le_bytes().to_vec(),
        })
    }

    /// Ids that all land in shard 0 (id % 16 == 0), non-zero.
    fn shard0(n: u64) -> Vec<u64> {
        (1..=n).map(|i| i * SHARDS as u64).collect()
    }

    #[test]
    fn hit_and_miss_counting() {
        let c = PageCache::new(64);
        assert!(c.get(PageId(5)).unwrap().is_none());
        let p = page(5);
        c.insert(p.clone()).unwrap();
        let got = c.get(PageId(5)).unwrap().expect("hit after insert");
        assert!(Arc::ptr_eq(&got, &p));
        assert!(c.get(PageId(6)).unwrap().is_none());
        let s = c.stats().unwrap();
        assert_eq!((s.hits, s.misses, s.inserts, s.evictions), (1, 2, 1, 0));
        assert_eq!(s.resident, 1);
        assert_eq!(s.capacity, 64);
    }

    #[test]
    fn insert_replaces_existing_entry() {
        let c = PageCache::new(64);
        c.insert(page(7)).unwrap();
        let newer = page(7);
        c.insert(newer.clone()).unwrap();
        let got = c.get(PageId(7)).unwrap().unwrap();
        assert!(Arc::ptr_eq(&got, &newer));
        let s = c.stats().unwrap();
        assert_eq!(s.resident, 1);
        assert_eq!(s.inserts, 2);
        assert_eq!(s.evictions, 0);
    }

    #[test]
    fn capacity_zero_disables_caching() {
        let c = PageCache::new(0);
        c.insert(page(1)).unwrap();
        assert!(c.get(PageId(1)).unwrap().is_none());
        let s = c.stats().unwrap();
        assert_eq!(s.resident, 0);
        assert_eq!(s.inserts, 0);
        assert_eq!(s.misses, 1);
        assert_eq!(s.capacity, 0);
    }

    #[test]
    fn clear_drops_entries_and_keeps_stats() {
        let c = PageCache::new(64);
        c.insert(page(1)).unwrap();
        c.insert(page(2)).unwrap();
        assert!(c.get(PageId(1)).unwrap().is_some());
        c.clear().unwrap();
        assert!(c.get(PageId(1)).unwrap().is_none());
        let s = c.stats().unwrap();
        assert_eq!(s.resident, 0);
        assert_eq!(s.hits, 1);
        assert_eq!(s.misses, 1);
        assert_eq!(s.inserts, 2);
    }

    #[test]
    fn capacity_bound_never_exceeded() {
        // per-shard capacity = ceil(40 / 16) = 3, total bound = 48
        let c = PageCache::new(40);
        for id in 1..=1000u64 {
            c.insert(page(id)).unwrap();
            for m in &c.shards {
                assert!(lock_shard(m).slots.len() <= 3);
            }
        }
        let s = c.stats().unwrap();
        assert!(s.resident <= 48);
        assert_eq!(s.resident + s.evictions, s.inserts);
        assert_eq!(s.inserts, 1000);
    }

    #[test]
    fn referenced_page_survives_one_sweep_unreferenced_is_evicted() {
        // per-shard capacity = 4; all ids below map to shard 0.
        let c = PageCache::new(SHARDS * 4);
        let ids = shard0(5); // 16, 32, 48, 64, 80
        for &id in &ids[..4] {
            c.insert(page(id)).unwrap();
        }
        // Re-reference the first page only.
        assert!(c.get(PageId(ids[0])).unwrap().is_some());
        // Inserting a fifth page sweeps: slot 0 is referenced (cleared, survives),
        // slot 1 (id 32) is unreferenced and gets evicted.
        c.insert(page(ids[4])).unwrap();
        assert!(c.get(PageId(ids[0])).unwrap().is_some(), "re-referenced page must survive");
        assert!(c.get(PageId(ids[1])).unwrap().is_none(), "unreferenced page must be evicted");
        assert!(c.get(PageId(ids[2])).unwrap().is_some());
        assert!(c.get(PageId(ids[3])).unwrap().is_some());
        assert!(c.get(PageId(ids[4])).unwrap().is_some());
        let s = c.stats().unwrap();
        assert_eq!(s.evictions, 1);
        assert_eq!(s.resident, 4);
    }

    #[test]
    fn concurrent_get_insert_from_8_threads() {
        let c = PageCache::new(128);
        std::thread::scope(|scope| {
            for t in 0..8u64 {
                let c = &c;
                scope.spawn(move || {
                    for i in 0..2000u64 {
                        let id = (t * 7919 + i * 31) % 512 + 1;
                        if i % 3 == 0 {
                            c.insert(page(id)).unwrap();
                        } else if let Some(p) = c.get(PageId(id)).unwrap() {
                            assert_eq!(p.id, PageId(id));
                        }
                    }
                });
            }
        });
        let s = c.stats().unwrap();
        // Replacement inserts count as inserts but add no entry, so only the bound is exact here.
        assert!(s.resident <= 128);
        assert!(s.evictions <= s.inserts);
        assert_eq!(s.inserts, 8 * 667); // i % 3 == 0 for i in 0..2000 is 667 values per thread
    }
}
