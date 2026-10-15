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
use pgarbor_catalog::model::GeneratedKind;
use pgarbor_common::{Oid, PgError, Result, SqlState};
use pgarbor_plan::plan::{ConflictPlanAction, ConflictSpec};
use pgarbor_types::Datum;
use std::collections::HashSet;

use crate::dml_insert;
use crate::dml_update;
use crate::error;
use crate::eval;
use crate::index_maint::{self, ConflictRow};
use crate::runtime::Executor;
use crate::tabledesc::TableDesc;

/// Attempts before giving up when concurrent inserters keep winning the probe/insert race (module docs).
const MAX_ATTEMPTS: usize = 8;

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

/// Insert one proposed row with ON CONFLICT semantics (module docs, steps 1-4).
pub fn insert_or_resolve(
    ex: &mut Executor,
    td: &TableDesc,
    conflict: &ConflictSpec,
    full_row: Vec<Datum>,
    state: &mut ConflictState,
) -> Result<ConflictOutcome> {
    let arbiters: Option<&[Oid]> = if conflict.any_unique { None } else { Some(conflict.arbiter_indexes.as_slice()) };
    let mut last_violation: Option<PgError> = None;
    for _ in 0..MAX_ATTEMPTS {
        if let Some(found) = index_maint::find_conflict(ex, td, &full_row, arbiters)? {
            return resolve_conflict(ex, td, conflict, &full_row, found, state);
        }
        match dml_insert::insert_row(ex, td, full_row.clone()) {
            Ok(rowid) => {
                state.touched.insert(rowid);
                return Ok(ConflictOutcome::Inserted { rowid, row: full_row });
            }
            // A concurrent inserter won the race after our probe: probe again, it now finds the winner.
            Err(e) if e.sqlstate == SqlState::UNIQUE_VIOLATION => last_violation = Some(e),
            Err(e) => return Err(e),
        }
    }
    Err(last_violation.unwrap_or_else(|| PgError::internal("ON CONFLICT: no insertion attempt was made")))
}

/// Resolve a conflict with a live row: DO NOTHING skips, DO UPDATE applies the assignments (module
/// docs, step 4).
fn resolve_conflict(
    ex: &mut Executor,
    td: &TableDesc,
    conflict: &ConflictSpec,
    full_row: &[Datum],
    found: ConflictRow,
    state: &mut ConflictState,
) -> Result<ConflictOutcome> {
    let (assignments, where_) = match &conflict.action {
        ConflictPlanAction::DoNothing => return Ok(ConflictOutcome::Skipped),
        ConflictPlanAction::DoUpdate { assignments, where_ } => (assignments, where_),
    };
    let ConflictRow { rowid, row: existing, .. } = found;
    if state.touched.contains(&rowid) {
        return Err(error::on_conflict_affects_row_twice());
    }
    let ncols = td.ncols();
    if existing.len() != ncols || full_row.len() != ncols {
        return Err(PgError::internal(format!(
            "ON CONFLICT row width mismatch: table has {ncols} columns, existing row has {}, proposed row has {}",
            existing.len(),
            full_row.len()
        )));
    }
    let eval_row = build_eval_row(&existing, full_row);
    if let Some(w) = where_ {
        if !eval::eval_qual(ex, w, &eval_row)? {
            return Ok(ConflictOutcome::Skipped);
        }
    }
    // The conflict spec carries no generated-column expressions, so stored generated columns cannot
    // be recomputed here. Fail explicitly rather than store stale generated values.
    if has_stored_generated(td) {
        return Err(PgError::unimplemented(
            "EXE-30",
            "ON CONFLICT DO UPDATE on a table with stored generated columns",
        ));
    }
    let mut new_row = existing.clone();
    let mut modified: Vec<i16> = Vec::with_capacity(assignments.len());
    for (attnum, expr) in assignments.iter() {
        let value = eval::eval(ex, expr, &eval_row)?;
        let slot = slot_index(ncols, *attnum)?;
        new_row[slot] = value;
        modified.push(*attnum);
    }
    let updated = dml_update::apply_update(ex, td, rowid, &existing, new_row, Some(modified.as_slice()))?;
    state.touched.insert(rowid);
    Ok(ConflictOutcome::Updated { rowid, row: updated })
}

/// Evaluation row for DO UPDATE: `existing ++ excluded` (planner layout, plan.rs `ConflictPlanAction`).
fn build_eval_row(existing: &[Datum], excluded: &[Datum]) -> Vec<Datum> {
    let mut row = Vec::with_capacity(existing.len() + excluded.len());
    row.extend_from_slice(existing);
    row.extend_from_slice(excluded);
    row
}

/// Slot index (`attnum - 1`) of an assignment target, checked against the table width.
fn slot_index(ncols: usize, attnum: i16) -> Result<usize> {
    usize::try_from(attnum)
        .ok()
        .and_then(|a| a.checked_sub(1))
        .filter(|&i| i < ncols)
        .ok_or_else(|| PgError::internal(format!("ON CONFLICT assignment targets invalid attribute number {attnum}")))
}

fn has_stored_generated(td: &TableDesc) -> bool {
    td.cols.iter().any(|c| !c.dropped && matches!(c.generated, Some(GeneratedKind::Stored)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn eval_row_is_existing_then_excluded() {
        let existing = vec![Datum::Int4(1), Datum::Int4(10)];
        let excluded = vec![Datum::Int4(1), Datum::Int4(5)];
        let row = build_eval_row(&existing, &excluded);
        assert_eq!(row, vec![Datum::Int4(1), Datum::Int4(10), Datum::Int4(1), Datum::Int4(5)]);
    }

    #[test]
    fn slot_index_maps_attnum_to_slot() {
        assert_eq!(slot_index(3, 1).ok(), Some(0));
        assert_eq!(slot_index(3, 3).ok(), Some(2));
        assert!(slot_index(3, 0).is_err());
        assert!(slot_index(3, -1).is_err());
        assert!(slot_index(3, 4).is_err());
    }

    #[test]
    fn touched_state_starts_empty() {
        let state = ConflictState::default();
        assert!(state.touched.is_empty());
    }
}
