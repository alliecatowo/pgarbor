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
use pgarbor_common::{PgError, Result};
use pgarbor_types::Datum;

use crate::session::Session;
use crate::types::{SimpleQueryOutput, StatementOutput, StatementResult};

impl Session {
    /// Simple Query message (module docs).
    #[allow(unused_variables)]
    pub fn simple_query_raw(&mut self, sql: &str) -> SimpleQueryOutput {
        SimpleQueryOutput {
            statements: Vec::new(),
            error: Some(PgError::unimplemented("API-07", "simple query (Session::simple_query_raw)")),
            error_notices: Vec::new(),
            error_param_status: Vec::new(),
            status: self.txn_status(),
        }
    }

    /// `simple_query_raw` returning the first error as `Err`.
    pub fn simple_query(&mut self, sql: &str) -> Result<Vec<StatementOutput>> {
        self.simple_query_raw(sql).into_result()
    }

    /// Run exactly one statement with typed parameter values (embedded convenience; `$1` etc. are
    /// bound with the Datum types' OIDs inferred from the values only when the statement leaves them
    /// undetermined: pass explicit casts for ambiguous cases).
    #[allow(unused_variables)]
    pub fn query(&mut self, sql: &str, params: &[Datum]) -> Result<StatementResult> {
        Err(PgError::unimplemented("API-07", "parameterized query (Session::query)"))
    }
}
