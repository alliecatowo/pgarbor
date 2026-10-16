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
use std::cmp::Ordering;

use pgarbor_catalog::session::SettingRow;
use pgarbor_common::{PgError, Result, SqlState};
use pgarbor_core::IsolationLevel;
use pgarbor_types::datetime::tz::{parse_timezone_setting, setting_display};

use crate::guc_def::{find_def, GucContext, GucDef, GucType, HIDDEN_DEFS, STARTUP_REPORT_ORDER};
use crate::guc_value;

/// 55P02 `cannot_change_runtime_param`.
const CANT_CHANGE_PARAM: SqlState = SqlState(*b"55P02");
/// 3B001 `invalid_savepoint_specification`.
const INVALID_SAVEPOINT: SqlState = SqlState(*b"3B001");

/// Parameters `RESET ALL` never touches (PG `GUC_NO_RESET_ALL`, `utils/misc/guc_tables.c`).
const NO_RESET_ALL: &[&str] = &[
    "seed",
    "role",
    "session_authorization",
    "is_superuser",
    "transaction_read_only",
    "transaction_deferrable",
    "transaction_isolation",
];

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
///
/// Entries are unique per (name, level). `value`/`source` hold the value in force before the first
/// change made at `level`: the value an abort (or a commit of a `local` entry) restores.
/// `local` marks a `SET LOCAL` in force at that level; `masked` holds the session value that a
/// `SET LOCAL` replaced after a `SET` in the same level (PG `GUC_SET_LOCAL` + `masked`), restored at
/// commit instead of `value`.
#[derive(Clone, Debug)]
pub(crate) struct GucSaved {
    pub(crate) name: String,
    pub(crate) value: String,
    pub(crate) source: GucSource,
    /// Stack level (1 = transaction, 2.. = savepoints) at which the change was made.
    pub(crate) level: usize,
    /// The change was a `SET LOCAL` (undone at level end regardless of commit).
    pub(crate) local: bool,
    /// Session value masked by a later `SET LOCAL` in the same level (see type docs).
    pub(crate) masked: Option<(String, GucSource)>,
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
    /// Custom placeholder parameters (`a.b`, lower-cased name, value); known after the first SET/RESET.
    pub(crate) placeholders: Vec<(String, String)>,
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
            .map(|d| {
                // Integer boot values are in base units; SHOW form uses the largest exact unit (`4MB`).
                let boot = if d.vartype == GucType::Int {
                    guc_value::normalize(d, d.boot_val).unwrap_or_else(|_| d.boot_val.to_string())
                } else {
                    d.boot_val.to_string()
                };
                GucSlot {
                    def: d,
                    value: boot.clone(),
                    source: GucSource::Default,
                    reset_val: boot,
                    reset_source: GucSource::Default,
                }
            })
            .collect();
        let mut inner = GucInner { slots, superuser, ..GucInner::default() };
        // `is_superuser` reflects the session user (read-only, reported at startup).
        let su = if superuser { "on" } else { "off" };
        if let Some(idx) = inner.slots.iter().position(|s| s.def.name == "is_superuser") {
            inner.slots[idx].value = su.to_string();
            inner.slots[idx].reset_val = su.to_string();
        }
        GucTable { inner: RefCell::new(inner) }
    }

    /// Apply startup assignments (`application_name`, `options -c`, role/database settings) as the
    /// session's reset values: value and `reset_val` both change, with `source`.
    pub fn apply_startup(&self, name: &str, value: &str, source: GucSource) -> Result<()> {
        let mut guard = self.inner.borrow_mut();
        let inner: &mut GucInner = &mut guard;
        if is_placeholder_name(name) {
            check_custom_name(name)?;
            placeholder_put(inner, name, value.to_string());
            return Ok(());
        }
        let idx = lookup(inner, name)?;
        let def = inner.slots[idx].def;
        check_settable(inner, def, true)?;
        let text = validate_value(def, &inner.slots[idx].value, value)?;
        let slot = &mut inner.slots[idx];
        slot.value = text.clone();
        slot.reset_val = text;
        slot.source = source;
        slot.reset_source = source;
        Ok(())
    }

    /// Current value in SHOW form; unknown -> 42704.
    pub fn show(&self, name: &str) -> Result<String> {
        let inner = self.inner.borrow();
        if find_def(name).is_none() {
            if let Some(v) = placeholder_get(&inner, name) {
                return Ok(v);
            }
        }
        let idx = lookup(&inner, name)?;
        Ok(inner.slots[idx].value.clone())
    }

    /// `SET [LOCAL] name = values...` (values already rendered, `SetValue::Values`). Returns the new
    /// stored value.
    pub fn set(&self, name: &str, values: &[String], scope: GucScope) -> Result<String> {
        let mut guard = self.inner.borrow_mut();
        let inner: &mut GucInner = &mut guard;
        if is_placeholder_name(name) {
            check_custom_name(name)?;
            let value = values.join(", ");
            placeholder_put(inner, name, value.clone());
            return Ok(value);
        }
        let idx = lookup(inner, name)?;
        let def = inner.slots[idx].def;
        check_settable(inner, def, false)?;
        let text = guc_value::join_set_values(def, values)?;
        let value = validate_value(def, &inner.slots[idx].value, &text)?;
        assign(inner, idx, value.clone(), GucSource::Session, scope);
        Ok(value)
    }

    /// `RESET name` / `SET name TO DEFAULT`.
    pub fn reset(&self, name: &str, scope: GucScope) -> Result<String> {
        let mut guard = self.inner.borrow_mut();
        let inner: &mut GucInner = &mut guard;
        if is_placeholder_name(name) {
            check_custom_name(name)?;
            placeholder_put(inner, name, String::new());
            return Ok(String::new());
        }
        let idx = lookup(inner, name)?;
        let def = inner.slots[idx].def;
        check_settable(inner, def, false)?;
        let value = inner.slots[idx].reset_val.clone();
        let source = inner.slots[idx].reset_source;
        assign(inner, idx, value.clone(), source, scope);
        Ok(value)
    }

    /// `RESET ALL`.
    pub fn reset_all(&self) -> Result<()> {
        let mut guard = self.inner.borrow_mut();
        let inner: &mut GucInner = &mut guard;
        for idx in 0..inner.slots.len() {
            let def = inner.slots[idx].def;
            // PG ResetAllOptions: only SET-able contexts, only values set in this session, and not
            // the GUC_NO_RESET_ALL exclusions.
            if inner.slots[idx].source != GucSource::Session {
                continue;
            }
            if !matches!(def.context, GucContext::User | GucContext::Superuser) {
                continue;
            }
            if NO_RESET_ALL.iter().any(|n| n.eq_ignore_ascii_case(def.name)) {
                continue;
            }
            let value = inner.slots[idx].reset_val.clone();
            let source = inner.slots[idx].reset_source;
            assign(inner, idx, value, source, GucScope::Session);
        }
        Ok(())
    }

    /// `SHOW ALL` rows `(name, setting, description)` sorted by name (hidden excluded).
    pub fn show_all(&self) -> Result<Vec<(String, String, String)>> {
        let inner = self.inner.borrow();
        let mut rows: Vec<(String, String, String)> = inner
            .slots
            .iter()
            .filter(|s| !is_hidden(s.def))
            .map(|s| (s.def.name.to_string(), s.value.clone(), s.def.short_desc.to_string()))
            .collect();
        rows.sort_by(|a, b| name_cmp(&a.0, &b.0));
        Ok(rows)
    }

    /// `pg_settings` rows.
    pub fn setting_rows(&self) -> Result<Vec<SettingRow>> {
        let inner = self.inner.borrow();
        Ok(inner.slots.iter().filter(|s| !is_hidden(s.def)).map(setting_row).collect())
    }

    /// Open the transaction level (BEGIN / implicit transaction start). Idempotent per transaction.
    pub fn at_txn_start(&self) -> Result<()> {
        let mut inner = self.inner.borrow_mut();
        if inner.levels.is_empty() {
            inner.levels.push(None);
        }
        Ok(())
    }

    /// Transaction end: commit keeps session assignments and drops SET LOCAL ones; abort restores
    /// everything saved. Queues ParameterStatus changes for reported parameters whose value changed.
    pub fn at_txn_end(&self, commit: bool) -> Result<()> {
        let mut guard = self.inner.borrow_mut();
        let inner: &mut GucInner = &mut guard;
        if commit {
            // Subtransactions still open are released first (PG COMMIT releases them all).
            while inner.levels.len() > 1 {
                let level = inner.levels.len();
                merge_level(inner, level);
                inner.levels.pop();
            }
            let entries = std::mem::take(&mut inner.saved);
            for entry in entries {
                if !entry.local {
                    continue;
                }
                let idx = lookup(inner, &entry.name)?;
                let (value, source) = match entry.masked {
                    Some(masked) => masked,
                    None => (entry.value, entry.source),
                };
                put(inner, idx, value, source);
            }
        } else {
            let entries = std::mem::take(&mut inner.saved);
            for entry in entries.into_iter().rev() {
                restore(inner, entry)?;
            }
        }
        inner.levels.clear();
        inner.saved.clear();
        Ok(())
    }

    /// SAVEPOINT: open a sub-level.
    pub fn at_savepoint(&self, name: &str) -> Result<()> {
        let mut inner = self.inner.borrow_mut();
        if inner.levels.is_empty() {
            inner.levels.push(None);
        }
        inner.levels.push(Some(name.to_string()));
        Ok(())
    }

    /// ROLLBACK TO SAVEPOINT: undo changes of the named level and everything above; the level stays
    /// open (PG restarts the subtransaction).
    pub fn at_rollback_to(&self, name: &str) -> Result<()> {
        let mut guard = self.inner.borrow_mut();
        let inner: &mut GucInner = &mut guard;
        let pos = savepoint_index(inner, name)?;
        undo_from(inner, pos + 1)?;
        inner.levels.truncate(pos + 1);
        Ok(())
    }

    /// RELEASE SAVEPOINT: merge the level into its parent.
    pub fn at_release(&self, name: &str) -> Result<()> {
        let mut guard = self.inner.borrow_mut();
        let inner: &mut GucInner = &mut guard;
        let pos = savepoint_index(inner, name)?;
        // RELEASE of an outer savepoint also releases every savepoint opened after it.
        while inner.levels.len() > pos {
            let level = inner.levels.len();
            merge_level(inner, level);
            inner.levels.pop();
        }
        Ok(())
    }

    /// Drain the queued ParameterStatus changes (module docs).
    pub fn take_reported_changes(&self) -> Vec<(String, String)> {
        let raw = std::mem::take(&mut self.inner.borrow_mut().reported);
        // One entry per name, keeping the last value, ordered by the position of that last change.
        let mut out: Vec<(String, String)> = Vec::with_capacity(raw.len());
        for (name, value) in raw.into_iter().rev() {
            if !out.iter().any(|(n, _)| *n == name) {
                out.push((name, value));
            }
        }
        out.reverse();
        out
    }

    /// Overwrite a value without validation or stack effects (session layer for `session_authorization`
    /// / `role` / `is_superuser` after it validated the role). Queues a report if `report`.
    pub fn force_value(&self, name: &str, value: &str) -> Result<()> {
        let mut guard = self.inner.borrow_mut();
        let inner: &mut GucInner = &mut guard;
        let idx = lookup(inner, name)?;
        put(inner, idx, value.to_string(), GucSource::Session);
        if name == "is_superuser" {
            // PG `superuser()` follows the current role (SET ROLE / SET SESSION AUTHORIZATION).
            inner.superuser = value == "on";
        }
        Ok(())
    }

    /// The ParameterStatus set for a new connection, in `STARTUP_REPORT_ORDER`.
    pub fn startup_report(&self) -> Result<Vec<(String, String)>> {
        let inner = self.inner.borrow();
        let mut out = Vec::with_capacity(STARTUP_REPORT_ORDER.len());
        for name in STARTUP_REPORT_ORDER {
            let idx = lookup(&inner, name)?;
            let slot = &inner.slots[idx];
            out.push((slot.def.name.to_string(), slot.value.clone()));
        }
        Ok(out)
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
    fn check_setting(&self, name: &str, value: &str) -> Result<()> {
        let guard = self.inner.borrow();
        let inner: &GucInner = &guard;
        if is_placeholder_name(name) {
            return check_custom_name(name);
        }
        let idx = lookup(inner, name)?;
        let def = inner.slots[idx].def;
        let text = guc_value::join_set_values(def, &[value.to_string()])?;
        validate_value(def, &inner.slots[idx].value, &text)?;
        Ok(())
    }
}

// ---------------------------------------------------------------------------------------------
// Lookup and validation helpers
// ---------------------------------------------------------------------------------------------

/// A custom (placeholder) parameter name: contains a dot and is not a built-in.
fn is_placeholder_name(name: &str) -> bool {
    name.contains('.') && find_def(name).is_none()
}

/// guc.c `valid_custom_variable_name`: two or more simple identifiers separated by dots, else 42602.
fn check_custom_name(name: &str) -> Result<()> {
    let mut saw_sep = false;
    let mut name_start = true;
    let mut valid = true;
    for c in name.chars() {
        if c == '.' {
            if name_start {
                valid = false;
                break;
            }
            saw_sep = true;
            name_start = true;
        } else if c.is_ascii_alphabetic() || c == '_' || !c.is_ascii() {
            name_start = false;
        } else if !name_start && (c.is_ascii_digit() || c == '$') {
        } else {
            valid = false;
            break;
        }
    }
    if valid && !name_start && saw_sep {
        return Ok(());
    }
    Err(PgError::new(SqlState::INVALID_NAME, format!("invalid configuration parameter name \"{name}\""))
        .with_detail("Custom parameter names must be two or more simple identifiers separated by dots."))
}

fn placeholder_get(inner: &GucInner, name: &str) -> Option<String> {
    inner.placeholders.iter().find(|(n, _)| n.eq_ignore_ascii_case(name)).map(|(_, v)| v.clone())
}

fn placeholder_put(inner: &mut GucInner, name: &str, value: String) {
    match inner.placeholders.iter_mut().find(|(n, _)| n.eq_ignore_ascii_case(name)) {
        Some(slot) => slot.1 = value,
        None => inner.placeholders.push((name.to_ascii_lowercase(), value)),
    }
}

fn unrecognized(name: &str) -> PgError {
    PgError::new(SqlState::UNDEFINED_OBJECT, format!("unrecognized configuration parameter \"{name}\""))
}

/// Slot index of a parameter (case-insensitive name, hidden parameters included).
fn lookup(inner: &GucInner, name: &str) -> Result<usize> {
    let def = find_def(name).ok_or_else(|| unrecognized(name))?;
    inner
        .slots
        .iter()
        .position(|s| std::ptr::eq(s.def, def))
        .ok_or_else(|| PgError::internal(format!("no GUC slot for \"{}\"", def.name)))
}

fn is_hidden(def: &GucDef) -> bool {
    HIDDEN_DEFS.iter().any(|h| h.name == def.name)
}

fn name_cmp(a: &str, b: &str) -> Ordering {
    a.bytes().map(|c| c.to_ascii_lowercase()).cmp(b.bytes().map(|c| c.to_ascii_lowercase()))
}

/// Context check of a write (PG `set_config_option` permission checks). `startup` allows the
/// connection-start-only contexts.
fn check_settable(inner: &GucInner, def: &GucDef, startup: bool) -> Result<()> {
    let name = def.name;
    match def.context {
        GucContext::Internal => {
            Err(PgError::new(CANT_CHANGE_PARAM, format!("parameter \"{name}\" cannot be changed")))
        }
        GucContext::Postmaster => Err(PgError::new(
            CANT_CHANGE_PARAM,
            format!("parameter \"{name}\" cannot be changed without restarting the server"),
        )),
        GucContext::Sighup => Err(PgError::new(CANT_CHANGE_PARAM, format!("parameter \"{name}\" cannot be changed now"))),
        GucContext::SuperuserBackend | GucContext::Backend => {
            if startup {
                Ok(())
            } else {
                Err(PgError::new(CANT_CHANGE_PARAM, format!("parameter \"{name}\" cannot be set after connection start")))
            }
        }
        GucContext::Superuser => {
            if inner.superuser {
                Ok(())
            } else {
                Err(PgError::new(SqlState::INSUFFICIENT_PRIVILEGE, format!("permission denied to set parameter \"{name}\"")))
            }
        }
        GucContext::User => Ok(()),
    }
}

/// `default_text_search_config` hook (ts_cat.c `check_default_text_search_config`). Only the builtin
/// `pg_catalog` configurations exist (CREATE TEXT SEARCH CONFIGURATION is unsupported), and PostgreSQL
/// stores the canonical `pg_catalog.<name>` form. Unquoted names fold to lower case; quoted
/// identifiers retain their case, including quoted schema names.
fn text_search_config_value(text: &str) -> Result<String> {
    // Sorted (binary search): the `pg_catalog` text search configurations of PostgreSQL 18.
    const BUILTIN_CONFIGS: &[&str] = &[
        "arabic", "armenian", "basque", "catalan", "danish", "dutch", "english", "estonian", "finnish", "french",
        "german", "greek", "hindi", "hungarian", "indonesian", "irish", "italian", "lithuanian", "nepali",
        "norwegian", "portuguese", "romanian", "russian", "serbian", "simple", "spanish", "swedish", "tamil",
        "turkish", "yiddish",
    ];
    fn identifiers(text: &str) -> Option<Vec<String>> {
        let mut chars = text.chars().peekable();
        let mut out = Vec::new();
        loop {
            while chars.peek().is_some_and(|c| c.is_ascii_whitespace()) { chars.next(); }
            let mut name = String::new();
            if chars.peek() == Some(&'"') {
                chars.next();
                loop {
                    match chars.next()? {
                        '"' if chars.peek() == Some(&'"') => { chars.next(); name.push('"'); }
                        '"' => break,
                        c => name.push(c),
                    }
                }
            } else {
                while chars.peek().is_some_and(|c| c.is_ascii_alphanumeric() || *c == '_' || *c == '$' || !c.is_ascii()) {
                    name.push(chars.next()?.to_ascii_lowercase());
                }
            }
            if name.is_empty() { return None; }
            out.push(name);
            while chars.peek().is_some_and(|c| c.is_ascii_whitespace()) { chars.next(); }
            match chars.next() {
                None => return Some(out),
                Some('.') if out.len() == 1 => {}
                _ => return None,
            }
        }
    }
    if let Some(parts) = identifiers(text) {
        let name = parts.last().map(String::as_str).unwrap_or("");
        if (parts.len() == 1 || parts[0] == "pg_catalog") && BUILTIN_CONFIGS.binary_search(&name).is_ok() {
            return Ok(format!("pg_catalog.{name}"));
        }
    }
    Err(PgError::new(
        SqlState::INVALID_PARAMETER_VALUE,
        format!("invalid value for parameter \"default_text_search_config\": \"{text}\""),
    ))
}

/// Validate and canonicalize a value for `def` (hooks by name, else `guc_value::normalize`).
/// `current` is the present SHOW-form value (`DateStyle` keeps the parts not mentioned).
fn validate_value(def: &GucDef, current: &str, text: &str) -> Result<String> {
    let name = def.name;
    if name.eq_ignore_ascii_case("DateStyle") {
        return parse_datestyle(text, current);
    }
    if name.eq_ignore_ascii_case("TimeZone") {
        return timezone_value(text);
    }
    if name.eq_ignore_ascii_case("client_encoding") {
        return client_encoding_value(text);
    }
    if name.eq_ignore_ascii_case("default_text_search_config") {
        return text_search_config_value(text);
    }
    if name.eq_ignore_ascii_case("transaction_isolation") || name.eq_ignore_ascii_case("default_transaction_isolation") {
        return isolation_value(def, text);
    }
    let value = guc_value::normalize(def, text)?;
    if name.eq_ignore_ascii_case("application_name") {
        return Ok(clean_ascii(&value));
    }
    Ok(value)
}

/// `TimeZone` hook: `LOCAL`/`DEFAULT` mean `UTC`; otherwise the types crate resolves the zone and
/// its SHOW form is returned.
fn timezone_value(text: &str) -> Result<String> {
    if text.eq_ignore_ascii_case("LOCAL") || text.eq_ignore_ascii_case("DEFAULT") {
        return Ok("UTC".to_string());
    }
    let setting = parse_timezone_setting(text)?;
    Ok(setting_display(&setting))
}

/// Non-UTF8 client encodings PG accepts (normalized key -> canonical name).
const CLIENT_ENCODINGS: &[(&str, &str)] = &[
    ("latin1", "LATIN1"),
    ("iso88591", "LATIN1"),
    ("latin2", "LATIN2"),
    ("iso88592", "LATIN2"),
    ("latin9", "LATIN9"),
    ("iso885915", "LATIN9"),
    ("win1252", "WIN1252"),
    ("windows1252", "WIN1252"),
    ("sqlascii", "SQL_ASCII"),
];

/// `client_encoding` hook: UTF8 aliases are accepted (`UTF-8`, `utf_8`, `unicode`...).
fn client_encoding_value(text: &str) -> Result<String> {
    let key: String = text.chars().filter(|c| *c != '-' && *c != '_').map(|c| c.to_ascii_lowercase()).collect();
    if key == "utf8" || key == "unicode" {
        Ok("UTF8".to_string())
    } else if let Some((_, canonical)) = CLIENT_ENCODINGS.iter().find(|(k, _)| *k == key) {
        // Accepted so SET/SHOW round-trip; output is not transcoded (ASCII data is identical).
        Ok((*canonical).to_string())
    } else {
        Err(PgError::new(
            SqlState::INVALID_PARAMETER_VALUE,
            format!("invalid value for parameter \"client_encoding\": \"{text}\""),
        ))
    }
}

/// `transaction_isolation` / `default_transaction_isolation` hook (`core::IsolationLevel::parse`
/// with the parameter name of the variable being set).
fn isolation_value(def: &GucDef, text: &str) -> Result<String> {
    match IsolationLevel::parse(text) {
        Ok(level) => Ok(level.as_pg_str().to_string()),
        Err(_) => Err(PgError::new(
            SqlState::INVALID_PARAMETER_VALUE,
            format!("invalid value for parameter \"{}\": \"{text}\"", def.name),
        )
        .with_hint("Available values: serializable, repeatable read, read committed, read uncommitted.")),
    }
}

/// `application_name` hook: each non-printable (or non-ASCII) byte becomes `?` (PG `pg_clean_ascii`).
fn clean_ascii(text: &str) -> String {
    text.bytes().map(|b| if (0x20..0x7f).contains(&b) { b as char } else { '?' }).collect()
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum DsStyle {
    Iso,
    Sql,
    Postgres,
    German,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum DsOrder {
    Ymd,
    Dmy,
    Mdy,
}

impl DsStyle {
    fn as_str(self) -> &'static str {
        match self {
            DsStyle::Iso => "ISO",
            DsStyle::Sql => "SQL",
            DsStyle::Postgres => "Postgres",
            DsStyle::German => "German",
        }
    }
}

impl DsOrder {
    fn as_str(self) -> &'static str {
        match self {
            DsOrder::Ymd => "YMD",
            DsOrder::Dmy => "DMY",
            DsOrder::Mdy => "MDY",
        }
    }
}

fn ds_style_word(word: &str) -> Option<DsStyle> {
    match word {
        "iso" => Some(DsStyle::Iso),
        "sql" => Some(DsStyle::Sql),
        "postgres" => Some(DsStyle::Postgres),
        "german" => Some(DsStyle::German),
        _ => None,
    }
}

fn ds_order_word(word: &str) -> Option<DsOrder> {
    match word {
        "ymd" => Some(DsOrder::Ymd),
        "dmy" | "european" | "euro" => Some(DsOrder::Dmy),
        "mdy" | "us" | "noneuropean" | "noneuro" => Some(DsOrder::Mdy),
        _ => None,
    }
}

/// Words of a list value: separated by commas or whitespace.
fn ds_words<'a>(text: &'a str) -> impl Iterator<Item = &'a str> + 'a {
    text.split(|c: char| c == ',' || c.is_ascii_whitespace()).filter(|w| !w.is_empty())
}

fn ds_conflict(text: &str) -> PgError {
    PgError::new(SqlState::INVALID_PARAMETER_VALUE, format!("invalid value for parameter \"DateStyle\": \"{text}\""))
        .with_detail("Conflicting \"datestyle\" specifications.")
}

/// `DateStyle` hook (PG `check_datestyle`): `ISO|SQL|Postgres|German` and `MDY|DMY|YMD` (with the
/// synonyms `US`, `NonEuropean`, `European`, `Euro`, `DEFAULT`), in any order. Parts not mentioned keep
/// the current value; `German` without an explicit order implies `DMY`. Stored as `Style, Order`.
fn parse_datestyle(text: &str, current: &str) -> Result<String> {
    let mut style = DsStyle::Iso;
    let mut order = DsOrder::Mdy;
    for word in ds_words(current) {
        let lower = word.to_ascii_lowercase();
        if let Some(s) = ds_style_word(&lower) {
            style = s;
        } else if let Some(o) = ds_order_word(&lower) {
            order = o;
        }
    }

    let mut have_style: Option<DsStyle> = None;
    let mut have_order: Option<DsOrder> = None;
    for word in ds_words(text) {
        let lower = word.to_ascii_lowercase();
        if lower == "default" {
            if have_style.is_none() {
                have_style = Some(DsStyle::Iso);
            }
            if have_order.is_none() {
                have_order = Some(DsOrder::Mdy);
            }
        } else if let Some(s) = ds_style_word(&lower) {
            if have_style.is_some_and(|prev| prev != s) {
                return Err(ds_conflict(text));
            }
            have_style = Some(s);
        } else if let Some(o) = ds_order_word(&lower) {
            if have_order.is_some_and(|prev| prev != o) {
                return Err(ds_conflict(text));
            }
            have_order = Some(o);
        } else {
            return Err(PgError::new(
                SqlState::INVALID_PARAMETER_VALUE,
                format!("invalid value for parameter \"DateStyle\": \"{text}\""),
            )
            .with_detail(format!("Unrecognized key word: \"{lower}\".")));
        }
    }

    if let Some(s) = have_style {
        style = s;
        if s == DsStyle::German && have_order.is_none() {
            order = DsOrder::Dmy;
        }
    }
    if let Some(o) = have_order {
        order = o;
    }
    Ok(format!("{}, {}", style.as_str(), order.as_str()))
}

// ---------------------------------------------------------------------------------------------
// Stack maintenance
// ---------------------------------------------------------------------------------------------

/// Store a value in a slot, queueing a ParameterStatus change when a reported parameter changes.
fn put(inner: &mut GucInner, idx: usize, value: String, source: GucSource) {
    let slot = &mut inner.slots[idx];
    if slot.value != value && slot.def.report {
        inner.reported.push((slot.def.name.to_string(), value.clone()));
    }
    slot.value = value;
    slot.source = source;
}

/// Undo one saved entry: the slot gets back its prior value and source.
fn restore(inner: &mut GucInner, entry: GucSaved) -> Result<()> {
    let idx = lookup(inner, &entry.name)?;
    put(inner, idx, entry.value, entry.source);
    Ok(())
}

/// Level index of the most recent savepoint called `name` (>= 1; the transaction level is never named).
fn savepoint_index(inner: &GucInner, name: &str) -> Result<usize> {
    inner
        .levels
        .iter()
        .rposition(|l| l.as_deref() == Some(name))
        .filter(|&i| i >= 1)
        .ok_or_else(|| PgError::new(INVALID_SAVEPOINT, format!("savepoint \"{name}\" does not exist")))
}

/// Record and apply an assignment at the current level (PG `set_config_option` + `push_old_value`).
/// Outside any transaction a session assignment is permanent; `SET LOCAL` has no lasting effect.
fn assign(inner: &mut GucInner, idx: usize, value: String, source: GucSource, scope: GucScope) {
    let level = inner.levels.len();
    if level > 0 {
        let name = inner.slots[idx].def.name.to_string();
        let prior = (inner.slots[idx].value.clone(), inner.slots[idx].source);
        let found = inner.saved.iter().position(|e| e.level == level && e.name == name);
        match (found, scope) {
            (Some(i), GucScope::Session) => {
                // Session value wins over an earlier SET LOCAL at this level: commit keeps it.
                let e = &mut inner.saved[i];
                e.local = false;
                e.masked = None;
            }
            (Some(i), GucScope::Local) => {
                // First SET LOCAL after a SET masks the session value; later ones change nothing.
                let e = &mut inner.saved[i];
                if !e.local {
                    e.local = true;
                    e.masked = Some(prior);
                }
            }
            (None, GucScope::Session) => {
                inner.saved.push(GucSaved { name, value: prior.0, source: prior.1, level, local: false, masked: None });
            }
            (None, GucScope::Local) => {
                inner.saved.push(GucSaved { name, value: prior.0, source: prior.1, level, local: true, masked: None });
            }
        }
    } else if scope == GucScope::Local {
        return;
    }
    put(inner, idx, value, source);
}

/// Undo every entry at `level` or above (ROLLBACK TO), newest first.
fn undo_from(inner: &mut GucInner, level: usize) -> Result<()> {
    let all = std::mem::take(&mut inner.saved);
    let (undo, keep): (Vec<GucSaved>, Vec<GucSaved>) = all.into_iter().partition(|e| e.level >= level);
    inner.saved = keep;
    for entry in undo.into_iter().rev() {
        restore(inner, entry)?;
    }
    Ok(())
}

/// Merge the entries of `level` into its parent level (RELEASE, and COMMIT of open subtransactions).
fn merge_level(inner: &mut GucInner, level: usize) {
    let all = std::mem::take(&mut inner.saved);
    let (children, mut rest): (Vec<GucSaved>, Vec<GucSaved>) = all.into_iter().partition(|e| e.level == level);
    let parent_level = level - 1;
    for child in children {
        let found = rest.iter().position(|e| e.level == parent_level && e.name == child.name);
        match found {
            Some(i) => {
                let merged = merge_entries(&rest[i], child);
                rest[i] = merged;
            }
            None => {
                let mut moved = child;
                moved.level = parent_level;
                rest.push(moved);
            }
        }
    }
    inner.saved = rest;
}

/// Combine a child-level entry into the parent-level entry for the same variable. The prior value
/// is the parent's; the resulting commit behaviour follows the newest session value.
fn merge_entries(parent: &GucSaved, child: GucSaved) -> GucSaved {
    let (local, masked) = if !child.local {
        (false, None)
    } else if let Some(m) = child.masked.clone() {
        (true, Some(m))
    } else if parent.local {
        (true, parent.masked.clone())
    } else {
        (true, Some((child.value.clone(), child.source)))
    };
    GucSaved {
        name: parent.name.clone(),
        value: parent.value.clone(),
        source: parent.source,
        level: parent.level,
        local,
        masked,
    }
}

// ---------------------------------------------------------------------------------------------
// Row builders
// ---------------------------------------------------------------------------------------------

/// Integer parameters are reported in base units (`pg_settings.setting`), others as stored.
fn base_text(def: &GucDef, text: &str) -> String {
    if def.vartype == GucType::Int {
        if let Ok(v) = guc_value::parse_int(def, text) {
            return v.to_string();
        }
    }
    text.to_string()
}

fn opt_text(s: &str) -> Option<String> {
    if s.is_empty() {
        None
    } else {
        Some(s.to_string())
    }
}

fn context_str(context: GucContext) -> &'static str {
    match context {
        GucContext::Internal => "internal",
        GucContext::Postmaster => "postmaster",
        GucContext::Sighup => "sighup",
        GucContext::SuperuserBackend => "superuser-backend",
        GucContext::Backend => "backend",
        GucContext::Superuser => "superuser",
        GucContext::User => "user",
    }
}

fn vartype_str(vartype: GucType) -> &'static str {
    match vartype {
        GucType::Bool => "bool",
        GucType::Int => "integer",
        GucType::Real => "real",
        GucType::String => "string",
        GucType::Enum => "enum",
    }
}

fn setting_row(slot: &GucSlot) -> SettingRow {
    let def = slot.def;
    SettingRow {
        name: def.name.to_string(),
        setting: Some(base_text(def, &slot.value)),
        unit: opt_text(def.unit),
        category: def.category.to_string(),
        short_desc: def.short_desc.to_string(),
        extra_desc: opt_text(def.extra_desc),
        context: context_str(def.context).to_string(),
        vartype: vartype_str(def.vartype).to_string(),
        source: slot.source.as_str().to_string(),
        min_val: opt_text(def.min_val),
        max_val: opt_text(def.max_val),
        enumvals: if def.vartype == GucType::Enum {
            Some(def.enumvals.iter().map(|s| s.to_string()).collect())
        } else {
            None
        },
        boot_val: Some(base_text(def, def.boot_val)),
        reset_val: Some(base_text(def, &slot.reset_val)),
        sourcefile: None,
        sourceline: None,
        pending_restart: false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn expect_err_sqlstate(result: Result<String>, code: &str) -> PgError {
        match result {
            Ok(v) => panic!("expected error {code}, got Ok({v})"),
            Err(e) => {
                assert_eq!(e.sqlstate.code(), code);
                e
            }
        }
    }

    #[test]
    fn datestyle_parts_and_defaults() {
        assert_eq!(parse_datestyle("SQL, DMY", "ISO, MDY").expect("valid"), "SQL, DMY");
        assert_eq!(parse_datestyle("SQL", "ISO, MDY").expect("valid"), "SQL, MDY");
        assert_eq!(parse_datestyle("postgres, dmy", "ISO, MDY").expect("valid"), "Postgres, DMY");
        assert_eq!(parse_datestyle("German", "ISO, MDY").expect("valid"), "German, DMY");
        assert_eq!(parse_datestyle("German, MDY", "ISO, DMY").expect("valid"), "German, MDY");
        assert_eq!(parse_datestyle("US", "ISO, DMY").expect("valid"), "ISO, MDY");
        assert_eq!(parse_datestyle("European", "SQL, MDY").expect("valid"), "SQL, DMY");
        assert_eq!(parse_datestyle("DEFAULT", "SQL, DMY").expect("valid"), "ISO, MDY");
        assert_eq!(parse_datestyle("", "SQL, DMY").expect("valid"), "SQL, DMY");
    }

    #[test]
    fn datestyle_errors() {
        let e = expect_err_sqlstate(parse_datestyle("foo", "ISO, MDY").map(|_| String::new()), "22023");
        assert_eq!(e.message, "invalid value for parameter \"DateStyle\": \"foo\"");
        assert_eq!(e.detail.as_deref(), Some("Unrecognized key word: \"foo\"."));
        let c = expect_err_sqlstate(parse_datestyle("iso, sql", "ISO, MDY").map(|_| String::new()), "22023");
        assert_eq!(c.detail.as_deref(), Some("Conflicting \"datestyle\" specifications."));
    }

    #[test]
    fn client_encoding_aliases() {
        assert_eq!(client_encoding_value("UTF-8").expect("alias"), "UTF8");
        assert_eq!(client_encoding_value("unicode").expect("alias"), "UTF8");
        assert_eq!(client_encoding_value("latin1").expect("latin1"), "LATIN1");
        let e = expect_err_sqlstate(client_encoding_value("NO_SUCH_ENCODING").map(|_| String::new()), "22023");
        assert_eq!(e.message, "invalid value for parameter \"client_encoding\": \"NO_SUCH_ENCODING\"");
    }

    #[test]
    fn isolation_names() {
        let def = find_def("transaction_isolation").expect("defined");
        assert_eq!(isolation_value(def, "Serializable").expect("level"), "serializable");
        let e = expect_err_sqlstate(isolation_value(def, "bogus").map(|_| String::new()), "22023");
        assert_eq!(e.hint.as_deref(), Some("Available values: serializable, repeatable read, read committed, read uncommitted."));
    }

    #[test]
    fn application_name_is_cleaned() {
        assert_eq!(clean_ascii("a\u{1}b c"), "a?b c");
        assert_eq!(clean_ascii("é"), "??");
    }

    #[test]
    fn merge_session_parent_with_local_child_masks_session_value() {
        let parent = GucSaved {
            name: "x".to_string(),
            value: "1".to_string(),
            source: GucSource::Default,
            level: 1,
            local: false,
            masked: None,
        };
        let child = GucSaved {
            name: "x".to_string(),
            value: "2".to_string(),
            source: GucSource::Session,
            level: 2,
            local: true,
            masked: None,
        };
        let merged = merge_entries(&parent, child);
        assert_eq!(merged.level, 1);
        assert_eq!(merged.value, "1");
        assert!(merged.local);
        assert_eq!(merged.masked, Some(("2".to_string(), GucSource::Session)));
    }

    #[test]
    fn startup_report_has_fifteen_entries_in_order() {
        let table = GucTable::new(true);
        let report = table.startup_report().expect("report");
        assert_eq!(report.len(), STARTUP_REPORT_ORDER.len());
        for (got, want) in report.iter().zip(STARTUP_REPORT_ORDER.iter()) {
            assert_eq!(got.0.to_ascii_lowercase(), want.to_ascii_lowercase());
        }
        let server_version = report.iter().find(|(n, _)| n == "server_version").map(|(_, v)| v.as_str());
        assert_eq!(server_version, Some("18.6"));
    }

    #[test]
    fn is_superuser_reflects_session_user() {
        assert_eq!(GucTable::new(true).show("is_superuser").expect("known"), "on");
        assert_eq!(GucTable::new(false).show("is_superuser").expect("known"), "off");
    }

    #[test]
    fn unknown_parameter_is_42704() {
        let table = GucTable::new(true);
        let e = table.show("foo").expect_err("unknown");
        assert_eq!(e.sqlstate.code(), "42704");
        assert_eq!(e.message, "unrecognized configuration parameter \"foo\"");
    }

    #[test]
    fn work_mem_boot_value_uses_units_and_placeholders_work() {
        let table = GucTable::new(true);
        assert_eq!(table.show("work_mem").expect("known"), "4MB");
        assert!(table.show("custom.my_guc").is_err());
        table.set("custom.my_guc", &["42".to_string()], GucScope::Session).expect("placeholder set");
        assert_eq!(table.show("custom.my_guc").expect("known now"), "42");
        table.reset("custom.my_guc", GucScope::Session).expect("placeholder reset");
        assert_eq!(table.show("custom.my_guc").expect("still known"), "");
    }

    #[test]
    fn show_all_excludes_hidden_and_is_sorted() {
        let rows = GucTable::new(true).show_all().expect("rows");
        assert_eq!(rows.len(), 399);
        assert!(rows.iter().all(|r| r.0 != "is_superuser"));
        assert!(rows.windows(2).all(|w| name_cmp(&w[0].0, &w[1].0) != Ordering::Greater));
    }

    #[test]
    fn take_reported_changes_keeps_last_value_per_name() {
        let table = GucTable::new(true);
        {
            let mut inner = table.inner.borrow_mut();
            inner.reported.push(("a".to_string(), "1".to_string()));
            inner.reported.push(("b".to_string(), "2".to_string()));
            inner.reported.push(("a".to_string(), "3".to_string()));
        }
        assert_eq!(
            table.take_reported_changes(),
            vec![("b".to_string(), "2".to_string()), ("a".to_string(), "3".to_string())]
        );
        assert!(table.take_reported_changes().is_empty());
    }

    #[test]
    fn savepoint_errors_are_3b001() {
        let table = GucTable::new(true);
        table.at_txn_start().expect("begin");
        let e = table.at_release("nope").expect_err("missing");
        assert_eq!(e.sqlstate.code(), "3B001");
        assert_eq!(e.message, "savepoint \"nope\" does not exist");
    }

    #[test]
    #[ignore = "needs API-01"]
    fn set_show_and_session_abort() {
        let table = GucTable::new(true);
        table.set("work_mem", &["4MB".to_string()], GucScope::Session).expect("set");
        assert_eq!(table.show("work_mem").expect("show"), "4MB");
        table.at_txn_start().expect("begin");
        table.set("work_mem", &["9MB".to_string()], GucScope::Session).expect("set");
        table.at_txn_end(false).expect("abort");
        assert_eq!(table.show("work_mem").expect("show"), "4MB");
    }

    #[test]
    #[ignore = "needs API-01"]
    fn set_local_reverts_at_commit() {
        let table = GucTable::new(true);
        table.at_txn_start().expect("begin");
        table.set("work_mem", &["8MB".to_string()], GucScope::Local).expect("set local");
        assert_eq!(table.show("work_mem").expect("show"), "8MB");
        table.at_txn_end(true).expect("commit");
        assert_eq!(table.show("work_mem").expect("show"), "4MB");
    }

    #[test]
    #[ignore = "needs API-01"]
    fn set_then_set_local_commit_keeps_session_value() {
        let table = GucTable::new(true);
        table.at_txn_start().expect("begin");
        table.set("work_mem", &["2MB".to_string()], GucScope::Session).expect("set");
        table.set("work_mem", &["8MB".to_string()], GucScope::Local).expect("set local");
        table.at_txn_end(true).expect("commit");
        assert_eq!(table.show("work_mem").expect("show"), "2MB");
    }

    #[test]
    #[ignore = "needs API-01"]
    fn savepoint_rollback_restores_inner_change() {
        let table = GucTable::new(true);
        table.at_txn_start().expect("begin");
        table.at_savepoint("s").expect("savepoint");
        table.set("work_mem", &["2MB".to_string()], GucScope::Session).expect("set");
        table.at_rollback_to("s").expect("rollback to");
        assert_eq!(table.show("work_mem").expect("show"), "4MB");
        table.at_txn_end(true).expect("commit");
    }

    #[test]
    #[ignore = "needs API-01"]
    fn set_errors_match_pg() {
        let table = GucTable::new(true);
        let e = table.set("foo", &["1".to_string()], GucScope::Session).expect_err("unknown");
        assert_eq!(e.sqlstate.code(), "42704");
        let e = table.set("max_connections", &["5".to_string()], GucScope::Session).expect_err("postmaster");
        assert_eq!(e.sqlstate.code(), "55P02");
        assert_eq!(e.message, "parameter \"max_connections\" cannot be changed without restarting the server");
        let e = table.set("server_version", &["x".to_string()], GucScope::Session).expect_err("internal");
        assert_eq!(e.message, "parameter \"server_version\" cannot be changed");
    }

    #[test]
    #[ignore = "needs API-01"]
    fn application_name_reports_cleaned_value() {
        let table = GucTable::new(true);
        table.set("application_name", &["x y".to_string()], GucScope::Session).expect("set");
        assert_eq!(table.take_reported_changes(), vec![("application_name".to_string(), "x y".to_string())]);
    }
}
