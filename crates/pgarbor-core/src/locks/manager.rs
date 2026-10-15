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
use crate::error;
use crate::locks::deadlock;
use crate::locks::mode::LockMode;
use crate::locks::table::{
    Grant, LockEntry, LockInfo, LockScope, LockTable, LockTagInfo, WaitSlot, WaitStatus, Waiter,
};
use crate::locks::tag::LockTag;
use crate::session::{SessionShared, WaitEvent};
use crate::types::{AcquireResult, SessionId, SubXactId, WaitLimits, WaitPolicy};
use pgarbor_common::{PgError, Result, TxnId};
use std::collections::{BTreeSet, VecDeque};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

/// Interrupt poll period of a lock wait.
const POLL: Duration = Duration::from_millis(100);

// CONTRACT
#[derive(Debug, Default)]
pub struct LockManager {
    pub(crate) table: Mutex<LockTable>,
}

fn unix_ms() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_millis() as u64).unwrap_or(0)
}

fn lock_slot(slot: &WaitSlot) -> MutexGuard<'_, WaitStatus> {
    slot.status.lock().unwrap_or_else(|e| e.into_inner())
}

fn check_mode(tag: &LockTag, mode: LockMode) -> Result<()> {
    let is_key = matches!(tag, LockTag::Key { .. });
    if is_key != mode.is_row_mode() {
        return Err(error::internal(format!("lock mode {:?} is not valid for this kind of lock tag", mode)));
    }
    Ok(())
}

// ---------------------------------------------------------------------------------------------
// Table-level logic (no threads, no waiting): testable on a bare `LockTable`.
// ---------------------------------------------------------------------------------------------

/// Whether a request (`session`, `mode`) is grantable against `granted` and the waiters `ahead`
/// of it in the queue (module docs, conflict rule).
fn grantable<'a>(
    granted: &[Grant],
    ahead: impl Iterator<Item = &'a Waiter>,
    session: SessionId,
    mode: LockMode,
) -> Result<bool> {
    for g in granted {
        if g.session != session && mode.conflicts_with(g.mode)? {
            return Ok(false);
        }
    }
    for w in ahead {
        if w.session == session || !mode.conflicts_with(w.mode)? {
            continue;
        }
        // Queueing behind `w` would deadlock it if we already hold something it needs.
        let mut holds_conflicting = false;
        for g in granted {
            if g.session == session && g.mode.conflicts_with(w.mode)? {
                holds_conflicting = true;
                break;
            }
        }
        if !holds_conflicting {
            return Ok(false);
        }
    }
    Ok(true)
}

/// Add one count of (session, scope, mode) on `tag`. Returns true when a new `Grant` was created.
fn add_grant(table: &mut LockTable, tag: &LockTag, session: SessionId, scope: LockScope, mode: LockMode) -> bool {
    let entry = table.entries.entry(tag.clone()).or_default();
    let newly = if let Some(g) =
        entry.granted.iter_mut().find(|g| g.session == session && g.scope == scope && g.mode == mode)
    {
        g.count = g.count.saturating_add(1);
        false
    } else {
        entry.granted.push(Grant { session, scope, mode, count: 1, granted_unix_ms: unix_ms() });
        true
    };
    table.held_by.entry(session).or_default().insert(tag.clone());
    newly
}

/// Drop `tag` from `held_by[session]` when the session no longer has a grant on it.
fn forget_held_if_none(table: &mut LockTable, tag: &LockTag, session: SessionId) {
    let still = table.entries.get(tag).map(|e| e.granted.iter().any(|g| g.session == session)).unwrap_or(false);
    if !still {
        if let Some(s) = table.held_by.get_mut(&session) {
            s.remove(tag);
            if s.is_empty() {
                table.held_by.remove(&session);
            }
        }
    }
}

/// Drop one count of (session, scope, mode); returns whether such a grant existed. The caller runs
/// `grant_waiters` afterwards.
fn release_one_in(table: &mut LockTable, session: SessionId, scope: LockScope, tag: &LockTag, mode: LockMode) -> bool {
    let Some(entry) = table.entries.get_mut(tag) else { return false };
    let Some(pos) = entry.granted.iter().position(|g| g.session == session && g.scope == scope && g.mode == mode)
    else {
        return false;
    };
    if entry.granted[pos].count > 1 {
        entry.granted[pos].count -= 1;
    } else {
        entry.granted.remove(pos);
    }
    forget_held_if_none(table, tag, session);
    true
}

/// Grant every queued waiter on `tag` that is grantable against the granted set and the waiters
/// still ahead of it; wake them. Removes the entry when empty.
fn grant_waiters(table: &mut LockTable, tag: &LockTag) -> Result<()> {
    let queue = match table.entries.get_mut(tag) {
        Some(e) => std::mem::take(&mut e.waiters),
        None => return Ok(()),
    };
    let mut kept: VecDeque<Waiter> = VecDeque::with_capacity(queue.len());
    let mut iter = queue.into_iter();
    let mut failure: Option<PgError> = None;
    while let Some(w) = iter.next() {
        let verdict = match table.entries.get(tag) {
            Some(e) => grantable(&e.granted, kept.iter(), w.session, w.mode),
            None => Ok(true),
        };
        match verdict {
            Ok(true) => {
                add_grant(table, tag, w.session, w.scope, w.mode);
                if table.waiting_on.get(&w.session) == Some(tag) {
                    table.waiting_on.remove(&w.session);
                }
                *lock_slot(&w.slot) = WaitStatus::Granted;
                w.slot.cv.notify_all();
            }
            Ok(false) => kept.push_back(w),
            Err(e) => {
                kept.push_back(w);
                kept.extend(iter.by_ref());
                failure = Some(e);
                break;
            }
        }
    }
    let empty = match table.entries.get_mut(tag) {
        Some(e) => {
            e.waiters = kept;
            e.is_empty()
        }
        None => false,
    };
    if empty {
        table.entries.remove(tag);
    }
    match failure {
        Some(e) => Err(e),
        None => Ok(()),
    }
}

/// Remove every grant of `session` for which `pred(tag, grant)` holds, then wake waiters on the
/// touched tags. Returns how many grants were removed.
fn release_matching(
    table: &mut LockTable,
    session: SessionId,
    pred: &dyn Fn(&LockTag, &Grant) -> bool,
) -> Result<usize> {
    let tags: Vec<LockTag> = table.held_by.get(&session).map(|s| s.iter().cloned().collect()).unwrap_or_default();
    let mut removed = 0usize;
    let mut touched: Vec<LockTag> = Vec::new();
    for tag in &tags {
        let n = match table.entries.get_mut(tag) {
            Some(e) => {
                let before = e.granted.len();
                e.granted.retain(|g| !(g.session == session && pred(tag, g)));
                before - e.granted.len()
            }
            None => 0,
        };
        removed += n;
        if n > 0 {
            touched.push(tag.clone());
        }
        forget_held_if_none(table, tag, session);
    }
    let mut first_err = None;
    for tag in &touched {
        if let Err(e) = grant_waiters(table, tag) {
            first_err.get_or_insert(e);
        }
    }
    match first_err {
        Some(e) => Err(e),
        None => Ok(removed),
    }
}

/// Remove the waiter owning `slot` from `tag`'s queue (no-op when absent).
fn remove_waiter(table: &mut LockTable, tag: &LockTag, slot: &Arc<WaitSlot>) {
    if let Some(e) = table.entries.get_mut(tag) {
        e.waiters.retain(|w| !Arc::ptr_eq(&w.slot, slot));
    }
}

fn tag_info(tag: &LockTag, locktype: &'static str) -> LockTagInfo {
    let (relation, classid, objid, objsubid) = match tag {
        LockTag::Relation { relid, .. } => (Some(*relid), None, None, None),
        LockTag::Key { .. } => (tag.key_relation(), None, None, None),
        LockTag::Advisory { key1, key2, field4, .. } => (None, Some(*key1), Some(*key2), Some(i32::from(*field4))),
        LockTag::Object { classid, objid, objsubid, .. } => (None, Some(*classid), Some(*objid), Some(*objsubid)),
    };
    LockTagInfo { locktype, database: tag.database(), relation, classid, objid, objsubid }
}

enum Step {
    Granted,
    Fail(PgError),
    Deadlock,
    Sleep(Duration),
}

enum DeadlockOutcome {
    Clear,
    Granted,
    Victim(PgError),
}

impl LockManager {
    pub fn new() -> LockManager {
        LockManager::default()
    }

    fn lock(&self) -> MutexGuard<'_, LockTable> {
        self.table.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Acquire `mode` on `tag` for `session` with `scope` (module docs).
    pub fn acquire(
        &self,
        session: &SessionShared,
        scope: LockScope,
        tag: &LockTag,
        mode: LockMode,
        policy: WaitPolicy,
        limits: &WaitLimits,
    ) -> Result<AcquireResult> {
        check_mode(tag, mode)?;
        let sid = session.id;
        let slot;
        let event;
        {
            let mut guard = self.lock();
            let t: &mut LockTable = &mut guard;
            // Re-entrant request for an identical grant.
            if let Some(entry) = t.entries.get_mut(tag) {
                if let Some(g) =
                    entry.granted.iter_mut().find(|g| g.session == sid && g.scope == scope && g.mode == mode)
                {
                    g.count = g.count.saturating_add(1);
                    return Ok(AcquireResult::Acquired { newly: false });
                }
            }
            let ok = match t.entries.get(tag) {
                Some(e) => grantable(&e.granted, e.waiters.iter(), sid, mode)?,
                None => true,
            };
            if ok {
                let newly = add_grant(t, tag, sid, scope, mode);
                return Ok(AcquireResult::Acquired { newly });
            }
            if policy != WaitPolicy::Block {
                return Ok(AcquireResult::NotAvailable);
            }
            event = tag.wait_event()?;
            slot = WaitSlot::new();
            let waiter = Waiter {
                session: sid,
                scope,
                mode,
                slot: Arc::clone(&slot),
                since: Instant::now(),
                since_unix_ms: unix_ms(),
            };
            let entry: &mut LockEntry = t.entries.entry(tag.clone()).or_default();
            entry.waiters.push_back(waiter);
            t.waiting_on.insert(sid, tag.clone());
            if let LockScope::Transaction { txn, .. } = scope {
                t.session_txn.insert(sid, txn);
            }
        }
        session.update_activity(|a| a.wait = Some(WaitEvent { event_type: "Lock", event }));
        let res = self.wait_for_grant(session, scope, tag, mode, limits, &slot);
        session.update_activity(|a| a.wait = None);
        res
    }

    fn wait_for_grant(
        &self,
        session: &SessionShared,
        scope: LockScope,
        tag: &LockTag,
        mode: LockMode,
        limits: &WaitLimits,
        slot: &Arc<WaitSlot>,
    ) -> Result<AcquireResult> {
        let sid = session.id;
        let start = Instant::now();
        let deadlock_at = start + Duration::from_millis(limits.deadlock_timeout_ms);
        let lock_deadline = (limits.lock_timeout_ms > 0).then(|| start + Duration::from_millis(limits.lock_timeout_ms));
        let mut deadlock_checked = false;
        loop {
            let guard = lock_slot(slot);
            // Granted is observed BEFORE any timeout / interrupt evaluation.
            let step = if *guard == WaitStatus::Granted {
                Step::Granted
            } else if let Err(e) = session.interrupts.check() {
                Step::Fail(e)
            } else {
                let now = Instant::now();
                if lock_deadline.is_some_and(|d| now >= d) {
                    Step::Fail(error::lock_timeout())
                } else if limits.statement_deadline.is_some_and(|d| now >= d) {
                    Step::Fail(error::statement_timeout())
                } else if !deadlock_checked && now >= deadlock_at {
                    Step::Deadlock
                } else {
                    let mut wait = POLL;
                    let mut bound = |d: Option<Instant>| {
                        if let Some(d) = d {
                            wait = wait.min(d.saturating_duration_since(now));
                        }
                    };
                    if !deadlock_checked {
                        bound(Some(deadlock_at));
                    }
                    bound(lock_deadline);
                    bound(limits.statement_deadline);
                    bound(session.interrupts.deadline());
                    Step::Sleep(wait.max(Duration::from_millis(1)))
                }
            };
            match step {
                Step::Granted => {
                    drop(guard);
                    return Ok(AcquireResult::Acquired { newly: true });
                }
                Step::Fail(e) => {
                    drop(guard);
                    let _ = self.cancel_wait(sid, scope, tag, mode, slot);
                    return Err(e);
                }
                Step::Deadlock => {
                    drop(guard);
                    deadlock_checked = true;
                    match self.deadlock_check(sid, tag, slot) {
                        Ok(DeadlockOutcome::Clear) => {}
                        Ok(DeadlockOutcome::Granted) => return Ok(AcquireResult::Acquired { newly: true }),
                        Ok(DeadlockOutcome::Victim(e)) => return Err(e),
                        Err(e) => {
                            let _ = self.cancel_wait(sid, scope, tag, mode, slot);
                            return Err(e);
                        }
                    }
                }
                Step::Sleep(d) => {
                    // The slot lock is held from the interrupt check until the condvar releases it,
                    // so a `wake` (which takes the slot lock before notifying) cannot be lost.
                    let (g, _) = slot.cv.wait_timeout(guard, d).unwrap_or_else(|e| e.into_inner());
                    drop(g);
                }
            }
        }
    }

    /// Run the deadlock check for `sid`; when it is the victim, its waiter is already removed.
    fn deadlock_check(&self, sid: SessionId, tag: &LockTag, slot: &Arc<WaitSlot>) -> Result<DeadlockOutcome> {
        let mut guard = self.lock();
        let t: &mut LockTable = &mut guard;
        if *lock_slot(slot) == WaitStatus::Granted {
            return Ok(DeadlockOutcome::Granted);
        }
        let edges = deadlock::wait_for_edges(t)?;
        match deadlock::find_cycle(&edges, sid)? {
            None => Ok(DeadlockOutcome::Clear),
            Some(cycle) => {
                let err = deadlock::deadlock_error(&cycle)?;
                t.waiting_on.remove(&sid);
                remove_waiter(t, tag, slot);
                let _ = grant_waiters(t, tag);
                Ok(DeadlockOutcome::Victim(err))
            }
        }
    }

    /// Abandon a wait: a grant that raced in is released again, otherwise the waiter is dequeued.
    fn cancel_wait(
        &self,
        sid: SessionId,
        scope: LockScope,
        tag: &LockTag,
        mode: LockMode,
        slot: &Arc<WaitSlot>,
    ) -> Result<()> {
        let mut guard = self.lock();
        let t: &mut LockTable = &mut guard;
        t.waiting_on.remove(&sid);
        let granted = *lock_slot(slot) == WaitStatus::Granted;
        if granted {
            release_one_in(t, sid, scope, tag, mode);
        } else {
            remove_waiter(t, tag, slot);
        }
        grant_waiters(t, tag)
    }

    /// Release ONE count of (session, scope, mode) on `tag` (advisory unlock). Returns false when
    /// no such grant exists. Count reaching 0 removes the grant and wakes waiters.
    pub fn release_one(&self, session: SessionId, scope: LockScope, tag: &LockTag, mode: LockMode) -> Result<bool> {
        let mut guard = self.lock();
        let t: &mut LockTable = &mut guard;
        if !release_one_in(t, session, scope, tag, mode) {
            return Ok(false);
        }
        grant_waiters(t, tag)?;
        Ok(true)
    }

    /// Release every Transaction-scope grant of `txn` (commit/abort). Also drops
    /// `session_txn[session]` when it equals `txn`.
    pub fn release_txn(&self, session: SessionId, txn: TxnId) -> Result<()> {
        let mut guard = self.lock();
        let t: &mut LockTable = &mut guard;
        if t.session_txn.get(&session) == Some(&txn) {
            t.session_txn.remove(&session);
        }
        release_matching(t, session, &|_, g| matches!(g.scope, LockScope::Transaction { txn: x, .. } if x == txn))?;
        Ok(())
    }

    /// ROLLBACK TO SAVEPOINT: release Transaction-scope grants of `txn` whose `subxact >= from`.
    pub fn release_subxacts(&self, session: SessionId, txn: TxnId, from: SubXactId) -> Result<()> {
        let mut guard = self.lock();
        let t: &mut LockTable = &mut guard;
        release_matching(t, session, &|_, g| {
            matches!(g.scope, LockScope::Transaction { txn: x, subxact } if x == txn && subxact >= from)
        })?;
        Ok(())
    }

    /// RELEASE SAVEPOINT: re-tag grants of `txn` with `subxact >= from` as `into` (the parent's
    /// subxact id) so a later ROLLBACK TO an outer savepoint still releases them. Merge counts when
    /// an equal (session, scope, mode) grant already exists.
    pub fn reassign_subxacts(&self, session: SessionId, txn: TxnId, from: SubXactId, into: SubXactId) -> Result<()> {
        let mut guard = self.lock();
        let t: &mut LockTable = &mut guard;
        let tags: Vec<LockTag> = t.held_by.get(&session).map(|s| s.iter().cloned().collect()).unwrap_or_default();
        for tag in &tags {
            let Some(e) = t.entries.get_mut(tag) else { continue };
            let mut changed = false;
            for g in e.granted.iter_mut() {
                if g.session != session {
                    continue;
                }
                if let LockScope::Transaction { txn: x, subxact } = g.scope {
                    if x == txn && subxact >= from && subxact != into {
                        g.scope = LockScope::Transaction { txn, subxact: into };
                        changed = true;
                    }
                }
            }
            if !changed {
                continue;
            }
            let old = std::mem::take(&mut e.granted);
            let mut merged: Vec<Grant> = Vec::with_capacity(old.len());
            for g in old {
                if let Some(m) =
                    merged.iter_mut().find(|m| m.session == g.session && m.scope == g.scope && m.mode == g.mode)
                {
                    m.count = m.count.saturating_add(g.count);
                    m.granted_unix_ms = m.granted_unix_ms.min(g.granted_unix_ms);
                } else {
                    merged.push(g);
                }
            }
            e.granted = merged;
        }
        Ok(())
    }

    /// Release every Session-scope grant of `session` (pg_advisory_unlock_all, disconnect), and
    /// remove any queued waiter of that session (defensive). Returns how many grants were removed.
    pub fn release_session(&self, session: SessionId) -> Result<usize> {
        let mut guard = self.lock();
        let t: &mut LockTable = &mut guard;
        let mut dequeued: Option<LockTag> = None;
        if let Some(tag) = t.waiting_on.remove(&session) {
            if let Some(e) = t.entries.get_mut(&tag) {
                for w in e.waiters.iter().filter(|w| w.session == session) {
                    w.slot.cv.notify_all();
                }
                e.waiters.retain(|w| w.session != session);
            }
            dequeued = Some(tag);
        }
        let n = release_matching(t, session, &|_, g| g.scope == LockScope::Session)?;
        if let Some(tag) = dequeued {
            grant_waiters(t, &tag)?;
        }
        Ok(n)
    }

    /// Release Session-scope grants matching `filter` (used by `pg_advisory_unlock_all`, which only
    /// drops advisory locks).
    pub fn release_session_where(&self, session: SessionId, filter: &dyn Fn(&LockTag) -> bool) -> Result<usize> {
        let mut guard = self.lock();
        let t: &mut LockTable = &mut guard;
        release_matching(t, session, &|tag, g| g.scope == LockScope::Session && filter(tag))
    }

    /// Wake `session`'s waiter (if any) without granting, so it re-checks its interrupts promptly
    /// (called after cancel/terminate requests).
    pub fn wake(&self, session: SessionId) -> Result<()> {
        let slots: Vec<Arc<WaitSlot>> = {
            let t = self.lock();
            match t.waiting_on.get(&session).and_then(|tag| t.entries.get(tag)) {
                Some(e) => e.waiters.iter().filter(|w| w.session == session).map(|w| Arc::clone(&w.slot)).collect(),
                None => Vec::new(),
            }
        };
        for slot in slots {
            // Taking the status lock orders this notify after a waiter's interrupt check.
            drop(lock_slot(&slot));
            slot.cv.notify_all();
        }
        Ok(())
    }

    /// Whether `session` holds `mode` (any scope) on `tag`.
    pub fn holds(&self, session: SessionId, tag: &LockTag, mode: LockMode) -> Result<bool> {
        let t = self.lock();
        Ok(t.entries.get(tag).is_some_and(|e| e.granted.iter().any(|g| g.session == session && g.mode == mode)))
    }

    /// Every grant and every waiter as `LockInfo` (pg_locks), ordered by (session, granted desc,
    /// locktype, tag). One consistent copy under the table mutex.
    pub fn snapshot(&self) -> Result<Vec<LockInfo>> {
        let mut rows: Vec<(LockTag, LockInfo)> = Vec::new();
        {
            let t = self.lock();
            for (tag, e) in &t.entries {
                let locktype = tag.locktype()?;
                for g in &e.granted {
                    rows.push((
                        tag.clone(),
                        LockInfo {
                            tag: tag_info(tag, locktype),
                            session: g.session,
                            mode: g.mode,
                            granted: true,
                            scope: g.scope,
                            wait_start_unix_ms: None,
                        },
                    ));
                }
                for w in &e.waiters {
                    rows.push((
                        tag.clone(),
                        LockInfo {
                            tag: tag_info(tag, locktype),
                            session: w.session,
                            mode: w.mode,
                            granted: false,
                            scope: w.scope,
                            wait_start_unix_ms: Some(w.since_unix_ms),
                        },
                    ));
                }
            }
        }
        rows.sort_by(|a, b| {
            (a.1.session, std::cmp::Reverse(a.1.granted), a.1.tag.locktype, &a.0, a.1.mode).cmp(&(
                b.1.session,
                std::cmp::Reverse(b.1.granted),
                b.1.tag.locktype,
                &b.0,
                b.1.mode,
            ))
        });
        Ok(rows.into_iter().map(|(_, i)| i).collect())
    }

    /// `pg_blocking_pids(pid)`: sessions holding a conflicting grant on the tag `session` waits on,
    /// plus sessions queued AHEAD of it with a conflicting requested mode (PG includes both),
    /// deduplicated, ascending. Not waiting → empty.
    pub fn blocking_sessions(&self, session: SessionId) -> Result<Vec<SessionId>> {
        let t = self.lock();
        let Some(tag) = t.waiting_on.get(&session) else { return Ok(Vec::new()) };
        let Some(entry) = t.entries.get(tag) else { return Ok(Vec::new()) };
        let Some(pos) = entry.waiters.iter().position(|w| w.session == session) else { return Ok(Vec::new()) };
        let mode = entry.waiters[pos].mode;
        let mut out: BTreeSet<SessionId> = BTreeSet::new();
        for g in &entry.granted {
            if g.session != session && mode.conflicts_with(g.mode)? {
                out.insert(g.session);
            }
        }
        for w in entry.waiters.iter().take(pos) {
            if w.session != session && mode.conflicts_with(w.mode)? {
                out.insert(w.session);
            }
        }
        Ok(out.into_iter().collect())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pgarbor_common::BranchId;

    fn tag(n: u32) -> LockTag {
        LockTag::Relation { branch: BranchId([1; 16]), database: 5, relid: n }
    }

    fn txn_scope(t: u64, s: u32) -> LockScope {
        LockScope::Transaction { txn: TxnId(t), subxact: SubXactId(s) }
    }

    #[test]
    fn grant_count_and_release_one() {
        let mut t = LockTable::default();
        let tg = tag(16384);
        assert!(add_grant(&mut t, &tg, SessionId(1), LockScope::Session, LockMode::Share));
        assert!(!add_grant(&mut t, &tg, SessionId(1), LockScope::Session, LockMode::Share));
        assert!(release_one_in(&mut t, SessionId(1), LockScope::Session, &tg, LockMode::Share));
        assert!(t.held_by.contains_key(&SessionId(1)));
        assert!(release_one_in(&mut t, SessionId(1), LockScope::Session, &tg, LockMode::Share));
        assert!(!t.held_by.contains_key(&SessionId(1)));
        assert!(!release_one_in(&mut t, SessionId(1), LockScope::Session, &tg, LockMode::Share));
    }

    #[test]
    fn release_matching_filters_by_scope() {
        let mut t = LockTable::default();
        add_grant(&mut t, &tag(16384), SessionId(1), LockScope::Session, LockMode::Share);
        add_grant(&mut t, &tag(16385), SessionId(1), txn_scope(7, 0), LockMode::Share);
        let n = release_matching(&mut t, SessionId(1), &|_, g| g.scope == LockScope::Session).unwrap();
        assert_eq!(n, 1);
        assert!(t.entries.contains_key(&tag(16385)));
        assert!(!t.entries.contains_key(&tag(16384)));
    }

    #[test]
    fn grant_waiters_grants_and_wakes() {
        let mut t = LockTable::default();
        let tg = tag(16384);
        let slot = WaitSlot::new();
        t.entries.entry(tg.clone()).or_default().waiters.push_back(Waiter {
            session: SessionId(2),
            scope: LockScope::Session,
            mode: LockMode::Exclusive,
            slot: Arc::clone(&slot),
            since: Instant::now(),
            since_unix_ms: 0,
        });
        t.waiting_on.insert(SessionId(2), tg.clone());
        grant_waiters(&mut t, &tg).unwrap();
        assert_eq!(*lock_slot(&slot), WaitStatus::Granted);
        assert!(t.waiting_on.is_empty());
        assert_eq!(t.entries[&tg].granted.len(), 1);
    }
}
