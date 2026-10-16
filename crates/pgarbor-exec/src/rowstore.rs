//! Heap row storage over the KV layer (lease EXE-02, north-star).
//!
//! ## Key and value formats (ON-DISK, frozen v1; ARCHITECTURE §4)
//! * Row key: `[relid u32 BE][rowid u64 BE]` (12 bytes; `pgarbor_core::alloc::row_key`). Row ids are
//!   allocated by `ExecTxn::alloc_rowids` (start at 1, never reused, gaps allowed).
//! * Row value: `pgarbor_types::tuple::encode_row(values)` with ONE value per attribute slot of the
//!   table at write time (`td.ncols()` values, dropped slots `Null`). Rows written before an
//!   `ADD COLUMN` have fewer values: decoding completes them with [`TableDesc::complete_row`]
//!   (missing-value fast default, else NULL). Rows are never rewritten by ADD/DROP COLUMN.
//! * Keys of one table never collide with its indexes' keys (different leading `relid`).
//!
//! ## Scanning without holding iterators
//! [`HeapCursor`] reads at most [`SCAN_BATCH`] rows per refill with `txn::kv_scan_vec` starting after
//! the last returned key (`Bound::Excluded(last)`), so no `KvIter` is alive between calls and the
//! transaction view can be written (UPDATE/DELETE) while scanning. Rows inserted by this
//! transaction during its own scan MAY be seen if their rowid is greater than the cursor position:
//! DML therefore drains its input first (see `modify`).
//!
//! ## Locking
//! Plain reads take no locks. [`lock_row`] locks the row key (`ExecTxn::lock_and_read_latest`) and
//! reports what the latest committed version looks like; callers decide (module `modify`).
use std::collections::VecDeque;
use std::ops::Bound;

use pgarbor_common::{KvRead, KvWrite, Oid, PgError, Result};
use pgarbor_core::locks::LockMode;
use pgarbor_core::WaitPolicy;
use pgarbor_types::tuple;
use pgarbor_types::Datum;

use crate::runtime::Executor;
use crate::tabledesc::TableDesc;
use crate::txn::{kv_scan_vec, LatestRead};

/// Rows fetched per cursor refill.
pub const SCAN_BATCH: usize = 256;

/// `[relid][rowid]` key (12 bytes).
pub fn row_key(relid: Oid, rowid: u64) -> [u8; 12] {
    pgarbor_core::alloc::row_key(relid, rowid)
}

/// Rowid of a heap key; keys that are not exactly 12 bytes are XX001 `invalid heap key`.
pub fn rowid_of_key(key: &[u8]) -> Result<u64> {
    if key.len() != 12 {
        return Err(PgError::corruption(format!("invalid heap key of length {}", key.len())));
    }
    let mut b = [0u8; 8];
    b.copy_from_slice(&key[4..12]);
    Ok(u64::from_be_bytes(b))
}

/// Encode a complete table row (`td.ncols()` values; dropped slots must be `Null`). A row of the
/// wrong width is XX000 (executor bug), never silently padded.
pub fn encode_row(td: &TableDesc, row: &[Datum]) -> Result<Vec<u8>> {
    if row.len() != td.ncols() {
        return Err(PgError::internal(format!(
            "heap row of table {} has {} values for {} attribute slots",
            td.oid,
            row.len(),
            td.ncols()
        )));
    }
    for (i, c) in td.cols.iter().enumerate() {
        if c.dropped && !matches!(row[i], Datum::Null) {
            return Err(PgError::internal(format!(
                "dropped attribute slot {} of table {} holds a non-NULL value",
                c.attnum, td.oid
            )));
        }
    }
    tuple::encode_row(row)
}

/// Decode a stored row and complete it to `td.ncols()` values (module docs). `needed`: when
/// `Some`, slots whose flag is false are left `Null` (may use `tuple::decode_columns`).
///
/// Surplus stored values (beyond `ncols()`) are dropped, as [`TableDesc::complete_row`] does. Stored
/// rows shorter than `ncols()` get the column `missing` value for every slot they lack (only for
/// slots that are needed, when a mask is given). Dropped slots are always `Null`.
pub fn decode_row(td: &TableDesc, bytes: &[u8], needed: Option<&[bool]>) -> Result<Vec<Datum>> {
    let flags = match needed {
        None => {
            let stored = tuple::decode_row(bytes)?;
            return Ok(td.complete_row(stored));
        }
        Some(flags) => flags,
    };
    let ncols = td.ncols();
    if flags.len() != ncols {
        return Err(PgError::internal(format!(
            "needed-column mask of length {} for table {} with {} attribute slots",
            flags.len(),
            td.oid,
            ncols
        )));
    }
    let wanted: Vec<usize> = (0..ncols).filter(|&i| flags[i] && !td.cols[i].dropped).collect();
    // ncols_hint = 0: the result has exactly as many entries as the row stores, so `stored.len()`
    // tells which slots were present in the row (a stored NULL differs from a missing slot).
    let mut stored = tuple::decode_columns(bytes, &wanted, 0)?;
    let stored_n = stored.len();
    let mut out = Vec::with_capacity(ncols);
    for (i, c) in td.cols.iter().enumerate() {
        if c.dropped || !flags[i] {
            out.push(Datum::Null);
        } else if i < stored_n {
            out.push(std::mem::replace(&mut stored[i], Datum::Null));
        } else {
            out.push(c.missing.clone().unwrap_or(Datum::Null));
        }
    }
    Ok(out)
}

/// Insert a NEW row: allocate a rowid, `put_fresh` the key. Returns the rowid. Does NOT touch
/// indexes, constraints or foreign keys (`dml_insert` orchestrates those).
pub fn insert_heap(ex: &mut Executor, td: &TableDesc, row: &[Datum]) -> Result<u64> {
    // Encode first so that a malformed row burns no row id.
    let value = encode_row(td, row)?;
    let rowid = ex.txn().alloc_rowids(td.oid, 1)?.start;
    let key = row_key(td.oid, rowid);
    ex.txn().put_fresh(&key, &value)?;
    Ok(rowid)
}

/// Insert many rows with ONE `alloc_rowids(n)` call (COPY, INSERT..SELECT). Returns the rowids.
pub fn insert_heap_batch(ex: &mut Executor, td: &TableDesc, rows: &[Vec<Datum>]) -> Result<Vec<u64>> {
    if rows.is_empty() {
        return Ok(Vec::new());
    }
    // Encode everything first so that a malformed row burns no row ids.
    let mut values: Vec<Vec<u8>> = Vec::with_capacity(rows.len());
    for row in rows {
        values.push(encode_row(td, row)?);
    }
    let n = rows.len() as u64;
    let range = ex.txn().alloc_rowids(td.oid, n)?;
    if range.end.checked_sub(range.start) != Some(n) {
        return Err(PgError::internal(format!(
            "row id allocation for table {} returned {} ids instead of {}",
            td.oid,
            range.end.wrapping_sub(range.start),
            n
        )));
    }
    let mut ids: Vec<u64> = Vec::with_capacity(rows.len());
    for (i, value) in values.iter().enumerate() {
        if i > 0 && i % 1024 == 0 {
            ex.check_interrupts()?;
        }
        let rowid = range.start + i as u64;
        let key = row_key(td.oid, rowid);
        ex.txn().put_fresh(&key, value)?;
        ids.push(rowid);
    }
    Ok(ids)
}

/// Read one row through the transaction view (own writes visible). `None` if absent.
pub fn get_row(ex: &mut Executor, td: &TableDesc, rowid: u64) -> Result<Option<Vec<Datum>>> {
    let key = row_key(td.oid, rowid);
    match ex.txn().get(&key)? {
        Some(bytes) => Ok(Some(decode_row(td, &bytes, None)?)),
        None => Ok(None),
    }
}

/// Overwrite an existing row's value in place (UPDATE keeps the rowid). Plain `put` (takes the key
/// lock if not yet held).
pub fn write_row(ex: &mut Executor, td: &TableDesc, rowid: u64, row: &[Datum]) -> Result<()> {
    let value = encode_row(td, row)?;
    let key = row_key(td.oid, rowid);
    ex.txn().put(&key, &value)
}

/// Delete a row's heap key. Returns whether it existed in the transaction view.
pub fn delete_heap(ex: &mut Executor, relid: Oid, rowid: u64) -> Result<bool> {
    let key = row_key(relid, rowid);
    ex.txn().delete(&key)
}

/// Result of [`lock_row`].
// CONTRACT
#[derive(Clone, Debug, PartialEq)]
pub enum RowLock {
    /// The row no longer exists (deleted by a concurrent committed transaction, or by this one).
    Gone,
    /// The row is locked and unchanged since the statement snapshot; payload = its current image
    /// (own earlier modifications of this transaction included).
    Current(Vec<Datum>),
    /// READ COMMITTED only: a concurrent transaction committed a new version after the snapshot;
    /// payload = the latest committed image. Callers run EvalPlanQual (module `modify`).
    Changed(Vec<Datum>),
    /// `NoWait` / `SkipLocked` and the row is locked by someone else.
    NotAvailable,
}

/// Lock the row `[td.oid][rowid]` and classify it (`ExecTxn::lock_and_read_latest`), decoding the
/// returned bytes with [`decode_row`] (`needed = None`). Under REPEATABLE READ / SERIALIZABLE a
/// concurrent change surfaces as the 40001 error of `lock_and_read_latest`.
pub fn lock_row(ex: &mut Executor, td: &TableDesc, rowid: u64, mode: LockMode, policy: WaitPolicy) -> Result<RowLock> {
    let key = row_key(td.oid, rowid);
    match ex.txn().lock_and_read_latest(&key, mode, policy)? {
        LatestRead::NotAvailable => Ok(RowLock::NotAvailable),
        LatestRead::Locked { value: None, .. } => Ok(RowLock::Gone),
        LatestRead::Locked { value: Some(bytes), changed } => {
            let row = decode_row(td, &bytes, None)?;
            if changed {
                Ok(RowLock::Changed(row))
            } else {
                Ok(RowLock::Current(row))
            }
        }
    }
}

/// Delete EVERY key whose first four bytes equal `relid` (a table's heap rows or an index's
/// entries): DROP TABLE/INDEX data, TRUNCATE, aborted index builds. Scans in `SCAN_BATCH` chunks and
/// deletes through the transaction view. Returns the number of keys deleted.
pub fn delete_relation_data(ex: &mut Executor, relid: Oid) -> Result<u64> {
    let prefix = relid.to_be_bytes();
    let next = relid.checked_add(1).map(|r| r.to_be_bytes());
    let upper: Bound<&[u8]> = match &next {
        Some(b) => Bound::Excluded(&b[..]),
        None => Bound::Unbounded,
    };
    let mut last: Option<Vec<u8>> = None;
    let mut deleted: u64 = 0;
    loop {
        ex.check_interrupts()?;
        // Collect one chunk first: no KvIter is alive while the keys are deleted.
        let batch = {
            let lower: Bound<&[u8]> = match &last {
                Some(k) => Bound::Excluded(k.as_slice()),
                None => Bound::Included(&prefix[..]),
            };
            kv_scan_vec(&*ex.cx.txn, lower, upper, SCAN_BATCH)?
        };
        let n = batch.len();
        for (k, _) in batch {
            if ex.txn().delete(&k)? {
                deleted += 1;
            }
            last = Some(k);
        }
        if n < SCAN_BATCH {
            break;
        }
    }
    Ok(deleted)
}

/// Resumable sequential scan of one table (module docs). Rows come in rowid order.
// CONTRACT
#[derive(Debug)]
pub struct HeapCursor {
    relid: Oid,
    /// Last key returned (exclusive lower bound of the next refill); `None` = start of the table.
    last_key: Option<Vec<u8>>,
    /// Fetched but not yet returned `(rowid, value bytes)`.
    buf: VecDeque<(u64, Vec<u8>)>,
    /// The last refill returned fewer than `SCAN_BATCH` rows: no more data.
    exhausted: bool,
}

impl HeapCursor {
    pub fn new(relid: Oid) -> HeapCursor {
        HeapCursor { relid, last_key: None, buf: VecDeque::new(), exhausted: false }
    }

    /// Restart from the first row.
    pub fn reset(&mut self) {
        self.last_key = None;
        self.buf.clear();
        self.exhausted = false;
    }

    pub fn relid(&self) -> Oid {
        self.relid
    }

    /// Next `(rowid, completed row)`; `needed` as in [`decode_row`]. Calls
    /// `ex.check_interrupts()` once per refill.
    pub fn next(&mut self, ex: &mut Executor, td: &TableDesc, needed: Option<&[bool]>) -> Result<Option<(u64, Vec<Datum>)>> {
        if td.oid != self.relid {
            return Err(PgError::internal(format!(
                "heap cursor over relation {} used with table descriptor of relation {}",
                self.relid, td.oid
            )));
        }
        loop {
            if let Some((rowid, bytes)) = self.buf.pop_front() {
                let row = decode_row(td, &bytes, needed)?;
                return Ok(Some((rowid, row)));
            }
            if self.exhausted {
                return Ok(None);
            }
            ex.check_interrupts()?;
            let prefix = self.relid.to_be_bytes();
            let next = self.relid.checked_add(1).map(|r| r.to_be_bytes());
            let upper: Bound<&[u8]> = match &next {
                Some(b) => Bound::Excluded(&b[..]),
                None => Bound::Unbounded,
            };
            let batch = {
                let lower: Bound<&[u8]> = match &self.last_key {
                    Some(k) => Bound::Excluded(k.as_slice()),
                    None => Bound::Included(&prefix[..]),
                };
                kv_scan_vec(&*ex.cx.txn, lower, upper, SCAN_BATCH)?
            };
            let n = batch.len();
            for (k, v) in batch {
                let rowid = rowid_of_key(&k)?;
                self.buf.push_back((rowid, v));
                self.last_key = Some(k);
            }
            if n < SCAN_BATCH {
                self.exhausted = true;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::context::{ExecContext, NoGuc, NoServices, SessionEnv};
    use crate::memtxn::MemTxn;
    use crate::runtime::Runtime;
    use crate::tabledesc::ColDesc;
    use pgarbor_catalog::model::RelKind;
    use pgarbor_types::context::IoConfig;
    use pgarbor_types::TypeId;
    use std::cell::Cell;

    const RELID: Oid = 16400;

    fn col(attnum: i16, name: &str, dropped: bool, missing: Option<Datum>) -> ColDesc {
        ColDesc {
            attnum,
            name: name.to_string(),
            ty: TypeId::of(23),
            not_null: false,
            dropped,
            generated: None,
            identity: None,
            missing,
        }
    }

    fn table(cols: Vec<ColDesc>) -> TableDesc {
        TableDesc {
            oid: RELID,
            schema: "public".to_string(),
            name: "t".to_string(),
            kind: RelKind::Table,
            cols,
            indexes: Vec::new(),
            checks: Vec::new(),
            virtual_not_null: Vec::new(),
            fks_out: Vec::new(),
            fks_in: Vec::new(),
        }
    }

    /// Three live int4 columns.
    fn simple_table() -> TableDesc {
        table(vec![col(1, "a", false, None), col(2, "b", false, None), col(3, "c", false, None)])
    }

    /// Run `f` with an executor over `txn` (catalog env from the bootstrapped in-memory store).
    fn with_ex<T>(txn: &mut MemTxn, f: impl FnOnce(&mut Executor<'_, '_, '_>) -> T) -> T {
        let guc = NoGuc;
        let services = NoServices;
        let rng = Cell::new(1u64);
        let (_, env) = pgarbor_catalog::testing::bootstrapped().expect("bootstrap catalog");
        let se = SessionEnv::new(env, IoConfig::default(), &guc, &services, &rng, 0, 0);
        let mut cx = ExecContext::new(txn, se);
        let mut rt = Runtime::default();
        let mut ex = Executor::new(&mut rt, &mut cx);
        f(&mut ex)
    }

    fn row3(i: i32) -> Vec<Datum> {
        vec![Datum::Int4(i), Datum::text(format!("v{i}")), Datum::Null]
    }

    #[test]
    fn encode_decode_roundtrip() {
        let td = simple_table();
        let row = row3(7);
        let bytes = encode_row(&td, &row).unwrap();
        assert_eq!(decode_row(&td, &bytes, None).unwrap(), row);
        assert_eq!(decode_row(&td, &bytes, Some(&[true, true, true])).unwrap(), row);
        assert_eq!(decode_row(&td, &bytes, Some(&[false, true, false])).unwrap(), vec![Datum::Null, Datum::text("v7"), Datum::Null]);
    }

    #[test]
    fn encode_rejects_wrong_width_and_dropped_values() {
        let td = simple_table();
        assert!(encode_row(&td, &[Datum::Int4(1)]).is_err());
        let td2 = table(vec![col(1, "a", false, None), col(2, "b", true, None)]);
        assert!(encode_row(&td2, &[Datum::Int4(1), Datum::Int4(5)]).is_err());
        assert!(encode_row(&td2, &[Datum::Int4(1), Datum::Null]).is_ok());
    }

    #[test]
    fn short_row_is_completed_with_missing_value() {
        let td = table(vec![
            col(1, "a", false, None),
            col(2, "b", true, None),
            col(3, "c", false, Some(Datum::Int4(9))),
        ]);
        // Written before `c` existed: one stored value, and the dropped slot 2 is not present.
        let stored = tuple::encode_row(&[Datum::Int4(1)]).unwrap();
        let expect = vec![Datum::Int4(1), Datum::Null, Datum::Int4(9)];
        assert_eq!(decode_row(&td, &stored, None).unwrap(), expect);
        assert_eq!(decode_row(&td, &stored, Some(&[true, true, true])).unwrap(), expect);
        assert_eq!(decode_row(&td, &stored, Some(&[true, false, true])).unwrap(), vec![Datum::Int4(1), Datum::Null, Datum::Int4(9)]);
        assert_eq!(decode_row(&td, &stored, Some(&[false, false, false])).unwrap(), vec![Datum::Null, Datum::Null, Datum::Null]);
        // A stored NULL is not replaced by the missing value.
        let stored_null = tuple::encode_row(&[Datum::Int4(1), Datum::Null, Datum::Null]).unwrap();
        assert_eq!(decode_row(&td, &stored_null, None).unwrap(), vec![Datum::Int4(1), Datum::Null, Datum::Null]);
    }

    #[test]
    fn insert_get_and_write_rows() {
        let mut txn = MemTxn::new();
        let td = simple_table();
        with_ex(&mut txn, |ex| {
            assert_eq!(insert_heap(ex, &td, &row3(1)).unwrap(), 1);
            assert_eq!(insert_heap(ex, &td, &row3(2)).unwrap(), 2);
            assert_eq!(insert_heap(ex, &td, &row3(3)).unwrap(), 3);
            assert_eq!(get_row(ex, &td, 2).unwrap(), Some(row3(2)));
            assert_eq!(get_row(ex, &td, 99).unwrap(), None);
            write_row(ex, &td, 2, &row3(20)).unwrap();
            assert_eq!(get_row(ex, &td, 2).unwrap(), Some(row3(20)));
        });
    }

    #[test]
    fn cursor_scans_in_rowid_order_across_batches() {
        let mut txn = MemTxn::new();
        let td = simple_table();
        let rows: Vec<Vec<Datum>> = (0..600).map(row3).collect();
        let out = with_ex(&mut txn, |ex| {
            let ids = insert_heap_batch(ex, &td, &rows).unwrap();
            assert_eq!(ids, (1..=600u64).collect::<Vec<u64>>());
            let mut cur = HeapCursor::new(RELID);
            let mut out: Vec<(u64, Vec<Datum>)> = Vec::new();
            while let Some(item) = cur.next(ex, &td, None).unwrap() {
                out.push(item);
            }
            // Exhausted cursors keep returning None.
            assert!(cur.next(ex, &td, None).unwrap().is_none());
            out
        });
        assert_eq!(out.len(), 600);
        for (i, (rowid, row)) in out.iter().enumerate() {
            assert_eq!(*rowid, i as u64 + 1);
            assert_eq!(row, &row3(i as i32));
        }
    }

    #[test]
    fn cursor_reset_restarts_scan() {
        let mut txn = MemTxn::new();
        let td = simple_table();
        with_ex(&mut txn, |ex| {
            insert_heap(ex, &td, &row3(1)).unwrap();
            insert_heap(ex, &td, &row3(2)).unwrap();
            let mut cur = HeapCursor::new(RELID);
            assert_eq!(cur.next(ex, &td, None).unwrap().map(|r| r.0), Some(1));
            cur.reset();
            assert_eq!(cur.next(ex, &td, None).unwrap().map(|r| r.0), Some(1));
            assert_eq!(cur.next(ex, &td, None).unwrap().map(|r| r.0), Some(2));
            assert!(cur.next(ex, &td, None).unwrap().is_none());
        });
    }

    #[test]
    fn delete_relation_data_removes_only_that_prefix() {
        let mut txn = MemTxn::new();
        let val = tuple::encode_row(&[Datum::Int4(1)]).unwrap();
        for r in 1..=600u64 {
            txn.put(&row_key(RELID, r), &val).unwrap();
        }
        txn.put(&row_key(RELID - 1, 1), &val).unwrap();
        txn.put(&row_key(RELID + 1, 1), &val).unwrap();
        let deleted = with_ex(&mut txn, |ex| delete_relation_data(ex, RELID).unwrap());
        assert_eq!(deleted, 600);
        assert_eq!(txn.scan_prefix(&RELID.to_be_bytes()).unwrap().count(), 0);
        assert!(txn.get(&row_key(RELID - 1, 1)).unwrap().is_some());
        assert!(txn.get(&row_key(RELID + 1, 1)).unwrap().is_some());
    }

    #[test]
    fn lock_row_reports_current_then_gone() {
        let mut txn = MemTxn::new();
        let td = simple_table();
        with_ex(&mut txn, |ex| {
            let id = insert_heap(ex, &td, &row3(5)).unwrap();
            let before = lock_row(ex, &td, id, LockMode::ForUpdate, WaitPolicy::Block).unwrap();
            assert_eq!(before, RowLock::Current(row3(5)));
            assert!(delete_heap(ex, RELID, id).unwrap());
            assert!(!delete_heap(ex, RELID, id).unwrap());
            assert_eq!(lock_row(ex, &td, id, LockMode::ForUpdate, WaitPolicy::Block).unwrap(), RowLock::Gone);
        });
    }

    #[test]
    fn heap_cursor_rejects_foreign_descriptor() {
        let mut txn = MemTxn::new();
        let td = simple_table();
        let mut other = table(vec![col(1, "a", false, None)]);
        other.oid = RELID + 1;
        with_ex(&mut txn, |ex| {
            let mut cur = HeapCursor::new(RELID);
            assert!(cur.next(ex, &other, None).is_err());
            assert!(cur.next(ex, &td, None).unwrap().is_none());
        });
    }
}
