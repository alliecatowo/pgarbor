//! INSERT row processing (lease EXE-27, north-star). PG: `nodeModifyTable.c` (`ExecInsert`),
//! `execMain.c` (`ExecConstraints`), `execIndexing.c`. Spec: plan.rs `InsertSpec`.
//!
//! ## `assemble_row` (INSERT only)
//! The input row has ONE value per `spec.columns` entry (already assignment-coerced). Build the full
//! row of `td.ncols()` slots: place the supplied values at `attnum - 1`; for every `(attnum, expr)`
//! of `spec.defaults` evaluate `expr` (over the empty row; `nextval` etc. run once per row, in
//! attnum order) into its slot (this also covers columns replaced by `OVERRIDING USER VALUE`); remaining
//! live slots stay NULL; dropped slots NULL; then for `(attnum, expr)` of `spec.generated` (stored
//! generated columns, attnum order) evaluate `expr` over the full row so far and store the result;
//! VIRTUAL generated columns stay NULL in storage. A `SetToDefault{attnum}` that survived in the
//! input row values is replaced by the evaluated default of that column (`spec.defaults`).
//!
//! ## `insert_row` (shared by INSERT, COPY, CTAS, matview refresh, FK actions)
//! `constraints::enforce` (NOT NULL then CHECK) -> `rowstore::insert_heap` (new rowid) ->
//! `index_maint::insert_entries` (unique checks; 23505) -> `fk::after_insert`. Returns the rowid. If
//! any step fails the statement aborts; partially written pieces are discarded with the transaction.
//! `ex.cx.stmt.rows_modified += 1`.
//!
//! ## `insert_input_row` (INSERT driver)
//! `assemble_row`; if `spec.on_conflict` is `Some` delegate to `dml_conflict::insert_or_resolve`
//! (which calls `insert_row` when there is no conflict); else `insert_row`. Returns
//! `Some(InsertedRow)` for rows that were inserted or updated, `None` for rows skipped by DO NOTHING.
//! A plain failing row is an `Err` (the statement fails).
use pgarbor_common::{PgError, Result};
use pgarbor_plan::plan::InsertSpec;
use pgarbor_types::Datum;

use crate::constraints;
use crate::dml_conflict::{self, ConflictOutcome, ConflictState};
use crate::eval;
use crate::fk;
use crate::index_maint;
use crate::rowstore;
use crate::runtime::Executor;
use crate::tabledesc::TableDesc;

/// A row that was inserted (or updated by ON CONFLICT DO UPDATE).
// CONTRACT
#[derive(Clone, Debug, PartialEq)]
pub struct InsertedRow {
    pub rowid: u64,
    /// Full stored image (`ncols` slots): the RETURNING input.
    pub row: Vec<Datum>,
}

/// Slot index (attnum - 1) of an attnum that must exist in `td`.
fn slot_index(td: &TableDesc, attnum: i16) -> Result<usize> {
    if td.col(attnum).is_none() {
        return Err(PgError::internal(format!(
            "INSERT attnum {attnum} is outside relation \"{}\"",
            td.name
        )));
    }
    Ok(attnum as usize - 1)
}

/// Build the full `ncols` row of an INSERT (module docs, "assemble_row").
pub fn assemble_row(ex: &mut Executor, td: &TableDesc, spec: &InsertSpec, input: &[Datum]) -> Result<Vec<Datum>> {
    if input.len() != spec.columns.len() {
        return Err(PgError::internal(format!(
            "INSERT row has {} values for {} target columns",
            input.len(),
            spec.columns.len()
        )));
    }
    let mut row: Vec<Datum> = vec![Datum::Null; td.ncols()];
    for (attnum, value) in spec.columns.iter().zip(input.iter()) {
        let slot = slot_index(td, *attnum)?;
        row[slot] = value.clone();
    }
    // Defaults are evaluated over no input row, in attnum order; they overwrite supplied values
    // for OVERRIDING USER VALUE columns.
    for (attnum, expr) in spec.defaults.iter() {
        let slot = slot_index(td, *attnum)?;
        let value = eval::eval(ex, expr, &[])?;
        row[slot] = value;
    }
    // Dropped slots are always NULL.
    for (slot, col) in td.cols.iter().enumerate() {
        if col.dropped {
            row[slot] = Datum::Null;
        }
    }
    // Stored generated columns see the full row built so far (attnum order).
    for (attnum, expr) in spec.generated.iter() {
        let slot = slot_index(td, *attnum)?;
        let value = eval::eval(ex, expr, &row)?;
        row[slot] = value;
    }
    Ok(row)
}

/// Shared insert pipeline over a borrowed row (see [`insert_row`]).
fn insert_row_ref(ex: &mut Executor, td: &TableDesc, row: &[Datum]) -> Result<u64> {
    constraints::enforce(ex, td, row)?;
    let rowid = rowstore::insert_heap(ex, td, row)?;
    index_maint::insert_entries(ex, td, rowid, row)?;
    fk::after_insert(ex, td, row)?;
    ex.cx.stmt.rows_modified += 1;
    Ok(rowid)
}

/// constraints -> heap -> index entries -> FK hooks; returns the new rowid.
pub fn insert_row(ex: &mut Executor, td: &TableDesc, row: Vec<Datum>) -> Result<u64> {
    insert_row_ref(ex, td, &row)
}

/// INSERT driver for one input row (module docs, "insert_input_row").
pub fn insert_input_row(
    ex: &mut Executor,
    td: &TableDesc,
    spec: &InsertSpec,
    input: &[Datum],
    conflict_state: &mut ConflictState,
) -> Result<Option<InsertedRow>> {
    let row = assemble_row(ex, td, spec, input)?;
    match &spec.on_conflict {
        Some(conflict) => match dml_conflict::insert_or_resolve(ex, td, conflict, row, conflict_state)? {
            ConflictOutcome::Inserted { rowid, row } | ConflictOutcome::Updated { rowid, row } => {
                Ok(Some(InsertedRow { rowid, row }))
            }
            ConflictOutcome::Skipped => Ok(None),
        },
        None => {
            let rowid = insert_row_ref(ex, td, &row)?;
            Ok(Some(InsertedRow { rowid, row }))
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
    use pgarbor_sql::ir::BoundExpr;
    use pgarbor_types::context::IoConfig;
    use pgarbor_types::TypeId;
    use std::cell::Cell;

    fn col(attnum: i16, name: &str, dropped: bool) -> ColDesc {
        ColDesc {
            attnum,
            name: name.to_string(),
            ty: TypeId::INT4,
            not_null: false,
            dropped,
            generated: None,
            identity: None,
            missing: None,
        }
    }

    fn table(cols: Vec<ColDesc>) -> TableDesc {
        TableDesc {
            oid: 16400,
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

    /// Runs `f` with a live executor over an empty in-memory transaction and bootstrapped catalog.
    fn with_executor<T>(f: impl FnOnce(&mut Executor<'_, '_, '_>) -> T) -> T {
        let (_kv, env) = pgarbor_catalog::testing::bootstrapped().unwrap();
        let mut txn = MemTxn::new();
        let cell = Cell::new(1u64);
        let guc = NoGuc;
        let services = NoServices;
        let se = SessionEnv::new(env, IoConfig::default(), &guc, &services, &cell, 0, 0);
        let mut cx = ExecContext::new(&mut txn, se);
        let mut rt = Runtime::default();
        let mut ex = Executor::new(&mut rt, &mut cx);
        f(&mut ex)
    }

    #[test]
    fn assemble_places_supplied_values_and_nulls_dropped_slots() {
        let td = table(vec![col(1, "a", false), col(2, "gone", true), col(3, "c", false)]);
        let spec = InsertSpec { columns: vec![1, 3], defaults: Vec::new(), generated: Vec::new(), on_conflict: None };
        let out = with_executor(|ex| assemble_row(ex, &td, &spec, &[Datum::Int4(1), Datum::Int4(3)]));
        assert_eq!(out.unwrap(), vec![Datum::Int4(1), Datum::Null, Datum::Int4(3)]);
    }

    #[test]
    fn assemble_rejects_value_count_mismatch() {
        let td = table(vec![col(1, "a", false), col(2, "b", false)]);
        let spec = InsertSpec { columns: vec![1, 2], defaults: Vec::new(), generated: Vec::new(), on_conflict: None };
        let out = with_executor(|ex| assemble_row(ex, &td, &spec, &[Datum::Int4(1)]));
        assert!(out.is_err());
    }

    #[test]
    fn assemble_rejects_attnum_outside_relation() {
        let td = table(vec![col(1, "a", false)]);
        let spec = InsertSpec { columns: vec![9], defaults: Vec::new(), generated: Vec::new(), on_conflict: None };
        let out = with_executor(|ex| assemble_row(ex, &td, &spec, &[Datum::Int4(1)]));
        assert!(out.is_err());
    }

    #[test]
    #[ignore = "needs EXE-05"]
    fn defaults_fill_unsupplied_columns_and_generated_sees_full_row() {
        let td = table(vec![col(1, "a", false), col(2, "b", false), col(3, "c", false)]);
        let spec = InsertSpec {
            columns: vec![1],
            defaults: vec![(2, BoundExpr::constant(Datum::Int4(7), TypeId::INT4))],
            generated: vec![(3, BoundExpr::input_ref(1, TypeId::INT4))],
            on_conflict: None,
        };
        let out = with_executor(|ex| assemble_row(ex, &td, &spec, &[Datum::Int4(5)]));
        assert_eq!(out.unwrap(), vec![Datum::Int4(5), Datum::Int4(7), Datum::Int4(7)]);
    }

    #[test]
    #[ignore = "needs EXE-31, EXE-02, EXE-04, EXE-32"]
    fn insert_row_rejects_null_in_not_null_column() {
        let mut c = col(1, "a", false);
        c.not_null = true;
        let td = table(vec![c]);
        let out = with_executor(|ex| insert_row(ex, &td, vec![Datum::Null]));
        assert!(out.is_err());
    }
}
