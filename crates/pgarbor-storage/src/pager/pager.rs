//! File-backed pager: verified page reads through the cache, coalesced page writes, superblock
//! slot I/O, sync. Lease STO-04 (STORE-03 extent writes with bounds checks).
//!
//! The Pager does NOT know which pages are committed; bound checks against `page_count` and
//! commit_seq checks are done by its callers (`Snapshot`, `WriteTxn`) via the `PageSource` impls.
//! It is `Send + Sync`; all methods take `&self`.
#[allow(unused_imports)]
use crate::error;
use crate::format::consts::*;
use crate::format::page::{Page, PageRef};
use crate::io::FileIo;
use crate::pager::cache::{CacheStats, PageCache};
use pgarbor_common::{PageId, PgError, Result};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

// CONTRACT
/// I/O counters (Relaxed atomics).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, serde::Serialize)]
pub struct PagerStats {
    pub page_reads: u64,
    pub page_writes: u64,
    pub bytes_read: u64,
    pub bytes_written: u64,
    pub syncs: u64,
    pub cache: CacheStats,
}

// CONTRACT
#[derive(Debug)]
pub struct Pager {
    file: Box<dyn FileIo>,
    path: PathBuf,
    page_size: u32,
    cache: PageCache,
    page_reads: AtomicU64,
    page_writes: AtomicU64,
    bytes_read: AtomicU64,
    bytes_written: AtomicU64,
    syncs: AtomicU64,
}

impl Pager {
    /// Wrap an open file. `path` is used only in error messages. `page_size` must already be
    /// validated (`is_valid_page_size`).
    pub fn new(file: Box<dyn FileIo>, path: PathBuf, page_size: u32, cache_pages: usize) -> Pager {
        Pager {
            file,
            path,
            page_size,
            cache: PageCache::new(cache_pages),
            page_reads: AtomicU64::new(0),
            page_writes: AtomicU64::new(0),
            bytes_read: AtomicU64::new(0),
            bytes_written: AtomicU64::new(0),
            syncs: AtomicU64::new(0),
        }
    }
    pub fn page_size(&self) -> u32 {
        self.page_size
    }
    pub fn path(&self) -> &Path {
        &self.path
    }
    pub fn file(&self) -> &dyn FileIo {
        self.file.as_ref()
    }
    pub fn cache(&self) -> &PageCache {
        &self.cache
    }
    /// Give back the file handle (used by compaction when swapping files).
    pub fn into_file(self) -> Box<dyn FileIo> {
        self.file
    }
    pub(crate) fn count_sync(&self) {
        self.syncs.fetch_add(1, Ordering::Relaxed);
    }

    /// Read + verify page `id`, via the cache. Steps: `id` NONE → `error::invalid_page(id,
    /// "null page reference")`; cache hit → return; else read exactly `page_size` bytes at
    /// `page_offset(page_size, id)` (`read_exact_at`; UnexpectedEof → `error::invalid_page(id,
    /// "page lies beyond end of file")`, other io errors → `error::read_failed(path, e)`), decode
    /// with `format::page::decode_page(page_size, id, &buf)`, insert `Arc` into cache, bump
    /// counters, return.
    #[allow(unused_variables)]
    pub fn read_page(&self, id: PageId) -> Result<PageRef> {
        Err(PgError::unimplemented("STO-04", "pager page read"))
    }

    /// Same as `read_page` but bypasses (neither reads nor fills) the cache. Used by doctor
    /// full scans and by compaction reading pages once.
    #[allow(unused_variables)]
    pub fn read_page_uncached(&self, id: PageId) -> Result<Page> {
        Err(PgError::unimplemented("STO-04", "pager uncached page read"))
    }

    /// Raw `page_size` bytes of page `id`, no verification (doctor/inspect). Short file → the
    /// missing tail is zero-filled (not an error).
    #[allow(unused_variables)]
    pub fn read_raw(&self, id: PageId) -> Result<Vec<u8>> {
        Err(PgError::unimplemented("STO-04", "pager raw page read"))
    }

    /// Write fully-encoded pages (`(id, bytes)` with `bytes.len() == page_size`, produced by
    /// `encode_page`). Sort by id and coalesce runs of consecutive ids into one `write_all_at`
    /// of up to 1 MiB each. Does NOT sync and does NOT touch the cache. Errors:
    /// `error::write_failed(path, e)` (ENOSPC → 53100). A wrong-length buffer → `PgError::internal`.
    #[allow(unused_variables)]
    pub fn write_pages(&self, pages: &mut [(PageId, Vec<u8>)]) -> Result<()> {
        Err(PgError::unimplemented("STO-04", "pager page write"))
    }

    /// Read superblock slot `slot` (0/1): exactly `SUPERBLOCK_SLOT_SIZE` bytes from
    /// `slot_offset(slot)`; bytes beyond EOF are returned as zeros (so a short/new file decodes as
    /// `SlotStatus::Empty`). io errors → `error::read_failed`.
    #[allow(unused_variables)]
    pub fn read_slot(&self, slot: usize) -> Result<Vec<u8>> {
        Err(PgError::unimplemented("STO-04", "superblock slot read"))
    }

    /// Write a full 4096-byte slot image at `slot_offset(slot)` with one `write_all_at`.
    /// Wrong length → `PgError::internal`. io errors → `error::write_failed`. Does not sync.
    #[allow(unused_variables)]
    pub fn write_slot(&self, slot: usize, image: &[u8]) -> Result<()> {
        Err(PgError::unimplemented("STO-04", "superblock slot write"))
    }

    /// `file.sync_data()`; error → `error::fsync_failed(path, e)`. Counts a sync.
    pub fn sync_data(&self) -> Result<()> {
        Err(PgError::unimplemented("STO-04", "pager sync_data"))
    }

    /// `file.sync_all()`; error → `error::fsync_failed(path, e)`. Counts a sync.
    pub fn sync_all(&self) -> Result<()> {
        Err(PgError::unimplemented("STO-04", "pager sync_all"))
    }

    /// Current file length; error → `error::read_failed`.
    pub fn file_len(&self) -> Result<u64> {
        Err(PgError::unimplemented("STO-04", "pager file length"))
    }

    /// `file.set_len(len)`; error → `error::truncate_failed(path, len, e)`. Does not sync.
    #[allow(unused_variables)]
    pub fn set_len(&self, len: u64) -> Result<()> {
        Err(PgError::unimplemented("STO-04", "pager set_len"))
    }

    /// Counter snapshot including cache stats.
    pub fn stats(&self) -> Result<PagerStats> {
        Err(PgError::unimplemented("STO-04", "pager stats"))
    }
}
