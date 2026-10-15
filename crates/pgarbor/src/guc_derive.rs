//! Typed views of the GUC table consumed by the engine layers (lease API-03). Pure functions over
//! [`GucTable`]; none caches (the table is cheap to read), the session calls them at statement start.
//!
//! * [`io_config`]: `pgarbor_types::IoConfig` from `DateStyle` (format + order), `IntervalStyle`,
//!   `extra_float_digits`, `bytea_output`, `TimeZone` (`pgarbor_types::datetime::tz::parse_timezone_setting`
//!   -> `TimeZoneSetting`; unparsable stored values fall back to UTC).
//! * [`search_path`]: `pgarbor_catalog::SearchPath::parse(show("search_path"))` (catalog `search_path.rs`).
//! * [`txn_defaults`]: `core::TxnOptions` for new transactions from `default_transaction_isolation`,
//!   `default_transaction_read_only`, `default_transaction_deferrable`.
//! * [`wait_limits`]: `core::WaitLimits{lock_timeout_ms, deadlock_timeout_ms, statement_deadline}` from
//!   `lock_timeout`, `deadlock_timeout`, and `statement_timeout` (deadline = `now + timeout`, 0 = none).
//! * [`plan_options`]: `pgarbor_plan::PlanOptions` from `enable_seqscan`, `enable_indexscan`,
//!   `enable_hashjoin`, `enable_mergejoin`, `enable_nestloop`, `enable_hashagg`, `enable_sort`,
//!   `enable_material`.
//! * [`idle_in_transaction_timeout_ms`]: `idle_in_transaction_session_timeout`.
//!
//! Values are read in their SHOW form, which the GUC layer (API-02) has already validated and
//! normalized on SET, so the parsers here only have to invert that normalization. A stored value that
//! does not parse is an internal inconsistency and is reported as XX000 rather than guessed at.
//! Reference: PG 18 `utils/adt/datetime.c` (`check_datestyle`), `utils/misc/guc.c` (`config_enum_lookup_by_name`,
//! boolean and integer GUC parsing), `access/transam/xact.c` (`DefaultXactIsoLevel`, `DefaultXactReadOnly`,
//! `DefaultXactDeferrable`).
use std::time::{Duration};
use pgarbor_common::time::{Instant};

use pgarbor_catalog::SearchPath;
use pgarbor_common::{PgError, Result, SqlState};
use pgarbor_core::{IsolationLevel, TxnKind, TxnOptions, WaitLimits};
use pgarbor_plan::PlanOptions;
use pgarbor_types::context::{ByteaOutput, DateOrder, DateStyle, IntervalStyle, TimeZoneSetting};
use pgarbor_types::datetime::tz::parse_timezone_setting;
use pgarbor_types::IoConfig;

use crate::guc::GucTable;
use crate::guc_def::{find_def, GucDef};
use crate::guc_value;

/// Typed view of the output-affecting GUCs (module docs).
pub fn io_config(g: &GucTable) -> Result<IoConfig> {
    let (date_style, date_order) = parse_date_style(&g.show("DateStyle")?)?;
    let interval_style = parse_interval_style(&g.show("IntervalStyle")?)?;
    let extra_float_digits = int_to_i32("extra_float_digits", int_setting(g, "extra_float_digits")?)?;
    let bytea_output = parse_bytea_output(&g.show("bytea_output")?)?;
    let time_zone = time_zone_setting(&g.show("TimeZone")?);
    Ok(IoConfig { date_style, date_order, interval_style, extra_float_digits, bytea_output, time_zone })
}

/// `search_path` as the catalog resolves names (`SearchPath::parse` of the SHOW form).
pub fn search_path(g: &GucTable) -> Result<SearchPath> {
    SearchPath::parse(&g.show("search_path")?)
}

/// Options for a new transaction, from the session defaults. `kind` is `Implicit`; the transaction
/// block code overrides it for BEGIN.
pub fn txn_defaults(g: &GucTable) -> Result<TxnOptions> {
    let iso_text = g.show("default_transaction_isolation")?;
    let isolation = IsolationLevel::parse(&iso_text)
        .map_err(|_| stored_error("default_transaction_isolation", &iso_text))?;
    let read_only = bool_setting(g, "default_transaction_read_only")?;
    let deferrable = bool_setting(g, "default_transaction_deferrable")?;
    Ok(TxnOptions { kind: TxnKind::Implicit, isolation, read_only, deferrable })
}

/// Lock and statement waits for a statement that started at `statement_start`. A `statement_timeout`
/// of 0 means no deadline; a deadline that cannot be represented as an `Instant` is treated as none.
pub fn wait_limits(g: &GucTable, statement_start: Instant) -> Result<WaitLimits> {
    let lock_timeout_ms = non_negative_ms(int_setting(g, "lock_timeout")?);
    let deadlock_timeout_ms = non_negative_ms(int_setting(g, "deadlock_timeout")?);
    let statement_ms = non_negative_ms(int_setting(g, "statement_timeout")?);
    let statement_deadline = if statement_ms == 0 {
        None
    } else {
        statement_start.checked_add(Duration::from_millis(statement_ms))
    };
    Ok(WaitLimits { lock_timeout_ms, deadlock_timeout_ms, statement_deadline })
}

/// Planner enable flags from the `enable_*` GUCs; the remaining planner options keep their defaults.
pub fn plan_options(g: &GucTable) -> Result<PlanOptions> {
    Ok(PlanOptions {
        enable_seqscan: bool_setting(g, "enable_seqscan")?,
        enable_indexscan: bool_setting(g, "enable_indexscan")?,
        enable_hashjoin: bool_setting(g, "enable_hashjoin")?,
        enable_mergejoin: bool_setting(g, "enable_mergejoin")?,
        enable_nestloop: bool_setting(g, "enable_nestloop")?,
        enable_hashagg: bool_setting(g, "enable_hashagg")?,
        enable_sort: bool_setting(g, "enable_sort")?,
        enable_material: bool_setting(g, "enable_material")?,
        ..PlanOptions::default()
    })
}

/// `idle_in_transaction_session_timeout` in milliseconds (0 = disabled).
pub fn idle_in_transaction_timeout_ms(g: &GucTable) -> Result<u64> {
    Ok(non_negative_ms(int_setting(g, "idle_in_transaction_session_timeout")?))
}

// ---------------------------------------------------------------------------------------------
// Helpers

/// Error for a stored value that the GUC layer should never have accepted.
fn stored_error(name: &str, value: &str) -> PgError {
    PgError::new(SqlState::INTERNAL_ERROR, format!("invalid stored value for parameter \"{name}\": \"{value}\""))
}

/// Definition of a parameter by name (case-insensitive). Every name used here is in the generated table.
fn def_of(name: &str) -> Result<&'static GucDef> {
    find_def(name).ok_or_else(|| PgError::new(SqlState::INTERNAL_ERROR, format!("unknown parameter \"{name}\"")))
}

/// Integer parameter in its base unit (`guc_value::parse_int` accepts the stored form with its unit).
fn int_setting(g: &GucTable, name: &str) -> Result<i64> {
    let def = def_of(name)?;
    let text = g.show(name)?;
    guc_value::parse_int(def, &text)
}

/// Boolean parameter; the stored form is `on` / `off`.
fn bool_setting(g: &GucTable, name: &str) -> Result<bool> {
    let text = g.show(name)?;
    match guc_value::parse_bool(&text)? {
        Some(b) => Ok(b),
        None => Err(stored_error(name, &text)),
    }
}

fn non_negative_ms(v: i64) -> u64 {
    u64::try_from(v).unwrap_or(0)
}

fn int_to_i32(name: &str, v: i64) -> Result<i32> {
    i32::try_from(v).map_err(|_| stored_error(name, &v.to_string()))
}

/// `DateStyle` SHOW form: `format, order`, e.g. `ISO, MDY`, `SQL, DMY`, `German, DMY`, `Postgres, YMD`.
/// Tokens are matched case-insensitively and in any order; the PG aliases `European` (DMY),
/// `US` and `NonEuropean` (MDY) are accepted as well. A part that is absent keeps its default
/// (ISO / MDY), as PG does for a partial SET.
fn parse_date_style(text: &str) -> Result<(DateStyle, DateOrder)> {
    let mut style = DateStyle::Iso;
    let mut order = DateOrder::Mdy;
    for token in text.split(',') {
        let t = token.trim();
        if t.is_empty() {
            continue;
        }
        match t.to_ascii_lowercase().as_str() {
            "iso" => style = DateStyle::Iso,
            "sql" => style = DateStyle::Sql,
            "postgres" => style = DateStyle::Postgres,
            "german" => style = DateStyle::German,
            "mdy" | "us" | "noneuropean" => order = DateOrder::Mdy,
            "dmy" | "european" => order = DateOrder::Dmy,
            "ymd" => order = DateOrder::Ymd,
            _ => return Err(stored_error("DateStyle", text)),
        }
    }
    Ok((style, order))
}

/// `IntervalStyle` SHOW form (enum labels: `postgres`, `postgres_verbose`, `sql_standard`, `iso_8601`).
fn parse_interval_style(text: &str) -> Result<IntervalStyle> {
    match text.trim().to_ascii_lowercase().as_str() {
        "postgres" => Ok(IntervalStyle::Postgres),
        "postgres_verbose" => Ok(IntervalStyle::PostgresVerbose),
        "sql_standard" => Ok(IntervalStyle::SqlStandard),
        "iso_8601" => Ok(IntervalStyle::Iso8601),
        _ => Err(stored_error("IntervalStyle", text)),
    }
}

/// `bytea_output` SHOW form (`hex` or `escape`).
fn parse_bytea_output(text: &str) -> Result<ByteaOutput> {
    match text.trim().to_ascii_lowercase().as_str() {
        "hex" => Ok(ByteaOutput::Hex),
        "escape" => Ok(ByteaOutput::Escape),
        _ => Err(stored_error("bytea_output", text)),
    }
}

/// `TimeZone` as a session setting. A value that does not parse falls back to UTC (module docs).
fn time_zone_setting(text: &str) -> TimeZoneSetting {
    parse_timezone_setting(text).unwrap_or(TimeZoneSetting::Utc)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn date_style_text_maps_to_format_and_order() {
        assert_eq!(parse_date_style("ISO, MDY").unwrap(), (DateStyle::Iso, DateOrder::Mdy));
        assert_eq!(parse_date_style("SQL, DMY").unwrap(), (DateStyle::Sql, DateOrder::Dmy));
        assert_eq!(parse_date_style("German, DMY").unwrap(), (DateStyle::German, DateOrder::Dmy));
        assert_eq!(parse_date_style("Postgres, YMD").unwrap(), (DateStyle::Postgres, DateOrder::Ymd));
        assert_eq!(parse_date_style("iso, european").unwrap(), (DateStyle::Iso, DateOrder::Dmy));
        assert_eq!(parse_date_style("SQL, US").unwrap(), (DateStyle::Sql, DateOrder::Mdy));
        assert!(parse_date_style("foo").is_err());
    }

    #[test]
    fn interval_and_bytea_labels() {
        assert_eq!(parse_interval_style("postgres").unwrap(), IntervalStyle::Postgres);
        assert_eq!(parse_interval_style("postgres_verbose").unwrap(), IntervalStyle::PostgresVerbose);
        assert_eq!(parse_interval_style("sql_standard").unwrap(), IntervalStyle::SqlStandard);
        assert_eq!(parse_interval_style("iso_8601").unwrap(), IntervalStyle::Iso8601);
        assert!(parse_interval_style("verbose").is_err());
        assert_eq!(parse_bytea_output("hex").unwrap(), ByteaOutput::Hex);
        assert_eq!(parse_bytea_output("escape").unwrap(), ByteaOutput::Escape);
        assert!(parse_bytea_output("base64").is_err());
    }

    #[test]
    fn time_zone_utc_and_fallback() {
        assert_eq!(time_zone_setting("UTC"), TimeZoneSetting::Utc);
    }

    #[test]
    fn helpers_map_ranges() {
        assert_eq!(non_negative_ms(-5), 0);
        assert_eq!(non_negative_ms(1500), 1500);
        assert_eq!(int_to_i32("extra_float_digits", 3).unwrap(), 3);
        assert!(int_to_i32("extra_float_digits", i64::MAX).is_err());
    }

    #[test]
    #[ignore = "needs API-02"]
    fn default_table_gives_default_io_config() {
        let g = GucTable::new(true);
        assert_eq!(io_config(&g).unwrap(), IoConfig::default());
    }

    #[test]
    #[ignore = "needs API-02"]
    fn set_datestyle_changes_format_and_order() {
        let g = GucTable::new(true);
        g.set("DateStyle", &["SQL, DMY".to_string()], crate::guc::GucScope::Session).unwrap();
        let cfg = io_config(&g).unwrap();
        assert_eq!(cfg.date_style, DateStyle::Sql);
        assert_eq!(cfg.date_order, DateOrder::Dmy);
    }

    #[test]
    #[ignore = "needs API-02"]
    fn statement_timeout_gives_deadline() {
        let g = GucTable::new(true);
        g.set("statement_timeout", &["1s".to_string()], crate::guc::GucScope::Session).unwrap();
        let start = Instant::now();
        let limits = wait_limits(&g, start).unwrap();
        let deadline = limits.statement_deadline.unwrap();
        assert_eq!(deadline.duration_since(start), Duration::from_millis(1000));
    }

    #[test]
    #[ignore = "needs API-02"]
    fn serializable_default_isolation() {
        let g = GucTable::new(true);
        g.set("default_transaction_isolation", &["serializable".to_string()], crate::guc::GucScope::Session).unwrap();
        let opts = txn_defaults(&g).unwrap();
        assert_eq!(opts.isolation, IsolationLevel::Serializable);
        assert!(!opts.read_only);
        assert!(!opts.deferrable);
    }

    #[test]
    #[ignore = "needs API-02"]
    fn default_plan_options_and_timeouts() {
        let g = GucTable::new(true);
        let po = plan_options(&g).unwrap();
        assert_eq!(po.enable_seqscan, PlanOptions::default().enable_seqscan);
        assert_eq!(idle_in_transaction_timeout_ms(&g).unwrap(), 0);
        let limits = wait_limits(&g, Instant::now()).unwrap();
        assert_eq!(limits.lock_timeout_ms, 0);
        assert_eq!(limits.deadlock_timeout_ms, 1000);
        assert!(limits.statement_deadline.is_none());
    }
}
