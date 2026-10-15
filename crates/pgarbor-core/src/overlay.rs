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
use std::cmp::Ordering;
use std::collections::BTreeMap;
use std::collections::btree_map;
use std::iter::Peekable;
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

    /// Replace the entry for `key` (`None` = tombstone) and keep `bytes` consistent.
    /// Returns the previous entry in `UndoRecord::prev` form.
    fn write_entry(&mut self, key: &[u8], value: Option<Vec<u8>>) -> Option<Option<Vec<u8>>> {
        let new_size = key.len() + value.as_ref().map_or(0, |v| v.len());
        if let Some(slot) = self.map.get_mut(key) {
            let old_size = key.len() + slot.as_ref().map_or(0, |v| v.len());
            self.bytes = (self.bytes - old_size) + new_size;
            return Some(std::mem::replace(slot, value));
        }
        self.bytes += new_size;
        self.map.insert(key.to_vec(), value);
        None
    }

    /// Remove the entry for `key` entirely (restoring "absent"), keeping `bytes` consistent.
    fn remove_entry(&mut self, key: &[u8]) {
        if let Some(old) = self.map.remove(key) {
            self.bytes -= key.len() + old.as_ref().map_or(0, |v| v.len());
        }
    }

    /// Insert/replace (`Some(value)` entry), logging the previous entry when `logging`; keep `bytes`.
    pub fn put(&mut self, key: &[u8], value: &[u8]) -> Result<()> {
        let prev = self.write_entry(key, Some(value.to_vec()));
        if self.logging {
            self.undo.push(UndoRecord { key: key.to_vec(), prev });
        }
        Ok(())
    }

    /// Record a tombstone (`None` entry), logging as `put`. Always records the tombstone, even if
    /// the key is not in the snapshot (applying a delete of an absent key is harmless).
    pub fn delete(&mut self, key: &[u8]) -> Result<()> {
        let prev = self.write_entry(key, None);
        if self.logging {
            self.undo.push(UndoRecord { key: key.to_vec(), prev });
        }
        Ok(())
    }

    /// Like `put` but never logged (see module docs).
    pub fn put_unlogged(&mut self, key: &[u8], value: &[u8]) -> Result<()> {
        self.write_entry(key, Some(value.to_vec()));
        Ok(())
    }

    /// Undo back to `mark` (see module docs). `mark > self.mark()` → XX000 internal error.
    pub fn rollback_to(&mut self, mark: OverlayMark) -> Result<()> {
        if mark > self.undo.len() {
            return Err(PgError::internal(format!(
                "overlay rollback mark {} is beyond the undo log length {}",
                mark,
                self.undo.len()
            )));
        }
        while self.undo.len() > mark {
            let Some(rec) = self.undo.pop() else { break };
            match rec.prev {
                None => self.remove_entry(&rec.key),
                Some(prev) => {
                    self.write_entry(&rec.key, prev);
                }
            }
        }
        Ok(())
    }

    /// Turn undo logging on/off; turning it off clears the undo log.
    pub fn set_logging(&mut self, on: bool) -> Result<()> {
        self.logging = on;
        if !on {
            self.undo.clear();
        }
        Ok(())
    }

    /// Entries within bounds, ascending (empty for an inverted range — never panic, unlike
    /// `BTreeMap::range`; reuse the guard logic of `WriteBatch::range`).
    pub fn range<'a>(
        &'a self,
        lower: Bound<&[u8]>,
        upper: Bound<&[u8]>,
    ) -> Result<Box<dyn Iterator<Item = (&'a [u8], Option<&'a [u8]>)> + 'a>> {
        let it: Box<dyn Iterator<Item = (&'a [u8], Option<&'a [u8]>)> + 'a> = match owned_bounds(lower, upper) {
            None => Box::new(std::iter::empty::<(&'a [u8], Option<&'a [u8]>)>()),
            Some((lo, hi)) => Box::new(
                self.map
                    .range::<Vec<u8>, _>((lo, hi))
                    .map(|(k, v)| (k.as_slice(), v.as_deref())),
            ),
        };
        Ok(it)
    }

    /// Copy into a storage batch for commit (keys/values cloned; tombstones become deletes).
    pub fn to_batch(&self) -> Result<WriteBatch> {
        Ok(WriteBatch::from_map(self.map.clone()))
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
pub fn merge_scan<'a>(
    base: KvIter<'a>,
    overlay: &'a Overlay,
    lower: Bound<&[u8]>,
    upper: Bound<&[u8]>,
) -> Result<KvIter<'a>> {
    // An inverted/empty range has no keys; the base scan over the same bounds yields nothing too.
    let it: KvIter<'a> = match owned_bounds(lower, upper) {
        None => Box::new(std::iter::empty::<Result<(Vec<u8>, Vec<u8>)>>()),
        Some((lo, hi)) => Box::new(MergeIter {
            base: base.peekable(),
            over: overlay.map.range::<Vec<u8>, _>((lo, hi)).peekable(),
            failed: false,
        }),
    };
    Ok(it)
}

/// Owned copy of the bounds, or `None` when the range is empty (inverted, or both ends excluded
/// at the same key). Mirrors `WriteBatch::range`, so `BTreeMap::range` is never asked to panic.
fn owned_bounds(lower: Bound<&[u8]>, upper: Bound<&[u8]>) -> Option<(Bound<Vec<u8>>, Bound<Vec<u8>>)> {
    let lower = lower.map(|b| b.to_vec());
    let upper = upper.map(|b| b.to_vec());
    if bounds_are_empty(&lower, &upper) { None } else { Some((lower, upper)) }
}

fn bounds_are_empty(lower: &Bound<Vec<u8>>, upper: &Bound<Vec<u8>>) -> bool {
    match (lower, upper) {
        (Bound::Included(a) | Bound::Excluded(a), Bound::Included(b) | Bound::Excluded(b)) => {
            a > b
                || (a == b
                    && (matches!(lower, Bound::Excluded(_)) || matches!(upper, Bound::Excluded(_))))
        }
        _ => false,
    }
}

/// Two-way ordered merge of the base scan and the overlay range (see `merge_scan`).
struct MergeIter<'a> {
    base: Peekable<KvIter<'a>>,
    over: Peekable<btree_map::Range<'a, Vec<u8>, Option<Vec<u8>>>>,
    failed: bool,
}

/// What the next step of the merge does, decided while the peeked heads are borrowed.
enum MergeStep {
    Done,
    BaseErr,
    Base,
    Over,
    Both,
}

impl<'a> Iterator for MergeIter<'a> {
    type Item = Result<(Vec<u8>, Vec<u8>)>;

    fn next(&mut self) -> Option<Self::Item> {
        if self.failed {
            return None;
        }
        loop {
            let step = match (self.base.peek(), self.over.peek()) {
                (None, None) => MergeStep::Done,
                (Some(Err(_)), _) => MergeStep::BaseErr,
                (Some(Ok(_)), None) => MergeStep::Base,
                (None, Some(_)) => MergeStep::Over,
                (Some(Ok((bk, _))), Some((ok, _))) => match bk.as_slice().cmp(ok.as_slice()) {
                    Ordering::Less => MergeStep::Base,
                    Ordering::Greater => MergeStep::Over,
                    Ordering::Equal => MergeStep::Both,
                },
            };
            match step {
                MergeStep::Done => return None,
                MergeStep::BaseErr => {
                    self.failed = true;
                    return self.base.next();
                }
                MergeStep::Base => return self.base.next(),
                MergeStep::Over => {
                    if let Some((k, Some(v))) = self.over.next() {
                        return Some(Ok((k.clone(), v.clone())));
                    }
                    // tombstone with no base counterpart: nothing to emit
                }
                MergeStep::Both => {
                    // overlay wins over the base entry with the same key
                    let _ = self.base.next();
                    if let Some((k, Some(v))) = self.over.next() {
                        return Some(Ok((k.clone(), v.clone())));
                    }
                    // tombstone: the key is deleted, skip both
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn snap(o: &Overlay) -> Vec<(Vec<u8>, Option<Vec<u8>>)> {
        o.as_map().iter().map(|(k, v)| (k.clone(), v.clone())).collect()
    }

    fn kv(k: &[u8], v: &[u8]) -> (Vec<u8>, Vec<u8>) {
        (k.to_vec(), v.to_vec())
    }

    fn base_of(pairs: &[(&str, &str)]) -> KvIter<'static> {
        let v: Vec<Result<(Vec<u8>, Vec<u8>)>> = pairs
            .iter()
            .map(|(k, v)| Ok((k.as_bytes().to_vec(), v.as_bytes().to_vec())))
            .collect();
        Box::new(v.into_iter())
    }

    fn collect_ok(it: KvIter<'_>) -> Vec<(String, String)> {
        it.map(|r| {
            let (k, v) = r.expect("no base error expected");
            (String::from_utf8(k).unwrap(), String::from_utf8(v).unwrap())
        })
        .collect()
    }

    fn s(pairs: &[(&str, &str)]) -> Vec<(String, String)> {
        pairs.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect()
    }

    #[test]
    fn put_delete_get() {
        let mut o = Overlay::new();
        assert_eq!(o.get(b"a"), None);
        o.put(b"a", b"1").unwrap();
        assert_eq!(o.get(b"a"), Some(Some(&b"1"[..])));
        o.delete(b"a").unwrap();
        assert_eq!(o.get(b"a"), Some(None));
        assert_eq!(o.len(), 1);
        assert_eq!(o.bytes(), 1); // tombstone counts its key only
        o.put(b"a", b"xx").unwrap();
        assert_eq!(o.bytes(), 3);
        o.put(b"b", b"zz").unwrap();
        assert_eq!(o.bytes(), 6);
        o.put(b"a", b"y").unwrap();
        assert_eq!(o.bytes(), 5);
        o.delete(b"b").unwrap();
        assert_eq!(o.bytes(), 3);
    }

    #[test]
    fn rollback_restores_absent_tombstone_and_value_across_nested_marks() {
        let mut o = Overlay::new();
        o.set_logging(true).unwrap();
        o.put(b"a", b"1").unwrap();
        let m0 = o.mark(); // 1
        o.put(b"a", b"2").unwrap();
        o.delete(b"b").unwrap();
        let m1 = o.mark(); // 3
        o.put(b"b", b"5").unwrap();
        o.put(b"c", b"6").unwrap();
        o.delete(b"a").unwrap();
        let m2 = o.mark(); // 6
        o.put(b"a", b"9").unwrap();
        o.put(b"b", b"7").unwrap();
        assert_eq!(o.mark(), 8);

        o.rollback_to(m2).unwrap();
        assert_eq!(
            snap(&o),
            vec![
                (b"a".to_vec(), None),
                (b"b".to_vec(), Some(b"5".to_vec())),
                (b"c".to_vec(), Some(b"6".to_vec())),
            ]
        );
        assert_eq!(o.mark(), m2);

        o.rollback_to(m1).unwrap();
        assert_eq!(snap(&o), vec![(b"a".to_vec(), Some(b"2".to_vec())), (b"b".to_vec(), None)]);
        assert_eq!(o.bytes(), 2 + 1);

        o.rollback_to(m0).unwrap();
        assert_eq!(snap(&o), vec![(b"a".to_vec(), Some(b"1".to_vec()))]);
        assert_eq!(o.bytes(), 2);

        o.rollback_to(0).unwrap();
        assert!(o.is_empty());
        assert_eq!(o.bytes(), 0);
        assert!(o.is_logging());
    }

    #[test]
    fn rollback_beyond_log_is_internal_error() {
        let mut o = Overlay::new();
        o.set_logging(true).unwrap();
        o.put(b"a", b"1").unwrap();
        let err = o.rollback_to(5).unwrap_err();
        assert_eq!(err.sqlstate.code(), "XX000");
        assert_eq!(snap(&o), vec![(b"a".to_vec(), Some(b"1".to_vec()))]);
    }

    #[test]
    fn put_unlogged_survives_rollback() {
        let mut o = Overlay::new();
        o.set_logging(true).unwrap();
        let m = o.mark();
        o.put_unlogged(b"seq", b"7").unwrap();
        o.put(b"x", b"1").unwrap();
        assert_eq!(o.mark(), m + 1);
        o.rollback_to(m).unwrap();
        assert_eq!(snap(&o), vec![(b"seq".to_vec(), Some(b"7".to_vec()))]);
    }

    #[test]
    fn set_logging_off_clears_undo() {
        let mut o = Overlay::new();
        o.set_logging(true).unwrap();
        o.put(b"a", b"1").unwrap();
        assert_eq!(o.mark(), 1);
        o.set_logging(false).unwrap();
        assert!(!o.is_logging());
        assert_eq!(o.mark(), 0);
        o.put(b"b", b"2").unwrap();
        assert_eq!(o.mark(), 0);
        assert_eq!(o.len(), 2);
    }

    #[test]
    fn range_bounds_and_inverted_guard() {
        let mut o = Overlay::new();
        o.put(b"a", b"1").unwrap();
        o.delete(b"b").unwrap();
        o.put(b"c", b"3").unwrap();
        o.put(b"e", b"5").unwrap();
        let got: Vec<(Vec<u8>, Option<Vec<u8>>)> = o
            .range(Bound::Included(b"b"), Bound::Excluded(b"e"))
            .unwrap()
            .map(|(k, v)| (k.to_vec(), v.map(|x| x.to_vec())))
            .collect();
        assert_eq!(got, vec![(b"b".to_vec(), None), (b"c".to_vec(), Some(b"3".to_vec()))]);
        assert_eq!(o.range(Bound::Unbounded, Bound::Unbounded).unwrap().count(), 4);
        assert_eq!(o.range(Bound::Included(b"z"), Bound::Included(b"a")).unwrap().count(), 0);
        assert_eq!(o.range(Bound::Excluded(b"c"), Bound::Excluded(b"c")).unwrap().count(), 0);
        assert_eq!(o.range(Bound::Included(b"c"), Bound::Excluded(b"c")).unwrap().count(), 0);
        assert_eq!(o.range(Bound::Included(b"c"), Bound::Included(b"c")).unwrap().count(), 1);
    }

    #[test]
    fn to_batch_copies_entries() {
        let mut o = Overlay::new();
        o.put(b"a", b"1").unwrap();
        o.delete(b"b").unwrap();
        let b = o.to_batch().unwrap();
        assert_eq!(b.len(), 2);
        assert_eq!(b.get(b"a"), Some(Some(&b"1"[..])));
        assert_eq!(b.get(b"b"), Some(None));
    }

    #[test]
    fn merge_unbounded_overlay_precedence_and_tombstones() {
        let mut o = Overlay::new();
        o.put(b"bb", b"x").unwrap();
        o.delete(b"b").unwrap();
        o.put(b"d", b"4").unwrap();
        let got = collect_ok(
            merge_scan(base_of(&[("a", "1"), ("b", "2"), ("c", "3")]), &o, Bound::Unbounded, Bound::Unbounded)
                .unwrap(),
        );
        assert_eq!(got, s(&[("a", "1"), ("bb", "x"), ("c", "3"), ("d", "4")]));
    }

    #[test]
    fn merge_overlay_value_overrides_base_same_key() {
        let mut o = Overlay::new();
        o.put(b"b", b"new").unwrap();
        let got = collect_ok(
            merge_scan(base_of(&[("a", "1"), ("b", "2")]), &o, Bound::Unbounded, Bound::Unbounded).unwrap(),
        );
        assert_eq!(got, s(&[("a", "1"), ("b", "new")]));
    }

    #[test]
    fn merge_bounded_range() {
        let mut o = Overlay::new();
        o.put(b"bb", b"x").unwrap();
        o.delete(b"b").unwrap();
        o.put(b"c", b"9").unwrap();
        o.put(b"d", b"4").unwrap();
        // base was opened with the same bounds [b, c)
        let got = collect_ok(
            merge_scan(base_of(&[("b", "2")]), &o, Bound::Included(b"b"), Bound::Excluded(b"c")).unwrap(),
        );
        assert_eq!(got, s(&[("bb", "x")]));

        let mut o2 = Overlay::new();
        o2.put(b"c", b"9").unwrap();
        let got = collect_ok(
            merge_scan(base_of(&[("c", "3")]), &o2, Bound::Excluded(b"b"), Bound::Included(b"c")).unwrap(),
        );
        assert_eq!(got, s(&[("c", "9")]));
    }

    #[test]
    fn merge_inverted_range_is_empty() {
        let mut o = Overlay::new();
        o.put(b"a", b"1").unwrap();
        let got = collect_ok(merge_scan(base_of(&[]), &o, Bound::Included(b"z"), Bound::Included(b"a")).unwrap());
        assert!(got.is_empty());
        let got = collect_ok(merge_scan(base_of(&[]), &o, Bound::Excluded(b"a"), Bound::Excluded(b"a")).unwrap());
        assert!(got.is_empty());
    }

    #[test]
    fn merge_base_error_propagates_once_then_fuses() {
        let mut o = Overlay::new();
        o.put(b"b", b"x").unwrap();
        let base: KvIter<'static> = Box::new(
            vec![Ok(kv(b"a", b"1")), Err(PgError::internal("boom")), Ok(kv(b"c", b"3"))].into_iter(),
        );
        let mut it = merge_scan(base, &o, Bound::Unbounded, Bound::Unbounded).unwrap();
        assert_eq!(it.next().unwrap().unwrap(), kv(b"a", b"1"));
        let err = it.next().unwrap().unwrap_err();
        assert_eq!(err.message, "boom");
        assert!(it.next().is_none());
        assert!(it.next().is_none());
    }
}
