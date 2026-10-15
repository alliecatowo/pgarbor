//! [`SessionInfo`]: everything virtual relations and catalog functions need from the SESSION / ENGINE
//! layer that the catalog cannot know itself (the catalog crate depends on neither pgarbor-core nor
//! the `pgarbor` crate). The `pgarbor` crate implements it over a session; tests use
//! [`NullSessionInfo`].
//!
//! Row types here are the catalog's own mirror of the engine's diagnostics (`pgarbor_core::diag`
//! uses unix-millisecond `u64`s; here every timestamp is a `timestamptz` value: microseconds since
//! 2000-01-01 UTC, exactly `Datum::TimestampTz`). The `pgarbor` crate converts.
//!
//! Defaults: methods whose "nothing there" answer is TRUE for an engine without that feature return
//! empty collections (`prepared_statements`, `cursors`, `activity`, `locks`); methods that need
//! engine state to be truthful (`settings`, `sequence_state`) are required.
//!
//! Fully implemented, frozen (v1).
use pgarbor_common::{Oid, PgError, Result};

/// One `pg_settings` row (PG 18 `pg_show_all_settings()`); `None` = SQL NULL.
// CONTRACT
#[derive(Clone, Debug, PartialEq, Default)]
pub struct SettingRow {
    pub name: String,
    pub setting: Option<String>,
    pub unit: Option<String>,
    pub category: String,
    pub short_desc: String,
    pub extra_desc: Option<String>,
    /// `internal`, `postmaster`, `sighup`, `superuser-backend`, `backend`, `superuser`, `user`.
    pub context: String,
    /// `bool`, `enum`, `integer`, `real`, `string`.
    pub vartype: String,
    /// `default`, `environment variable`, `configuration file`, `command line`, `client`,
    /// `override`, `session`, ...
    pub source: String,
    pub min_val: Option<String>,
    pub max_val: Option<String>,
    pub enumvals: Option<Vec<String>>,
    pub boot_val: Option<String>,
    pub reset_val: Option<String>,
    pub sourcefile: Option<String>,
    pub sourceline: Option<i32>,
    pub pending_restart: bool,
}

/// One `pg_stat_activity` row.
// CONTRACT
#[derive(Clone, Debug, PartialEq, Default)]
pub struct ActivityRow {
    pub datid: Oid,
    pub datname: String,
    pub pid: i32,
    pub leader_pid: Option<i32>,
    pub usesysid: Oid,
    pub usename: String,
    pub application_name: String,
    /// `inet` has no `Datum` variant yet (amendment pgarbor-types-03): the virtual relation emits
    /// NULL for `client_addr` until then; the text is kept here for the future.
    pub client_addr: Option<String>,
    pub client_hostname: Option<String>,
    pub client_port: Option<i32>,
    pub backend_start: Option<i64>,
    pub xact_start: Option<i64>,
    pub query_start: Option<i64>,
    pub state_change: Option<i64>,
    pub wait_event_type: Option<String>,
    pub wait_event: Option<String>,
    pub state: Option<String>,
    pub backend_xid: Option<u64>,
    pub backend_xmin: Option<u64>,
    pub query_id: Option<i64>,
    pub query: String,
    /// `client backend` for sessions.
    pub backend_type: String,
}

/// One `pg_locks` row.
// CONTRACT
#[derive(Clone, Debug, PartialEq, Default)]
pub struct LockRow {
    pub locktype: String,
    pub database: Option<Oid>,
    pub relation: Option<Oid>,
    pub page: Option<i32>,
    pub tuple: Option<i16>,
    pub virtualxid: Option<String>,
    pub transactionid: Option<u64>,
    pub classid: Option<Oid>,
    pub objid: Option<Oid>,
    pub objsubid: Option<i16>,
    pub virtualtransaction: Option<String>,
    pub pid: Option<i32>,
    pub mode: String,
    pub granted: bool,
    pub fastpath: bool,
    pub waitstart: Option<i64>,
}

/// One `pg_prepared_statements` row (this session's prepared statements, SQL-level and protocol-level).
// CONTRACT
#[derive(Clone, Debug, PartialEq, Default)]
pub struct PreparedStatementRow {
    pub name: String,
    pub statement: String,
    pub prepare_time: i64,
    pub parameter_types: Vec<Oid>,
    /// `None` for statements without a result set.
    pub result_types: Option<Vec<Oid>>,
    /// true for SQL `PREPARE`, false for protocol-level Parse.
    pub from_sql: bool,
    pub generic_plans: i64,
    pub custom_plans: i64,
}

/// One `pg_cursors` row.
// CONTRACT
#[derive(Clone, Debug, PartialEq, Default)]
pub struct CursorRow {
    pub name: String,
    pub statement: String,
    pub is_holdable: bool,
    pub is_binary: bool,
    pub is_scrollable: bool,
    pub creation_time: i64,
}

/// Mutable state of a sequence (`SELECT * FROM seq`, `pg_sequences.last_value`,
/// `pg_sequence_last_value`).
// CONTRACT
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SequenceStateRow {
    /// `None` when the sequence has never been used (`is_called = false`): `pg_sequences.last_value`
    /// is NULL then, while `SELECT last_value FROM seq` shows the start value.
    pub last_value: Option<i64>,
    pub start_value: i64,
    pub log_cnt: i64,
    pub is_called: bool,
}

/// Which relation fork a size is requested for.
// CONTRACT
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SizeKind {
    /// `pg_relation_size(rel)` main fork.
    Relation,
    /// `pg_table_size`: relation + toast (here: relation data only).
    Table,
    /// `pg_indexes_size`: all indexes of a table.
    Indexes,
    /// `pg_total_relation_size`: table + indexes.
    Total,
}

/// Session/engine services for virtual relations and catalog functions.
// CONTRACT
pub trait SessionInfo {
    /// All GUCs as `pg_settings` rows, sorted by name (PG order is by name, case-insensitively).
    fn settings(&self) -> Result<Vec<SettingRow>>;
    /// Current value of one GUC as text (`current_setting(name)`), `None` if unknown.
    fn setting(&self, name: &str) -> Result<Option<String>> {
        Ok(self.settings()?.into_iter().find(|s| s.name == name).and_then(|s| s.setting))
    }
    /// State of sequence `oid`; `None` if the sequence state does not exist.
    fn sequence_state(&self, oid: Oid) -> Result<Option<SequenceStateRow>>;
    /// Size in bytes of relation `oid` (see [`SizeKind`]). The default raises 0A000 `relation sizes are
    /// not available in this session` (the engine owns storage statistics).
    fn relation_size(&self, oid: Oid, kind: SizeKind) -> Result<i64> {
        let _ = (oid, kind);
        Err(PgError::new(pgarbor_common::SqlState::FEATURE_NOT_SUPPORTED, "relation sizes are not available in this session"))
    }
    /// `pg_stat_activity` rows visible to this session.
    fn activity(&self) -> Result<Vec<ActivityRow>> {
        Ok(Vec::new())
    }
    /// `pg_locks` rows.
    fn locks(&self) -> Result<Vec<LockRow>> {
        Ok(Vec::new())
    }
    /// This session's prepared statements.
    fn prepared_statements(&self) -> Result<Vec<PreparedStatementRow>> {
        Ok(Vec::new())
    }
    /// This session's open cursors.
    fn cursors(&self) -> Result<Vec<CursorRow>> {
        Ok(Vec::new())
    }
    /// `pg_backend_pid()`.
    fn backend_pid(&self) -> i32 {
        0
    }
    /// `pg_blocking_pids(pid)`.
    fn blocking_pids(&self, pid: i32) -> Result<Vec<i32>> {
        let _ = pid;
        Ok(Vec::new())
    }
    /// `pg_postmaster_start_time()` (timestamptz µs).
    fn server_start_time(&self) -> i64 {
        0
    }
    /// `pg_conf_load_time()` (timestamptz µs).
    fn config_load_time(&self) -> i64 {
        0
    }
    /// `current_query()` (text of the statement being executed).
    fn current_query(&self) -> Option<String> {
        None
    }
    /// The session's temp schema OID, if one exists (`pg_my_temp_schema()`; 0 when none).
    fn temp_namespace(&self) -> Option<Oid> {
        None
    }
}

/// A [`SessionInfo`] with no settings, no activity and no sequence state: for unit tests of
/// producers that do not touch those.
#[derive(Clone, Copy, Debug, Default)]
pub struct NullSessionInfo;

impl SessionInfo for NullSessionInfo {
    fn settings(&self) -> Result<Vec<SettingRow>> {
        Ok(Vec::new())
    }
    fn sequence_state(&self, _oid: Oid) -> Result<Option<SequenceStateRow>> {
        Ok(None)
    }
}
