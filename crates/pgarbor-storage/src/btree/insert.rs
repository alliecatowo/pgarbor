//! Copy-on-write insert/replace with splits (STORE-07). Lease STO-08 (sonnet).
#[allow(unused_imports)]
use crate::btree::node::{
    max_inline_value, max_key_len, node_capacity, write_node, InternalEntry, InternalNode, LeafEntry,
    LeafNode, LeafValue, Node,
};
#[allow(unused_imports)]
use crate::btree::overflow::{release_overflow, write_overflow};
#[allow(unused_imports)]
use crate::error;
use crate::pager::PageSink;
use pgarbor_common::{PageId, PgError, Result};

// CONTRACT
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PutResult {
    /// New root (may equal the old root when every touched page was private or the put was a
    /// no-op).
    pub root: PageId,
    /// The key existed before (its value was replaced).
    pub replaced: bool,
}

// CONTRACT
/// Insert or replace `key → value` in the tree at `root`, returning the new root.
///
/// Errors: `key.len() > max_key_len(ps)` → `error::key_too_large(len, max)` (54000);
/// `value.len() > MAX_VALUE_LEN` → `error::value_too_large`; empty keys ARE allowed;
/// structural corruption found while descending → XX001 (same checks as `read::get`).
///
/// Algorithm (deterministic; the resulting page layout is part of the contract only through the
/// doctor invariants, not byte-for-byte):
/// 1. Build the stored value: `value.len() > max_inline_value(ps)` → `write_overflow` →
///    `LeafValue::Overflow{first, len}`; else `LeafValue::Inline`.
/// 2. `root == NONE` → write a one-entry leaf with `sink.write_new`, return `{root: new, replaced:
///    false}`.
/// 3. Recursive descent `insert_rec(page, rightmost) -> (new_id, Option<(sep, right_id)>, replaced)`
///    where `rightmost` = every ancestor chose its last child:
///    - Leaf: decode to `LeafNode`; `search(key)`. Found: if old and new are both Inline and equal
///      → return the page unchanged (`new_id == page`, no writes, replaced = true). Otherwise if
///      the old value was Overflow, `release_overflow` it; replace. Not found: insert at the
///      insertion point.
///    - Internal: decode, `i = child_index(key)`, recurse into child i with
///      `rightmost && i == last`. If the child id is unchanged and there is no split, return this
///      page unchanged WITHOUT rewriting it (crucial: repeated inserts into private pages cost no
///      parent rewrites). Else `set_child(i, new_child)` and if split insert
///      `InternalEntry{key: sep, child: right}` at entries index `i`.
///    - If `encoded_len() <= node_capacity(ps)`: `write_node(sink, page, &node)` (cow).
///    - Else split:
///      * Leaf, append case (`rightmost` and the new key is greater than all old keys): left keeps
///        all old entries, right gets only the new entry (keeps sequential rowid inserts ~100%
///        full, like PG's rightmost-page split heuristic in nbtsplitloc.c).
///      * Otherwise choose split index `s` (1 <= s < n) minimizing |bytes(left) - bytes(right)|
///        with both sides `<= capacity`; ties → smaller `s`. Such `s` always exists (cell size
///        rules in `node.rs`).
///      * Leaf: left = entries[..s], right = entries[s..], sep = right[0].key.
///      * Internal: pick a middle entry index `m` by the same byte balancing; left = first_child +
///        entries[..m]; sep = entries[m].key; right = {first_child: entries[m].child,
///        entries[m+1..]}; both keep `level`.
///      * Left is written with `write_node(sink, page, ..)` (cow), right with `PageId::NONE` (new).
/// 4. Root split → new root `InternalNode{level: old_level + 1, first_child: left, entries:
///    [(sep, right)]}` written fresh.
///
/// Example: empty tree, put(b"a", b"1") then put(b"a", b"2") → second result `replaced == true`
/// and `get(b"a") == b"2"`; the first root is still readable with value "1" if it was committed.
#[allow(unused_variables)]
pub fn put(sink: &mut dyn PageSink, root: PageId, key: &[u8], value: &[u8]) -> Result<PutResult> {
    Err(PgError::unimplemented("STO-08", "b-tree insert"))
}
