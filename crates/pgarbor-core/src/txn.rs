//! `Txn`: one transaction on one branch/database (ARCHITECTURE §7, TXN-01..08). Lease CORE-04
//! (opus) for `begin`, `begin_statement`, `set_*`, `lock_key*`, `lock_relation`, `rollback`,
//! `release_all`. Accessors and `Drop` are implemented.
//!
//! Methods of `Txn` are spread over files by concern, each method declared in exactly ONE file:
//! txn.rs (lifecycle, snapshots, locks), savepoint.rs (SAVEPOINT/RELEASE/ROLLBACK TO),
//! commit.rs (COMMIT), txn_kv.rs (`kv()` view). Implementers must NOT add new methods to `Txn`;
//! put helpers in private free functions of their own file.
//!
//! # Snapshots (TXN-02, TXN-07, TXN-08)
//! A snapshot is ONE `pgarbor_storage::Snapshot` plus the branch record read from THAT snapshot's
//! directory, and the database tree `snap.tree(record.database_root(db))` of that same snapshot.
//! Never pair a root read from one storage snapshot with another snapshot (a newer root against an
//! older snapshot fails with XX001 `page_out_of_range`): always go through
//! `Engine::branch_head`, which returns the matched triple.
//! - READ COMMITTED / READ UNCOMMITTED: `begin_statement` takes a fresh head every statement.
//! - REPEATABLE READ / SERIALIZABLE: the first `begin_statement` takes it; later ones keep it.
//! Own writes live in the overlay and are visible through `kv()` over any snapshot.
//!
//! # Branch generation guard
//! `begin` records `record.generation`. Every fresh head (`begin_statement` in RC, `lock_key`
//! outcome checks, commit) verifies the record is still Active with the same generation; otherwise
//! `error::branch_changed_under_transaction(name)` (forced reset/delete happened under us).
//!
//! # Key locks and LockOutcome (TXN-09)
//! `lock_key(key, mode, policy)`:
//! 1. Fast path: `held_keys[key]` exists with a mode at least as strong (ForUpdate > ForNoKeyUpdate
//!    > ForShare > ForKeyShare) → `Unchanged` without touching the lock manager.
//! 2. `locks.acquire(session, Transaction{txn, current_subxact}, Key{branch, db, key}, mode,
//!    policy, limits)`; `NotAvailable` → `LockOutcome::NotAvailable`.
//! 3. Record `held_keys[key] = (mode, current_subxact)`.
//! 4. Outcome: if no snapshot yet → `Unchanged`. Take `head = engine.branch_head(..)` AFTER the
//!    grant (so a just-committed holder's change is visible). If `head.snapshot.commit_seq() ==`
//!    the txn snapshot's commit seq → `Unchanged` (nothing committed since). Else compare
//!    `head.tree.get(key)` with `self.snapshot.get(key)` (snapshot tree only, NOT the overlay):
//!    equal → `Unchanged`, else `Changed(latest)`.
//! `lock_key_for_write`: `lock_key(key, ForUpdate, Block)`; under RR/SERIALIZABLE `Changed(Some)` →
//! `error::serialization_concurrent_update()`, `Changed(None)` →
//! `error::serialization_concurrent_delete()`; RC returns the outcome for EvalPlanQual.
//!
//! # Write tracking
//! First successful write (TxnKv put/delete) sets `wrote = true` and the session activity
//! `backend_xid = Some(id)`.
use crate::engine::Engine;
use crate::error;
use crate::locks::mode::LockMode;
use crate::locks::table::LockScope;
use crate::locks::tag::LockTag;
use crate::overlay::Overlay;
use crate::savepoint::Savepoint;
use crate::session::SessionShared;
use crate::types::{
    AcquireResult, IsolationLevel, LockOutcome, SubXactId, TxnKind, TxnOptions, TxnState, WaitLimits, WaitPolicy,
};
use pgarbor_common::{BranchId, CommitSeq, KvRead, Oid, RelId, Result, TxnId};
use pgarbor_storage::TreeSnapshot;
use std::collections::HashMap;
use std::sync::Arc;

// CONTRACT
/// A key lock this transaction holds (fast-path cache of the lock manager's state).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct HeldKey {
    pub mode: LockMode,
    pub subxact: SubXactId,
}

// CONTRACT
/// The transaction's read snapshot.
#[derive(Clone, Debug)]
pub struct TxnSnapshot {
    /// Tree of the transaction's database as of `commit_seq`.
    pub tree: TreeSnapshot,
    pub commit_seq: CommitSeq,
}

// CONTRACT
#[derive(Debug)]
pub struct Txn {
    pub(crate) engine: Arc<Engine>,
    pub(crate) session: Arc<SessionShared>,
    pub(crate) id: TxnId,
    pub(crate) branch: BranchId,
    pub(crate) branch_name: String,
    pub(crate) database: Oid,
    /// `BranchRecord.generation` at begin.
    pub(crate) generation: u64,
    pub(crate) kind: TxnKind,
    pub(crate) isolation: IsolationLevel,
    pub(crate) read_only: bool,
    pub(crate) deferrable: bool,
    pub(crate) state: TxnState,
    /// None until the first `begin_statement` (PG "FirstSnapshotSet").
    pub(crate) snapshot: Option<TxnSnapshot>,
    /// Statements that took a snapshot so far.
    pub(crate) statements: u64,
    pub(crate) overlay: Overlay,
    pub(crate) savepoints: Vec<Savepoint>,
    /// Last subxact id handed out (ids start at 1; 0 = top level).
    pub(crate) last_subxact: u32,
    pub(crate) held_keys: HashMap<Vec<u8>, HeldKey>,
    pub(crate) limits: WaitLimits,
    pub(crate) wrote: bool,
    pub(crate) started_unix_ms: u64,
    /// Set by commit/rollback; `Drop` releases locks only when false.
    pub(crate) finished: bool,
}

// CONTRACT: Txn moves between threads with its session (wire: thread per connection).
const _: fn() = || {
    fn assert_send<T: Send>() {}
    assert_send::<Txn>();
};

impl Txn {
    /// Start a transaction for `session` (called by `Engine::begin`).
    /// - `id = engine.next_txn_id()`; `head = engine.branch_head(session.branch, session.database_oid)`
    ///   (branch deleted → `pgarbor_branches::error::branch_deleted`), keep `generation` and name;
    ///   do NOT keep the head's snapshot (snapshots are taken by `begin_statement`).
    /// - read-only engine forces `read_only = true`.
    /// - session activity: `xact_start_ms = now`, `txn = Some(id)`, `backend_xid = None`.
    /// - register `session_txn` for deadlock reports happens lazily in the lock manager.
    pub fn begin(engine: Arc<Engine>, session: Arc<SessionShared>, opts: &TxnOptions) -> Result<Txn> {
        let head = engine.branch_head(&session.branch, session.database_oid)?;
        if !head.record.is_active() {
            return Err(pgarbor_branches::error::branch_deleted(&head.record.name, &head.record.id));
        }
        let id = engine.next_txn_id();
        let now = now_unix_ms();
        let read_only = opts.read_only || engine.is_read_only();
        session.update_activity(|a| {
            a.xact_start_ms = Some(now);
            a.txn = Some(id);
            a.backend_xid = None;
            a.wait = None;
        });
        Ok(Txn {
            engine,
            branch: session.branch,
            branch_name: head.record.name.clone(),
            database: session.database_oid,
            session,
            id,
            generation: head.record.generation,
            kind: opts.kind,
            isolation: opts.isolation,
            read_only,
            deferrable: opts.deferrable,
            state: TxnState::InProgress,
            snapshot: None,
            statements: 0,
            overlay: Overlay::new(),
            savepoints: Vec::new(),
            last_subxact: 0,
            held_keys: HashMap::new(),
            limits: WaitLimits::default(),
            wrote: false,
            started_unix_ms: now,
            finished: false,
        })
    }

    pub fn id(&self) -> TxnId {
        self.id
    }
    pub fn engine(&self) -> &Arc<Engine> {
        &self.engine
    }
    pub fn session(&self) -> &Arc<SessionShared> {
        &self.session
    }
    pub fn branch(&self) -> BranchId {
        self.branch
    }
    pub fn branch_name(&self) -> &str {
        &self.branch_name
    }
    pub fn database(&self) -> Oid {
        self.database
    }
    pub fn kind(&self) -> TxnKind {
        self.kind
    }
    pub fn isolation(&self) -> IsolationLevel {
        self.isolation
    }
    pub fn is_read_only(&self) -> bool {
        self.read_only
    }
    pub fn is_deferrable(&self) -> bool {
        self.deferrable
    }
    pub fn state(&self) -> TxnState {
        self.state
    }
    pub fn has_written(&self) -> bool {
        self.wrote
    }
    /// Whether a snapshot has been taken (PG FirstSnapshotSet).
    pub fn has_snapshot(&self) -> bool {
        self.snapshot.is_some()
    }
    pub fn snapshot(&self) -> Option<&TxnSnapshot> {
        self.snapshot.as_ref()
    }
    pub fn overlay(&self) -> &Overlay {
        &self.overlay
    }
    /// Innermost savepoint's subxact id, or TOP.
    pub fn current_subxact(&self) -> SubXactId {
        self.savepoints.last().map(|s| s.subxact).unwrap_or(SubXactId::TOP)
    }
    pub fn limits(&self) -> WaitLimits {
        self.limits
    }
    /// Set by the session before each statement from `lock_timeout`, `deadlock_timeout`,
    /// `statement_timeout`.
    pub fn set_limits(&mut self, limits: WaitLimits) {
        self.limits = limits;
    }
    /// Mark the transaction failed (an error escaped a statement inside an explicit block). For an
    /// implicit transaction the caller rolls back instead.
    pub fn mark_failed(&mut self) {
        self.state = TxnState::Failed;
    }
    /// Poll interrupts (cancel / terminate / statement_timeout) — executors call this in loops.
    pub fn check_interrupts(&self) -> Result<()> {
        self.session.interrupts.check()
    }
    /// 25P02 unless InProgress. Session layer calls this before every statement other than
    /// ROLLBACK / ROLLBACK TO / COMMIT.
    pub fn check_not_failed(&self) -> Result<()> {
        match self.state {
            TxnState::Failed => Err(error::in_failed_transaction()),
            _ => Ok(()),
        }
    }
    /// 25006 `cannot execute <command> in a read-only transaction` when read-only.
    pub fn check_writable(&self, command: &str) -> Result<()> {
        if self.read_only {
            return Err(error::read_only_transaction(command));
        }
        Ok(())
    }

    /// Start of a statement that needs a snapshot: `check_not_failed`; RC/RU or no snapshot yet →
    /// fresh `engine.branch_head(..)` + generation guard → `snapshot = Some(TxnSnapshot{..})`;
    /// `statements += 1`. (Utility statements that need no snapshot — SET, SAVEPOINT, SHOW... —
    /// must not call this, so `SET TRANSACTION` stays legal after them.)
    pub fn begin_statement(&mut self) -> Result<()> {
        self.check_not_failed()?;
        if self.isolation.uses_statement_snapshot() || self.snapshot.is_none() {
            let head = fresh_head(self)?;
            self.snapshot = Some(TxnSnapshot { tree: head.tree, commit_seq: head.snapshot.commit_seq() });
        }
        self.statements += 1;
        Ok(())
    }

    /// `SET TRANSACTION ISOLATION LEVEL` (variable.c check_transaction_isolation): same level → Ok;
    /// inside a savepoint → `error::isolation_in_subtransaction`; snapshot already taken →
    /// `error::isolation_after_query`; else set.
    pub fn set_isolation(&mut self, level: IsolationLevel) -> Result<()> {
        if level == self.isolation {
            return Ok(());
        }
        // variable.c checks FirstSnapshotSet before IsSubTransaction.
        if self.snapshot.is_some() {
            return Err(error::isolation_after_query());
        }
        if !self.savepoints.is_empty() {
            return Err(error::isolation_in_subtransaction());
        }
        self.isolation = level;
        Ok(())
    }

    /// `SET TRANSACTION READ ONLY|READ WRITE` (check_transaction_read_only): setting read-only is
    /// always allowed; read-write while currently read-only: inside a savepoint →
    /// `read_write_inside_read_only`, snapshot taken → `read_write_after_query`, read-only engine →
    /// 25006 `cannot set transaction read-write mode during recovery`-style: use
    /// `error::read_only_transaction("SET TRANSACTION READ WRITE")`; else set.
    pub fn set_read_only(&mut self, read_only: bool) -> Result<()> {
        if !read_only && self.read_only {
            if !self.savepoints.is_empty() {
                return Err(error::read_write_inside_read_only());
            }
            if self.snapshot.is_some() {
                return Err(error::read_write_after_query());
            }
            if self.engine.is_read_only() {
                return Err(error::read_only_transaction("SET TRANSACTION READ WRITE"));
            }
        }
        self.read_only = read_only;
        Ok(())
    }

    /// `SET TRANSACTION [NOT] DEFERRABLE` (check_transaction_deferrable): inside a savepoint →
    /// `deferrable_in_subtransaction`; snapshot taken → `deferrable_after_query`; else set
    /// (DEFERRABLE has no effect in v1 besides being reported, as in PG for non-serializable).
    pub fn set_deferrable(&mut self, deferrable: bool) -> Result<()> {
        if !self.savepoints.is_empty() {
            return Err(error::deferrable_in_subtransaction());
        }
        if self.snapshot.is_some() {
            return Err(error::deferrable_after_query());
        }
        self.deferrable = deferrable;
        Ok(())
    }

    /// Lock a data-tree key (module docs).
    pub fn lock_key(&mut self, key: &[u8], mode: LockMode, policy: WaitPolicy) -> Result<LockOutcome> {
        if let Some(h) = self.held_keys.get(key) {
            if strength(h.mode) >= strength(mode) {
                return Ok(LockOutcome::Unchanged);
            }
        }
        let subxact = self.current_subxact();
        let tag = LockTag::Key { branch: self.branch, database: self.database, key: key.to_vec() };
        let scope = LockScope::Transaction { txn: self.id, subxact };
        let res = self.engine.locks().acquire(&self.session, scope, &tag, mode, policy, &self.limits)?;
        if res == AcquireResult::NotAvailable {
            return Ok(LockOutcome::NotAvailable);
        }
        // The cache keeps the strongest mode; only reached when the request is stronger anyway.
        self.held_keys.insert(key.to_vec(), HeldKey { mode, subxact });
        if self.snapshot.is_none() {
            return Ok(LockOutcome::Unchanged);
        }
        // Read the head AFTER the grant so a just-committed holder's change is visible.
        let head = fresh_head(self)?;
        let Some(snap) = self.snapshot.as_ref() else {
            return Ok(LockOutcome::Unchanged);
        };
        if head.snapshot.commit_seq() == snap.commit_seq {
            return Ok(LockOutcome::Unchanged);
        }
        let latest = head.tree.get(key)?;
        let mine = snap.tree.get(key)?;
        if latest == mine {
            Ok(LockOutcome::Unchanged)
        } else {
            Ok(LockOutcome::Changed(latest))
        }
    }

    /// Lock for UPDATE/DELETE with isolation-dependent conflict handling (module docs).
    pub fn lock_key_for_write(&mut self, key: &[u8]) -> Result<LockOutcome> {
        let outcome = self.lock_key(key, LockMode::ForUpdate, WaitPolicy::Block)?;
        if !self.isolation.uses_statement_snapshot() {
            if let LockOutcome::Changed(latest) = &outcome {
                return Err(if latest.is_some() {
                    error::serialization_concurrent_update()
                } else {
                    error::serialization_concurrent_delete()
                });
            }
        }
        Ok(outcome)
    }

    /// Table-level lock (DML takes RowExclusive/AccessShare, DDL AccessExclusive, LOCK TABLE any):
    /// `LockTag::Relation{branch, database, relid}` with scope Transaction{id, current_subxact}.
    pub fn lock_relation(&mut self, relid: RelId, mode: LockMode, policy: WaitPolicy) -> Result<AcquireResult> {
        let tag = LockTag::Relation { branch: self.branch, database: self.database, relid };
        let scope = LockScope::Transaction { txn: self.id, subxact: self.current_subxact() };
        self.engine.locks().acquire(&self.session, scope, &tag, mode, policy, &self.limits)
    }

    /// ROLLBACK (also of a Failed txn, and the implicit rollback of a failed implicit txn):
    /// `release_all()`, mark finished, session activity `xact_start_ms = None`, `txn = None`,
    /// `backend_xid = None`. Sequence state is not touched (non-transactional).
    pub fn rollback(mut self) -> Result<()> {
        let res = self.release_all();
        self.session.update_activity(|a| {
            a.xact_start_ms = None;
            a.txn = None;
            a.backend_xid = None;
            a.wait = None;
        });
        res
    }

    /// Discard the overlay and release all transaction-scope locks (`locks.release_txn`).
    /// Idempotent; sets `finished = true`. Used by rollback, commit (after durability) and Drop.
    pub fn release_all(&mut self) -> Result<()> {
        self.overlay.clear();
        self.savepoints.clear();
        self.held_keys.clear();
        let res = self.engine.locks().release_txn(self.session.id, self.id);
        self.finished = true;
        res
    }
}

/// Fresh branch head with the generation guard (module docs).
fn fresh_head(txn: &Txn) -> Result<crate::engine::BranchHead> {
    let head = txn.engine.branch_head(&txn.branch, txn.database)?;
    if !head.record.is_active() || head.record.generation != txn.generation {
        return Err(error::branch_changed_under_transaction(&txn.branch_name));
    }
    Ok(head)
}

/// Row-lock strength order: ForKeyShare < ForShare < ForNoKeyUpdate < ForUpdate. Table-level modes
/// never reach a key lock (the manager rejects them); they rank 0.
fn strength(mode: LockMode) -> u8 {
    match mode {
        LockMode::ForKeyShare => 1,
        LockMode::ForShare => 2,
        LockMode::ForNoKeyUpdate => 3,
        LockMode::ForUpdate => 4,
        _ => 0,
    }
}

fn now_unix_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

impl Drop for Txn {
    /// A transaction dropped without commit/rollback is rolled back (connection loss, panic
    /// unwinding in the caller). Errors are ignored.
    fn drop(&mut self) {
        if !self.finished {
            let _ = self.release_all();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strength_order() {
        assert!(strength(LockMode::ForKeyShare) < strength(LockMode::ForShare));
        assert!(strength(LockMode::ForShare) < strength(LockMode::ForNoKeyUpdate));
        assert!(strength(LockMode::ForNoKeyUpdate) < strength(LockMode::ForUpdate));
        assert_eq!(strength(LockMode::AccessShare), 0);
    }

    #[test]
    fn isolation_parse_roundtrip() {
        for l in [IsolationLevel::ReadCommitted, IsolationLevel::RepeatableRead, IsolationLevel::Serializable] {
            assert_eq!(IsolationLevel::parse(l.as_pg_str()).unwrap(), l);
        }
    }
}
