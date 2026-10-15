//! Lock table data structures shared by `manager` and `deadlock`. CONTRACT. Data only, fully
//! implemented (no lease). All access happens under `LockManager::table`'s mutex.
use crate::locks::mode::LockMode;
use crate::locks::tag::LockTag;
use crate::types::{SessionId, SubXactId};
use pgarbor_common::TxnId;
use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::{Arc, Condvar, Mutex};
use pgarbor_common::time::Instant;

// CONTRACT
/// Lifetime of a held lock.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, serde::Serialize)]
pub enum LockScope {
    /// Released when the transaction ends; locks with `subxact >= s` are released by ROLLBACK TO a
    /// savepoint whose subxact id is `s`.
    Transaction { txn: TxnId, subxact: SubXactId },
    /// Held until explicitly released or the session ends (advisory session locks).
    Session,
}

// CONTRACT
/// One granted (owner, mode) on a tag, with a re-entrancy count.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Grant {
    pub session: SessionId,
    pub scope: LockScope,
    pub mode: LockMode,
    pub count: u32,
    pub granted_unix_ms: u64,
}

// CONTRACT
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WaitStatus {
    Waiting,
    /// The releaser moved this request into `granted`; the waiter just returns.
    Granted,
}

// CONTRACT
/// Per-waiter rendezvous: the waiter sleeps on `cv` until `status` becomes `Granted` or a timeout /
/// interrupt makes it give up. Releasers set `Granted` and `notify_one`. `LockManager::wake`
/// notifies without changing status (interrupt delivery).
#[derive(Debug)]
pub struct WaitSlot {
    pub status: Mutex<WaitStatus>,
    pub cv: Condvar,
}

impl WaitSlot {
    pub fn new() -> Arc<WaitSlot> {
        Arc::new(WaitSlot { status: Mutex::new(WaitStatus::Waiting), cv: Condvar::new() })
    }
}

// CONTRACT
#[derive(Clone, Debug)]
pub struct Waiter {
    pub session: SessionId,
    pub scope: LockScope,
    pub mode: LockMode,
    pub slot: Arc<WaitSlot>,
    pub since: Instant,
    pub since_unix_ms: u64,
}

// CONTRACT
#[derive(Clone, Debug, Default)]
pub struct LockEntry {
    pub granted: Vec<Grant>,
    /// FIFO queue of blocked requests.
    pub waiters: VecDeque<Waiter>,
}

impl LockEntry {
    pub fn is_empty(&self) -> bool {
        self.granted.is_empty() && self.waiters.is_empty()
    }
}

// CONTRACT
#[derive(Debug, Default)]
pub struct LockTable {
    pub entries: HashMap<LockTag, LockEntry>,
    /// Tags on which each session holds at least one grant (for fast release).
    pub held_by: HashMap<SessionId, HashSet<LockTag>>,
    /// Tag each blocked session is waiting on (a session waits on at most one tag).
    pub waiting_on: HashMap<SessionId, LockTag>,
    /// Transaction id of each session's current transaction (deadlock report text
    /// `ShareLock on transaction <txn>`; pg_locks virtualtransaction/transactionid).
    pub session_txn: HashMap<SessionId, TxnId>,
}

// CONTRACT
/// One row of `pg_locks` (built by `LockManager::snapshot`, rendered by `diag`).
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize)]
pub struct LockInfo {
    pub tag: LockTagInfo,
    pub session: SessionId,
    pub mode: LockMode,
    pub granted: bool,
    pub scope: LockScope,
    /// Wait start (unix ms) for waiting requests.
    pub wait_start_unix_ms: Option<u64>,
}

// CONTRACT
/// Serializable copy of a tag's identity for diagnostics.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize)]
pub struct LockTagInfo {
    pub locktype: &'static str,
    pub database: u32,
    pub relation: Option<u32>,
    pub classid: Option<u32>,
    pub objid: Option<u32>,
    pub objsubid: Option<i32>,
}
