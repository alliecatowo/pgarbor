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
use crate::options::{BranchRef, CompareOptions};
use pgarbor_common::{BranchId, CommitSeq, PageId, PgError, Result};
use pgarbor_storage::Snapshot;

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
#[allow(unused_variables)]
pub fn compare(snap: &Snapshot, a: &BranchRef, b: &BranchRef, opts: &CompareOptions) -> Result<CompareReport> {
    Err(PgError::unimplemented("BR-06", "branch compare"))
}

/// Structural diff of two trees of the same snapshot (algorithm in the module docs). Returns the
/// per-space diffs (sorted, empty spaces omitted) and the `truncated` flag.
#[allow(unused_variables)]
pub fn diff_trees(snap: &Snapshot, a: PageId, b: PageId, opts: &CompareOptions) -> Result<(Vec<SpaceDiff>, bool)> {
    Err(PgError::unimplemented("BR-06", "structural tree diff"))
}
