//! Engine-wide sequence state cache and the non-transactional "mini-commit" (TXN-13).
//! Lease CORE-13 (sonnet).
//!
//! # Where state lives
//! - Durable: key `seq_key(relid)` in the sequence's database tree of the branch head.
//! - In memory: `SequenceStore.entries[(branch, db, relid)]` = the visible `SequenceState`
//!   (what PG keeps in the shared buffer). Loaded on first use from the LATEST branch head
//!   (`engine.branch_head`), never from a transaction snapshot (sequences are non-transactional).
//!
//! # nextval(txn, relid, params)
//! 1. `txn.check_not_failed()`; read-only txn → `error::read_only_transaction("nextval()")`
//!    (oracle: 25006 `cannot execute nextval() in a read-only transaction`).
//! 2. Session CACHE range non-empty → pop front, record currval/lastval, return (no shared state).
//! 3. Created-in-this-transaction case: the txn's overlay holds `Some(state)` for `seq_key(relid)`
//!    and the latest head has no such key → advance that overlay state and write it back with
//!    `overlay.put_unlogged` (the sequence vanishes on rollback; nextval survives ROLLBACK TO).
//! 4. Else lock `entries` (one Mutex; mini-commits for different sequences serialize — acceptable
//!    in v1), load the entry if absent (`force_log = true` on first load in this process),
//!    `state::advance(..)`; if `persist` is Some → MINI-COMMIT before returning the value:
//!    `w = container.begin_write()`; read the branch record (`directory::load_record`) — deleted →
//!    `branch_deleted`; `new_root = w.apply(rec.database_root(db), {seq_key → persist.encode()})`;
//!    `ops::advance_head(.., rec.generation, db, new_root)` (the generation of the CURRENT record:
//!    sequence writes are valid on whatever the head is); `w.commit()`. Commit failure → error,
//!    in-memory state unchanged (no value handed out).
//! 5. Update the entry, push extras into the session CACHE range, record currval/lastval.
//!
//! # setval(txn, relid, params, value, is_called)
//! Read-only → 25006 `cannot execute setval() in a read-only transaction`. `validate_setval`;
//! persist immediately via mini-commit (PG WAL-logs every setval); clear this session's CACHE range
//! for the sequence; record currval/lastval (PG sets currval to the setval value).
//!
//! # Transactional DDL helpers (write into the txn overlay through `TxnKv`, NOT mini-commits)
//! `create(txn, relid, params)` → put `initial(start)`; `restart(txn, relid, value)` (ALTER SEQUENCE
//! RESTART) → put `initial(value)`; `drop(txn, relid)` → delete key. On COMMIT, `commit.rs` calls
//! `invalidate_keys` so cached entries reload from the new head.
//!
//! # Lock order (CONTRACT, deadlock-free)
//! `SequenceStore.entries` mutex BEFORE the container writer (`begin_write`). nextval/setval
//! mini-commits take `entries` then `begin_write`; `Engine::create_branch` takes
//! `lock_entries()` FIRST, then `begin_write`, then calls `flush_branch` with the guard. Never call
//! `begin_write` and then lock `entries`. `forget_branch`/`invalidate_keys` lock `entries` alone
//! and must not be called while holding a `WriteTxn` (commit.rs calls `invalidate_keys` after
//! `w.commit()` consumed the writer).
//!
//! # Branch operations
//! `flush_branch(entries, w, rec)`: before forking a branch, write every cached entry of `rec.id` as its
//! VISIBLE state (not the logged-ahead image) into `rec`'s database trees inside the caller's write
//! txn and drop those entries, so the child starts exactly at the parent's last value (RFC 001 §9
//! "fork copies state at the branch snapshot"). Next nextval on the parent reloads with
//! `force_log`, re-reserving ahead — never reissuing a value.
//! `forget_branch(id)`: drop entries after reset/delete. `flush_all(engine)`: clean shutdown writes
//! visible states of all branches in one commit (PG's shutdown checkpoint), so a clean reopen shows
//! `log_cnt` as before.
use crate::engine::Engine;
use crate::sequence::state::{SequenceParams, SequenceState};
use crate::txn::Txn;
use pgarbor_branches::BranchRecord;
use pgarbor_common::{BranchId, Oid, PgError, RelId, Result};
use pgarbor_storage::WriteTxn;
use std::collections::HashMap;
use std::sync::{Mutex, MutexGuard};

/// The cache map type guarded by `SequenceStore::entries`.
pub type SeqEntries = HashMap<SeqCacheKey, SeqEntry>;

// CONTRACT
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct SeqCacheKey {
    pub branch: BranchId,
    pub database: Oid,
    pub relid: RelId,
}

// CONTRACT
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SeqEntry {
    /// Visible state.
    pub state: SequenceState,
}

// CONTRACT
#[derive(Debug, Default)]
pub struct SequenceStore {
    pub(crate) entries: Mutex<SeqEntries>,
}

impl SequenceStore {
    pub fn new() -> SequenceStore {
        SequenceStore::default()
    }

    /// Lock the cache (first step of the lock order in the module docs).
    pub fn lock_entries(&self) -> MutexGuard<'_, SeqEntries> {
        self.entries.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// See module docs. `entries` is the guard from `lock_entries()`, taken BEFORE `w` was begun.
    #[allow(unused_variables)]
    pub fn flush_branch(&self, entries: &mut SeqEntries, w: &mut WriteTxn<'_>, rec: &mut BranchRecord) -> Result<usize> {
        Err(PgError::unimplemented("CORE-13", "sequence flush before branch fork"))
    }

    /// Drop all cached entries of a branch.
    pub fn forget_branch(&self, branch: &BranchId) {
        self.entries.lock().unwrap_or_else(|e| e.into_inner()).retain(|k, _| k.branch != *branch);
    }

    /// Drop cached entries whose `seq_key` appears in `keys` (committed DDL on sequences).
    pub fn invalidate_keys(&self, branch: &BranchId, database: Oid, keys: &[RelId]) {
        let mut m = self.entries.lock().unwrap_or_else(|e| e.into_inner());
        for relid in keys {
            m.remove(&SeqCacheKey { branch: *branch, database, relid: *relid });
        }
    }

    /// Clean-shutdown flush (module docs).
    #[allow(unused_variables)]
    pub fn flush_all(&self, engine: &Engine) -> Result<()> {
        Err(PgError::unimplemented("CORE-13", "sequence shutdown flush"))
    }
}

/// `nextval(regclass)`.
#[allow(unused_variables)]
pub fn nextval(txn: &mut Txn, relid: RelId, params: &SequenceParams) -> Result<i64> {
    Err(PgError::unimplemented("CORE-13", "nextval"))
}

/// `setval(regclass, bigint [, boolean])`; returns `value`.
#[allow(unused_variables)]
pub fn setval(txn: &mut Txn, relid: RelId, params: &SequenceParams, value: i64, is_called: bool) -> Result<i64> {
    Err(PgError::unimplemented("CORE-13", "setval"))
}

/// `currval(regclass)`: session memory only; absent → `state::currval_undefined(name)`.
#[allow(unused_variables)]
pub fn currval(txn: &Txn, relid: RelId, name: &str) -> Result<i64> {
    Err(PgError::unimplemented("CORE-13", "currval"))
}

/// `lastval()`: session memory; absent → `state::lastval_undefined()`.
#[allow(unused_variables)]
pub fn lastval(txn: &Txn) -> Result<i64> {
    Err(PgError::unimplemented("CORE-13", "lastval"))
}

/// Visible state for `SELECT * FROM seq`: overlay (created in txn) → cached entry → latest head;
/// missing everywhere → XX000 `sequence state for relation <oid> is missing`.
#[allow(unused_variables)]
pub fn read_state(txn: &Txn, relid: RelId) -> Result<SequenceState> {
    Err(PgError::unimplemented("CORE-13", "sequence state read"))
}

/// CREATE SEQUENCE: transactional initial state (module docs).
#[allow(unused_variables)]
pub fn create(txn: &mut Txn, relid: RelId, params: &SequenceParams) -> Result<()> {
    Err(PgError::unimplemented("CORE-13", "sequence create"))
}

/// ALTER SEQUENCE ... RESTART [WITH v]: transactional (module docs).
#[allow(unused_variables)]
pub fn restart(txn: &mut Txn, relid: RelId, value: i64) -> Result<()> {
    Err(PgError::unimplemented("CORE-13", "sequence restart"))
}

/// DROP SEQUENCE: transactional delete of the state key.
#[allow(unused_variables)]
pub fn drop(txn: &mut Txn, relid: RelId) -> Result<()> {
    Err(PgError::unimplemented("CORE-13", "sequence drop"))
}
