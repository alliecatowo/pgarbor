//! Extended query protocol primitives (lease API-10, north-star). The wire layer maps each frontend
//! message to one method and sends the backend messages described here.
//!
//! | message | method | backend answer |
//! |---|---|---|
//! | Parse(name, query, param OIDs) | [`Session::parse`] | ParseComplete |
//! | Bind(portal, stmt, formats, params, result formats) | [`Session::bind`] | BindComplete |
//! | Describe('S', name) | [`Session::describe_statement`] | ParameterDescription + RowDescription/NoData |
//! | Describe('P', name) | [`Session::describe_portal`] | RowDescription/NoData |
//! | Execute(portal, max_rows) | [`Session::execute`] | DataRow*, then CommandComplete / EmptyQueryResponse / PortalSuspended |
//! | Close('S'/'P', name) | [`Session::close_statement`] / [`Session::close_portal`] | CloseComplete (closing a missing object is NOT an error) |
//! | Sync | [`Session::sync`] | ReadyForQuery(status) |
//! | Flush | (wire only) | - |
//!
//! ## Transactions (PG `exec_*_message`, `finish_xact_command` at Sync)
//! * Every message first calls `ensure_txn()` (Idle -> Started: an implicit transaction that lasts until
//!   Sync). After the FIRST Execute completes while still `Started`, switch to the implicit block
//!   (`begin_implicit_block`, `pipelining = true`) so later statements before Sync share the transaction
//!   (PG pipelining semantics).
//! * An error in ANY message: `abort_after_error(&err)` immediately (implicit transaction rolled back;
//!   explicit block -> `Failed`), set `error_since_sync = true`, return the error. The wire layer discards
//!   messages until Sync.
//! * [`Session::sync`]: `end_implicit_block()`, `finish_command()` (commit if the transaction is still
//!   `Started` and no error happened; with `error_since_sync` it was already aborted), clear
//!   `error_since_sync`/`pipelining`, drop the unnamed portal if finished, return `txn_status()`. A commit
//!   error is returned as `Err` (the wire layer sends ErrorResponse before ReadyForQuery).
//!
//! ## Parse
//! `prepare_statement(name, query, declared types from OIDs (0 = unspecified), from_sql=false)`.
//! A named statement that already exists: 42P05 `prepared statement "x" already exists`; the unnamed statement
//! is replaced. Transaction-control statements can be prepared. An empty query is allowed (EmptyQueryResponse at
//! Execute).
//! ## Bind
//! Look up the statement (26000 `prepared statement "x" does not exist`). Parameter count mismatch 08P01
//! `bind message supplies {n} parameters, but prepared statement "{name}" requires {m}`. Parameter format
//! codes: empty = all text, one = all, else one per parameter (mismatch 08P01 `bind message has {a}
//! parameter formats but {b} parameters`). Decode each parameter with `decode_param` (text: `io::input`
//! with the parameter type, error 22P02 etc.; binary: `io::recv`). Result formats likewise validated
//! against the column count. Then `open_portal`.
//! ## Describe
//! Statement: `StatementDescription{param_oids, columns}` from the prepared statement (revalidated with
//! `ensure_current`, so a dropped table errors here as in PG). Portal: its `columns`. Unknown names: 26000
//! `prepared statement "x" does not exist` / 34000 `portal "x" does not exist`.
//! ## Execute
//! `run_portal(portal, max_rows)`; unknown portal 34000 `portal "x" does not exist`.
use pgarbor_common::{Oid, PgError, Result, SqlState};
use pgarbor_sql::ir::{BoundStatement, TransactionStmt};

use crate::portal::PortalState;
use crate::prepared::PreparedStatement;
use crate::run_stmt::ParamValues;
use crate::session::Session;
use crate::txn_block::BlockState;
use crate::types::{ColumnInfo, Datum, FormatCode, PortalOutput, StatementDescription, TxnStatus, TypeId};

/// `prepared statement "x" does not exist` (26000).
fn no_statement(name: &str) -> PgError {
    PgError::new(SqlState::INVALID_SQL_STATEMENT_NAME, format!("prepared statement \"{name}\" does not exist"))
}

/// `portal "x" does not exist` (34000).
fn no_portal(name: &str) -> PgError {
    PgError::new(SqlState::INVALID_CURSOR_NAME, format!("portal \"{name}\" does not exist"))
}

/// `bind message supplies {n} parameters, but prepared statement "{name}" requires {m}` (08P01).
fn check_param_count(supplied: usize, required: usize, stmt_name: &str) -> Result<()> {
    if supplied != required {
        return Err(PgError::new(
            SqlState::PROTOCOL_VIOLATION,
            format!("bind message supplies {supplied} parameters, but prepared statement \"{stmt_name}\" requires {required}"),
        ));
    }
    Ok(())
}

/// `bind message has {a} parameter formats but {b} parameters` (08P01): zero, one or one-per-parameter.
fn check_param_formats(nformats: usize, nparams: usize) -> Result<()> {
    if nformats > 1 && nformats != nparams {
        return Err(PgError::new(
            SqlState::PROTOCOL_VIOLATION,
            format!("bind message has {nformats} parameter formats but {nparams} parameters"),
        ));
    }
    Ok(())
}

/// `bind message has {a} result formats but query has {b} columns` (08P01). Only statements that return
/// rows have a tuple descriptor, so the caller checks `columns` first (PG `PortalSetResultFormat`).
fn check_result_formats(nformats: usize, ncols: usize) -> Result<()> {
    if nformats > 1 && nformats != ncols {
        return Err(PgError::new(
            SqlState::PROTOCOL_VIOLATION,
            format!("bind message has {nformats} result formats but query has {ncols} columns"),
        ));
    }
    Ok(())
}

/// Format code of parameter `index`: empty list = all text, one entry = all, else one per parameter.
/// Callers validate the count with [`check_param_formats`] first.
fn param_format(formats: &[FormatCode], index: usize) -> FormatCode {
    if formats.len() <= 1 {
        return formats.first().copied().unwrap_or(FormatCode::Text);
    }
    formats.get(index).copied().unwrap_or(FormatCode::Text)
}

/// PG `IsTransactionExitStmt`: COMMIT / END / ROLLBACK / ABORT (the statements a failed block still runs
/// from a portal).
fn is_exit_stmt(stmt: &BoundStatement) -> bool {
    matches!(stmt, BoundStatement::Transaction(TransactionStmt::Commit { .. } | TransactionStmt::Rollback { .. }))
}

impl Session {
    /// Run one extended-protocol message body: `ensure_txn()` first, and on any error roll back per the
    /// transaction state (`abort_after_error`) and mark `error_since_sync` (module docs). A failure of the
    /// cleanup itself does not replace the original error, as in PG where the first error is reported.
    fn message<T>(&mut self, f: impl FnOnce(&mut Session) -> Result<T>) -> Result<T> {
        let outcome = match self.ensure_txn() {
            Ok(()) => f(self),
            Err(e) => Err(e),
        };
        match outcome {
            Ok(value) => Ok(value),
            Err(err) => {
                let _cleanup = self.abort_after_error(&err);
                self.error_since_sync = true;
                Err(err)
            }
        }
    }

    /// The prepared statement `name` as stored (no revalidation), or 26000.
    fn statement_ref(&self, name: &str) -> Result<&PreparedStatement> {
        self.prepared.get(name).ok_or_else(|| no_statement(name))
    }

    /// Bind message body (module docs, "Bind").
    fn bind_message(
        &mut self,
        portal: &str,
        statement: &str,
        param_formats: &[FormatCode],
        params: &[Option<Vec<u8>>],
        result_formats: &[FormatCode],
    ) -> Result<()> {
        let (required, exit_stmt) = {
            let ps = self.statement_ref(statement)?;
            (ps.param_types.len(), ps.bound.as_ref().map_or(false, is_exit_stmt))
        };
        // PG `exec_bind_message` order: parameter formats, parameter count, failed-block check, portal.
        check_param_formats(param_formats.len(), params.len())?;
        check_param_count(params.len(), required, statement)?;
        if self.block == BlockState::Failed && (!exit_stmt || !params.is_empty()) {
            return Err(PgError::new(
                SqlState::IN_FAILED_SQL_TRANSACTION,
                "current transaction is aborted, commands ignored until end of transaction block",
            ));
        }
        if !portal.is_empty() && self.portals.contains(portal) {
            return Err(PgError::new(SqlState(*b"42P03"), format!("cursor \"{portal}\" already exists")));
        }

        // Revalidate after catalog changes (PG plans the bound statement here), then decode the values.
        let ps = self.ensure_current(statement)?;
        check_param_count(params.len(), ps.param_types.len(), statement)?;
        let mut values: Vec<Datum> = Vec::with_capacity(params.len());
        for (index, (ty, bytes)) in ps.param_types.iter().zip(params.iter()).enumerate() {
            let format = param_format(param_formats, index);
            values.push(self.decode_param(*ty, bytes.as_deref(), format)?);
        }
        if let Some(columns) = ps.columns.as_ref() {
            check_result_formats(result_formats.len(), columns.len())?;
        }
        let param_types: Vec<TypeId> = ps.param_types;
        self.open_portal(
            portal,
            statement,
            ParamValues { types: param_types, values },
            result_formats.to_vec(),
        )
    }

    /// Parse message.
    pub fn parse(&mut self, name: &str, query: &str, param_oids: &[Oid]) -> Result<()> {
        self.message(|s: &mut Session| {
            if !name.is_empty() && s.prepared.get(name).is_some() {
                return Err(PgError::new(
                    SqlState::DUPLICATE_PREPARED_STATEMENT,
                    format!("prepared statement \"{name}\" already exists"),
                ));
            }
            let declared: Vec<TypeId> = param_oids.iter().map(|&oid| TypeId::of(oid)).collect();
            s.prepare_statement(name, query, &declared, false)
        })
    }

    /// Bind message. `params[i] == None` is SQL NULL.
    pub fn bind(
        &mut self,
        portal: &str,
        statement: &str,
        param_formats: &[FormatCode],
        params: &[Option<Vec<u8>>],
        result_formats: &[FormatCode],
    ) -> Result<()> {
        self.message(|s: &mut Session| s.bind_message(portal, statement, param_formats, params, result_formats))
    }

    /// Describe statement.
    pub fn describe_statement(&mut self, name: &str) -> Result<StatementDescription> {
        self.message(|s: &mut Session| {
            s.statement_ref(name)?;
            let ps = s.ensure_current(name)?;
            let param_oids: Vec<Oid> = ps.param_types.iter().map(|t| t.oid).collect();
            let columns: Option<Vec<ColumnInfo>> = ps.columns;
            Ok(StatementDescription { param_oids, columns })
        })
    }

    /// Describe portal: `None` = NoData.
    pub fn describe_portal(&mut self, name: &str) -> Result<Option<Vec<ColumnInfo>>> {
        self.message(|s: &mut Session| match s.portals.get(name) {
            Some(portal) => Ok(portal.columns.clone()),
            None => Err(no_portal(name)),
        })
    }

    /// Execute message (`max_rows == 0` = no limit).
    pub fn execute(&mut self, portal: &str, max_rows: u32) -> Result<PortalOutput> {
        self.message(|s: &mut Session| {
            if !s.portals.contains(portal) {
                return Err(no_portal(portal));
            }
            let output = s.run_portal(portal, max_rows)?;
            // PG pipelining: after the first Execute the implicit transaction lasts until Sync.
            if s.block == BlockState::Started {
                s.begin_implicit_block();
                s.pipelining = true;
            }
            Ok(output)
        })
    }

    /// Close statement (missing = Ok).
    pub fn close_statement(&mut self, name: &str) -> Result<()> {
        self.message(|s: &mut Session| {
            let _removed = s.prepared.remove(name);
            Ok(())
        })
    }

    /// Close portal (missing = Ok).
    pub fn close_portal(&mut self, name: &str) -> Result<()> {
        self.message(|s: &mut Session| {
            let _removed = s.portals.remove(name);
            Ok(())
        })
    }

    /// Sync message.
    pub fn sync(&mut self) -> Result<TxnStatus> {
        self.end_implicit_block();
        // After an error the transaction was already aborted by `abort_after_error`; otherwise an
        // implicit transaction still `Started` is committed here (PG `finish_xact_command`).
        let commit = if self.error_since_sync { Ok(()) } else { self.finish_command().map(|_| ()) };
        self.error_since_sync = false;
        self.pipelining = false;
        let unnamed_done = self.portals.get("").map_or(false, |p| matches!(p.state, PortalState::Done { .. }));
        if unnamed_done {
            let _removed = self.portals.remove("");
        }
        commit?;
        Ok(self.txn_status())
    }

    /// True after an error until the next `sync` (the wire layer skips messages meanwhile).
    pub fn in_error_state(&self) -> bool {
        self.error_since_sync
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::database::Database;
    use crate::types::{PortalStatus, SessionOptions};

    #[test]
    fn param_count_message_matches_pg() {
        let e = check_param_count(1, 2, "s").unwrap_err();
        assert_eq!(e.sqlstate.code(), "08P01");
        assert_eq!(e.message, "bind message supplies 1 parameters, but prepared statement \"s\" requires 2");
        assert!(check_param_count(2, 2, "s").is_ok());
    }

    #[test]
    fn format_count_rules() {
        assert!(check_param_formats(0, 3).is_ok());
        assert!(check_param_formats(1, 3).is_ok());
        assert!(check_param_formats(3, 3).is_ok());
        let e = check_param_formats(2, 3).unwrap_err();
        assert_eq!(e.sqlstate.code(), "08P01");
        assert_eq!(e.message, "bind message has 2 parameter formats but 3 parameters");
        assert!(check_result_formats(0, 4).is_ok());
        assert!(check_result_formats(1, 4).is_ok());
        let r = check_result_formats(2, 4).unwrap_err();
        assert_eq!(r.message, "bind message has 2 result formats but query has 4 columns");
    }

    #[test]
    fn param_format_selection() {
        let text = FormatCode::Text;
        let bin = FormatCode::Binary;
        assert_eq!(param_format(&[], 2), text);
        assert_eq!(param_format(&[bin], 2), bin);
        assert_eq!(param_format(&[bin, text, bin], 1), text);
        assert_eq!(param_format(&[bin, text, bin], 2), bin);
    }

    #[test]
    #[ignore = "needs API-06, API-08, API-11, API-12"]
    fn parse_bind_execute_sync_round_trip() {
        let db = Database::in_memory().unwrap();
        let mut s = db.session("main", SessionOptions::default()).unwrap();
        s.parse("", "SELECT $1::int + 1", &[]).unwrap();
        s.bind("", "", &[], &[Some(b"41".to_vec())], &[]).unwrap();
        let out = s.execute("", 0).unwrap();
        assert_eq!(out.rows, vec![vec![Some(b"42".to_vec())]]);
        assert_eq!(s.sync().unwrap(), TxnStatus::Idle);
        assert!(!s.in_error_state());
    }

    #[test]
    #[ignore = "needs API-06, API-08, API-11, API-12"]
    fn parse_rejects_multiple_commands() {
        let db = Database::in_memory().unwrap();
        let mut s = db.session("main", SessionOptions::default()).unwrap();
        let e = s.parse("", "select 1; select 2", &[]).unwrap_err();
        assert_eq!(e.sqlstate.code(), "42601");
        assert_eq!(e.message, "cannot insert multiple commands into a prepared statement");
    }

    #[test]
    #[ignore = "needs API-06, API-08, API-11, API-12"]
    fn duplicate_named_statement_is_42p05() {
        let db = Database::in_memory().unwrap();
        let mut s = db.session("main", SessionOptions::default()).unwrap();
        s.parse("s", "select 1", &[]).unwrap();
        let e = s.parse("s", "select 1", &[]).unwrap_err();
        assert_eq!(e.sqlstate.code(), "42P05");
        assert_eq!(e.message, "prepared statement \"s\" already exists");
    }

    #[test]
    #[ignore = "needs API-06, API-08, API-11, API-12"]
    fn bind_wrong_count_is_08p01() {
        let db = Database::in_memory().unwrap();
        let mut s = db.session("main", SessionOptions::default()).unwrap();
        s.parse("s", "select $1::int + $2::int", &[]).unwrap();
        let e = s.bind("", "s", &[], &[Some(b"1".to_vec())], &[]).unwrap_err();
        assert_eq!(e.sqlstate.code(), "08P01");
        assert_eq!(e.message, "bind message supplies 1 parameters, but prepared statement \"s\" requires 2");
    }

    #[test]
    #[ignore = "needs API-06, API-08, API-11, API-12"]
    fn execute_with_row_limit_suspends_then_completes() {
        let db = Database::in_memory().unwrap();
        let mut s = db.session("main", SessionOptions::default()).unwrap();
        s.parse("", "SELECT x FROM (VALUES (1),(2),(3),(4),(5)) t(x)", &[]).unwrap();
        s.bind("p", "", &[], &[], &[]).unwrap();
        let first = s.execute("p", 2).unwrap();
        assert_eq!(first.rows.len(), 2);
        assert_eq!(first.status, PortalStatus::Suspended);
        let rest = s.execute("p", 0).unwrap();
        assert_eq!(rest.rows.len(), 3);
        assert!(matches!(rest.status, PortalStatus::Complete { .. }));
        s.close_statement("missing").unwrap();
        s.close_portal("missing").unwrap();
        s.sync().unwrap();
    }

    #[test]
    #[ignore = "needs API-06, API-08, API-11, API-12"]
    fn execute_error_aborts_until_sync() {
        let db = Database::in_memory().unwrap();
        let mut s = db.session("main", SessionOptions::default()).unwrap();
        s.parse("", "SELECT 1/0", &[]).unwrap();
        s.bind("", "", &[], &[], &[]).unwrap();
        let e = s.execute("", 0).unwrap_err();
        assert_eq!(e.sqlstate.code(), "22012");
        assert!(s.in_error_state());
        assert_eq!(s.sync().unwrap(), TxnStatus::Idle);
        assert!(!s.in_error_state());
    }
}
