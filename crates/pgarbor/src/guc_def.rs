//! Static description of one configuration parameter (PostgreSQL 18.6 `pg_settings` row) and the
//! table generated from the oracle (`guc_table.rs`, produced by `gen/gen_guc.py`). Fully implemented,
//! frozen (v1).
//!
//! The generated table contains EVERY GUC of PG 18.6 (399 rows) with exact names (mixed case such as
//! `DateStyle`, `TimeZone`, `IntervalStyle`), categories, descriptions, contexts, types, ranges, enum
//! values and boot values. PGArbor overrides a few defaults (see `gen/gen_guc.py`): `server_version`
//! `18.6`, `application_name` empty, `client_encoding` `UTF8`, `TimeZone` `UTC`, `server_encoding`
//! `UTF8`, `data_directory` empty, `lc_*` `C.UTF-8`. Parameters PGArbor does not implement behaviorally
//! (autovacuum, WAL, replication, ...) are accepted by SET (when their context allows) and stored,
//! exactly like PG stores them; they have no effect.
use std::collections::HashMap;
use std::sync::OnceLock;

/// When a parameter may be changed (`pg_settings.context`).
// CONTRACT
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum GucContext {
    /// Read-only (`server_version`, `server_encoding`, `max_index_keys`, ...): SET fails with 55P02
    /// `parameter "x" cannot be changed`.
    Internal,
    /// Needs a server restart: 55P02 `parameter "x" cannot be changed without restarting the server`.
    Postmaster,
    /// Needs a config reload: 55P02 `parameter "x" cannot be changed now`.
    Sighup,
    /// Settable only at connection start (`ignore_system_table_changes`...): after startup
    /// 55P02 `parameter "x" cannot be set after connection start`.
    SuperuserBackend,
    Backend,
    /// Settable by superusers only (PGArbor's default role is a superuser) - else 42501
    /// `permission denied to set parameter "x"`.
    Superuser,
    /// Settable by anyone.
    User,
}

/// Value type (`pg_settings.vartype`).
// CONTRACT
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum GucType {
    Bool,
    Int,
    Real,
    String,
    Enum,
}

/// One parameter definition.
// CONTRACT
#[derive(Clone, Copy, Debug)]
pub struct GucDef {
    /// Canonical name (case as PG prints it: `DateStyle`). Lookup is case-insensitive.
    pub name: &'static str,
    pub category: &'static str,
    pub short_desc: &'static str,
    pub extra_desc: &'static str,
    pub context: GucContext,
    pub vartype: GucType,
    /// Unit of integer/real values (`kB`, `8kB`, `ms`, `s`, `min`, `B`, `MB`) or "".
    pub unit: &'static str,
    /// Range as text ("" = none).
    pub min_val: &'static str,
    pub max_val: &'static str,
    /// Allowed enum labels in `pg_settings.enumvals` order.
    pub enumvals: &'static [&'static str],
    /// Default value in SHOW (display) form.
    pub boot_val: &'static str,
    /// Reported to the client with ParameterStatus when it changes (PG `GUC_REPORT`).
    pub report: bool,
}

pub use crate::guc_table::GUC_DEFS;

const fn hidden(
    name: &'static str,
    context: GucContext,
    vartype: GucType,
    boot_val: &'static str,
    short_desc: &'static str,
    report: bool,
) -> GucDef {
    GucDef {
        name,
        category: "Preset Options",
        short_desc,
        extra_desc: "",
        context,
        vartype,
        unit: "",
        min_val: "",
        max_val: "",
        enumvals: &[],
        boot_val,
        report,
    }
}

/// Parameters PG hides from `pg_settings` / `SHOW ALL` (flags `GUC_NO_SHOW_ALL`) but that `SHOW name`,
/// `SET` and ParameterStatus know: `is_superuser` (read-only, reported), `session_authorization`
/// (settable via `SET SESSION AUTHORIZATION`, reported), `role` (`SET ROLE`; default `none`).
pub static HIDDEN_DEFS: &[GucDef] = &[
    hidden("is_superuser", GucContext::Internal, GucType::Bool, "on", "Shows whether the current user is a superuser.", true),
    hidden(
        "session_authorization",
        GucContext::User,
        GucType::String,
        "postgres",
        "Sets the session user name.",
        true,
    ),
    hidden("role", GucContext::User, GucType::String, "none", "Sets the current role.", false),
];

fn index() -> &'static HashMap<String, &'static GucDef> {
    static IDX: OnceLock<HashMap<String, &'static GucDef>> = OnceLock::new();
    IDX.get_or_init(|| GUC_DEFS.iter().chain(HIDDEN_DEFS.iter()).map(|d| (d.name.to_ascii_lowercase(), d)).collect())
}

/// Definition by (case-insensitive) name, including the hidden ones.
pub fn find_def(name: &str) -> Option<&'static GucDef> {
    index().get(&name.to_ascii_lowercase()).copied()
}

/// All definitions visible in `pg_settings` / `SHOW ALL` (sorted by name as PG sorts them).
pub fn visible_defs() -> &'static [GucDef] {
    GUC_DEFS
}

/// The ParameterStatus set sent at startup, in the order the oracle sends it.
pub const STARTUP_REPORT_ORDER: &[&str] = &[
    "in_hot_standby",
    "integer_datetimes",
    "TimeZone",
    "IntervalStyle",
    "search_path",
    "is_superuser",
    "application_name",
    "default_transaction_read_only",
    "scram_iterations",
    "DateStyle",
    "standard_conforming_strings",
    "session_authorization",
    "client_encoding",
    "server_version",
    "server_encoding",
];

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn table_is_complete_and_findable() {
        assert!(GUC_DEFS.len() >= 399);
        assert_eq!(find_def("datestyle").map(|d| d.name), Some("DateStyle"));
        assert_eq!(find_def("TIMEZONE").map(|d| d.name), Some("TimeZone"));
        assert!(find_def("no_such_guc").is_none());
        for n in STARTUP_REPORT_ORDER {
            assert!(find_def(n).is_some(), "{n} missing");
        }
    }
}
