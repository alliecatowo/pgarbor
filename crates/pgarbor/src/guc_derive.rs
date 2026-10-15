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
use std::time::Instant;

use pgarbor_common::{PgError, Result};
use pgarbor_core::{TxnOptions, WaitLimits};
use pgarbor_plan::PlanOptions;
use pgarbor_types::IoConfig;

use crate::guc::GucTable;

#[allow(unused_variables)]
pub fn io_config(g: &GucTable) -> Result<IoConfig> {
    Err(PgError::unimplemented("API-03", "IoConfig from GUCs (guc_derive::io_config)"))
}

#[allow(unused_variables)]
pub fn search_path(g: &GucTable) -> Result<pgarbor_catalog::SearchPath> {
    Err(PgError::unimplemented("API-03", "search_path from GUCs (guc_derive::search_path)"))
}

#[allow(unused_variables)]
pub fn txn_defaults(g: &GucTable) -> Result<TxnOptions> {
    Err(PgError::unimplemented("API-03", "transaction defaults from GUCs (guc_derive::txn_defaults)"))
}

#[allow(unused_variables)]
pub fn wait_limits(g: &GucTable, statement_start: Instant) -> Result<WaitLimits> {
    Err(PgError::unimplemented("API-03", "wait limits from GUCs (guc_derive::wait_limits)"))
}

#[allow(unused_variables)]
pub fn plan_options(g: &GucTable) -> Result<PlanOptions> {
    Err(PgError::unimplemented("API-03", "planner options from GUCs (guc_derive::plan_options)"))
}

#[allow(unused_variables)]
pub fn idle_in_transaction_timeout_ms(g: &GucTable) -> Result<u64> {
    Err(PgError::unimplemented("API-03", "idle timeout from GUCs (guc_derive::idle_in_transaction_timeout_ms)"))
}
