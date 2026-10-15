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
use std::sync::Arc;
use std::time::{Instant, SystemTime, UNIX_EPOCH};

use pgarbor_catalog::ddl::drop_stmt::DropKind;
use pgarbor_common::{PgError, Result};
use pgarbor_core::types::{IsolationLevel, TxnState};
use pgarbor_core::{BackendState, Txn};
use pgarbor_exec::error::is_catalog_retry;
use pgarbor_exec::eval::eval_standalone;
use pgarbor_exec::execute::NoCopyChannel;
use pgarbor_exec::{execute_with_channel, ExecResult};
use pgarbor_plan::Plan;
use pgarbor_sql::classify::{classify_parsed, StmtClass, StmtFamily};
use pgarbor_sql::extra_stmt::ExtraStatement;
use pgarbor_sql::ir::{BoundExecute, BoundExplain, BoundStatement, CopyTarget, MaintenanceKind};
use pgarbor_sql::{BindOutput, RawStatement};
use pgarbor_types::{Datum, TypeId};

use crate::guc_derive;
use crate::session::Session;
use crate::txn_block::{allowed_in_failed_block, BlockState};
use crate::types::{StatementOutput, StatementResult};

/// Parameters for one execution (empty for the simple protocol).
// CONTRACT
#[derive(Clone, Debug, Default)]
pub struct ParamValues {
    /// Final parameter types ($1..$n); for the simple protocol empty.
    pub types: Vec<TypeId>,
    pub values: Vec<Datum>,
}

/// Bind/plan/execute attempts of one statement under READ COMMITTED (module docs, step 7).
const MAX_CATALOG_ATTEMPTS: u32 = 8;

impl Session {
    /// Run one parsed statement end to end (module docs). Returns the output with notices and
    /// ParameterStatus changes. Errors are NOT cleaned up here: the caller applies
    /// `abort_after_error`.
    pub(crate) fn run_statement(&mut self, raw: &RawStatement, params: &ParamValues) -> Result<StatementOutput> {
        let shared = Arc::clone(self.handle.shared());
        let started = now_unix_ms();
        let text = raw.source.text().to_string();
        shared.update_activity(|a| {
            a.state = BackendState::Active;
            a.query = text;
            a.query_start_ms = Some(started);
            a.state_change_ms = started;
        });

        let outcome = self.run_statement_inner(raw, params);

        shared.interrupts.end_statement();
        let finished = now_unix_ms();
        // An error inside an explicit block is marked failed by the caller (`abort_after_error`), so
        // the backend reports the aborted state already.
        let state = match &outcome {
            Err(_) if self.block == BlockState::InBlock => BackendState::IdleInTransactionAborted,
            _ => BackendState::after_statement(self.txn.as_ref().map_or(TxnState::Idle, |t| t.state())),
        };
        shared.update_activity(|a| {
            a.state = state;
            a.state_change_ms = finished;
            a.wait = None;
        });
        outcome
    }

    /// Bind `raw` against the current env with `declared` parameter types (module docs step 4).
    pub(crate) fn bind_statement(&mut self, raw: &RawStatement, declared: &[TypeId]) -> Result<BindOutput> {
        let env = self
            .env
            .as_ref()
            .ok_or_else(|| PgError::internal("statement bound without a catalog environment"))?;
        let ctx = self.bind_context(env, declared.to_vec())?;
        pgarbor_sql::bind(raw, &ctx)
    }

    /// Everything of `run_statement` between the activity bookkeeping (module docs, steps 1-8).
    fn run_statement_inner(&mut self, raw: &RawStatement, params: &ParamValues) -> Result<StatementOutput> {
        let limits = guc_derive::wait_limits(&self.guc, Instant::now())?;
        self.handle.shared().interrupts.begin_statement(limits.statement_deadline);
        if let Some(txn) = self.txn.as_mut() {
            txn.set_limits(limits);
        }

        let mut notices: Vec<PgError> = Vec::new();
        let mut attempt: u32 = 0;
        let result = loop {
            attempt += 1;
            notices.clear();
            match self.run_attempt(raw, params, &mut notices) {
                Ok(result) => break result,
                Err(e) if attempt < MAX_CATALOG_ATTEMPTS && is_catalog_retry(&e) && self.read_committed() => continue,
                Err(e) => {
                    // Notices raised before the failure stay queued for the caller (sent before the error).
                    self.pending_notices.append(&mut notices);
                    return Err(e);
                }
            }
        };

        // Notices in wire order: bind and executor notices (already in `notices`), then the notices the
        // transaction-control layer queued while running this statement.
        notices.append(&mut self.pending_notices);
        let param_status = self.guc.take_reported_changes();
        Ok(StatementOutput { result, notices, param_status })
    }

    /// One bind/plan/execute attempt (steps 2-6). Notices are appended to `notices` as they arise.
    fn run_attempt(
        &mut self,
        raw: &RawStatement,
        params: &ParamValues,
        notices: &mut Vec<PgError>,
    ) -> Result<StatementResult> {
        // Step 2 (failed block) and steps 3-4 (snapshot, catalog refresh, bind).
        let bound = if self.block == BlockState::Failed {
            match self.bind_statement(raw, &params.types) {
                Ok(out) if allowed_in_failed_block(&out.statement) => out,
                _ => return Err(pgarbor_core::error::in_failed_transaction()),
            }
        } else {
            if statement_needs_snapshot(raw) {
                self.live_txn()?.begin_statement()?;
            }
            self.refresh_env()?;
            self.bind_statement(raw, &params.types)?
        };
        let BindOutput { statement, param_types, notices: bind_notices } = bound;
        notices.extend(bind_notices);

        // Step 5: session-level statements never reach the executor.
        if let Some(result) = self.exec_session_statement(&statement)? {
            return Ok(result);
        }

        match &statement {
            BoundStatement::Lock(_) => self.require_transaction_block("LOCK TABLE")?,
            BoundStatement::Maintenance { kind: MaintenanceKind::Vacuum, .. } => {
                self.prevent_in_transaction_block("VACUUM")?
            }
            BoundStatement::CreateDatabase(_) => self.prevent_in_transaction_block("CREATE DATABASE")?,
            BoundStatement::DropDatabase { .. } => self.prevent_in_transaction_block("DROP DATABASE")?,
            _ => {}
        }

        let mut statement = statement;
        let mut values = params.values.clone();
        let mut types = param_types;

        // EXPLAIN EXECUTE: run the prepared statement's inner statement under EXPLAIN.
        let substituted = match &statement {
            BoundStatement::Explain(explain) => match explain.statement.as_ref() {
                BoundStatement::Execute(exe) => {
                    let (inner, vals, ptypes) = self.explain_execute_parts(exe, raw)?;
                    let explain_stmt = BoundStatement::Explain(Box::new(BoundExplain {
                        options: explain.options.clone(),
                        statement: Box::new(inner),
                    }));
                    Some((explain_stmt, vals, ptypes))
                }
                _ => None,
            },
            _ => None,
        };
        if let Some((explain_stmt, vals, ptypes)) = substituted {
            statement = explain_stmt;
            values = vals;
            types = ptypes;
        }

        // Step 5 (plannable statements) and step 6 (execute).
        let plan = self.plan_statement(&statement, &types, &values)?;
        let env = self
            .env
            .clone()
            .ok_or_else(|| PgError::internal("statement executed without a catalog environment"))?;
        let command = command_name(&statement);
        let mut channel = self.copy_channel.take();
        let executed = self.with_exec(command, &env, values, types.clone(), |cx| match channel.as_deref_mut() {
            Some(chan) => execute_with_channel(&statement, plan.as_ref(), cx, chan),
            None => execute_with_channel(&statement, plan.as_ref(), cx, &mut NoCopyChannel),
        });
        // Put the COPY channel back on success and on error alike.
        self.copy_channel = channel;
        let (exec_result, side) = executed?;
        notices.extend(side.notices);

        match exec_result {
            ExecResult::Empty => Ok(StatementResult::Empty),
            ExecResult::Command { tag } => Ok(StatementResult::Command { tag }),
            ExecResult::Rows { desc, rows, tag } => {
                let columns = self.column_infos(&env, &desc)?;
                Ok(StatementResult::Rows { columns, rows, tag })
            }
        }
    }

    /// Plan `statement` when it reads data (module docs, step 5): plannable statements, and the query
    /// of `COPY (query) TO`. `None` for everything the executor runs without a plan.
    fn plan_statement(&self, statement: &BoundStatement, types: &[TypeId], values: &[Datum]) -> Result<Option<Plan>> {
        let copy_query: Option<BoundStatement> = match statement {
            BoundStatement::Copy(copy) => match &copy.target {
                CopyTarget::Query(q) => Some(BoundStatement::Query(q.clone())),
                CopyTarget::Table { .. } => None,
            },
            _ => None,
        };
        let to_plan: &BoundStatement = match &copy_query {
            Some(q) => q,
            None if statement.is_plannable() => statement,
            None => return Ok(None),
        };
        let env = self
            .env
            .as_ref()
            .ok_or_else(|| PgError::internal("statement planned without a catalog environment"))?;
        let param_values = if values.is_empty() { None } else { Some(values.to_vec()) };
        let ctx = self.plan_context(env, types.to_vec(), param_values, false)?;
        Ok(Some(pgarbor_plan::plan(to_plan, &ctx)?))
    }

    /// For `EXPLAIN EXECUTE name (args)`: the prepared statement's bound inner statement, the evaluated
    /// argument values and the parameter types (module docs, step 5).
    fn explain_execute_parts(
        &mut self,
        exe: &BoundExecute,
        raw: &RawStatement,
    ) -> Result<(BoundStatement, Vec<Datum>, Vec<TypeId>)> {
        let prepared = self.ensure_current(&exe.name)?;
        let inner = prepared.bound.clone().ok_or_else(|| {
            PgError::internal(format!("prepared statement \"{}\" has no statement to explain", exe.name))
        })?;
        let coerced = {
            let env = self
                .env
                .as_ref()
                .ok_or_else(|| PgError::internal("EXPLAIN EXECUTE without a catalog environment"))?;
            let ctx = self.bind_context(env, Vec::new())?;
            pgarbor_sql::binder::utility::coerce_execute_args(
                &ctx,
                &raw.source,
                &exe.name,
                exe.args.clone(),
                &prepared.param_types,
            )?
        };
        let env = self
            .env
            .clone()
            .ok_or_else(|| PgError::internal("EXPLAIN EXECUTE without a catalog environment"))?;
        let (values, _side) = self.with_exec("EXECUTE", &env, Vec::new(), Vec::new(), |cx| {
            let mut out = Vec::with_capacity(coerced.len());
            for arg in &coerced {
                out.push(eval_standalone(cx, arg, &[])?);
            }
            Ok(out)
        })?;
        Ok((inner, values, prepared.param_types))
    }

    /// The live core transaction (present whenever a statement runs; `ensure_txn` made it).
    fn live_txn(&mut self) -> Result<&mut Txn> {
        self.txn.as_mut().ok_or_else(|| PgError::internal("statement started without a transaction"))
    }

    /// True when the live transaction is READ COMMITTED or READ UNCOMMITTED (fresh snapshot per statement,
    /// eligible for the catalog retry loop).
    fn read_committed(&self) -> bool {
        matches!(
            self.txn.as_ref().map(|t| t.isolation()),
            Some(IsolationLevel::ReadCommitted | IsolationLevel::ReadUncommitted)
        )
    }
}

/// Statements that need a statement snapshot before binding (module docs, step 3). Transaction control,
/// SET/RESET/SHOW (incl. SET ROLE, SET TRANSACTION), DISCARD, LISTEN/UNLISTEN/NOTIFY, DEALLOCATE and
/// SET CONSTRAINTS take none, so `SET TRANSACTION ISOLATION LEVEL` stays legal after `SET`. Everything
/// else (data, catalog, PREPARE which binds its inner statement, EXECUTE, LOCK, ...) takes one. Statements
/// that do not classify as handled take one too, so their binder error is raised against a fresh catalog.
fn statement_needs_snapshot(raw: &RawStatement) -> bool {
    if matches!(raw.as_extra(), Some(ExtraStatement::SetConstraints { .. })) {
        return false;
    }
    !matches!(
        classify_parsed(&raw.parsed),
        StmtClass::Handled(
            StmtFamily::Transaction
                | StmtFamily::Set
                | StmtFamily::Show
                | StmtFamily::Discard
                | StmtFamily::Notify
                | StmtFamily::Deallocate
        )
    )
}

/// Command name of a bound statement for 25006 messages (`cannot execute {name} in a read-only
/// transaction`). Session-level statements never reach the executor and fall back to `statement`.
fn command_name(stmt: &BoundStatement) -> &'static str {
    use BoundStatement as S;
    match stmt {
        S::Query(_) => "SELECT",
        S::Insert(_) => "INSERT",
        S::Update(_) => "UPDATE",
        S::Delete(_) => "DELETE",
        S::CreateTableAs(_) => "CREATE TABLE AS",
        S::Truncate(_) => "TRUNCATE",
        S::CreateSchema { .. } => "CREATE SCHEMA",
        S::CreateTable(_) => "CREATE TABLE",
        S::CreateIndex(_) => "CREATE INDEX",
        S::CreateView(_) => "CREATE VIEW",
        S::CreateSequence(_) => "CREATE SEQUENCE",
        S::CreateEnum(_) | S::CreateComposite(_) => "CREATE TYPE",
        S::CreateDomain(_) => "CREATE DOMAIN",
        S::CreateFunction(_) => "CREATE FUNCTION",
        S::CreateRole(_) => "CREATE ROLE",
        S::CreateDatabase(_) => "CREATE DATABASE",
        S::AlterTable(_) => "ALTER TABLE",
        S::Alter(action) => action.tag(),
        S::Drop(d) => drop_command(d.kind),
        S::DropDatabase { .. } => "DROP DATABASE",
        S::Comment { .. } => "COMMENT",
        S::Grant { revoke: true, .. } | S::RevokeRole { .. } => "REVOKE",
        S::Grant { .. } | S::GrantRole(_) => "GRANT",
        S::ReassignOwned { .. } => "REASSIGN OWNED",
        S::DropOwned { .. } => "DROP OWNED",
        S::RefreshMatView { .. } => "REFRESH MATERIALIZED VIEW",
        S::Maintenance { kind, .. } => maintenance_command(kind),
        S::Copy(_) => "COPY",
        S::Lock(_) => "LOCK TABLE",
        S::Do(_) => "DO",
        S::Call(_) => "CALL",
        S::Explain(_) => "EXPLAIN",
        _ => "statement",
    }
}

fn drop_command(kind: DropKind) -> &'static str {
    match kind {
        DropKind::Table => "DROP TABLE",
        DropKind::View => "DROP VIEW",
        DropKind::MaterializedView => "DROP MATERIALIZED VIEW",
        DropKind::Index => "DROP INDEX",
        DropKind::Sequence => "DROP SEQUENCE",
        DropKind::ForeignTable => "DROP FOREIGN TABLE",
        DropKind::Type => "DROP TYPE",
        DropKind::Domain => "DROP DOMAIN",
        DropKind::Schema => "DROP SCHEMA",
        DropKind::Function => "DROP FUNCTION",
        DropKind::Procedure => "DROP PROCEDURE",
        DropKind::Routine => "DROP ROUTINE",
        DropKind::Aggregate => "DROP AGGREGATE",
        DropKind::Role => "DROP ROLE",
    }
}

fn maintenance_command(kind: &MaintenanceKind) -> &'static str {
    match kind {
        MaintenanceKind::Vacuum => "VACUUM",
        MaintenanceKind::Analyze => "ANALYZE",
        MaintenanceKind::Reindex => "REINDEX",
        MaintenanceKind::Cluster => "CLUSTER",
        MaintenanceKind::Checkpoint => "CHECKPOINT",
    }
}

/// Wall-clock milliseconds since the unix epoch for `pg_stat_activity` (0 if the clock is before 1970).
fn now_unix_ms() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_millis() as u64).unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn command_names_for_read_only_errors() {
        assert_eq!(command_name(&BoundStatement::Empty), "statement");
        assert_eq!(
            command_name(&BoundStatement::Maintenance { kind: MaintenanceKind::Vacuum, targets: Vec::new() }),
            "VACUUM"
        );
        assert_eq!(
            command_name(&BoundStatement::DropDatabase { name: "d".into(), if_exists: false, force: false }),
            "DROP DATABASE"
        );
        assert_eq!(drop_command(DropKind::Routine), "DROP ROUTINE");
        assert_eq!(maintenance_command(&MaintenanceKind::Checkpoint), "CHECKPOINT");
    }

    #[test]
    fn unix_clock_is_after_2020() {
        assert!(now_unix_ms() > 1_577_836_800_000);
    }

    #[test]
    #[ignore = "needs SQL-01"]
    fn snapshot_classification() {
        let session_sql = [
            "BEGIN",
            "COMMIT",
            "ROLLBACK TO SAVEPOINT s",
            "SET search_path = public",
            "SET TRANSACTION ISOLATION LEVEL REPEATABLE READ",
            "SHOW server_version",
            "DISCARD ALL",
            "LISTEN ch",
            "NOTIFY ch, 'x'",
            "DEALLOCATE p",
        ];
        for sql in session_sql {
            let raw = pgarbor_sql::parse_one(sql).expect("fixture parses");
            assert!(!statement_needs_snapshot(&raw), "{sql} must not take a snapshot");
        }
        for sql in ["SELECT 1", "INSERT INTO t VALUES (1)", "CREATE TABLE t(id int)", "PREPARE p AS SELECT 1", "EXECUTE p"] {
            let raw = pgarbor_sql::parse_one(sql).expect("fixture parses");
            assert!(statement_needs_snapshot(&raw), "{sql} must take a snapshot");
        }
    }

    #[test]
    #[ignore = "needs API-04, API-05, API-06, API-09, API-11, API-13 and SQL-01"]
    fn create_insert_select_tags_and_rows() {
        let db = crate::database::Database::in_memory().expect("in-memory database");
        let mut s = db.session("main", crate::types::SessionOptions::default()).expect("session");
        let outs = s
            .simple_query("CREATE TABLE t(id int primary key); INSERT INTO t VALUES (1); SELECT * FROM t")
            .expect("statements succeed");
        let tags: Vec<&str> = outs.iter().map(|o| o.result.tag()).collect();
        assert_eq!(tags, vec!["CREATE TABLE", "INSERT 0 1", "SELECT 1"]);
        assert_eq!(outs[2].result.rows().len(), 1);
    }
}
