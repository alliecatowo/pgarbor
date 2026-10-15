//! Copy-on-write insert/replace with splits (STORE-07). Lease STO-08 (sonnet).
use crate::btree::node::{
    decode_node, max_inline_value, max_key_len, node_capacity, write_node, InternalEntry,
    InternalNode, LeafEntry, LeafNode, LeafValue, Node, INTERNAL_HEADER_LEN, LEAF_HEADER_LEN,
};
use crate::btree::overflow::{release_overflow, write_overflow};
use crate::error;
use crate::format::consts::MAX_VALUE_LEN;
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
pub fn put(sink: &mut dyn PageSink, root: PageId, key: &[u8], value: &[u8]) -> Result<PutResult> {
    let ps = sink.page_size();
    let max_key = max_key_len(ps);
    if key.len() > max_key {
        return Err(error::key_too_large(key.len(), max_key));
    }
    if value.len() as u64 > MAX_VALUE_LEN {
        return Err(error::value_too_large(value.len(), MAX_VALUE_LEN));
    }
    let stored = if value.len() > max_inline_value(ps) {
        let first = write_overflow(sink, value)?;
        LeafValue::Overflow { first, len: value.len() as u32 }
    } else {
        LeafValue::Inline(value.to_vec())
    };
    if root.is_none() {
        let leaf = LeafNode { entries: vec![LeafEntry { key: key.to_vec(), value: stored }] };
        let id = write_node(sink, PageId::NONE, &Node::Leaf(leaf))?;
        return Ok(PutResult { root: id, replaced: false });
    }
    let rec = insert_rec(sink, root, key, &stored, true, 0, None)?;
    match rec.split {
        None => Ok(PutResult { root: rec.id, replaced: rec.replaced }),
        Some((sep, right)) => {
            let level = rec
                .level
                .checked_add(1)
                .ok_or_else(|| PgError::internal("b-tree level overflow"))?;
            let new_root = InternalNode {
                level,
                first_child: rec.id,
                entries: vec![InternalEntry { key: sep, child: right }],
            };
            let id = write_node(sink, PageId::NONE, &Node::Internal(new_root))?;
            Ok(PutResult { root: id, replaced: rec.replaced })
        }
    }
}

/// Maximum descent depth before the tree is declared corrupt (cycle guard).
const MAX_DEPTH: u32 = 64;

/// Result of inserting into one subtree.
struct Rec {
    /// Possibly new id of the (left) node.
    id: PageId,
    /// Level of the node `id` (0 = leaf).
    level: u16,
    /// Right sibling produced by a split, with its separator.
    split: Option<(Vec<u8>, PageId)>,
    replaced: bool,
}

fn insert_rec(
    sink: &mut dyn PageSink,
    page: PageId,
    key: &[u8],
    value: &LeafValue,
    rightmost: bool,
    depth: u32,
    expect_level: Option<u16>,
) -> Result<Rec> {
    if depth > MAX_DEPTH {
        return Err(error::invalid_page(page, "b-tree too deep, possible cycle"));
    }
    let cap = node_capacity(sink.page_size());
    let page_ref = sink.read_page(page)?;
    let node = decode_node(&page_ref)?;
    drop(page_ref);
    match node {
        Node::Leaf(mut leaf) => {
            if let Some(l) = expect_level {
                if l != 0 {
                    return Err(error::invalid_page(page, format!("leaf found where level {l} expected")));
                }
            }
            let mut replaced = false;
            let mut append = false;
            match leaf.search(key) {
                Ok(i) => {
                    replaced = true;
                    if let (LeafValue::Inline(a), LeafValue::Inline(b)) = (&leaf.entries[i].value, value) {
                        if a == b {
                            return Ok(Rec { id: page, level: 0, split: None, replaced: true });
                        }
                    }
                    if let LeafValue::Overflow { first, len } = leaf.entries[i].value {
                        release_overflow(sink, first, len)?;
                    }
                    leaf.entries[i].value = value.clone();
                }
                Err(i) => {
                    append = i == leaf.entries.len();
                    leaf.entries.insert(i, LeafEntry { key: key.to_vec(), value: value.clone() });
                }
            }
            if leaf.encoded_len() <= cap {
                let id = write_node(sink, page, &Node::Leaf(leaf))?;
                return Ok(Rec { id, level: 0, split: None, replaced });
            }
            let n = leaf.entries.len();
            let at = if rightmost && append && n >= 2 {
                n - 1
            } else {
                let lens: Vec<usize> = leaf.entries.iter().map(LeafEntry::encoded_len).collect();
                choose_split(&lens, LEAF_HEADER_LEN, cap, false).ok_or_else(|| {
                    PgError::internal("b-tree leaf split: no balanced split point fits capacity")
                })?
            };
            let right_entries = leaf.entries.split_off(at);
            let sep = right_entries[0].key.clone();
            let right = LeafNode { entries: right_entries };
            let left_id = write_node(sink, page, &Node::Leaf(leaf))?;
            let right_id = write_node(sink, PageId::NONE, &Node::Leaf(right))?;
            Ok(Rec { id: left_id, level: 0, split: Some((sep, right_id)), replaced })
        }
        Node::Internal(mut node) => {
            if let Some(l) = expect_level {
                if l != node.level {
                    return Err(error::invalid_page(
                        page,
                        format!("child has level {} where level {l} expected", node.level),
                    ));
                }
            }
            let level = node.level;
            let i = node.child_index(key);
            let last = node.entries.len();
            let child = node.child_at(i);
            let rec = insert_rec(
                sink,
                child,
                key,
                value,
                rightmost && i == last,
                depth + 1,
                Some(level.saturating_sub(1)),
            )?;
            if rec.id == child && rec.split.is_none() {
                return Ok(Rec { id: page, level, split: None, replaced: rec.replaced });
            }
            let replaced = rec.replaced;
            node.set_child(i, rec.id);
            if let Some((sep, right)) = rec.split {
                node.entries.insert(i, InternalEntry { key: sep, child: right });
            }
            if node.encoded_len() <= cap {
                let id = write_node(sink, page, &Node::Internal(node))?;
                return Ok(Rec { id, level, split: None, replaced });
            }
            let lens: Vec<usize> = node.entries.iter().map(InternalEntry::encoded_len).collect();
            let m = choose_split(&lens, INTERNAL_HEADER_LEN, cap, true).ok_or_else(|| {
                PgError::internal("b-tree internal split: no balanced split point fits capacity")
            })?;
            let mut right_entries = node.entries.split_off(m);
            let mid = right_entries.remove(0);
            let right = InternalNode { level, first_child: mid.child, entries: right_entries };
            let left_id = write_node(sink, page, &Node::Internal(node))?;
            let right_id = write_node(sink, PageId::NONE, &Node::Internal(right))?;
            Ok(Rec { id: left_id, level, split: Some((mid.key, right_id)), replaced })
        }
    }
}

/// Choose a split point over cells with encoded lengths `lens`.
///
/// Leaf (`middle == false`): returns `s` in `1..n`; left = `lens[..s]`, right = `lens[s..]`.
/// Internal (`middle == true`): returns `m` in `1..n-1`; left = `lens[..m]`, cell `m` becomes the
/// promoted separator, right = `lens[m+1..]`. Both sides include `header` bytes and must be
/// `<= cap`; the minimal size difference wins, ties go to the smaller index.
fn choose_split(lens: &[usize], header: usize, cap: usize, middle: bool) -> Option<usize> {
    let n = lens.len();
    let total: usize = lens.iter().sum();
    let hi = if middle { n.saturating_sub(1) } else { n };
    let mut best: Option<(usize, usize)> = None; // (diff, index)
    let mut left = 0usize; // sum of lens[..idx]
    for idx in 1..hi {
        left += lens[idx - 1];
        let right = if middle { total - left - lens[idx] } else { total - left };
        let (l, r) = (header + left, header + right);
        if l > cap || r > cap {
            continue;
        }
        let diff = l.abs_diff(r);
        if best.map_or(true, |(d, _)| diff < d) {
            best = Some((diff, idx));
        }
    }
    best.map(|(_, i)| i)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::btree::read::{get, RangeIter};
    use crate::format::consts::PageKind;
    use crate::format::page::{Page, PageRef};
    use crate::pager::PageSource;
    use pgarbor_common::CommitSeq;
    use std::collections::{BTreeMap, HashMap, HashSet};
    use std::ops::Bound;
    use std::sync::Arc;

    struct MemSink {
        ps: u32,
        pages: HashMap<PageId, PageRef>,
        private: HashSet<PageId>,
        next: u64,
    }

    impl MemSink {
        fn new(ps: u32) -> Self {
            MemSink { ps, pages: HashMap::new(), private: HashSet::new(), next: 1 }
        }
        /// Mark everything written so far as committed (shared).
        fn commit(&mut self) {
            self.private.clear();
        }
    }

    impl PageSource for MemSink {
        fn page_size(&self) -> u32 {
            self.ps
        }
        fn read_page(&self, id: PageId) -> Result<PageRef> {
            self.pages.get(&id).cloned().ok_or_else(|| error::invalid_page(id, "no such page"))
        }
    }

    impl PageSink for MemSink {
        fn alloc_page(&mut self) -> Result<PageId> {
            let id = PageId(self.next);
            self.next += 1;
            self.private.insert(id);
            Ok(id)
        }
        fn write_page(&mut self, id: PageId, kind: PageKind, payload: Vec<u8>) -> Result<()> {
            if !self.private.contains(&id) {
                return Err(PgError::internal(format!("write to shared page {}", id.0)));
            }
            self.pages.insert(id, Arc::new(Page { id, kind, commit_seq: CommitSeq(0), payload }));
            Ok(())
        }
        fn is_private(&self, id: PageId) -> bool {
            self.private.contains(&id)
        }
        fn release_page(&mut self, _id: PageId) -> Result<()> {
            Ok(())
        }
    }

    fn rng(state: &mut u64) -> u64 {
        *state ^= *state << 13;
        *state ^= *state >> 7;
        *state ^= *state << 17;
        *state
    }

    fn scan(sink: &MemSink, root: PageId) -> Vec<(Vec<u8>, Vec<u8>)> {
        RangeIter::new(sink, root, Bound::Unbounded, Bound::Unbounded)
            .unwrap()
            .collect::<Result<Vec<_>>>()
            .unwrap()
    }

    #[test]
    fn random_inserts_match_model() {
        let mut sink = MemSink::new(4096);
        let mut model = BTreeMap::new();
        let mut root = PageId::NONE;
        let mut st = 0x9E37_79B9_7F4A_7C15u64;
        for _ in 0..10_000 {
            let k = (rng(&mut st) % 6000).to_be_bytes().to_vec();
            let vl = (rng(&mut st) % 40) as usize;
            let v = vec![(rng(&mut st) & 0xff) as u8; vl];
            let r = put(&mut sink, root, &k, &v).unwrap();
            assert_eq!(r.replaced, model.insert(k, v).is_some());
            root = r.root;
        }
        let got = scan(&sink, root);
        let want: Vec<_> = model.into_iter().collect();
        assert_eq!(got, want);
    }

    #[test]
    fn ascending_inserts_fill_leaves() {
        let mut sink = MemSink::new(4096);
        let mut root = PageId::NONE;
        for i in 0u64..3000 {
            root = put(&mut sink, root, &i.to_be_bytes(), &[7u8; 16]).unwrap().root;
        }
        let cap = node_capacity(4096);
        let mut leaves = 0;
        let mut total = 0usize;
        for p in sink.pages.values() {
            if p.kind == PageKind::Leaf {
                leaves += 1;
                total += LeafNode::decode(p.id, &p.payload).unwrap().encoded_len();
            }
        }
        let fill = total as f64 / (leaves * cap) as f64;
        assert!(fill > 0.9, "fill {fill}");
        assert_eq!(scan(&sink, root).len(), 3000);
    }

    #[test]
    fn replace_values_and_noop() {
        let mut sink = MemSink::new(4096);
        let r1 = put(&mut sink, PageId::NONE, b"a", b"1").unwrap();
        assert!(!r1.replaced);
        let r2 = put(&mut sink, r1.root, b"a", b"1").unwrap();
        assert!(r2.replaced);
        assert_eq!(r2.root, r1.root);
        let r3 = put(&mut sink, r2.root, b"a", b"2").unwrap();
        assert!(r3.replaced);
        assert_eq!(get(&sink, r3.root, b"a").unwrap(), Some(b"2".to_vec()));
        let r4 = put(&mut sink, r3.root, b"", b"e").unwrap();
        assert!(!r4.replaced);
        assert_eq!(get(&sink, r4.root, b"").unwrap(), Some(b"e".to_vec()));
    }

    #[test]
    fn overflow_values_roundtrip_and_replace() {
        let mut sink = MemSink::new(4096);
        let big: Vec<u8> = (0..20_000u32).map(|i| (i % 251) as u8).collect();
        let r1 = put(&mut sink, PageId::NONE, b"k", &big).unwrap();
        assert_eq!(get(&sink, r1.root, b"k").unwrap(), Some(big.clone()));
        let r2 = put(&mut sink, r1.root, b"k", b"small").unwrap();
        assert!(r2.replaced);
        assert_eq!(get(&sink, r2.root, b"k").unwrap(), Some(b"small".to_vec()));
    }

    #[test]
    fn key_size_limits() {
        let mut sink = MemSink::new(4096);
        let max = max_key_len(4096);
        let ok = put(&mut sink, PageId::NONE, &vec![1u8; max], b"v").unwrap();
        let err = put(&mut sink, ok.root, &vec![1u8; max + 1], b"v").unwrap_err();
        assert_eq!(err.sqlstate.code(), "54000");
    }

    #[test]
    fn cow_never_writes_committed_pages() {
        let mut sink = MemSink::new(4096);
        let mut root = PageId::NONE;
        for i in 0u64..2000 {
            root = put(&mut sink, root, &i.to_be_bytes(), &[1u8; 20]).unwrap().root;
        }
        sink.commit();
        let old_root = root;
        let before: HashMap<PageId, PageRef> = sink.pages.clone();
        let mut st = 12345u64;
        for _ in 0..500 {
            let k = (rng(&mut st) % 3000).to_be_bytes().to_vec();
            root = put(&mut sink, root, &k, &[2u8; 33]).unwrap().root;
        }
        // MemSink::write_page rejects shared pages, so reaching here proves none was written;
        // additionally the old tree is byte-identical.
        for (id, p) in &before {
            assert_eq!(sink.pages[id], *p);
        }
        assert_eq!(scan(&sink, old_root).len(), 2000);
        assert!(scan(&sink, root).len() >= 2000);
    }

    #[test]
    fn choose_split_balances() {
        assert_eq!(choose_split(&[100, 100, 100, 100], 4, 1000, false), Some(2));
        assert_eq!(choose_split(&[400, 100, 100, 400], 0, 600, false), Some(2));
        assert_eq!(choose_split(&[10, 10, 10, 10, 10], 0, 100, true), Some(2));
        assert_eq!(choose_split(&[10], 0, 100, true), None);
    }
}
