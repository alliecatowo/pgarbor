//! Structural, sharing-preserving tree copy between files (used by export and compaction).
//! Lease STO-15.
//!
//! Copying preserves tree SHAPE (node boundaries, separators) and SHARING: a page reachable from
//! several roots (e.g. two branches) is copied once; the memo maps old id → new id. Only page ids
//! and overflow pointers change. New ids are assigned by the destination sink in the order pages
//! are written (post-order: children before parents), which lays subtrees out contiguously.
#[allow(unused_imports)]
use crate::btree::bulk::build_from_sorted;
#[allow(unused_imports)]
use crate::btree::node::{decode_node, LeafValue, Node};
#[allow(unused_imports)]
use crate::btree::overflow::copy_overflow;
#[allow(unused_imports)]
use crate::btree::read::RangeIter;
use crate::pager::{PageSink, PageSource};
use crate::roots::RootResolver;
use pgarbor_common::{PageId, PgError, Result};
use std::collections::HashMap;

// CONTRACT
#[derive(Debug, Default)]
pub struct TreeCopier {
    /// old node/overflow-first-page id → new id.
    pub(crate) memo: HashMap<u64, u64>,
    pub pages_copied: u64,
}

impl TreeCopier {
    pub fn new() -> TreeCopier {
        TreeCopier::default()
    }
    /// New id of an already-copied page (node or first page of an overflow chain).
    pub fn mapped(&self, old: PageId) -> Option<PageId> {
        self.memo.get(&old.0).map(|n| PageId(*n))
    }

    /// Copy the tree at `root` from `src` into `dst`, returning the new root (NONE → NONE).
    /// Post-order recursion (depth <= 64 else XX001): for an internal node copy each child
    /// (memoized) then write the node with remapped children via `dst.write_new`; for a leaf copy
    /// every overflow chain (memo keyed by the chain's first page id; `copy_overflow`) and write
    /// the leaf with remapped pointers. Payload bytes are re-encoded with `LeafNode::encode` /
    /// `InternalNode::encode` (identical layout). `pages_copied` counts every page written.
    #[allow(unused_variables)]
    pub fn copy_tree(&mut self, src: &dyn PageSource, dst: &mut dyn PageSink, root: PageId) -> Result<PageId> {
        Err(PgError::unimplemented("STO-15", "structural tree copy"))
    }

    /// Copy a directory tree, renumbering embedded roots: scan `dir_root` in `src` ascending; for
    /// each `(k, v)` call `resolver.remap(k, v, &mut |r| self.copy_tree(src, dst, r))` and keep
    /// `remapped.unwrap_or(v)`; collect into a Vec, then `build_from_sorted(dst, ..)` the new
    /// directory (a fresh, densely packed tree). Returns the new directory root (NONE if empty).
    #[allow(unused_variables)]
    pub fn copy_directory(
        &mut self,
        src: &dyn PageSource,
        dst: &mut dyn PageSink,
        dir_root: PageId,
        resolver: &dyn RootResolver,
    ) -> Result<PageId> {
        Err(PgError::unimplemented("STO-15", "directory copy with root remapping"))
    }
}
