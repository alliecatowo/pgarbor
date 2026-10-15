//! Transaction block state machine and transaction-control statements (lease API-06, north-star).
//! PG: `access/transam/xact.c` (`BeginTransactionBlock`, `EndTransactionBlock`,
//! `UserAbortTransactionBlock`, `BeginImplicitTransactionBlock`, `EndImplicitTransactionBlock`,
//! `DefineSavepoint`, `ReleaseSavepoint`, `RollbackToSavepoint`), `tcop/postgres.c`
//! (`exec_simple_query`), all oracle-verified on PG 18.6.
//!
//! ## States ([`BlockState`])
//! | state | PG name | meaning |
//! |---|---|---|
//! | `Idle` | TBLOCK_DEFAULT | no core transaction (`Session::txn == None`) |
//! | `Started` | TBLOCK_STARTED | a transaction exists for ONE top-level statement (autocommit) |
//! | `ImplicitBlock` | TBLOCK_IMPLICIT_INPROGRESS | several statements of one Query string (or pipelined extended-protocol statements) share one transaction without BEGIN |
//! | `InBlock` | TBLOCK_INPROGRESS | after BEGIN (savepoint depth lives in the core `Txn`) |
//! | `Failed` | TBLOCK_ABORT | an error happened inside an explicit block; only ROLLBACK, ROLLBACK TO SAVEPOINT, COMMIT (rolls back) and END/ABORT are accepted, everything else: 25P02 `current transaction is aborted, commands ignored until end of transaction block` |
//! `Session::txn_status()` maps Idle/Started/ImplicitBlock -> `I`, InBlock -> `T`, Failed -> `E`.
//!
//! ## Operations (all `impl Session`)
//! * [`Session::ensure_txn`]: `Idle` -> `Started`: `engine.begin(handle, &TxnOptions{kind: Implicit,
//!   ..guc_derive::txn_defaults})`, `guc.at_txn_start()`; other states unchanged.
//! * [`Session::begin_implicit_block`] / [`Session::end_implicit_block`]: `Started` <-> `ImplicitBlock`
//!   (no-ops in other states). Called around each statement of a multi-statement Query string.
//! * [`Session::finish_command`]: end of a top-level command: `Started` -> commit (below) -> `Idle`;
//!   every other state unchanged.
//! * [`Session::abort_after_error`]: an error escaped a statement: `Started`/`ImplicitBlock` -> roll back
//!   -> `Idle`; `InBlock` -> `txn.mark_failed()` -> `Failed`; `Failed` stays. Always: discard portals only
//!   when the transaction is gone; `guc.at_txn_end(false)` when rolled back; `listen.at_abort()`.
//! * commit = `Txn::commit()` (`CommitOutcome::{Committed, RolledBack}`) then `guc.at_txn_end(true)`,
//!   `listen.at_commit(hub, pid)`, portals dropped, activity -> idle; a commit error is returned after
//!   the same cleanup as an abort.
//!
//! ## Transaction statements ([`Session::exec_transaction_stmt`], `TransactionStmt`)
//! (`txn_status` before -> behavior; tag in brackets; WARNINGs go to `pending_notices`)
//! * `BEGIN [modes]`: `Started`/`ImplicitBlock` -> `InBlock` (the already-started transaction becomes the
//!   explicit block even if statements already ran); modes: `Txn::set_isolation/set_read_only/
//!   set_deferrable` (errors 25001 `SET TRANSACTION ISOLATION LEVEL must be called before any query`
//!   etc. come from core). `InBlock`/`Failed`: WARNING 25001 `there is already a transaction in
//!   progress`, no state change. Tag `BEGIN`.
//! * `COMMIT [AND CHAIN]` (also END): `InBlock` -> commit -> `Idle` [`COMMIT`]; `Failed` -> rollback ->
//!   `Idle` [`ROLLBACK`]; `Started`/`ImplicitBlock` -> WARNING 25P01 `there is no transaction in
//!   progress`, then commit (everything the implicit block did is committed) -> `Idle` [`COMMIT`].
//!   `AND CHAIN` outside an explicit block: ERROR 25P01 `COMMIT AND CHAIN can only be used in
//!   transaction blocks`; inside: after ending, immediately begin a new `InBlock` transaction with the
//!   same isolation/read-only/deferrable.
//! * `ROLLBACK [AND CHAIN]` (also ABORT): `InBlock`/`Failed` -> rollback -> `Idle`; `Started`/
//!   `ImplicitBlock` -> WARNING 25P01 `there is no transaction in progress` then rollback -> `Idle`.
//!   Tag `ROLLBACK`. Chain errors as for COMMIT (`ROLLBACK AND CHAIN can only be used in transaction
//!   blocks`).
//! * `SAVEPOINT name`: `InBlock` only (`Txn::savepoint`, `guc.at_savepoint`) [`SAVEPOINT`]; outside a block
//!   ERROR 25P01 `SAVEPOINT can only be used in transaction blocks`; `Failed` -> 25P02.
//! * `RELEASE [SAVEPOINT] name`: `InBlock` (`Txn::release_savepoint`, `guc.at_release`) [`RELEASE`]; unknown
//!   name 3B001 `savepoint "x" does not exist`; outside a block 25P01 `RELEASE SAVEPOINT can only be used in
//!   transaction blocks`; `Failed` -> 25P02.
//! * `ROLLBACK TO [SAVEPOINT] name`: `InBlock` or `Failed` (`Txn::rollback_to_savepoint`, `guc.
//!   at_rollback_to`; a Failed block becomes `InBlock` again) [`ROLLBACK`]; unknown name 3B001; outside a
//!   block 25P01 `ROLLBACK TO SAVEPOINT can only be used in transaction blocks`.
//! * `SET TRANSACTION modes`: applies `Txn::set_*` to the CURRENT transaction; in `Started` (autocommit
//!   statement) additionally WARNING 25P01 `SET TRANSACTION can only be used in transaction blocks`;
//!   no warning in `ImplicitBlock`/`InBlock`. Tag `SET`.
//! * `SET SESSION CHARACTERISTICS AS TRANSACTION modes`: updates `default_transaction_isolation`,
//!   `default_transaction_read_only`, `default_transaction_deferrable` (session scope). Tag `SET`.
//! `SET LOCAL` warns (25P01) only in state `Started`. `LOCK`/`SAVEPOINT` use `require_transaction_block`,
//! `VACUUM`/`DISCARD ALL`/`CREATE DATABASE` use `prevent_in_transaction_block`.
//! Statements other than ROLLBACK / ROLLBACK TO / COMMIT / END / ABORT in `Failed` fail with 25P02 BEFORE
//! they are bound (`check_statement_allowed`).
use pgarbor_common::{PgError, Result};
use pgarbor_core::CommitOutcome;
use pgarbor_sql::ir::{BoundStatement, TransactionStmt};

use crate::session::Session;
use crate::types::StatementResult;

/// Transaction block state (module docs).
// CONTRACT
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BlockState {
    Idle,
    Started,
    ImplicitBlock,
    InBlock,
    Failed,
}

/// True for the statements a failed block still accepts: COMMIT, END, ROLLBACK, ABORT, ROLLBACK TO
/// SAVEPOINT (COMMIT/END/ABORT/ROLLBACK map to `TransactionStmt::{Commit, Rollback}`).
pub fn allowed_in_failed_block(stmt: &BoundStatement) -> bool {
    matches!(
        stmt,
        BoundStatement::Transaction(
            TransactionStmt::Commit { .. } | TransactionStmt::Rollback { .. } | TransactionStmt::RollbackToSavepoint(_)
        )
    )
}

impl Session {
    /// `Idle` -> `Started` (module docs).
    pub(crate) fn ensure_txn(&mut self) -> Result<()> {
        Err(PgError::unimplemented("API-06", "transaction start (Session::ensure_txn)"))
    }

    /// `Started` -> `ImplicitBlock`; other states unchanged.
    pub(crate) fn begin_implicit_block(&mut self) {
        if self.block == BlockState::Started {
            self.block = BlockState::ImplicitBlock;
        }
    }

    /// `ImplicitBlock` -> `Started`; other states unchanged.
    pub(crate) fn end_implicit_block(&mut self) {
        if self.block == BlockState::ImplicitBlock {
            self.block = BlockState::Started;
        }
    }

    /// End of a top-level command: commit if `Started` (module docs).
    pub(crate) fn finish_command(&mut self) -> Result<Option<CommitOutcome>> {
        Err(PgError::unimplemented("API-06", "end of command (Session::finish_command)"))
    }

    /// Error cleanup per state (module docs).
    #[allow(unused_variables)]
    pub(crate) fn abort_after_error(&mut self, err: &PgError) -> Result<()> {
        Err(PgError::unimplemented("API-06", "error cleanup (Session::abort_after_error)"))
    }

    /// 25P02 check for a statement about to be bound while the block is `Failed` (`stmt` is `None` when
    /// the kind is not known yet: then only transaction-control keywords of the raw text are
    /// recognized: the caller binds transaction statements with the cached env first).
    #[allow(unused_variables)]
    pub(crate) fn check_statement_allowed(&self, stmt: &BoundStatement) -> Result<()> {
        Err(PgError::unimplemented("API-06", "failed-block check (Session::check_statement_allowed)"))
    }

    /// Execute a transaction-control statement (module docs).
    #[allow(unused_variables)]
    pub(crate) fn exec_transaction_stmt(&mut self, t: &TransactionStmt) -> Result<StatementResult> {
        Err(PgError::unimplemented("API-06", "transaction statements (Session::exec_transaction_stmt)"))
    }

    /// 25P01 `{what} can only be used in transaction blocks` unless the state is `InBlock`/`Failed`
    /// (PG `RequireTransactionBlock`: LOCK TABLE, SAVEPOINT, DECLARE, ...). `ImplicitBlock` counts as NOT
    /// a block here (oracle: `lock table t; select 1` in one Query string fails).
    #[allow(unused_variables)]
    pub(crate) fn require_transaction_block(&self, what: &str) -> Result<()> {
        Err(PgError::unimplemented("API-06", "transaction block requirement (Session::require_transaction_block)"))
    }

    /// 25001 `{what} cannot run inside a transaction block` when the state is `ImplicitBlock`, `InBlock`
    /// or `Failed` (PG `PreventInTransactionBlock`: VACUUM, DISCARD ALL, CREATE/DROP DATABASE, CREATE
    /// INDEX CONCURRENTLY...). Oracle: `select 1; discard all` and `select 1; vacuum` fail; a single
    /// statement (`Started`) passes.
    #[allow(unused_variables)]
    pub(crate) fn prevent_in_transaction_block(&self, what: &str) -> Result<()> {
        Err(PgError::unimplemented("API-06", "transaction block prohibition (Session::prevent_in_transaction_block)"))
    }
}
