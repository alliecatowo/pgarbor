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
use pgarbor_common::{Oid, PgError, Result};

use crate::session::Session;
use crate::types::{ColumnInfo, FormatCode, PortalOutput, StatementDescription, TxnStatus};

impl Session {
    /// Parse message.
    #[allow(unused_variables)]
    pub fn parse(&mut self, name: &str, query: &str, param_oids: &[Oid]) -> Result<()> {
        Err(PgError::unimplemented("API-10", "Parse message (Session::parse)"))
    }

    /// Bind message. `params[i] == None` is SQL NULL.
    #[allow(unused_variables)]
    pub fn bind(
        &mut self,
        portal: &str,
        statement: &str,
        param_formats: &[FormatCode],
        params: &[Option<Vec<u8>>],
        result_formats: &[FormatCode],
    ) -> Result<()> {
        Err(PgError::unimplemented("API-10", "Bind message (Session::bind)"))
    }

    /// Describe statement.
    #[allow(unused_variables)]
    pub fn describe_statement(&mut self, name: &str) -> Result<StatementDescription> {
        Err(PgError::unimplemented("API-10", "Describe statement (Session::describe_statement)"))
    }

    /// Describe portal: `None` = NoData.
    #[allow(unused_variables)]
    pub fn describe_portal(&mut self, name: &str) -> Result<Option<Vec<ColumnInfo>>> {
        Err(PgError::unimplemented("API-10", "Describe portal (Session::describe_portal)"))
    }

    /// Execute message (`max_rows == 0` = no limit).
    #[allow(unused_variables)]
    pub fn execute(&mut self, portal: &str, max_rows: u32) -> Result<PortalOutput> {
        Err(PgError::unimplemented("API-10", "Execute message (Session::execute)"))
    }

    /// Close statement (missing = Ok).
    #[allow(unused_variables)]
    pub fn close_statement(&mut self, name: &str) -> Result<()> {
        Err(PgError::unimplemented("API-10", "Close statement (Session::close_statement)"))
    }

    /// Close portal (missing = Ok).
    #[allow(unused_variables)]
    pub fn close_portal(&mut self, name: &str) -> Result<()> {
        Err(PgError::unimplemented("API-10", "Close portal (Session::close_portal)"))
    }

    /// Sync message.
    pub fn sync(&mut self) -> Result<TxnStatus> {
        Err(PgError::unimplemented("API-10", "Sync message (Session::sync)"))
    }

    /// True after an error until the next `sync` (the wire layer skips messages meanwhile).
    pub fn in_error_state(&self) -> bool {
        self.error_since_sync
    }
}
