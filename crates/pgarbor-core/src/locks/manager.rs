//! `LockManager`: acquire/release/wait (TXN-09, TXN-10). Lease CORE-09 (opus).
//!
//! # Data
//! One `Mutex<LockTable>` (table.rs). Critical sections are short (no I/O, no waiting while holding
//! it). Waiters sleep on their own `WaitSlot` condvar, never on the table mutex.
//! Lock order: `table` mutex → a `WaitSlot.status` mutex (releasers set Granted while holding the
//! table). Waiters lock their slot WITHOUT holding the table, then re-take the table to clean up.
//! No other mutex (session activity, container writer) may be held while calling into the manager
//! except `SessionShared.activity`, which `acquire` takes briefly AFTER releasing the table.
//!
//! # Conflict rule (PG lock.c LockCheckConflicts + ProcSleep queue rules)
//! A request (session S, mode M) on tag T is grantable iff:
//! 1. M conflicts with no granted mode on T held by a session OTHER than S; and
//! 2. M conflicts with no mode requested by a waiter already queued on T, EXCEPT when S already
//!    holds a granted lock on T that conflicts with that waiter's mode (then S would deadlock the
//!    waiter by queueing behind it; PG inserts ahead — we grant immediately if (1) holds).
//! Same-session grants never conflict. Re-requesting an already held (session, scope, mode)
//! increments `count` and returns `Acquired{newly:false}` immediately.
//! A row mode on a non-Key tag or a table mode on a Key tag → XX000 internal error.
//!
//! # acquire (policy Block)
//! 1. Lock table. Grantable → push/increment `Grant`, update `held_by`, return Acquired.
//! 2. `NoWait`/`SkipLocked` → return `NotAvailable` (nothing queued).
//! 3. Enqueue `Waiter{slot: WaitSlot::new(), since: now}` at the queue tail, set `waiting_on[S]`,
//!    `session_txn[S]` (from scope), unlock table, set session activity
//!    `wait = Some(WaitEvent{event_type:"Lock", event: tag.wait_event()})`.
//! 4. Loop: wait on `slot.cv` with timeout = min(100ms interrupt poll, time until deadlock check,
//!    time until lock_timeout, time until statement deadline). After each wake:
//!    a. status Granted → clear wait info, return `Acquired{newly:true}`.
//!    b. `interrupts.check()` Err → `cancel_wait` and return that error (57014 / FATAL 57P01).
//!    c. `limits.lock_timeout_ms > 0` and waited >= it → cancel_wait, `error::lock_timeout()`.
//!    d. deadline passed → cancel_wait, `error::statement_timeout()`.
//!    e. waited >= `deadlock_timeout_ms` and the check has not run yet for this wait: lock table,
//!       `deadlock::wait_for_edges(&table)` + `deadlock::find_cycle(&edges, S)`; Some(cycle) →
//!       remove own waiter, unlock, return `deadlock::deadlock_error(&cycle)` (the session whose
//!       timer fires first aborts itself — PG semantics; oracle: the earlier waiter is the victim).
//!       Run the check once per wait (PG reruns only on new waits).
//!    `cancel_wait`: lock table; if the slot became Granted meanwhile, the grant stands — release
//!    it immediately (decrement / remove) so the caller's error leaves no lock; else remove the
//!    waiter; then run `grant_waiters(tag)` (removing a waiter can unblock later ones).
//!    ALWAYS clear `waiting_on[S]` and the session wait event on exit.
//!
//! # release paths (all → `grant_waiters` on every touched tag)
//! `grant_waiters(tag)`: walk the queue front to back; grant each waiter that is grantable against
//! the granted set AND against waiters still ahead of it in the queue (FIFO fairness), moving it to
//! `granted` (count 1), setting its slot Granted and `notify_one`. Remove empty entries.
//!
//! Wake-up race rule: a Granted waiter that then times out must observe Granted first (check slot
//! status before evaluating timeouts) — a granted lock is never silently dropped.
use crate::locks::mode::LockMode;
use crate::locks::table::{LockInfo, LockScope, LockTable};
use crate::locks::tag::LockTag;
use crate::session::SessionShared;
use crate::types::{AcquireResult, SessionId, SubXactId, WaitLimits, WaitPolicy};
use pgarbor_common::{PgError, Result, TxnId};
use std::sync::Mutex;

// CONTRACT
#[derive(Debug, Default)]
pub struct LockManager {
    pub(crate) table: Mutex<LockTable>,
}

impl LockManager {
    pub fn new() -> LockManager {
        LockManager::default()
    }

    /// Acquire `mode` on `tag` for `session` with `scope` (module docs).
    #[allow(unused_variables)]
    pub fn acquire(
        &self,
        session: &SessionShared,
        scope: LockScope,
        tag: &LockTag,
        mode: LockMode,
        policy: WaitPolicy,
        limits: &WaitLimits,
    ) -> Result<AcquireResult> {
        Err(PgError::unimplemented("CORE-09", "lock acquisition"))
    }

    /// Release ONE count of (session, scope, mode) on `tag` (advisory unlock). Returns false when
    /// no such grant exists. Count reaching 0 removes the grant and wakes waiters.
    #[allow(unused_variables)]
    pub fn release_one(&self, session: SessionId, scope: LockScope, tag: &LockTag, mode: LockMode) -> Result<bool> {
        Err(PgError::unimplemented("CORE-09", "single lock release"))
    }

    /// Release every Transaction-scope grant of `txn` (commit/abort). Also drops
    /// `session_txn[session]` when it equals `txn`.
    #[allow(unused_variables)]
    pub fn release_txn(&self, session: SessionId, txn: TxnId) -> Result<()> {
        Err(PgError::unimplemented("CORE-09", "transaction lock release"))
    }

    /// ROLLBACK TO SAVEPOINT: release Transaction-scope grants of `txn` whose `subxact >= from`.
    #[allow(unused_variables)]
    pub fn release_subxacts(&self, session: SessionId, txn: TxnId, from: SubXactId) -> Result<()> {
        Err(PgError::unimplemented("CORE-09", "subtransaction lock release"))
    }

    /// RELEASE SAVEPOINT: re-tag grants of `txn` with `subxact >= from` as `into` (the parent's
    /// subxact id) so a later ROLLBACK TO an outer savepoint still releases them. Merge counts when
    /// an equal (session, scope, mode) grant already exists.
    #[allow(unused_variables)]
    pub fn reassign_subxacts(&self, session: SessionId, txn: TxnId, from: SubXactId, into: SubXactId) -> Result<()> {
        Err(PgError::unimplemented("CORE-09", "subtransaction lock reassignment"))
    }

    /// Release every Session-scope grant of `session` (pg_advisory_unlock_all, disconnect), and
    /// remove any queued waiter of that session (defensive). Returns how many grants were removed.
    #[allow(unused_variables)]
    pub fn release_session(&self, session: SessionId) -> Result<usize> {
        Err(PgError::unimplemented("CORE-09", "session lock release"))
    }

    /// Release Session-scope grants matching `filter` (used by `pg_advisory_unlock_all`, which only
    /// drops advisory locks).
    #[allow(unused_variables)]
    pub fn release_session_where(&self, session: SessionId, filter: &dyn Fn(&LockTag) -> bool) -> Result<usize> {
        Err(PgError::unimplemented("CORE-09", "filtered session lock release"))
    }

    /// Wake `session`'s waiter (if any) without granting, so it re-checks its interrupts promptly
    /// (called after cancel/terminate requests).
    #[allow(unused_variables)]
    pub fn wake(&self, session: SessionId) -> Result<()> {
        Err(PgError::unimplemented("CORE-09", "lock waiter wake-up"))
    }

    /// Whether `session` holds `mode` (any scope) on `tag`.
    #[allow(unused_variables)]
    pub fn holds(&self, session: SessionId, tag: &LockTag, mode: LockMode) -> Result<bool> {
        Err(PgError::unimplemented("CORE-09", "lock ownership query"))
    }

    /// Every grant and every waiter as `LockInfo` (pg_locks), ordered by (session, granted desc,
    /// locktype, tag). One consistent copy under the table mutex.
    #[allow(unused_variables)]
    pub fn snapshot(&self) -> Result<Vec<LockInfo>> {
        Err(PgError::unimplemented("CORE-09", "lock table snapshot"))
    }

    /// `pg_blocking_pids(pid)`: sessions holding a conflicting grant on the tag `session` waits on,
    /// plus sessions queued AHEAD of it with a conflicting requested mode (PG includes both),
    /// deduplicated, ascending. Not waiting → empty.
    #[allow(unused_variables)]
    pub fn blocking_sessions(&self, session: SessionId) -> Result<Vec<SessionId>> {
        Err(PgError::unimplemented("CORE-09", "blocking session query"))
    }
}
