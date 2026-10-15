//! Point lookup and ordered range iteration (STORE-06). Lease STO-07.
//!
//! Reads never modify anything and work identically on snapshots, write txns and export sinks.
//! Key order is bytewise (`<[u8] as Ord>`), i.e. memcmp order with shorter-prefix-first.
use crate::btree::node::{
    internal_span_key, internal_spans, leaf_spans, span_key, span_value, InternalSpan, LeafSpan, LeafValueRef,
};
use crate::btree::overflow::read_overflow;
use crate::error;
use crate::format::consts::PageKind;
use crate::format::page::PageRef;
use crate::pager::PageSource;
use pgarbor_common::{PageId, PgError, Result};
use std::ops::Bound;

// CONTRACT
/// Materialize a leaf value (inline copy or overflow read).
pub fn materialize(src: &dyn PageSource, v: LeafValueRef<'_>) -> Result<Vec<u8>> {
    match v {
        LeafValueRef::Inline(b) => Ok(b.to_vec()),
        LeafValueRef::Overflow { first, len } => read_overflow(src, first, len),
    }
}

// CONTRACT
/// Point lookup. `root == NONE` → `Ok(None)`. Descend: at each internal page parse with
/// `internal_spans`, follow child `child_index(key)`; at the leaf binary-search with `leaf_spans`
/// and `materialize` the value. Structural checks during descent (XX001 via
/// `error::invalid_page`): a page of kind Overflow/FreeList where a node is expected; a child's
/// level must be exactly parent level - 1 (leaf = 0); depth > 64 ("b-tree too deep, possible
/// cycle").
///
/// Example: tree {a→1, b→2}: `get(src, root, b"b") == Ok(Some(b"2".to_vec()))`,
/// `get(src, root, b"c") == Ok(None)`.
pub fn get(src: &dyn PageSource, root: PageId, key: &[u8]) -> Result<Option<Vec<u8>>> {
    if root.is_none() {
        return Ok(None);
    }
    let mut id = root;
    let mut expected: Option<u16> = None;
    let mut depth = 0usize;
    loop {
        depth += 1;
        if depth > MAX_DEPTH {
            return Err(too_deep(id));
        }
        match load_node(src, id, expected)? {
            Loaded::Leaf { page, spans } => {
                return match spans.binary_search_by(|s| span_key(&page.payload, s).cmp(key)) {
                    Ok(i) => Ok(Some(materialize(src, span_value(&page.payload, &spans[i]))?)),
                    Err(_) => Ok(None),
                };
            }
            Loaded::Internal { page, level, first_child, spans } => {
                let idx = child_index(&page, &spans, key);
                id = child_of(first_child, &spans, idx);
                expected = Some(level - 1);
            }
        }
    }
}

/// Maximum root-to-leaf depth accepted before the tree is declared cyclic.
const MAX_DEPTH: usize = 64;

fn too_deep(id: PageId) -> PgError {
    error::invalid_page(id, "b-tree too deep, possible cycle")
}

/// A parsed b-tree node page.
enum Loaded {
    Internal { page: PageRef, level: u16, first_child: PageId, spans: Vec<InternalSpan> },
    Leaf { page: PageRef, spans: Vec<LeafSpan> },
}

/// Read and parse page `id`, checking its kind and (when `expected` is given) its level.
fn load_node(src: &dyn PageSource, id: PageId, expected: Option<u16>) -> Result<Loaded> {
    let page = src.read_page(id)?;
    match page.kind {
        PageKind::Leaf => {
            if let Some(x) = expected {
                if x != 0 {
                    return Err(error::invalid_page(id, format!("expected level {x}, found 0")));
                }
            }
            let spans = leaf_spans(id, &page.payload)?;
            Ok(Loaded::Leaf { page, spans })
        }
        PageKind::Internal => {
            let (level, first_child, spans) = internal_spans(id, &page.payload)?;
            if level == 0 {
                return Err(error::invalid_page(id, "internal page has level 0"));
            }
            if let Some(x) = expected {
                if x != level {
                    return Err(error::invalid_page(id, format!("expected level {x}, found {level}")));
                }
            }
            Ok(Loaded::Internal { page, level, first_child, spans })
        }
        k => Err(error::invalid_page(id, format!("expected a b-tree node, found {} page", k.name()))),
    }
}

/// Index of the child covering `key`: number of separators `<= key`.
fn child_index(page: &PageRef, spans: &[InternalSpan], key: &[u8]) -> usize {
    spans.partition_point(|s| internal_span_key(&page.payload, s) <= key)
}

fn child_of(first_child: PageId, spans: &[InternalSpan], i: usize) -> PageId {
    if i == 0 {
        first_child
    } else {
        spans[i - 1].child
    }
}

/// One level of the iterator's root-to-leaf stack.
#[derive(Debug)]
pub(crate) enum Frame {
    /// Internal node: children not yet visited are `next_child..=spans.len()`.
    Internal { page: PageRef, level: u16, first_child: PageId, spans: Vec<InternalSpan>, next_child: usize },
    /// Leaf: entries not yet yielded are `next..spans.len()`.
    Leaf { page: PageRef, spans: Vec<LeafSpan>, next: usize },
}

/// Descend from `start` (expected level `expected`) to a leaf, pushing a frame per level. At each
/// internal node `choose` picks the child; the pushed frame has `next_child = chosen + 1`. The
/// leaf frame is pushed with `next = 0`.
fn descend(
    src: &dyn PageSource,
    stack: &mut Vec<Frame>,
    start: PageId,
    mut expected: Option<u16>,
    choose: &dyn Fn(&PageRef, &[InternalSpan]) -> usize,
) -> Result<()> {
    let mut id = start;
    loop {
        if stack.len() >= MAX_DEPTH {
            return Err(too_deep(id));
        }
        match load_node(src, id, expected)? {
            Loaded::Leaf { page, spans } => {
                stack.push(Frame::Leaf { page, spans, next: 0 });
                return Ok(());
            }
            Loaded::Internal { page, level, first_child, spans } => {
                let idx = choose(&page, &spans);
                id = child_of(first_child, &spans, idx);
                expected = Some(level - 1);
                stack.push(Frame::Internal { page, level, first_child, spans, next_child: idx + 1 });
            }
        }
    }
}

fn beyond_upper(upper: &Bound<Vec<u8>>, key: &[u8]) -> bool {
    match upper {
        Bound::Unbounded => false,
        Bound::Included(u) => key > u.as_slice(),
        Bound::Excluded(u) => key >= u.as_slice(),
    }
}

// CONTRACT
/// Ascending iterator over `[lower, upper]` bounds. Lazy: holds one page per level (Arc'd), so
/// memory is O(height) regardless of range size. Yields owned `(key, value)`; overflow values are
/// read as they are reached. After the first `Err` the iterator is fused (returns `None`).
///
/// Semantics: `Included(k)` lower = first key >= k; `Excluded(k)` = first key > k; upper
/// analogous; `Unbounded` = open. An empty range (lower above upper) yields nothing — never panics
/// (unlike `BTreeMap::range`).
pub struct RangeIter<'a> {
    pub(crate) src: &'a dyn PageSource,
    pub(crate) stack: Vec<Frame>,
    pub(crate) upper: Bound<Vec<u8>>,
    pub(crate) done: bool,
}

impl<'a> RangeIter<'a> {
    /// Position at the first key satisfying `lower`: descend from `root` choosing at each internal
    /// node child `child_index(lower_key)` (Unbounded → child 0), pushing frames with
    /// `next_child = chosen + 1`; in the leaf set `next` to the binary-search position
    /// (`Included`: first >= k, `Excluded`: first > k). If `next` lands past the leaf's end,
    /// `advance` will pop up and continue. `root == NONE` → an iterator that is already done.
    /// Structural checks as in [`get`].
    pub fn new(src: &'a dyn PageSource, root: PageId, lower: Bound<&[u8]>, upper: Bound<&[u8]>) -> Result<RangeIter<'a>> {
        let upper = match upper {
            Bound::Unbounded => Bound::Unbounded,
            Bound::Included(u) => Bound::Included(u.to_vec()),
            Bound::Excluded(u) => Bound::Excluded(u.to_vec()),
        };
        let mut it = RangeIter { src, stack: Vec::new(), upper, done: false };
        if root.is_none() {
            it.done = true;
            return Ok(it);
        }
        let lower_key: Option<&[u8]> = match lower {
            Bound::Unbounded => None,
            Bound::Included(k) | Bound::Excluded(k) => Some(k),
        };
        let choose = |page: &PageRef, spans: &[InternalSpan]| match lower_key {
            None => 0,
            Some(k) => child_index(page, spans, k),
        };
        descend(src, &mut it.stack, root, None, &choose)?;
        if let (Some(k), Some(Frame::Leaf { page, spans, next })) = (lower_key, it.stack.last_mut()) {
            let pos = spans.binary_search_by(|s| span_key(&page.payload, s).cmp(k));
            let excluded = matches!(lower, Bound::Excluded(_));
            *next = match pos {
                Ok(i) if excluded => i + 1,
                Ok(i) | Err(i) => i,
            };
        }
        Ok(it)
    }

    /// Produce the next pair or `Ok(None)` at the end of range/tree. Algorithm: if top frame is a
    /// leaf with remaining entries, take entry `next`, stop (set done, return None) if its key is
    /// beyond `upper`, else materialize, increment, return. If the leaf is exhausted pop frames
    /// until an internal frame has `next_child <= len`, descend into that child down to its
    /// leftmost leaf (pushing frames, checking levels), and continue. Empty stack → None.
    pub fn advance(&mut self) -> Result<Option<(Vec<u8>, Vec<u8>)>> {
        if self.done {
            return Ok(None);
        }
        let src = self.src;
        loop {
            // Decide the next step while borrowing the top frame, then act after the borrow ends.
            let (child, child_level) = match self.stack.last_mut() {
                None => {
                    self.done = true;
                    return Ok(None);
                }
                Some(Frame::Leaf { page, spans, next }) => {
                    if *next < spans.len() {
                        let span = &spans[*next];
                        let key = span_key(&page.payload, span);
                        if beyond_upper(&self.upper, key) {
                            self.done = true;
                            return Ok(None);
                        }
                        let val = materialize(src, span_value(&page.payload, span));
                        let key = key.to_vec();
                        *next += 1;
                        return match val {
                            Ok(v) => Ok(Some((key, v))),
                            Err(e) => {
                                self.done = true;
                                Err(e)
                            }
                        };
                    }
                    self.stack.pop();
                    continue;
                }
                Some(Frame::Internal { level, first_child, spans, next_child, .. }) => {
                    if *next_child <= spans.len() {
                        let c = child_of(*first_child, spans, *next_child);
                        *next_child += 1;
                        (c, *level - 1)
                    } else {
                        self.stack.pop();
                        continue;
                    }
                }
            };
            if let Err(e) = descend(src, &mut self.stack, child, Some(child_level), &|_, _| 0) {
                self.done = true;
                return Err(e);
            }
        }
    }
}

impl Iterator for RangeIter<'_> {
    type Item = Result<(Vec<u8>, Vec<u8>)>;
    fn next(&mut self) -> Option<Self::Item> {
        if self.done {
            return None;
        }
        match self.advance() {
            Ok(Some(kv)) => Some(Ok(kv)),
            Ok(None) => {
                self.done = true;
                None
            }
            Err(e) => {
                self.done = true;
                Some(Err(e))
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::btree::node::{InternalEntry, InternalNode, LeafEntry, LeafNode, LeafValue};
    use crate::btree::overflow::write_overflow;
    use crate::format::page::Page;
    use crate::pager::PageSink;
    use pgarbor_common::CommitSeq;
    use std::collections::{BTreeMap, HashMap};
    use std::sync::Arc;

    const PS: u32 = 4096;

    #[derive(Default)]
    struct Store {
        pages: HashMap<u64, PageRef>,
        next: u64,
    }

    impl PageSource for Store {
        fn page_size(&self) -> u32 {
            PS
        }
        fn read_page(&self, id: PageId) -> Result<PageRef> {
            self.pages.get(&id.0).cloned().ok_or_else(|| error::invalid_page(id, "missing page"))
        }
    }

    impl PageSink for Store {
        fn alloc_page(&mut self) -> Result<PageId> {
            self.next += 1;
            Ok(PageId(self.next))
        }
        fn write_page(&mut self, id: PageId, kind: PageKind, payload: Vec<u8>) -> Result<()> {
            self.pages.insert(id.0, Arc::new(Page { id, kind, commit_seq: CommitSeq(1), payload }));
            Ok(())
        }
        fn is_private(&self, _id: PageId) -> bool {
            false
        }
        fn release_page(&mut self, id: PageId) -> Result<()> {
            self.pages.remove(&id.0);
            Ok(())
        }
    }

    fn inline(k: &str, v: &str) -> LeafEntry {
        LeafEntry { key: k.as_bytes().to_vec(), value: LeafValue::Inline(v.as_bytes().to_vec()) }
    }

    fn put_leaf(st: &mut Store, entries: Vec<LeafEntry>) -> PageId {
        let p = LeafNode { entries }.encode(PS).unwrap();
        st.write_new(PageKind::Leaf, p).unwrap()
    }

    fn put_internal(st: &mut Store, level: u16, first: PageId, rest: Vec<(&str, PageId)>) -> PageId {
        let n = InternalNode {
            level,
            first_child: first,
            entries: rest.into_iter().map(|(k, c)| InternalEntry { key: k.as_bytes().to_vec(), child: c }).collect(),
        };
        st.write_new(PageKind::Internal, n.encode(PS).unwrap()).unwrap()
    }

    /// Leaves {b,d} {f,h} {j,l}; "l" is an overflow value.
    fn build() -> (Store, PageId, BTreeMap<Vec<u8>, Vec<u8>>, Vec<u8>) {
        let mut st = Store::default();
        let big: Vec<u8> = (0..20_000u32).map(|i| (i % 251) as u8).collect();
        let first = write_overflow(&mut st, &big).unwrap();
        let l1 = put_leaf(&mut st, vec![inline("b", "1"), inline("d", "2")]);
        let l2 = put_leaf(&mut st, vec![inline("f", "3"), inline("h", "4")]);
        let l3 = put_leaf(
            &mut st,
            vec![inline("j", "5"), LeafEntry { key: b"l".to_vec(), value: LeafValue::Overflow { first, len: big.len() as u32 } }],
        );
        let root = put_internal(&mut st, 1, l1, vec![("f", l2), ("j", l3)]);
        let mut m = BTreeMap::new();
        for (k, v) in [("b", "1"), ("d", "2"), ("f", "3"), ("h", "4"), ("j", "5")] {
            m.insert(k.as_bytes().to_vec(), v.as_bytes().to_vec());
        }
        m.insert(b"l".to_vec(), big.clone());
        (st, root, m, big)
    }

    fn model(m: &BTreeMap<Vec<u8>, Vec<u8>>, lo: Bound<&[u8]>, hi: Bound<&[u8]>) -> Vec<(Vec<u8>, Vec<u8>)> {
        m.iter()
            .filter(|(k, _)| match lo {
                Bound::Unbounded => true,
                Bound::Included(l) => k.as_slice() >= l,
                Bound::Excluded(l) => k.as_slice() > l,
            })
            .filter(|(k, _)| match hi {
                Bound::Unbounded => true,
                Bound::Included(u) => k.as_slice() <= u,
                Bound::Excluded(u) => k.as_slice() < u,
            })
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect()
    }

    #[test]
    fn get_hits_and_misses() {
        let (st, root, m, big) = build();
        for (k, v) in &m {
            assert_eq!(get(&st, root, k).unwrap().as_ref(), Some(v));
        }
        assert_eq!(get(&st, root, b"l").unwrap().unwrap(), big);
        let misses: [&[u8]; 8] = [b"a", b"c", b"e", b"g", b"i", b"k", b"m", b""];
        for k in misses {
            assert_eq!(get(&st, root, k).unwrap(), None);
        }
        assert_eq!(get(&st, PageId::NONE, b"b").unwrap(), None);
    }

    #[test]
    fn all_bound_combinations_match_model() {
        let (st, root, m, _) = build();
        let probes: [&[u8]; 14] = [b"", b"a", b"b", b"c", b"d", b"e", b"f", b"g", b"h", b"i", b"j", b"k", b"l", b"m"];
        let mut lows: Vec<Bound<&[u8]>> = vec![Bound::Unbounded];
        let mut highs: Vec<Bound<&[u8]>> = vec![Bound::Unbounded];
        for p in probes {
            lows.push(Bound::Included(p));
            lows.push(Bound::Excluded(p));
            highs.push(Bound::Included(p));
            highs.push(Bound::Excluded(p));
        }
        for lo in &lows {
            for hi in &highs {
                let got: Vec<_> = RangeIter::new(&st, root, *lo, *hi).unwrap().map(|r| r.unwrap()).collect();
                assert_eq!(got, model(&m, *lo, *hi), "lo={lo:?} hi={hi:?}");
            }
        }
    }

    #[test]
    fn empty_tree_and_single_leaf_root() {
        let st = Store::default();
        let mut it = RangeIter::new(&st, PageId::NONE, Bound::Unbounded, Bound::Unbounded).unwrap();
        assert!(it.next().is_none());
        let mut st = Store::default();
        let root = put_leaf(&mut st, vec![inline("a", "1"), inline("b", "2")]);
        let got: Vec<_> = RangeIter::new(&st, root, Bound::Excluded(&b"a"[..]), Bound::Unbounded)
            .unwrap()
            .map(|r| r.unwrap())
            .collect();
        assert_eq!(got, vec![(b"b".to_vec(), b"2".to_vec())]);
        assert_eq!(get(&st, root, b"a").unwrap(), Some(b"1".to_vec()));
    }

    #[test]
    fn wrong_child_level_is_rejected() {
        let mut st = Store::default();
        let l1 = put_leaf(&mut st, vec![inline("a", "1")]);
        let l2 = put_leaf(&mut st, vec![inline("m", "2")]);
        // Level 2 root whose children are leaves (should be level 1).
        let root = put_internal(&mut st, 2, l1, vec![("m", l2)]);
        assert!(get(&st, root, b"a").is_err());
        assert!(RangeIter::new(&st, root, Bound::Unbounded, Bound::Unbounded).is_err());
    }

    #[test]
    fn overflow_page_where_node_expected() {
        let mut st = Store::default();
        let first = write_overflow(&mut st, &vec![7u8; 10_000]).unwrap();
        assert!(get(&st, first, b"x").is_err());
    }
}
