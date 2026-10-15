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

use pgarbor_common::{Oid, PgError, Result};
use pgarbor_core::locks::LockMode;
use pgarbor_core::WaitPolicy;
use pgarbor_types::Datum;

use crate::runtime::Executor;
use crate::tabledesc::TableDesc;

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
#[allow(unused_variables)]
pub fn encode_row(td: &TableDesc, row: &[Datum]) -> Result<Vec<u8>> {
    Err(PgError::unimplemented("EXE-02", "heap row encoding (rowstore::encode_row)"))
}

/// Decode a stored row and complete it to `td.ncols()` values (module docs). `needed`: when
/// `Some`, slots whose flag is false are left `Null` (may use `tuple::decode_columns`).
#[allow(unused_variables)]
pub fn decode_row(td: &TableDesc, bytes: &[u8], needed: Option<&[bool]>) -> Result<Vec<Datum>> {
    Err(PgError::unimplemented("EXE-02", "heap row decoding (rowstore::decode_row)"))
}

/// Insert a NEW row: allocate a rowid, `put_fresh` the key. Returns the rowid. Does NOT touch
/// indexes, constraints or foreign keys (`dml_insert` orchestrates those).
#[allow(unused_variables)]
pub fn insert_heap(ex: &mut Executor, td: &TableDesc, row: &[Datum]) -> Result<u64> {
    Err(PgError::unimplemented("EXE-02", "heap insert (rowstore::insert_heap)"))
}

/// Insert many rows with ONE `alloc_rowids(n)` call (COPY, INSERT..SELECT). Returns the rowids.
#[allow(unused_variables)]
pub fn insert_heap_batch(ex: &mut Executor, td: &TableDesc, rows: &[Vec<Datum>]) -> Result<Vec<u64>> {
    Err(PgError::unimplemented("EXE-02", "heap batch insert (rowstore::insert_heap_batch)"))
}

/// Read one row through the transaction view (own writes visible). `None` if absent.
#[allow(unused_variables)]
pub fn get_row(ex: &mut Executor, td: &TableDesc, rowid: u64) -> Result<Option<Vec<Datum>>> {
    Err(PgError::unimplemented("EXE-02", "heap point read (rowstore::get_row)"))
}

/// Overwrite an existing row's value in place (UPDATE keeps the rowid). Plain `put` (takes the key
/// lock if not yet held).
#[allow(unused_variables)]
pub fn write_row(ex: &mut Executor, td: &TableDesc, rowid: u64, row: &[Datum]) -> Result<()> {
    Err(PgError::unimplemented("EXE-02", "heap overwrite (rowstore::write_row)"))
}

/// Delete a row's heap key. Returns whether it existed in the transaction view.
#[allow(unused_variables)]
pub fn delete_heap(ex: &mut Executor, relid: Oid, rowid: u64) -> Result<bool> {
    Err(PgError::unimplemented("EXE-02", "heap delete (rowstore::delete_heap)"))
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
#[allow(unused_variables)]
pub fn lock_row(ex: &mut Executor, td: &TableDesc, rowid: u64, mode: LockMode, policy: WaitPolicy) -> Result<RowLock> {
    Err(PgError::unimplemented("EXE-02", "row locking (rowstore::lock_row)"))
}

/// Delete EVERY key whose first four bytes equal `relid` (a table's heap rows or an index's
/// entries): DROP TABLE/INDEX data, TRUNCATE, aborted index builds. Scans in `SCAN_BATCH` chunks and
/// deletes through the transaction view. Returns the number of keys deleted.
#[allow(unused_variables)]
pub fn delete_relation_data(ex: &mut Executor, relid: Oid) -> Result<u64> {
    Err(PgError::unimplemented("EXE-02", "relation data removal (rowstore::delete_relation_data)"))
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
    #[allow(unused_variables)]
    pub fn next(&mut self, ex: &mut Executor, td: &TableDesc, needed: Option<&[bool]>) -> Result<Option<(u64, Vec<Datum>)>> {
        Err(PgError::unimplemented("EXE-02", "heap cursor (rowstore::HeapCursor::next)"))
    }
}
