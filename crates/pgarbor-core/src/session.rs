//! Sessions ("backends"): registry, branch binding, cancel/terminate plumbing and activity state
//! for `pg_stat_activity` (TXN-01, TXN-12). Lease CORE-02 for the registry; `Interrupts`,
//! `Activity` and accessors are implemented.
//!
//! A session is created by `Engine::attach_session` and lives as long as its `SessionHandle`
//! (the pgarbor crate's `Session` owns one). It is bound to exactly one branch (by id — renames
//! are harmless) and one logical database for its lifetime. Dropping the handle unregisters the
//! session, releases its session-level advisory locks and wakes anything waiting on it.
use crate::engine::Engine;
use crate::error;
use crate::sequence::SessionSequences;
use crate::types::{SessionId, TxnState};
use pgarbor_common::{BranchId, Oid, PgError, Result, Severity, SqlState, TxnId};
use std::collections::hash_map::RandomState;
use std::collections::HashMap;
use std::hash::{BuildHasher, Hasher};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, RwLock};
use std::time::{Instant, SystemTime, UNIX_EPOCH};

// CONTRACT
/// Asynchronous interrupt flags of one session (PG `ProcessInterrupts`). Set from other threads
/// (wire CancelRequest, `pg_cancel_backend`, `pg_terminate_backend`, forced branch delete/reset),
/// polled by the session's own thread at safe points (`check`), including inside lock waits.
#[derive(Debug, Default)]
pub struct Interrupts {
    cancel: AtomicBool,
    terminate: AtomicBool,
    deadline: Mutex<Option<Instant>>,
}

impl Interrupts {
    pub fn new() -> Interrupts {
        Interrupts::default()
    }
    pub fn request_cancel(&self) {
        self.cancel.store(true, Ordering::SeqCst);
    }
    pub fn request_terminate(&self) {
        self.terminate.store(true, Ordering::SeqCst);
    }
    pub fn is_terminating(&self) -> bool {
        self.terminate.load(Ordering::SeqCst)
    }
    /// Start of each statement: clear a stale cancel (PG ignores a cancel that arrives while idle)
    /// and arm `statement_timeout` (`None` = no timeout).
    pub fn begin_statement(&self, deadline: Option<Instant>) {
        self.cancel.store(false, Ordering::SeqCst);
        *self.deadline.lock().unwrap_or_else(|e| e.into_inner()) = deadline;
    }
    /// End of statement: disarm the timeout.
    pub fn end_statement(&self) {
        *self.deadline.lock().unwrap_or_else(|e| e.into_inner()) = None;
    }
    pub fn deadline(&self) -> Option<Instant> {
        *self.deadline.lock().unwrap_or_else(|e| e.into_inner())
    }
    /// Priority: terminate → FATAL 57P01; cancel → 57014 `canceling statement due to user request`
    /// (and clears the flag, like PG's QueryCancelPending); deadline passed → 57014
    /// `canceling statement due to statement timeout` (disarms it).
    pub fn check(&self) -> Result<()> {
        if self.terminate.load(Ordering::SeqCst) {
            return Err(error::terminated_by_administrator());
        }
        if self.cancel.swap(false, Ordering::SeqCst) {
            return Err(error::query_canceled());
        }
        let mut d = self.deadline.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(t) = *d {
            if Instant::now() >= t {
                *d = None;
                return Err(error::statement_timeout());
            }
        }
        Ok(())
    }
}

// CONTRACT
/// `pg_stat_activity.state` values.
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize)]
pub enum BackendState {
    Idle,
    Active,
    IdleInTransaction,
    IdleInTransactionAborted,
}

impl BackendState {
    /// `idle`, `active`, `idle in transaction`, `idle in transaction (aborted)`.
    pub fn as_pg_str(self) -> &'static str {
        match self {
            BackendState::Idle => "idle",
            BackendState::Active => "active",
            BackendState::IdleInTransaction => "idle in transaction",
            BackendState::IdleInTransactionAborted => "idle in transaction (aborted)",
        }
    }
    /// State after a statement finished, given the transaction state.
    pub fn after_statement(txn: TxnState) -> BackendState {
        match txn {
            TxnState::Idle => BackendState::Idle,
            TxnState::InProgress => BackendState::IdleInTransaction,
            TxnState::Failed => BackendState::IdleInTransactionAborted,
        }
    }
}

// CONTRACT
/// `pg_stat_activity.wait_event_type` / `wait_event` while blocked on a lock: type `Lock`, event
/// `relation` | `tuple` | `transactionid` | `advisory` | `object` (key waits report
/// `transactionid`, as PG row-lock waits do).
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize)]
pub struct WaitEvent {
    pub event_type: &'static str,
    pub event: &'static str,
}

// CONTRACT
/// Mutable activity snapshot (all times unix ms).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Activity {
    pub state: BackendState,
    pub query: String,
    pub query_start_ms: Option<u64>,
    pub xact_start_ms: Option<u64>,
    pub state_change_ms: u64,
    /// Set once the current transaction has written (PG assigns an xid on first write).
    pub backend_xid: Option<TxnId>,
    /// Current transaction (for virtualtransaction / pg_locks), if any.
    pub txn: Option<TxnId>,
    pub wait: Option<WaitEvent>,
}

// CONTRACT
/// Shared per-session state (one `Arc` held by the registry, the handle and every `Txn`).
#[derive(Debug)]
pub struct SessionShared {
    pub id: SessionId,
    pub branch: BranchId,
    /// Branch name at attach time (refreshed by `Engine::rename_branch`; display only).
    pub branch_name: Mutex<String>,
    pub database_oid: Oid,
    pub database_name: String,
    pub user: String,
    pub application_name: Mutex<String>,
    pub client_addr: Option<String>,
    pub client_port: Option<i32>,
    pub backend_start_ms: u64,
    /// Secret for CancelRequest (length per `SessionRequest::cancel_key_len`).
    pub cancel_key: Vec<u8>,
    pub interrupts: Interrupts,
    pub activity: Mutex<Activity>,
    /// currval/lastval and CACHE>1 ranges (sequence/store.rs).
    pub sequences: Mutex<SessionSequences>,
}

impl SessionShared {
    pub fn activity(&self) -> Activity {
        self.activity.lock().unwrap_or_else(|e| e.into_inner()).clone()
    }
    /// Update activity in place.
    pub fn update_activity(&self, f: impl FnOnce(&mut Activity)) {
        f(&mut self.activity.lock().unwrap_or_else(|e| e.into_inner()))
    }
    pub fn branch_name(&self) -> String {
        self.branch_name.lock().unwrap_or_else(|e| e.into_inner()).clone()
    }
}

// CONTRACT
/// All live sessions of an engine.
#[derive(Debug, Default)]
pub struct SessionRegistry {
    pub(crate) sessions: RwLock<HashMap<SessionId, Arc<SessionShared>>>,
    /// Next pid candidate.
    pub(crate) next_pid: Mutex<i32>,
}

impl SessionRegistry {
    pub fn new() -> SessionRegistry {
        SessionRegistry { sessions: RwLock::new(HashMap::new()), next_pid: Mutex::new(FIRST_PID) }
    }

    /// Register a session bound to `branch` (already resolved by the engine).
    /// - more than `max_sessions` live sessions → FATAL 53300 `sorry, too many clients already`;
    /// - pid: take `next_pid`, skip pids in use, wrap from `i32::MAX` back to `FIRST_PID`;
    /// - cancel key: `key_len` bytes (clamped to 4..=256) from `random_bytes`;
    /// - activity: state Idle, empty query, `state_change_ms = backend_start_ms = now`.
    #[allow(clippy::too_many_arguments)]
    pub fn register(
        &self,
        branch: BranchId,
        branch_name: &str,
        req: &crate::options::SessionRequest,
        max_sessions: usize,
        now_ms: u64,
    ) -> Result<Arc<SessionShared>> {
        let mut sessions = self.sessions.write().unwrap_or_else(|e| e.into_inner());
        if sessions.len() >= max_sessions {
            return Err(PgError::new(SqlState::TOO_MANY_CONNECTIONS, "sorry, too many clients already")
                .with_severity(Severity::Fatal));
        }
        let id = {
            let mut next = self.next_pid.lock().unwrap_or_else(|e| e.into_inner());
            let mut candidate = (*next).max(FIRST_PID);
            // Ids in use are bounded by max_sessions, so a free pid is found well before a full
            // lap; the bound only guards against a corrupted `next_pid`.
            let lap = (i32::MAX as i64 - FIRST_PID as i64 + 1) as u64;
            let mut attempts: u64 = 0;
            let found = loop {
                if attempts >= lap {
                    return Err(PgError::internal("no free backend pid"));
                }
                attempts += 1;
                let pid = SessionId(candidate);
                candidate = if candidate == i32::MAX { FIRST_PID } else { candidate + 1 };
                if !sessions.contains_key(&pid) {
                    break pid;
                }
            };
            *next = candidate;
            found
        };
        let cancel_key = random_bytes(req.cancel_key_len.clamp(4, 256))?;
        let shared = Arc::new(SessionShared {
            id,
            branch,
            branch_name: Mutex::new(branch_name.to_string()),
            database_oid: req.database_oid,
            database_name: req.database_name.clone(),
            user: req.user.clone(),
            application_name: Mutex::new(req.application_name.clone()),
            client_addr: req.client_addr.clone(),
            client_port: req.client_port,
            backend_start_ms: now_ms,
            cancel_key,
            interrupts: Interrupts::new(),
            activity: Mutex::new(Activity {
                state: BackendState::Idle,
                query: String::new(),
                query_start_ms: None,
                xact_start_ms: None,
                state_change_ms: now_ms,
                backend_xid: None,
                txn: None,
                wait: None,
            }),
            sequences: Mutex::new(SessionSequences::default()),
        });
        sessions.insert(id, Arc::clone(&shared));
        Ok(shared)
    }

    /// Remove a session; returns whether it was registered.
    pub fn unregister(&self, id: SessionId) -> Result<bool> {
        let mut sessions = self.sessions.write().unwrap_or_else(|e| e.into_inner());
        Ok(sessions.remove(&id).is_some())
    }

    pub fn get(&self, id: SessionId) -> Option<Arc<SessionShared>> {
        self.sessions.read().unwrap_or_else(|e| e.into_inner()).get(&id).cloned()
    }

    /// All sessions, sorted by pid.
    pub fn all(&self) -> Vec<Arc<SessionShared>> {
        let mut v: Vec<_> = self.sessions.read().unwrap_or_else(|e| e.into_inner()).values().cloned().collect();
        v.sort_by_key(|s| s.id);
        v
    }

    /// Sessions bound to `branch`, excluding `except` (the caller's own session). Includes
    /// terminating sessions; callers counting "in use" filter `!interrupts.is_terminating()`.
    pub fn on_branch(&self, branch: &BranchId, except: Option<SessionId>) -> Vec<Arc<SessionShared>> {
        self.all().into_iter().filter(|s| s.branch == *branch && Some(s.id) != except).collect()
    }

    /// Wire CancelRequest: if a session with `pid` exists AND `secret` equals its cancel key
    /// (constant-time comparison: fold XOR over all bytes, lengths must match), request cancel and
    /// return true. Wrong key or unknown pid → false, silently (PG logs only).
    pub fn cancel_with_key(&self, pid: i32, secret: &[u8]) -> Result<bool> {
        let sessions = self.sessions.read().unwrap_or_else(|e| e.into_inner());
        let Some(session) = sessions.get(&SessionId(pid)) else {
            return Ok(false);
        };
        if !constant_time_eq(&session.cancel_key, secret) {
            return Ok(false);
        }
        session.interrupts.request_cancel();
        Ok(true)
    }
}

/// Equal lengths and no differing byte; the scan never exits early on a mismatch.
fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    a.iter().zip(b.iter()).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

/// First backend pid handed out.
pub const FIRST_PID: i32 = 1000;

/// `n` unpredictable bytes without an RNG dependency: repeatedly hash (pid, counter, nanos since
/// UNIX_EPOCH, a static AtomicU64 counter) with `std::collections::hash_map::RandomState::new()
/// .build_hasher()` (randomly keyed SipHash) and append the 8-byte outputs.
pub fn random_bytes(n: usize) -> Result<Vec<u8>> {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let pid = std::process::id();
    let nanos = SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_nanos()).unwrap_or(0);
    let mut out = Vec::with_capacity(n + 8);
    let mut chunk: u64 = 0;
    while out.len() < n {
        let counter = COUNTER.fetch_add(1, Ordering::Relaxed);
        let mut hasher = RandomState::new().build_hasher();
        hasher.write_u32(pid);
        hasher.write_u64(chunk);
        hasher.write_u128(nanos);
        hasher.write_u64(counter);
        out.extend_from_slice(&hasher.finish().to_le_bytes());
        chunk += 1;
    }
    out.truncate(n);
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::options::SessionRequest;

    fn branch() -> BranchId {
        BranchId([7u8; 16])
    }

    fn req() -> SessionRequest {
        SessionRequest::embedded("main")
    }

    #[test]
    fn pids_unique_start_at_1000_and_wrap() {
        let reg = SessionRegistry::new();
        let a = reg.register(branch(), "main", &req(), 100, 1).unwrap();
        let b = reg.register(branch(), "main", &req(), 100, 1).unwrap();
        let c = reg.register(branch(), "main", &req(), 100, 1).unwrap();
        assert_eq!(a.id, SessionId(FIRST_PID));
        assert_eq!(b.id, SessionId(FIRST_PID + 1));
        assert_eq!(c.id, SessionId(FIRST_PID + 2));
        // A freed pid is not reused immediately: allocation continues from next_pid.
        assert!(reg.unregister(a.id).unwrap());
        let d = reg.register(branch(), "main", &req(), 100, 1).unwrap();
        assert_eq!(d.id, SessionId(FIRST_PID + 3));
        // Wrap: i32::MAX is handed out, then the next allocation returns to FIRST_PID.
        *reg.next_pid.lock().unwrap() = i32::MAX;
        let e = reg.register(branch(), "main", &req(), 100, 1).unwrap();
        let f = reg.register(branch(), "main", &req(), 100, 1).unwrap();
        assert_eq!(e.id, SessionId(i32::MAX));
        assert_eq!(f.id, SessionId(FIRST_PID));
        // Live ids are never duplicated.
        let ids: Vec<SessionId> = reg.all().iter().map(|s| s.id).collect();
        let mut dedup = ids.clone();
        dedup.sort();
        dedup.dedup();
        assert_eq!(ids.len(), dedup.len());
    }

    #[test]
    fn pid_skips_in_use() {
        let reg = SessionRegistry::new();
        let _a = reg.register(branch(), "main", &req(), 100, 1).unwrap();
        *reg.next_pid.lock().unwrap() = FIRST_PID;
        let b = reg.register(branch(), "main", &req(), 100, 1).unwrap();
        assert_eq!(b.id, SessionId(FIRST_PID + 1));
    }

    #[test]
    fn max_sessions_enforced_fatal_53300() {
        let reg = SessionRegistry::new();
        let _a = reg.register(branch(), "main", &req(), 2, 1).unwrap();
        let _b = reg.register(branch(), "main", &req(), 2, 1).unwrap();
        match reg.register(branch(), "main", &req(), 2, 1) {
            Ok(_) => panic!("third session must be refused"),
            Err(e) => {
                assert!(e.sqlstate == SqlState::TOO_MANY_CONNECTIONS);
                assert_eq!(e.severity, Severity::Fatal);
                assert_eq!(e.message, "sorry, too many clients already");
            }
        }
        assert_eq!(reg.all().len(), 2);
    }

    #[test]
    fn activity_initialized_idle() {
        let reg = SessionRegistry::new();
        let s = reg.register(branch(), "main", &req(), 10, 42).unwrap();
        let act = s.activity();
        assert_eq!(act.state, BackendState::Idle);
        assert!(act.query.is_empty());
        assert_eq!(act.state_change_ms, 42);
        assert_eq!(s.backend_start_ms, 42);
        assert_eq!(s.cancel_key.len(), 4);
    }

    #[test]
    fn cancel_requires_exact_key() {
        let reg = SessionRegistry::new();
        let s = reg.register(branch(), "main", &req(), 10, 1).unwrap();
        let pid = s.id.0;
        let key = s.cancel_key.clone();
        let mut wrong = key.clone();
        wrong[0] ^= 0xff;
        // Wrong key: false, and nothing is set.
        assert!(!reg.cancel_with_key(pid, &wrong).unwrap());
        assert!(!reg.cancel_with_key(pid, &key[..key.len() - 1]).unwrap());
        assert!(!reg.cancel_with_key(pid + 1000, &key).unwrap());
        assert!(s.interrupts.check().is_ok());
        // Right key: cancel fires exactly once.
        assert!(reg.cancel_with_key(pid, &key).unwrap());
        match s.interrupts.check() {
            Ok(()) => panic!("cancel must fire"),
            Err(e) => assert!(e.sqlstate == SqlState::QUERY_CANCELED),
        }
        assert!(s.interrupts.check().is_ok());
    }

    #[test]
    fn random_bytes_length_and_variation() {
        let a = random_bytes(32).unwrap();
        let b = random_bytes(32).unwrap();
        assert_eq!(a.len(), 32);
        assert_eq!(b.len(), 32);
        assert_ne!(a, b);
        assert!(random_bytes(0).unwrap().is_empty());
        assert_eq!(random_bytes(13).unwrap().len(), 13);
    }

    #[test]
    fn constant_time_eq_behaves() {
        assert!(constant_time_eq(b"abcd", b"abcd"));
        assert!(!constant_time_eq(b"abcd", b"abce"));
        assert!(!constant_time_eq(b"abc", b"abcd"));
    }
}

// CONTRACT
/// Owning handle of one session. Not `Clone`. Drop = disconnect: (1) roll back is the caller's job
/// (the session's `Option<Txn>` is dropped first, which releases transaction locks), (2)
/// `engine.locks().release_session(id)` (session-level advisory locks), (3)
/// `engine.sessions().unregister(id)`. Errors during drop are ignored.
#[derive(Debug)]
pub struct SessionHandle {
    pub(crate) engine: Arc<Engine>,
    pub(crate) shared: Arc<SessionShared>,
}

impl SessionHandle {
    pub fn id(&self) -> SessionId {
        self.shared.id
    }
    pub fn shared(&self) -> &Arc<SessionShared> {
        &self.shared
    }
    pub fn engine(&self) -> &Arc<Engine> {
        &self.engine
    }
    pub fn branch(&self) -> BranchId {
        self.shared.branch
    }
    pub fn interrupts(&self) -> &Interrupts {
        &self.shared.interrupts
    }
}

impl Drop for SessionHandle {
    fn drop(&mut self) {
        let _ = self.engine.locks().release_session(self.shared.id);
        let _ = self.engine.sessions().unregister(self.shared.id);
    }
}
