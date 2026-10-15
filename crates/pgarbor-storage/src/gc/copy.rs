//! Structural, sharing-preserving tree copy between files (used by export and compaction).
//! Lease STO-15.
//!
//! Copying preserves tree SHAPE (node boundaries, separators) and SHARING: a page reachable from
//! several roots (e.g. two branches) is copied once; the memo maps old id → new id. Only page ids
//! and overflow pointers change. New ids are assigned by the destination sink in the order pages
//! are written (post-order: children before parents), which lays subtrees out contiguously.
use crate::btree::bulk::build_from_sorted;
use crate::btree::node::{decode_node, InternalEntry, InternalNode, LeafEntry, LeafNode, LeafValue, Node};
use crate::btree::overflow::{copy_overflow, pages_for};
use crate::btree::read::RangeIter;
use crate::error;
use crate::format::consts::PageKind;
use crate::pager::{PageSink, PageSource};
use crate::roots::RootResolver;
use pgarbor_common::{PageId, PgError, Result};
use std::collections::HashMap;
use std::ops::Bound;

/// Maximum recursion depth of the post-order copy; deeper trees are corrupt or cyclic.
const MAX_COPY_DEPTH: usize = 64;

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
    pub fn copy_tree(&mut self, src: &dyn PageSource, dst: &mut dyn PageSink, root: PageId) -> Result<PageId> {
        if root.is_none() {
            return Ok(PageId::NONE);
        }
        self.copy_node(src, dst, root, 0)
    }

    /// Copy one node (memoized) at recursion `depth`.
    fn copy_node(&mut self, src: &dyn PageSource, dst: &mut dyn PageSink, id: PageId, depth: usize) -> Result<PageId> {
        if let Some(new) = self.mapped(id) {
            return Ok(new);
        }
        if depth > MAX_COPY_DEPTH {
            return Err(error::invalid_page(id, "b-tree too deep, possible cycle"));
        }
        let page = src.read_page(id)?;
        let page_size = src.page_size();
        let new_id = match decode_node(&page)? {
            Node::Leaf(leaf) => {
                let mut entries = Vec::with_capacity(leaf.entries.len());
                for entry in leaf.entries {
                    let value = match entry.value {
                        LeafValue::Inline(bytes) => LeafValue::Inline(bytes),
                        LeafValue::Overflow { first, len } => {
                            LeafValue::Overflow { first: self.copy_chain(src, dst, first, len)?, len }
                        }
                    };
                    entries.push(LeafEntry { key: entry.key, value });
                }
                let payload = LeafNode { entries }.encode(page_size)?;
                self.pages_copied += 1;
                dst.write_new(PageKind::Leaf, payload)?
            }
            Node::Internal(node) => {
                let first_child = self.copy_node(src, dst, node.first_child, depth + 1)?;
                let mut entries = Vec::with_capacity(node.entries.len());
                for entry in node.entries {
                    let child = self.copy_node(src, dst, entry.child, depth + 1)?;
                    entries.push(InternalEntry { key: entry.key, child });
                }
                let payload = InternalNode { level: node.level, first_child, entries }.encode(page_size)?;
                self.pages_copied += 1;
                dst.write_new(PageKind::Internal, payload)?
            }
        };
        self.memo.insert(id.0, new_id.0);
        Ok(new_id)
    }

    /// Copy one overflow chain (memoized by its first page id).
    fn copy_chain(&mut self, src: &dyn PageSource, dst: &mut dyn PageSink, first: PageId, len: u32) -> Result<PageId> {
        if let Some(new) = self.mapped(first) {
            return Ok(new);
        }
        let new_first = copy_overflow(src, dst, first, len)?;
        self.pages_copied += pages_for(src.page_size(), len as u64);
        self.memo.insert(first.0, new_first.0);
        Ok(new_first)
    }

    /// Copy a directory tree, renumbering embedded roots: scan `dir_root` in `src` ascending; for
    /// each `(k, v)` call `resolver.remap(k, v, &mut |r| self.copy_tree(src, dst, r))` and keep
    /// `remapped.unwrap_or(v)`; collect into a Vec, then `build_from_sorted(dst, ..)` the new
    /// directory (a fresh, densely packed tree). Returns the new directory root (NONE if empty).
    pub fn copy_directory(
        &mut self,
        src: &dyn PageSource,
        dst: &mut dyn PageSink,
        dir_root: PageId,
        resolver: &dyn RootResolver,
    ) -> Result<PageId> {
        let entries: Vec<(Vec<u8>, Vec<u8>)> =
            RangeIter::new(src, dir_root, Bound::Unbounded, Bound::Unbounded)?.collect::<Result<Vec<_>>>()?;
        let mut out: Vec<(Vec<u8>, Vec<u8>)> = Vec::with_capacity(entries.len());
        for (key, value) in entries {
            let remapped = resolver.remap(&key, &value, &mut |r: PageId| self.copy_tree(src, &mut *dst, r))?;
            out.push((key, remapped.unwrap_or(value)));
        }
        let mut items = out.into_iter().map(Ok::<(Vec<u8>, Vec<u8>), PgError>);
        build_from_sorted(dst, &mut items)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::btree::node::{InternalEntry as TestInternalEntry, LeafValue as TestLeafValue};
    use crate::btree::overflow::chain_pages;
    use crate::format::page::Page;
    use crate::pager::PageRef;
    use crate::roots::PrefixU64Roots;
    use pgarbor_common::CommitSeq;
    use std::collections::{HashMap as Map, HashSet};
    use std::sync::Arc;

    const PS: u32 = 8192;

    #[derive(Default)]
    struct MemStore {
        pages: Map<u64, Page>,
        next: u64,
    }

    impl PageSource for MemStore {
        fn page_size(&self) -> u32 {
            PS
        }
        fn read_page(&self, id: PageId) -> Result<PageRef> {
            self.pages.get(&id.0).map(|p| Arc::new(p.clone())).ok_or_else(|| error::page_out_of_range(id, self.next))
        }
    }

    impl PageSink for MemStore {
        fn alloc_page(&mut self) -> Result<PageId> {
            self.next += 1;
            Ok(PageId(self.next))
        }
        fn write_page(&mut self, id: PageId, kind: PageKind, payload: Vec<u8>) -> Result<()> {
            self.pages.insert(id.0, Page { id, kind, commit_seq: CommitSeq(1), payload });
            Ok(())
        }
        fn is_private(&self, id: PageId) -> bool {
            id.0 >= 1 && id.0 <= self.next
        }
        fn release_page(&mut self, _id: PageId) -> Result<()> {
            Ok(())
        }
    }

    fn key(i: usize) -> Vec<u8> {
        format!("k{i:06}").into_bytes()
    }

    fn build(store: &mut MemStore, n: usize, vlen: usize) -> PageId {
        let items: Vec<Result<(Vec<u8>, Vec<u8>)>> =
            (0..n).map(|i| Ok((key(i), vec![(i % 251) as u8; vlen]))).collect();
        let mut it = items.into_iter();
        build_from_sorted(store, &mut it).unwrap()
    }

    fn scan(src: &dyn PageSource, root: PageId) -> Vec<(Vec<u8>, Vec<u8>)> {
        RangeIter::new(src, root, Bound::Unbounded, Bound::Unbounded).unwrap().map(|r| r.unwrap()).collect()
    }

    /// Every page id reachable from `root` (nodes and overflow chains).
    fn reachable(src: &MemStore, root: PageId, out: &mut HashSet<u64>) {
        if root.is_none() || !out.insert(root.0) {
            return;
        }
        let page = src.read_page(root).unwrap();
        match decode_node(&page).unwrap() {
            Node::Leaf(leaf) => {
                for e in leaf.entries {
                    if let TestLeafValue::Overflow { first, len } = e.value {
                        for p in chain_pages(src, first, len).unwrap() {
                            out.insert(p.0);
                        }
                    }
                }
            }
            Node::Internal(node) => {
                for c in node.children() {
                    reachable(src, c, out);
                }
            }
        }
    }

    #[test]
    fn none_root_copies_to_none() {
        let src = MemStore::default();
        let mut dst = MemStore::default();
        let mut c = TreeCopier::new();
        assert_eq!(c.copy_tree(&src, &mut dst, PageId::NONE).unwrap(), PageId::NONE);
        assert_eq!(c.pages_copied, 0);
    }

    #[test]
    fn copied_tree_scans_equal_source() {
        let mut src = MemStore::default();
        let root = build(&mut src, 3000, 20);
        let big = build(&mut src, 30, 5000);
        let mut dst = MemStore::default();
        let mut c = TreeCopier::new();
        let new_root = c.copy_tree(&src, &mut dst, root).unwrap();
        let new_big = c.copy_tree(&src, &mut dst, big).unwrap();
        assert_eq!(scan(&src, root), scan(&dst, new_root));
        assert_eq!(scan(&src, big), scan(&dst, new_big));
    }

    #[test]
    fn repeated_roots_write_each_page_once() {
        let mut src = MemStore::default();
        let root = build(&mut src, 1500, 16);
        let big = build(&mut src, 40, 5000);
        let mut dst = MemStore::default();
        let mut c = TreeCopier::new();
        let a = c.copy_tree(&src, &mut dst, root).unwrap();
        let b = c.copy_tree(&src, &mut dst, root).unwrap();
        assert_eq!(a, b);
        let before = c.pages_copied;
        c.copy_tree(&src, &mut dst, big).unwrap();
        let mut distinct = HashSet::new();
        reachable(&src, root, &mut distinct);
        reachable(&src, big, &mut distinct);
        assert_eq!(c.pages_copied, distinct.len() as u64);
        assert_eq!(dst.pages.len() as u64, c.pages_copied);
        assert!(c.pages_copied > before);
    }

    #[test]
    fn shared_subtree_is_written_once() {
        let mut src = MemStore::default();
        let leaf = build(&mut src, 10, 4);
        // Internal root whose two children are the same leaf (structurally valid for copying).
        let inner = InternalNode {
            level: 1,
            first_child: leaf,
            entries: vec![TestInternalEntry { key: key(5), child: leaf }],
        };
        let root = src.write_new(PageKind::Internal, inner.encode(PS).unwrap()).unwrap();
        let mut dst = MemStore::default();
        let mut c = TreeCopier::new();
        let new_root = c.copy_tree(&src, &mut dst, root).unwrap();
        assert_eq!(c.pages_copied, 2);
        assert_eq!(dst.pages.len(), 2);
        match decode_node(&dst.read_page(new_root).unwrap()).unwrap() {
            Node::Internal(n) => {
                assert_eq!(n.entries.len(), 1);
                assert_eq!(n.first_child, n.entries[0].child);
                assert_eq!(Some(n.first_child), c.mapped(leaf));
            }
            Node::Leaf(_) => panic!("expected an internal root"),
        }
    }

    #[test]
    fn directory_copy_remaps_roots() {
        let mut src = MemStore::default();
        let data1 = build(&mut src, 3000, 20);
        let data2 = build(&mut src, 50, 5000);
        let dir_items: Vec<Result<(Vec<u8>, Vec<u8>)>> = vec![
            Ok((b"Rempty".to_vec(), 0u64.to_le_bytes().to_vec())),
            Ok((b"Rmain".to_vec(), data1.0.to_le_bytes().to_vec())),
            Ok((b"Rsnap".to_vec(), data2.0.to_le_bytes().to_vec())),
            Ok((b"Xmeta".to_vec(), b"x".to_vec())),
        ];
        let mut it = dir_items.into_iter();
        let dir = build_from_sorted(&mut src, &mut it).unwrap();

        let mut dst = MemStore::default();
        let mut c = TreeCopier::new();
        let resolver = PrefixU64Roots::new(b"R");
        let new_dir = c.copy_directory(&src, &mut dst, dir, &resolver).unwrap();

        let entries = scan(&dst, new_dir);
        assert_eq!(entries.len(), 4);
        assert_eq!(entries[0], (b"Rempty".to_vec(), 0u64.to_le_bytes().to_vec()));
        assert_eq!(entries[3], (b"Xmeta".to_vec(), b"x".to_vec()));
        let new_main = PageId(u64::from_le_bytes(entries[1].1.as_slice().try_into().unwrap()));
        let new_snap = PageId(u64::from_le_bytes(entries[2].1.as_slice().try_into().unwrap()));
        assert_eq!(scan(&src, data1), scan(&dst, new_main));
        assert_eq!(scan(&src, data2), scan(&dst, new_snap));

        let mut distinct = HashSet::new();
        reachable(&src, data1, &mut distinct);
        reachable(&src, data2, &mut distinct);
        assert_eq!(c.pages_copied, distinct.len() as u64);
    }

    #[test]
    fn empty_directory_copies_to_none() {
        let src = MemStore::default();
        let mut dst = MemStore::default();
        let mut c = TreeCopier::new();
        let out = c.copy_directory(&src, &mut dst, PageId::NONE, &PrefixU64Roots::new(b"R")).unwrap();
        assert_eq!(out, PageId::NONE);
    }
}
