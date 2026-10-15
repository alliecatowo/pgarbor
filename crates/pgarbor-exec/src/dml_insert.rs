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

use crate::dml_conflict::ConflictState;
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

#[allow(unused_variables)]
pub fn assemble_row(ex: &mut Executor, td: &TableDesc, spec: &InsertSpec, input: &[Datum]) -> Result<Vec<Datum>> {
    Err(PgError::unimplemented("EXE-27", "INSERT row assembly (dml_insert::assemble_row)"))
}

#[allow(unused_variables)]
pub fn insert_row(ex: &mut Executor, td: &TableDesc, row: Vec<Datum>) -> Result<u64> {
    Err(PgError::unimplemented("EXE-27", "row insertion (dml_insert::insert_row)"))
}

#[allow(unused_variables)]
pub fn insert_input_row(
    ex: &mut Executor,
    td: &TableDesc,
    spec: &InsertSpec,
    input: &[Datum],
    conflict_state: &mut ConflictState,
) -> Result<Option<InsertedRow>> {
    Err(PgError::unimplemented("EXE-27", "INSERT input row (dml_insert::insert_input_row)"))
}
