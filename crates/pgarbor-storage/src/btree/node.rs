//! B+tree node layout (STORE-05). Lease STO-05 (`leaf_spans`, `internal_spans`, `LeafNode::encode`,
//! `InternalNode::encode`). Everything else here is implemented.
//!
//! # Leaf node payload (CONTRACT, little-endian; page kind = Leaf)
//! ```text
//! 0      2  nkeys  (u16)
//! 2      2  level  (u16) = 0
//! 4   2*n   slot[i] (u16) = offset of cell i from payload start; cells in ascending key order
//! ...       cells, packed back-to-back right after the slot array, in slot order:
//!           key_len u16 | vtag u8 | vlen u32 | key[key_len] | vbytes
//!           vtag 0 (inline):   vbytes = value[vlen]
//!           vtag 1 (overflow): vbytes = first overflow PageId u64 (8 bytes); vlen = total value length
//! payload_len = end of last cell
//! ```
//! # Internal node payload (CONTRACT; page kind = Internal)
//! ```text
//! 0      2  nkeys (u16) >= 1   (number of separator keys; children = nkeys + 1)
//! 2      2  level (u16) >= 1   (children have level - 1; leaves are level 0)
//! 4      8  child0 (PageId u64) leftmost child
//! 12  2*n   slot[i] (u16) offsets of cells
//! ...       cells: key_len u16 | child u64 | key[key_len]      (cell i = (key_i, child_{i+1}))
//! ```
//! Routing invariant: keys in subtree child_0 < key_0 <= keys in child_1 < key_1 <= ... ; i.e. the
//! child for `k` is index `#{i : key_i <= k}`. Separators are the full first key of the right
//! subtree at split time (no suffix truncation in format v1).
//!
//! # Size rules (CONTRACT)
//! - `max_key_len(ps) = ps/4 - 64` (8192 → 1984). Longer keys → `error::key_too_large` (54000).
//! - values with `len > max_inline_value(ps) = ps/4` (8192 → 2048) are stored in overflow chains.
//! - Hence every leaf cell (incl. its slot) is <= ps/2 - 55, so any two cells fit one leaf, and
//!   every internal cell is <= ps/4 - 52, so >= 3 separators fit one internal node.
//! - Node capacity = `payload_capacity(ps)` = ps - 32. Underflow threshold = capacity / 4.
use crate::error;
use crate::format::consts::{payload_capacity, PageKind};
use crate::format::page::Page;
use crate::pager::PageSink;
use pgarbor_common::{PageId, PgError, Result};

// CONTRACT
pub const LEAF_HEADER_LEN: usize = 4;
// CONTRACT
pub const INTERNAL_HEADER_LEN: usize = 12;
// CONTRACT
pub const SLOT_LEN: usize = 2;
// CONTRACT
pub const LEAF_CELL_OVERHEAD: usize = 7;
// CONTRACT
pub const INTERNAL_CELL_OVERHEAD: usize = 10;
// CONTRACT
pub const OVERFLOW_REF_LEN: usize = 8;
// CONTRACT
pub const VTAG_INLINE: u8 = 0;
// CONTRACT
pub const VTAG_OVERFLOW: u8 = 1;

// CONTRACT
/// Largest key a tree accepts for this page size.
pub fn max_key_len(page_size: u32) -> usize {
    page_size as usize / 4 - 64
}
// CONTRACT
/// Largest value stored inline in a leaf; longer values go to overflow pages.
pub fn max_inline_value(page_size: u32) -> usize {
    page_size as usize / 4
}
// CONTRACT
pub fn node_capacity(page_size: u32) -> usize {
    payload_capacity(page_size)
}
// CONTRACT
/// A non-root node whose `encoded_len` is below this is rebalanced on delete.
pub fn underflow_threshold(page_size: u32) -> usize {
    node_capacity(page_size) / 4
}

// CONTRACT
/// A leaf value as stored in a cell.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum LeafValue {
    Inline(Vec<u8>),
    Overflow { first: PageId, len: u32 },
}

impl LeafValue {
    /// Bytes this value occupies inside the cell (inline length or 8).
    pub fn stored_len(&self) -> usize {
        match self {
            LeafValue::Inline(v) => v.len(),
            LeafValue::Overflow { .. } => OVERFLOW_REF_LEN,
        }
    }
    /// Logical value length.
    pub fn value_len(&self) -> u64 {
        match self {
            LeafValue::Inline(v) => v.len() as u64,
            LeafValue::Overflow { len, .. } => *len as u64,
        }
    }
}

// CONTRACT
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LeafEntry {
    pub key: Vec<u8>,
    pub value: LeafValue,
}

impl LeafEntry {
    /// Cell bytes + slot.
    pub fn encoded_len(&self) -> usize {
        SLOT_LEN + LEAF_CELL_OVERHEAD + self.key.len() + self.value.stored_len()
    }
}

// CONTRACT
/// Owned, decoded leaf (used by mutation paths).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct LeafNode {
    pub entries: Vec<LeafEntry>,
}

impl LeafNode {
    pub fn encoded_len(&self) -> usize {
        LEAF_HEADER_LEN + self.entries.iter().map(LeafEntry::encoded_len).sum::<usize>()
    }
    /// Binary search by key (bytewise): `Ok(i)` found, `Err(i)` insertion point.
    pub fn search(&self, key: &[u8]) -> std::result::Result<usize, usize> {
        self.entries.binary_search_by(|e| e.key.as_slice().cmp(key))
    }
    /// Encode to a payload per the layout above. Errors: `PgError::internal("leaf node overflow:
    /// <len> bytes exceeds capacity <cap>")` if `encoded_len() > node_capacity(page_size)`;
    /// `PgError::internal` if a key exceeds u16 or entries are not strictly ascending
    /// (debug-quality checks; callers guarantee both).
    #[allow(unused_variables)]
    pub fn encode(&self, page_size: u32) -> Result<Vec<u8>> {
        Err(PgError::unimplemented("STO-05", "leaf node encoding"))
    }
    /// Decode from a verified page payload (implemented via `leaf_spans`).
    pub fn decode(id: PageId, payload: &[u8]) -> Result<LeafNode> {
        Ok(LeafView::parse(id, payload)?.to_owned_node())
    }
}

// CONTRACT
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct InternalEntry {
    /// Separator: smallest key that routes to `child`.
    pub key: Vec<u8>,
    pub child: PageId,
}

impl InternalEntry {
    pub fn encoded_len(&self) -> usize {
        SLOT_LEN + INTERNAL_CELL_OVERHEAD + self.key.len()
    }
}

// CONTRACT
/// Owned, decoded internal node.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct InternalNode {
    pub level: u16,
    pub first_child: PageId,
    pub entries: Vec<InternalEntry>,
}

impl InternalNode {
    pub fn encoded_len(&self) -> usize {
        INTERNAL_HEADER_LEN + self.entries.iter().map(InternalEntry::encoded_len).sum::<usize>()
    }
    pub fn num_children(&self) -> usize {
        self.entries.len() + 1
    }
    /// Child index for `key`: number of separators `<= key`.
    pub fn child_index(&self, key: &[u8]) -> usize {
        self.entries.partition_point(|e| e.key.as_slice() <= key)
    }
    /// Child pointer `i` (0 = first_child). Caller guarantees `i < num_children()`.
    pub fn child_at(&self, i: usize) -> PageId {
        if i == 0 { self.first_child } else { self.entries[i - 1].child }
    }
    /// Replace child pointer `i`.
    pub fn set_child(&mut self, i: usize, id: PageId) {
        if i == 0 {
            self.first_child = id
        } else {
            self.entries[i - 1].child = id
        }
    }
    pub fn children(&self) -> Vec<PageId> {
        let mut v = Vec::with_capacity(self.num_children());
        v.push(self.first_child);
        v.extend(self.entries.iter().map(|e| e.child));
        v
    }
    /// Encode per the layout. Errors: overflow / empty `entries` / level 0 → `PgError::internal`.
    #[allow(unused_variables)]
    pub fn encode(&self, page_size: u32) -> Result<Vec<u8>> {
        Err(PgError::unimplemented("STO-05", "internal node encoding"))
    }
    /// Decode from a verified payload (implemented via `internal_spans`).
    pub fn decode(id: PageId, payload: &[u8]) -> Result<InternalNode> {
        Ok(InternalView::parse(id, payload)?.to_owned_node())
    }
}

// CONTRACT
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Node {
    Leaf(LeafNode),
    Internal(InternalNode),
}

// CONTRACT
/// Decode a verified page into a node; non-node kinds → `error::invalid_page(id, "expected a
/// b-tree node, found <kind> page")`.
pub fn decode_node(page: &Page) -> Result<Node> {
    match page.kind {
        PageKind::Leaf => Ok(Node::Leaf(LeafNode::decode(page.id, &page.payload)?)),
        PageKind::Internal => Ok(Node::Internal(InternalNode::decode(page.id, &page.payload)?)),
        k => Err(error::invalid_page(page.id, format!("expected a b-tree node, found {} page", k.name()))),
    }
}

// CONTRACT
/// Encode a node, returning its page kind.
pub fn encode_node(node: &Node, page_size: u32) -> Result<(PageKind, Vec<u8>)> {
    match node {
        Node::Leaf(l) => Ok((PageKind::Leaf, l.encode(page_size)?)),
        Node::Internal(n) => Ok((PageKind::Internal, n.encode(page_size)?)),
    }
}

// CONTRACT
/// Copy-on-write a node: encode + `sink.cow_write(old, ..)`. `old` may be NONE (fresh page).
pub fn write_node(sink: &mut dyn PageSink, old: PageId, node: &Node) -> Result<PageId> {
    let (kind, payload) = encode_node(node, sink.page_size())?;
    sink.cow_write(old, kind, payload)
}

// CONTRACT
/// Location of one leaf cell inside a payload (byte offsets into the payload).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct LeafSpan {
    pub key_start: u32,
    pub key_len: u32,
    pub vtag: u8,
    /// Offset of vbytes (inline value or 8-byte overflow PageId).
    pub val_start: u32,
    /// Inline: value length. Overflow: total logical length.
    pub vlen: u32,
}

// CONTRACT
/// Location of one internal cell.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct InternalSpan {
    pub key_start: u32,
    pub key_len: u32,
    pub child: PageId,
}

// CONTRACT
/// Parse + validate a leaf payload into cell spans. All failures are
/// `error::invalid_page(id, <reason>)` (XX001) — never panic on any input bytes. Checks:
/// payload shorter than header/slot array ("truncated leaf header"); level != 0
/// ("leaf with level <l>"); slot offset < header+slots or cell header/key/value past payload end
/// ("cell <i> out of bounds"); vtag not 0/1 ("cell <i> has unknown value tag <t>"); overflow
/// cell whose 8-byte first PageId is 0 ("cell <i> has null overflow pointer"); keys not strictly
/// ascending ("keys out of order at cell <i>"). `nkeys == 0` is accepted (doctor reports it).
/// Whether an overflow `vlen` exceeds `max_inline_value` is NOT checked here (doctor does).
/// Example: payload `[1,0, 0,0, 6,0, 1,0, 0, 1,0,0,0, b'k', b'v']` → one span: key "k" at 13,
/// inline value "v" at 14 (nkeys=1, level=0, slot0=6, key_len=1, vtag=0, vlen=1).
#[allow(unused_variables)]
pub fn leaf_spans(id: PageId, payload: &[u8]) -> Result<Vec<LeafSpan>> {
    Err(PgError::unimplemented("STO-05", "leaf node parsing"))
}

// CONTRACT
/// Parse + validate an internal payload. Returns `(level, first_child, spans)`. Errors as for
/// `leaf_spans` plus: level 0 ("internal node with level 0"), nkeys 0 ("internal node without
/// separators"), any child (incl. first) == 0 ("null child pointer at <i>").
#[allow(unused_variables)]
pub fn internal_spans(id: PageId, payload: &[u8]) -> Result<(u16, PageId, Vec<InternalSpan>)> {
    Err(PgError::unimplemented("STO-05", "internal node parsing"))
}

// CONTRACT
/// Borrowed value reference from a leaf view.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LeafValueRef<'a> {
    Inline(&'a [u8]),
    Overflow { first: PageId, len: u32 },
}

// CONTRACT
/// Zero-copy validated view over a leaf payload (read path).
#[derive(Clone, Debug)]
pub struct LeafView<'a> {
    payload: &'a [u8],
    spans: Vec<LeafSpan>,
}

impl<'a> LeafView<'a> {
    pub fn parse(id: PageId, payload: &'a [u8]) -> Result<LeafView<'a>> {
        Ok(LeafView { payload, spans: leaf_spans(id, payload)? })
    }
    /// Build from spans previously produced by `leaf_spans` on the same payload.
    pub fn from_spans(payload: &'a [u8], spans: Vec<LeafSpan>) -> LeafView<'a> {
        LeafView { payload, spans }
    }
    pub fn len(&self) -> usize {
        self.spans.len()
    }
    pub fn is_empty(&self) -> bool {
        self.spans.is_empty()
    }
    pub fn spans(&self) -> &[LeafSpan] {
        &self.spans
    }
    pub fn key(&self, i: usize) -> &'a [u8] {
        span_key(self.payload, &self.spans[i])
    }
    pub fn value(&self, i: usize) -> LeafValueRef<'a> {
        span_value(self.payload, &self.spans[i])
    }
    pub fn search(&self, key: &[u8]) -> std::result::Result<usize, usize> {
        self.spans.binary_search_by(|s| span_key(self.payload, s).cmp(key))
    }
    pub fn to_owned_node(&self) -> LeafNode {
        LeafNode {
            entries: (0..self.len())
                .map(|i| LeafEntry {
                    key: self.key(i).to_vec(),
                    value: match self.value(i) {
                        LeafValueRef::Inline(v) => LeafValue::Inline(v.to_vec()),
                        LeafValueRef::Overflow { first, len } => LeafValue::Overflow { first, len },
                    },
                })
                .collect(),
        }
    }
}

/// Key bytes of a validated leaf span.
pub fn span_key<'a>(payload: &'a [u8], s: &LeafSpan) -> &'a [u8] {
    &payload[s.key_start as usize..(s.key_start + s.key_len) as usize]
}

/// Value of a validated leaf span.
pub fn span_value<'a>(payload: &'a [u8], s: &LeafSpan) -> LeafValueRef<'a> {
    let start = s.val_start as usize;
    if s.vtag == VTAG_OVERFLOW {
        let mut b = [0u8; 8];
        b.copy_from_slice(&payload[start..start + 8]);
        LeafValueRef::Overflow { first: PageId(u64::from_le_bytes(b)), len: s.vlen }
    } else {
        LeafValueRef::Inline(&payload[start..start + s.vlen as usize])
    }
}

/// Key bytes of a validated internal span.
pub fn internal_span_key<'a>(payload: &'a [u8], s: &InternalSpan) -> &'a [u8] {
    &payload[s.key_start as usize..(s.key_start + s.key_len) as usize]
}

// CONTRACT
/// Zero-copy validated view over an internal payload.
#[derive(Clone, Debug)]
pub struct InternalView<'a> {
    payload: &'a [u8],
    level: u16,
    first_child: PageId,
    spans: Vec<InternalSpan>,
}

impl<'a> InternalView<'a> {
    pub fn parse(id: PageId, payload: &'a [u8]) -> Result<InternalView<'a>> {
        let (level, first_child, spans) = internal_spans(id, payload)?;
        Ok(InternalView { payload, level, first_child, spans })
    }
    pub fn level(&self) -> u16 {
        self.level
    }
    /// Number of separator keys.
    pub fn len(&self) -> usize {
        self.spans.len()
    }
    pub fn is_empty(&self) -> bool {
        self.spans.is_empty()
    }
    pub fn num_children(&self) -> usize {
        self.spans.len() + 1
    }
    pub fn key(&self, i: usize) -> &'a [u8] {
        internal_span_key(self.payload, &self.spans[i])
    }
    pub fn child(&self, i: usize) -> PageId {
        if i == 0 { self.first_child } else { self.spans[i - 1].child }
    }
    /// Child index for `key` (number of separators `<= key`).
    pub fn child_index(&self, key: &[u8]) -> usize {
        self.spans.partition_point(|s| internal_span_key(self.payload, s) <= key)
    }
    pub fn children(&self) -> Vec<PageId> {
        (0..self.num_children()).map(|i| self.child(i)).collect()
    }
    pub fn to_owned_node(&self) -> InternalNode {
        InternalNode {
            level: self.level,
            first_child: self.first_child,
            entries: (0..self.len()).map(|i| InternalEntry { key: self.key(i).to_vec(), child: self.child(i + 1) }).collect(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn size_rules_hold() {
        for ps in [4096u32, 8192, 16384, 32768, 65536] {
            let cap = node_capacity(ps);
            let max_leaf_cell = SLOT_LEN + LEAF_CELL_OVERHEAD + max_key_len(ps) + max_inline_value(ps);
            assert!(2 * max_leaf_cell + LEAF_HEADER_LEN <= cap, "ps {ps}");
            let max_int_cell = SLOT_LEN + INTERNAL_CELL_OVERHEAD + max_key_len(ps);
            assert!(3 * max_int_cell + INTERNAL_HEADER_LEN <= cap, "ps {ps}");
        }
        assert_eq!(max_key_len(8192), 1984);
    }
    // STO-05 adds: encode/decode roundtrip (empty leaf, 1 entry, overflow entries, max-size
    // cells, internal with 1..n separators); fuzz-style: random bytes never panic in *_spans.
}
