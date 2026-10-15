//! Advisory locks: the `pg_advisory_*` / `pg_try_advisory_*` function family (functions-admin
//! "Advisory Lock Functions"; PG src/backend/utils/adt/lockfuncs.c). Lease CORE-11.
//!
//! | SQL function | call |
//! |---|---|
//! | `pg_advisory_lock(k)` / `(k1,k2)` | `lock(txn, key, Exclusive, Session, wait=true)` → void |
//! | `pg_advisory_lock_shared` | `lock(.., Shared, Session, true)` |
//! | `pg_advisory_xact_lock[_shared]` | `lock(.., Transaction, true)` |
//! | `pg_try_advisory_lock[_shared]`, `pg_try_advisory_xact_lock[_shared]` | `wait=false` → bool |
//! | `pg_advisory_unlock[_shared]` | `unlock(txn, key, mode)` → bool + optional WARNING |
//! | `pg_advisory_unlock_all()` | `unlock_all(txn)` |
//!
//! Semantics (oracle-verified on PG18.6):
//! - Session locks are re-entrant: locking twice needs two unlocks; each unlock returns `t` while a
//!   count remains, then `f` + WARNING `you don't own a lock of type ExclusiveLock`.
//! - `pg_advisory_unlock` only releases SESSION-level locks; on a key held only at transaction
//!   level it returns `f` with the same WARNING (oracle: `pg_advisory_xact_lock(3)` then
//!   `pg_advisory_unlock(3)` → f + WARNING).
//! - Shared and exclusive are distinct: `pg_advisory_unlock(7)` on a shared-held key → f + WARNING
//!   `... ExclusiveLock`; `pg_advisory_unlock_shared(7)` → t.
//! - Tag mapping (pg_locks): bigint `k` → key1 = (k >> 32) as u32, key2 = k as u32 (low 32 bits),
//!   field4 = 1; `(a int4, b int4)` → key1 = a as u32, key2 = b as u32, field4 = 2. Example:
//!   `pg_advisory_lock(-1)` shows classid 4294967295, objid 4294967295, objsubid 1.
//! - Transaction-level locks are released at commit/abort and by ROLLBACK TO a savepoint taken
//!   before them; session-level locks survive both (even when taken inside a rolled-back subxact).
//! - Waiting obeys lock_timeout / statement_timeout / cancel / deadlock detection like any lock.
//! - Every tag carries the session's branch and database.
use crate::locks::tag::LockTag;
use crate::txn::Txn;
use pgarbor_common::{PgError, Result};

// CONTRACT
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum AdvisoryKey {
    /// `pg_advisory_lock(bigint)`.
    Single(i64),
    /// `pg_advisory_lock(int4, int4)`.
    Pair(i32, i32),
}

impl AdvisoryKey {
    /// (key1, key2, field4) per the module docs.
    pub fn tag_fields(self) -> (u32, u32, u16) {
        match self {
            AdvisoryKey::Single(k) => (((k as u64) >> 32) as u32, k as u64 as u32, 1),
            AdvisoryKey::Pair(a, b) => (a as u32, b as u32, 2),
        }
    }
}

// CONTRACT
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AdvisoryMode {
    /// `LockMode::Exclusive` (`ExclusiveLock`).
    Exclusive,
    /// `LockMode::Share` (`ShareLock`).
    Shared,
}

// CONTRACT
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AdvisoryScope {
    Session,
    Transaction,
}

// CONTRACT
/// Result of `pg_advisory_unlock*`: the boolean to return and an optional WARNING notice to send
/// (`error::advisory_not_owned`), which the caller forwards as a NoticeResponse.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct UnlockResult {
    pub released: bool,
    pub warning: Option<PgError>,
}

/// Build the tag for `key` in the transaction's branch/database.
pub fn advisory_tag(txn: &Txn, key: AdvisoryKey) -> LockTag {
    let (key1, key2, field4) = key.tag_fields();
    LockTag::Advisory { branch: txn.branch(), database: txn.database(), key1, key2, field4 }
}

/// Acquire. `wait = true` → blocks (Block policy with the txn's `WaitLimits`) and returns true;
/// `wait = false` → `NoWait`, returns whether acquired. Scope Transaction uses the txn's current
/// subxact (`LockScope::Transaction{txn: txn.id(), subxact: txn.current_subxact()}`); Session uses
/// `LockScope::Session`. Uses `txn.engine().locks().acquire(txn.session(), ..)`.
#[allow(unused_variables)]
pub fn lock(txn: &Txn, key: AdvisoryKey, mode: AdvisoryMode, scope: AdvisoryScope, wait: bool) -> Result<bool> {
    Err(PgError::unimplemented("CORE-11", "advisory lock"))
}

/// `pg_advisory_unlock[_shared]`: `release_one(session, LockScope::Session, tag, mode)`; false →
/// `UnlockResult{released: false, warning: Some(advisory_not_owned("ExclusiveLock"|"ShareLock"))}`.
#[allow(unused_variables)]
pub fn unlock(txn: &Txn, key: AdvisoryKey, mode: AdvisoryMode) -> Result<UnlockResult> {
    Err(PgError::unimplemented("CORE-11", "advisory unlock"))
}

/// `pg_advisory_unlock_all()`: release every Session-scope ADVISORY grant of the session
/// (`release_session_where(.., |t| matches!(t, LockTag::Advisory{..}))`); transaction-level
/// advisory locks are untouched (PG behavior).
#[allow(unused_variables)]
pub fn unlock_all(txn: &Txn) -> Result<()> {
    Err(PgError::unimplemented("CORE-11", "advisory unlock all"))
}
