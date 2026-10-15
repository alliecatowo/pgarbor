//! `TreeWriter`: `KvRead + KvWrite` adapter over one tree inside a `WriteTxn`. CONTRACT.
//! Fully implemented (delegates). Used by pgarbor-branches to edit the directory and by
//! pgarbor-core for immediate mini-commits (sequences).
//!
//! ```ignore
//! let mut w = container.begin_write()?;
//! let dir = w.directory_root();
//! let mut t = w.tree(dir);
//! t.put(b"Nmain", &branch_id)?;          // KvWrite
//! let new_dir = t.root();
//! w.set_directory_root(new_dir);
//! w.commit()?;
//! ```
use crate::btree::read::RangeIter;
use crate::container::write::WriteTxn;
use pgarbor_common::{KvIter, KvRead, KvWrite, PageId, Result};
use std::ops::Bound;

// CONTRACT
pub struct TreeWriter<'t, 'c> {
    txn: &'t mut WriteTxn<'c>,
    root: PageId,
}

impl<'t, 'c> TreeWriter<'t, 'c> {
    pub fn new(txn: &'t mut WriteTxn<'c>, root: PageId) -> TreeWriter<'t, 'c> {
        TreeWriter { txn, root }
    }
    /// Current root after the writes so far.
    pub fn root(&self) -> PageId {
        self.root
    }
}

impl KvRead for TreeWriter<'_, '_> {
    fn get(&self, key: &[u8]) -> Result<Option<Vec<u8>>> {
        self.txn.get(self.root, key)
    }
    fn scan<'a>(&'a self, lower: Bound<&[u8]>, upper: Bound<&[u8]>) -> Result<KvIter<'a>> {
        let src: &WriteTxn<'_> = self.txn;
        Ok(Box::new(RangeIter::new(src, self.root, lower, upper)?))
    }
}

impl KvWrite for TreeWriter<'_, '_> {
    fn put(&mut self, key: &[u8], value: &[u8]) -> Result<()> {
        self.root = self.txn.put(self.root, key, value)?.root;
        Ok(())
    }
    fn delete(&mut self, key: &[u8]) -> Result<bool> {
        let r = self.txn.delete(self.root, key)?;
        self.root = r.root;
        Ok(r.existed)
    }
}
