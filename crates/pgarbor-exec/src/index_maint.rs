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
use std::collections::HashSet;
use std::ops::Bound;

use pgarbor_common::kv::prefix_upper_bound;
use pgarbor_common::{KvRead, KvWrite, Oid, PgError, Result};
use pgarbor_core::locks::LockMode;
use pgarbor_core::WaitPolicy;
use pgarbor_types::{Datum, TypeId};

use crate::error;
use crate::index_key::{self, EntryKey};
use crate::rowstore::{self, HeapCursor, RowLock};
use crate::runtime::Executor;
use crate::tabledesc::{IndexDesc, TableDesc};
use crate::txn::{kv_scan_vec, ExecTxn, LatestRead};

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

/// `Key (a, b)=(1, 2)` for the key columns of `idx` and the given evaluated values.
fn key_desc(ex: &Executor, idx: &IndexDesc, values: &[Datum]) -> Result<String> {
    let names: Vec<String> = idx.keys.iter().map(|k| k.label.clone()).collect();
    let types: Vec<TypeId> = idx.keys.iter().map(|k| k.ty).collect();
    error::key_description(&ex.se().io_ctx(), &names, &types, values)
}

/// Write the entry `e` (rowid `rowid`) of index `idx`, enforcing uniqueness when `e` has a probe
/// (module docs, "Unique check"). `fresh_row`: the heap row is brand new (no existing entries).
fn put_entry(ex: &mut Executor, td: &TableDesc, idx: &IndexDesc, e: &EntryKey, rowid: u64, fresh_row: bool) -> Result<()> {
    let value = index_key::entry_value(rowid);
    match &e.probe {
        Some(probe) => {
            let latest = ex.txn().lock_and_read_latest(probe, LockMode::ForUpdate, WaitPolicy::Block)?;
            match latest {
                LatestRead::Locked { value: Some(_), .. } => {
                    let desc = key_desc(ex, idx, &e.values)?;
                    Err(error::unique_violation(&td.schema, &td.name, idx.violation_name(), &desc))
                }
                LatestRead::Locked { value: None, .. } => ex.txn().put(&e.key, &value),
                LatestRead::NotAvailable => Err(PgError::internal(format!(
                    "unique probe lock on index \"{}\" not available under a blocking wait",
                    idx.name
                ))),
            }
        }
        None => {
            if fresh_row {
                ex.txn().put_fresh(&e.key, &value)
            } else {
                ex.txn().put(&e.key, &value)
            }
        }
    }
}

/// Insert the entries of a NEW heap row `rowid` into every index of `td` (module docs). On a unique
/// violation returns 23505 and leaves already-written entries of this row in place (the statement
/// aborts and the transaction layer discards them).
pub fn insert_entries(ex: &mut Executor, td: &TableDesc, rowid: u64, row: &[Datum]) -> Result<()> {
    for idx in &td.indexes {
        if let Some(e) = index_key::make_entry(ex, idx, rowid, row)? {
            put_entry(ex, td, idx, &e, rowid, true)?;
        }
    }
    Ok(())
}

/// Remove the entries of the old image of row `rowid` from every index.
pub fn delete_entries(ex: &mut Executor, td: &TableDesc, rowid: u64, old_row: &[Datum]) -> Result<()> {
    for idx in &td.indexes {
        if let Some(e) = index_key::make_entry(ex, idx, rowid, old_row)? {
            ex.txn().delete(&e.key)?;
        }
    }
    Ok(())
}

/// Whether an UPDATE assigning `cols` can change the entries of `idx`. Conservative: expression
/// keys and partial-index predicates are always treated as touched.
fn index_touched(idx: &IndexDesc, cols: &[i16]) -> bool {
    if idx.predicate.is_some() {
        return true;
    }
    idx.keys.iter().any(|k| match k.column {
        Some(attnum) => cols.contains(&attnum),
        None => true,
    })
}

/// Replace the entries of an updated row (module docs). `modified_cols`: attnums assigned by the
/// UPDATE when known (lets the implementation skip indexes whose key columns, expressions and
/// predicate columns are untouched); `None` = recompute everything.
pub fn update_entries(
    ex: &mut Executor,
    td: &TableDesc,
    rowid: u64,
    old_row: &[Datum],
    new_row: &[Datum],
    modified_cols: Option<&[i16]>,
) -> Result<()> {
    for idx in &td.indexes {
        if let Some(cols) = modified_cols {
            if !index_touched(idx, cols) {
                continue;
            }
        }
        let old_e = index_key::make_entry(ex, idx, rowid, old_row)?;
        let new_e = index_key::make_entry(ex, idx, rowid, new_row)?;
        match (old_e, new_e) {
            (None, None) => {}
            (Some(o), None) => {
                ex.txn().delete(&o.key)?;
            }
            (None, Some(n)) => {
                put_entry(ex, td, idx, &n, rowid, false)?;
            }
            (Some(o), Some(n)) => {
                if o.key == n.key {
                    // Same key bytes (same key values, same rowid): nothing to do.
                    continue;
                }
                // Delete the old entry first so the unique check does not see the row's own key.
                ex.txn().delete(&o.key)?;
                put_entry(ex, td, idx, &n, rowid, false)?;
            }
        }
    }
    Ok(())
}

/// Find a live row conflicting with `row` on a unique index (module docs, "ON CONFLICT probing").
pub fn find_conflict(ex: &mut Executor, td: &TableDesc, row: &[Datum], arbiters: Option<&[Oid]>) -> Result<Option<ConflictRow>> {
    for idx in td.unique_indexes() {
        if let Some(list) = arbiters {
            if !list.contains(&idx.oid) {
                continue;
            }
        }
        // The probe does not depend on the rowid (unique entries without a NULL have no suffix), so
        // a placeholder rowid 0 is fine for the proposed row, which has none yet.
        let e = match index_key::make_entry(ex, idx, 0, row)? {
            Some(e) => e,
            None => continue,
        };
        let probe = match &e.probe {
            Some(p) => p.clone(),
            None => continue,
        };
        let latest = ex.txn().lock_and_read_latest(&probe, LockMode::ForUpdate, WaitPolicy::Block)?;
        let value = match latest {
            LatestRead::Locked { value: Some(v), .. } => v,
            LatestRead::Locked { value: None, .. } => continue,
            LatestRead::NotAvailable => {
                return Err(PgError::internal(format!(
                    "unique probe lock on index \"{}\" not available under a blocking wait",
                    idx.name
                )));
            }
        };
        let rowid = index_key::rowid_of_entry_value(&value)?;
        match rowstore::lock_row(ex, td, rowid, LockMode::ForUpdate, WaitPolicy::Block)? {
            RowLock::Current(img) => {
                return Ok(Some(ConflictRow { index: idx.oid, rowid, row: img, changed: false }));
            }
            RowLock::Changed(img) => {
                return Ok(Some(ConflictRow { index: idx.oid, rowid, row: img, changed: true }));
            }
            RowLock::Gone => {
                return Err(PgError::corruption(format!(
                    "index \"{}\" has a live entry for missing row {rowid}",
                    idx.name
                )));
            }
            RowLock::NotAvailable => {
                return Err(PgError::internal(format!(
                    "row lock for rowid {rowid} not available under a blocking wait"
                )));
            }
        }
    }
    Ok(None)
}

/// Point lookup in a UNIQUE index by key values (already cast to the index key types): the heap
/// rowid of the live row with that key, `None` if absent. Read-only (no lock): used by foreign-key
/// parent lookups, which lock the parent row separately.
pub fn lookup_unique(ex: &mut Executor, idx: &IndexDesc, values: &[Datum]) -> Result<Option<u64>> {
    if !idx.unique || values.len() != idx.key_cols.len() {
        return Err(PgError::internal(format!(
            "lookup_unique on index \"{}\" with {} key values for {} key columns",
            idx.name,
            values.len(),
            idx.key_cols.len()
        )));
    }
    // NULLs are distinct in a unique index unless NULLS NOT DISTINCT: such a key never matches.
    if !idx.nulls_not_distinct && values.iter().any(|v| v.is_null()) {
        return Ok(None);
    }
    let probe = index_key::encode_prefix(&ex.se().env, idx.oid, &idx.key_cols, values)?;
    match ex.txn().get(&probe)? {
        Some(v) => Ok(Some(index_key::rowid_of_entry_value(&v)?)),
        None => Ok(None),
    }
}

/// Populate a NEW index from the table's rows (module docs, "Building"). Returns the number of
/// entries written.
pub fn build_index(ex: &mut Executor, td: &TableDesc, idx: &IndexDesc) -> Result<u64> {
    let prefix = index_key::index_prefix(idx.oid);
    let upper = prefix_upper_bound(&prefix);
    let upper_bound = match &upper {
        Some(u) => Bound::Excluded(u.as_slice()),
        None => Bound::Unbounded,
    };
    // Entries of this index already present (REINDEX) need plain puts; a fresh index can use
    // put_fresh because the duplicate check below guarantees every key is written once.
    let existing = kv_scan_vec(&*ex.txn(), Bound::Included(&prefix[..]), upper_bound, 1)?;
    let fresh = existing.is_empty();

    let mut cursor = HeapCursor::new(td.oid);
    let mut seen: HashSet<Vec<u8>> = HashSet::new();
    let mut written: u64 = 0;
    let mut ticks: u32 = 0;
    while let Some((rowid, row)) = cursor.next(ex, td, None)? {
        ex.tick(&mut ticks)?;
        let e = match index_key::make_entry(ex, idx, rowid, &row)? {
            Some(e) => e,
            None => continue,
        };
        if let Some(probe) = &e.probe {
            if !seen.insert(probe.clone()) {
                let desc = key_desc(ex, idx, &e.values)?;
                return Err(error::unique_build_violation(&td.schema, &td.name, &idx.name, &desc));
            }
        }
        let value = index_key::entry_value(rowid);
        if fresh {
            ex.txn().put_fresh(&e.key, &value)?;
        } else {
            ex.txn().put(&e.key, &value)?;
        }
        written += 1;
    }
    Ok(written)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::context::{ExecContext, NoGuc, NoServices, SessionEnv};
    use crate::memtxn::MemTxn;
    use crate::runtime::Runtime;
    use crate::tabledesc::{ColDesc, IdxKeyDesc};
    use pgarbor_catalog::model::RelKind;
    use pgarbor_types::context::IoConfig;
    use pgarbor_types::keycodec::KeyColumn;
    use std::cell::Cell;

    /// Table 16400 with one column `a int4` and a unique index 16401 on it.
    fn test_table() -> TableDesc {
        TableDesc {
            oid: 16400,
            schema: "public".into(),
            name: "t".into(),
            kind: RelKind::Table,
            cols: vec![ColDesc {
                attnum: 1,
                name: "a".into(),
                ty: TypeId::INT4,
                not_null: false,
                dropped: false,
                generated: None,
                identity: None,
                missing: None,
            }],
            indexes: vec![IndexDesc {
                oid: 16401,
                name: "t_a_key".into(),
                unique: true,
                primary: false,
                nulls_not_distinct: false,
                include: Vec::new(),
                keys: vec![IdxKeyDesc {
                    column: Some(1),
                    expr: None,
                    ty: TypeId::INT4,
                    descending: false,
                    nulls_first: false,
                    label: "a".into(),
                }],
                key_cols: vec![KeyColumn::asc(TypeId::INT4)],
                predicate: None,
                constraint: None,
            }],
            checks: Vec::new(),
            virtual_not_null: Vec::new(),
            fks_out: Vec::new(),
            fks_in: Vec::new(),
        }
    }

    /// Runs `body` with an executor over `txn` (a MemTxn with the bootstrapped catalog).
    fn run_with_ex(txn: &mut MemTxn, body: &mut dyn FnMut(&mut Executor<'_, '_, '_>) -> Result<()>) -> Result<()> {
        let (_, catalog) = pgarbor_catalog::testing::bootstrapped()?;
        let guc = NoGuc;
        let services = NoServices;
        let rng = Cell::new(1u64);
        let se = SessionEnv::new(catalog, IoConfig::default(), &guc, &services, &rng, 0, 0);
        let mut cx = ExecContext::new(txn, se);
        let mut rt = Runtime::default();
        let mut ex = Executor::new(&mut rt, &mut cx);
        body(&mut ex)
    }

    fn all_keys(ex: &mut Executor) -> Result<Vec<(Vec<u8>, Vec<u8>)>> {
        kv_scan_vec(&*ex.txn(), Bound::Unbounded, Bound::Unbounded, usize::MAX)
    }

    #[test]
    #[ignore = "needs EXE-03"]
    fn duplicate_key_insert_is_23505() -> Result<()> {
        let td = test_table();
        let mut txn = MemTxn::new();
        run_with_ex(&mut txn, &mut |ex: &mut Executor<'_, '_, '_>| {
            insert_entries(ex, &td, 1, &[Datum::Int4(1)])?;
            let err = match insert_entries(ex, &td, 2, &[Datum::Int4(1)]) {
                Err(e) => e,
                Ok(()) => return Err(PgError::internal("duplicate insert was accepted")),
            };
            assert_eq!(err.sqlstate.code(), "23505");
            assert_eq!(err.message, "duplicate key value violates unique constraint \"t_a_key\"");
            assert_eq!(err.detail.as_deref(), Some("Key (a)=(1) already exists."));
            Ok(())
        })
    }

    #[test]
    #[ignore = "needs EXE-03"]
    fn null_keys_do_not_conflict() -> Result<()> {
        let td = test_table();
        let mut txn = MemTxn::new();
        run_with_ex(&mut txn, &mut |ex: &mut Executor<'_, '_, '_>| {
            insert_entries(ex, &td, 1, &[Datum::Null])?;
            insert_entries(ex, &td, 2, &[Datum::Null])?;
            assert_eq!(all_keys(ex)?.len(), 2);
            Ok(())
        })
    }

    #[test]
    #[ignore = "needs EXE-03"]
    fn update_with_unchanged_key_writes_nothing() -> Result<()> {
        let td = test_table();
        let mut txn = MemTxn::new();
        run_with_ex(&mut txn, &mut |ex: &mut Executor<'_, '_, '_>| {
            insert_entries(ex, &td, 1, &[Datum::Int4(5)])?;
            let before = all_keys(ex)?;
            update_entries(ex, &td, 1, &[Datum::Int4(5)], &[Datum::Int4(5)], None)?;
            assert_eq!(all_keys(ex)?, before);
            Ok(())
        })
    }

    #[test]
    #[ignore = "needs EXE-03"]
    fn update_that_changes_key_moves_entry() -> Result<()> {
        let td = test_table();
        let mut txn = MemTxn::new();
        run_with_ex(&mut txn, &mut |ex: &mut Executor<'_, '_, '_>| {
            insert_entries(ex, &td, 1, &[Datum::Int4(5)])?;
            update_entries(ex, &td, 1, &[Datum::Int4(5)], &[Datum::Int4(6)], Some(&[1i16][..]))?;
            // The old key is free again, the new key is taken.
            insert_entries(ex, &td, 2, &[Datum::Int4(5)])?;
            assert!(insert_entries(ex, &td, 3, &[Datum::Int4(6)]).is_err());
            Ok(())
        })
    }

    #[test]
    #[ignore = "needs EXE-03"]
    fn delete_removes_entry() -> Result<()> {
        let td = test_table();
        let mut txn = MemTxn::new();
        run_with_ex(&mut txn, &mut |ex: &mut Executor<'_, '_, '_>| {
            insert_entries(ex, &td, 1, &[Datum::Int4(7)])?;
            delete_entries(ex, &td, 1, &[Datum::Int4(7)])?;
            assert!(all_keys(ex)?.is_empty());
            Ok(())
        })
    }

    #[test]
    #[ignore = "needs EXE-02, EXE-03"]
    fn find_conflict_returns_live_row() -> Result<()> {
        let td = test_table();
        let mut txn = MemTxn::new();
        run_with_ex(&mut txn, &mut |ex: &mut Executor<'_, '_, '_>| {
            let rowid = rowstore::insert_heap(ex, &td, &[Datum::Int4(9)])?;
            insert_entries(ex, &td, rowid, &[Datum::Int4(9)])?;
            let found = find_conflict(ex, &td, &[Datum::Int4(9)], None)?;
            let found = found.ok_or_else(|| PgError::internal("expected a conflicting row"))?;
            assert_eq!(found.rowid, rowid);
            assert_eq!(found.row, vec![Datum::Int4(9)]);
            assert!(find_conflict(ex, &td, &[Datum::Int4(10)], None)?.is_none());
            Ok(())
        })
    }
}
