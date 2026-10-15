//! Utility statements executed by the executor that need no session state (lease EXE-39): LOCK TABLE,
//! DO, CALL, maintenance commands. (Transaction control, SET/SHOW, PREPARE/EXECUTE, LISTEN/NOTIFY,
//! DISCARD are SESSION statements handled by the `pgarbor` crate and never reach the executor.)
//!
//! * [`exec_lock`] (`BoundLock{tables, mode, nowait}`): `perm::require_relation_privilege` first (see
//!   `perm.rs`), then for each table `ExecTxn::lock_relation(oid,
//!   core LockMode, policy)` with the mode mapped one-to-one from `pgarbor_sql::ir::LockMode`
//!   (`AccessShare`... `AccessExclusive`) and `WaitPolicy::NoWait` if `nowait` else `Block`; not
//!   acquired -> `error::relation_lock_not_available(name)` (55P03 `could not obtain lock on relation
//!   "t"`). Tag `LOCK TABLE`. (The "can only be used in transaction blocks" check is the session's.)
//! * [`exec_do`] / [`exec_call`]: PGArbor does not execute procedural code in v1: 0A000
//!   (`PgError::unimplemented("EXE-39", ...)`), tags would be `DO` / `CALL`.
//! * [`exec_maintenance`] (`MaintenanceKind`, `targets` = table OIDs, empty = all):
//!   - `Reindex`: for each target table (or the table of each target index) delete the entry data of
//!     its btree indexes (`rowstore::delete_relation_data(index)`) and rebuild them
//!     (`index_maint::build_index`); tag `REINDEX`;
//!   - `Vacuum`, `Analyze`, `Cluster`, `Checkpoint`: accepted no-ops (no dead tuples exist, no
//!     statistics are kept, durability is per commit); tags `VACUUM`, `ANALYZE`, `CLUSTER`, `CHECKPOINT`.
use pgarbor_common::{Oid, PgError, Result};
use pgarbor_sql::ir::{BoundCall, BoundDo, BoundLock, MaintenanceKind};

use crate::context::ExecContext;
use crate::result::ExecResult;

#[allow(unused_variables)]
pub fn exec_lock(cx: &mut ExecContext, l: &BoundLock) -> Result<ExecResult> {
    Err(PgError::unimplemented("EXE-39", "LOCK TABLE (utility::exec_lock)"))
}

#[allow(unused_variables)]
pub fn exec_do(cx: &mut ExecContext, d: &BoundDo) -> Result<ExecResult> {
    Err(PgError::unimplemented("EXE-39", "DO blocks (utility::exec_do)"))
}

#[allow(unused_variables)]
pub fn exec_call(cx: &mut ExecContext, c: &BoundCall) -> Result<ExecResult> {
    Err(PgError::unimplemented("EXE-39", "CALL (utility::exec_call)"))
}

#[allow(unused_variables)]
pub fn exec_maintenance(cx: &mut ExecContext, kind: MaintenanceKind, targets: &[Oid]) -> Result<ExecResult> {
    Err(PgError::unimplemented("EXE-39", "maintenance commands (utility::exec_maintenance)"))
}
