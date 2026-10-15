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
use pgarbor_common::{Oid, PgError, Result, SqlState};
use pgarbor_core::{diag, Engine, SessionId, SessionShared};
use pgarbor_exec::EngineServices;
use std::sync::Arc;

use crate::database::Database;
use crate::notify::ListenState;
use crate::prepared::{PreparedStatement, PreparedStore};

/// The services object (borrows session parts; module docs).
pub struct SessionServices<'a> {
    pub db: &'a Database,
    pub shared: &'a Arc<SessionShared>,
    pub prepared: &'a PreparedStore,
    pub listen: &'a RefCell<ListenState>,
    pub database_name: String,
}

/// PostgreSQL epoch (2000-01-01 00:00:00 UTC) in unix microseconds.
const PG_EPOCH_UNIX_US: i64 = 946_684_800_000_000;

/// Role name of the bootstrap superuser (`pg_authid` oid 10). Every v1 session connects as it unless
/// the wire layer says otherwise, so a session with this name stands in for "current role is superuser".
const BOOTSTRAP_SUPERUSER: &str = "postgres";

/// Unix milliseconds (core diagnostics) -> `timestamptz` microseconds since 2000-01-01.
fn unix_ms_to_timestamptz(ms: u64) -> i64 {
    let ms = i64::try_from(ms).unwrap_or(i64::MAX / 1000);
    ms.saturating_mul(1000).saturating_sub(PG_EPOCH_UNIX_US)
}

/// One `pg_stat_activity` row from the core diagnostics row (field-for-field, times converted).
fn activity_from_diag(r: pgarbor_core::diag::ActivityRow) -> ActivityRow {
    ActivityRow {
        datid: r.datid,
        datname: r.datname,
        pid: r.pid,
        leader_pid: r.leader_pid,
        usesysid: r.usesysid,
        usename: r.usename,
        application_name: r.application_name,
        client_addr: r.client_addr,
        client_hostname: r.client_hostname,
        client_port: r.client_port,
        backend_start: Some(unix_ms_to_timestamptz(r.backend_start_ms)),
        xact_start: r.xact_start_ms.map(unix_ms_to_timestamptz),
        query_start: r.query_start_ms.map(unix_ms_to_timestamptz),
        state_change: r.state_change_ms.map(unix_ms_to_timestamptz),
        wait_event_type: r.wait_event_type,
        wait_event: r.wait_event,
        state: r.state,
        backend_xid: r.backend_xid,
        backend_xmin: r.backend_xmin,
        query_id: r.query_id,
        query: r.query,
        backend_type: r.backend_type,
    }
}

/// One `pg_locks` row from the core diagnostics row (one-to-one; `waitstart` converted).
fn lock_from_diag(r: pgarbor_core::diag::LockRow) -> LockRow {
    LockRow {
        locktype: r.locktype,
        database: r.database,
        relation: r.relation,
        page: r.page,
        tuple: r.tuple,
        virtualxid: r.virtualxid,
        transactionid: r.transactionid,
        classid: r.classid,
        objid: r.objid,
        objsubid: r.objsubid,
        virtualtransaction: r.virtualtransaction,
        pid: r.pid,
        mode: r.mode,
        granted: r.granted,
        fastpath: r.fastpath,
        waitstart: r.waitstart_ms.map(unix_ms_to_timestamptz),
    }
}

/// One `pg_prepared_statements` row for a named prepared statement.
fn prepared_row(ps: &PreparedStatement) -> PreparedStatementRow {
    PreparedStatementRow {
        name: ps.name.clone(),
        statement: ps.text.clone(),
        prepare_time: ps.prepare_time,
        parameter_types: ps.param_types.iter().map(|t| t.oid).collect::<Vec<Oid>>(),
        result_types: ps.columns.as_ref().map(|cols| cols.iter().map(|c| c.type_oid).collect::<Vec<Oid>>()),
        from_sql: ps.from_sql,
        generic_plans: ps.generic_plans,
        custom_plans: ps.custom_plans,
    }
}

/// 42501 for a cancel/terminate of a superuser backend by a non-superuser (PG `signalfuncs.c`).
fn signal_denied_superuser(terminate: bool) -> PgError {
    if terminate {
        PgError::new(SqlState::INSUFFICIENT_PRIVILEGE, "permission denied to terminate process").with_detail(
            "Only roles with the SUPERUSER attribute may terminate processes of roles with the SUPERUSER attribute.",
        )
    } else {
        PgError::new(SqlState::INSUFFICIENT_PRIVILEGE, "permission denied to cancel query").with_detail(
            "Only roles with the SUPERUSER attribute may cancel queries of roles with the SUPERUSER attribute.",
        )
    }
}

/// 42501 for a cancel/terminate of another role's backend without `pg_signal_backend` (PG `signalfuncs.c`).
fn signal_denied_role(terminate: bool) -> PgError {
    if terminate {
        PgError::new(SqlState::INSUFFICIENT_PRIVILEGE, "permission denied to terminate process").with_detail(
            "Only roles with privileges of the role whose process is being terminated or with privileges of the \"pg_signal_backend\" role may terminate this process.",
        )
    } else {
        PgError::new(SqlState::INSUFFICIENT_PRIVILEGE, "permission denied to cancel query").with_detail(
            "Only roles with privileges of the role whose query is being canceled or with privileges of the \"pg_signal_backend\" role may cancel this query.",
        )
    }
}

impl EngineServices for SessionServices<'_> {
    fn backend_pid(&self) -> i32 {
        self.shared.id.0
    }
    fn activity(&self) -> Result<Vec<ActivityRow>> {
        let engine: &Engine = self.db.engine();
        let rows = diag::activity_rows(engine, Some(self.shared.branch))?;
        Ok(rows.into_iter().map(activity_from_diag).collect())
    }
    fn locks(&self) -> Result<Vec<LockRow>> {
        let engine: &Engine = self.db.engine();
        let rows = diag::lock_rows(engine, Some(self.shared.branch))?;
        Ok(rows.into_iter().map(lock_from_diag).collect())
    }
    fn prepared_statements(&self) -> Result<Vec<PreparedStatementRow>> {
        let mut rows = Vec::new();
        // Named statements only: the unnamed statement is not listed by pg_prepared_statements.
        for name in self.prepared.names() {
            if let Some(ps) = self.prepared.get(&name) {
                rows.push(prepared_row(ps));
            }
        }
        Ok(rows)
    }
    fn cursors(&self) -> Result<Vec<CursorRow>> {
        Ok(Vec::new())
    }
    fn blocking_pids(&self, pid: i32) -> Result<Vec<i32>> {
        let engine: &Engine = self.db.engine();
        diag::blocking_pids(engine, SessionId(pid))
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
    fn cancel_backend(&self, pid: i32, terminate: bool) -> Result<bool> {
        let engine: &Engine = self.db.engine();
        let target_id = SessionId(pid);
        // PG: an unknown pid returns false (with a WARNING we cannot emit here); a backend of another
        // branch is not visible to this session's pg_stat_activity, so it is treated as unknown too.
        let Some(target) = engine.sessions().get(target_id) else {
            return Ok(false);
        };
        if target.branch != self.shared.branch {
            return Ok(false);
        }
        // Same role, or superuser caller. Non-superusers may not signal superuser backends
        // (`signalfuncs.c`: NOSUPERUSER), nor backends of other roles (NOPERMISSION).
        // Role membership and pg_signal_backend are not modelled in v1.
        if self.shared.user != BOOTSTRAP_SUPERUSER {
            if target.user == BOOTSTRAP_SUPERUSER {
                return Err(signal_denied_superuser(terminate));
            }
            if target.user != self.shared.user {
                return Err(signal_denied_role(terminate));
            }
        }
        if terminate {
            engine.terminate_backend(target_id)
        } else {
            engine.cancel_backend(target_id)
        }
    }
    fn notify(&self, channel: &str, payload: &str) -> Result<()> {
        // Queued in the session's pending LISTEN/NOTIFY state; delivered when the transaction commits.
        let mut listen = self
            .listen
            .try_borrow_mut()
            .map_err(|_| PgError::internal("LISTEN/NOTIFY state is busy"))?;
        listen.queue_notify(channel, payload)
    }
    fn listening_channels(&self) -> Vec<String> {
        self.db.inner.notify.listening(self.shared.id.0)
    }
    fn database_name(&self) -> String {
        self.database_name.clone()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::SessionOptions;

    #[test]
    fn unix_ms_maps_to_pg_epoch() {
        assert_eq!(unix_ms_to_timestamptz(946_684_800_000), 0);
        assert_eq!(unix_ms_to_timestamptz(946_684_800_001), 1_000);
        assert_eq!(unix_ms_to_timestamptz(0), -PG_EPOCH_UNIX_US);
    }

    #[test]
    fn unix_ms_conversion_does_not_overflow() {
        // Huge inputs saturate instead of wrapping.
        let v = unix_ms_to_timestamptz(u64::MAX);
        assert!(v > 0);
    }

    #[test]
    #[ignore = "needs API-04/API-05 (Database::in_memory, Session connect) and API-11 (prepared store)"]
    fn activity_and_notify_over_session() {
        let db = Database::in_memory().expect("in-memory database");
        let s = db.session("main", SessionOptions::default()).expect("session");
        let shared = s.handle.shared().clone();
        let svc = SessionServices {
            db: &db,
            shared: &shared,
            prepared: &s.prepared,
            listen: &s.listen,
            database_name: "postgres".to_string(),
        };
        let rows = svc.activity().expect("activity");
        assert_eq!(rows.iter().filter(|r| r.pid == svc.backend_pid()).count(), 1);
        svc.notify("ch", "hi").expect("notify");
        assert_eq!(s.listen.borrow().notifies, vec![("ch".to_string(), "hi".to_string())]);
    }
}
