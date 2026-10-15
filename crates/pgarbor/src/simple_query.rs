//! The simple Query protocol (lease API-07, north-star). PG: `tcop/postgres.c` `exec_simple_query`.
//!
//! ## `simple_query_raw(sql) -> SimpleQueryOutput` (never returns `Err`: errors are in the output)
//! 1. Update `pg_stat_activity` (`state = active`, `query = sql`).
//! 2. **Parse the whole string first** (`pgarbor_sql::parse`). A syntax error (42601 with position) or
//!    unsupported-syntax error (0A000) fails the WHOLE string before ANY statement runs; the transaction
//!    consequence of that error follows the usual rule (`abort_after_error`: an explicit block becomes
//!    `Failed`). Oracle: `create table t(a int); begin; insert ...; select 1/0; commit;` leaves nothing.
//! 3. Zero statements -> one `StatementResult::Empty` (EmptyQueryResponse).
//! 4. `use_implicit_block = statements.len() > 1`. For each statement `i`:
//!    a. if `block == Failed` and the statement is not COMMIT/ROLLBACK/ROLLBACK TO the error is 25P02
//!       (decided in `run_statement`; transaction statements are recognized after binding with the cached
//!       env);
//!    b. `ensure_txn()` (Idle -> Started);
//!    c. if `use_implicit_block`: `begin_implicit_block()` (done on EVERY iteration: a COMMIT in the middle
//!       ends the block and the next statement starts a new implicit one - oracle: `create table a1(x int);
//!       commit; create table a2(x int); select 1/0;` keeps `a1`, discards `a2`);
//!    d. `run_statement(raw, &ParamValues::default())`;
//!    e. last statement: `end_implicit_block()` then `finish_command()` (commit if `Started`). The commit happens
//!       BEFORE the last statement's output is returned, so commit-time errors replace its CommandComplete;
//!       not last: if the statement was a transaction-control statement `finish_command()` (commits an
//!       autocommit transaction, e.g. after COMMIT), else nothing (the implicit transaction continues).
//!    f. on error: record it with the statement's already queued notices, call `abort_after_error(&err)`,
//!       stop executing (the remaining statements are skipped), return.
//! 5. Return all outputs and `status = txn_status()`.
//! Results of statements before the failing one ARE returned (their effects stay inside the implicit
//! transaction, which is rolled back as a whole on error: with no `BEGIN` the earlier statements' rows are
//! still reported but their changes are not durable - same as PG).
//!
//! ## Convenience
//! `simple_query(sql)` = `simple_query_raw(sql).into_result()`. `query(sql, params)` runs ONE statement
//! with typed parameters through the extended path (`prepare` + `execute_prepared` on the unnamed
//! statement) and returns its `StatementResult`.
use pgarbor_common::{Oid, PgError, Result, Severity, SqlState};
use pgarbor_core::BackendState;
use pgarbor_sql::{parse, RawStatement};
use pgarbor_types::{oids, Datum};

use crate::run_stmt::ParamValues;
use crate::session::Session;
use crate::types::{SimpleQueryOutput, StatementOutput, StatementResult, TxnStatus};

/// A statement of a simple Query string that failed: its error plus the notices it had already raised
/// (they are sent before the ErrorResponse).
struct StepFailure {
    error: PgError,
    notices: Vec<PgError>,
}

/// Wall-clock time in unix milliseconds for `pg_stat_activity` (0 if the clock is before 1970).
fn now_unix_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX))
        .unwrap_or(0)
}

/// Type OID declared for a typed parameter of `query()`: the Datum variant decides, NULL is `unknown`
/// (inferred by the binder). Arrays and records need an explicit cast in the SQL text and are rejected.
fn param_type_oid(value: &Datum) -> Result<Oid> {
    Ok(match value {
        Datum::Null => oids::UNKNOWN,
        Datum::Bool(_) => oids::BOOL,
        Datum::Int2(_) => oids::INT2,
        Datum::Int4(_) => oids::INT4,
        Datum::Int8(_) => oids::INT8,
        Datum::Float4(_) => oids::FLOAT4,
        Datum::Float8(_) => oids::FLOAT8,
        Datum::Numeric(_) => oids::NUMERIC,
        Datum::Text(_) => oids::TEXT,
        Datum::Bytea(_) => oids::BYTEA,
        Datum::Uuid(_) => oids::UUID,
        Datum::Date(_) => oids::DATE,
        Datum::Time(_) => oids::TIME,
        Datum::TimeTz(..) => oids::TIMETZ,
        Datum::Timestamp(_) => oids::TIMESTAMP,
        Datum::TimestampTz(_) => oids::TIMESTAMPTZ,
        Datum::Interval(_) => oids::INTERVAL,
        Datum::Json(_) => oids::JSON,
        Datum::Jsonb(_) => oids::JSONB,
        Datum::Oid(_) => oids::OID,
        Datum::Char(_) => oids::CHAR,
        Datum::Array(_) | Datum::Record(_) => {
            return Err(PgError::new(
                SqlState::FEATURE_NOT_SUPPORTED,
                format!("query() cannot bind a parameter of type {}", value.variant_name()),
            )
            .with_hint("Pass the value as text and add an explicit cast in the SQL."));
        }
    })
}

impl Session {
    /// Simple Query message (module docs).
    pub fn simple_query_raw(&mut self, sql: &str) -> SimpleQueryOutput {
        self.set_activity(BackendState::Active, Some(sql));
        let mut out = SimpleQueryOutput {
            statements: Vec::new(),
            error: None,
            error_notices: Vec::new(),
            error_param_status: Vec::new(),
            status: TxnStatus::Idle,
        };

        match parse(sql) {
            // Step 2: a syntax error fails the whole string before any statement runs.
            Err(error) => self.fail_query(&mut out, StepFailure { error, notices: Vec::new() }),
            // Step 3: no statements -> one EmptyQueryResponse.
            Ok(statements) if statements.is_empty() => {
                let notices = self.take_notices();
                let param_status = self.guc.take_reported_changes();
                out.statements.push(StatementOutput { result: StatementResult::Empty, notices, param_status });
            }
            // Step 4: one implicit block when the string has several statements.
            Ok(statements) => {
                let implicit = statements.len() > 1;
                let count = statements.len();
                for (index, raw) in statements.iter().enumerate() {
                    match self.run_query_statement(raw, implicit, index + 1 == count) {
                        Ok(output) => out.statements.push(output),
                        Err(failure) => {
                            // Step 4f: the remaining statements are skipped.
                            self.fail_query(&mut out, failure);
                            break;
                        }
                    }
                }
            }
        }

        // Step 5.
        self.set_activity(self.idle_activity(), None);
        out.status = self.txn_status();
        out
    }

    /// One statement of a simple Query string (module docs, step 4a-4e). `implicit` = the string has
    /// several statements; `last` = final statement of the string.
    fn run_query_statement(
        &mut self,
        raw: &RawStatement,
        implicit: bool,
        last: bool,
    ) -> std::result::Result<StatementOutput, StepFailure> {
        // 4b: Idle -> Started.
        if let Err(error) = self.ensure_txn() {
            return Err(StepFailure { error, notices: Vec::new() });
        }
        // 4c: every statement of the string starts (or continues) the implicit block.
        if implicit {
            self.begin_implicit_block();
        }
        // 4d.
        let mut output = match self.run_statement(raw, &ParamValues::default()) {
            Ok(output) => output,
            Err(error) => return Err(StepFailure { error, notices: Vec::new() }),
        };
        // 4e: the last statement ends the implicit block and commits the autocommit transaction before
        // its output is returned. For other statements `finish_command` only acts on `Started`, which a
        // COMMIT/ROLLBACK in the middle of the string leaves behind as `Idle` (a new transaction then
        // starts with the next statement).
        if last {
            self.end_implicit_block();
        }
        let committed = self.finish_command();
        output.param_status.extend(self.guc.take_reported_changes());
        if let Err(error) = committed {
            let notices = std::mem::take(&mut output.notices);
            return Err(StepFailure { error, notices });
        }
        Ok(output)
    }

    /// Step 4f: record the error of a statement, roll the transaction back per its state, and capture the
    /// notices and ParameterStatus changes that go with it.
    fn fail_query(&mut self, out: &mut SimpleQueryOutput, failure: StepFailure) {
        let StepFailure { error, mut notices } = failure;
        self.abort_for_error(&error);
        notices.extend(self.take_notices());
        out.error_param_status = self.guc.take_reported_changes();
        out.error_notices = notices;
        out.error = Some(error);
    }

    /// `abort_after_error` for an error that is reported to the client anyway. A failure of the cleanup
    /// itself cannot replace that error, so it is queued as a WARNING notice.
    fn abort_for_error(&mut self, error: &PgError) {
        if let Err(abort_err) = self.abort_after_error(error) {
            self.pending_notices.push(abort_err.with_severity(Severity::Warning));
        }
    }

    /// Activity state after a top-level command, from the transaction state.
    fn idle_activity(&self) -> BackendState {
        match self.txn_status() {
            TxnStatus::Idle => BackendState::Idle,
            TxnStatus::InTransaction => BackendState::IdleInTransaction,
            TxnStatus::Failed => BackendState::IdleInTransactionAborted,
        }
    }

    /// Update the `pg_stat_activity` row of this session (state, and the query text when given).
    fn set_activity(&self, state: BackendState, query: Option<&str>) {
        let now = now_unix_ms();
        self.handle.shared().update_activity(|a| {
            a.state = state;
            a.state_change_ms = now;
            if let Some(text) = query {
                a.query = text.to_string();
                a.query_start_ms = Some(now);
            }
        });
    }

    /// `simple_query_raw` returning the first error as `Err`.
    pub fn simple_query(&mut self, sql: &str) -> Result<Vec<StatementOutput>> {
        self.simple_query_raw(sql).into_result()
    }

    /// Run exactly one statement with typed parameter values (embedded convenience; `$1` etc. are
    /// bound with the Datum types' OIDs inferred from the values only when the statement leaves them
    /// undetermined: pass explicit casts for ambiguous cases).
    pub fn query(&mut self, sql: &str, params: &[Datum]) -> Result<StatementResult> {
        let mut param_oids = Vec::with_capacity(params.len());
        for value in params {
            param_oids.push(param_type_oid(value)?);
        }
        // An autocommit statement runs in a `Started` transaction, committed below (or rolled back on error).
        self.ensure_txn()?;
        let result = match self.prepare("", sql, &param_oids) {
            Ok(_) => self.execute_prepared("", params),
            Err(err) => Err(err),
        };
        match result {
            Ok(result) => {
                self.finish_command()?;
                Ok(result)
            }
            Err(err) => {
                self.abort_for_error(&err);
                Err(err)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::database::Database;
    use crate::types::SessionOptions;

    #[test]
    fn param_oids_follow_datum_variants() {
        assert_eq!(param_type_oid(&Datum::Null).unwrap(), oids::UNKNOWN);
        assert_eq!(param_type_oid(&Datum::Bool(true)).unwrap(), oids::BOOL);
        assert_eq!(param_type_oid(&Datum::Int2(1)).unwrap(), oids::INT2);
        assert_eq!(param_type_oid(&Datum::Int4(1)).unwrap(), oids::INT4);
        assert_eq!(param_type_oid(&Datum::Int8(1)).unwrap(), oids::INT8);
        assert_eq!(param_type_oid(&Datum::Text("x".to_string())).unwrap(), oids::TEXT);
        assert_eq!(param_type_oid(&Datum::Char(b'a')).unwrap(), oids::CHAR);
        assert_eq!(param_type_oid(&Datum::Json("{}".to_string())).unwrap(), oids::JSON);
        assert_eq!(param_type_oid(&Datum::Oid(5)).unwrap(), oids::OID);
    }

    #[test]
    fn composite_params_are_rejected_0a000() {
        let err = param_type_oid(&Datum::Record(Vec::new())).unwrap_err();
        assert_eq!(err.sqlstate.code(), "0A000");
        assert!(err.hint.is_some());
    }

    #[test]
    fn clock_is_after_2020() {
        assert!(now_unix_ms() > 1_577_836_800_000);
    }

    #[test]
    #[ignore = "needs API-04 (Database::in_memory, Database::session)"]
    fn empty_string_gives_one_empty_result() {
        let db = Database::in_memory().unwrap();
        let mut s = db.session("main", SessionOptions::default()).unwrap();
        let out = s.simple_query_raw("");
        assert!(out.error.is_none());
        assert_eq!(out.statements.len(), 1);
        assert_eq!(out.statements[0].result, StatementResult::Empty);
        assert_eq!(out.status, TxnStatus::Idle);
    }

    #[test]
    #[ignore = "needs API-04, API-06, API-08"]
    fn results_before_error_are_returned() {
        let db = Database::in_memory().unwrap();
        let mut s = db.session("main", SessionOptions::default()).unwrap();
        let out = s.simple_query_raw("select 1; select 1/0; select 2");
        assert_eq!(out.statements.len(), 1);
        assert_eq!(out.statements[0].result.tag(), "SELECT 1");
        assert_eq!(out.error.as_ref().unwrap().sqlstate.code(), "22012");
        assert_eq!(out.status, TxnStatus::Idle);
    }

    #[test]
    #[ignore = "needs API-04, API-06, API-08"]
    fn multi_statement_tags_in_order() {
        let db = Database::in_memory().unwrap();
        let mut s = db.session("main", SessionOptions::default()).unwrap();
        let outs = s.simple_query("create table t(a int); insert into t values (1), (2); select a from t").unwrap();
        let tags: Vec<&str> = outs.iter().map(|o| o.result.tag()).collect();
        assert_eq!(tags, vec!["CREATE TABLE", "INSERT 0 2", "SELECT 2"]);
    }

    #[test]
    #[ignore = "needs API-04, API-06, API-08"]
    fn syntax_error_runs_nothing() {
        let db = Database::in_memory().unwrap();
        let mut s = db.session("main", SessionOptions::default()).unwrap();
        let out = s.simple_query_raw("create table t(a int); selec 1");
        assert!(out.statements.is_empty());
        assert_eq!(out.error.as_ref().unwrap().sqlstate.code(), "42601");
    }

    #[test]
    #[ignore = "needs API-04, API-06, API-08"]
    fn explicit_block_fails_and_commit_rolls_back() {
        let db = Database::in_memory().unwrap();
        let mut s = db.session("main", SessionOptions::default()).unwrap();
        let out = s.simple_query_raw("begin; select 1/0; commit;");
        assert_eq!(out.error.as_ref().unwrap().sqlstate.code(), "22012");
        assert_eq!(out.status, TxnStatus::Idle);
    }

    #[test]
    #[ignore = "needs API-04, API-06, API-08"]
    fn set_local_warns_only_as_single_statement() {
        let db = Database::in_memory().unwrap();
        let mut s = db.session("main", SessionOptions::default()).unwrap();
        let multi = s.simple_query("set local statement_timeout = 0; select 1").unwrap();
        assert!(multi.iter().all(|o| o.notices.is_empty()));
        let single = s.simple_query_raw("set local statement_timeout = 0");
        assert_eq!(single.statements[0].notices[0].sqlstate.code(), "25P01");
    }
}
