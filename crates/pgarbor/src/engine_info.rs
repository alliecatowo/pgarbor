//! `EngineServices` for the executor (lease API-14): activity / locks / prepared statements /
//! cursors / notifications / cancel, implemented over `pgarbor_core::{diag, Engine}` and the
//! session's own tables. Built per statement by `Session::with_exec`.
//!
//! * `backend_pid()`: `shared.id`. `activity()`: `core::diag::activity_rows(engine, Some(branch))`
//!   (only sessions of the viewer's branch, core amendment 02) mapped to `catalog::session::ActivityRow`
//!   (`*_ms` unix-millisecond fields -> timestamptz us since 2000-01-01: `ms * 1000 - 946_684_800_000_000`;
//!   `client_addr` text kept; ids as given). `locks()`: `diag::lock_rows` -> `LockRow` (`virtualxid`,
//!   `mode`, `granted`... one-to-one). `blocking_pids(pid)`: `diag::blocking_pids`.
//! * `prepared_statements()`: one `PreparedStatementRow` per entry of the `PreparedStore` (named ones; the
//!   unnamed statement is not listed): `name`, `statement` text, `prepare_time`, `parameter_types` OIDs,
//!   `result_types` (None when no result), `from_sql`, plan counters.
//! * `cursors()`: portals created by `DECLARE` (none in v1) -> empty. `server_start_time()`: the
//!   database's open time. `config_load_time()`: same.
//! * `current_query()`: the activity record's query text. `database_name()`: the session's database.
//! * `cancel_backend(pid, terminate)`: `engine.cancel_backend/terminate_backend(SessionId(pid))`; other
//!   sessions only (PG requires the same role or `pg_signal_backend` membership; v1: superuser check via
//!   the session's current role).
//! * `notify(channel, payload)`: `listen.queue_notify(..)` (delivered at commit).
//!   `listening_channels()`: `notify.listening(pid)` including channels with pending LISTEN ops.
//! * `relation_size(oid, kind)`: not available in v1 (inherits the 0A000 default).
use std::cell::RefCell;

use pgarbor_catalog::session::{ActivityRow, CursorRow, LockRow, PreparedStatementRow, SizeKind};
use pgarbor_common::{Oid, PgError, Result};
use pgarbor_core::SessionShared;
use pgarbor_exec::EngineServices;
use std::sync::Arc;

use crate::database::Database;
use crate::notify::ListenState;
use crate::prepared::PreparedStore;

/// The services object (borrows session parts; module docs).
pub struct SessionServices<'a> {
    pub db: &'a Database,
    pub shared: &'a Arc<SessionShared>,
    pub prepared: &'a PreparedStore,
    pub listen: &'a RefCell<ListenState>,
    pub database_name: String,
}

impl EngineServices for SessionServices<'_> {
    fn backend_pid(&self) -> i32 {
        self.shared.id.0
    }
    fn activity(&self) -> Result<Vec<ActivityRow>> {
        Err(PgError::unimplemented("API-14", "pg_stat_activity rows (SessionServices::activity)"))
    }
    fn locks(&self) -> Result<Vec<LockRow>> {
        Err(PgError::unimplemented("API-14", "pg_locks rows (SessionServices::locks)"))
    }
    fn prepared_statements(&self) -> Result<Vec<PreparedStatementRow>> {
        Err(PgError::unimplemented("API-14", "pg_prepared_statements rows (SessionServices::prepared_statements)"))
    }
    fn cursors(&self) -> Result<Vec<CursorRow>> {
        Ok(Vec::new())
    }
    #[allow(unused_variables)]
    fn blocking_pids(&self, pid: i32) -> Result<Vec<i32>> {
        Err(PgError::unimplemented("API-14", "pg_blocking_pids (SessionServices::blocking_pids)"))
    }
    fn server_start_time(&self) -> i64 {
        self.db.inner.start_time_us
    }
    fn config_load_time(&self) -> i64 {
        self.db.inner.start_time_us
    }
    fn current_query(&self) -> Option<String> {
        Some(self.shared.activity().query)
    }
    #[allow(unused_variables)]
    fn relation_size(&self, oid: Oid, kind: SizeKind) -> Result<i64> {
        Err(PgError::new(
            pgarbor_common::SqlState::FEATURE_NOT_SUPPORTED,
            "relation sizes are not available in this session",
        ))
    }
    #[allow(unused_variables)]
    fn cancel_backend(&self, pid: i32, terminate: bool) -> Result<bool> {
        Err(PgError::unimplemented("API-14", "pg_cancel_backend (SessionServices::cancel_backend)"))
    }
    #[allow(unused_variables)]
    fn notify(&self, channel: &str, payload: &str) -> Result<()> {
        Err(PgError::unimplemented("API-14", "pg_notify (SessionServices::notify)"))
    }
    fn listening_channels(&self) -> Vec<String> {
        self.db.inner.notify.listening(self.shared.id.0)
    }
    fn database_name(&self) -> String {
        self.database_name.clone()
    }
}
