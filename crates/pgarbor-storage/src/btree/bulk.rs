//! Batch application and bottom-up bulk build. Lease STO-10.
#[allow(unused_imports)]
use crate::btree::node::{
    encode_node, max_inline_value, max_key_len, node_capacity, InternalEntry, InternalNode, LeafEntry,
    LeafNode, LeafValue, Node, INTERNAL_CELL_OVERHEAD, INTERNAL_HEADER_LEN, LEAF_HEADER_LEN, SLOT_LEN,
};
#[allow(unused_imports)]
use crate::btree::{delete::delete, insert::put, overflow::write_overflow};
use crate::batch::WriteBatch;
use crate::error;
use crate::pager::{PageSink, PageSource};
use pgarbor_common::{PageId, PgError, Result};

// CONTRACT
/// Leaf/internal fill target for bulk builds: nodes are closed when adding the next entry would
/// exceed `BULK_FILL_PERCENT`% of `node_capacity` (PG btree leaf fillfactor default is 90).
pub const BULK_FILL_PERCENT: usize = 90;

// CONTRACT
/// Apply every operation of `batch` (ascending key order) to the tree at `root`; returns the new
/// root. `Some(v)` → `put`, `None` → `delete` (absent keys are fine). Fast path: when
/// `root == NONE` and the batch has no deletes, use `build_from_sorted` (identical logical
/// result). Errors propagate from put/delete (e.g. 54000 key too large); on error the sink may
/// hold partially written private pages — callers abandon the whole write txn.
///
/// This is how pgarbor-core commits a transaction overlay (`BTreeMap<Vec<u8>, Option<Vec<u8>>>`)
/// onto a branch head.
pub fn apply_batch(sink: &mut dyn PageSink, root: PageId, batch: &WriteBatch) -> Result<PageId> {
    if root.is_none() && !batch.has_deletes() {
        let mut items = batch.iter().map(|(k, v)| -> Result<(Vec<u8>, Vec<u8>)> {
            match v {
                Some(v) => Ok((k.to_vec(), v.to_vec())),
                // Unreachable: `has_deletes()` was false. Refuse rather than drop the delete.
                None => Err(PgError::internal("bulk build fast path received a delete")),
            }
        });
        return build_from_sorted(sink, &mut items);
    }
    let mut root = root;
    for (key, value) in batch.iter() {
        root = match value {
            Some(v) => put(sink, root, key, v)?.root,
            None => delete(sink, root, key)?.root,
        };
    }
    Ok(root)
}

// CONTRACT
/// Build a new tree from strictly ascending `(key, value)` items; returns its root (`NONE` for no
/// items). Streaming, O(height) memory: keep one open node per level; when a node is closed
/// (fill rule above; always >= 1 entry, and a single entry that alone exceeds the fill target is
/// still placed), write it with `sink.write_new` and push `(first_key, id)` into the parent
/// level (first child of a parent becomes `first_child`, later ones become separators with that
/// child's first key). At the end close all levels bottom-up; a level with exactly one node is
/// the root. Before closing the last node of a level, if it is below `underflow_threshold` and a
/// previous sibling exists, rebalance the last two nodes of that level by bytes (requires keeping
/// the previous closed node unwritten until its successor closes — "write one behind").
/// Values longer than `max_inline_value` go through `write_overflow`.
///
/// Errors: keys not strictly ascending → `PgError::internal("bulk build input keys are not
/// strictly ascending")`; key too long → `error::key_too_large`; item `Err` propagates.
pub fn build_from_sorted(
    sink: &mut dyn PageSink,
    items: &mut dyn Iterator<Item = Result<(Vec<u8>, Vec<u8>)>>,
) -> Result<PageId> {
    let page_size = sink.page_size();
    let max_key = max_key_len(page_size);
    let max_inline = max_inline_value(page_size);
    let mut builder = Builder::new(page_size);
    let mut last_key: Option<Vec<u8>> = None;
    for item in items {
        let (key, value) = item?;
        if let Some(prev) = &last_key {
            if key.as_slice() <= prev.as_slice() {
                return Err(PgError::internal("bulk build input keys are not strictly ascending"));
            }
        }
        if key.len() > max_key {
            return Err(error::key_too_large(key.len(), max_key));
        }
        if value.len() as u64 > u32::MAX as u64 {
            return Err(error::value_too_large(value.len(), u32::MAX as u64));
        }
        let stored = if value.len() > max_inline {
            let first = write_overflow(sink, &value)?;
            LeafValue::Overflow { first, len: value.len() as u32 }
        } else {
            LeafValue::Inline(value)
        };
        last_key = Some(key.clone());
        builder.push(sink, 0, Item::Leaf(LeafEntry { key, value: stored }))?;
    }
    builder.finish(sink)
}

// ---------------------------------------------------------------------------------------------
// Private bulk-build machinery.
// ---------------------------------------------------------------------------------------------

/// Node kind of one builder level: level 0 holds leaf entries, higher levels hold child pointers.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Kind {
    Leaf,
    Internal,
}

fn kind_of(lvl: usize) -> Kind {
    if lvl == 0 { Kind::Leaf } else { Kind::Internal }
}

fn header_len(kind: Kind) -> usize {
    match kind {
        Kind::Leaf => LEAF_HEADER_LEN,
        Kind::Internal => INTERNAL_HEADER_LEN,
    }
}

/// One item of an open or closed node. `Child.key` is the first key of the child subtree, which
/// becomes the separator in the parent (and is also the first key of the child's node).
enum Item {
    Leaf(LeafEntry),
    Child { key: Vec<u8>, id: PageId },
}

impl Item {
    fn key(&self) -> &[u8] {
        match self {
            Item::Leaf(e) => e.key.as_slice(),
            Item::Child { key, .. } => key.as_slice(),
        }
    }

    /// Cell plus slot bytes as they appear in the encoded node.
    fn encoded_len(&self) -> usize {
        match self {
            Item::Leaf(e) => e.encoded_len(),
            Item::Child { key, .. } => SLOT_LEN + INTERNAL_CELL_OVERHEAD + key.len(),
        }
    }
}

/// Bytes an item adds to its node. The first child of an internal node is the leftmost pointer,
/// which is already covered by the node header and costs nothing.
fn cost(kind: Kind, first: bool, item: &Item) -> usize {
    match kind {
        Kind::Internal if first => 0,
        _ => item.encoded_len(),
    }
}

/// Encode `items` as one node of level `lvl` and write it as a new page.
fn write_items(sink: &mut dyn PageSink, lvl: usize, items: Vec<Item>, page_size: u32) -> Result<PageId> {
    let node = match kind_of(lvl) {
        Kind::Leaf => {
            let mut entries = Vec::with_capacity(items.len());
            for item in items {
                match item {
                    Item::Leaf(e) => entries.push(e),
                    Item::Child { .. } => {
                        return Err(PgError::internal("bulk build leaf level holds child pointers"));
                    }
                }
            }
            Node::Leaf(LeafNode { entries })
        }
        Kind::Internal => {
            let level = u16::try_from(lvl)
                .map_err(|_| PgError::internal("bulk build exceeds maximum b-tree height"))?;
            let mut first_child = PageId::NONE;
            let mut entries = Vec::with_capacity(items.len().saturating_sub(1));
            for (i, item) in items.into_iter().enumerate() {
                match item {
                    Item::Child { key, id } => {
                        if i == 0 {
                            first_child = id;
                        } else {
                            entries.push(InternalEntry { key, child: id });
                        }
                    }
                    Item::Leaf(_) => {
                        return Err(PgError::internal("bulk build internal level holds leaf entries"));
                    }
                }
            }
            if first_child.is_none() {
                return Err(PgError::internal("bulk build closed an empty internal node"));
            }
            Node::Internal(InternalNode { level, first_child, entries })
        }
    };
    let (kind, payload) = encode_node(&node, page_size)?;
    sink.write_new(kind, payload)
}

/// Byte sizes of the two nodes produced by splitting `all` before index `s`. Internal nodes do not
/// pay for their first child (it lives in the header), so the right node's first item is free too.
fn split_sizes(kind: Kind, head: usize, prefix: &[usize], s: usize) -> (usize, usize) {
    let n = prefix.len() - 1;
    match kind {
        Kind::Leaf => (head + prefix[s], head + prefix[n] - prefix[s]),
        Kind::Internal => (head + prefix[s] - prefix[1], head + prefix[n] - prefix[s + 1]),
    }
}

/// Rebalance two adjacent nodes given as their concatenated items. Returns one node when everything
/// fits in a single node (the merge keeps the last node at least half full), otherwise two nodes
/// split at the byte-balanced boundary. Internal nodes keep at least two children each.
fn rebalance(kind: Kind, mut all: Vec<Item>, page_size: u32) -> Result<(Vec<Item>, Option<Vec<Item>>)> {
    let cap = node_capacity(page_size);
    let head = header_len(kind);
    let n = all.len();
    if n < 2 {
        return Ok((all, None));
    }
    let mut prefix = Vec::with_capacity(n + 1);
    prefix.push(0usize);
    for item in &all {
        let next = prefix[prefix.len() - 1] + item.encoded_len();
        prefix.push(next);
    }
    let merged = match kind {
        Kind::Leaf => head + prefix[n],
        Kind::Internal => head + prefix[n] - prefix[1],
    };
    if merged <= cap {
        return Ok((all, None));
    }
    let (lo, hi) = match kind {
        Kind::Leaf => (1, n - 1),
        Kind::Internal => (2, n.saturating_sub(2)),
    };
    let mut best: Option<(usize, usize)> = None;
    for s in lo..=hi {
        let (left, right) = split_sizes(kind, head, &prefix, s);
        let diff = left.abs_diff(right);
        if best.is_none_or(|(d, _)| diff < d) {
            best = Some((diff, s));
        }
    }
    let Some((_, s)) = best else {
        return Err(PgError::internal("bulk build could not balance adjacent nodes"));
    };
    let (left, right) = split_sizes(kind, head, &prefix, s);
    if left > cap || right > cap {
        return Err(PgError::internal("bulk build could not balance adjacent nodes"));
    }
    let right_items = all.split_off(s);
    Ok((all, Some(right_items)))
}

/// Open node and closed-but-unwritten node of one level.
struct Level {
    /// The node currently being filled.
    open: Vec<Item>,
    /// Encoded length of `open` (header included).
    open_bytes: usize,
    /// The most recently closed node. It is written only when its successor closes or at finish,
    /// so the last two nodes of a level can still be rebalanced.
    held: Option<Vec<Item>>,
}

struct Builder {
    page_size: u32,
    /// Closing threshold for an open node, in encoded bytes.
    fill_limit: usize,
    /// A final node below this many bytes is rebalanced with its predecessor. This is half of
    /// `node_capacity`, which is the STO-10 acceptance floor (`underflow_threshold` is only a
    /// quarter of capacity, which would leave leaves between 25% and 50% full).
    min_fill: usize,
    levels: Vec<Level>,
}

impl Builder {
    fn new(page_size: u32) -> Builder {
        let cap = node_capacity(page_size);
        Builder {
            page_size,
            fill_limit: cap * BULK_FILL_PERCENT / 100,
            min_fill: cap / 2,
            levels: Vec::new(),
        }
    }

    /// Add `item` to level `lvl`, closing the open node first when the item would overflow the
    /// fill target. A closed node is written one behind, when its successor closes.
    fn push(&mut self, sink: &mut dyn PageSink, lvl: usize, item: Item) -> Result<()> {
        if lvl == self.levels.len() {
            self.levels.push(Level { open: Vec::new(), open_bytes: 0, held: None });
        }
        let kind = kind_of(lvl);
        let (has_open, open_bytes) = (!self.levels[lvl].open.is_empty(), self.levels[lvl].open_bytes);
        if has_open && open_bytes + cost(kind, false, &item) > self.fill_limit {
            let closed = std::mem::take(&mut self.levels[lvl].open);
            let older = self.levels[lvl].held.replace(closed);
            if let Some(items) = older {
                self.emit(sink, lvl, items)?;
            }
        }
        let lv = &mut self.levels[lvl];
        if lv.open.is_empty() {
            lv.open_bytes = header_len(kind);
        }
        lv.open_bytes += cost(kind, lv.open.is_empty(), &item);
        lv.open.push(item);
        Ok(())
    }

    /// Write a closed node of level `lvl` and register it with the parent level.
    fn emit(&mut self, sink: &mut dyn PageSink, lvl: usize, items: Vec<Item>) -> Result<()> {
        let first_key = match items.first() {
            Some(item) => item.key().to_vec(),
            None => return Err(PgError::internal("bulk build closed an empty node")),
        };
        let id = write_items(sink, lvl, items, self.page_size)?;
        self.push(sink, lvl + 1, Item::Child { key: first_key, id })
    }

    /// Close every level bottom-up and return the root. Levels that end up with a single node
    /// make that node the root.
    fn finish(&mut self, sink: &mut dyn PageSink) -> Result<PageId> {
        if self.levels.is_empty() {
            return Ok(PageId::NONE);
        }
        let mut lvl = 0usize;
        loop {
            let kind = kind_of(lvl);
            let is_top = self.levels.len() == lvl + 1;
            let held = self.levels[lvl].held.take();
            let open = std::mem::take(&mut self.levels[lvl].open);
            let open_bytes = self.levels[lvl].open_bytes;
            let Some(held) = held else {
                // Nothing was closed at this level, so it holds the only node and it is the root.
                if !is_top {
                    return Err(PgError::internal("bulk build level bookkeeping is inconsistent"));
                }
                return write_items(sink, lvl, open, self.page_size);
            };
            let (first, second) = if open_bytes < self.min_fill {
                let mut all = held;
                all.extend(open);
                rebalance(kind, all, self.page_size)?
            } else {
                (held, Some(open))
            };
            if second.is_none() && is_top {
                return write_items(sink, lvl, first, self.page_size);
            }
            self.emit(sink, lvl, first)?;
            if let Some(second) = second {
                self.emit(sink, lvl, second)?;
            }
            lvl += 1;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::btree::node::decode_node;
    use crate::btree::read::{get, RangeIter};
    use crate::format::consts::{payload_capacity, PageKind};
    use crate::format::page::{Page, PageRef};
    use pgarbor_common::{CommitSeq, SqlState};
    use std::collections::{BTreeMap, HashMap};
    use std::ops::Bound;
    use std::sync::Arc;

    /// Minimal in-memory sink: every allocated page is private.
    struct MemSink {
        page_size: u32,
        next: u64,
        pages: HashMap<u64, Arc<Page>>,
    }

    impl MemSink {
        fn new(page_size: u32) -> MemSink {
            MemSink { page_size, next: 1, pages: HashMap::new() }
        }
    }

    impl PageSource for MemSink {
        fn page_size(&self) -> u32 {
            self.page_size
        }
        fn read_page(&self, id: PageId) -> Result<PageRef> {
            match self.pages.get(&id.0) {
                Some(p) => Ok(Arc::clone(p)),
                None => Err(error::page_out_of_range(id, self.next - 1)),
            }
        }
    }

    impl PageSink for MemSink {
        fn alloc_page(&mut self) -> Result<PageId> {
            let id = self.next;
            self.next += 1;
            Ok(PageId(id))
        }
        fn write_page(&mut self, id: PageId, kind: PageKind, payload: Vec<u8>) -> Result<()> {
            if id.is_none() || id.0 >= self.next || payload.len() > payload_capacity(self.page_size) {
                return Err(PgError::internal("test sink rejected a write"));
            }
            self.pages.insert(id.0, Arc::new(Page { id, kind, commit_seq: CommitSeq(0), payload }));
            Ok(())
        }
        fn is_private(&self, id: PageId) -> bool {
            self.pages.contains_key(&id.0)
        }
        fn release_page(&mut self, id: PageId) -> Result<()> {
            self.pages.remove(&id.0);
            Ok(())
        }
    }

    fn items(n: usize, vlen: usize) -> Vec<(Vec<u8>, Vec<u8>)> {
        (0..n).map(|i| (format!("key{i:08}").into_bytes(), vec![(i % 251) as u8; vlen])).collect()
    }

    fn build(sink: &mut MemSink, data: &[(Vec<u8>, Vec<u8>)]) -> Result<PageId> {
        let mut it = data
            .iter()
            .map(|(k, v)| -> Result<(Vec<u8>, Vec<u8>)> { Ok((k.clone(), v.clone())) });
        build_from_sorted(sink, &mut it)
    }

    fn scan(sink: &MemSink, root: PageId) -> Result<Vec<(Vec<u8>, Vec<u8>)>> {
        RangeIter::new(sink, root, Bound::Unbounded, Bound::Unbounded)?.collect()
    }

    fn root_level(sink: &MemSink, root: PageId) -> Result<u16> {
        let page = sink.read_page(root)?;
        match decode_node(&page)? {
            Node::Leaf(_) => Ok(0),
            Node::Internal(n) => Ok(n.level),
        }
    }

    /// Structural invariant walk: node levels, routing bounds, sorted keys, no empty or
    /// single-child internal nodes. Returns the encoded length of every leaf, in key order.
    fn check_node(
        sink: &MemSink,
        id: PageId,
        level: u16,
        lo: Option<&[u8]>,
        hi: Option<&[u8]>,
        leaf_lens: &mut Vec<usize>,
    ) -> Result<usize> {
        let page = sink.read_page(id)?;
        match decode_node(&page)? {
            Node::Leaf(leaf) => {
                if level != 0 {
                    return Err(PgError::internal(format!("leaf found at level {level}")));
                }
                let mut prev: Option<&[u8]> = None;
                for e in &leaf.entries {
                    let k = e.key.as_slice();
                    if prev.is_some_and(|p| k <= p)
                        || lo.is_some_and(|l| k < l)
                        || hi.is_some_and(|h| k >= h)
                    {
                        return Err(PgError::internal("leaf key violates ordering or routing bounds"));
                    }
                    prev = Some(k);
                }
                leaf_lens.push(leaf.encoded_len());
                Ok(leaf.entries.len())
            }
            Node::Internal(node) => {
                if level == 0 || node.level != level {
                    return Err(PgError::internal("internal node level mismatch"));
                }
                if node.entries.is_empty() {
                    return Err(PgError::internal("internal node without separators"));
                }
                let mut total = 0;
                for i in 0..node.num_children() {
                    let child_lo = if i == 0 { lo } else { Some(node.entries[i - 1].key.as_slice()) };
                    let child_hi = if i == node.entries.len() { hi } else { Some(node.entries[i].key.as_slice()) };
                    total += check_node(sink, node.child_at(i), level - 1, child_lo, child_hi, leaf_lens)?;
                }
                Ok(total)
            }
        }
    }

    fn check_tree(sink: &MemSink, root: PageId) -> Result<Vec<usize>> {
        let mut leaf_lens = Vec::new();
        if root.is_none() {
            return Ok(leaf_lens);
        }
        let level = root_level(sink, root)?;
        check_node(sink, root, level, None, None, &mut leaf_lens)?;
        Ok(leaf_lens)
    }

    fn assert_built(sink: &MemSink, root: PageId, expected: &[(Vec<u8>, Vec<u8>)]) {
        let got = scan(sink, root).expect("scan");
        assert_eq!(got.len(), expected.len());
        assert!(got == expected, "scan differs from input");
        let leaf_lens = check_tree(sink, root).expect("invariants");
        let cap = node_capacity(sink.page_size);
        if leaf_lens.len() > 1 {
            for len in &leaf_lens {
                assert!(len * 2 >= cap, "leaf {len} bytes is under half of capacity {cap}");
            }
        }
    }

    #[test]
    fn build_zero_items_gives_none() {
        let mut sink = MemSink::new(8192);
        let root = build(&mut sink, &[]).unwrap();
        assert!(root.is_none());
        assert!(scan(&sink, root).unwrap().is_empty());
    }

    #[test]
    fn build_one_item() {
        let mut sink = MemSink::new(8192);
        let data = items(1, 4);
        let root = build(&mut sink, &data).unwrap();
        assert_built(&sink, root, &data);
        assert_eq!(get(&sink, root, b"key00000000").unwrap(), Some(vec![0u8; 4]));
    }

    #[test]
    fn build_thousand_items() {
        for ps in [4096u32, 8192] {
            let mut sink = MemSink::new(ps);
            let data = items(1000, 16);
            let root = build(&mut sink, &data).unwrap();
            assert_built(&sink, root, &data);
            assert_eq!(get(&sink, root, b"key00000999").unwrap(), Some(vec![(999 % 251) as u8; 16]));
            assert!(get(&sink, root, b"key00001000").unwrap().is_none());
        }
    }

    #[test]
    fn build_hundred_thousand_items() {
        let mut sink = MemSink::new(8192);
        let data = items(100_000, 16);
        let root = build(&mut sink, &data).unwrap();
        assert!(root_level(&sink, root).unwrap() >= 1);
        assert_built(&sink, root, &data);
    }

    #[test]
    fn build_with_overflow_values() {
        let mut sink = MemSink::new(4096);
        let data: Vec<(Vec<u8>, Vec<u8>)> = (0..50usize)
            .map(|i| (format!("big{i:04}").into_bytes(), vec![(i % 7) as u8; 3000 + i * 13]))
            .collect();
        let root = build(&mut sink, &data).unwrap();
        assert_built(&sink, root, &data);
    }

    #[test]
    fn build_rejects_unsorted_keys() {
        let mut sink = MemSink::new(8192);
        let data = vec![(b"b".to_vec(), b"1".to_vec()), (b"a".to_vec(), b"2".to_vec())];
        let err = build(&mut sink, &data).unwrap_err();
        assert_eq!(err.message, "bulk build input keys are not strictly ascending");
        let dup = vec![(b"a".to_vec(), b"1".to_vec()), (b"a".to_vec(), b"2".to_vec())];
        assert!(build(&mut sink, &dup).is_err());
    }

    #[test]
    fn build_rejects_long_key() {
        let mut sink = MemSink::new(8192);
        let long = vec![b'x'; max_key_len(8192) + 1];
        let err = build(&mut sink, &[(long, b"v".to_vec())]).unwrap_err();
        assert_eq!(err.sqlstate, SqlState::PROGRAM_LIMIT_EXCEEDED);
    }

    #[test]
    fn apply_batch_fast_path_matches_model() {
        let mut sink = MemSink::new(8192);
        let mut batch = WriteBatch::new();
        let mut model = BTreeMap::new();
        for i in (0..3000usize).rev() {
            let k = format!("k{i:06}").into_bytes();
            let v = vec![(i % 200) as u8; 1 + i % 40];
            batch.put(&k, &v);
            model.insert(k, v);
        }
        let root = apply_batch(&mut sink, PageId::NONE, &batch).unwrap();
        let expected: Vec<_> = model.into_iter().collect();
        assert_eq!(scan(&sink, root).unwrap(), expected);
        check_tree(&sink, root).unwrap();
    }

    #[test]
    fn apply_batch_mixed_ops_match_model() {
        let mut sink = MemSink::new(4096);
        let base = items(2000, 8);
        let root = build(&mut sink, &base).unwrap();
        let mut model: BTreeMap<Vec<u8>, Vec<u8>> = base.iter().cloned().collect();
        let mut batch = WriteBatch::new();
        for i in (0..3000usize).step_by(3) {
            let k = format!("key{i:08}").into_bytes();
            batch.put(&k, b"replacement-value-0123456789");
            model.insert(k, b"replacement-value-0123456789".to_vec());
        }
        for i in (0..2000usize).step_by(7) {
            let k = format!("key{i:08}").into_bytes();
            batch.delete(&k);
            model.remove(&k);
        }
        let big = vec![9u8; 5000];
        batch.put(b"key00000005", &big);
        model.insert(b"key00000005".to_vec(), big.clone());
        let new_root = apply_batch(&mut sink, root, &batch).unwrap();
        let expected: Vec<_> = model.into_iter().collect();
        assert_eq!(scan(&sink, new_root).unwrap(), expected);
    }

    #[test]
    fn apply_batch_equals_one_by_one() {
        let mut sink = MemSink::new(4096);
        let base = items(500, 12);
        let root = build(&mut sink, &base).unwrap();
        let mut batch = WriteBatch::new();
        batch.delete(b"key00000010");
        batch.put(b"key00000011", b"changed");
        batch.put(b"zzz", b"tail");
        batch.delete(b"nonexistent");
        let batched = apply_batch(&mut sink, root, &batch).unwrap();
        let mut seq = root;
        for (k, v) in batch.iter() {
            seq = match v {
                Some(v) => put(&mut sink, seq, k, v).unwrap().root,
                None => delete(&mut sink, seq, k).unwrap().root,
            };
        }
        assert_eq!(scan(&sink, batched).unwrap(), scan(&sink, seq).unwrap());
    }

    #[test]
    fn apply_batch_empty_batch_returns_root() {
        let mut sink = MemSink::new(8192);
        let root = build(&mut sink, &items(10, 2)).unwrap();
        assert_eq!(apply_batch(&mut sink, root, &WriteBatch::new()).unwrap(), root);
        assert!(apply_batch(&mut sink, PageId::NONE, &WriteBatch::new()).unwrap().is_none());
    }
}
