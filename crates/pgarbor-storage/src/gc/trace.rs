//! Reachability tracing (STORE-14). Lease STO-14 (`collect_roots`, `visit_tree`, `trace`).
//!
//! Live roots of a container = the directory tree (superblock `directory_root`) + every root the
//! [`RootResolver`] reports for directory entries (branch heads, named snapshots) + caller extras.
//! Reader pins never add roots in format v1 (see `container::pins`).
#[allow(unused_imports)]
use crate::btree::node::{internal_spans, leaf_spans, span_value, LeafValueRef};
#[allow(unused_imports)]
use crate::btree::overflow::chain_pages;
#[allow(unused_imports)]
use crate::btree::read::RangeIter;
use crate::format::consts::PageKind;
use crate::pager::PageSource;
use crate::roots::{NamedRoot, RootResolver};
use pgarbor_common::{PageId, PgError, Result};
use serde::Serialize;

use crate::error;
use std::ops::Bound;

/// Maximum b-tree depth a walk descends to (root = depth 0), matching `get` / `RangeIter`.
const MAX_TREE_DEPTH: u32 = 64;

/// Bytes of `u32::MAX` used as the "unowned" marker in the owner array.
const OWNER_NONE: u32 = u32::MAX;
/// Owner-array marker for a page reachable from more than one root.
const OWNER_SHARED: u32 = u32::MAX - 1;

/// `error::page_out_of_range` unless `id` is in `1..=page_count`.
fn check_id(id: PageId, page_count: u64) -> Result<()> {
    if id.0 == 0 || id.0 > page_count {
        Err(error::page_out_of_range(id, page_count))
    } else {
        Ok(())
    }
}

/// Visit every page of one overflow chain (chain order). A `false` on the chain's first page
/// skips the remaining pages of that chain; later `false` results are ignored (chain pages have
/// no children).
fn visit_chain(
    src: &dyn PageSource,
    first: PageId,
    len: u32,
    visit: &mut dyn FnMut(PageId, PageKind) -> Result<bool>,
) -> Result<()> {
    let pages = chain_pages(src, first, len)?;
    for (i, pid) in pages.iter().enumerate() {
        let cont = visit(*pid, PageKind::Overflow)?;
        if i == 0 && !cont {
            break;
        }
    }
    Ok(())
}

// CONTRACT
/// Dense bitmap over page ids `1..=page_count`. Fully implemented.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ReachSet {
    bits: Vec<u64>,
    page_count: u64,
    count: u64,
}

impl ReachSet {
    pub fn new(page_count: u64) -> ReachSet {
        ReachSet { bits: vec![0; (page_count as usize).div_ceil(64) + 1], page_count, count: 0 }
    }
    pub fn page_count(&self) -> u64 {
        self.page_count
    }
    /// Mark `id`; returns true if newly marked. Ids outside `1..=page_count` return false and are
    /// not stored (callers report them as out-of-range issues).
    pub fn insert(&mut self, id: PageId) -> bool {
        if id.0 == 0 || id.0 > self.page_count {
            return false;
        }
        let (w, b) = ((id.0 / 64) as usize, id.0 % 64);
        if self.bits[w] & (1 << b) != 0 {
            return false;
        }
        self.bits[w] |= 1 << b;
        self.count += 1;
        true
    }
    pub fn contains(&self, id: PageId) -> bool {
        if id.0 == 0 || id.0 > self.page_count {
            return false;
        }
        self.bits[(id.0 / 64) as usize] & (1 << (id.0 % 64)) != 0
    }
    /// Number of marked pages.
    pub fn len(&self) -> u64 {
        self.count
    }
    pub fn is_empty(&self) -> bool {
        self.count == 0
    }
    /// Pages in `1..=page_count` not marked.
    pub fn unmarked(&self) -> u64 {
        self.page_count - self.count
    }
    /// Ascending iterator over unmarked ids.
    pub fn iter_unmarked(&self) -> impl Iterator<Item = PageId> + '_ {
        (1..=self.page_count).map(PageId).filter(move |p| !self.contains(*p))
    }
}

// CONTRACT
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize)]
pub struct KindCounts {
    pub leaf: u64,
    pub internal: u64,
    pub overflow: u64,
    pub other: u64,
}

impl KindCounts {
    pub fn add(&mut self, kind: PageKind) {
        match kind {
            PageKind::Leaf => self.leaf += 1,
            PageKind::Internal => self.internal += 1,
            PageKind::Overflow => self.overflow += 1,
            PageKind::FreeList => self.other += 1,
        }
    }
    pub fn total(&self) -> u64 {
        self.leaf + self.internal + self.overflow + self.other
    }
}

// CONTRACT
/// Space attributed to one root.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct RootUsage {
    pub label: String,
    pub root: PageId,
    /// Pages reachable from this root.
    pub pages: u64,
    /// Pages reachable from this root and from no other root ("retained by this branch only").
    pub exclusive_pages: u64,
}

// CONTRACT
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TraceResult {
    pub reachable: ReachSet,
    pub kinds: KindCounts,
    /// Empty unless `per_root` was requested; same order as the input roots.
    pub roots: Vec<RootUsage>,
}

// CONTRACT
/// Live roots: `NamedRoot{label: "directory", root: directory_root}` first (omitted when NONE),
/// then, scanning the directory tree in key order, whatever `resolver.roots(key, value)` appends.
/// Duplicates are kept (two branches sharing a root are both listed).
#[allow(unused_variables)]
pub fn collect_roots(
    src: &dyn PageSource,
    directory_root: PageId,
    resolver: &dyn RootResolver,
) -> Result<Vec<NamedRoot>> {
    let mut out = Vec::new();
    if !directory_root.is_none() {
        out.push(NamedRoot { label: "directory".to_string(), root: directory_root });
    }
    for item in RangeIter::new(src, directory_root, Bound::Unbounded, Bound::Unbounded)? {
        let (key, value) = item?;
        resolver.roots(&key, &value, &mut out)?;
    }
    Ok(out)
}

// CONTRACT
/// Depth-first pre-order walk over every page of the tree at `root` (nodes and overflow chain
/// pages). `visit(id, kind)` is called once per page encounter; returning `Ok(false)` means "do
/// not descend below this page" (callers use it to skip already-visited shared subtrees;
/// overflow chain pages after a `false` on the chain's first page are skipped too). NONE root →
/// no calls. Children are visited in ascending key order; overflow chains of a leaf right after
/// the leaf, in cell order. Depth > 64 → XX001 ("b-tree too deep, possible cycle").
#[allow(unused_variables)]
pub fn visit_tree(
    src: &dyn PageSource,
    root: PageId,
    visit: &mut dyn FnMut(PageId, PageKind) -> Result<bool>,
) -> Result<()> {
    if root.is_none() {
        return Ok(());
    }
    // Explicit stack of (page, depth); children are pushed in reverse so the first child pops first.
    let mut stack: Vec<(PageId, u32)> = vec![(root, 0)];
    while let Some((id, depth)) = stack.pop() {
        if depth > MAX_TREE_DEPTH {
            return Err(error::corrupt("b-tree too deep, possible cycle"));
        }
        let page = src.read_page(id)?;
        let kind = page.kind;
        if !visit(id, kind)? {
            continue;
        }
        match kind {
            PageKind::Leaf => {
                let spans = leaf_spans(id, &page.payload)?;
                for span in &spans {
                    if let LeafValueRef::Overflow { first, len } = span_value(&page.payload, span) {
                        visit_chain(src, first, len, &mut *visit)?;
                    }
                }
            }
            PageKind::Internal => {
                let (_level, first_child, spans) = internal_spans(id, &page.payload)?;
                for span in spans.iter().rev() {
                    stack.push((span.child, depth + 1));
                }
                stack.push((first_child, depth + 1));
            }
            other => {
                return Err(error::invalid_page(id, format!("expected a b-tree node, found {} page", other.name())));
            }
        }
    }
    Ok(())
}

// CONTRACT
/// Mark every page reachable from `roots`. `page_count` sizes the bitmap; a reference beyond it is
/// XX001 `error::page_out_of_range`. With `per_root == false`: one global walk, skipping subtrees
/// already marked (O(reachable pages)). With `per_root == true`: additionally walk each root with
/// its own visited set to count `pages`, and maintain an owner array (`u32` per page: unowned /
/// root index / SHARED) to derive `exclusive_pages` (O(sum of per-root reachable pages)).
#[allow(unused_variables)]
pub fn trace(src: &dyn PageSource, page_count: u64, roots: &[NamedRoot], per_root: bool) -> Result<TraceResult> {
    // Global pass: one walk over all roots, skipping anything already marked.
    let mut reachable = ReachSet::new(page_count);
    let mut kinds = KindCounts::default();
    for r in roots {
        if r.root.is_none() {
            continue;
        }
        check_id(r.root, page_count)?;
        visit_tree(src, r.root, &mut |id: PageId, kind: PageKind| -> Result<bool> {
            check_id(id, page_count)?;
            if reachable.insert(id) {
                kinds.add(kind);
                Ok(true)
            } else {
                Ok(false)
            }
        })?;
    }

    if !per_root {
        return Ok(TraceResult { reachable, kinds, roots: Vec::new() });
    }

    // Per-root pass. `stamp[id]` = index+1 of the last root whose walk visited `id`, which gives
    // each root its own visited set without reallocating a bitmap per root.
    if roots.len() >= OWNER_SHARED as usize {
        return Err(PgError::internal("too many roots to trace"));
    }
    let n = page_count as usize;
    let mut owner = vec![OWNER_NONE; n + 1];
    let mut stamp = vec![0u32; n + 1];
    let mut pages_per_root: Vec<u64> = Vec::with_capacity(roots.len());
    for (i, r) in roots.iter().enumerate() {
        let idx = i as u32;
        let tag = idx + 1;
        let mut pages: u64 = 0;
        if !r.root.is_none() {
            check_id(r.root, page_count)?;
            visit_tree(src, r.root, &mut |id: PageId, _kind: PageKind| -> Result<bool> {
                check_id(id, page_count)?;
                let ix = id.0 as usize;
                if stamp[ix] == tag {
                    return Ok(false);
                }
                stamp[ix] = tag;
                pages += 1;
                owner[ix] = match owner[ix] {
                    OWNER_NONE => idx,
                    o if o == idx => o,
                    _ => OWNER_SHARED,
                };
                Ok(true)
            })?;
        }
        pages_per_root.push(pages);
    }

    let mut exclusive = vec![0u64; roots.len()];
    for &o in &owner[1..] {
        if o != OWNER_NONE && o != OWNER_SHARED {
            exclusive[o as usize] += 1;
        }
    }
    let usage: Vec<RootUsage> = roots
        .iter()
        .enumerate()
        .map(|(i, r)| RootUsage {
            label: r.label.clone(),
            root: r.root,
            pages: pages_per_root[i],
            exclusive_pages: exclusive[i],
        })
        .collect();
    Ok(TraceResult { reachable, kinds, roots: usage })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::btree::node::{InternalEntry, InternalNode, LeafEntry, LeafNode, LeafValue};
    use crate::format::page::{Page, PageRef};
    use crate::roots::PrefixU64Roots;
    use pgarbor_common::CommitSeq;
    use std::collections::BTreeMap;
    use std::sync::Arc;

    const PS: u32 = 4096;

    struct MemSrc {
        pages: BTreeMap<u64, PageRef>,
    }

    impl PageSource for MemSrc {
        fn page_size(&self) -> u32 {
            PS
        }
        fn read_page(&self, id: PageId) -> Result<PageRef> {
            self.pages.get(&id.0).cloned().ok_or_else(|| error::page_out_of_range(id, self.pages.len() as u64))
        }
    }

    fn put(src: &mut MemSrc, id: u64, kind: PageKind, payload: Vec<u8>) {
        src.pages.insert(id, Arc::new(Page { id: PageId(id), kind, commit_seq: CommitSeq(1), payload }));
    }

    fn leaf_payload(kvs: Vec<(&str, LeafValue)>) -> Vec<u8> {
        let entries = kvs
            .into_iter()
            .map(|(k, v)| LeafEntry { key: k.as_bytes().to_vec(), value: v })
            .collect();
        LeafNode { entries }.encode(PS).unwrap()
    }

    fn internal_payload(first: u64, seps: Vec<(&str, u64)>) -> Vec<u8> {
        let entries = seps
            .into_iter()
            .map(|(k, c)| InternalEntry { key: k.as_bytes().to_vec(), child: PageId(c) })
            .collect();
        InternalNode { level: 1, first_child: PageId(first), entries }.encode(PS).unwrap()
    }

    fn inline(s: &str) -> LeafValue {
        LeafValue::Inline(s.as_bytes().to_vec())
    }

    /// Pages 1..=9:
    /// 1 leaf "a"; 2 leaf "b"; 3 internal -> {1, 2} (shared subtree S);
    /// 4 leaf "c"; 5 internal -> {3, 4} (root A); 6 internal -> {3, 7} (root B);
    /// 7 leaf "d"; 8 overflow chain page (3 bytes "xyz"); 9 leaf "o" with overflow value (root C).
    fn sample() -> MemSrc {
        let mut src = MemSrc { pages: BTreeMap::new() };
        put(&mut src, 1, PageKind::Leaf, leaf_payload(vec![("a", inline("1"))]));
        put(&mut src, 2, PageKind::Leaf, leaf_payload(vec![("b", inline("2"))]));
        put(&mut src, 3, PageKind::Internal, internal_payload(1, vec![("b", 2)]));
        put(&mut src, 4, PageKind::Leaf, leaf_payload(vec![("c", inline("3"))]));
        put(&mut src, 5, PageKind::Internal, internal_payload(3, vec![("c", 4)]));
        put(&mut src, 6, PageKind::Internal, internal_payload(3, vec![("d", 7)]));
        put(&mut src, 7, PageKind::Leaf, leaf_payload(vec![("d", inline("4"))]));
        let mut chain = vec![0u8; 8];
        chain.extend_from_slice(b"xyz");
        put(&mut src, 8, PageKind::Overflow, chain);
        put(
            &mut src,
            9,
            PageKind::Leaf,
            leaf_payload(vec![("o", LeafValue::Overflow { first: PageId(8), len: 3 })]),
        );
        src
    }

    fn named(label: &str, root: u64) -> NamedRoot {
        NamedRoot { label: label.to_string(), root: PageId(root) }
    }

    #[test]
    fn shared_subtree_is_not_exclusive() {
        let src = sample();
        let roots = vec![named("A", 5), named("B", 6)];
        let t = trace(&src, 9, &roots, true).unwrap();
        // Distinct pages: 1,2,3,4,5,6,7 = 7.
        assert_eq!(t.reachable.len(), 7);
        assert_eq!(t.reachable.unmarked(), 2);
        assert_eq!(t.kinds.total(), 7);
        assert_eq!(t.kinds.leaf, 4);
        assert_eq!(t.kinds.internal, 3);
        assert_eq!(t.kinds.overflow, 0);
        assert_eq!(t.roots.len(), 2);
        // Each root reaches 5 pages: {5,3,1,2,4} and {6,3,1,2,7}.
        for u in &t.roots {
            assert_eq!(u.pages, 5);
            assert_eq!(u.exclusive_pages, 2);
            assert!(u.exclusive_pages <= u.pages);
        }
        assert_eq!(t.roots[0].label, "A");
        assert_eq!(t.roots[1].root, PageId(6));
        // Shared pages {1,2,3} are in neither exclusive count: 2 + 2 + 3 = 7 total.
        assert_eq!(t.roots[0].exclusive_pages + t.roots[1].exclusive_pages + 3, t.reachable.len());
    }

    #[test]
    fn overflow_pages_counted_as_overflow() {
        let src = sample();
        let roots = vec![named("A", 5), named("B", 6), named("C", 9)];
        let t = trace(&src, 9, &roots, true).unwrap();
        assert_eq!(t.reachable.len(), 9);
        assert_eq!(t.reachable.unmarked(), 0);
        assert_eq!(t.kinds.total(), 9);
        assert_eq!(t.kinds.overflow, 1);
        assert_eq!(t.kinds.leaf, 5);
        assert_eq!(t.kinds.internal, 3);
        // Root C owns the leaf 9 and its overflow page 8 exclusively.
        assert_eq!(t.roots[2].pages, 2);
        assert_eq!(t.roots[2].exclusive_pages, 2);
    }

    #[test]
    fn global_trace_matches_per_root_totals() {
        let src = sample();
        let roots = vec![named("A", 5), named("B", 6), named("C", 9)];
        let global = trace(&src, 9, &roots, false).unwrap();
        let detailed = trace(&src, 9, &roots, true).unwrap();
        assert!(global.roots.is_empty());
        assert_eq!(global.reachable, detailed.reachable);
        assert_eq!(global.kinds, detailed.kinds);
    }

    #[test]
    fn none_roots_are_skipped() {
        let src = sample();
        let roots = vec![named("empty", 0)];
        let t = trace(&src, 9, &roots, true).unwrap();
        assert!(t.reachable.is_empty());
        assert_eq!(t.roots[0].pages, 0);
        assert_eq!(t.roots[0].exclusive_pages, 0);
    }

    #[test]
    fn root_beyond_page_count_is_xx001() {
        let src = sample();
        let err = trace(&src, 9, &[named("bad", 20)], false).unwrap_err();
        assert_eq!(err.sqlstate.code(), "XX001");
        assert!(err.message.contains("beyond the end of committed data"));
    }

    #[test]
    fn collect_roots_reads_directory() {
        let mut src = sample();
        // Directory leaf 10: "Rmain" -> root 9, "Rsnap" -> root 5, "Xother" -> not a root.
        let dir = leaf_payload(vec![
            ("Rmain", LeafValue::Inline(9u64.to_le_bytes().to_vec())),
            ("Rsnap", LeafValue::Inline(5u64.to_le_bytes().to_vec())),
            ("Xother", LeafValue::Inline(b"junk".to_vec())),
        ]);
        put(&mut src, 10, PageKind::Leaf, dir);
        let resolver = PrefixU64Roots::new(b"R");
        let roots = collect_roots(&src, PageId(10), &resolver).unwrap();
        assert_eq!(roots.len(), 3);
        assert_eq!(roots[0], NamedRoot { label: "directory".into(), root: PageId(10) });
        assert_eq!(roots[1], NamedRoot { label: "main".into(), root: PageId(9) });
        assert_eq!(roots[2], NamedRoot { label: "snap".into(), root: PageId(5) });
        assert!(collect_roots(&src, PageId::NONE, &resolver).unwrap().is_empty());
    }
}
