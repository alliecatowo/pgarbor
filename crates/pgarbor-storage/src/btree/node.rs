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
    pub fn encode(&self, page_size: u32) -> Result<Vec<u8>> {
        let cap = node_capacity(page_size);
        let total = self.encoded_len();
        if total > cap {
            return Err(PgError::internal(format!("leaf node overflow: {total} bytes exceeds capacity {cap}")));
        }
        let nkeys = u16::try_from(self.entries.len())
            .map_err(|_| PgError::internal(format!("leaf node has {} entries, more than u16 allows", self.entries.len())))?;
        let mut out: Vec<u8> = Vec::with_capacity(total);
        out.extend_from_slice(&nkeys.to_le_bytes());
        out.extend_from_slice(&0u16.to_le_bytes()); // level
        let slots_at = out.len();
        out.resize(slots_at + SLOT_LEN * self.entries.len(), 0);
        let mut prev: Option<&[u8]> = None;
        for (i, e) in self.entries.iter().enumerate() {
            let key = e.key.as_slice();
            if let Some(p) = prev {
                if p >= key {
                    return Err(PgError::internal(format!("leaf keys out of order at entry {i}")));
                }
            }
            prev = Some(key);
            let cell_at = out.len();
            let off = u16::try_from(cell_at)
                .map_err(|_| PgError::internal(format!("leaf cell offset {cell_at} exceeds u16")))?;
            let slot = slots_at + SLOT_LEN * i;
            out[slot..slot + SLOT_LEN].copy_from_slice(&off.to_le_bytes());
            let key_len = u16::try_from(key.len())
                .map_err(|_| PgError::internal(format!("leaf key length {} exceeds u16", key.len())))?;
            out.extend_from_slice(&key_len.to_le_bytes());
            match &e.value {
                LeafValue::Inline(v) => {
                    let vlen = u32::try_from(v.len())
                        .map_err(|_| PgError::internal(format!("leaf value length {} exceeds u32", v.len())))?;
                    out.push(VTAG_INLINE);
                    out.extend_from_slice(&vlen.to_le_bytes());
                    out.extend_from_slice(key);
                    out.extend_from_slice(v);
                }
                LeafValue::Overflow { first, len } => {
                    if first.is_none() {
                        return Err(PgError::internal("overflow leaf value has a null first page"));
                    }
                    out.push(VTAG_OVERFLOW);
                    out.extend_from_slice(&len.to_le_bytes());
                    out.extend_from_slice(key);
                    out.extend_from_slice(&first.0.to_le_bytes());
                }
            }
        }
        debug_assert_eq!(out.len(), total);
        Ok(out)
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
    pub fn encode(&self, page_size: u32) -> Result<Vec<u8>> {
        if self.level == 0 {
            return Err(PgError::internal("internal node with level 0"));
        }
        if self.entries.is_empty() {
            return Err(PgError::internal("internal node without separators"));
        }
        if self.first_child.is_none() {
            return Err(PgError::internal("null child pointer at 0"));
        }
        let cap = node_capacity(page_size);
        let total = self.encoded_len();
        if total > cap {
            return Err(PgError::internal(format!("internal node overflow: {total} bytes exceeds capacity {cap}")));
        }
        let nkeys = u16::try_from(self.entries.len())
            .map_err(|_| PgError::internal(format!("internal node has {} separators, more than u16 allows", self.entries.len())))?;
        let mut out: Vec<u8> = Vec::with_capacity(total);
        out.extend_from_slice(&nkeys.to_le_bytes());
        out.extend_from_slice(&self.level.to_le_bytes());
        out.extend_from_slice(&self.first_child.0.to_le_bytes());
        let slots_at = out.len();
        out.resize(slots_at + SLOT_LEN * self.entries.len(), 0);
        let mut prev: Option<&[u8]> = None;
        for (i, e) in self.entries.iter().enumerate() {
            let key = e.key.as_slice();
            if let Some(p) = prev {
                if p >= key {
                    return Err(PgError::internal(format!("internal keys out of order at entry {i}")));
                }
            }
            prev = Some(key);
            if e.child.is_none() {
                return Err(PgError::internal(format!("null child pointer at {}", i + 1)));
            }
            let cell_at = out.len();
            let off = u16::try_from(cell_at)
                .map_err(|_| PgError::internal(format!("internal cell offset {cell_at} exceeds u16")))?;
            let slot = slots_at + SLOT_LEN * i;
            out[slot..slot + SLOT_LEN].copy_from_slice(&off.to_le_bytes());
            let key_len = u16::try_from(key.len())
                .map_err(|_| PgError::internal(format!("internal key length {} exceeds u16", key.len())))?;
            out.extend_from_slice(&key_len.to_le_bytes());
            out.extend_from_slice(&e.child.0.to_le_bytes());
            out.extend_from_slice(key);
        }
        debug_assert_eq!(out.len(), total);
        Ok(out)
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
pub fn leaf_spans(id: PageId, payload: &[u8]) -> Result<Vec<LeafSpan>> {
    let nkeys = le_u16(payload, 0).ok_or_else(|| error::invalid_page(id, "truncated leaf header"))? as usize;
    let level = le_u16(payload, 2).ok_or_else(|| error::invalid_page(id, "truncated leaf header"))?;
    if level != 0 {
        return Err(error::invalid_page(id, format!("leaf with level {level}")));
    }
    let slots_end = LEAF_HEADER_LEN + nkeys * SLOT_LEN;
    if payload.len() < slots_end {
        return Err(error::invalid_page(id, "truncated leaf header"));
    }
    let mut spans: Vec<LeafSpan> = Vec::with_capacity(nkeys);
    let mut prev: Option<&[u8]> = None;
    for i in 0..nkeys {
        let off = le_u16(payload, LEAF_HEADER_LEN + i * SLOT_LEN).ok_or_else(|| cell_oob(id, i))? as usize;
        if off < slots_end {
            return Err(cell_oob(id, i));
        }
        let key_len = le_u16(payload, off).ok_or_else(|| cell_oob(id, i))? as usize;
        let vtag = *payload.get(off + 2).ok_or_else(|| cell_oob(id, i))?;
        let vlen = le_u32(payload, off + 3).ok_or_else(|| cell_oob(id, i))?;
        if vtag != VTAG_INLINE && vtag != VTAG_OVERFLOW {
            return Err(error::invalid_page(id, format!("cell {i} has unknown value tag {vtag}")));
        }
        let key_start = off + LEAF_CELL_OVERHEAD;
        let key_end = key_start + key_len;
        let key = payload.get(key_start..key_end).ok_or_else(|| cell_oob(id, i))?;
        let val_start = key_end;
        let val_bytes = if vtag == VTAG_OVERFLOW { OVERFLOW_REF_LEN } else { vlen as usize };
        let val_end = val_start.checked_add(val_bytes).ok_or_else(|| cell_oob(id, i))?;
        if payload.len() < val_end {
            return Err(cell_oob(id, i));
        }
        if vtag == VTAG_OVERFLOW {
            let first = le_u64(payload, val_start).ok_or_else(|| cell_oob(id, i))?;
            if first == 0 {
                return Err(error::invalid_page(id, format!("cell {i} has null overflow pointer")));
            }
        }
        if let Some(p) = prev {
            if key <= p {
                return Err(error::invalid_page(id, format!("keys out of order at cell {i}")));
            }
        }
        prev = Some(key);
        spans.push(LeafSpan {
            key_start: span_u32(key_start, id, i)?,
            key_len: key_len as u32,
            vtag,
            val_start: span_u32(val_start, id, i)?,
            vlen,
        });
    }
    Ok(spans)
}

// CONTRACT
/// Parse + validate an internal payload. Returns `(level, first_child, spans)`. Errors as for
/// `leaf_spans` plus: level 0 ("internal node with level 0"), nkeys 0 ("internal node without
/// separators"), any child (incl. first) == 0 ("null child pointer at <i>").
pub fn internal_spans(id: PageId, payload: &[u8]) -> Result<(u16, PageId, Vec<InternalSpan>)> {
    let nkeys = le_u16(payload, 0).ok_or_else(|| error::invalid_page(id, "truncated internal header"))? as usize;
    let level = le_u16(payload, 2).ok_or_else(|| error::invalid_page(id, "truncated internal header"))?;
    let first_child = le_u64(payload, 4).ok_or_else(|| error::invalid_page(id, "truncated internal header"))?;
    if level == 0 {
        return Err(error::invalid_page(id, "internal node with level 0"));
    }
    if nkeys == 0 {
        return Err(error::invalid_page(id, "internal node without separators"));
    }
    if first_child == 0 {
        return Err(error::invalid_page(id, "null child pointer at 0"));
    }
    let slots_end = INTERNAL_HEADER_LEN + nkeys * SLOT_LEN;
    if payload.len() < slots_end {
        return Err(error::invalid_page(id, "truncated internal header"));
    }
    let mut spans: Vec<InternalSpan> = Vec::with_capacity(nkeys);
    let mut prev: Option<&[u8]> = None;
    for i in 0..nkeys {
        let off = le_u16(payload, INTERNAL_HEADER_LEN + i * SLOT_LEN).ok_or_else(|| cell_oob(id, i))? as usize;
        if off < slots_end {
            return Err(cell_oob(id, i));
        }
        let key_len = le_u16(payload, off).ok_or_else(|| cell_oob(id, i))? as usize;
        let child = le_u64(payload, off + 2).ok_or_else(|| cell_oob(id, i))?;
        if child == 0 {
            // Cell i carries child i + 1 (child 0 is the header's first child).
            return Err(error::invalid_page(id, format!("null child pointer at {}", i + 1)));
        }
        let key_start = off + INTERNAL_CELL_OVERHEAD;
        let key_end = key_start + key_len;
        let key = payload.get(key_start..key_end).ok_or_else(|| cell_oob(id, i))?;
        if let Some(p) = prev {
            if key <= p {
                return Err(error::invalid_page(id, format!("keys out of order at cell {i}")));
            }
        }
        prev = Some(key);
        spans.push(InternalSpan {
            key_start: span_u32(key_start, id, i)?,
            key_len: key_len as u32,
            child: PageId(child),
        });
    }
    Ok((level, PageId(first_child), spans))
}

/// Little-endian u16 at `at`, or `None` when out of bounds.
fn le_u16(p: &[u8], at: usize) -> Option<u16> {
    let end = at.checked_add(2)?;
    Some(u16::from_le_bytes(<[u8; 2]>::try_from(p.get(at..end)?).ok()?))
}

/// Little-endian u32 at `at`, or `None` when out of bounds.
fn le_u32(p: &[u8], at: usize) -> Option<u32> {
    let end = at.checked_add(4)?;
    Some(u32::from_le_bytes(<[u8; 4]>::try_from(p.get(at..end)?).ok()?))
}

/// Little-endian u64 at `at`, or `None` when out of bounds.
fn le_u64(p: &[u8], at: usize) -> Option<u64> {
    let end = at.checked_add(8)?;
    Some(u64::from_le_bytes(<[u8; 8]>::try_from(p.get(at..end)?).ok()?))
}

fn cell_oob(id: PageId, i: usize) -> PgError {
    error::invalid_page(id, format!("cell {i} out of bounds"))
}

/// Span offsets are stored as u32; a payload offset that does not fit is reported as corruption.
fn span_u32(v: usize, id: PageId, i: usize) -> Result<u32> {
    u32::try_from(v).map_err(|_| cell_oob(id, i))
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

    const PS: u32 = 8192;

    /// Small deterministic generator for the fuzz-style tests (no external RNG).
    struct Lcg(u64);
    impl Lcg {
        fn next_u64(&mut self) -> u64 {
            self.0 = self.0.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
            self.0 >> 33
        }
        fn below(&mut self, n: u64) -> u64 {
            self.next_u64() % n
        }
        fn bytes(&mut self, len: usize) -> Vec<u8> {
            (0..len).map(|_| self.next_u64() as u8).collect()
        }
    }

    fn entry(key: &[u8], value: LeafValue) -> LeafEntry {
        LeafEntry { key: key.to_vec(), value }
    }

    #[test]
    fn empty_leaf_roundtrip() {
        let node = LeafNode::default();
        let payload = node.encode(PS).unwrap();
        assert_eq!(payload, vec![0, 0, 0, 0]);
        assert_eq!(payload.len(), node.encoded_len());
        assert_eq!(LeafNode::decode(PageId(1), &payload).unwrap(), node);
    }

    #[test]
    fn mixed_leaf_roundtrip_and_length() {
        let node = LeafNode {
            entries: vec![
                entry(b"a", LeafValue::Inline(vec![1, 2, 3])),
                entry(b"bb", LeafValue::Overflow { first: PageId(9), len: 100_000 }),
                entry(b"ccc", LeafValue::Inline(Vec::new())),
            ],
        };
        let payload = node.encode(PS).unwrap();
        // 4 + (2+7+1+3) + (2+7+2+8) + (2+7+3+0)
        assert_eq!(node.encoded_len(), 48);
        assert_eq!(payload.len(), 48);
        assert_eq!(LeafNode::decode(PageId(1), &payload).unwrap(), node);
        let spans = leaf_spans(PageId(1), &payload).unwrap();
        assert_eq!(spans.len(), 3);
        assert_eq!(span_key(&payload, &spans[1]), b"bb");
        assert_eq!(span_value(&payload, &spans[1]), LeafValueRef::Overflow { first: PageId(9), len: 100_000 });
        assert_eq!(span_value(&payload, &spans[2]), LeafValueRef::Inline(&[]));
    }

    #[test]
    fn documented_leaf_example() {
        let payload: Vec<u8> = vec![1, 0, 0, 0, 6, 0, 1, 0, 0, 1, 0, 0, 0, b'k', b'v'];
        let spans = leaf_spans(PageId(1), &payload).unwrap();
        assert_eq!(
            spans,
            vec![LeafSpan { key_start: 13, key_len: 1, vtag: VTAG_INLINE, val_start: 14, vlen: 1 }]
        );
        let node = LeafNode::decode(PageId(1), &payload).unwrap();
        assert_eq!(node.entries, vec![entry(b"k", LeafValue::Inline(b"v".to_vec()))]);
        assert_eq!(node.encode(PS).unwrap(), payload);
    }

    #[test]
    fn max_size_leaf_cells_fit_two_per_node() {
        let k1 = vec![0u8; max_key_len(PS)];
        let k2 = vec![1u8; max_key_len(PS)];
        let node = LeafNode {
            entries: vec![
                entry(&k1, LeafValue::Inline(vec![7u8; max_inline_value(PS)])),
                entry(&k2, LeafValue::Inline(vec![8u8; max_inline_value(PS)])),
            ],
        };
        let payload = node.encode(PS).unwrap();
        assert_eq!(payload.len(), node.encoded_len());
        assert_eq!(LeafNode::decode(PageId(3), &payload).unwrap(), node);
    }

    #[test]
    fn leaf_overflow_is_internal_error() {
        let k = |b: u8| vec![b; max_key_len(PS)];
        let node = LeafNode {
            entries: vec![
                entry(&k(0), LeafValue::Inline(vec![0u8; max_inline_value(PS)])),
                entry(&k(1), LeafValue::Inline(vec![0u8; max_inline_value(PS)])),
                entry(&k(2), LeafValue::Inline(vec![0u8; max_inline_value(PS)])),
            ],
        };
        let err = node.encode(PS).unwrap_err();
        assert_eq!(err.sqlstate, pgarbor_common::SqlState::INTERNAL_ERROR);
        assert!(err.message.starts_with("leaf node overflow: "), "{}", err.message);
    }

    #[test]
    fn leaf_encode_rejects_unordered_keys() {
        let node = LeafNode {
            entries: vec![entry(b"b", LeafValue::Inline(vec![])), entry(b"a", LeafValue::Inline(vec![]))],
        };
        assert!(node.encode(PS).is_err());
        let dup = LeafNode {
            entries: vec![entry(b"a", LeafValue::Inline(vec![])), entry(b"a", LeafValue::Inline(vec![]))],
        };
        assert!(dup.encode(PS).is_err());
    }

    #[test]
    fn leaf_encode_rejects_null_overflow_page() {
        let node = LeafNode { entries: vec![entry(b"a", LeafValue::Overflow { first: PageId::NONE, len: 9 })] };
        assert!(node.encode(PS).is_err());
    }

    #[test]
    fn leaf_parse_reports_reasons() {
        let e = leaf_spans(PageId(1), &[]).unwrap_err();
        assert_eq!(e.sqlstate, pgarbor_common::SqlState::DATA_CORRUPTED);
        assert_eq!(e.detail.as_deref(), Some("truncated leaf header"));

        let e = leaf_spans(PageId(1), &[0, 0, 1, 0]).unwrap_err();
        assert_eq!(e.detail.as_deref(), Some("leaf with level 1"));

        // one slot pointing into the header
        let e = leaf_spans(PageId(1), &[1, 0, 0, 0, 2, 0]).unwrap_err();
        assert_eq!(e.detail.as_deref(), Some("cell 0 out of bounds"));

        // unknown value tag 7
        let e = leaf_spans(PageId(1), &[1, 0, 0, 0, 6, 0, 1, 0, 7, 0, 0, 0, 0, b'k']).unwrap_err();
        assert_eq!(e.detail.as_deref(), Some("cell 0 has unknown value tag 7"));

        // overflow cell with a zero first page
        let mut p = vec![1, 0, 0, 0, 6, 0, 1, 0, 1, 1, 0, 0, 0, b'k'];
        p.extend_from_slice(&[0u8; 8]);
        let e = leaf_spans(PageId(1), &p).unwrap_err();
        assert_eq!(e.detail.as_deref(), Some("cell 0 has null overflow pointer"));

        // keys out of order: "b" then "a"
        let p = vec![
            2, 0, 0, 0, // nkeys 2, level 0
            8, 0, 16, 0, // slots
            1, 0, 0, 0, 0, 0, 0, b'b', // cell 0: key "b", inline empty value
            1, 0, 0, 0, 0, 0, 0, b'a', // cell 1: key "a", inline empty value
        ];
        let e = leaf_spans(PageId(1), &p).unwrap_err();
        assert_eq!(e.detail.as_deref(), Some("keys out of order at cell 1"));
    }

    fn sample_internal() -> InternalNode {
        InternalNode {
            level: 1,
            first_child: PageId(2),
            entries: vec![
                InternalEntry { key: b"m".to_vec(), child: PageId(3) },
                InternalEntry { key: b"t".to_vec(), child: PageId(4) },
            ],
        }
    }

    #[test]
    fn internal_roundtrip() {
        let node = sample_internal();
        let payload = node.encode(PS).unwrap();
        // 12 + 2 * (2 + 10 + 1)
        assert_eq!(payload.len(), 38);
        assert_eq!(payload.len(), node.encoded_len());
        assert_eq!(InternalNode::decode(PageId(5), &payload).unwrap(), node);
        let (level, first, spans) = internal_spans(PageId(5), &payload).unwrap();
        assert_eq!((level, first, spans.len()), (1, PageId(2), 2));
        assert_eq!(internal_span_key(&payload, &spans[1]), b"t");
        assert_eq!(spans[1].child, PageId(4));
    }

    #[test]
    fn internal_single_separator_roundtrip() {
        let node = InternalNode {
            level: 3,
            first_child: PageId(1),
            entries: vec![InternalEntry { key: Vec::new(), child: PageId(2) }],
        };
        let payload = node.encode(PS).unwrap();
        assert_eq!(payload.len(), node.encoded_len());
        assert_eq!(InternalNode::decode(PageId(1), &payload).unwrap(), node);
    }

    #[test]
    fn internal_encode_rejects_invalid_input() {
        let mut n = sample_internal();
        n.entries.clear();
        assert!(n.encode(PS).is_err());

        let mut n = sample_internal();
        n.level = 0;
        assert!(n.encode(PS).is_err());

        let mut n = sample_internal();
        n.first_child = PageId::NONE;
        assert!(n.encode(PS).is_err());

        let mut n = sample_internal();
        n.entries[1].child = PageId::NONE;
        assert!(n.encode(PS).is_err());

        let mut n = sample_internal();
        n.entries[1].key = b"a".to_vec();
        assert!(n.encode(PS).is_err());
    }

    #[test]
    fn internal_parse_reports_reasons() {
        let e = internal_spans(PageId(1), &[1, 0, 0, 0]).unwrap_err();
        assert_eq!(e.detail.as_deref(), Some("truncated internal header"));
        let mut p = sample_internal().encode(PS).unwrap();
        p[2] = 0; // level 0
        let e = internal_spans(PageId(1), &p).unwrap_err();
        assert_eq!(e.detail.as_deref(), Some("internal node with level 0"));
        let mut p = sample_internal().encode(PS).unwrap();
        p[0] = 0;
        p[1] = 0;
        let e = internal_spans(PageId(1), &p).unwrap_err();
        assert_eq!(e.detail.as_deref(), Some("internal node without separators"));
        let mut p = sample_internal().encode(PS).unwrap();
        p[4..12].copy_from_slice(&[0u8; 8]);
        let e = internal_spans(PageId(1), &p).unwrap_err();
        assert_eq!(e.detail.as_deref(), Some("null child pointer at 0"));
        let mut p = sample_internal().encode(PS).unwrap();
        // second cell's child becomes null: cell 1 carries child index 2
        let off = u16::from_le_bytes([p[14], p[15]]) as usize;
        p[off + 2..off + 10].copy_from_slice(&[0u8; 8]);
        let e = internal_spans(PageId(1), &p).unwrap_err();
        assert_eq!(e.detail.as_deref(), Some("null child pointer at 2"));
    }

    #[test]
    fn random_payloads_never_panic() {
        let mut rng = Lcg(0x5EED_1234_ABCD_0001);
        for _ in 0..20_000 {
            let len = rng.below(300) as usize;
            let mut p = rng.bytes(len);
            // Bias the header so the slot/cell paths are exercised, not just the truncation check.
            if len >= 12 && rng.below(2) == 0 {
                p[0] = rng.below(8) as u8;
                p[1] = 0;
                p[2] = rng.below(3) as u8;
                p[3] = 0;
            }
            let _ = leaf_spans(PageId(1), &p);
            let _ = internal_spans(PageId(1), &p);
            let _ = LeafNode::decode(PageId(1), &p);
            let _ = InternalNode::decode(PageId(1), &p);
        }
    }

    #[test]
    fn mutated_valid_payloads_never_panic() {
        let mut rng = Lcg(0xFEED_BEEF_0042_0007);
        let leaf = LeafNode {
            entries: vec![
                entry(b"alpha", LeafValue::Inline(vec![1, 2, 3, 4])),
                entry(b"beta", LeafValue::Overflow { first: PageId(5), len: 70_000 }),
                entry(b"gamma", LeafValue::Inline(vec![9; 20])),
            ],
        }
        .encode(PS)
        .unwrap();
        let internal = sample_internal().encode(PS).unwrap();
        for base in [&leaf, &internal] {
            for _ in 0..10_000 {
                let mut p = base.clone();
                let flips = 1 + rng.below(4) as usize;
                for _ in 0..flips {
                    let at = rng.below(p.len() as u64) as usize;
                    p[at] = rng.next_u64() as u8;
                }
                let cut = rng.below(p.len() as u64 + 1) as usize;
                p.truncate(cut);
                let _ = leaf_spans(PageId(1), &p);
                let _ = internal_spans(PageId(1), &p);
            }
        }
    }
}
