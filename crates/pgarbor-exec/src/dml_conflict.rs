//! `INSERT ... ON CONFLICT` (lease EXE-30, sonnet). PG: `nodeModifyTable.c` (`ExecOnConflictUpdate`,
//! `ExecCheckIndexConstraints`), `execIndexing.c` (`ExecCheckIndexConstraints`). Spec: plan.rs
//! `ConflictSpec` / `ConflictPlanAction`.
//!
//! ## `insert_or_resolve(ex, td, conflict, full_row, state)`
//! 1. Loop (at most 8 attempts; a concurrent inserter can win the race between probe and write):
//!    `index_maint::find_conflict(ex, td, &full_row, arbiters)` where `arbiters = None` if
//!    `conflict.any_unique` else `Some(&conflict.arbiter_indexes)`.
//! 2. No conflict: `dml_insert::insert_row`; if it fails with 23505 (lost a race against a concurrent
//!    inserter) retry the loop (the probe now finds the winner); success -> `Inserted`. Record the new
//!    rowid in `state.touched`.
//! 3. Conflict + `DoNothing` -> `Skipped` (not counted, no RETURNING row).
//! 4. Conflict + `DoUpdate{assignments, where_}`:
//!    * the conflicting row was already inserted/updated by THIS statement (`rowid` in
//!      `state.touched`) -> `error::on_conflict_affects_row_twice()` (21000);
//!    * evaluation row = `existing_row ++ excluded_row` (`2 * ncols` slots: the planner rewrote
//!      `t.c`/`excluded.c` to those slots). `existing_row` = the conflict's latest image; `excluded_row`
//!      = the row proposed for insertion (`full_row`);
//!    * `where_` (if any) FALSE/NULL -> `Skipped` (row neither inserted nor updated, locks kept);
//!    * new row = existing row with `assignments` (`(attnum, expr)` evaluated over the evaluation row)
//!      applied, then stored generated columns recomputed (`dml_update::recompute_generated`);
//!    * `dml_update::apply_update(ex, td, rowid, &existing, new_row, modified_cols)` (constraints,
//!      heap write, index update, FK hooks); `state.touched.insert(rowid)`; result `Updated`.
//! The row lock taken by `find_conflict` is released only at transaction end (PG keeps it too).
use pgarbor_common::{PgError, Result};
use pgarbor_plan::plan::ConflictSpec;
use pgarbor_types::Datum;
use std::collections::HashSet;

use crate::runtime::Executor;
use crate::tabledesc::TableDesc;

/// Statement-level conflict bookkeeping.
// CONTRACT
#[derive(Debug, Default)]
pub struct ConflictState {
    /// Row ids inserted or updated by the statement so far.
    pub touched: HashSet<u64>,
}

/// What happened to the proposed row.
// CONTRACT
#[derive(Clone, Debug, PartialEq)]
pub enum ConflictOutcome {
    Inserted { rowid: u64, row: Vec<Datum> },
    Updated { rowid: u64, row: Vec<Datum> },
    Skipped,
}

#[allow(unused_variables)]
pub fn insert_or_resolve(
    ex: &mut Executor,
    td: &TableDesc,
    conflict: &ConflictSpec,
    full_row: Vec<Datum>,
    state: &mut ConflictState,
) -> Result<ConflictOutcome> {
    Err(PgError::unimplemented("EXE-30", "ON CONFLICT resolution (dml_conflict::insert_or_resolve)"))
}
