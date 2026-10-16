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
use pgarbor_common::{PgError, Result, Severity, SqlState};
use pgarbor_core::{CommitOutcome, IsolationLevel, Txn, TxnKind, TxnOptions};
use pgarbor_sql::ir::{BoundStatement, IsolationLevel as SqlIsolation, TransactionStmt, TxnModes};

use crate::guc::GucScope;
use crate::guc_derive;
use crate::session::Session;
use crate::types::StatementResult;

/// 25P02 for a statement that a failed block does not accept (module docs).
fn failed_block_err() -> PgError {
    PgError::new(
        SqlState::IN_FAILED_SQL_TRANSACTION,
        "current transaction is aborted, commands ignored until end of transaction block",
    )
}

/// 25000 when an operation needs the core transaction and none exists (internal state invariant).
fn no_txn_err() -> PgError {
    PgError::new(SqlState::INVALID_TRANSACTION_STATE, "no transaction in progress")
}

/// Command-complete result with a bare tag.
fn command(tag: &str) -> StatementResult {
    StatementResult::Command { tag: tag.to_string() }
}

/// Command tag of COMMIT / ROLLBACK ends: a COMMIT of a failed block reports `ROLLBACK` (PG).
fn tag_for(outcome: CommitOutcome) -> &'static str {
    match outcome {
        CommitOutcome::Committed { .. } => "COMMIT",
        CommitOutcome::RolledBack => "ROLLBACK",
    }
}

/// Map the SQL-level isolation level of a statement to the core level.
fn core_isolation(level: SqlIsolation) -> IsolationLevel {
    match level {
        SqlIsolation::ReadUncommitted => IsolationLevel::ReadUncommitted,
        SqlIsolation::ReadCommitted => IsolationLevel::ReadCommitted,
        SqlIsolation::RepeatableRead => IsolationLevel::RepeatableRead,
        SqlIsolation::Serializable => IsolationLevel::Serializable,
    }
}

/// `on` / `off` spelling of a boolean GUC value.
fn on_off(v: bool) -> String {
    if v {
        "on".to_string()
    } else {
        "off".to_string()
    }
}

/// Apply the modes of `BEGIN` / `SET TRANSACTION` to a core transaction (the core raises the
/// 25001 / 25006 errors for late or illegal changes).
fn apply_modes(txn: &mut Txn, modes: &TxnModes) -> Result<()> {
    if let Some(level) = modes.isolation {
        txn.set_isolation(core_isolation(level))?;
    }
    if let Some(read_only) = modes.read_only {
        txn.set_read_only(read_only)?;
    }
    if let Some(deferrable) = modes.deferrable {
        txn.set_deferrable(deferrable)?;
    }
    Ok(())
}

/// Explicit-block options carrying the characteristics of an existing core transaction (used by
/// `AND CHAIN`).
fn characteristics(txn: &Txn) -> TxnOptions {
    TxnOptions {
        kind: TxnKind::Explicit,
        isolation: txn.isolation(),
        read_only: txn.is_read_only(),
        deferrable: txn.is_deferrable(),
    }
}

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
        if self.block != BlockState::Idle {
            return Ok(());
        }
        // Session defaults (GUC-derived) for an implicit transaction (xact.c StartTransactionCommand).
        let defaults = guc_derive::txn_defaults(&self.guc)?;
        let opts = TxnOptions { kind: TxnKind::Implicit, ..defaults };
        let txn = self.db.engine().begin(&self.handle, &opts)?;
        // If this fails the new transaction is dropped (its locks are released by `Drop`).
        self.guc.at_txn_start()?;
        self.txn = Some(txn);
        self.block = BlockState::Started;
        Ok(())
    }

    /// Commit (or roll back) the core transaction and do the per-end cleanup: portals dropped,
    /// LISTEN/NOTIFY applied (commit) or discarded (abort), GUC level ended. The block becomes `Idle`
    /// before anything else can fail.
    fn end_transaction(&mut self, commit: bool) -> Result<CommitOutcome> {
        self.block = BlockState::Idle;
        let result = match self.txn.take() {
            Some(txn) => {
                if commit {
                    txn.commit()
                } else {
                    txn.rollback().map(|()| CommitOutcome::RolledBack)
                }
            }
            None => Ok(CommitOutcome::RolledBack),
        };
        let committed = matches!(result, Ok(CommitOutcome::Committed { .. }));
        let cleanup = self.finish_transaction(committed);
        let outcome = result?;
        cleanup?;
        Ok(outcome)
    }

    /// Per-end cleanup shared by every transaction end (module docs).
    fn finish_transaction(&mut self, committed: bool) -> Result<()> {
        self.portals.clear();
        let pid = self.pid();
        {
            let mut listen = self.listen.borrow_mut();
            if committed {
                listen.at_commit(self.db.notify_hub(), pid);
            } else {
                listen.at_abort();
            }
        }
        self.guc.at_txn_end(committed)
    }

    /// Characteristics of a block chained from a failed one: the abort already reverted the
    /// transaction-scoped `transaction_isolation` / `transaction_read_only` settings, so xact.c
    /// `RestoreTransactionCharacteristics` restores the session defaults.
    fn chain_defaults(&self) -> Result<TxnOptions> {
        let defaults = guc_derive::txn_defaults(&self.guc)?;
        Ok(TxnOptions { kind: TxnKind::Explicit, ..defaults })
    }

    /// Start a new explicit block with the given characteristics (`AND CHAIN`).
    fn start_chained(&mut self, opts: TxnOptions) -> Result<()> {
        let txn = self.db.engine().begin(&self.handle, &opts)?;
        self.guc.at_txn_start()?;
        self.txn = Some(txn);
        self.block = BlockState::InBlock;
        Ok(())
    }

    /// The core transaction of the current state, for statements that need it.
    fn current_txn_mut(&mut self) -> Result<&mut Txn> {
        self.txn.as_mut().ok_or_else(no_txn_err)
    }

    /// Queue a WARNING for the next statement output.
    fn push_warning(&mut self, sqlstate: SqlState, message: &str) {
        self.pending_notices.push(PgError::new(sqlstate, message).with_severity(Severity::Warning));
    }

    /// BEGIN from `Started` / `ImplicitBlock` (module docs). The core transaction of an autocommit or
    /// implicit transaction is `Implicit` kind (savepoints refuse it), so when it has written nothing it
    /// is replaced by an explicit one with the same characteristics and then the modes are applied.
    /// An implicit transaction that already wrote keeps its kind (core has no way to change it).
    fn convert_to_block(&mut self, modes: &TxnModes) -> Result<()> {
        let mut current = self.txn.take().ok_or_else(no_txn_err)?;
        if current.has_written() {
            let res = apply_modes(&mut current, modes);
            self.txn = Some(current);
            res?;
        } else {
            let opts = characteristics(&current);
            // Rolling back the empty implicit transaction first: `Txn::begin` sets the activity fields
            // that a rollback clears, so the new transaction must begin after it.
            let fresh = match current.rollback().and_then(|()| self.db.engine().begin(&self.handle, &opts)) {
                Ok(t) => t,
                Err(e) => {
                    // The implicit transaction is gone; ending it is what an error in an implicit block does.
                    self.block = BlockState::Idle;
                    let _ = self.finish_transaction(false);
                    return Err(e);
                }
            };
            self.txn = Some(fresh);
            let res = match self.txn.as_mut() {
                Some(t) => apply_modes(t, modes),
                None => Err(no_txn_err()),
            };
            res?;
        }
        self.block = BlockState::InBlock;
        Ok(())
    }

    /// `BEGIN` / `START TRANSACTION` (module docs).
    fn begin_block(&mut self, modes: &TxnModes) -> Result<StatementResult> {
        match self.block {
            BlockState::InBlock | BlockState::Failed => {
                self.push_warning(SqlState::ACTIVE_SQL_TRANSACTION, "there is already a transaction in progress");
                // PG `standard_ProcessUtility` applies the modes of a nested BEGIN/START TRANSACTION
                // after the warning (errors such as 25001 after the first query still surface).
                if self.block == BlockState::InBlock {
                    if let Some(t) = self.txn.as_mut() {
                        apply_modes(t, modes)?;
                    }
                }
            }
            _ => self.convert_to_block(modes)?,
        }
        Ok(command("BEGIN"))
    }

    /// `COMMIT [AND CHAIN]` / `END` (module docs).
    fn commit_statement(&mut self, chain: bool) -> Result<StatementResult> {
        match self.block {
            BlockState::InBlock | BlockState::Failed => {
                let failed = self.block == BlockState::Failed;
                let chars = characteristics(self.txn.as_ref().ok_or_else(no_txn_err)?);
                let outcome = self.end_transaction(true)?;
                if chain {
                    self.start_chained(if failed { self.chain_defaults()? } else { chars })?;
                }
                Ok(command(tag_for(outcome)))
            }
            _ => {
                if chain {
                    return Err(PgError::new(
                        SqlState::NO_ACTIVE_SQL_TRANSACTION,
                        "COMMIT AND CHAIN can only be used in transaction blocks",
                    ));
                }
                self.push_warning(SqlState::NO_ACTIVE_SQL_TRANSACTION, "there is no transaction in progress");
                let outcome = self.end_transaction(true)?;
                Ok(command(tag_for(outcome)))
            }
        }
    }

    /// `ROLLBACK [AND CHAIN]` / `ABORT` (module docs).
    fn rollback_statement(&mut self, chain: bool) -> Result<StatementResult> {
        match self.block {
            BlockState::InBlock | BlockState::Failed => {
                let failed = self.block == BlockState::Failed;
                let chars = characteristics(self.txn.as_ref().ok_or_else(no_txn_err)?);
                self.end_transaction(false)?;
                if chain {
                    self.start_chained(if failed { self.chain_defaults()? } else { chars })?;
                }
                Ok(command("ROLLBACK"))
            }
            _ => {
                if chain {
                    return Err(PgError::new(
                        SqlState::NO_ACTIVE_SQL_TRANSACTION,
                        "ROLLBACK AND CHAIN can only be used in transaction blocks",
                    ));
                }
                self.push_warning(SqlState::NO_ACTIVE_SQL_TRANSACTION, "there is no transaction in progress");
                self.end_transaction(false)?;
                Ok(command("ROLLBACK"))
            }
        }
    }

    /// `SAVEPOINT name` (module docs).
    fn savepoint_statement(&mut self, name: &str) -> Result<StatementResult> {
        self.require_transaction_block("SAVEPOINT")?;
        self.current_txn_mut()?.savepoint(name)?;
        self.guc.at_savepoint(name)?;
        Ok(command("SAVEPOINT"))
    }

    /// `RELEASE [SAVEPOINT] name` (module docs).
    fn release_statement(&mut self, name: &str) -> Result<StatementResult> {
        self.require_transaction_block("RELEASE SAVEPOINT")?;
        self.current_txn_mut()?.release_savepoint(name)?;
        self.guc.at_release(name)?;
        Ok(command("RELEASE"))
    }

    /// `ROLLBACK TO [SAVEPOINT] name` (module docs). A failed block becomes `InBlock` again.
    fn rollback_to_statement(&mut self, name: &str) -> Result<StatementResult> {
        self.require_transaction_block("ROLLBACK TO SAVEPOINT")?;
        self.current_txn_mut()?.rollback_to_savepoint(name)?;
        self.block = BlockState::InBlock;
        self.guc.at_rollback_to(name)?;
        Ok(command("ROLLBACK"))
    }

    /// `SET TRANSACTION modes` (module docs).
    fn set_transaction_statement(&mut self, modes: &TxnModes) -> Result<StatementResult> {
        if self.block == BlockState::Started {
            self.push_warning(
                SqlState::NO_ACTIVE_SQL_TRANSACTION,
                "SET TRANSACTION can only be used in transaction blocks",
            );
        }
        apply_modes(self.current_txn_mut()?, modes)?;
        Ok(command("SET"))
    }

    /// `SET SESSION CHARACTERISTICS AS TRANSACTION modes` (module docs).
    fn set_session_characteristics(&mut self, modes: &TxnModes) -> Result<StatementResult> {
        if let Some(level) = modes.isolation {
            let value = core_isolation(level).as_pg_str().to_string();
            self.guc.set("default_transaction_isolation", &[value], GucScope::Session)?;
        }
        if let Some(read_only) = modes.read_only {
            self.guc.set("default_transaction_read_only", &[on_off(read_only)], GucScope::Session)?;
        }
        if let Some(deferrable) = modes.deferrable {
            self.guc.set("default_transaction_deferrable", &[on_off(deferrable)], GucScope::Session)?;
        }
        Ok(command("SET"))
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
        if self.block != BlockState::Started {
            return Ok(None);
        }
        self.end_transaction(true).map(Some)
    }

    /// Error cleanup per state (module docs).
    pub(crate) fn abort_after_error(&mut self, err: &PgError) -> Result<()> {
        // The error's text and SQLSTATE do not change the transition; only the block state does.
        let _ = err;
        match self.block {
            BlockState::Idle => {
                // No transaction to end: discard portals and pending LISTEN/NOTIFY work only.
                self.portals.clear();
                self.listen.borrow_mut().at_abort();
                Ok(())
            }
            BlockState::Started | BlockState::ImplicitBlock => self.end_transaction(false).map(|_| ()),
            BlockState::InBlock => {
                self.current_txn_mut()?.mark_failed();
                self.block = BlockState::Failed;
                self.listen.borrow_mut().at_abort();
                Ok(())
            }
            BlockState::Failed => {
                self.listen.borrow_mut().at_abort();
                Ok(())
            }
        }
    }

    /// 25P02 check for a statement about to be bound while the block is `Failed` (`stmt` is `None` when
    /// the kind is not known yet: then only transaction-control keywords of the raw text are
    /// recognized: the caller binds transaction statements with the cached env first).
    pub(crate) fn check_statement_allowed(&self, stmt: &BoundStatement) -> Result<()> {
        if self.block == BlockState::Failed && !allowed_in_failed_block(stmt) {
            return Err(failed_block_err());
        }
        Ok(())
    }

    /// Execute a transaction-control statement (module docs).
    pub(crate) fn exec_transaction_stmt(&mut self, t: &TransactionStmt) -> Result<StatementResult> {
        // A statement outside any transaction runs in a transaction of its own (xact.c
        // StartTransactionCommand); the other states already have one.
        self.ensure_txn()?;
        if self.block == BlockState::Failed {
            let accepted = matches!(
                t,
                TransactionStmt::Begin(_)
                    | TransactionStmt::Commit { .. }
                    | TransactionStmt::Rollback { .. }
                    | TransactionStmt::RollbackToSavepoint(_)
            );
            if !accepted {
                return Err(failed_block_err());
            }
        }
        match t {
            TransactionStmt::Begin(modes) => self.begin_block(modes),
            TransactionStmt::Commit { chain } => self.commit_statement(*chain),
            TransactionStmt::Rollback { chain } => self.rollback_statement(*chain),
            TransactionStmt::Savepoint(name) => self.savepoint_statement(name),
            TransactionStmt::ReleaseSavepoint(name) => self.release_statement(name),
            TransactionStmt::RollbackToSavepoint(name) => self.rollback_to_statement(name),
            TransactionStmt::SetTransaction(modes) => self.set_transaction_statement(modes),
            TransactionStmt::SetSessionCharacteristics(modes) => self.set_session_characteristics(modes),
        }
    }

    /// 25P01 `{what} can only be used in transaction blocks` unless the state is `InBlock`/`Failed`
    /// (PG `RequireTransactionBlock`: LOCK TABLE, SAVEPOINT, DECLARE, ...). `ImplicitBlock` counts as NOT
    /// a block here (oracle: `lock table t; select 1` in one Query string fails).
    pub(crate) fn require_transaction_block(&self, what: &str) -> Result<()> {
        match self.block {
            BlockState::InBlock | BlockState::Failed => Ok(()),
            _ => Err(PgError::new(
                SqlState::NO_ACTIVE_SQL_TRANSACTION,
                format!("{what} can only be used in transaction blocks"),
            )),
        }
    }

    /// 25001 `{what} cannot run inside a transaction block` when the state is `ImplicitBlock`, `InBlock`
    /// or `Failed` (PG `PreventInTransactionBlock`: VACUUM, DISCARD ALL, CREATE/DROP DATABASE, CREATE
    /// INDEX CONCURRENTLY...). Oracle: `select 1; discard all` and `select 1; vacuum` fail; a single
    /// statement (`Started`) passes.
    pub(crate) fn prevent_in_transaction_block(&self, what: &str) -> Result<()> {
        match self.block {
            BlockState::ImplicitBlock | BlockState::InBlock | BlockState::Failed => Err(PgError::new(
                SqlState::ACTIVE_SQL_TRANSACTION,
                format!("{what} cannot run inside a transaction block"),
            )),
            _ => Ok(()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::database::Database;
    use crate::types::{SessionOptions, TxnStatus};
    use pgarbor_sql::ir::BoundStatement;

    #[test]
    fn failed_block_error_is_25p02() {
        let e = failed_block_err();
        assert_eq!(e.sqlstate.code(), "25P02");
        assert_eq!(e.message, "current transaction is aborted, commands ignored until end of transaction block");
    }

    #[test]
    fn failed_block_accepts_only_exit_statements() {
        let commit = BoundStatement::Transaction(TransactionStmt::Commit { chain: false });
        let rb = BoundStatement::Transaction(TransactionStmt::Rollback { chain: true });
        let rb_to = BoundStatement::Transaction(TransactionStmt::RollbackToSavepoint("a".to_string()));
        let sp = BoundStatement::Transaction(TransactionStmt::Savepoint("a".to_string()));
        assert!(allowed_in_failed_block(&commit));
        assert!(allowed_in_failed_block(&rb));
        assert!(allowed_in_failed_block(&rb_to));
        assert!(!allowed_in_failed_block(&sp));
    }

    #[test]
    fn sql_isolation_maps_to_core() {
        assert_eq!(core_isolation(SqlIsolation::ReadUncommitted), IsolationLevel::ReadUncommitted);
        assert_eq!(core_isolation(SqlIsolation::ReadCommitted), IsolationLevel::ReadCommitted);
        assert_eq!(core_isolation(SqlIsolation::RepeatableRead), IsolationLevel::RepeatableRead);
        assert_eq!(core_isolation(SqlIsolation::Serializable), IsolationLevel::Serializable);
    }

    #[test]
    fn end_tags_and_bool_spelling() {
        assert_eq!(tag_for(CommitOutcome::Committed { seq: None }), "COMMIT");
        assert_eq!(tag_for(CommitOutcome::RolledBack), "ROLLBACK");
        assert_eq!(on_off(true), "on");
        assert_eq!(on_off(false), "off");
    }

    #[test]
    fn block_requirements_follow_state() {
        let db = Database::in_memory().expect("in-memory database");
        let mut s = db.session("main", SessionOptions::default()).expect("session");
        assert!(s.require_transaction_block("SAVEPOINT").is_err());
        assert!(s.prevent_in_transaction_block("VACUUM").is_ok());
        s.block = BlockState::ImplicitBlock;
        assert!(s.require_transaction_block("LOCK TABLE").is_err());
        assert_eq!(s.prevent_in_transaction_block("VACUUM").err().map(|e| e.sqlstate.code().to_string()).as_deref(), Some("25001"));
        s.block = BlockState::InBlock;
        assert!(s.require_transaction_block("SAVEPOINT").is_ok());
        s.block = BlockState::Failed;
        assert!(s.require_transaction_block("SAVEPOINT").is_ok());
        s.block = BlockState::Idle;
    }

    // Session-level behavior: needs the connect path (API-04/API-05) and simple Query (API-07/API-08).

    fn session() -> Session {
        let db = Database::in_memory().expect("in-memory database");
        db.session("main", SessionOptions::default()).expect("session")
    }

    #[test]
    #[ignore = "needs API-04"]
    fn commit_outside_block_warns_and_tags_commit() {
        let mut s = session();
        let out = s.simple_query_raw("COMMIT");
        assert!(out.error.is_none());
        assert_eq!(out.statements[0].result.tag(), "COMMIT");
        assert!(out.statements[0].notices.iter().any(|n| n.message == "there is no transaction in progress"));
        assert_eq!(out.status, TxnStatus::Idle);
    }

    #[test]
    #[ignore = "needs API-04"]
    fn savepoint_outside_block_is_25p01() {
        let mut s = session();
        let out = s.simple_query_raw("SAVEPOINT a");
        let err = out.error.expect("error");
        assert_eq!(err.sqlstate.code(), "25P01");
        assert_eq!(err.message, "SAVEPOINT can only be used in transaction blocks");
    }

    #[test]
    #[ignore = "needs API-04"]
    fn begin_twice_warns_25001() {
        let mut s = session();
        let out = s.simple_query_raw("BEGIN; BEGIN");
        assert!(out.error.is_none());
        assert_eq!(out.status, TxnStatus::InTransaction);
        assert!(out.statements[1].notices.iter().any(|n| n.message == "there is already a transaction in progress"));
    }

    #[test]
    #[ignore = "needs API-04"]
    fn error_in_block_fails_until_rollback() {
        let mut s = session();
        let out = s.simple_query_raw("BEGIN; SELECT 1/0; SELECT 1");
        assert_eq!(out.status, TxnStatus::Failed);
        let next = s.simple_query_raw("SELECT 1");
        assert_eq!(next.error.as_ref().map(|e| e.sqlstate.code()), Some("25P02"));
        let end = s.simple_query_raw("COMMIT");
        assert_eq!(end.statements[0].result.tag(), "ROLLBACK");
        assert_eq!(end.status, TxnStatus::Idle);
    }

    #[test]
    #[ignore = "needs API-04"]
    fn rollback_to_savepoint_leaves_failed_state() {
        let mut s = session();
        let out = s.simple_query_raw("BEGIN; SAVEPOINT a; SELECT 1/0");
        assert_eq!(out.status, TxnStatus::Failed);
        let back = s.simple_query_raw("ROLLBACK TO a");
        assert!(back.error.is_none());
        assert_eq!(back.status, TxnStatus::InTransaction);
    }
}
