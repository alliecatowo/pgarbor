//! Read-only snapshots (STORE-06, STORE-13). CONTRACT. Fully implemented (delegates to btree).
//!
//! A [`Snapshot`] freezes one committed superblock (commit_seq, directory_root, page_count) plus an
//! `Arc<Pager>` of the file generation it came from. It is `Clone + Send + Sync`, cheap to clone,
//! and stays valid forever: later commits only append pages, and compaction swaps in a new
//! Pager while this snapshot keeps reading the old file handle (Unix: renamed-over inode stays
//! readable; MemVfs/FaultVfs emulate this).
//!
//! [`TreeSnapshot`] = snapshot + one tree root; it implements `pgarbor_common::KvRead` and is what
//! pgarbor-core uses as a transaction's read snapshot of a branch.
use crate::btree::read::{get, RangeIter};
use crate::container::pins::PinGuard;
use crate::error;
use crate::format::page::PageRef;
use crate::format::superblock::Superblock;
use crate::pager::{PageSource, Pager};
use pgarbor_common::{CommitSeq, DatabaseUuid, KvIter, KvRead, PageId, Result};
use std::ops::Bound;
use std::sync::Arc;

#[derive(Debug)]
struct SnapshotInner {
    pager: Arc<Pager>,
    sb: Superblock,
    _pin: Option<PinGuard>,
}

// CONTRACT
#[derive(Clone, Debug)]
pub struct Snapshot {
    inner: Arc<SnapshotInner>,
}

impl Snapshot {
    /// Used by `Container::snapshot`, compaction and doctor. `pin` is `None` for internal
    /// short-lived snapshots.
    pub(crate) fn new(pager: Arc<Pager>, sb: Superblock, pin: Option<PinGuard>) -> Snapshot {
        Snapshot { inner: Arc::new(SnapshotInner { pager, sb, _pin: pin }) }
    }
    pub fn commit_seq(&self) -> CommitSeq {
        self.inner.sb.commit_seq
    }
    pub fn directory_root(&self) -> PageId {
        self.inner.sb.directory_root
    }
    pub fn page_count(&self) -> u64 {
        self.inner.sb.page_count
    }
    pub fn db_uuid(&self) -> DatabaseUuid {
        self.inner.sb.db_uuid
    }
    pub fn superblock(&self) -> &Superblock {
        &self.inner.sb
    }
    pub fn pager(&self) -> &Arc<Pager> {
        &self.inner.pager
    }
    /// A tree of this snapshot. `root` must be NONE or within `1..=page_count`, else
    /// `error::page_out_of_range` (XX001).
    pub fn tree(&self, root: PageId) -> Result<TreeSnapshot> {
        if root.0 > self.page_count() {
            return Err(error::page_out_of_range(root, self.page_count()));
        }
        Ok(TreeSnapshot { snap: self.clone(), root })
    }
    /// The directory tree of this snapshot.
    pub fn directory(&self) -> TreeSnapshot {
        TreeSnapshot { snap: self.clone(), root: self.directory_root() }
    }
}

impl PageSource for Snapshot {
    fn page_size(&self) -> u32 {
        self.inner.pager.page_size()
    }
    fn read_page(&self, id: PageId) -> Result<PageRef> {
        if id.is_none() || id.0 > self.page_count() {
            return Err(error::page_out_of_range(id, self.page_count()));
        }
        let page = self.inner.pager.read_page(id)?;
        if page.commit_seq > self.commit_seq() {
            return Err(error::invalid_page(
                id,
                format!(
                    "page written by commit {}, newer than snapshot commit {}",
                    page.commit_seq.0,
                    self.commit_seq().0
                ),
            ));
        }
        Ok(page)
    }
}

// CONTRACT
/// One tree as of a snapshot. Implements `KvRead`. `Clone + Send + Sync`.
#[derive(Clone, Debug)]
pub struct TreeSnapshot {
    snap: Snapshot,
    root: PageId,
}

impl TreeSnapshot {
    pub fn root(&self) -> PageId {
        self.root
    }
    pub fn snapshot(&self) -> &Snapshot {
        &self.snap
    }
    /// Typed range iterator (same as `scan` without boxing).
    pub fn range(&self, lower: Bound<&[u8]>, upper: Bound<&[u8]>) -> Result<RangeIter<'_>> {
        RangeIter::new(&self.snap, self.root, lower, upper)
    }
}

impl KvRead for TreeSnapshot {
    fn get(&self, key: &[u8]) -> Result<Option<Vec<u8>>> {
        get(&self.snap, self.root, key)
    }
    fn scan<'a>(&'a self, lower: Bound<&[u8]>, upper: Bound<&[u8]>) -> Result<KvIter<'a>> {
        Ok(Box::new(RangeIter::new(&self.snap, self.root, lower, upper)?))
    }
}
