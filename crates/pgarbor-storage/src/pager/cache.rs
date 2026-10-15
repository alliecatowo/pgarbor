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
use pgarbor_common::{PageId, PgError, Result};
use std::sync::atomic::AtomicU64;
use std::sync::Mutex;

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

    /// Look up a page. Returns `Ok(None)` on miss. `Err` never happens for valid use; it exists
    /// so the body can be leased.
    #[allow(unused_variables)]
    pub fn get(&self, id: PageId) -> Result<Option<PageRef>> {
        Err(PgError::unimplemented("STO-03", "page cache lookup"))
    }

    /// Insert (or replace) a committed page, evicting by CLOCK if the shard is full.
    #[allow(unused_variables)]
    pub fn insert(&self, page: PageRef) -> Result<()> {
        Err(PgError::unimplemented("STO-03", "page cache insert"))
    }

    /// Drop every entry (stats kept).
    pub fn clear(&self) -> Result<()> {
        Err(PgError::unimplemented("STO-03", "page cache clear"))
    }

    /// Snapshot of counters; `resident` = total entries over all shards.
    pub fn stats(&self) -> Result<CacheStats> {
        Err(PgError::unimplemented("STO-03", "page cache stats"))
    }
}
