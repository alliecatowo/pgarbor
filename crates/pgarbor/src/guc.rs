//! The session's GUC table: SET / SET LOCAL / RESET / SHOW semantics with transactional rollback
//! and ParameterStatus reporting (lease API-02, north-star). PG: `utils/misc/guc.c`
//! (`set_config_option`, `AtEOXact_GUC`, `AtSubStart_GUC`, `AtEOSubXact_GUC`, `GUC_ACTION_*`),
//! `commands/variable.c` (check/assign hooks), `utils/misc/guc_funcs.c` (SHOW/SET statements).
//!
//! ## Model
//! One [`GucSlot`] per parameter (seeded from `guc_def::GUC_DEFS` + `HIDDEN_DEFS`), holding the
//! current value (SHOW form), its `source`, the session-start value (`reset_val`: what RESET restores:
//! the definition's boot value overridden by startup parameters, `ALTER ROLE ... SET` /
//! `ALTER DATABASE ... SET` settings are applied by `Session` at startup via `apply_startup`).
//! All state sits behind `RefCell` so `GucAccess` (shared reference, used by the executor for
//! `set_config`/`current_setting`) can mutate.
//!
//! ## Scopes and transactions (GUC stack, PG `GucStack`)
//! * `SET x = v` (session): takes effect immediately; if the enclosing transaction (or subtransaction)
//!   ABORTS the previous value is restored; if it commits the new value stays.
//! * `SET LOCAL x = v`: value is restored at the end of the current transaction (commit or abort); outside
//!   a transaction block it only produces WARNING 25P01 `SET LOCAL can only be used in transaction blocks`
//!   and has no lasting effect (the session layer issues the warning; in an implicit multi-statement
//!   transaction no warning is raised).
//! * `RESET x` / `SET x TO DEFAULT`: value := `reset_val` (a session-scope assignment).
//! * `RESET ALL`: every parameter back to `reset_val` except those with `GUC_NO_RESET_ALL`
//!   (`session_authorization`, `role`, `is_superuser`); inside a transaction it is rolled back on abort.
//! * Subtransactions: `at_savepoint(name)` pushes a level; `at_rollback_to(name)` undoes changes made
//!   after it; `at_release(name)` merges the level into its parent.
//! * `at_txn_start()` (BEGIN or implicit transaction start) opens level 1; `at_commit()` keeps session
//!   assignments and drops SET LOCAL ones (restoring the pre-local value); `at_abort()` undoes both.
//! Changes to parameters with `report == true` (whenever the visible value changes, including by
//! rollback) are queued; `take_reported_changes()` drains `(name, value)` pairs in change order,
//! de-duplicated by name keeping the LAST value (PG sends one ParameterStatus per changed parameter
//! per statement).
//!
//! ## Validation (errors are exact PG wording, SQLSTATE 22023 unless stated)
//! * unknown name: 42704 `unrecognized configuration parameter "foo"` (SET, SHOW, RESET).
//! * `context`: `Internal` -> 55P02 `parameter "server_version" cannot be changed`; `Postmaster` ->
//!   55P02 `parameter "max_connections" cannot be changed without restarting the server`; `Sighup` ->
//!   55P02 `parameter "x" cannot be changed now`; `SuperuserBackend`/`Backend` -> 55P02 `parameter "x"
//!   cannot be set after connection start`; `Superuser` requires `is_superuser` (v1: the session user's
//!   `rolsuper`) else 42501 `permission denied to set parameter "x"`.
//! * value syntax by type: `guc_value::normalize`.
//! * hooks (after normalization): `DateStyle` (format ISO|SQL|Postgres|German and order MDY|DMY|YMD or
//!   `European`/`US`/`NonEuropean`/`DEFAULT`; unspecified part keeps the current one; stored as
//!   `ISO, MDY`; error `invalid value for parameter "DateStyle": "foo"` DETAIL `Unrecognized key
//!   word: "foo".`; conflicting -> DETAIL `Conflicting "datestyle" specifications.`), `TimeZone`
//!   (`pgarbor_types::datetime::tz::parse_timezone_setting`; failure: `invalid value for parameter
//!   "TimeZone": "Nowhere/Bad"`; `LOCAL`/`DEFAULT` -> `UTC`), `IntervalStyle`, `client_encoding` (only
//!   UTF8/UNICODE aliases, stored `UTF8`; others 22023 `invalid value for parameter "client_encoding":
//!   "x"`), `standard_conforming_strings` (bool), `bytea_output`, `extra_float_digits` (range), `search_path`
//!   (any string; stored as given), `transaction_isolation` / `default_transaction_isolation`
//!   (`core::IsolationLevel::parse`), `statement_timeout` / `lock_timeout` /
//!   `idle_in_transaction_session_timeout` (ms >= 0), `application_name` (non-printable ASCII replaced by
//!   `?`), `role` / `session_authorization` (set by the session layer with checked role names).
//! ## SHOW
//! `show(name)` returns the stored SHOW-form value (`DateStyle` -> `ISO, MDY`; `TimeZone` -> `UTC`;
//! `transaction_isolation` is overridden by the session with the live transaction's level); `SHOW ALL`
//! rows `(name, setting, description)` sorted by name case-insensitively (hidden parameters excluded).
//! `rows()` produces `SettingRow`s for `pg_settings` (`source` `default`, `session`, `client`,
//! `configuration file`...; `reset_val`, `boot_val`, `pending_restart=false`).
use std::cell::RefCell;

use pgarbor_catalog::session::SettingRow;
use pgarbor_common::{PgError, Result};

use crate::guc_def::GucDef;

/// Where the current value came from (`pg_settings.source`).
// CONTRACT
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum GucSource {
    Default,
    /// Startup packet parameter (`client`).
    Client,
    /// `ALTER ROLE/DATABASE ... SET` (`database` / `user`).
    Database,
    User,
    /// `SET` by the session (`session`).
    Session,
    /// Internally forced (`override`).
    Override,
}

impl GucSource {
    /// `pg_settings.source` text.
    pub fn as_str(self) -> &'static str {
        match self {
            GucSource::Default => "default",
            GucSource::Client => "client",
            GucSource::Database => "database",
            GucSource::User => "user",
            GucSource::Session => "session",
            GucSource::Override => "override",
        }
    }
}

/// Scope of an assignment.
// CONTRACT
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum GucScope {
    /// `SET`.
    Session,
    /// `SET LOCAL`.
    Local,
}

/// Current state of one parameter.
// CONTRACT
#[derive(Clone, Debug)]
pub struct GucSlot {
    pub def: &'static GucDef,
    pub value: String,
    pub source: GucSource,
    pub reset_val: String,
    pub reset_source: GucSource,
}

/// One saved value on the transaction stack.
#[derive(Clone, Debug)]
pub(crate) struct GucSaved {
    pub(crate) name: String,
    pub(crate) value: String,
    pub(crate) source: GucSource,
    /// Stack level (1 = transaction, 2.. = savepoints) at which the change was made.
    pub(crate) level: usize,
    /// The change was a `SET LOCAL` (undone at level end regardless of commit).
    pub(crate) local: bool,
}

/// Interior state of [`GucTable`].
#[derive(Debug, Default)]
pub struct GucInner {
    pub(crate) slots: Vec<GucSlot>,
    pub(crate) saved: Vec<GucSaved>,
    /// Open stack levels with the savepoint name that opened them (`None` for the transaction level).
    pub(crate) levels: Vec<Option<String>>,
    pub(crate) reported: Vec<(String, String)>,
    pub(crate) superuser: bool,
}

/// The table (module docs).
#[derive(Debug, Default)]
pub struct GucTable {
    pub(crate) inner: RefCell<GucInner>,
}

impl GucTable {
    /// Table seeded with every definition at its boot value (`reset_val = boot_val`, source
    /// `Default`). `superuser`: whether the session user may set `Superuser`-context parameters.
    pub fn new(superuser: bool) -> GucTable {
        let slots: Vec<GucSlot> = crate::guc_def::GUC_DEFS
            .iter()
            .chain(crate::guc_def::HIDDEN_DEFS.iter())
            .map(|d| GucSlot {
                def: d,
                value: d.boot_val.to_string(),
                source: GucSource::Default,
                reset_val: d.boot_val.to_string(),
                reset_source: GucSource::Default,
            })
            .collect();
        GucTable { inner: RefCell::new(GucInner { slots, superuser, ..GucInner::default() }) }
    }

    /// Apply startup assignments (`application_name`, `options -c`, role/database settings) as the
    /// session's reset values: value and `reset_val` both change, with `source`.
    #[allow(unused_variables)]
    pub fn apply_startup(&self, name: &str, value: &str, source: GucSource) -> Result<()> {
        Err(PgError::unimplemented("API-02", "startup GUC assignment (GucTable::apply_startup)"))
    }

    /// Current value in SHOW form; unknown -> 42704.
    #[allow(unused_variables)]
    pub fn show(&self, name: &str) -> Result<String> {
        Err(PgError::unimplemented("API-02", "SHOW (GucTable::show)"))
    }

    /// `SET [LOCAL] name = values...` (values already rendered, `SetValue::Values`). Returns the new
    /// stored value.
    #[allow(unused_variables)]
    pub fn set(&self, name: &str, values: &[String], scope: GucScope) -> Result<String> {
        Err(PgError::unimplemented("API-02", "SET (GucTable::set)"))
    }

    /// `RESET name` / `SET name TO DEFAULT`.
    #[allow(unused_variables)]
    pub fn reset(&self, name: &str, scope: GucScope) -> Result<String> {
        Err(PgError::unimplemented("API-02", "RESET (GucTable::reset)"))
    }

    /// `RESET ALL`.
    pub fn reset_all(&self) -> Result<()> {
        Err(PgError::unimplemented("API-02", "RESET ALL (GucTable::reset_all)"))
    }

    /// `SHOW ALL` rows `(name, setting, description)` sorted by name (hidden excluded).
    pub fn show_all(&self) -> Result<Vec<(String, String, String)>> {
        Err(PgError::unimplemented("API-02", "SHOW ALL (GucTable::show_all)"))
    }

    /// `pg_settings` rows.
    pub fn setting_rows(&self) -> Result<Vec<SettingRow>> {
        Err(PgError::unimplemented("API-02", "pg_settings rows (GucTable::setting_rows)"))
    }

    /// Open the transaction level (BEGIN / implicit transaction start). Idempotent per transaction.
    pub fn at_txn_start(&self) -> Result<()> {
        Err(PgError::unimplemented("API-02", "GUC transaction start (GucTable::at_txn_start)"))
    }

    /// Transaction end: commit keeps session assignments, drops SET LOCAL ones; abort restores
    /// everything saved. Queues ParameterStatus changes for reported parameters whose value changed.
    #[allow(unused_variables)]
    pub fn at_txn_end(&self, commit: bool) -> Result<()> {
        Err(PgError::unimplemented("API-02", "GUC transaction end (GucTable::at_txn_end)"))
    }

    /// SAVEPOINT: open a sub-level.
    #[allow(unused_variables)]
    pub fn at_savepoint(&self, name: &str) -> Result<()> {
        Err(PgError::unimplemented("API-02", "GUC savepoint (GucTable::at_savepoint)"))
    }

    /// ROLLBACK TO SAVEPOINT: undo changes of the named level and everything above; the level stays
    /// open (PG restarts the subtransaction).
    #[allow(unused_variables)]
    pub fn at_rollback_to(&self, name: &str) -> Result<()> {
        Err(PgError::unimplemented("API-02", "GUC rollback to savepoint (GucTable::at_rollback_to)"))
    }

    /// RELEASE SAVEPOINT: merge the level into its parent.
    #[allow(unused_variables)]
    pub fn at_release(&self, name: &str) -> Result<()> {
        Err(PgError::unimplemented("API-02", "GUC release savepoint (GucTable::at_release)"))
    }

    /// Drain the queued ParameterStatus changes (module docs).
    pub fn take_reported_changes(&self) -> Vec<(String, String)> {
        std::mem::take(&mut self.inner.borrow_mut().reported)
    }

    /// Overwrite a value without validation or stack effects (session layer for `session_authorization`
    /// / `role` / `is_superuser` after it validated the role). Queues a report if `report`.
    #[allow(unused_variables)]
    pub fn force_value(&self, name: &str, value: &str) -> Result<()> {
        Err(PgError::unimplemented("API-02", "forced GUC value (GucTable::force_value)"))
    }

    /// The ParameterStatus set for a new connection, in `STARTUP_REPORT_ORDER`.
    pub fn startup_report(&self) -> Result<Vec<(String, String)>> {
        Err(PgError::unimplemented("API-02", "startup ParameterStatus set (GucTable::startup_report)"))
    }
}

impl pgarbor_exec::GucAccess for GucTable {
    fn get(&self, name: &str) -> Option<String> {
        self.show(name).ok()
    }
    fn set(&self, name: &str, value: &str, is_local: bool) -> Result<String> {
        GucTable::set(self, name, &[value.to_string()], if is_local { GucScope::Local } else { GucScope::Session })
    }
    fn settings(&self) -> Result<Vec<SettingRow>> {
        self.setting_rows()
    }
}
