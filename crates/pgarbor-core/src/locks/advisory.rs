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
use crate::error;
use crate::locks::mode::LockMode;
use crate::locks::table::LockScope;
use crate::locks::tag::LockTag;
use crate::txn::Txn;
use crate::types::{AcquireResult, WaitPolicy};
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
pub fn lock(txn: &Txn, key: AdvisoryKey, mode: AdvisoryMode, scope: AdvisoryScope, wait: bool) -> Result<bool> {
    let tag = advisory_tag(txn, key);
    let lock_scope = match scope {
        AdvisoryScope::Session => LockScope::Session,
        AdvisoryScope::Transaction => LockScope::Transaction { txn: txn.id(), subxact: txn.current_subxact() },
    };
    let policy = if wait { WaitPolicy::Block } else { WaitPolicy::NoWait };
    let limits = txn.limits();
    let result = txn.engine().locks().acquire(txn.session(), lock_scope, &tag, lock_mode(mode), policy, &limits)?;
    Ok(matches!(result, AcquireResult::Acquired { .. }))
}

/// `pg_advisory_unlock[_shared]`: `release_one(session, LockScope::Session, tag, mode)`; false →
/// `UnlockResult{released: false, warning: Some(advisory_not_owned("ExclusiveLock"|"ShareLock"))}`.
pub fn unlock(txn: &Txn, key: AdvisoryKey, mode: AdvisoryMode) -> Result<UnlockResult> {
    let tag = advisory_tag(txn, key);
    let released =
        txn.engine().locks().release_one(txn.session().id, LockScope::Session, &tag, lock_mode(mode))?;
    if released {
        Ok(UnlockResult { released: true, warning: None })
    } else {
        Ok(UnlockResult { released: false, warning: Some(error::advisory_not_owned(mode_name(mode))) })
    }
}

/// `pg_advisory_unlock_all()`: release every Session-scope ADVISORY grant of the session
/// (`release_session_where(.., |t| matches!(t, LockTag::Advisory{..}))`); transaction-level
/// advisory locks are untouched (PG behavior).
pub fn unlock_all(txn: &Txn) -> Result<()> {
    let is_advisory = |t: &LockTag| matches!(t, LockTag::Advisory { .. });
    txn.engine().locks().release_session_where(txn.session().id, &is_advisory)?;
    Ok(())
}

/// Table-level mode used for the advisory tag (lockfuncs.c: EXCLUSIVE_LOCK / SHARE_LOCK).
fn lock_mode(mode: AdvisoryMode) -> LockMode {
    match mode {
        AdvisoryMode::Exclusive => LockMode::Exclusive,
        AdvisoryMode::Shared => LockMode::Share,
    }
}

/// Lock-mode name used in the `you don't own a lock of type ...` WARNING (lock.c GetLockmodeName).
fn mode_name(mode: AdvisoryMode) -> &'static str {
    match mode {
        AdvisoryMode::Exclusive => "ExclusiveLock",
        AdvisoryMode::Shared => "ShareLock",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn single_key_splits_into_halves() {
        assert_eq!(AdvisoryKey::Single(-1).tag_fields(), (u32::MAX, u32::MAX, 1));
        assert_eq!(AdvisoryKey::Single(0).tag_fields(), (0, 0, 1));
        assert_eq!(AdvisoryKey::Single((1i64 << 32) | 5).tag_fields(), (1, 5, 1));
        assert_eq!(AdvisoryKey::Single(i64::MIN).tag_fields(), (0x8000_0000, 0, 1));
        assert_eq!(AdvisoryKey::Single(i64::MAX).tag_fields(), (0x7fff_ffff, u32::MAX, 1));
    }

    #[test]
    fn pair_key_keeps_both_halves() {
        assert_eq!(AdvisoryKey::Pair(1, 2).tag_fields(), (1, 2, 2));
        assert_eq!(AdvisoryKey::Pair(-1, -1).tag_fields(), (u32::MAX, u32::MAX, 2));
        assert_eq!(AdvisoryKey::Pair(i32::MIN, 0).tag_fields(), (0x8000_0000, 0, 2));
    }

    #[test]
    fn modes_map_to_pg_lock_modes() {
        assert_eq!(lock_mode(AdvisoryMode::Exclusive), LockMode::Exclusive);
        assert_eq!(lock_mode(AdvisoryMode::Shared), LockMode::Share);
        assert_eq!(mode_name(AdvisoryMode::Exclusive), "ExclusiveLock");
        assert_eq!(mode_name(AdvisoryMode::Shared), "ShareLock");
    }

    #[test]
    fn warning_text_matches_pg() {
        let w = error::advisory_not_owned(mode_name(AdvisoryMode::Exclusive));
        assert!(format!("{w:?}").contains("you don't own a lock of type ExclusiveLock"));
    }
}
