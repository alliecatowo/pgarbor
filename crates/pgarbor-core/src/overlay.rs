//! Transaction-private write overlay with an undo log for savepoints (TXN-03, TXN-06).
//! Lease CORE-03. Accessors are implemented.
//!
//! The overlay is the transaction's write set: `BTreeMap<key, Option<value>>` (None = tombstone),
//! exactly ARCHITECTURE §7's shape, applied to the branch head at commit via
//! `WriteBatch::from_map`. Reads see overlay-over-snapshot (`merge_scan`).
//!
//! Savepoints: while `logging` is on (the transaction has at least one savepoint), every
//! `put`/`delete` first appends an `UndoRecord` with the key's PREVIOUS overlay entry
//! (`None` = key was absent from the overlay). `mark()` = current undo length;
//! `rollback_to(mark)` pops records back to `mark`, restoring each previous entry, so the overlay
//! is exactly as it was at `mark()`. Cost: O(writes since the savepoint), never O(overlay size).
//! When the last savepoint is released/rolled back past, the transaction calls
//! `set_logging(false)` which also clears the undo log.
//!
//! `put_unlogged` writes WITHOUT an undo record: used for non-transactional state that happens to
//! live in the overlay (state of a sequence created in this same transaction: `nextval` must not
//! be undone by ROLLBACK TO SAVEPOINT, as in PostgreSQL).
use pgarbor_common::{KvIter, PgError, Result};
use pgarbor_storage::WriteBatch;
use std::collections::BTreeMap;
use std::ops::Bound;

// CONTRACT
/// Position in the undo log.
pub type OverlayMark = usize;

// CONTRACT
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct UndoRecord {
    pub key: Vec<u8>,
    /// Previous overlay entry: `None` = absent; `Some(None)` = tombstone; `Some(Some(v))` = value.
    pub prev: Option<Option<Vec<u8>>>,
}

// CONTRACT
#[derive(Clone, Debug, Default)]
pub struct Overlay {
    pub(crate) map: BTreeMap<Vec<u8>, Option<Vec<u8>>>,
    pub(crate) undo: Vec<UndoRecord>,
    pub(crate) logging: bool,
    /// Sum of key + value lengths currently in `map` (diagnostics / memory limits).
    pub(crate) bytes: usize,
}

impl Overlay {
    pub fn new() -> Overlay {
        Overlay::default()
    }
    pub fn len(&self) -> usize {
        self.map.len()
    }
    pub fn is_empty(&self) -> bool {
        self.map.is_empty()
    }
    pub fn bytes(&self) -> usize {
        self.bytes
    }
    pub fn is_logging(&self) -> bool {
        self.logging
    }
    /// `None` = not in overlay; `Some(None)` = deleted in overlay; `Some(Some(v))` = written.
    pub fn get(&self, key: &[u8]) -> Option<Option<&[u8]>> {
        self.map.get(key).map(|v| v.as_deref())
    }
    pub fn as_map(&self) -> &BTreeMap<Vec<u8>, Option<Vec<u8>>> {
        &self.map
    }
    /// Current undo position.
    pub fn mark(&self) -> OverlayMark {
        self.undo.len()
    }

    /// Insert/replace (`Some(value)` entry), logging the previous entry when `logging`; keep `bytes`.
    #[allow(unused_variables)]
    pub fn put(&mut self, key: &[u8], value: &[u8]) -> Result<()> {
        Err(PgError::unimplemented("CORE-03", "overlay put"))
    }

    /// Record a tombstone (`None` entry), logging as `put`. Always records the tombstone, even if
    /// the key is not in the snapshot (applying a delete of an absent key is harmless).
    #[allow(unused_variables)]
    pub fn delete(&mut self, key: &[u8]) -> Result<()> {
        Err(PgError::unimplemented("CORE-03", "overlay delete"))
    }

    /// Like `put` but never logged (see module docs).
    #[allow(unused_variables)]
    pub fn put_unlogged(&mut self, key: &[u8], value: &[u8]) -> Result<()> {
        Err(PgError::unimplemented("CORE-03", "overlay unlogged put"))
    }

    /// Undo back to `mark` (see module docs). `mark > self.mark()` → XX000 internal error.
    #[allow(unused_variables)]
    pub fn rollback_to(&mut self, mark: OverlayMark) -> Result<()> {
        Err(PgError::unimplemented("CORE-03", "overlay rollback to mark"))
    }

    /// Turn undo logging on/off; turning it off clears the undo log.
    #[allow(unused_variables)]
    pub fn set_logging(&mut self, on: bool) -> Result<()> {
        Err(PgError::unimplemented("CORE-03", "overlay undo logging switch"))
    }

    /// Entries within bounds, ascending (empty for an inverted range — never panic, unlike
    /// `BTreeMap::range`; reuse the guard logic of `WriteBatch::range`).
    #[allow(unused_variables)]
    pub fn range<'a>(
        &'a self,
        lower: Bound<&[u8]>,
        upper: Bound<&[u8]>,
    ) -> Result<Box<dyn Iterator<Item = (&'a [u8], Option<&'a [u8]>)> + 'a>> {
        Err(PgError::unimplemented("CORE-03", "overlay range"))
    }

    /// Copy into a storage batch for commit (keys/values cloned; tombstones become deletes).
    #[allow(unused_variables)]
    pub fn to_batch(&self) -> Result<WriteBatch> {
        Err(PgError::unimplemented("CORE-03", "overlay to write batch"))
    }

    /// Drop everything (rollback).
    pub fn clear(&mut self) {
        self.map.clear();
        self.undo.clear();
        self.bytes = 0;
    }
}

/// Merge an ascending snapshot iterator with the overlay entries in the same bounds:
/// - key only in base → yield base pair;
/// - key in overlay with `Some(v)` → yield `(key, v)` (overrides base);
/// - key in overlay with `None` → skip (deleted), whether or not base has it;
/// - output strictly ascending, no duplicates; the first base `Err` is yielded and then the
///   iterator is fused.
/// `lower`/`upper` are the SAME bounds the base iterator was opened with (copied into owned
/// `Bound<Vec<u8>>` before returning — implementations must not borrow the caller's bounds, see
/// `KvRead::scan_prefix`).
#[allow(unused_variables)]
pub fn merge_scan<'a>(
    base: KvIter<'a>,
    overlay: &'a Overlay,
    lower: Bound<&[u8]>,
    upper: Bound<&[u8]>,
) -> Result<KvIter<'a>> {
    Err(PgError::unimplemented("CORE-03", "overlay merge scan"))
}
