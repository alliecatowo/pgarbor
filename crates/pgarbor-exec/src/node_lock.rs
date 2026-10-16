//! `LockRows` (`SELECT ... FOR UPDATE / NO KEY UPDATE / SHARE / KEY SHARE [NOWAIT | SKIP LOCKED]`)
//! (lease EXE-25, sonnet). PG: `nodeLockRows.c`. Layout: plan.rs `LockRowsNode`.
//!
//! For each input row and each `LockTarget{table_oid, rowid_slot}`: skip targets whose rowid slot is
//! NULL (outer-joined row without a match). Otherwise lock the heap row with
//! `rowstore::lock_row(ex, td, rowid, mode, policy)` where
//! * `mode`: `ForUpdate`, `ForNoKeyUpdate`, `ForShare`, `ForKeyShare` from `LockStrength`
//!   (`pgarbor_core::locks::LockMode`), policy: `Block` / `NoWait` / `SkipLocked` from `LockWait`;
//! * `RowLock::Gone` -> the row was deleted concurrently: drop the whole output row;
//! * `RowLock::Current(_)` -> keep the row;
//! * `RowLock::Changed(latest)` (READ COMMITTED) -> EvalPlanQual lite: re-fetch is only valid for
//!   single-table queries; replace the target's slots (the first `ncols` slots of that table's range in
//!   the input row, located by `rowid_slot - ncols`) with `latest`, keep the row (the query's WHERE is
//!   NOT re-evaluated in v1; documented known gap). For a multi-target (join) row with `Changed` raise
//!   40001 `could not serialize access due to concurrent update`.
//! * `RowLock::NotAvailable`: `SkipLocked` -> drop the row; `NoWait` -> 55P03
//!   `error::row_lock_not_available(table_name)` (message `could not obtain lock on row in relation
//!   "t"`).
//! Rows are locked as they are pulled (lazy); output row = input row (with refreshed slots).
use pgarbor_common::{PgError, Result, SqlState};
use pgarbor_core::locks::LockMode;
use pgarbor_core::WaitPolicy;
use pgarbor_plan::plan::{LockRowsNode, LockStrength, LockWait, PlanNode};
use pgarbor_types::Datum;
use std::rc::Rc;

use pgarbor_catalog::model::common::acl_mode;

use crate::build::BuildCtx;
use crate::error::row_lock_not_available;
use crate::perm;
use crate::rowstore::{self, RowLock};
use crate::runtime::{Executor, Row, RowSource};
use crate::tabledesc::TableDesc;

/// One locked table, resolved at build time.
struct LockTargetRt {
    td: Rc<TableDesc>,
    /// Input slot holding the table's rowid.
    rowid_slot: usize,
    /// Number of attribute slots of the table (`td.ncols()`).
    ncols: usize,
    /// First input slot of the table's columns (`rowid_slot - ncols`).
    start: usize,
}

/// The `LockRows` operator (module docs). Owns copies of everything it needs.
struct LockRowsOp {
    input: Box<dyn RowSource>,
    strength: LockStrength,
    wait: LockWait,
    targets: Vec<LockTargetRt>,
}

pub fn build_lock_rows(node: &PlanNode, n: &LockRowsNode, b: &mut BuildCtx) -> Result<Box<dyn RowSource>> {
    let _ = node;
    let input = b.child(&n.input)?;
    // PG `markQueryForLocking` sets ACL_SELECT_FOR_UPDATE (= ACL_UPDATE) on every locked relation,
    // whatever the strength, so each locked table needs UPDATE (table-level, 42501 otherwise).
    for t in &n.targets {
        perm::require_relation_privilege(&b.cx.se, t.table_oid, acl_mode::UPDATE, &[])?;
    }
    let mut targets = Vec::with_capacity(n.targets.len());
    for t in &n.targets {
        let td = b.table_desc(t.table_oid)?;
        let ncols = td.ncols();
        let rowid_slot = t.rowid_slot as usize;
        if rowid_slot < ncols {
            return Err(PgError::internal(format!(
                "LockRows target {} has rowid slot {} before its {} column slots",
                t.table_oid, rowid_slot, ncols
            )));
        }
        // PG `ExecCheckXactReadOnly`: a row lock needs write permission (ACL_UPDATE) on a non-temp relation.
        if !td.schema.starts_with("pg_temp") {
            let what = match n.strength {
                LockStrength::Update => "SELECT FOR UPDATE",
                LockStrength::NoKeyUpdate => "SELECT FOR NO KEY UPDATE",
                LockStrength::Share => "SELECT FOR SHARE",
                LockStrength::KeyShare => "SELECT FOR KEY SHARE",
            };
            b.cx.txn.check_writable(what)?;
        }
        targets.push(LockTargetRt { td, rowid_slot, ncols, start: rowid_slot - ncols });
    }
    Ok(Box::new(LockRowsOp { input, strength: n.strength, wait: n.wait, targets }))
}

impl RowSource for LockRowsOp {
    fn next(&mut self, ex: &mut Executor) -> Result<Option<Row>> {
        loop {
            let mut row = match self.input.next(ex)? {
                Some(r) => r,
                None => return Ok(None),
            };
            if lock_row_targets(ex, &self.targets, self.strength, self.wait, &mut row)? {
                return Ok(Some(row));
            }
        }
    }

    fn rescan(&mut self, ex: &mut Executor, params: &[Datum]) -> Result<()> {
        self.input.rescan(ex, params)
    }
}

/// Lock every target of one input row (module docs). Returns `Ok(true)` when the row is kept (with
/// refreshed slots) and `Ok(false)` when it must be dropped (deleted concurrently or skipped).
fn lock_row_targets(
    ex: &mut Executor,
    targets: &[LockTargetRt],
    strength: LockStrength,
    wait: LockWait,
    row: &mut Row,
) -> Result<bool> {
    let multi = targets.len() > 1;
    for t in targets {
        let rowid = match rowid_of(row, t.rowid_slot)? {
            Some(rowid) => rowid,
            // Outer-joined side without a match: nothing to lock.
            None => continue,
        };
        let mode = lock_mode(strength);
        let policy = wait_policy(wait);
        match rowstore::lock_row(ex, &t.td, rowid, mode, policy)? {
            RowLock::Gone => return Ok(false),
            RowLock::Current(_) => {}
            RowLock::Changed(latest) => {
                if multi {
                    return Err(concurrent_update_error());
                }
                replace_table_slots(row, t.start, t.ncols, latest)?;
            }
            RowLock::NotAvailable => match wait {
                LockWait::SkipLocked => return Ok(false),
                LockWait::NoWait | LockWait::Block => return Err(row_lock_not_available(&t.td.name)),
            },
        }
    }
    Ok(true)
}

/// Map the SQL lock strength to the core lock mode (`FOR UPDATE` .. `FOR KEY SHARE`).
fn lock_mode(strength: LockStrength) -> LockMode {
    match strength {
        LockStrength::Update => LockMode::ForUpdate,
        LockStrength::NoKeyUpdate => LockMode::ForNoKeyUpdate,
        LockStrength::Share => LockMode::ForShare,
        LockStrength::KeyShare => LockMode::ForKeyShare,
    }
}

/// Map `NOWAIT` / `SKIP LOCKED` / plain blocking to the core wait policy.
fn wait_policy(wait: LockWait) -> WaitPolicy {
    match wait {
        LockWait::Block => WaitPolicy::Block,
        LockWait::NoWait => WaitPolicy::NoWait,
        LockWait::SkipLocked => WaitPolicy::SkipLocked,
    }
}

/// Rowid of the target in `slot`: `Ok(None)` for a NULL slot, the u64 rowid for an `Int8` slot
/// (scans store the rowid bit-for-bit as `i64`), XX000 for anything else (executor bug).
fn rowid_of(row: &[Datum], slot: usize) -> Result<Option<u64>> {
    match row.get(slot) {
        Some(Datum::Null) => Ok(None),
        Some(Datum::Int8(v)) => Ok(Some(*v as u64)),
        Some(_) => Err(PgError::internal(format!("LockRows rowid slot {slot} does not hold a rowid"))),
        None => Err(PgError::internal(format!(
            "LockRows rowid slot {slot} is outside the input row of {} slots",
            row.len()
        ))),
    }
}

/// Overwrite the `ncols` slots starting at `start` with the latest committed image of the table
/// (EvalPlanQual lite). The rowid slot is untouched.
fn replace_table_slots(row: &mut Row, start: usize, ncols: usize, latest: Vec<Datum>) -> Result<()> {
    if latest.len() != ncols {
        return Err(PgError::internal(format!(
            "LockRows latest row image has {} values, expected {}",
            latest.len(),
            ncols
        )));
    }
    if start + ncols > row.len() {
        return Err(PgError::internal(format!(
            "LockRows table slots {}..{} are outside the input row of {} slots",
            start,
            start + ncols,
            row.len()
        )));
    }
    for (i, value) in latest.into_iter().enumerate() {
        row[start + i] = value;
    }
    Ok(())
}

/// 40001 raised when a join row has a concurrently changed target (module docs; PG would re-run
/// the join, which is a documented v1 gap).
fn concurrent_update_error() -> PgError {
    PgError::new(SqlState::SERIALIZATION_FAILURE, "could not serialize access due to concurrent update")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strength_maps_to_lock_mode() {
        assert!(matches!(lock_mode(LockStrength::Update), LockMode::ForUpdate));
        assert!(matches!(lock_mode(LockStrength::NoKeyUpdate), LockMode::ForNoKeyUpdate));
        assert!(matches!(lock_mode(LockStrength::Share), LockMode::ForShare));
        assert!(matches!(lock_mode(LockStrength::KeyShare), LockMode::ForKeyShare));
    }

    #[test]
    fn wait_maps_to_policy() {
        assert!(matches!(wait_policy(LockWait::Block), WaitPolicy::Block));
        assert!(matches!(wait_policy(LockWait::NoWait), WaitPolicy::NoWait));
        assert!(matches!(wait_policy(LockWait::SkipLocked), WaitPolicy::SkipLocked));
    }

    #[test]
    fn null_rowid_slot_is_skipped() {
        let row = vec![Datum::Int4(1), Datum::Null];
        assert_eq!(rowid_of(&row, 1).unwrap(), None);
    }

    #[test]
    fn int8_rowid_slot_is_read_bit_for_bit() {
        let row = vec![Datum::Int4(1), Datum::Int8(-1)];
        assert_eq!(rowid_of(&row, 1).unwrap(), Some(u64::MAX));
    }

    #[test]
    fn bad_rowid_slots_are_internal_errors() {
        let row = vec![Datum::Int4(1), Datum::Text("x".to_string())];
        assert_eq!(rowid_of(&row, 1).unwrap_err().sqlstate, SqlState::INTERNAL_ERROR);
        assert_eq!(rowid_of(&row, 5).unwrap_err().sqlstate, SqlState::INTERNAL_ERROR);
    }

    #[test]
    fn replace_table_slots_only_touches_the_table_range() {
        // Input row: [a, b (table of 2 cols at start 0)] [x (other table col)] [rowid]
        let mut row = vec![Datum::Int4(1), Datum::Int4(2), Datum::Int4(9), Datum::Int8(7)];
        replace_table_slots(&mut row, 0, 2, vec![Datum::Int4(10), Datum::Int4(20)]).unwrap();
        assert_eq!(
            row,
            vec![Datum::Int4(10), Datum::Int4(20), Datum::Int4(9), Datum::Int8(7)]
        );
    }

    #[test]
    fn replace_table_slots_rejects_width_mismatch() {
        let mut row = vec![Datum::Int4(1), Datum::Int8(7)];
        let err = replace_table_slots(&mut row, 0, 1, vec![Datum::Int4(1), Datum::Int4(2)]).unwrap_err();
        assert_eq!(err.sqlstate, SqlState::INTERNAL_ERROR);
        assert_eq!(row, vec![Datum::Int4(1), Datum::Int8(7)]);
    }

    #[test]
    fn concurrent_update_is_40001() {
        let e = concurrent_update_error();
        assert_eq!(e.sqlstate, SqlState::SERIALIZATION_FAILURE);
        assert_eq!(e.message, "could not serialize access due to concurrent update");
    }

    #[test]
    fn nowait_message_is_pg_exact() {
        let e = row_lock_not_available("t");
        assert_eq!(e.sqlstate, SqlState::LOCK_NOT_AVAILABLE);
        assert_eq!(e.message, "could not obtain lock on row in relation \"t\"");
    }
}
