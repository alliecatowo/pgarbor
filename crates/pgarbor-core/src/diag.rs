//! Diagnostics rows for `pg_stat_activity`, `pg_locks`, `pg_blocking_pids` and branch session
//! listings (TXN-12). Lease CORE-15. pgarbor-catalog/exec turn these into virtual relation rows;
//! all times are unix milliseconds (exec converts to timestamptz).
//!
//! Column semantics follow PG18 (`contracts/reference/pg18/catalog_attributes.csv` lists the exact
//! columns): fields we cannot know are None (rendered NULL).
use crate::engine::Engine;
use crate::types::SessionId;
use pgarbor_common::{PgError, Result};

// CONTRACT
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize)]
pub struct ActivityRow {
    pub datid: u32,
    pub datname: String,
    pub pid: i32,
    /// Always None in v1 (no parallel workers).
    pub leader_pid: Option<i32>,
    /// Role OID of `usename` (10 for postgres; resolved by the caller when other roles exist).
    pub usesysid: u32,
    pub usename: String,
    pub application_name: String,
    pub client_addr: Option<String>,
    pub client_hostname: Option<String>,
    /// -1 for Unix-socket/embedded, like PG.
    pub client_port: Option<i32>,
    pub backend_start_ms: u64,
    pub xact_start_ms: Option<u64>,
    pub query_start_ms: Option<u64>,
    pub state_change_ms: Option<u64>,
    pub wait_event_type: Option<String>,
    pub wait_event: Option<String>,
    /// `BackendState::as_pg_str`.
    pub state: Option<String>,
    pub backend_xid: Option<u64>,
    pub backend_xmin: Option<u64>,
    pub query_id: Option<i64>,
    pub query: String,
    /// `client backend`.
    pub backend_type: String,
    /// PGArbor extension column (not in pg_stat_activity; exposed by `pgarbor_stat_branch_sessions`).
    pub branch: String,
}

// CONTRACT
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize)]
pub struct LockRow {
    pub locktype: String,
    pub database: Option<u32>,
    pub relation: Option<u32>,
    pub page: Option<i32>,
    pub tuple: Option<i16>,
    pub virtualxid: Option<String>,
    pub transactionid: Option<u64>,
    pub classid: Option<u32>,
    pub objid: Option<u32>,
    pub objsubid: Option<i16>,
    /// `<pid>/<local txn id>` like PG's `3/45` (we use `<pid>/<txn id>`).
    pub virtualtransaction: Option<String>,
    pub pid: Option<i32>,
    pub mode: String,
    pub granted: bool,
    pub fastpath: bool,
    pub waitstart_ms: Option<u64>,
}

/// One row per registered session (`SessionRegistry::all`), visible to every session of the
/// engine (v1 has no privilege filtering; PG hides `query` of other users from non-superusers —
/// documented gap). wait_event_type/wait_event from `Activity::wait`; `backend_xid` from activity.
/// Rows for sessions of OTHER branches are included in `branch_sessions` but EXCLUDED from
/// `pg_stat_activity` (a branch is its own "cluster" from the client's point of view) — so this
/// function takes the viewing session's branch filter.
#[allow(unused_variables)]
pub fn activity_rows(engine: &Engine, branch_filter: Option<pgarbor_common::BranchId>) -> Result<Vec<ActivityRow>> {
    Err(PgError::unimplemented("CORE-15", "pg_stat_activity rows"))
}

/// `pg_locks` rows from `LockManager::snapshot()` (filtered to `branch_filter`'s tags):
/// relation → locktype `relation`, database, relation; key → locktype `tuple`, database, relation
/// = `LockTag::key_relation()`, page/tuple None; advisory → `advisory`, database, classid = key1,
/// objid = key2, objsubid = field4; object → `object`, classid/objid/objsubid. Plus, like PG, one
/// `virtualxid` ExclusiveLock row per session with an open transaction (virtualxid =
/// virtualtransaction = `<pid>/<txn>`), and one `transactionid` ExclusiveLock row per transaction
/// that has written (`backend_xid`).
#[allow(unused_variables)]
pub fn lock_rows(engine: &Engine, branch_filter: Option<pgarbor_common::BranchId>) -> Result<Vec<LockRow>> {
    Err(PgError::unimplemented("CORE-15", "pg_locks rows"))
}

/// `pg_blocking_pids(pid)` → `LockManager::blocking_sessions` as i32 pids.
#[allow(unused_variables)]
pub fn blocking_pids(engine: &Engine, pid: SessionId) -> Result<Vec<i32>> {
    Err(PgError::unimplemented("CORE-15", "pg_blocking_pids"))
}
