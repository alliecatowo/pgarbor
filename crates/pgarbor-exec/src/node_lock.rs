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
use pgarbor_common::{PgError, Result};
use pgarbor_plan::plan::{LockRowsNode, PlanNode};

use crate::build::BuildCtx;
use crate::runtime::RowSource;

#[allow(unused_variables)]
pub fn build_lock_rows(node: &PlanNode, n: &LockRowsNode, b: &mut BuildCtx) -> Result<Box<dyn RowSource>> {
    Err(PgError::unimplemented("EXE-25", "LockRows node (node_lock::build_lock_rows)"))
}
