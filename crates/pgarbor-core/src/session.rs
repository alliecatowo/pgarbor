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
use pgarbor_common::{BranchId, Oid, PgError, Result, TxnId};
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, RwLock};
use std::time::Instant;

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
    #[allow(unused_variables, clippy::too_many_arguments)]
    pub fn register(
        &self,
        branch: BranchId,
        branch_name: &str,
        req: &crate::options::SessionRequest,
        max_sessions: usize,
        now_ms: u64,
    ) -> Result<Arc<SessionShared>> {
        Err(PgError::unimplemented("CORE-02", "session registration"))
    }

    /// Remove a session; returns whether it was registered.
    #[allow(unused_variables)]
    pub fn unregister(&self, id: SessionId) -> Result<bool> {
        Err(PgError::unimplemented("CORE-02", "session unregistration"))
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
    #[allow(unused_variables)]
    pub fn cancel_with_key(&self, pid: i32, secret: &[u8]) -> Result<bool> {
        Err(PgError::unimplemented("CORE-02", "cancel request matching"))
    }
}

/// First backend pid handed out.
pub const FIRST_PID: i32 = 1000;

/// `n` unpredictable bytes without an RNG dependency: repeatedly hash (pid, counter, nanos since
/// UNIX_EPOCH, a static AtomicU64 counter) with `std::collections::hash_map::RandomState::new()
/// .build_hasher()` (randomly keyed SipHash) and append the 8-byte outputs.
#[allow(unused_variables)]
pub fn random_bytes(n: usize) -> Result<Vec<u8>> {
    Err(PgError::unimplemented("CORE-02", "cancel key generation"))
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
