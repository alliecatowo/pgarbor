//! Foreign-key enforcement (lease EXE-32, sonnet): PostgreSQL's RI triggers
//! (`utils/adt/ri_triggers.c`) re-implemented as direct index probes.
//!
//! ## Hooks (called by `dml_insert`, `dml_update`, `dml_delete` AFTER the row and its index entries are written)
//! * [`after_insert`]: new row in the referencing table -> queue a [`PendingFkCheck::Child`] per FK in
//!   `td.fks_out` whose columns are all non-NULL (MATCH SIMPLE: any NULL skips the check; MATCH FULL:
//!   all-NULL skips, a mix of NULL and non-NULL fails immediately with 23503 `insert or update on
//!   table "c" violates foreign key constraint "fk"` DETAIL `MATCH FULL does not allow mixing of null
//!   and nonnull key values.`).
//! * [`after_update`]: referencing side: queue Child checks only for FKs whose referencing columns
//!   CHANGED (identical key values need no check). Referenced side: for every FK in `td.fks_in` whose
//!   referenced columns changed (compare with `compare::not_distinct` on the key columns), run the
//!   ON UPDATE action for the old key.
//! * [`after_delete`]: referenced side: for every FK in `td.fks_in`, run the ON DELETE action for the
//!   old row's key (skip when any key column is NULL).
//!
//! ## Actions on the referenced side (`apply_parent_action`)
//! Find the referencing rows: probe an index of the FK table whose leading key columns are exactly
//! `fk.cols` (any btree index of `fks_in`'s table; encode the key with the FK-column types), else
//! sequentially scan the FK table comparing `fk.cols` with the OLD key (`sql_eq`, cast to a common type
//! when the FK column type differs from the referenced column type: use
//! `pgarbor_types::cast::cast_datum` assignment/implicit casts from the referenced column type to the
//! FK column type; if no cast exists the FK could not have been created).
//! * `NO ACTION`: queue [`PendingFkCheck::ParentGone`] (checked at statement end: if a row with the old
//!   key exists in the referenced table again, e.g. after `UPDATE p SET id = id + 1`, nothing is
//!   wrong; else if referencing rows exist raise [`crate::error::fk_still_referenced`]).
//! * `RESTRICT`: same check but IMMEDIATELY (not queued) and ignoring re-insertion.
//! * `CASCADE` delete: delete each referencing row through `dml_delete::apply_delete` (which recurses
//!   into its own `fks_in`); `CASCADE` update: update the FK columns of each referencing row to the new
//!   referenced values through `dml_update::apply_update`.
//! * `SET NULL` / `SET DEFAULT`: update the FK columns (`delete_set_columns` if non-empty for ON
//!   DELETE) of each referencing row to NULL / the column default (`TableDesc` default exprs via
//!   the `tabledesc::bind_stored_text` of the column default, evaluated with `eval`); a NOT NULL FK
//!   column then fails with 23502 as usual. `SET DEFAULT` then queues a `ParentGone` recheck for the
//!   old key (PG `ri_set` -> NO ACTION): if the default still equals the vanished key, 23503 fires.
//! * Recursion depth is bounded by 1000 nested cascade levels (54001 `stack depth limit exceeded`).
//! Row locks: every referencing row is changed through `rowstore::lock_row` first (EvalPlanQual not
//! needed: a concurrently deleted row is simply skipped).
//!
//! ## End-of-statement checks (`run_pending`)
//! Called by `modify` after all rows were applied, before RETURNING rows are released.
//! * `Child { fk, key }` (key values in the FK table's column types): look up the referenced key in
//!   the unique index `fk.ref_index` of the referenced table (cast the values to the referenced column
//!   types first; encode with the index `key_cols`; entry key `[ref_index][enc]` exists?), then take
//!   `ExecTxn::lock_and_read_latest(heap row key, ForKeyShare)` on the parent row and require the
//!   latest value to exist (a concurrent committed DELETE of the parent => violation). Missing =>
//!   [`crate::error::fk_missing_parent`] with `key_description` of the FK column names/values.
//! * `ParentGone { fk, old_key }`: see NO ACTION above.
//! Deferrable constraints are treated as immediate-at-end-of-statement in v1 (known gap: INITIALLY
//! DEFERRED is not honored until COMMIT; `SET CONSTRAINTS` is a no-op).
//!
//! ## Validation (`validate_foreign_key`, FollowUp::ValidateForeignKey)
//! Scan the whole FK table; for every row with a fully non-NULL key run the Child check; first failure
//! is returned (PG reports the same message/detail as an INSERT would).
use std::cell::Cell;
use std::rc::Rc;

use pgarbor_catalog::model::{FkAction, FkMatch};
use pgarbor_common::{KvRead, Oid, PgError, Result, SqlState};
use pgarbor_core::locks::LockMode;
use pgarbor_core::WaitPolicy;
use pgarbor_types::cast::{cast_datum, CastContext};
use pgarbor_types::compare::{not_distinct, sql_eq};
use pgarbor_types::context::{FuncCtx, TypeEnv};
use pgarbor_types::keycodec;
use pgarbor_types::{Datum, TypeId};

use crate::dml_delete::apply_delete;
use crate::dml_update::apply_update;
use crate::error::{fk_missing_parent, fk_still_referenced, key_description, row_lock_not_available};
use crate::eval::eval;
use crate::exec_session::ExecSessionContext;
use crate::index_key::{entry_rowid, index_prefix};
use crate::index_maint;
use crate::rowstore::{self, HeapCursor, RowLock};
use crate::runtime::Executor;
use crate::tabledesc::{bind_stored_text, columns_to_input_refs, table_desc, FkDesc, IndexDesc, TableDesc};

/// A check postponed to the end of the statement (module docs).
// CONTRACT
#[derive(Clone, Debug)]
pub enum PendingFkCheck {
    /// The referencing row with these FK-column values must have a parent.
    Child { fk: Oid, key: Vec<Datum> },
    /// The referenced key `old_key` disappeared (or changed): no referencing row may remain.
    ParentGone { fk: Oid, old_key: Vec<Datum> },
}

/// Maximum nesting of referential actions (module docs).
const MAX_CASCADE_DEPTH: u32 = 1000;

thread_local! {
    /// Current nesting of cascading referential actions on this thread (one session = one thread).
    static CASCADE_DEPTH: Cell<u32> = const { Cell::new(0) };
}

/// RAII marker for one nested cascade level; the depth is restored when it drops, also on error.
struct DepthGuard;

impl DepthGuard {
    fn enter() -> Result<DepthGuard> {
        CASCADE_DEPTH.with(|depth| {
            let n = depth.get();
            if n >= MAX_CASCADE_DEPTH {
                return Err(PgError::new(SqlState::STATEMENT_TOO_COMPLEX, "stack depth limit exceeded"));
            }
            depth.set(n + 1);
            Ok(DepthGuard)
        })
    }
}

impl Drop for DepthGuard {
    fn drop(&mut self) {
        CASCADE_DEPTH.with(|depth| depth.set(depth.get().saturating_sub(1)));
    }
}

/// Hook after a heap row was inserted (module docs).
pub fn after_insert(ex: &mut Executor, td: &TableDesc, new_row: &[Datum]) -> Result<()> {
    for fk in &td.fks_out {
        if let Some(key) = child_key(new_row, fk)? {
            ex.cx.stmt.pending_fk.push(PendingFkCheck::Child { fk: fk.oid, key });
        }
    }
    Ok(())
}

/// Hook after a heap row was updated in place (module docs).
pub fn after_update(ex: &mut Executor, td: &TableDesc, old_row: &[Datum], new_row: &[Datum]) -> Result<()> {
    for fk in &td.fks_out {
        if cols_changed(&ex.cx.se.env, td, &fk.cols, old_row, new_row)? {
            if let Some(key) = child_key(new_row, fk)? {
                ex.cx.stmt.pending_fk.push(PendingFkCheck::Child { fk: fk.oid, key });
            }
        }
    }
    for fk in &td.fks_in {
        if !cols_changed(&ex.cx.se.env, td, &fk.ref_cols, old_row, new_row)? {
            continue;
        }
        let old_key = key_vals(old_row, &fk.ref_cols)?;
        if old_key.iter().any(Datum::is_null) {
            continue;
        }
        let new_key = key_vals(new_row, &fk.ref_cols)?;
        apply_parent_action(ex, fk, &old_key, Some(new_key.as_slice()))?;
    }
    Ok(())
}

/// Hook after a heap row was deleted (module docs).
pub fn after_delete(ex: &mut Executor, td: &TableDesc, old_row: &[Datum]) -> Result<()> {
    for fk in &td.fks_in {
        let old_key = key_vals(old_row, &fk.ref_cols)?;
        if old_key.iter().any(Datum::is_null) {
            continue;
        }
        apply_parent_action(ex, fk, &old_key, None)?;
    }
    Ok(())
}

/// Execute and clear `ex.cx.stmt.pending_fk` (module docs).
pub fn run_pending(ex: &mut Executor) -> Result<()> {
    let pending = std::mem::take(&mut ex.cx.stmt.pending_fk);
    for check in pending {
        match check {
            PendingFkCheck::Child { fk, key } => {
                let (fk_td, fk_desc) = find_fk(ex, fk)?;
                check_child_key(ex, &fk_td, &fk_desc, &key)?;
            }
            PendingFkCheck::ParentGone { fk, old_key } => {
                let (fk_td, fk_desc) = find_fk(ex, fk)?;
                check_parent_gone(ex, &fk_td, &fk_desc, &old_key)?;
            }
        }
    }
    Ok(())
}

/// Validate an existing foreign key against all rows of `table` (module docs).
pub fn validate_foreign_key(ex: &mut Executor, table: Oid, constraint: Oid) -> Result<()> {
    let fk_td = table_desc(&*ex.cx, table)?;
    let fk = fk_td
        .fks_out
        .iter()
        .find(|f| f.oid == constraint)
        .cloned()
        .ok_or_else(|| PgError::internal(format!("foreign key {constraint} does not belong to relation {table}")))?;
    let mut cursor = HeapCursor::new(fk_td.oid);
    while let Some((_rowid, row)) = cursor.next(ex, &fk_td, None)? {
        if let Some(key) = child_key(&row, &fk)? {
            check_child_key(ex, &fk_td, &fk, &key)?;
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------------------------
// Small helpers
// ---------------------------------------------------------------------------------------------

fn missing_col(td: &TableDesc, attnum: i16) -> PgError {
    PgError::internal(format!("attribute {attnum} of relation {} does not exist", td.oid))
}

fn col_type(td: &TableDesc, attnum: i16) -> Result<TypeId> {
    td.col(attnum).map(|c| c.ty).ok_or_else(|| missing_col(td, attnum))
}

fn col_types(td: &TableDesc, attnums: &[i16]) -> Result<Vec<TypeId>> {
    attnums.iter().map(|a| col_type(td, *a)).collect()
}

fn col_names(td: &TableDesc, attnums: &[i16]) -> Result<Vec<String>> {
    attnums.iter().map(|a| td.col(*a).map(|c| c.name.clone()).ok_or_else(|| missing_col(td, *a))).collect()
}

/// Value of attribute `attnum` (1-based) in a full table row.
fn col_val(row: &[Datum], attnum: i16) -> Result<Datum> {
    usize::try_from(attnum)
        .ok()
        .and_then(|a| a.checked_sub(1))
        .and_then(|i| row.get(i))
        .cloned()
        .ok_or_else(|| PgError::internal(format!("attribute {attnum} is outside the row")))
}

fn key_vals(row: &[Datum], attnums: &[i16]) -> Result<Vec<Datum>> {
    attnums.iter().map(|a| col_val(row, *a)).collect()
}

fn set_col(row: &mut Vec<Datum>, attnum: i16, value: Datum) -> Result<()> {
    let slot = usize::try_from(attnum).ok().and_then(|a| a.checked_sub(1)).and_then(|i| row.get_mut(i));
    match slot {
        Some(s) => {
            *s = value;
            Ok(())
        }
        None => Err(PgError::internal(format!("attribute {attnum} is outside the row"))),
    }
}

/// True when some column in `attnums` differs (`IS DISTINCT FROM`) between the two row images.
fn cols_changed(env: &dyn TypeEnv, td: &TableDesc, attnums: &[i16], old: &[Datum], new: &[Datum]) -> Result<bool> {
    for a in attnums {
        let ty = col_type(td, *a)?;
        let o = col_val(old, *a)?;
        let n = col_val(new, *a)?;
        if !not_distinct(env, ty, &o, &n)? {
            return Ok(true);
        }
    }
    Ok(false)
}

/// `key` (values of `attnums`) equals the row's values under SQL `=` (NULL never matches).
fn key_matches(env: &dyn TypeEnv, row: &[Datum], attnums: &[i16], types: &[TypeId], key: &[Datum]) -> Result<bool> {
    if attnums.len() != types.len() || attnums.len() != key.len() {
        return Err(PgError::internal("foreign key arity mismatch"));
    }
    for ((a, t), k) in attnums.iter().zip(types.iter()).zip(key.iter()) {
        let v = col_val(row, *a)?;
        if sql_eq(env, *t, &v, k)? != Some(true) {
            return Ok(false);
        }
    }
    Ok(true)
}

/// The referencing key of a row, or `None` when the check is skipped (module docs, MATCH rules).
fn child_key(row: &[Datum], fk: &FkDesc) -> Result<Option<Vec<Datum>>> {
    let key = key_vals(row, &fk.cols)?;
    let nulls = key.iter().filter(|d| d.is_null()).count();
    match fk.match_type {
        FkMatch::Simple => Ok(if nulls == 0 { Some(key) } else { None }),
        FkMatch::Full => {
            if nulls == 0 {
                Ok(Some(key))
            } else if nulls == key.len() {
                Ok(None)
            } else {
                Err(PgError::new(
                    SqlState::FOREIGN_KEY_VIOLATION,
                    format!(
                        "insert or update on table \"{}\" violates foreign key constraint \"{}\"",
                        fk.fk_table_name, fk.name
                    ),
                )
                .with_detail("MATCH FULL does not allow mixing of null and nonnull key values.")
                .with_table(fk.fk_schema.as_str(), fk.fk_table_name.as_str())
                .with_constraint(fk.name.as_str()))
            }
        }
        FkMatch::Partial => Err(PgError::new(SqlState::FEATURE_NOT_SUPPORTED, "MATCH PARTIAL not yet implemented")),
    }
}

/// Cast one value between two types (no-op when the type OIDs are equal).
fn cast_value(ex: &mut Executor, v: &Datum, from: TypeId, to: TypeId) -> Result<Datum> {
    if from.oid == to.oid || v.is_null() {
        return Ok(v.clone());
    }
    let session = ExecSessionContext::new(&ex.cx.se, &*ex.cx.txn);
    let ctx = FuncCtx {
        env: &ex.cx.se.env,
        io: &ex.cx.se.io,
        session: &session,
        arg_types: &[],
        result_type: to,
        collation: 0,
    };
    cast_datum(v.clone(), from, to, CastContext::Implicit, &ctx)
}

fn cast_all(ex: &mut Executor, values: &[Datum], from: &[TypeId], to: &[TypeId]) -> Result<Vec<Datum>> {
    if values.len() != from.len() || values.len() != to.len() {
        return Err(PgError::internal("foreign key arity mismatch"));
    }
    let mut out = Vec::with_capacity(values.len());
    for ((v, f), t) in values.iter().zip(from.iter()).zip(to.iter()) {
        out.push(cast_value(ex, v, *f, *t)?);
    }
    Ok(out)
}

fn referenced_types(ex: &mut Executor, fk: &FkDesc) -> Result<Vec<TypeId>> {
    let ref_td = table_desc(&*ex.cx, fk.ref_table)?;
    col_types(&ref_td, &fk.ref_cols)
}

/// Position of each unique-index key column inside `attnums` (the FK's referenced columns), so that
/// a key given in `attnums` order can be probed in the index's own column order.
fn index_order(idx: &IndexDesc, attnums: &[i16]) -> Result<Vec<usize>> {
    if idx.keys.len() != attnums.len() {
        return Err(PgError::internal(format!("unique index {} does not match its foreign key", idx.oid)));
    }
    idx.keys
        .iter()
        .map(|k| -> Result<usize> {
            let col = k
                .column
                .ok_or_else(|| PgError::internal(format!("unique index {} has an expression key", idx.oid)))?;
            attnums
                .iter()
                .position(|a| *a == col)
                .ok_or_else(|| PgError::internal(format!("unique index {} does not match its foreign key", idx.oid)))
        })
        .collect()
}

fn reorder<T: Clone>(values: &[T], pos: &[usize]) -> Result<Vec<T>> {
    pos.iter()
        .map(|p| {
            values
                .get(*p)
                .cloned()
                .ok_or_else(|| PgError::internal("unique index column mapping is out of range"))
        })
        .collect()
}

/// The FK constraint `fk_oid` with the description of its referencing table.
fn find_fk(ex: &mut Executor, fk_oid: Oid) -> Result<(Rc<TableDesc>, FkDesc)> {
    let table = match ex.cx.se.env.snapshot().constraint(fk_oid) {
        Some(c) => c.table,
        None => return Err(PgError::internal(format!("cache lookup failed for constraint {fk_oid}"))),
    };
    let td = table_desc(&*ex.cx, table)?;
    let fk = td
        .fks_out
        .iter()
        .find(|f| f.oid == fk_oid)
        .cloned()
        .ok_or_else(|| PgError::internal(format!("foreign key {fk_oid} not found on relation {table}")))?;
    Ok((td, fk))
}

fn missing_parent_error(ex: &mut Executor, fk_td: &TableDesc, fk: &FkDesc, key: &[Datum]) -> Result<PgError> {
    let names = col_names(fk_td, &fk.cols)?;
    let types = col_types(fk_td, &fk.cols)?;
    let desc = key_description(&ex.cx.se.io_ctx(), &names, &types, key)?;
    Ok(fk_missing_parent(&fk.fk_schema, &fk.fk_table_name, &fk.name, &desc, &fk.ref_table_name))
}

fn still_referenced_error(ex: &mut Executor, fk: &FkDesc, ref_key: &[Datum], ref_types: &[TypeId]) -> Result<PgError> {
    let ref_td = table_desc(&*ex.cx, fk.ref_table)?;
    let names = col_names(&ref_td, &fk.ref_cols)?;
    let desc = key_description(&ex.cx.se.io_ctx(), &names, ref_types, ref_key)?;
    Ok(fk_still_referenced(&fk.fk_schema, &fk.ref_table_name, &fk.name, &fk.fk_table_name, &desc))
}

// ---------------------------------------------------------------------------------------------
// Child side
// ---------------------------------------------------------------------------------------------

/// Verify that the referenced row for `key` (FK-column types) exists and is still live (module docs).
fn check_child_key(ex: &mut Executor, fk_td: &TableDesc, fk: &FkDesc, key: &[Datum]) -> Result<()> {
    let fk_types = col_types(fk_td, &fk.cols)?;
    let ref_td = table_desc(&*ex.cx, fk.ref_table)?;
    let ref_types = col_types(&ref_td, &fk.ref_cols)?;
    let probe_ref = cast_all(ex, key, &fk_types, &ref_types)?;
    let idx = ref_td.index(fk.ref_index).ok_or_else(|| {
        PgError::internal(format!("unique index {} of relation {} does not exist", fk.ref_index, ref_td.oid))
    })?;
    let pos = index_order(idx, &fk.ref_cols)?;
    let probe = reorder(&probe_ref, &pos)?;
    let present = match index_maint::lookup_unique(ex, idx, &probe)? {
        None => false,
        Some(rowid) => match rowstore::lock_row(ex, &ref_td, rowid, LockMode::ForKeyShare, WaitPolicy::Block)? {
            RowLock::Gone => false,
            RowLock::NotAvailable => return Err(row_lock_not_available(&ref_td.name)),
            RowLock::Current(img) | RowLock::Changed(img) => {
                key_matches(&ex.cx.se.env, &img, &fk.ref_cols, &ref_types, &probe_ref)?
            }
        },
    };
    if present {
        Ok(())
    } else {
        Err(missing_parent_error(ex, fk_td, fk, key)?)
    }
}

/// End-of-statement NO ACTION check for a vanished referenced key (module docs).
fn check_parent_gone(ex: &mut Executor, fk_td: &TableDesc, fk: &FkDesc, old_key: &[Datum]) -> Result<()> {
    let ref_td = table_desc(&*ex.cx, fk.ref_table)?;
    let ref_types = col_types(&ref_td, &fk.ref_cols)?;
    let idx = ref_td.index(fk.ref_index).ok_or_else(|| {
        PgError::internal(format!("unique index {} of relation {} does not exist", fk.ref_index, ref_td.oid))
    })?;
    let pos = index_order(idx, &fk.ref_cols)?;
    let probe = reorder(old_key, &pos)?;
    if index_maint::lookup_unique(ex, idx, &probe)?.is_some() {
        return Ok(());
    }
    let fk_types = col_types(fk_td, &fk.cols)?;
    let fk_key = cast_all(ex, old_key, &ref_types, &fk_types)?;
    if find_referencing(ex, fk_td, fk, &fk_key, 1)?.is_empty() {
        return Ok(());
    }
    Err(still_referenced_error(ex, fk, old_key, &ref_types)?)
}

// ---------------------------------------------------------------------------------------------
// Referencing-row discovery
// ---------------------------------------------------------------------------------------------

/// An index of the FK table whose leading plain key columns are exactly `attnums` (partial indexes
/// are excluded: they do not contain every referencing row).
fn index_leads_with(idx: &IndexDesc, attnums: &[i16]) -> bool {
    idx.predicate.is_none()
        && idx.keys.len() >= attnums.len()
        && idx.key_cols.len() >= attnums.len()
        && idx.keys.iter().zip(attnums.iter()).all(|(k, a)| k.column == Some(*a) && k.expr.is_none())
}

/// Heap rowids of the index entries whose leading `n` key values equal `key`.
fn referencing_rowids(ex: &mut Executor, idx: &IndexDesc, n: usize, key: &[Datum]) -> Result<Vec<u64>> {
    let cols = idx
        .key_cols
        .get(..n)
        .ok_or_else(|| PgError::internal("index key description is shorter than the foreign key"))?;
    let mut prefix = index_prefix(idx.oid).to_vec();
    let enc = keycodec::encode_key_vec(&ex.cx.se.env, cols, key)?;
    prefix.extend_from_slice(&enc);
    let mut rowids = Vec::new();
    for item in ex.txn().scan_prefix(&prefix)? {
        let (k, v) = item?;
        rowids.push(entry_rowid(&k, &v)?);
    }
    Ok(rowids)
}

/// Rowids of the referencing rows whose FK columns equal `fk_key` (FK-column types), at most `limit`.
fn find_referencing(ex: &mut Executor, fk_td: &TableDesc, fk: &FkDesc, fk_key: &[Datum], limit: usize) -> Result<Vec<u64>> {
    let fk_types = col_types(fk_td, &fk.cols)?;
    let mut found: Vec<u64> = Vec::new();
    if let Some(idx) = fk_td.indexes.iter().find(|i| index_leads_with(i, &fk.cols)) {
        for rowid in referencing_rowids(ex, idx, fk.cols.len(), fk_key)? {
            if found.len() >= limit {
                break;
            }
            if let Some(row) = rowstore::get_row(ex, fk_td, rowid)? {
                if key_matches(&ex.cx.se.env, &row, &fk.cols, &fk_types, fk_key)? {
                    found.push(rowid);
                }
            }
        }
    } else {
        let mut cursor = HeapCursor::new(fk_td.oid);
        while found.len() < limit {
            let Some((rowid, row)) = cursor.next(ex, fk_td, None)? else {
                break;
            };
            if key_matches(&ex.cx.se.env, &row, &fk.cols, &fk_types, fk_key)? {
                found.push(rowid);
            }
        }
    }
    Ok(found)
}

/// Lock a referencing row and return its current image if it still references `fk_key`.
fn lock_matching(
    ex: &mut Executor,
    fk_td: &TableDesc,
    fk: &FkDesc,
    rowid: u64,
    mode: LockMode,
    fk_key: &[Datum],
) -> Result<Option<Vec<Datum>>> {
    let fk_types = col_types(fk_td, &fk.cols)?;
    let img = match rowstore::lock_row(ex, fk_td, rowid, mode, WaitPolicy::Block)? {
        RowLock::Gone => return Ok(None),
        RowLock::NotAvailable => return Err(row_lock_not_available(&fk_td.name)),
        RowLock::Current(img) | RowLock::Changed(img) => img,
    };
    if key_matches(&ex.cx.se.env, &img, &fk.cols, &fk_types, fk_key)? {
        Ok(Some(img))
    } else {
        Ok(None)
    }
}

// ---------------------------------------------------------------------------------------------
// Parent side
// ---------------------------------------------------------------------------------------------

/// The default value of column `attnum` of the FK table, evaluated for `row` (NULL when none).
fn column_default(ex: &mut Executor, fk_td: &TableDesc, attnum: i16, row: &[Datum]) -> Result<Datum> {
    let default_sql: Option<String> = ex
        .cx
        .se
        .env
        .snapshot()
        .relation(fk_td.oid)
        .and_then(|rel| rel.columns.iter().find(|c| c.attnum == attnum))
        .and_then(|c| c.default.as_ref())
        .map(|d| d.expr.sql.clone());
    let Some(sql) = default_sql else {
        return Ok(Datum::Null);
    };
    let bound = bind_stored_text(&ex.cx.se, &sql, fk_td.oid)?;
    let expr = columns_to_input_refs(bound)?;
    let value = eval(ex, &expr, row)?;
    let col_ty = col_type(fk_td, attnum)?;
    cast_value(ex, &value, expr.ty, col_ty)
}

/// Run the referential action of `fk` for a referenced key that was deleted (`new_key` None) or
/// updated to `new_key` (module docs, "Actions on the referenced side").
fn apply_parent_action(ex: &mut Executor, fk: &FkDesc, old_key: &[Datum], new_key: Option<&[Datum]>) -> Result<()> {
    let action = if new_key.is_some() { fk.on_update } else { fk.on_delete };
    match action {
        FkAction::NoAction => {
            ex.cx.stmt.pending_fk.push(PendingFkCheck::ParentGone { fk: fk.oid, old_key: old_key.to_vec() });
            Ok(())
        }
        FkAction::Restrict => {
            let fk_td = table_desc(&*ex.cx, fk.fk_table)?;
            let fk_types = col_types(&fk_td, &fk.cols)?;
            let ref_types = referenced_types(ex, fk)?;
            let fk_key = cast_all(ex, old_key, &ref_types, &fk_types)?;
            if find_referencing(ex, &fk_td, fk, &fk_key, 1)?.is_empty() {
                Ok(())
            } else {
                Err(still_referenced_error(ex, fk, old_key, &ref_types)?)
            }
        }
        FkAction::Cascade | FkAction::SetNull | FkAction::SetDefault => {
            let _depth = DepthGuard::enter()?;
            cascade_parent_action(ex, fk, action, old_key, new_key)?;
            if action == FkAction::SetDefault {
                // PostgreSQL (ri_set) re-runs the NO ACTION check after SET DEFAULT: the default may
                // still be the vanished key, in which case the referencing rows point at it again.
                ex.cx.stmt.pending_fk.push(PendingFkCheck::ParentGone { fk: fk.oid, old_key: old_key.to_vec() });
            }
            Ok(())
        }
    }
}

/// CASCADE / SET NULL / SET DEFAULT over every referencing row (module docs).
fn cascade_parent_action(
    ex: &mut Executor,
    fk: &FkDesc,
    action: FkAction,
    old_key: &[Datum],
    new_key: Option<&[Datum]>,
) -> Result<()> {
    let fk_td = table_desc(&*ex.cx, fk.fk_table)?;
    let fk_types = col_types(&fk_td, &fk.cols)?;
    let ref_types = referenced_types(ex, fk)?;
    let fk_key = cast_all(ex, old_key, &ref_types, &fk_types)?;
    let new_fk_vals: Option<Vec<Datum>> = match new_key {
        Some(nk) => Some(cast_all(ex, nk, &ref_types, &fk_types)?),
        None => None,
    };
    let set_cols: Vec<i16> = if new_key.is_none() && !fk.delete_set_columns.is_empty() {
        fk.delete_set_columns.clone()
    } else {
        fk.cols.clone()
    };
    let mode = if new_key.is_some() { LockMode::ForNoKeyUpdate } else { LockMode::ForUpdate };
    for rowid in find_referencing(ex, &fk_td, fk, &fk_key, usize::MAX)? {
        let Some(img) = lock_matching(ex, &fk_td, fk, rowid, mode, &fk_key)? else {
            continue;
        };
        match action {
            FkAction::Cascade => match &new_fk_vals {
                None => {
                    apply_delete(ex, &fk_td, rowid, &img)?;
                }
                Some(vals) => {
                    let mut row = img.clone();
                    for (a, v) in fk.cols.iter().zip(vals.iter()) {
                        set_col(&mut row, *a, v.clone())?;
                    }
                    apply_update(ex, &fk_td, rowid, &img, row, Some(fk.cols.as_slice()))?;
                }
            },
            FkAction::SetNull | FkAction::SetDefault => {
                let mut row = img.clone();
                for a in &set_cols {
                    let v = if action == FkAction::SetNull {
                        Datum::Null
                    } else {
                        column_default(ex, &fk_td, *a, &img)?
                    };
                    set_col(&mut row, *a, v)?;
                }
                apply_update(ex, &fk_td, rowid, &img, row, Some(set_cols.as_slice()))?;
            }
            FkAction::NoAction | FkAction::Restrict => {
                return Err(PgError::internal("unexpected referential action in cascade"));
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tabledesc::IdxKeyDesc;

    fn fk_with(match_type: FkMatch, cols: Vec<i16>) -> FkDesc {
        FkDesc {
            oid: 1000,
            name: "c_a_fkey".to_string(),
            fk_table: 2000,
            fk_schema: "public".to_string(),
            fk_table_name: "c".to_string(),
            cols: cols.clone(),
            ref_table: 3000,
            ref_schema: "public".to_string(),
            ref_table_name: "p".to_string(),
            ref_cols: cols,
            ref_index: 4000,
            on_update: FkAction::NoAction,
            on_delete: FkAction::NoAction,
            match_type,
            deferrable: false,
            initially_deferred: false,
            validated: true,
            delete_set_columns: Vec::new(),
        }
    }

    #[test]
    fn match_simple_skips_any_null_key() {
        let fk = fk_with(FkMatch::Simple, vec![1, 2]);
        assert_eq!(child_key(&[Datum::Int4(1), Datum::Null], &fk).unwrap(), None);
        assert_eq!(
            child_key(&[Datum::Int4(1), Datum::Int4(2)], &fk).unwrap(),
            Some(vec![Datum::Int4(1), Datum::Int4(2)])
        );
    }

    #[test]
    fn match_full_skips_all_null_and_rejects_mixed() {
        let fk = fk_with(FkMatch::Full, vec![1, 2]);
        assert_eq!(child_key(&[Datum::Null, Datum::Null], &fk).unwrap(), None);
        let err = child_key(&[Datum::Int4(1), Datum::Null], &fk).unwrap_err();
        assert!(err.sqlstate == SqlState::FOREIGN_KEY_VIOLATION);
        assert_eq!(err.detail.as_deref(), Some("MATCH FULL does not allow mixing of null and nonnull key values."));
    }

    #[test]
    fn index_order_maps_permuted_unique_index() {
        let key = |attnum: i16| IdxKeyDesc {
            column: Some(attnum),
            expr: None,
            ty: TypeId::of(23),
            descending: false,
            nulls_first: false,
            label: format!("c{attnum}"),
        };
        let idx = IndexDesc {
            oid: 4000,
            name: "p_pkey".to_string(),
            unique: true,
            primary: true,
            nulls_not_distinct: false,
            keys: vec![key(2), key(1)],
            include: Vec::new(),
            key_cols: Vec::new(),
            predicate: None,
            constraint: None,
        };
        assert_eq!(index_order(&idx, &[1, 2]).unwrap(), vec![1, 0]);
        assert!(index_order(&idx, &[1, 3]).is_err());
    }

    #[test]
    fn cascade_depth_is_bounded() {
        let mut guards = Vec::new();
        for _ in 0..MAX_CASCADE_DEPTH {
            guards.push(DepthGuard::enter().unwrap());
        }
        assert!(DepthGuard::enter().is_err());
        drop(guards);
        assert!(DepthGuard::enter().is_ok());
    }
}
