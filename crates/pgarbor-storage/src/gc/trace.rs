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
    Err(PgError::unimplemented("STO-14", "live root collection"))
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
    Err(PgError::unimplemented("STO-14", "tree page walk"))
}

// CONTRACT
/// Mark every page reachable from `roots`. `page_count` sizes the bitmap; a reference beyond it is
/// XX001 `error::page_out_of_range`. With `per_root == false`: one global walk, skipping subtrees
/// already marked (O(reachable pages)). With `per_root == true`: additionally walk each root with
/// its own visited set to count `pages`, and maintain an owner array (`u32` per page: unowned /
/// root index / SHARED) to derive `exclusive_pages` (O(sum of per-root reachable pages)).
#[allow(unused_variables)]
pub fn trace(src: &dyn PageSource, page_count: u64, roots: &[NamedRoot], per_root: bool) -> Result<TraceResult> {
    Err(PgError::unimplemented("STO-14", "reachability trace"))
}
