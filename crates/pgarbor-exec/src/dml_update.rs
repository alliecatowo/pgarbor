//! UPDATE row processing (lease EXE-28, sonnet, north-star). PG: `nodeModifyTable.c`
//! (`ExecUpdate`, `ExecUpdateAct`, EvalPlanQual), `execIndexing.c`. Spec: plan.rs `UpdateSpec`.
//!
//! ## Input and identity
//! The ModifyTable input row is `table row (ncols) ++ rowid ++ extra FROM columns`;
//! `rowid = input[rowid_slot]` (`Datum::Int8` -> u64). The first `ncols` slots are the old row image as
//! seen by the statement snapshot.
//!
//! ## `update_one` (one input row)
//! 1. `rowstore::lock_row(ex, td, rowid, ForNoKeyUpdate, Block)` (use `ForUpdate` if any assigned
//!    column is a key of a unique index or referenced by a foreign key). Outcomes:
//!    * `Gone` -> skip the row (concurrently deleted); returns `None`.
//!    * `Current(img)` -> old image `img`.
//!    * `Changed(latest)` (READ COMMITTED) -> EvalPlanQual per [`EpqPolicy`]: if `policy.quals` is `None`
//!      (joins / FROM) return the 40001 error `could not serialize access due to concurrent update`;
//!      else evaluate every qual over `latest` (`eval::eval_qual`); any false -> skip the row (`None`);
//!      otherwise rebuild the input row as `latest ++ rowid ++ <extra columns unchanged>` and continue
//!      with `latest` as the old image (assignments are re-evaluated against it).
//! 2. [`build_new_row`]: evaluate the assignments over the input row; apply to a copy of the old image;
//!    `SetToDefault{attnum}` that survives in an assignment expression is replaced by the evaluated
//!    column default (`ModifyTarget.meta.columns[attnum-1].default`, NULL if none); then
//!    [`recompute_generated`] for `spec.generated`.
//! 3. If the new row is identical to the old row (all slots `not_distinct`) the row is STILL counted and
//!    written (PG writes a new tuple version); skipping index maintenance is allowed.
//! 4. [`apply_update`]: `constraints::enforce` -> `rowstore::write_row` (same rowid) ->
//!    `index_maint::update_entries(.., modified_cols)` -> `fk::after_update`. Returns the new image.
//! A target row matched by SEVERAL input rows is updated once (first match): the caller
//! (`modify`) dedups by rowid before calling.
use pgarbor_common::{PgError, Result, SqlState};
use pgarbor_core::locks::LockMode;
use pgarbor_core::WaitPolicy;
use pgarbor_plan::plan::{ModifyTarget, UpdateSpec};
use pgarbor_sql::ir::{BoundExpr, ExprKind};
use pgarbor_types::Datum;

use crate::constraints;
use crate::error;
use crate::eval;
use crate::fk;
use crate::index_maint;
use crate::rowstore::{self, RowLock};
use crate::runtime::Executor;
use crate::tabledesc::TableDesc;

/// How to handle a concurrently changed row under READ COMMITTED (EvalPlanQual lite).
// CONTRACT
#[derive(Clone, Debug, Default)]
pub struct EpqPolicy {
    /// Qualifications to re-check against the latest row version (expressions over a TABLE row,
    /// `InputRef(attnum-1)`); `None` = EPQ unsupported for this plan shape (joins): 40001.
    pub quals: Option<Vec<BoundExpr>>,
}

pub fn update_one(
    ex: &mut Executor,
    td: &TableDesc,
    target: &ModifyTarget,
    spec: &UpdateSpec,
    policy: &EpqPolicy,
    input: &[Datum],
    rowid: u64,
) -> Result<Option<Vec<Datum>>> {
    let n = td.ncols();
    let assigned = assigned_attnums(spec);
    let mode = if touches_key(td, &assigned) { LockMode::ForUpdate } else { LockMode::ForNoKeyUpdate };

    // `eval_input` is `Some` only when EvalPlanQual rebuilt the input row from the latest image.
    let (old_row, eval_input): (Vec<Datum>, Option<Vec<Datum>>) =
        match rowstore::lock_row(ex, td, rowid, mode, WaitPolicy::Block)? {
            RowLock::Gone => return Ok(None),
            RowLock::Current(img) => (img, None),
            RowLock::NotAvailable => return Err(error::row_lock_not_available(&td.name)),
            RowLock::Changed(latest) => {
                let quals = match &policy.quals {
                    Some(q) => q,
                    None => return Err(serialization_failure()),
                };
                if latest.len() != n {
                    return Err(PgError::internal(format!(
                        "latest image of row {rowid} has {} values, table {} has {n} attributes",
                        latest.len(),
                        td.name
                    )));
                }
                for q in quals.iter() {
                    if !eval::eval_qual(ex, q, &latest)? {
                        return Ok(None);
                    }
                }
                // New input row = latest table image ++ the input's rowid and extra FROM slots.
                let mut rebuilt: Vec<Datum> = Vec::with_capacity(input.len().max(n));
                rebuilt.extend_from_slice(&latest);
                if input.len() > n {
                    rebuilt.extend_from_slice(&input[n..]);
                }
                (latest, Some(rebuilt))
            }
        };

    let eval_row: &[Datum] = match &eval_input {
        Some(v) => v.as_slice(),
        None => input,
    };
    let new_row = build_new_row(ex, td, target, spec, &old_row, eval_row)?;
    let stored = apply_update(ex, td, rowid, &old_row, new_row, Some(assigned.as_slice()))?;
    Ok(Some(stored))
}

pub fn build_new_row(
    ex: &mut Executor,
    td: &TableDesc,
    target: &ModifyTarget,
    spec: &UpdateSpec,
    old_row: &[Datum],
    input: &[Datum],
) -> Result<Vec<Datum>> {
    let n = td.ncols();
    if old_row.len() != n {
        return Err(PgError::internal(format!(
            "UPDATE old row has {} values, table {} has {n} attributes",
            old_row.len(),
            td.name
        )));
    }
    let mut row: Vec<Datum> = old_row.to_vec();
    // Every assignment is evaluated over the INPUT row (the old values), so order does not matter.
    for (attnum, expr) in spec.assignments.iter() {
        let idx = slot(*attnum, n)?;
        if td.cols[idx].dropped {
            return Err(PgError::internal(format!("UPDATE assigns dropped attribute {attnum}")));
        }
        let value = assignment_value(ex, target, expr, input)?;
        row[idx] = value;
    }
    recompute_generated(ex, &spec.generated, &mut row)?;
    Ok(row)
}

/// Recompute stored generated columns `(attnum, expr)` over the (new) row, in order.
pub fn recompute_generated(ex: &mut Executor, generated: &[(i16, BoundExpr)], row: &mut Vec<Datum>) -> Result<()> {
    for (attnum, expr) in generated.iter() {
        let idx = slot(*attnum, row.len())?;
        let value = eval::eval(ex, expr, row.as_slice())?;
        row[idx] = value;
    }
    Ok(())
}

/// Low-level in-place update used by UPDATE, ON CONFLICT DO UPDATE and foreign-key actions (module
/// docs step 4). `modified_cols`: assigned attnums when known (index maintenance hint).
///
/// Row counting is NOT done here: the ModifyTable driver counts top-level rows only (PG does not count
/// cascaded or conflict-action changes in the command's row count for FK actions).
pub fn apply_update(
    ex: &mut Executor,
    td: &TableDesc,
    rowid: u64,
    old_row: &[Datum],
    new_row: Vec<Datum>,
    modified_cols: Option<&[i16]>,
) -> Result<Vec<Datum>> {
    // A partition's new row must stay inside its bound (PG `ExecUpdateAct`: before the constraints).
    if crate::partition::is_partition(&*ex.cx, td.oid)? && !crate::partition::check_partition_constraint(ex, td, &new_row)? {
        return Err(crate::partition::partition_constraint_error(ex, td, &new_row));
    }
    constraints::enforce(ex, td, &new_row)?;
    rowstore::write_row(ex, td, rowid, &new_row)?;
    index_maint::update_entries(ex, td, rowid, old_row, &new_row, modified_cols)?;
    fk::after_update(ex, td, old_row, &new_row)?;
    Ok(new_row)
}

/// 40001 for an EvalPlanQual situation that cannot be re-checked (joins / FROM sources).
fn serialization_failure() -> PgError {
    PgError::new(SqlState::SERIALIZATION_FAILURE, "could not serialize access due to concurrent update")
}

/// Attnums written by the UPDATE: assignments plus stored generated columns (recomputed).
fn assigned_attnums(spec: &UpdateSpec) -> Vec<i16> {
    let mut out: Vec<i16> = Vec::with_capacity(spec.assignments.len() + spec.generated.len());
    out.extend(spec.assignments.iter().map(|(a, _)| *a));
    out.extend(spec.generated.iter().map(|(a, _)| *a));
    out
}

/// True when the UPDATE may change a key: a column of a unique index (expression keys are treated
/// conservatively as keys) or a column referenced by a foreign key. Such updates take `FOR UPDATE`
/// instead of `FOR NO KEY UPDATE` (PG `ExecUpdate`, `RelationGetIndexAttrBitmap`).
fn touches_key(td: &TableDesc, assigned: &[i16]) -> bool {
    let unique_key_hit = td.unique_indexes().any(|idx| {
        idx.keys.iter().any(|k| k.expr.is_some() || k.column.map_or(false, |c| assigned.contains(&c)))
    });
    if unique_key_hit {
        return true;
    }
    td.fks_in.iter().any(|f| f.ref_cols.iter().any(|c| assigned.contains(c)))
}

/// Map a 1-based attnum to a slot of a row with `n` slots.
fn slot(attnum: i16, n: usize) -> Result<usize> {
    if attnum < 1 || attnum as usize > n {
        return Err(PgError::internal(format!("attribute number {attnum} out of range for {n} attributes")));
    }
    Ok(attnum as usize - 1)
}

/// Value of one SET expression over the input row. A `DEFAULT` (`SetToDefault`) that survived planning
/// is replaced by the column default, evaluated once per row (PG evaluates defaults per row).
fn assignment_value(ex: &mut Executor, target: &ModifyTarget, expr: &BoundExpr, input: &[Datum]) -> Result<Datum> {
    let has_default = expr.any(&mut |e: &BoundExpr| matches!(e.kind, ExprKind::SetToDefault { .. }));
    if has_default {
        let resolved = expr.clone().transform(&mut |e: BoundExpr| -> Result<BoundExpr> {
            let default_attnum = if let ExprKind::SetToDefault { attnum } = e.kind { Some(attnum) } else { None };
            match default_attnum {
                Some(attnum) => {
                    let value = column_default(ex, target, attnum)?;
                    Ok(BoundExpr::constant(value, e.ty))
                }
                None => Ok(e),
            }
        })?;
        return eval::eval(ex, &resolved, input);
    }
    eval::eval(ex, expr, input)
}

/// Evaluated column default of `attnum` (NULL when the column has none). Defaults are already coerced
/// to the column type when the DDL stores them.
fn column_default(ex: &mut Executor, target: &ModifyTarget, attnum: i16) -> Result<Datum> {
    let col = target
        .meta
        .column(attnum)
        .ok_or_else(|| PgError::internal(format!("DEFAULT refers to unknown attribute {attnum}")))?;
    match &col.default {
        None => Ok(Datum::Null),
        Some(d) => {
            let empty: &[Datum] = &[];
            eval::eval(ex, d, empty)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pgarbor_types::TypeId;

    #[test]
    fn slot_maps_attnum_to_index() {
        assert_eq!(slot(1, 3).unwrap(), 0);
        assert_eq!(slot(3, 3).unwrap(), 2);
    }

    #[test]
    fn slot_rejects_out_of_range_attnums() {
        assert!(slot(0, 3).is_err());
        assert!(slot(-1, 3).is_err());
        assert!(slot(4, 3).is_err());
    }

    #[test]
    fn assigned_attnums_includes_generated_columns() {
        let spec = UpdateSpec {
            assignments: vec![(2, BoundExpr::null(TypeId::of(0)))],
            generated: vec![(4, BoundExpr::null(TypeId::of(0)))],
        };
        assert_eq!(assigned_attnums(&spec), vec![2, 4]);
    }

    #[test]
    fn serialization_failure_message_and_state() {
        let e = serialization_failure();
        assert!(e.sqlstate == SqlState::SERIALIZATION_FAILURE);
        assert_eq!(e.message, "could not serialize access due to concurrent update");
    }
}
