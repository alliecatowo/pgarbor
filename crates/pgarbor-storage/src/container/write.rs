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
    #[allow(unused_variables)]
    pub(crate) fn spill(&mut self) -> Result<()> {
        Err(PgError::unimplemented("STO-12", "write txn spill"))
    }

    /// Run the commit protocol described in the module docs. Consumes the txn (the writer mutex is
    /// released on return, success or failure).
    #[allow(unused_mut)]
    pub fn commit(mut self) -> Result<CommitSeq> {
        Err(PgError::unimplemented("STO-12", "container commit"))
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
    #[allow(unused_variables)]
    fn read_page(&self, id: PageId) -> Result<PageRef> {
        Err(PgError::unimplemented("STO-12", "write txn page read"))
    }
}

impl PageSink for WriteTxn<'_> {
    /// Pop `free_private` if non-empty, else `next_page++`.
    fn alloc_page(&mut self) -> Result<PageId> {
        Err(PgError::unimplemented("STO-12", "write txn page allocation"))
    }
    /// Private ids only (else `PgError::internal("write to non-private page <id>")`); payload
    /// within `payload_capacity` (else internal). Insert `Arc<Page{id, kind, commit_seq:
    /// self.commit_seq, payload}>` into `dirty` (replacing; adjust `dirty_bytes`), remove from
    /// `spilled` if present (the dirty version supersedes the spilled bytes).
    #[allow(unused_variables)]
    fn write_page(&mut self, id: PageId, kind: PageKind, payload: Vec<u8>) -> Result<()> {
        Err(PgError::unimplemented("STO-12", "write txn page write"))
    }
    fn is_private(&self, id: PageId) -> bool {
        id.0 > self.base.page_count && id.0 < self.next_page
    }
    /// Private → remove from `dirty`/`spilled`, push onto `free_private`
    /// (`PgError::internal` on double release). Shared → `released_shared += 1`.
    #[allow(unused_variables)]
    fn release_page(&mut self, id: PageId) -> Result<()> {
        Err(PgError::unimplemented("STO-12", "write txn page release"))
    }
}
