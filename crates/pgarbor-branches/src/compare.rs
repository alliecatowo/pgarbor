//! Branch compare / divergence report (BRANCH-08). Lease BR-06 (sonnet).
//!
//! Reports HOW two branches differ without claiming any merge semantics (RFC 001 §4.4: "Do not
//! claim SQL row-level merge"). Differences are grouped by the 4-byte keyspace prefix of
//! ARCHITECTURE §4 (catalog = schema divergence, sequences, branch-local metadata, one entry per
//! relation OID = row/index divergence). This crate cannot name relations (no catalog
//! dependency); pgarbor/CLI map `SpaceKind::Relation(oid)` to names with the catalog.
//!
//! # Algorithm (cost proportional to the DIVERGENCE, not to the database)
//!
//! Both trees are copy-on-write descendants of common history, so unchanged subtrees are the very
//! same `PageId`s. `diff_trees` walks both trees in key order with two cursors, each a stack of
//! decoded nodes (`pgarbor_storage::btree::node::decode_node(&src.read_page(id)?)`):
//! 1. If both roots are equal → no differences (O(1); this is the common "fresh branch" case).
//! 2. At any point where both cursors are positioned at the START of a subtree with the same
//!    PageId (same child pointer next to be descended, at the same level), skip that subtree in
//!    both cursors without reading it. Subtree identity implies identical key range and content.
//! 3. Otherwise descend the cursor whose next pending subtree has the smaller lower bound (or
//!    both when equal), down to leaves, and merge leaf entries by key: key only in A →
//!    `only_in_a`, only in B → `only_in_b`, in both with different values → `changed`. Values
//!    are compared without materializing overflow chains when both are `LeafValue::Overflow` with
//!    the same first PageId (equal) — otherwise materialize with `btree::read::materialize` and
//!    compare bytes.
//! 4. Stop counting after `opts.max_differences` differences (set `truncated = true`).
//!
//! A correct simpler fallback the implementer must keep as a test oracle: merge two full
//! `RangeIter` scans. Unit tests must check `diff_trees` == fallback on randomized trees built with
//! a `WriteTxn` (insert/delete random keys on a forked root, then compare).
use crate::directory;
use crate::options::{BranchRef, CompareOptions};
use pgarbor_common::{BranchId, CommitSeq, PageId, PgError, Result};
use pgarbor_storage::btree::node::{decode_node, InternalNode, LeafNode, LeafValue, LeafValueRef, Node};
use pgarbor_storage::btree::read::materialize;
use pgarbor_storage::gc::trace::trace;
use pgarbor_storage::{NamedRoot, PageSource, Snapshot};
use std::cmp::Ordering;
use std::collections::BTreeMap;
use std::fmt::Write as _;

/// Deepest tree a cursor follows before reporting a cycle (matches storage's walkers).
const MAX_DEPTH: usize = 64;

// CONTRACT
/// Keyspace classification of the 4-byte big-endian prefix (ARCHITECTURE §4).
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, serde::Serialize)]
#[serde(rename_all = "snake_case", tag = "kind", content = "oid")]
pub enum SpaceKind {
    /// Prefix 1: catalog rows (schema divergence).
    Catalog,
    /// Prefix 2: sequence state.
    Sequences,
    /// Prefix 3: branch-local metadata.
    BranchMeta,
    /// Prefix >= 16384: data of relation `oid` (table rows or index entries).
    Relation(u32),
    /// Any other prefix, or a key shorter than 4 bytes (reported, never an error).
    Other(u32),
}

impl SpaceKind {
    /// Classify a key by its first 4 bytes (big-endian). Keys shorter than 4 bytes → `Other(0)`.
    pub fn of_key(key: &[u8]) -> SpaceKind {
        let Some(p) = key.get(0..4) else { return SpaceKind::Other(0) };
        let id = u32::from_be_bytes([p[0], p[1], p[2], p[3]]);
        match id {
            1 => SpaceKind::Catalog,
            2 => SpaceKind::Sequences,
            3 => SpaceKind::BranchMeta,
            n if n >= pgarbor_common::FIRST_NORMAL_OID => SpaceKind::Relation(n),
            n => SpaceKind::Other(n),
        }
    }
}

// CONTRACT
/// One differing key, for human-readable samples (`key_hex` = lowercase hex of the full key).
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize)]
pub struct KeyDiffSample {
    pub key_hex: String,
    /// "only_in_a" | "only_in_b" | "changed".
    pub change: String,
}

// CONTRACT
/// Differences within one keyspace.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize)]
pub struct SpaceDiff {
    pub space: SpaceKind,
    pub only_in_a: u64,
    pub only_in_b: u64,
    pub changed: u64,
    pub samples: Vec<KeyDiffSample>,
}

// CONTRACT
/// Identity of one compared side.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize)]
pub struct CompareSide {
    pub id: BranchId,
    pub name: String,
    pub root: PageId,
    pub head_seq: CommitSeq,
}

// CONTRACT
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize)]
pub struct CompareReport {
    pub a: CompareSide,
    pub b: CompareSide,
    /// Same root page (or both empty): definitely identical, nothing was read.
    pub same_root: bool,
    /// No key differences found (implies identical logical content).
    pub identical: bool,
    /// Sorted by `space` ascending; spaces without differences are omitted.
    pub spaces: Vec<SpaceDiff>,
    pub truncated: bool,
    /// Whether one branch's `parent` is the other (direct lineage), for display.
    pub a_is_parent_of_b: bool,
    pub b_is_parent_of_a: bool,
    /// Page accounting (only when `opts.include_pages`): pages reachable from both, only A, only B.
    pub shared_pages: Option<u64>,
    pub exclusive_pages_a: Option<u64>,
    pub exclusive_pages_b: Option<u64>,
}

/// Compare two ACTIVE branches as of `snap` (resolve both via `directory::resolve` on
/// `snap.directory()`), comparing their `data_root` trees ("postgres" database; extra databases
/// are compared too once they exist — in v1 assert both have none, else 0A000 via
/// `PgError::unimplemented("BR-06", "comparing branches with several databases")`).
/// Page accounting: `pgarbor_storage::gc::trace(snap, snap.page_count(), &[a, b], true)` →
/// `exclusive_pages` per root and shared = reachable - exclusive_a - exclusive_b.
pub fn compare(snap: &Snapshot, a: &BranchRef, b: &BranchRef, opts: &CompareOptions) -> Result<CompareReport> {
    let dir = snap.directory();
    let ra = directory::resolve(&dir, a)?;
    let rb = directory::resolve(&dir, b)?;
    if !ra.extra_databases.is_empty() || !rb.extra_databases.is_empty() {
        return Err(PgError::unimplemented("BR-06", "comparing branches with several databases"));
    }

    let same_root = ra.data_root == rb.data_root;
    let (spaces, truncated) = if same_root {
        (Vec::new(), false)
    } else {
        diff_trees(snap, ra.data_root, rb.data_root, opts)?
    };

    let (mut shared_pages, mut exclusive_pages_a, mut exclusive_pages_b) = (None, None, None);
    if opts.include_pages {
        let roots = [
            NamedRoot { label: format!("branch {}", ra.name), root: ra.data_root },
            NamedRoot { label: format!("branch {}", rb.name), root: rb.data_root },
        ];
        let t = trace(snap, snap.page_count(), &roots, true)?;
        if t.roots.len() != 2 {
            return Err(PgError::internal("page trace returned an unexpected number of roots"));
        }
        let ex_a = t.roots[0].exclusive_pages;
        let ex_b = t.roots[1].exclusive_pages;
        shared_pages = Some(t.reachable.len().saturating_sub(ex_a).saturating_sub(ex_b));
        exclusive_pages_a = Some(ex_a);
        exclusive_pages_b = Some(ex_b);
    }

    Ok(CompareReport {
        a: CompareSide { id: ra.id, name: ra.name.clone(), root: ra.data_root, head_seq: ra.head_seq },
        b: CompareSide { id: rb.id, name: rb.name.clone(), root: rb.data_root, head_seq: rb.head_seq },
        same_root,
        identical: spaces.is_empty() && !truncated,
        spaces,
        truncated,
        a_is_parent_of_b: rb.parent == Some(ra.id),
        b_is_parent_of_a: ra.parent == Some(rb.id),
        shared_pages,
        exclusive_pages_a,
        exclusive_pages_b,
    })
}

/// One decoded node on a cursor stack.
enum Frame {
    /// `lb` is the lower bound inherited by child 0 (empty = unbounded below).
    Internal { node: InternalNode, lb: Vec<u8>, next: usize },
    Leaf { node: LeafNode, next: usize },
}

/// What a settled cursor is positioned at.
enum Peek<'c> {
    End,
    /// Next pending subtree: page, lower bound of its key range, level (leaf = 0).
    Subtree { id: PageId, lb: &'c [u8], level: u16 },
    Entry { key: &'c [u8], value: &'c LeafValue },
}

/// In-order cursor over one tree, able to hand out whole unread subtrees.
struct Cursor<'a> {
    snap: &'a Snapshot,
    stack: Vec<Frame>,
}

impl<'a> Cursor<'a> {
    fn new(snap: &'a Snapshot, root: PageId) -> Result<Cursor<'a>> {
        let mut c = Cursor { snap, stack: Vec::new() };
        if !root.is_none() {
            c.push_node(root, Vec::new())?;
        }
        Ok(c)
    }

    fn push_node(&mut self, id: PageId, lb: Vec<u8>) -> Result<()> {
        if self.stack.len() > MAX_DEPTH {
            return Err(PgError::corruption("b-tree too deep, possible cycle"));
        }
        let page = self.snap.read_page(id)?;
        match decode_node(&page)? {
            Node::Leaf(node) => self.stack.push(Frame::Leaf { node, next: 0 }),
            Node::Internal(node) => self.stack.push(Frame::Internal { node, lb, next: 0 }),
        }
        Ok(())
    }

    /// Pop exhausted frames so the top (if any) has a pending item.
    fn settle(&mut self) {
        loop {
            let done = match self.stack.last() {
                None => break,
                Some(Frame::Internal { node, next, .. }) => *next >= node.num_children(),
                Some(Frame::Leaf { node, next }) => *next >= node.entries.len(),
            };
            if done {
                self.stack.pop();
            } else {
                break;
            }
        }
    }

    /// Requires a settled cursor.
    fn peek(&self) -> Peek<'_> {
        match self.stack.last() {
            None => Peek::End,
            Some(Frame::Internal { node, lb, next }) => Peek::Subtree {
                id: node.child_at(*next),
                lb: if *next == 0 { lb.as_slice() } else { node.entries[*next - 1].key.as_slice() },
                level: node.level.saturating_sub(1),
            },
            Some(Frame::Leaf { node, next }) => {
                let e = &node.entries[*next];
                Peek::Entry { key: e.key.as_slice(), value: &e.value }
            }
        }
    }

    /// Step past the pending item (a leaf entry, or a whole subtree without reading it).
    fn advance(&mut self) {
        match self.stack.last_mut() {
            Some(Frame::Internal { next, .. }) | Some(Frame::Leaf { next, .. }) => *next += 1,
            None => {}
        }
    }

    /// Read the pending subtree and make it the new top of stack.
    fn descend(&mut self) -> Result<()> {
        let (id, lb) = match self.stack.last_mut() {
            Some(Frame::Internal { node, lb, next }) => {
                let id = node.child_at(*next);
                let l = if *next == 0 { lb.clone() } else { node.entries[*next - 1].key.clone() };
                *next += 1;
                (id, l)
            }
            _ => return Err(PgError::internal("tree cursor cannot descend from a leaf")),
        };
        self.push_node(id, lb)
    }
}

#[derive(Clone, Copy)]
enum Change {
    OnlyA,
    OnlyB,
    Changed,
}

/// Difference accumulator with the `max_differences` / sampling limits.
struct Acc {
    spaces: BTreeMap<SpaceKind, SpaceDiff>,
    total: u64,
    max: u64,
    samples: usize,
    truncated: bool,
}

impl Acc {
    /// Count one difference; `false` means the limit was hit and the walk must stop.
    fn record(&mut self, key: &[u8], change: Change) -> bool {
        if self.total >= self.max {
            self.truncated = true;
            return false;
        }
        self.total += 1;
        let space = SpaceKind::of_key(key);
        let d = self.spaces.entry(space).or_insert_with(|| SpaceDiff {
            space,
            only_in_a: 0,
            only_in_b: 0,
            changed: 0,
            samples: Vec::new(),
        });
        let name = match change {
            Change::OnlyA => {
                d.only_in_a += 1;
                "only_in_a"
            }
            Change::OnlyB => {
                d.only_in_b += 1;
                "only_in_b"
            }
            Change::Changed => {
                d.changed += 1;
                "changed"
            }
        };
        if d.samples.len() < self.samples {
            let mut key_hex = String::with_capacity(key.len() * 2);
            for byte in key {
                let _ = write!(key_hex, "{byte:02x}");
            }
            d.samples.push(KeyDiffSample { key_hex, change: name.to_string() });
        }
        true
    }
}

fn value_ref(v: &LeafValue) -> LeafValueRef<'_> {
    match v {
        LeafValue::Inline(b) => LeafValueRef::Inline(b.as_slice()),
        LeafValue::Overflow { first, len } => LeafValueRef::Overflow { first: *first, len: *len },
    }
}

/// Logical value equality; shared overflow chains compare equal without being read.
fn values_equal(snap: &Snapshot, a: &LeafValue, b: &LeafValue) -> Result<bool> {
    match (a, b) {
        (LeafValue::Inline(x), LeafValue::Inline(y)) => Ok(x == y),
        (LeafValue::Overflow { first: f1, len: l1 }, LeafValue::Overflow { first: f2, len: l2 })
            if f1 == f2 && l1 == l2 =>
        {
            Ok(true)
        }
        _ => {
            if a.value_len() != b.value_len() {
                return Ok(false);
            }
            let x = materialize(snap, value_ref(a))?;
            let y = materialize(snap, value_ref(b))?;
            Ok(x == y)
        }
    }
}

#[derive(Clone, Copy)]
enum Act {
    Done,
    SkipBoth,
    DescA,
    DescB,
    DescBoth,
    EmitA,
    EmitB,
    Same,
    Differ,
}

/// Structural diff of two trees of the same snapshot (algorithm in the module docs). Returns the
/// per-space diffs (sorted, empty spaces omitted) and the `truncated` flag.
pub fn diff_trees(snap: &Snapshot, a: PageId, b: PageId, opts: &CompareOptions) -> Result<(Vec<SpaceDiff>, bool)> {
    if a == b {
        return Ok((Vec::new(), false));
    }
    let mut ca = Cursor::new(snap, a)?;
    let mut cb = Cursor::new(snap, b)?;
    let mut acc = Acc {
        spaces: BTreeMap::new(),
        total: 0,
        max: opts.max_differences,
        samples: opts.samples_per_space,
        truncated: false,
    };
    loop {
        ca.settle();
        cb.settle();
        let act = match (ca.peek(), cb.peek()) {
            (Peek::End, Peek::End) => Act::Done,
            (Peek::Subtree { id: ia, lb: la, level: lva }, Peek::Subtree { id: ib, lb: lbb, level: lvb }) => {
                if ia == ib {
                    Act::SkipBoth
                } else {
                    match la.cmp(lbb) {
                        Ordering::Less => Act::DescA,
                        Ordering::Greater => Act::DescB,
                        Ordering::Equal => match lva.cmp(&lvb) {
                            Ordering::Greater => Act::DescA,
                            Ordering::Less => Act::DescB,
                            Ordering::Equal => Act::DescBoth,
                        },
                    }
                }
            }
            (Peek::Subtree { .. }, Peek::End) => Act::DescA,
            (Peek::End, Peek::Subtree { .. }) => Act::DescB,
            (Peek::Subtree { lb, .. }, Peek::Entry { key, .. }) => {
                if lb <= key {
                    Act::DescA
                } else {
                    Act::EmitB
                }
            }
            (Peek::Entry { key, .. }, Peek::Subtree { lb, .. }) => {
                if lb <= key {
                    Act::DescB
                } else {
                    Act::EmitA
                }
            }
            (Peek::Entry { .. }, Peek::End) => Act::EmitA,
            (Peek::End, Peek::Entry { .. }) => Act::EmitB,
            (Peek::Entry { key: ka, value: va }, Peek::Entry { key: kb, value: vb }) => match ka.cmp(kb) {
                Ordering::Less => Act::EmitA,
                Ordering::Greater => Act::EmitB,
                Ordering::Equal => {
                    if values_equal(snap, va, vb)? {
                        Act::Same
                    } else {
                        Act::Differ
                    }
                }
            },
        };
        match act {
            Act::Done => break,
            Act::SkipBoth => {
                ca.advance();
                cb.advance();
            }
            Act::DescA => ca.descend()?,
            Act::DescB => cb.descend()?,
            Act::DescBoth => {
                ca.descend()?;
                cb.descend()?;
            }
            Act::EmitA => {
                let go = match ca.peek() {
                    Peek::Entry { key, .. } => acc.record(key, Change::OnlyA),
                    _ => true,
                };
                if !go {
                    break;
                }
                ca.advance();
            }
            Act::EmitB => {
                let go = match cb.peek() {
                    Peek::Entry { key, .. } => acc.record(key, Change::OnlyB),
                    _ => true,
                };
                if !go {
                    break;
                }
                cb.advance();
            }
            Act::Same => {
                ca.advance();
                cb.advance();
            }
            Act::Differ => {
                let go = match ca.peek() {
                    Peek::Entry { key, .. } => acc.record(key, Change::Changed),
                    _ => true,
                };
                if !go {
                    break;
                }
                ca.advance();
                cb.advance();
            }
        }
    }
    Ok((acc.spaces.into_values().collect(), acc.truncated))
}

#[cfg(test)]
mod tests {
    use super::*;
    use pgarbor_storage::{Container, CreateOptions, MemVfs, SyncMode};
    use std::ops::Bound;
    use std::path::Path;
    use std::sync::Arc;

    fn container() -> Container {
        let opts = CreateOptions { sync: SyncMode::Off, page_size: 4096, ..Default::default() };
        Container::create_with(Arc::new(MemVfs::new()), Path::new("t.pga"), &opts).unwrap()
    }

    fn key(space: u32, n: u32) -> Vec<u8> {
        let mut k = space.to_be_bytes().to_vec();
        k.extend_from_slice(&n.to_be_bytes());
        k
    }

    fn val(n: u32, tag: u8) -> Vec<u8> {
        let mut v = n.to_be_bytes().to_vec();
        v.push(tag);
        v.extend_from_slice(b"-payload-payload");
        v
    }

    struct Rng(u64);
    impl Rng {
        fn next(&mut self) -> u64 {
            self.0 ^= self.0 << 13;
            self.0 ^= self.0 >> 7;
            self.0 ^= self.0 << 17;
            self.0
        }
        fn below(&mut self, n: u64) -> u64 {
            self.next() % n
        }
    }

    fn all(snap: &Snapshot, root: PageId) -> Vec<(Vec<u8>, Vec<u8>)> {
        let t = snap.tree(root).unwrap();
        t.range(Bound::Unbounded, Bound::Unbounded).unwrap().map(|r| r.unwrap()).collect()
    }

    /// Naive oracle: merge two full scans.
    fn naive(snap: &Snapshot, a: PageId, b: PageId) -> BTreeMap<SpaceKind, (u64, u64, u64)> {
        let (xa, xb) = (all(snap, a), all(snap, b));
        let mut out: BTreeMap<SpaceKind, (u64, u64, u64)> = BTreeMap::new();
        let (mut i, mut j) = (0, 0);
        while i < xa.len() || j < xb.len() {
            let ord = if i >= xa.len() {
                Ordering::Greater
            } else if j >= xb.len() {
                Ordering::Less
            } else {
                xa[i].0.cmp(&xb[j].0)
            };
            match ord {
                Ordering::Less => {
                    out.entry(SpaceKind::of_key(&xa[i].0)).or_default().0 += 1;
                    i += 1;
                }
                Ordering::Greater => {
                    out.entry(SpaceKind::of_key(&xb[j].0)).or_default().1 += 1;
                    j += 1;
                }
                Ordering::Equal => {
                    if xa[i].1 != xb[j].1 {
                        out.entry(SpaceKind::of_key(&xa[i].0)).or_default().2 += 1;
                    }
                    i += 1;
                    j += 1;
                }
            }
        }
        out
    }

    fn as_counts(spaces: &[SpaceDiff]) -> BTreeMap<SpaceKind, (u64, u64, u64)> {
        spaces.iter().map(|d| (d.space, (d.only_in_a, d.only_in_b, d.changed))).collect()
    }

    fn build(c: &Container, keys: &[(Vec<u8>, Vec<u8>)]) -> PageId {
        let mut txn = c.begin_write().unwrap();
        let mut root = PageId::NONE;
        for (k, v) in keys {
            root = txn.put(root, k, v).unwrap().root;
        }
        txn.commit().unwrap();
        root
    }

    fn base_keys() -> Vec<(Vec<u8>, Vec<u8>)> {
        let mut v = Vec::new();
        for n in 0..500 {
            v.push((key(1, n), val(n, 0)));
        }
        for n in 0..2500 {
            v.push((key(16384, n), val(n, 0)));
        }
        for n in 0..2000 {
            v.push((key(16385, n), val(n, 0)));
        }
        v
    }

    #[test]
    fn space_classification() {
        assert_eq!(SpaceKind::of_key(&key(1, 7)), SpaceKind::Catalog);
        assert_eq!(SpaceKind::of_key(&key(2, 7)), SpaceKind::Sequences);
        assert_eq!(SpaceKind::of_key(&key(3, 7)), SpaceKind::BranchMeta);
        assert_eq!(SpaceKind::of_key(&key(16384, 7)), SpaceKind::Relation(16384));
        assert_eq!(SpaceKind::of_key(&key(9, 7)), SpaceKind::Other(9));
        assert_eq!(SpaceKind::of_key(b"ab"), SpaceKind::Other(0));
    }

    #[test]
    fn fork_with_known_edits() {
        let c = container();
        let base = build(&c, &base_keys());
        let mut txn = c.begin_write().unwrap();
        let mut f = base;
        for n in [10u32, 1000, 2000] {
            f = txn.put(f, &key(16384, n), &val(n, 1)).unwrap().root;
        }
        f = txn.put(f, &key(16385, 5000), b"new1").unwrap().root;
        f = txn.put(f, &key(1, 9999), b"new2").unwrap().root;
        f = txn.delete(f, &key(16385, 3)).unwrap().root;
        txn.commit().unwrap();
        let snap = c.snapshot();
        let (spaces, truncated) = diff_trees(&snap, base, f, &CompareOptions::default()).unwrap();
        assert!(!truncated);
        assert_eq!(spaces.len(), 3);
        assert_eq!(spaces[0].space, SpaceKind::Catalog);
        assert_eq!((spaces[0].only_in_a, spaces[0].only_in_b, spaces[0].changed), (0, 1, 0));
        assert_eq!(spaces[1].space, SpaceKind::Relation(16384));
        assert_eq!((spaces[1].only_in_a, spaces[1].only_in_b, spaces[1].changed), (0, 0, 3));
        assert_eq!(spaces[1].samples.len(), 3);
        assert_eq!(spaces[1].samples[0].change, "changed");
        assert_eq!(spaces[1].samples[0].key_hex, "000040000000000a");
        assert_eq!(spaces[2].space, SpaceKind::Relation(16385));
        assert_eq!((spaces[2].only_in_a, spaces[2].only_in_b, spaces[2].changed), (1, 1, 0));
        assert_eq!(as_counts(&spaces), naive(&snap, base, f));
    }

    #[test]
    fn identical_and_empty() {
        let c = container();
        let base = build(&c, &base_keys());
        let snap = c.snapshot();
        let (s, t) = diff_trees(&snap, base, base, &CompareOptions::default()).unwrap();
        assert!(s.is_empty() && !t);
        let (s, t) = diff_trees(&snap, PageId::NONE, PageId::NONE, &CompareOptions::default()).unwrap();
        assert!(s.is_empty() && !t);
        let (s, _) = diff_trees(&snap, PageId::NONE, base, &CompareOptions::default()).unwrap();
        assert_eq!(as_counts(&s), naive(&snap, PageId::NONE, base));
        assert_eq!(s.iter().map(|d| d.only_in_b).sum::<u64>(), 5000);
        let (s, _) = diff_trees(&snap, base, PageId::NONE, &CompareOptions::default()).unwrap();
        assert_eq!(s.iter().map(|d| d.only_in_a).sum::<u64>(), 5000);
    }

    #[test]
    fn truncation_limits_counts_and_samples() {
        let c = container();
        let base = build(&c, &base_keys());
        let snap = c.snapshot();
        let opts = CompareOptions { max_differences: 10, samples_per_space: 2, include_pages: false };
        let (s, t) = diff_trees(&snap, PageId::NONE, base, &opts).unwrap();
        assert!(t);
        assert_eq!(s.iter().map(|d| d.only_in_b).sum::<u64>(), 10);
        assert!(s.iter().all(|d| d.samples.len() <= 2));
        // Exactly max differences is not truncated.
        let opts = CompareOptions { max_differences: 5000, samples_per_space: 0, include_pages: false };
        let (s, t) = diff_trees(&snap, PageId::NONE, base, &opts).unwrap();
        assert!(!t);
        assert_eq!(s.iter().map(|d| d.only_in_b).sum::<u64>(), 5000);
    }

    #[test]
    fn overflow_values() {
        let c = container();
        let big1 = vec![7u8; 20_000];
        let mut big2 = big1.clone();
        big2[19_999] = 8;
        let mut items: Vec<(Vec<u8>, Vec<u8>)> = (0..200).map(|n| (key(16384, n), val(n, 0))).collect();
        items.push((key(16384, 1000), big1.clone()));
        items.push((key(16384, 1001), big1.clone()));
        let base = build(&c, &items);
        let mut txn = c.begin_write().unwrap();
        let mut f = txn.put(base, &key(16384, 1001), &big2).unwrap().root;
        f = txn.put(f, &key(16384, 5), b"x").unwrap().root;
        txn.commit().unwrap();
        let snap = c.snapshot();
        let (s, _) = diff_trees(&snap, base, f, &CompareOptions::default()).unwrap();
        assert_eq!(as_counts(&s), naive(&snap, base, f));
        assert_eq!(s[0].changed, 2);
    }

    #[test]
    fn randomized_matches_naive_scan() {
        let mut rng = Rng(0x9e3779b97f4a7c15);
        for case in 0..20u32 {
            let c = container();
            let n = 50 + rng.below(3000) as u32;
            let spaces = [1u32, 2, 16384, 16385, 20000];
            let mut items = Vec::new();
            for i in 0..n {
                let sp = spaces[rng.below(spaces.len() as u64) as usize];
                items.push((key(sp, rng.below(4000) as u32), val(i, 0)));
            }
            let base = build(&c, &items);
            let mut txn = c.begin_write().unwrap();
            let mut f = base;
            let edits = rng.below(200);
            for e in 0..edits {
                let sp = spaces[rng.below(spaces.len() as u64) as usize];
                let k = key(sp, rng.below(4500) as u32);
                match rng.below(3) {
                    0 => f = txn.delete(f, &k).unwrap().root,
                    _ => f = txn.put(f, &k, &val(e as u32, 9)).unwrap().root,
                }
            }
            txn.commit().unwrap();
            let snap = c.snapshot();
            let (got, t) = diff_trees(&snap, base, f, &CompareOptions::default()).unwrap();
            assert!(!t);
            assert_eq!(as_counts(&got), naive(&snap, base, f), "case {case}");
            let (back, _) = diff_trees(&snap, f, base, &CompareOptions::default()).unwrap();
            let flipped: BTreeMap<_, _> = as_counts(&back).into_iter().map(|(k, (a, b, c))| (k, (b, a, c))).collect();
            assert_eq!(as_counts(&got), flipped, "case {case} (reversed)");
        }
    }
}
