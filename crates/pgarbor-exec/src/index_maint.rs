//! Index maintenance and uniqueness enforcement (lease EXE-04, sonnet, north-star).
//! Entry layout: see `index_key`. PG references: `access/nbtree/nbtinsert.c` (`_bt_check_unique`),
//! `executor/execIndexing.c` (`ExecInsertIndexTuples`, `check_exclusion_or_unique_constraint`),
//! `access/index/genam.c` (`BuildIndexValueDescription`).
//!
//! ## Rules
//! * Indexes are maintained for every `IndexDesc` of the table (`td.indexes`, PRIMARY KEY first).
//! * **Unique check** of entry `e` with `e.probe = Some(p)`: `ExecTxn::lock_and_read_latest(p,
//!   ForUpdate, Block)` (waits for a concurrent inserter/deleter of the same key, then reports the
//!   latest committed value merged with own writes). `Locked{value: Some(_)}` = a live conflicting
//!   row -> 23505 via `error::unique_violation(schema, table, idx.violation_name(),
//!   key_description(idx key labels, key types, e.values))`. `Locked{value: None}` = free; write the
//!   entry (`put`; the key is already locked). `NotAvailable` cannot happen with `Block`.
//!   NEVER decide uniqueness from `kv.get` alone (module `txn` docs, item 2).
//! * Non-unique entries and unique entries with a NULL key (suffix form) are brand new keys when the
//!   rowid is new: use `put_fresh` for inserts of a new heap row, plain `put` otherwise.
//! * UPDATE: for each index recompute the entry of the OLD and NEW row; if `entry.key` is identical
//!   (same key values, same rowid) do nothing; else delete the old key and insert the new one (with
//!   the unique check, which must not report the row's own old entry: the old entry is deleted
//!   first).
//! * DELETE: delete the entry key of every index for the old row image (the caller passes the exact
//!   image that was stored; it is recomputed, not looked up).
//! * Multi-row statements check uniqueness row by row against the transaction view (own earlier rows
//!   included), like PG's immediate unique constraints.
//!
//! ## ON CONFLICT probing (`find_conflict`)
//! Read-only: for each unique index in scope (all `td.unique_indexes()` when `arbiters` is None, else
//! those with an OID in the list) compute the probe of the proposed row; skip indexes whose entry has
//! no probe; `lock_and_read_latest` the probe (so the conflicting row cannot vanish/change under us);
//! first index with a live value wins: return the conflicting `rowid` and the LATEST committed image
//! of that heap row (via `rowstore::lock_row`), and keep the probe locks. No entries are written.
//! The caller (`dml_conflict`) then either skips the row (DO NOTHING) or updates the existing row.
//!
//! ## Building (`build_index`, FollowUp::BuildIndex)
//! Scan the whole table (`HeapCursor`), compute entries, and write them. For a unique index detect
//! duplicates among the rows themselves (hash set of probes; NOT through the transaction lock path,
//! the table is being scanned under the DDL's AccessExclusive lock) and fail with
//! `error::unique_build_violation(schema, table, index_name, key_description)` (`Key (a)=(1) is
//! duplicated.`); the first duplicate in rowid order is reported. Entries are `put_fresh`-style bulk
//! writes (use plain `put` when the index already has entries, e.g. REINDEX).
use pgarbor_common::{Oid, PgError, Result};
use pgarbor_types::Datum;

use crate::runtime::Executor;
use crate::tabledesc::{IndexDesc, TableDesc};

/// A live row that conflicts with a proposed row (module docs, "ON CONFLICT probing").
// CONTRACT
#[derive(Clone, Debug, PartialEq)]
pub struct ConflictRow {
    /// The unique index that found it.
    pub index: Oid,
    pub rowid: u64,
    /// Latest committed (merged with own writes) image of the conflicting row.
    pub row: Vec<Datum>,
    /// The latest image differs from the statement snapshot (READ COMMITTED).
    pub changed: bool,
}

/// Insert the entries of a NEW heap row `rowid` into every index of `td` (module docs). On a unique
/// violation returns 23505 and leaves already-written entries of this row in place (the statement
/// aborts and the transaction layer discards them).
#[allow(unused_variables)]
pub fn insert_entries(ex: &mut Executor, td: &TableDesc, rowid: u64, row: &[Datum]) -> Result<()> {
    Err(PgError::unimplemented("EXE-04", "index entry insertion (index_maint::insert_entries)"))
}

/// Remove the entries of the old image of row `rowid` from every index.
#[allow(unused_variables)]
pub fn delete_entries(ex: &mut Executor, td: &TableDesc, rowid: u64, old_row: &[Datum]) -> Result<()> {
    Err(PgError::unimplemented("EXE-04", "index entry deletion (index_maint::delete_entries)"))
}

/// Replace the entries of an updated row (module docs). `modified_cols`: attnums assigned by the
/// UPDATE when known (lets the implementation skip indexes whose key columns, expressions and
/// predicate columns are untouched); `None` = recompute everything.
#[allow(unused_variables)]
pub fn update_entries(
    ex: &mut Executor,
    td: &TableDesc,
    rowid: u64,
    old_row: &[Datum],
    new_row: &[Datum],
    modified_cols: Option<&[i16]>,
) -> Result<()> {
    Err(PgError::unimplemented("EXE-04", "index entry update (index_maint::update_entries)"))
}

/// Find a live row conflicting with `row` on a unique index (module docs, "ON CONFLICT probing").
#[allow(unused_variables)]
pub fn find_conflict(ex: &mut Executor, td: &TableDesc, row: &[Datum], arbiters: Option<&[Oid]>) -> Result<Option<ConflictRow>> {
    Err(PgError::unimplemented("EXE-04", "conflict probe (index_maint::find_conflict)"))
}

/// Point lookup in a UNIQUE index by key values (already cast to the index key types): the heap
/// rowid of the live row with that key, `None` if absent. Read-only (no lock): used by foreign-key
/// parent lookups, which lock the parent row separately.
#[allow(unused_variables)]
pub fn lookup_unique(ex: &mut Executor, idx: &IndexDesc, values: &[Datum]) -> Result<Option<u64>> {
    Err(PgError::unimplemented("EXE-04", "unique index lookup (index_maint::lookup_unique)"))
}

/// Populate a NEW index from the table's rows (module docs, "Building"). Returns the number of
/// entries written.
#[allow(unused_variables)]
pub fn build_index(ex: &mut Executor, td: &TableDesc, idx: &IndexDesc) -> Result<u64> {
    Err(PgError::unimplemented("EXE-04", "index build (index_maint::build_index)"))
}
