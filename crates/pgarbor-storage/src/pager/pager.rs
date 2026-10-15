//! File-backed pager: verified page reads through the cache, coalesced page writes, superblock
//! slot I/O, sync. Lease STO-04 (STORE-03 extent writes with bounds checks).
//!
//! The Pager does NOT know which pages are committed; bound checks against `page_count` and
//! commit_seq checks are done by its callers (`Snapshot`, `WriteTxn`) via the `PageSource` impls.
//! It is `Send + Sync`; all methods take `&self`.
#[allow(unused_imports)]
use crate::error;
use crate::format::consts::*;
use crate::format::page::{decode_page, Page, PageRef};
use crate::io::FileIo;
use crate::pager::cache::{CacheStats, PageCache};
use pgarbor_common::{PageId, PgError, Result};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

/// Upper bound on one coalesced write (bytes). Runs of consecutive pages are split at this size.
const MAX_WRITE_RUN_BYTES: usize = 1024 * 1024;

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

/// Fill `buf` from `offset` until it is full or EOF is reached. Returns the number of bytes
/// actually read; the caller is responsible for the zero tail (buffers are pre-zeroed).
fn read_fill_at(file: &dyn FileIo, offset: u64, buf: &mut [u8]) -> std::io::Result<usize> {
    let mut done = 0usize;
    while done < buf.len() {
        match file.read_at(offset + done as u64, &mut buf[done..]) {
            Ok(0) => break,
            Ok(n) => done += n,
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
            Err(e) => return Err(e),
        }
    }
    Ok(done)
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

    /// Disk read + verify of one page, bypassing the cache. Counts the read on success.
    fn read_verified(&self, id: PageId) -> Result<Page> {
        let ps = self.page_size as usize;
        let mut buf = vec![0u8; ps];
        if let Err(e) = self.file.read_exact_at(page_offset(self.page_size, id), &mut buf) {
            return Err(if e.kind() == std::io::ErrorKind::UnexpectedEof {
                error::invalid_page(id, "page lies beyond end of file")
            } else {
                error::read_failed(&self.path, &e)
            });
        }
        let page = decode_page(self.page_size, id, &buf)?;
        self.page_reads.fetch_add(1, Ordering::Relaxed);
        self.bytes_read.fetch_add(ps as u64, Ordering::Relaxed);
        Ok(page)
    }

    /// Read + verify page `id`, via the cache. Steps: `id` NONE → `error::invalid_page(id,
    /// "null page reference")`; cache hit → return; else read exactly `page_size` bytes at
    /// `page_offset(page_size, id)` (`read_exact_at`; UnexpectedEof → `error::invalid_page(id,
    /// "page lies beyond end of file")`, other io errors → `error::read_failed(path, e)`), decode
    /// with `format::page::decode_page(page_size, id, &buf)`, insert `Arc` into cache, bump
    /// counters, return.
    pub fn read_page(&self, id: PageId) -> Result<PageRef> {
        if id.is_none() {
            return Err(error::invalid_page(id, "null page reference"));
        }
        if let Some(hit) = self.cache.get(id)? {
            return Ok(hit);
        }
        let page: PageRef = Arc::new(self.read_verified(id)?);
        self.cache.insert(page.clone())?;
        Ok(page)
    }

    /// Same as `read_page` but bypasses (neither reads nor fills) the cache. Used by doctor
    /// full scans and by compaction reading pages once.
    pub fn read_page_uncached(&self, id: PageId) -> Result<Page> {
        if id.is_none() {
            return Err(error::invalid_page(id, "null page reference"));
        }
        self.read_verified(id)
    }

    /// Raw `page_size` bytes of page `id`, no verification (doctor/inspect). Short file → the
    /// missing tail is zero-filled (not an error).
    pub fn read_raw(&self, id: PageId) -> Result<Vec<u8>> {
        if id.is_none() {
            return Err(error::invalid_page(id, "null page reference"));
        }
        let mut buf = vec![0u8; self.page_size as usize];
        let n = read_fill_at(self.file(), page_offset(self.page_size, id), &mut buf)
            .map_err(|e| error::read_failed(&self.path, &e))?;
        self.page_reads.fetch_add(1, Ordering::Relaxed);
        self.bytes_read.fetch_add(n as u64, Ordering::Relaxed);
        Ok(buf)
    }

    /// Write fully-encoded pages (`(id, bytes)` with `bytes.len() == page_size`, produced by
    /// `encode_page`). Sort by id and coalesce runs of consecutive ids into one `write_all_at`
    /// of up to 1 MiB each. Does NOT sync and does NOT touch the cache. Errors:
    /// `error::write_failed(path, e)` (ENOSPC → 53100). A wrong-length buffer → `PgError::internal`.
    ///
    /// Validation happens before any byte is written, so a rejected batch leaves the file
    /// untouched. Counters: `page_writes` counts pages, `bytes_written` counts bytes.
    pub fn write_pages(&self, pages: &mut [(PageId, Vec<u8>)]) -> Result<()> {
        let ps = self.page_size as usize;
        for (id, bytes) in pages.iter() {
            if id.is_none() {
                return Err(PgError::internal("cannot write page with null page id"));
            }
            if bytes.len() != ps {
                return Err(PgError::internal(format!(
                    "page {} buffer is {} bytes, expected page size {}",
                    id.0,
                    bytes.len(),
                    ps
                )));
            }
        }
        pages.sort_by_key(|(id, _)| *id);
        for w in pages.windows(2) {
            if w[0].0 == w[1].0 {
                return Err(PgError::internal(format!(
                    "page {} appears twice in one write batch",
                    w[0].0 .0
                )));
            }
        }

        let max_run_pages = (MAX_WRITE_RUN_BYTES / ps).max(1);
        let mut run_buf: Vec<u8> = Vec::new();
        let mut start = 0usize;
        while start < pages.len() {
            let mut end = start + 1;
            while end < pages.len() && end - start < max_run_pages && pages[end].0 .0 == pages[end - 1].0 .0 + 1
            {
                end += 1;
            }
            let count = end - start;
            let offset = page_offset(self.page_size, pages[start].0);
            let result = if count == 1 {
                self.file.write_all_at(offset, &pages[start].1)
            } else {
                run_buf.clear();
                run_buf.reserve(count * ps);
                for (_, bytes) in &pages[start..end] {
                    run_buf.extend_from_slice(bytes);
                }
                self.file.write_all_at(offset, &run_buf)
            };
            result.map_err(|e| error::write_failed(&self.path, &e))?;
            self.page_writes.fetch_add(count as u64, Ordering::Relaxed);
            self.bytes_written.fetch_add((count * ps) as u64, Ordering::Relaxed);
            start = end;
        }
        Ok(())
    }

    /// Read superblock slot `slot` (0/1): exactly `SUPERBLOCK_SLOT_SIZE` bytes from
    /// `slot_offset(slot)`; bytes beyond EOF are returned as zeros (so a short/new file decodes as
    /// `SlotStatus::Empty`). io errors → `error::read_failed`.
    pub fn read_slot(&self, slot: usize) -> Result<Vec<u8>> {
        if slot >= SUPERBLOCK_SLOTS {
            return Err(PgError::internal(format!("superblock slot {slot} out of range")));
        }
        let mut buf = vec![0u8; SUPERBLOCK_SLOT_SIZE];
        let n = read_fill_at(self.file(), slot_offset(slot), &mut buf)
            .map_err(|e| error::read_failed(&self.path, &e))?;
        self.bytes_read.fetch_add(n as u64, Ordering::Relaxed);
        Ok(buf)
    }

    /// Write a full 4096-byte slot image at `slot_offset(slot)` with one `write_all_at`.
    /// Wrong length → `PgError::internal`. io errors → `error::write_failed`. Does not sync.
    pub fn write_slot(&self, slot: usize, image: &[u8]) -> Result<()> {
        if slot >= SUPERBLOCK_SLOTS {
            return Err(PgError::internal(format!("superblock slot {slot} out of range")));
        }
        if image.len() != SUPERBLOCK_SLOT_SIZE {
            return Err(PgError::internal(format!(
                "superblock slot image is {} bytes, expected {}",
                image.len(),
                SUPERBLOCK_SLOT_SIZE
            )));
        }
        self.file
            .write_all_at(slot_offset(slot), image)
            .map_err(|e| error::write_failed(&self.path, &e))?;
        self.bytes_written.fetch_add(image.len() as u64, Ordering::Relaxed);
        Ok(())
    }

    /// `file.sync_data()`; error → `error::fsync_failed(path, e)`. Counts a sync.
    pub fn sync_data(&self) -> Result<()> {
        self.file.sync_data().map_err(|e| error::fsync_failed(&self.path, &e))?;
        self.count_sync();
        Ok(())
    }

    /// `file.sync_all()`; error → `error::fsync_failed(path, e)`. Counts a sync.
    pub fn sync_all(&self) -> Result<()> {
        self.file.sync_all().map_err(|e| error::fsync_failed(&self.path, &e))?;
        self.count_sync();
        Ok(())
    }

    /// Current file length; error → `error::read_failed`.
    pub fn file_len(&self) -> Result<u64> {
        self.file.len().map_err(|e| error::read_failed(&self.path, &e))
    }

    /// `file.set_len(len)`; error → `error::truncate_failed(path, len, e)`. Does not sync.
    pub fn set_len(&self, len: u64) -> Result<()> {
        self.file.set_len(len).map_err(|e| error::truncate_failed(&self.path, len, &e))
    }

    /// Counter snapshot including cache stats.
    pub fn stats(&self) -> Result<PagerStats> {
        Ok(PagerStats {
            page_reads: self.page_reads.load(Ordering::Relaxed),
            page_writes: self.page_writes.load(Ordering::Relaxed),
            bytes_read: self.bytes_read.load(Ordering::Relaxed),
            bytes_written: self.bytes_written.load(Ordering::Relaxed),
            syncs: self.syncs.load(Ordering::Relaxed),
            cache: self.cache.stats()?,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::format::consts::PageKind;
    use crate::format::page::encode_page;
    use crate::io::{LockMode, MemFile};
    use pgarbor_common::{CommitSeq, SqlState};
    use std::sync::atomic::AtomicUsize;

    /// `MemFile` wrapper that counts `write_at` calls (shared counter) and can fail every write
    /// with ENOSPC.
    #[derive(Debug)]
    struct TestIo {
        inner: MemFile,
        writes: Arc<AtomicUsize>,
        fail_writes: bool,
    }

    impl FileIo for TestIo {
        fn read_at(&self, offset: u64, buf: &mut [u8]) -> std::io::Result<usize> {
            self.inner.read_at(offset, buf)
        }
        fn write_at(&self, offset: u64, buf: &[u8]) -> std::io::Result<usize> {
            self.writes.fetch_add(1, Ordering::Relaxed);
            if self.fail_writes {
                return Err(std::io::Error::from_raw_os_error(28));
            }
            self.inner.write_at(offset, buf)
        }
        fn sync_data(&self) -> std::io::Result<()> {
            self.inner.sync_data()
        }
        fn sync_all(&self) -> std::io::Result<()> {
            self.inner.sync_all()
        }
        fn len(&self) -> std::io::Result<u64> {
            self.inner.len()
        }
        fn set_len(&self, len: u64) -> std::io::Result<()> {
            self.inner.set_len(len)
        }
        fn try_lock(&self, mode: LockMode) -> std::io::Result<()> {
            self.inner.try_lock(mode)
        }
        fn unlock(&self) -> std::io::Result<()> {
            self.inner.unlock()
        }
    }

    const PS: u32 = 4096;

    fn new_pager(writes: Arc<AtomicUsize>, fail: bool, cache_pages: usize) -> Pager {
        let io = TestIo { inner: MemFile::new(), writes, fail_writes: fail };
        Pager::new(Box::new(io), PathBuf::from("test.pga"), PS, cache_pages)
    }

    fn sample_page(id: u64, fill: u8) -> Page {
        Page { id: PageId(id), kind: PageKind::Leaf, commit_seq: CommitSeq(1), payload: vec![fill; 100] }
    }

    fn encoded(id: u64, fill: u8) -> (PageId, Vec<u8>) {
        (PageId(id), encode_page(PS, &sample_page(id, fill)).expect("encode"))
    }

    #[test]
    fn pages_written_out_of_order_read_back_identically() {
        let pager = new_pager(Arc::new(AtomicUsize::new(0)), false, 8);
        let mut batch = vec![encoded(3, 0x33), encoded(1, 0x11), encoded(2, 0x22)];
        pager.write_pages(&mut batch).expect("write");
        for (id, fill) in [(1u64, 0x11u8), (2, 0x22), (3, 0x33)] {
            let got = pager.read_page(PageId(id)).expect("read");
            assert_eq!(*got, sample_page(id, fill));
        }
        let uncached = pager.read_page_uncached(PageId(2)).expect("uncached read");
        assert_eq!(uncached, sample_page(2, 0x22));
        let stats = pager.stats().expect("stats");
        assert_eq!(stats.page_writes, 3);
        assert_eq!(stats.bytes_written, 3 * PS as u64);
        assert_eq!(stats.page_reads, 4);
        assert_eq!(stats.cache.resident, 3);
    }

    #[test]
    fn consecutive_pages_use_one_write_call() {
        let writes = Arc::new(AtomicUsize::new(0));
        let pager = new_pager(writes.clone(), false, 0);
        let mut batch = vec![encoded(1, 1), encoded(2, 2), encoded(3, 3)];
        pager.write_pages(&mut batch).expect("write");
        assert_eq!(writes.load(Ordering::Relaxed), 1);
        // A gap starts a second run.
        let mut gapped = vec![encoded(10, 4), encoded(11, 5), encoded(20, 6)];
        pager.write_pages(&mut gapped).expect("write gapped");
        assert_eq!(writes.load(Ordering::Relaxed), 3);
        assert_eq!(*pager.read_page(PageId(20)).expect("read"), sample_page(20, 6));
    }

    #[test]
    fn write_batch_respects_one_mib_run_limit() {
        let writes = Arc::new(AtomicUsize::new(0));
        let pager = new_pager(writes.clone(), false, 0);
        // 300 consecutive 4 KiB pages; a run holds at most 256 pages (1 MiB), so two writes.
        let mut batch: Vec<(PageId, Vec<u8>)> = (1..=300u64).map(|i| encoded(i, (i % 200) as u8)).collect();
        pager.write_pages(&mut batch).expect("write");
        assert_eq!(writes.load(Ordering::Relaxed), 2);
        assert_eq!(*pager.read_page(PageId(300)).expect("read"), sample_page(300, (300 % 200) as u8));
    }

    #[test]
    fn enospc_on_write_is_disk_full_with_hint() {
        let pager = new_pager(Arc::new(AtomicUsize::new(0)), true, 0);
        let mut batch = vec![encoded(1, 1)];
        let err = pager.write_pages(&mut batch).expect_err("must fail");
        assert_eq!(err.sqlstate, SqlState::DISK_FULL);
        assert_eq!(err.sqlstate.code(), "53100");
        assert_eq!(err.hint.as_deref(), Some("Check free disk space."));
    }

    #[test]
    fn wrong_length_buffer_is_internal_and_writes_nothing() {
        let writes = Arc::new(AtomicUsize::new(0));
        let pager = new_pager(writes.clone(), false, 0);
        let mut batch = vec![encoded(1, 1), (PageId(2), vec![0u8; 10])];
        let err = pager.write_pages(&mut batch).expect_err("must fail");
        assert_eq!(err.sqlstate, SqlState::INTERNAL_ERROR);
        assert_eq!(writes.load(Ordering::Relaxed), 0);
    }

    #[test]
    fn corrupt_byte_is_detected_as_xx001() {
        let pager = new_pager(Arc::new(AtomicUsize::new(0)), false, 0);
        let mut batch = vec![encoded(1, 0x11), encoded(2, 0x22), encoded(3, 0x33)];
        pager.write_pages(&mut batch).expect("write");
        // Overwrite payload byte 8 of page 2 (the payload starts at offset 32 within the page).
        let off = page_offset(PS, PageId(2)) + 40;
        pager.file().write_at(off, &[0xAB]).expect("corrupt");
        let err = pager.read_page(PageId(2)).expect_err("corrupt page must fail");
        assert_eq!(err.sqlstate.code(), "XX001");
        // Neighbouring pages are unaffected.
        assert_eq!(*pager.read_page(PageId(1)).expect("page 1"), sample_page(1, 0x11));
    }

    #[test]
    fn read_beyond_eof_is_invalid_page() {
        let pager = new_pager(Arc::new(AtomicUsize::new(0)), false, 4);
        let mut batch = vec![encoded(1, 1), encoded(2, 2), encoded(3, 3)];
        pager.write_pages(&mut batch).expect("write");
        let err = pager.read_page(PageId(4)).expect_err("beyond EOF");
        assert_eq!(err.sqlstate.code(), "XX001");
        assert_eq!(err.detail.as_deref(), Some("page lies beyond end of file"));
        let none = pager.read_page(PageId::NONE).expect_err("null id");
        assert_eq!(none.detail.as_deref(), Some("null page reference"));
    }

    #[test]
    fn read_slot_on_empty_file_is_zeros() {
        let pager = new_pager(Arc::new(AtomicUsize::new(0)), false, 0);
        let s0 = pager.read_slot(0).expect("slot 0");
        assert_eq!(s0.len(), SUPERBLOCK_SLOT_SIZE);
        assert!(s0.iter().all(|&b| b == 0));
        let s1 = pager.read_slot(1).expect("slot 1");
        assert!(s1.iter().all(|&b| b == 0));
        assert!(pager.read_slot(2).is_err());
    }

    #[test]
    fn slot_roundtrip_and_length_checks() {
        let pager = new_pager(Arc::new(AtomicUsize::new(0)), false, 0);
        let mut image = vec![0u8; SUPERBLOCK_SLOT_SIZE];
        image[0] = 0x5A;
        image[SUPERBLOCK_SLOT_SIZE - 1] = 0xA5;
        pager.write_slot(1, &image).expect("write slot");
        assert_eq!(pager.read_slot(1).expect("read slot"), image);
        assert_eq!(pager.read_slot(0).expect("slot 0"), vec![0u8; SUPERBLOCK_SLOT_SIZE]);
        let short = pager.write_slot(0, &image[..10]).expect_err("short image");
        assert_eq!(short.sqlstate, SqlState::INTERNAL_ERROR);
    }

    #[test]
    fn read_raw_zero_fills_past_eof() {
        let pager = new_pager(Arc::new(AtomicUsize::new(0)), false, 0);
        let mut batch = vec![encoded(1, 0x77)];
        pager.write_pages(&mut batch).expect("write");
        let raw = pager.read_raw(PageId(2)).expect("raw read past EOF");
        assert_eq!(raw.len(), PS as usize);
        assert!(raw.iter().all(|&b| b == 0));
        let raw1 = pager.read_raw(PageId(1)).expect("raw page 1");
        assert_eq!(raw1, batch[0].1);
    }

    #[test]
    fn file_len_set_len_and_sync_counts() {
        let pager = new_pager(Arc::new(AtomicUsize::new(0)), false, 0);
        pager.set_len(file_len_for(PS, 2)).expect("set_len");
        assert_eq!(pager.file_len().expect("len"), file_len_for(PS, 2));
        pager.sync_data().expect("sync_data");
        pager.sync_all().expect("sync_all");
        assert_eq!(pager.stats().expect("stats").syncs, 2);
    }
}
