//! One statement through the whole pipeline (lease API-08, north-star): parse result -> bind -> plan ->
//! execute, plus session-level statements and the DDL retry loop. The simple-query driver
//! (`simple_query`), the extended protocol (`extended`/`portal`) and `prepared` all call into here.
//!
//! ## `run_statement(raw, params)` (simple protocol; `params` empty)
//! Precondition: the caller ran `ensure_txn` (state `Started`/`ImplicitBlock`/`InBlock`/`Failed`).
//! 1. **Activity / limits**: `shared.update_activity(|a| { a.state = Active; a.query = <whole query text>;
//!    a.query_start_ms = now })`; `interrupts().begin_statement(deadline)` with `guc_derive::wait_limits`
//!    (`statement_timeout`); `txn.set_limits(..)`. On exit (success or error): `end_statement`, activity back to
//!    `idle` / `idle in transaction` / `idle in transaction (aborted)` (`BackendState::after_statement`).
//! 2. **Failed block**: before binding anything else, if `block == Failed`: bind ONLY with the cached
//!    env (`self.env`, no catalog access needed for transaction control) to learn whether the statement is
//!    COMMIT/ROLLBACK/ROLLBACK TO; others fail with `core::error::in_failed_transaction()` (25P02). A
//!    statement that cannot be bound without a snapshot while failed is rejected 25P02 as well.
//! 3. **Snapshot**: statements that read or write data or catalog (everything except transaction control,
//!    SET/RESET/SHOW, DISCARD, PREPARE/DEALLOCATE of known names, LISTEN/NOTIFY, SET ROLE...) call
//!    `txn.begin_statement()` (READ COMMITTED takes a fresh snapshot per statement) BEFORE `refresh_env`.
//!    Session-control statements must NOT call it, so `SET TRANSACTION ISOLATION LEVEL` stays legal after
//!    `SET`.
//! 4. **Bind**: `pgarbor_sql::bind(raw, &bind_context(&env, declared))` -> `BindOutput{statement,
//!    param_types, notices}`; notices go to the statement output.
//! 5. **Dispatch** on the bound statement:
//!    * `Transaction(t)` -> `txn_block::exec_transaction_stmt`;
//!    * `Set/Reset/Show/SetRole/SetSessionAuthorization/Discard/Listen/Unlisten/Notify` ->
//!      `session_utility::*`; `Prepare/Execute/Deallocate` -> `prepared::*`;
//!    * `Lock` additionally requires a transaction block (`require_transaction_block("LOCK TABLE")`);
//!      `Maintenance` of `Vacuum` inside a block: 25001 `VACUUM cannot run inside a transaction block`;
//!      `CreateDatabase`/`DropDatabase`: 25001 `CREATE DATABASE cannot run inside a transaction block`
//!      (then the executor's 0A000);
//!    * `Explain` whose inner statement is `Execute`: substitute the prepared statement's bound inner
//!      statement and parameter values before calling the executor;
//!    * plannable (`is_plannable`): `pgarbor_plan::plan(&stmt, &plan_context(.., param_values: Some(values)
//!      if any parameter else None, ..))` -> `Plan`;
//!    * everything else: straight to the executor.
//! 6. **Execute**: `with_exec(command, ..., |cx| pgarbor_exec::execute_with_channel(&stmt, plan.as_ref(), cx,
//!    chan))` where `chan` is the COPY channel installed by the wire layer (`Session::set_copy_channel`,
//!    default: `NoCopyChannel`, so `COPY ... STDIN/STDOUT` without a channel fails 0A000). `ExecResult` -> `StatementResult` (columns via `column_infos`; for `Rows`
//!    results from `Query` statements the binder's `OutputColumn` names/types equal the plan's).
//! 7. **Catalog retry** (READ COMMITTED only): if execution fails with `pgarbor_exec::error::
//!    is_catalog_retry(&e)` and the isolation level is READ COMMITTED/UNCOMMITTED and fewer than 8
//!    attempts were made: `txn.begin_statement()` (fresh snapshot), `refresh_env`, re-bind, re-plan,
//!    re-execute. Any other error propagates (the caller aborts per `txn_block`).
//! 8. **After success**: `guc.take_reported_changes()` -> `param_status`, pending notices, `listen`/NOTIFY
//!    stay queued until commit. Row-returning results are materialized (`Vec<Vec<Datum>>`).
//! Statement text for `pg_stat_activity.query`: the WHOLE submitted string (`raw.source.text()`).
use pgarbor_common::{PgError, Result};
use pgarbor_sql::{BindOutput, RawStatement};
use pgarbor_types::{Datum, TypeId};

use crate::session::Session;
use crate::types::StatementOutput;

/// Parameters for one execution (empty for the simple protocol).
// CONTRACT
#[derive(Clone, Debug, Default)]
pub struct ParamValues {
    /// Final parameter types ($1..$n); for the simple protocol empty.
    pub types: Vec<TypeId>,
    pub values: Vec<Datum>,
}

impl Session {
    /// Run one parsed statement end to end (module docs). Returns the output with notices and
    /// ParameterStatus changes. Errors are NOT cleaned up here: the caller applies
    /// `abort_after_error`.
    #[allow(unused_variables)]
    pub(crate) fn run_statement(&mut self, raw: &RawStatement, params: &ParamValues) -> Result<StatementOutput> {
        Err(PgError::unimplemented("API-08", "statement pipeline (Session::run_statement)"))
    }

    /// Bind `raw` against the current env with `declared` parameter types (module docs step 4).
    #[allow(unused_variables)]
    pub(crate) fn bind_statement(&mut self, raw: &RawStatement, declared: &[TypeId]) -> Result<BindOutput> {
        Err(PgError::unimplemented("API-08", "statement binding (Session::bind_statement)"))
    }
}
