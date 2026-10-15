//! Copy-on-write delete with rebalancing (STORE-08). Lease STO-09 (sonnet).
use crate::btree::node::{
    decode_node, node_capacity, underflow_threshold, write_node, InternalEntry, InternalNode, LeafEntry, LeafNode,
    LeafValue, Node, INTERNAL_HEADER_LEN, LEAF_HEADER_LEN,
};
use crate::btree::overflow::release_overflow;
use crate::btree::read::RangeIter;
use crate::error;
use crate::pager::{PageSink, PageSource};
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
pub fn delete(sink: &mut dyn PageSink, root: PageId, key: &[u8]) -> Result<DeleteResult> {
    if root.is_none() {
        return Ok(DeleteResult { root, existed: false });
    }
    let out = match del_rec(sink, root, key, None, 0)? {
        None => return Ok(DeleteResult { root, existed: false }),
        Some(o) => o,
    };
    let dirty = out.dirty;
    let new_root = match out.node {
        Node::Leaf(l) if l.entries.is_empty() => {
            sink.release_page(root)?;
            PageId::NONE
        }
        Node::Internal(n) if n.entries.is_empty() => {
            // The only remaining child was already written by the rebalance step.
            sink.release_page(root)?;
            n.first_child
        }
        node => {
            if dirty {
                write_node(sink, root, &node)?
            } else {
                root
            }
        }
    };
    Ok(DeleteResult { root: new_root, existed: true })
}

// CONTRACT
/// Delete every key within the bounds (DROP TABLE / TRUNCATE of a relation prefix). Correct v1
/// algorithm: loop { collect up to 1024 keys from `RangeIter::new(sink, root, cursor, upper)`;
/// if none, stop; `delete` each; cursor = Excluded(last collected key) }. (A subtree-dropping fast
/// path is a permitted later optimization.) Returns new root and count.
pub fn delete_range(
    sink: &mut dyn PageSink,
    root: PageId,
    lower: Bound<&[u8]>,
    upper: Bound<&[u8]>,
) -> Result<RangeDeleteResult> {
    const BATCH: usize = 1024;
    let mut root = root;
    let mut deleted: u64 = 0;
    let mut cursor: Bound<Vec<u8>> = match lower {
        Bound::Included(k) => Bound::Included(k.to_vec()),
        Bound::Excluded(k) => Bound::Excluded(k.to_vec()),
        Bound::Unbounded => Bound::Unbounded,
    };
    loop {
        if root.is_none() {
            break;
        }
        let mut keys: Vec<Vec<u8>> = Vec::new();
        {
            let src: &dyn PageSource = &*sink;
            let lo: Bound<&[u8]> = match &cursor {
                Bound::Included(k) => Bound::Included(k.as_slice()),
                Bound::Excluded(k) => Bound::Excluded(k.as_slice()),
                Bound::Unbounded => Bound::Unbounded,
            };
            let iter = RangeIter::new(src, root, lo, upper)?;
            for item in iter.take(BATCH) {
                keys.push(item?.0);
            }
        }
        let last = match keys.last() {
            Some(k) => k.clone(),
            None => break,
        };
        for k in &keys {
            let r = delete(sink, root, k)?;
            root = r.root;
            if r.existed {
                deleted += 1;
            }
        }
        cursor = Bound::Excluded(last);
    }
    Ok(RangeDeleteResult { root, deleted })
}

/// Maximum descent depth before declaring a cycle (same bound as `read::get`).
const MAX_DEPTH: usize = 64;

/// Result of a recursive delete step: the modified node (not yet written) and whether its page
/// needs to be (re)written. `dirty == false` means the page itself is untouched.
struct Outcome {
    node: Node,
    dirty: bool,
}

fn node_level(n: &Node) -> u16 {
    match n {
        Node::Leaf(_) => 0,
        Node::Internal(i) => i.level,
    }
}

fn is_underflow(n: &Node, ps: u32) -> bool {
    let thr = underflow_threshold(ps);
    match n {
        Node::Leaf(l) => l.entries.is_empty() || l.encoded_len() < thr,
        Node::Internal(i) => i.entries.is_empty() || i.encoded_len() < thr,
    }
}

fn del_rec(
    sink: &mut dyn PageSink,
    page: PageId,
    key: &[u8],
    expect_level: Option<u16>,
    depth: usize,
) -> Result<Option<Outcome>> {
    if depth > MAX_DEPTH {
        return Err(error::invalid_page(page, "b-tree too deep, possible cycle"));
    }
    let node = {
        let pg = sink.read_page(page)?;
        decode_node(&pg)?
    };
    if let Some(e) = expect_level {
        let got = node_level(&node);
        if got != e {
            return Err(error::invalid_page(page, format!("b-tree node has level {got}, expected {e}")));
        }
    }
    match node {
        Node::Leaf(mut l) => match l.search(key) {
            Err(_) => Ok(None),
            Ok(i) => {
                let e = l.entries.remove(i);
                if let LeafValue::Overflow { first, len } = e.value {
                    release_overflow(sink, first, len)?;
                }
                Ok(Some(Outcome { node: Node::Leaf(l), dirty: true }))
            }
        },
        Node::Internal(mut n) => {
            let i = n.child_index(key);
            let cp = n.child_at(i);
            let child_level =
                n.level.checked_sub(1).ok_or_else(|| error::invalid_page(page, "internal node with level 0"))?;
            let out = match del_rec(sink, cp, key, Some(child_level), depth + 1)? {
                None => return Ok(None),
                Some(o) => o,
            };
            if !out.dirty {
                return Ok(Some(Outcome { node: Node::Internal(n), dirty: false }));
            }
            let ps = sink.page_size();
            if is_underflow(&out.node, ps) {
                rebalance(sink, &mut n, i, cp, out.node)?;
            } else {
                let nid = write_node(sink, cp, &out.node)?;
                if nid == cp {
                    return Ok(Some(Outcome { node: Node::Internal(n), dirty: false }));
                }
                n.set_child(i, nid);
            }
            Ok(Some(Outcome { node: Node::Internal(n), dirty: true }))
        }
    }
}

/// Pick the split index. Entries have encoded sizes `sizes`; `skip` is 0 for leaves (the entry at
/// the boundary goes right) and 1 for internal nodes (the entry at the boundary moves up).
/// Left side = `sizes[..s]`, right side = `sizes[s+skip..]`; both plus `header` must fit `cap` and
/// be non-empty. `key_ok(s)` filters candidates by the separator that would be installed in the
/// parent. Minimizes the byte imbalance; ties go to the smaller index.
fn pick_split(sizes: &[usize], header: usize, cap: usize, skip: usize, key_ok: &dyn Fn(usize) -> bool) -> Option<usize> {
    let n = sizes.len();
    let total: usize = sizes.iter().sum();
    let mut best: Option<(usize, usize)> = None; // (diff, s)
    let mut pre = 0usize;
    for s in 1..n {
        pre += sizes[s - 1];
        if s + skip >= n {
            break;
        }
        let left = header + pre;
        let right = header + total - pre - if skip == 1 { sizes[s] } else { 0 };
        if left > cap || right > cap || !key_ok(s) {
            continue;
        }
        let diff = left.abs_diff(right);
        if best.is_none_or(|(d, _)| diff < d) {
            best = Some((diff, s));
        }
    }
    best.map(|(_, s)| s)
}

/// Rebalance the underflowing (or empty) child `i` (page `cp`, decoded and modified node `cn`,
/// not yet written) of `parent` against a sibling. Updates `parent` in place; the caller writes it.
fn rebalance(sink: &mut dyn PageSink, parent: &mut InternalNode, i: usize, cp: PageId, cn: Node) -> Result<()> {
    let ps = sink.page_size();
    let cap = node_capacity(ps);
    let si = if i > 0 { i - 1 } else { i + 1 };
    if si >= parent.num_children() {
        return Err(error::corrupt("b-tree internal node has a single child while rebalancing"));
    }
    let sp = parent.child_at(si);
    let sib = {
        let pg = sink.read_page(sp)?;
        decode_node(&pg)?
    };
    let child_is_left = i == 0;
    let li = i.min(si); // index of the left child of the pair; separator is parent.entries[li]
    let (lp, rp, ln, rn) = if child_is_left { (cp, sp, cn, sib) } else { (sp, cp, sib, cn) };
    let sep_key = parent.entries[li].key.clone();
    // Largest separator the parent can absorb in place of the current one.
    let allowed = sep_key.len() + cap.saturating_sub(parent.encoded_len());
    match (ln, rn) {
        (Node::Leaf(l), Node::Leaf(r)) => {
            let merged_len = l.encoded_len() + r.encoded_len() - LEAF_HEADER_LEN;
            let l_len = l.entries.len();
            let mut all: Vec<LeafEntry> = l.entries;
            all.extend(r.entries);
            if merged_len <= cap {
                let nid = write_node(sink, lp, &Node::Leaf(LeafNode { entries: all }))?;
                sink.release_page(rp)?;
                parent.set_child(li, nid);
                parent.entries.remove(li);
                return Ok(());
            }
            let sizes: Vec<usize> = all.iter().map(LeafEntry::encoded_len).collect();
            let split = pick_split(&sizes, LEAF_HEADER_LEN, cap, 0, &|s| all[s].key.len() <= allowed);
            match split {
                Some(s) => {
                    let right = all.split_off(s);
                    let new_sep = right[0].key.clone();
                    let lid = write_node(sink, lp, &Node::Leaf(LeafNode { entries: all }))?;
                    let rid = write_node(sink, rp, &Node::Leaf(LeafNode { entries: right }))?;
                    parent.set_child(li, lid);
                    parent.set_child(li + 1, rid);
                    parent.entries[li].key = new_sep;
                }
                None => {
                    // The parent cannot take a longer separator: write the (non-empty) child alone.
                    let (node, page, idx) = if child_is_left {
                        all.truncate(l_len);
                        (LeafNode { entries: all }, lp, li)
                    } else {
                        (LeafNode { entries: all.split_off(l_len) }, rp, li + 1)
                    };
                    let nid = write_node(sink, page, &Node::Leaf(node))?;
                    parent.set_child(idx, nid);
                }
            }
            Ok(())
        }
        (Node::Internal(l), Node::Internal(r)) if l.level == r.level => {
            let level = l.level;
            let sep_entry = InternalEntry { key: sep_key, child: r.first_child };
            let merged_len = l.encoded_len() + (r.encoded_len() - INTERNAL_HEADER_LEN) + sep_entry.encoded_len();
            let l_len = l.entries.len();
            let r_len = r.entries.len();
            let l_first = l.first_child;
            let mut all: Vec<InternalEntry> = l.entries;
            all.push(sep_entry);
            all.extend(r.entries);
            if merged_len <= cap {
                let node = InternalNode { level, first_child: l_first, entries: all };
                let nid = write_node(sink, lp, &Node::Internal(node))?;
                sink.release_page(rp)?;
                parent.set_child(li, nid);
                parent.entries.remove(li);
                return Ok(());
            }
            let sizes: Vec<usize> = all.iter().map(InternalEntry::encoded_len).collect();
            let split = pick_split(&sizes, INTERNAL_HEADER_LEN, cap, 1, &|m| all[m].key.len() <= allowed);
            match split {
                Some(m) => {
                    let mut right_entries = all.split_off(m);
                    let up = right_entries.remove(0);
                    let left = InternalNode { level, first_child: l_first, entries: all };
                    let right = InternalNode { level, first_child: up.child, entries: right_entries };
                    let lid = write_node(sink, lp, &Node::Internal(left))?;
                    let rid = write_node(sink, rp, &Node::Internal(right))?;
                    parent.set_child(li, lid);
                    parent.set_child(li + 1, rid);
                    parent.entries[li].key = up.key;
                }
                None => {
                    let child_entries = if child_is_left { l_len } else { r_len };
                    if child_entries == 0 {
                        return Err(PgError::internal(
                            "b-tree delete cannot rebalance an empty internal node: separator would overflow its parent",
                        ));
                    }
                    let (node, page, idx) = if child_is_left {
                        all.truncate(l_len);
                        (InternalNode { level, first_child: l_first, entries: all }, lp, li)
                    } else {
                        // all = L.entries ++ [sep(R.first_child)] ++ R.entries
                        let mut tail = all.split_off(l_len);
                        let sep = tail.remove(0);
                        (InternalNode { level, first_child: sep.child, entries: tail }, rp, li + 1)
                    };
                    let nid = write_node(sink, page, &Node::Internal(node))?;
                    parent.set_child(idx, nid);
                }
            }
            Ok(())
        }
        _ => Err(error::invalid_page(sp, "sibling b-tree node has a different kind or level")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::btree::insert::put;
    use crate::btree::node::{InternalView, LeafView};
    use crate::btree::read::get;
    use crate::format::consts::PageKind;
    use crate::format::page::{Page, PageRef};
    use pgarbor_common::CommitSeq;
    use std::collections::{BTreeMap, HashMap, HashSet};
    use std::sync::Arc;

    const PS: u32 = 4096;

    struct MemSink {
        pages: HashMap<u64, (PageKind, Vec<u8>)>,
        private: HashSet<u64>,
        free: Vec<u64>,
        next: u64,
        writes: usize,
    }

    impl MemSink {
        fn new() -> MemSink {
            MemSink { pages: HashMap::new(), private: HashSet::new(), free: Vec::new(), next: 1, writes: 0 }
        }
        /// Simulate a commit: every page becomes shared.
        fn commit(&mut self) {
            self.private.clear();
            self.free.clear();
        }
    }

    impl PageSource for MemSink {
        fn page_size(&self) -> u32 {
            PS
        }
        fn read_page(&self, id: PageId) -> Result<PageRef> {
            match self.pages.get(&id.0) {
                Some((kind, payload)) => {
                    Ok(Arc::new(Page { id, kind: *kind, commit_seq: CommitSeq(1), payload: payload.clone() }))
                }
                None => Err(error::invalid_page(id, "no such page")),
            }
        }
    }

    impl PageSink for MemSink {
        fn alloc_page(&mut self) -> Result<PageId> {
            let id = match self.free.pop() {
                Some(id) => id,
                None => {
                    let id = self.next;
                    self.next += 1;
                    id
                }
            };
            self.private.insert(id);
            Ok(PageId(id))
        }
        fn write_page(&mut self, id: PageId, kind: PageKind, payload: Vec<u8>) -> Result<()> {
            if !self.private.contains(&id.0) {
                return Err(PgError::internal("write to shared page"));
            }
            self.writes += 1;
            self.pages.insert(id.0, (kind, payload));
            Ok(())
        }
        fn is_private(&self, id: PageId) -> bool {
            self.private.contains(&id.0)
        }
        fn release_page(&mut self, id: PageId) -> Result<()> {
            if self.private.remove(&id.0) {
                self.free.push(id.0);
            }
            Ok(())
        }
    }

    struct Rng(u64);
    impl Rng {
        fn next(&mut self) -> u64 {
            self.0 ^= self.0 << 13;
            self.0 ^= self.0 >> 7;
            self.0 ^= self.0 << 17;
            self.0
        }
    }

    fn make_key(n: u64, pad: usize) -> Vec<u8> {
        let mut k = format!("key-{n:08}").into_bytes();
        k.extend(std::iter::repeat(b'x').take(pad));
        k
    }

    /// Returns (depth, keys in order) after checking all invariants below `page`.
    fn check(
        sink: &MemSink,
        page: PageId,
        lo: Option<&[u8]>,
        hi: Option<&[u8]>,
        is_root: bool,
        out: &mut Vec<Vec<u8>>,
    ) -> u16 {
        let pg = sink.read_page(page).unwrap();
        match pg.kind {
            PageKind::Leaf => {
                let v = LeafView::parse(page, &pg.payload).unwrap();
                assert!(is_root || !v.is_empty(), "empty non-root leaf");
                for i in 0..v.len() {
                    let k = v.key(i);
                    if let Some(lo) = lo {
                        assert!(k >= lo, "key below lower bound");
                    }
                    if let Some(hi) = hi {
                        assert!(k < hi, "key above upper bound");
                    }
                    out.push(k.to_vec());
                }
                0
            }
            PageKind::Internal => {
                let v = InternalView::parse(page, &pg.payload).unwrap();
                assert!(v.len() >= 1, "internal node without separators");
                let mut level = None;
                for c in 0..v.num_children() {
                    let clo = if c == 0 { lo } else { Some(v.key(c - 1)) };
                    let chi = if c == v.len() { hi } else { Some(v.key(c)) };
                    let d = check(sink, v.child(c), clo, chi, false, out);
                    match level {
                        None => level = Some(d),
                        Some(l) => assert_eq!(l, d, "non-uniform depth"),
                    }
                }
                assert_eq!(level.unwrap() + 1, v.level());
                v.level()
            }
            k => panic!("unexpected page kind {k:?}"),
        }
    }

    fn verify(sink: &MemSink, root: PageId, model: &BTreeMap<Vec<u8>, Vec<u8>>) {
        if root.is_none() {
            assert!(model.is_empty());
            return;
        }
        let mut keys = Vec::new();
        check(sink, root, None, None, true, &mut keys);
        let expect: Vec<Vec<u8>> = model.keys().cloned().collect();
        assert_eq!(keys, expect);
        for (k, v) in model.iter().step_by(7) {
            assert_eq!(get(sink, root, k).unwrap().as_deref(), Some(v.as_slice()));
        }
    }

    #[test]
    fn small_delete_and_absent() {
        let mut s = MemSink::new();
        let mut root = PageId::NONE;
        for k in [b"a", b"b", b"c"] {
            root = put(&mut s, root, k, b"v").unwrap().root;
        }
        s.commit();
        let w = s.writes;
        let r = delete(&mut s, root, b"zz").unwrap();
        assert!(!r.existed);
        assert_eq!(r.root, root);
        assert_eq!(s.writes, w, "absent key must not write");
        let r = delete(&mut s, root, b"b").unwrap();
        assert!(r.existed);
        assert_eq!(get(&s, r.root, b"b").unwrap(), None);
        assert_eq!(get(&s, r.root, b"a").unwrap().as_deref(), Some(&b"v"[..]));
        // The old root is untouched (copy-on-write).
        assert_eq!(get(&s, root, b"b").unwrap().as_deref(), Some(&b"v"[..]));
    }

    #[test]
    fn delete_from_empty_tree() {
        let mut s = MemSink::new();
        let r = delete(&mut s, PageId::NONE, b"a").unwrap();
        assert!(!r.existed);
        assert!(r.root.is_none());
    }

    #[test]
    fn random_interleavings_match_model() {
        for (seed, pad, shared) in [(1u64, 150usize, false), (7, 150, true), (99, 400, false), (12345, 30, true)] {
            // Deterministic seeds are listed in the table above; the multiply must wrap
            // (a plain `*` overflows u64 in debug builds for any seed >= 2).
            let mut rng = Rng(seed.wrapping_mul(0x9E3779B97F4A7C15) | 1);
            let mut s = MemSink::new();
            let mut root = PageId::NONE;
            let mut model: BTreeMap<Vec<u8>, Vec<u8>> = BTreeMap::new();
            for step in 0..4000u32 {
                let n = rng.next() % 600;
                let k = make_key(n, pad + (n as usize % 3) * 20);
                // Phases: insert-heavy, then delete-heavy.
                let insert = if step < 1800 { rng.next() % 10 < 7 } else { rng.next() % 10 < 3 };
                if insert {
                    let vlen = if rng.next() % 40 == 0 { 3000 } else { (rng.next() % 50) as usize };
                    let v = vec![(n % 251) as u8; vlen];
                    root = put(&mut s, root, &k, &v).unwrap().root;
                    model.insert(k, v);
                } else {
                    let r = delete(&mut s, root, &k).unwrap();
                    assert_eq!(r.existed, model.remove(&k).is_some());
                    root = r.root;
                }
                if shared && step % 50 == 0 {
                    s.commit();
                }
                if step % 97 == 0 {
                    verify(&s, root, &model);
                }
            }
            verify(&s, root, &model);
            // Delete everything.
            let keys: Vec<Vec<u8>> = model.keys().cloned().collect();
            for (j, k) in keys.iter().enumerate() {
                let r = delete(&mut s, root, k).unwrap();
                assert!(r.existed);
                root = r.root;
                model.remove(k);
                if j % 53 == 0 {
                    verify(&s, root, &model);
                }
            }
            assert!(root.is_none());
        }
    }

    #[test]
    fn delete_all_yields_none() {
        let mut s = MemSink::new();
        let mut root = PageId::NONE;
        for n in 0..500 {
            root = put(&mut s, root, &make_key(n, 100), b"value").unwrap().root;
        }
        for n in (0..500).rev() {
            let r = delete(&mut s, root, &make_key(n, 100)).unwrap();
            assert!(r.existed);
            root = r.root;
        }
        assert!(root.is_none());
    }

    #[test]
    fn range_delete_middle_prefix() {
        let mut s = MemSink::new();
        let mut root = PageId::NONE;
        let mut model = BTreeMap::new();
        for n in 0..3000u64 {
            let k = make_key(n, 10);
            root = put(&mut s, root, &k, b"v").unwrap().root;
            model.insert(k, b"v".to_vec());
        }
        let lo = make_key(500, 0);
        let hi = make_key(2500, 0);
        let r = delete_range(&mut s, root, Bound::Included(lo.as_slice()), Bound::Excluded(hi.as_slice())).unwrap();
        // Keys "key-00000500xxxxxxxxxx" >= lo; keys with n in 500..2500 are removed.
        assert_eq!(r.deleted, 2000);
        model.retain(|k, _| !(k.as_slice() >= lo.as_slice() && k.as_slice() < hi.as_slice()));
        verify(&s, r.root, &model);
        let all = delete_range(&mut s, r.root, Bound::Unbounded, Bound::Unbounded).unwrap();
        assert_eq!(all.deleted, 1000);
        assert!(all.root.is_none());
    }
}
