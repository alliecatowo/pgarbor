//! The single write transaction and the container commit protocol (STORE-09, STORE-10, STORE-18).
//! Lease STO-12 (opus): `read_page`, `alloc_page`, `write_page`, `release_page`, `spill`,
//! `commit`. Everything else here is implemented.
//!
//! # Page ownership inside a txn
//! - Committed pages: ids `1..=base.page_count`. Read through the shared pager/cache, never written.
//! - Private pages: ids `base.page_count+1 .. next_page`, allocated by this txn. Live versions are in
//!   `dirty` (in memory) or `spilled` (already written to the file past the committed end; read
//!   back uncached). Private pages may be overwritten in place and released for reuse
//!   (`free_private`). A private page is never inserted into the shared cache before commit.
//!
//! # Commit protocol (CONTRACT — ARCHITECTURE §5, byte order of operations matters)
//! 0. No dirty/spilled pages and `directory_root == base.directory_root` → return
//!    `Ok(base.commit_seq)` without any I/O.
//! 1. `page_count = max(base.page_count, highest id in dirty ∪ spilled)`. Released private ids above
//!    it are simply forgotten; released ids below it are unreachable holes (garbage).
//! 2. Encode every dirty page (`encode_page(ps, page)`; header commit_seq = `self.commit_seq`) and
//!    `pager.write_pages`. Failure → return the error; container NOT poisoned (the superblock is
//!    untouched, the pages are unreachable; a later txn reuses those ids).
//! 3. `sync_data` (SyncMode::Full only). Failure → poison (`inner.poison(msg)`), return error.
//!    (Conservative: after a failed fsync the kernel may have dropped dirty pages — PG treats
//!    this as PANIC, see src/backend/storage/file/fd.c `data_sync_elevel`.)
//! 4. `new_sb = Superblock{commit_seq: base+1, directory_root, page_count, ..base}`; write
//!    `new_sb.encode()` to slot `1 - base_slot`. Failure → poison, return error.
//! 5. `sync_data` (Full only). Failure → poison, return error. (The new superblock may or may not
//!    be durable; only recovery can tell, so no further writes are allowed in this process.)
//! 6. Publish `ContainerState{pager, sb: new_sb, slot: 1 - base_slot}`; `writer.commits += 1`.
//! 7. Insert the committed dirty pages into the pager cache (errors ignored: the commit is already
//!    durable and acknowledged-ready; the cache is an optimization).
//! 8. Return `Ok(new_sb.commit_seq)`. Only now may a caller acknowledge COMMIT to a client.
//!
//! Dropping a `WriteTxn` without `commit` aborts it: nothing is published, private pages become
//! garbage past the committed end (recovery truncates them; the next txn overwrites them).
use crate::batch::WriteBatch;
use crate::btree;
use crate::btree::read::RangeIter;
use crate::container::handle::{ContainerInner, WriterState};
use crate::container::tree_writer::TreeWriter;
#[allow(unused_imports)]
use crate::error;
use crate::format::consts::PageKind;
#[allow(unused_imports)]
use crate::format::page::{encode_page, Page, PageRef};
use crate::format::consts::max_page_count;
use crate::format::superblock::Superblock;
use crate::pager::{PageSink, PageSource, Pager};
use pgarbor_common::{CommitSeq, KvIter, PageId, PgError, Result};
use std::collections::{HashMap, HashSet};
use std::ops::Bound;
use std::sync::{Arc, MutexGuard};

// CONTRACT
/// The container's single writer. Holds the writer mutex until committed or dropped. Not `Send`
/// (it holds a `MutexGuard`); use it on one thread.
pub struct WriteTxn<'c> {
    pub(crate) inner: &'c ContainerInner,
    pub(crate) guard: MutexGuard<'c, WriterState>,
    pub(crate) pager: Arc<Pager>,
    pub(crate) base: Superblock,
    pub(crate) base_slot: usize,
    /// `base.commit_seq + 1`; stamped into every page header this txn writes.
    pub(crate) commit_seq: CommitSeq,
    pub(crate) directory_root: PageId,
    /// Next never-allocated id (starts at `base.page_count + 1`).
    pub(crate) next_page: u64,
    pub(crate) dirty: HashMap<u64, PageRef>,
    pub(crate) spilled: HashSet<u64>,
    pub(crate) free_private: Vec<PageId>,
    /// Sum of `payload.len()` over `dirty`.
    pub(crate) dirty_bytes: usize,
    /// Committed pages released by CoW replacement (space accounting only).
    pub(crate) released_shared: u64,
    pub(crate) spill_threshold: usize,
}

impl std::fmt::Debug for WriteTxn<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("WriteTxn")
            .field("base_seq", &self.base.commit_seq)
            .field("directory_root", &self.directory_root)
            .field("next_page", &self.next_page)
            .field("dirty", &self.dirty.len())
            .field("spilled", &self.spilled.len())
            .finish()
    }
}

impl<'c> WriteTxn<'c> {
    /// Acquire the writer mutex (blocking) and snapshot the published state.
    pub(crate) fn begin(inner: &'c ContainerInner) -> Result<WriteTxn<'c>> {
        inner.check_writable()?;
        let guard = inner.writer.lock().unwrap_or_else(|e| e.into_inner());
        // Another writer may have poisoned the container while we waited.
        inner.check_writable()?;
        let st = inner.current();
        if st.sb.commit_seq.0 >= u64::MAX - 1 {
            return Err(error::corrupt("commit sequence exhausted"));
        }
        Ok(WriteTxn {
            inner,
            guard,
            pager: st.pager,
            base: st.sb,
            base_slot: st.slot,
            commit_seq: CommitSeq(st.sb.commit_seq.0 + 1),
            directory_root: st.sb.directory_root,
            next_page: st.sb.page_count + 1,
            dirty: HashMap::new(),
            spilled: HashSet::new(),
            free_private: Vec::new(),
            dirty_bytes: 0,
            released_shared: 0,
            spill_threshold: inner.spill_threshold,
        })
    }

    /// Commit this txn will publish.
    pub fn commit_seq(&self) -> CommitSeq {
        self.commit_seq
    }
    /// Durable commit this txn started from.
    pub fn base_seq(&self) -> CommitSeq {
        self.base.commit_seq
    }
    pub fn base_superblock(&self) -> &Superblock {
        &self.base
    }
    /// Directory root as of this txn (base, or the last `set_directory_root`).
    pub fn directory_root(&self) -> PageId {
        self.directory_root
    }
    /// Root to publish in the superblock at commit.
    pub fn set_directory_root(&mut self, root: PageId) {
        self.directory_root = root;
    }
    pub fn dirty_pages(&self) -> usize {
        self.dirty.len()
    }
    pub fn spilled_pages(&self) -> usize {
        self.spilled.len()
    }

    /// Point lookup in any tree visible to this txn (committed or built in this txn).
    pub fn get(&self, root: PageId, key: &[u8]) -> Result<Option<Vec<u8>>> {
        btree::read::get(self, root, key)
    }
    /// Range scan in a tree visible to this txn.
    pub fn scan<'a>(&'a self, root: PageId, lower: Bound<&[u8]>, upper: Bound<&[u8]>) -> Result<KvIter<'a>> {
        Ok(Box::new(RangeIter::new(self, root, lower, upper)?))
    }
    /// Insert/replace; returns the tree's new root.
    pub fn put(&mut self, root: PageId, key: &[u8], value: &[u8]) -> Result<btree::PutResult> {
        let r = btree::insert::put(self, root, key, value)?;
        self.maybe_spill()?;
        Ok(r)
    }
    /// Delete; returns the tree's new root.
    pub fn delete(&mut self, root: PageId, key: &[u8]) -> Result<btree::DeleteResult> {
        let r = btree::delete::delete(self, root, key)?;
        self.maybe_spill()?;
        Ok(r)
    }
    /// Delete a key range (e.g. a dropped relation's prefix).
    pub fn delete_range(
        &mut self,
        root: PageId,
        lower: Bound<&[u8]>,
        upper: Bound<&[u8]>,
    ) -> Result<btree::RangeDeleteResult> {
        let r = btree::delete::delete_range(self, root, lower, upper)?;
        self.maybe_spill()?;
        Ok(r)
    }
    /// Apply an overlay/batch to a tree (pgarbor-core commit path).
    pub fn apply(&mut self, root: PageId, batch: &WriteBatch) -> Result<PageId> {
        let r = btree::bulk::apply_batch(self, root, batch)?;
        self.maybe_spill()?;
        Ok(r)
    }
    /// Bulk-build a new tree from strictly ascending pairs (e.g. CREATE INDEX).
    pub fn build_tree(&mut self, items: &mut dyn Iterator<Item = Result<(Vec<u8>, Vec<u8>)>>) -> Result<PageId> {
        let r = btree::bulk::build_from_sorted(self, items)?;
        self.maybe_spill()?;
        Ok(r)
    }
    /// `KvRead + KvWrite` view of one tree.
    pub fn tree<'t>(&'t mut self, root: PageId) -> TreeWriter<'t, 'c> {
        TreeWriter::new(self, root)
    }

    fn maybe_spill(&mut self) -> Result<()> {
        if self.dirty_bytes > self.spill_threshold { self.spill() } else { Ok(()) }
    }

    /// Write every dirty page to the file (ids are private, past the committed end), move their
    /// ids to `spilled`, clear `dirty`, reset `dirty_bytes`. Pages are encoded with
    /// `self.commit_seq`. No sync. Write failure → error (txn should be dropped), no poison.
    pub(crate) fn spill(&mut self) -> Result<()> {
        if self.dirty.is_empty() {
            return Ok(());
        }
        let ps = self.pager.page_size();
        let mut images: Vec<(PageId, Vec<u8>)> = Vec::with_capacity(self.dirty.len());
        for (id, page) in self.dirty.iter() {
            images.push((PageId(*id), encode_page(ps, page)?));
        }
        self.pager.write_pages(&mut images)?;
        // Only after the write succeeded do the pages change state.
        for (id, _) in images.iter() {
            self.spilled.insert(id.0);
        }
        self.dirty.clear();
        self.dirty_bytes = 0;
        Ok(())
    }

    /// Run the commit protocol described in the module docs. Consumes the txn (the writer mutex is
    /// released on return, success or failure).
    pub fn commit(mut self) -> Result<CommitSeq> {
        // Step 0: nothing to do.
        if self.dirty.is_empty() && self.spilled.is_empty() && self.directory_root == self.base.directory_root {
            return Ok(self.base.commit_seq);
        }
        // Step 1: final page count.
        let highest = self
            .dirty
            .keys()
            .copied()
            .chain(self.spilled.iter().copied())
            .max()
            .unwrap_or(0);
        let page_count = self.base.page_count.max(highest);
        let ps = self.pager.page_size();

        // Step 2: encode and write the still-dirty pages. Failure does not poison.
        let mut images: Vec<(PageId, Vec<u8>)> = Vec::with_capacity(self.dirty.len());
        for (id, page) in self.dirty.iter() {
            images.push((PageId(*id), encode_page(ps, page)?));
        }
        self.pager.write_pages(&mut images)?;
        drop(images);

        // Step 3: make the pages durable before the superblock can reference them.
        if self.inner.syncing() {
            if let Err(e) = self.pager.sync_data() {
                self.inner.poison(format!("fsync after page writes failed: {}", e.message));
                return Err(e);
            }
        }

        // Step 4: write the new superblock into the inactive slot.
        let new_slot = 1 - self.base_slot;
        let new_sb = Superblock {
            commit_seq: self.commit_seq,
            directory_root: self.directory_root,
            page_count,
            ..self.base
        };
        let image = new_sb.encode()?;
        if let Err(e) = self.pager.write_slot(new_slot, &image) {
            self.inner.poison(format!("superblock write to slot {new_slot} failed: {}", e.message));
            return Err(e);
        }

        // Step 5: make the superblock durable.
        if self.inner.syncing() {
            if let Err(e) = self.pager.sync_data() {
                self.inner.poison(format!("fsync after superblock write failed: {}", e.message));
                return Err(e);
            }
        }

        // Step 6: publish.
        self.inner.publish(crate::container::handle::ContainerState {
            pager: self.pager.clone(),
            sb: new_sb,
            slot: new_slot,
        });
        self.guard.commits += 1;

        // Step 7: warm the cache; errors are irrelevant now.
        let dirty = std::mem::take(&mut self.dirty);
        for (_, page) in dirty {
            let _ = self.pager.cache().insert(page);
        }

        // Step 8.
        Ok(new_sb.commit_seq)
    }

    /// Abort explicitly (same as drop).
    pub fn abort(self) {}
}

impl PageSource for WriteTxn<'_> {
    fn page_size(&self) -> u32 {
        self.pager.page_size()
    }
    /// `id` in `1..=base.page_count` → `pager.read_page(id)` and reject header commit_seq >
    /// `base.commit_seq` (as `Snapshot` does). Private live id → `dirty` entry, or for `spilled`
    /// ids `pager.read_page_uncached(id)` (must have commit_seq == `self.commit_seq`, wrapped in
    /// `Arc`). NONE, released, or never-allocated ids → `error::page_out_of_range(id,
    /// next_page - 1)`.
    fn read_page(&self, id: PageId) -> Result<PageRef> {
        if id.0 >= 1 && id.0 <= self.base.page_count {
            let page = self.pager.read_page(id)?;
            if page.commit_seq > self.base.commit_seq {
                return Err(error::invalid_page(
                    id,
                    format!(
                        "page written by commit {}, newer than snapshot commit {}",
                        page.commit_seq.0, self.base.commit_seq.0
                    ),
                ));
            }
            return Ok(page);
        }
        if self.is_private(id) {
            if let Some(p) = self.dirty.get(&id.0) {
                return Ok(p.clone());
            }
            if self.spilled.contains(&id.0) {
                let page = self.pager.read_page_uncached(id)?;
                if page.commit_seq != self.commit_seq {
                    return Err(error::invalid_page(
                        id,
                        format!(
                            "spilled page written by commit {}, expected commit {}",
                            page.commit_seq.0, self.commit_seq.0
                        ),
                    ));
                }
                return Ok(Arc::new(page));
            }
        }
        Err(error::page_out_of_range(id, self.next_page - 1))
    }
}

impl PageSink for WriteTxn<'_> {
    /// Pop `free_private` if non-empty, else `next_page++`.
    fn alloc_page(&mut self) -> Result<PageId> {
        if let Some(id) = self.free_private.pop() {
            return Ok(id);
        }
        if self.next_page >= max_page_count(self.base.page_size) {
            return Err(error::corrupt("page allocation limit reached"));
        }
        let id = PageId(self.next_page);
        self.next_page += 1;
        Ok(id)
    }
    /// Private ids only (else `PgError::internal("write to non-private page <id>")`); payload
    /// within `payload_capacity` (else internal). Insert `Arc<Page{id, kind, commit_seq:
    /// self.commit_seq, payload}>` into `dirty` (replacing; adjust `dirty_bytes`), remove from
    /// `spilled` if present (the dirty version supersedes the spilled bytes).
    fn write_page(&mut self, id: PageId, kind: PageKind, payload: Vec<u8>) -> Result<()> {
        if !self.is_private(id) {
            return Err(PgError::internal(format!("write to non-private page {}", id.0)));
        }
        let cap = crate::format::consts::payload_capacity(self.pager.page_size());
        if payload.len() > cap {
            return Err(PgError::internal(format!(
                "payload of {} bytes exceeds page capacity {} (page {})",
                payload.len(),
                cap,
                id.0
            )));
        }
        self.dirty_bytes += payload.len();
        let page = Arc::new(Page { id, kind, commit_seq: self.commit_seq, payload });
        if let Some(old) = self.dirty.insert(id.0, page) {
            self.dirty_bytes = self.dirty_bytes.saturating_sub(old.payload.len());
        }
        self.spilled.remove(&id.0);
        Ok(())
    }
    fn is_private(&self, id: PageId) -> bool {
        id.0 > self.base.page_count && id.0 < self.next_page
    }
    /// Private → remove from `dirty`/`spilled`, push onto `free_private`
    /// (`PgError::internal` on double release). Shared → `released_shared += 1`.
    fn release_page(&mut self, id: PageId) -> Result<()> {
        if self.is_private(id) {
            let mut was_live = false;
            if let Some(old) = self.dirty.remove(&id.0) {
                self.dirty_bytes = self.dirty_bytes.saturating_sub(old.payload.len());
                was_live = true;
            }
            if self.spilled.remove(&id.0) {
                was_live = true;
            }
            // A live (written) page cannot be on the free list; only scan otherwise.
            if !was_live && self.free_private.contains(&id) {
                return Err(PgError::internal(format!("double release of private page {}", id.0)));
            }
            self.free_private.push(id);
            Ok(())
        } else if id.is_none() || id.0 >= self.next_page {
            Err(PgError::internal(format!("release of invalid page {}", id.0)))
        } else {
            self.released_shared += 1;
            Ok(())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::container::handle::Container;
    use crate::container::options::{CreateOptions, SyncMode};
    use crate::io::{FileIo, LockMode, MemVfs, OpenMode, Vfs};
    use pgarbor_common::KvRead;
    use std::io;
    use std::path::Path;
    use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

    #[derive(Debug, Default)]
    struct Ctl {
        writes: AtomicU64,
        syncs: AtomicU64,
        fail_write: AtomicBool,
        fail_sync: AtomicBool,
    }

    #[derive(Debug)]
    struct CountFile {
        inner: Box<dyn FileIo>,
        ctl: Arc<Ctl>,
    }
    impl FileIo for CountFile {
        fn read_at(&self, o: u64, b: &mut [u8]) -> io::Result<usize> {
            self.inner.read_at(o, b)
        }
        fn write_at(&self, o: u64, b: &[u8]) -> io::Result<usize> {
            if self.ctl.fail_write.load(Ordering::SeqCst) {
                return Err(io::Error::from_raw_os_error(28));
            }
            self.ctl.writes.fetch_add(1, Ordering::SeqCst);
            self.inner.write_at(o, b)
        }
        fn sync_data(&self) -> io::Result<()> {
            if self.ctl.fail_sync.load(Ordering::SeqCst) {
                return Err(io::Error::from_raw_os_error(5));
            }
            self.ctl.syncs.fetch_add(1, Ordering::SeqCst);
            self.inner.sync_data()
        }
        fn sync_all(&self) -> io::Result<()> {
            self.sync_data()
        }
        fn len(&self) -> io::Result<u64> {
            self.inner.len()
        }
        fn set_len(&self, l: u64) -> io::Result<()> {
            self.inner.set_len(l)
        }
        fn try_lock(&self, m: LockMode) -> io::Result<()> {
            self.inner.try_lock(m)
        }
        fn unlock(&self) -> io::Result<()> {
            self.inner.unlock()
        }
    }

    #[derive(Debug)]
    struct CountVfs {
        inner: MemVfs,
        ctl: Arc<Ctl>,
    }
    impl Vfs for CountVfs {
        fn open(&self, p: &Path, m: OpenMode) -> io::Result<Box<dyn FileIo>> {
            let f = self.inner.open(p, m)?;
            Ok(Box::new(CountFile { inner: f, ctl: self.ctl.clone() }))
        }
        fn exists(&self, p: &Path) -> io::Result<bool> {
            self.inner.exists(p)
        }
        fn rename(&self, a: &Path, b: &Path) -> io::Result<()> {
            self.inner.rename(a, b)
        }
        fn remove_file(&self, p: &Path) -> io::Result<()> {
            self.inner.remove_file(p)
        }
        fn sync_dir(&self, d: &Path) -> io::Result<()> {
            self.inner.sync_dir(d)
        }
    }

    fn make(spill: usize) -> (Container, Arc<Ctl>) {
        let ctl = Arc::new(Ctl::default());
        let vfs: Arc<dyn Vfs> = Arc::new(CountVfs { inner: MemVfs::new(), ctl: ctl.clone() });
        let opts = CreateOptions { spill_threshold_bytes: spill, sync: SyncMode::Full, ..CreateOptions::default() };
        let c = Container::create_with(vfs, Path::new("t.pga"), &opts).unwrap();
        (c, ctl)
    }

    fn put_commit(c: &Container, k: &[u8], v: &[u8]) -> Result<CommitSeq> {
        let mut w = c.begin_write()?;
        let root = w.put(PageId::NONE, k, v)?.root;
        w.set_directory_root(root);
        w.commit()
    }

    #[test]
    fn commit_then_snapshot_sees_data() {
        let (c, _) = make(64 << 20);
        let seq = put_commit(&c, b"k", b"v").unwrap();
        assert_eq!(seq, c.commit_seq());
        assert_eq!(seq.0, 2);
        let snap = c.snapshot();
        assert_eq!(snap.directory().get(b"k").unwrap(), Some(b"v".to_vec()));
    }

    #[test]
    fn drop_without_commit_publishes_nothing() {
        let (c, _) = make(64 << 20);
        let before = c.commit_seq();
        {
            let mut w = c.begin_write().unwrap();
            let r = w.put(PageId::NONE, b"k", b"v").unwrap().root;
            w.set_directory_root(r);
        }
        assert_eq!(c.commit_seq(), before);
        assert_eq!(c.snapshot().directory_root(), PageId::NONE);
        assert!(!c.is_poisoned());
    }

    #[test]
    fn empty_commit_does_no_io() {
        let (c, ctl) = make(64 << 20);
        let w0 = ctl.writes.load(Ordering::SeqCst);
        let s0 = ctl.syncs.load(Ordering::SeqCst);
        let w = c.begin_write().unwrap();
        let seq = w.commit().unwrap();
        assert_eq!(seq, c.commit_seq());
        assert_eq!(ctl.writes.load(Ordering::SeqCst), w0);
        assert_eq!(ctl.syncs.load(Ordering::SeqCst), s0);
    }

    #[test]
    fn tiny_spill_threshold_still_correct() {
        let (c, _) = make(1);
        let mut w = c.begin_write().unwrap();
        let mut root = PageId::NONE;
        for i in 0..300u32 {
            let k = i.to_be_bytes();
            root = w.put(root, &k, &[i as u8; 200]).unwrap().root;
        }
        for i in 0..300u32 {
            assert_eq!(w.get(root, &i.to_be_bytes()).unwrap(), Some(vec![i as u8; 200]));
        }
        w.set_directory_root(root);
        w.commit().unwrap();
        let snap = c.snapshot();
        for i in 0..300u32 {
            assert_eq!(snap.directory().get(&i.to_be_bytes()).unwrap(), Some(vec![i as u8; 200]));
        }
    }

    #[test]
    fn page_write_failure_does_not_poison() {
        let (c, ctl) = make(64 << 20);
        put_commit(&c, b"a", b"1").unwrap();
        ctl.fail_write.store(true, Ordering::SeqCst);
        let err = put_commit(&c, b"b", b"2").unwrap_err();
        assert_eq!(err.sqlstate.code(), "53100");
        assert!(!c.is_poisoned());
        assert_eq!(c.commit_seq().0, 2);
        ctl.fail_write.store(false, Ordering::SeqCst);
        put_commit(&c, b"b", b"2").unwrap();
        assert_eq!(c.commit_seq().0, 3);
    }

    #[test]
    fn sync_failure_poisons() {
        let (c, ctl) = make(64 << 20);
        ctl.fail_sync.store(true, Ordering::SeqCst);
        assert!(put_commit(&c, b"a", b"1").is_err());
        assert!(c.is_poisoned());
        ctl.fail_sync.store(false, Ordering::SeqCst);
        let e = c.begin_write().unwrap_err();
        assert_eq!(e.sqlstate.code(), "58030");
        assert_eq!(c.commit_seq().0, 1);
    }

    #[test]
    fn release_and_reuse_private_pages() {
        let (c, _) = make(64 << 20);
        let mut w = c.begin_write().unwrap();
        let a = w.alloc_page().unwrap();
        assert!(w.is_private(a));
        w.write_page(a, PageKind::Leaf, vec![1, 2, 3]).unwrap();
        w.release_page(a).unwrap();
        assert!(w.release_page(a).is_err());
        assert!(w.read_page(a).is_err());
        assert_eq!(w.alloc_page().unwrap(), a);
        assert!(w.write_page(PageId(0), PageKind::Leaf, vec![]).is_err());
    }
}
