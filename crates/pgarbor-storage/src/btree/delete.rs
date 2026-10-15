//! Copy-on-write delete with rebalancing (STORE-08). Lease STO-09 (sonnet).
#[allow(unused_imports)]
use crate::btree::node::{
    node_capacity, underflow_threshold, write_node, InternalEntry, InternalNode, LeafNode, LeafValue, Node,
};
#[allow(unused_imports)]
use crate::btree::overflow::release_overflow;
use crate::pager::PageSink;
use pgarbor_common::{PageId, PgError, Result};
use std::ops::Bound;

// CONTRACT
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DeleteResult {
    /// New root; `PageId::NONE` when the tree became empty; unchanged when the key was absent.
    pub root: PageId,
    pub existed: bool,
}

// CONTRACT
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RangeDeleteResult {
    pub root: PageId,
    pub deleted: u64,
}

// CONTRACT
/// Delete `key`. Absent key → `{root, existed: false}` with NO page writes.
///
/// Algorithm (recursive, returns the rewritten child + whether it underflows):
/// - Leaf: remove the entry; if its value was Overflow, `release_overflow`. Write via
///   `write_node(sink, page, ..)` unless the leaf became empty and is not the root (then the
///   parent removes it and calls `sink.release_page(page)`).
/// - Internal at child index `i`: after the child returns, if the child is empty or its
///   `encoded_len() < underflow_threshold(ps)`, rebalance with a sibling: the left sibling `i-1`
///   when `i > 0`, else the right sibling `i+1`. Let L, R be (left, right) of the pair and
///   `sep` the parent separator between them.
///   * Merge if the combined node fits `node_capacity(ps)`: leaves concatenate; internal nodes
///     concatenate as `L.entries + [(sep, R.first_child)] + R.entries`. Write the merged node
///     cow over L's page id, `release_page` R's page id (if R had been rewritten privately in this
///     same operation, releasing makes it reusable), remove the separator entry and R's child
///     pointer from the parent.
///   * Otherwise redistribute so both halves are as byte-balanced as possible (both
///     <= capacity): leaves move boundary entries and the new separator is the first key of the
///     new right leaf; internal nodes rotate through the parent (the old `sep` moves down, a
///     boundary key moves up). Write both cow, update the separator.
///   * A sibling is only read, never modified unless merged/redistributed (then cow).
/// - Root fix-up: internal root with zero separators → new root = its only child
///   (`release_page(old root)`); empty leaf root → `PageId::NONE` (`release_page`).
///
/// Errors: structural corruption → XX001 (checks as in `read::get`).
/// Example: tree {a,b,c}; delete(b"b") → existed; get(b"b") == None; delete(b"zz") → !existed.
#[allow(unused_variables)]
pub fn delete(sink: &mut dyn PageSink, root: PageId, key: &[u8]) -> Result<DeleteResult> {
    Err(PgError::unimplemented("STO-09", "b-tree delete"))
}

// CONTRACT
/// Delete every key within the bounds (DROP TABLE / TRUNCATE of a relation prefix). Correct v1
/// algorithm: loop { collect up to 1024 keys from `RangeIter::new(sink, root, cursor, upper)`;
/// if none, stop; `delete` each; cursor = Excluded(last collected key) }. (A subtree-dropping fast
/// path is a permitted later optimization.) Returns new root and count.
#[allow(unused_variables)]
pub fn delete_range(
    sink: &mut dyn PageSink,
    root: PageId,
    lower: Bound<&[u8]>,
    upper: Bound<&[u8]>,
) -> Result<RangeDeleteResult> {
    Err(PgError::unimplemented("STO-09", "b-tree range delete"))
}
