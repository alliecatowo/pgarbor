//! [`ExecTxn`]: the executor's view of the current transaction, and its adapter over
//! `pgarbor_core::TxnKv`. Fully implemented, frozen (v1).
//!
//! The executor never touches `pgarbor_core::Txn` directly (it only sees `&mut dyn ExecTxn`), so
//! unit tests can run the whole executor over [`crate::memtxn::MemTxn`] without an `Engine`, and
//! `pgarbor-core` internals can change behind this trait.
//!
//! ## What the trait guarantees (read before writing any DML code)
//! 1. **Reads see this transaction's own writes** (overlay over snapshot). Reads do NOT see rows
//!    committed by other transactions after the statement snapshot, even if a lock wait just
//!    finished: use [`ExecTxn::lock_and_read_latest`] to learn about those.
//! 2. **Plain `put`/`delete` take an exclusive key lock** (core `TxnKv` write protocol) but under READ
//!    COMMITTED they do NOT fail when somebody committed a change to that key meanwhile, and they
//!    silently overwrite it. Therefore:
//!    * unique-index inserts, ON CONFLICT arbitration, and UPDATE/DELETE of an existing row MUST first
//!      call [`ExecTxn::lock_and_read_latest`] and decide from ITS answer, never from `get` alone;
//!    * brand-new heap rows use [`ExecTxn::alloc_rowids`] + [`ExecTxn::put_fresh`] (no lock).
//! 3. **Sequences and OIDs are non-transactional** (core `alloc`/`sequence::store`).
//! 4. Every write method fails with 25P02 in a failed transaction and 25006 in a read-only one.
use std::ops::Range;

use pgarbor_common::{KvIter, KvRead, KvWrite, Oid, PgError, RelId, Result};
use pgarbor_core::locks::{AdvisoryKey, AdvisoryMode, AdvisoryScope, LockMode, UnlockResult};
use pgarbor_core::sequence::store as seqstore;
use pgarbor_core::sequence::{SequenceParams, SequenceState};
use pgarbor_core::{IsolationLevel, LockOutcome, TxnKv, WaitPolicy};

/// Answer of [`ExecTxn::lock_and_read_latest`].
// CONTRACT
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum LatestRead {
    /// The key is locked by this transaction now. `value` is the LATEST COMMITTED value merged with
    /// this transaction's own pending write (own write wins); `None` = the key does not exist.
    /// `changed` = the latest committed value differs from what this statement's snapshot sees (a
    /// concurrent transaction committed a change to this key): READ COMMITTED callers re-evaluate
    /// against `value` (EvalPlanQual), REPEATABLE READ / SERIALIZABLE never get `changed = true`
    /// (the call fails with 40001 `could not serialize access due to concurrent update`/`delete`).
    Locked { value: Option<Vec<u8>>, changed: bool },
    /// `WaitPolicy::NoWait` / `SkipLocked` and another transaction holds the lock.
    NotAvailable,
}

/// The transaction as the executor sees it (module docs). Object-safe; `KvWrite` is a supertrait so
/// `&mut dyn ExecTxn` can be passed wherever `&mut dyn KvWrite` is expected (catalog code).
// CONTRACT
pub trait ExecTxn: KvWrite {
    // ---- identity / state --------------------------------------------------------------------
    /// In-process transaction id (diagnostics, `txid_current()`, catalog stamps).
    fn txn_id(&self) -> u64;
    /// Isolation level of the transaction.
    fn isolation(&self) -> IsolationLevel;
    fn is_read_only(&self) -> bool;
    /// Poll cancel / terminate / statement timeout (57014 / FATAL 57P01). Call at least once per
    /// 1024 rows in every loop that can run long.
    fn check_interrupts(&self) -> Result<()>;
    /// 25006 `cannot execute <command> in a read-only transaction` when read-only.
    fn check_writable(&self, command: &str) -> Result<()>;

    // ---- heap rows ---------------------------------------------------------------------------
    /// `n` consecutive fresh row ids for table `relid` (core `alloc::next_rowids`; non-transactional,
    /// never reused, ids start at 1). `n == 0` gives an empty range.
    fn alloc_rowids(&mut self, relid: RelId, n: u64) -> Result<Range<u64>>;
    /// Insert a key whose uniqueness is guaranteed by construction (a heap row key built from a row
    /// id just returned by [`ExecTxn::alloc_rowids`], or a brand-new index entry of such a row's
    /// non-unique index): no key lock, no outcome check (core `TxnKv::put_fresh`).
    fn put_fresh(&mut self, key: &[u8], value: &[u8]) -> Result<()>;

    // ---- locking -----------------------------------------------------------------------------
    /// Lock `key` (exclusive row/key lock, `mode` = `ForUpdate` or `ForNoKeyUpdate` or
    /// `ForShare`/`ForKeyShare` for SELECT FOR ...) and return the latest committed value merged
    /// with own writes (see [`LatestRead`]). Waits for conflicting holders under `WaitPolicy::Block`
    /// subject to lock_timeout / deadlock detection / cancel.
    fn lock_and_read_latest(&mut self, key: &[u8], mode: LockMode, policy: WaitPolicy) -> Result<LatestRead>;
    /// Table-level lock (DML: `RowExclusive`/`AccessShare`; DDL: `AccessExclusive`; LOCK TABLE: any).
    /// Returns false only for `NoWait`/`SkipLocked` when the lock is not available.
    fn lock_relation(&mut self, relid: RelId, mode: LockMode, policy: WaitPolicy) -> Result<bool>;
    /// Take the per-branch catalog write lock (`pgarbor_catalog::ddl_lock_key()`), protocol step 1 of
    /// the catalog contract. If another transaction committed catalog changes after this statement's
    /// snapshot, fails with [`crate::error::catalog_retry`] (the session re-snapshots and re-binds
    /// under READ COMMITTED; under REPEATABLE READ / SERIALIZABLE that is the final 40001).
    fn lock_ddl(&mut self) -> Result<()>;
    /// `pg_advisory_*` family (core `locks::advisory`). `wait = false` = the `pg_try_*` forms.
    fn advisory_lock(&mut self, key: AdvisoryKey, mode: AdvisoryMode, scope: AdvisoryScope, wait: bool) -> Result<bool>;
    fn advisory_unlock(&mut self, key: AdvisoryKey, mode: AdvisoryMode) -> Result<UnlockResult>;
    fn advisory_unlock_all(&mut self) -> Result<()>;

    // ---- ids / sequences ---------------------------------------------------------------------
    /// Fresh OID for a catalog object (core `alloc::next_oid`, non-transactional). `seed` must be
    /// `pgarbor_catalog::persist::oid_seed`-like: smallest OID above every catalog OID.
    fn alloc_oid(&mut self, seed: &dyn Fn(&dyn KvRead) -> Result<Oid>) -> Result<Oid>;
    /// `nextval` (core `sequence::store::nextval`).
    fn seq_nextval(&mut self, relid: RelId, params: &SequenceParams) -> Result<i64>;
    /// `setval(seq, value, is_called)`; returns `value`.
    fn seq_setval(&mut self, relid: RelId, params: &SequenceParams, value: i64, is_called: bool) -> Result<i64>;
    /// `currval`: 55000 `currval of sequence "s" is not yet defined in this session`.
    fn seq_currval(&self, relid: RelId, name: &str) -> Result<i64>;
    /// `lastval`: 55000 `lastval is not yet defined in this session`.
    fn seq_lastval(&self) -> Result<i64>;
    /// Visible state for `SELECT * FROM seq` / `pg_sequences`.
    fn seq_state(&self, relid: RelId) -> Result<SequenceState>;
    /// CREATE SEQUENCE: transactional initial state `(start, 0, false)`.
    fn seq_create(&mut self, relid: RelId, params: &SequenceParams) -> Result<()>;
    /// ALTER SEQUENCE RESTART: next `nextval` returns `value`.
    fn seq_restart(&mut self, relid: RelId, value: i64) -> Result<()>;
    /// DROP SEQUENCE: delete the state.
    fn seq_drop(&mut self, relid: RelId) -> Result<()>;

    // ---- internal subtransactions (amendment exec-10) ----------------------------------------
    /// Start an internal subtransaction (PL/pgSQL exception block); legal in implicit transactions.
    /// Returns the token for [`ExecTxn::subxact_release`] / [`ExecTxn::subxact_rollback`].
    fn subxact_begin(&mut self) -> Result<usize> {
        Err(PgError::unimplemented("EXE-06", "subtransactions in this transaction view"))
    }
    /// Keep the subtransaction's writes (they become part of the enclosing level).
    fn subxact_release(&mut self, _token: usize) -> Result<()> {
        Err(PgError::unimplemented("EXE-06", "subtransactions in this transaction view"))
    }
    /// Undo the subtransaction's writes and locks; the transaction stays usable.
    fn subxact_rollback(&mut self, _token: usize) -> Result<()> {
        Err(PgError::unimplemented("EXE-06", "subtransactions in this transaction view"))
    }
}

/// Convenience used by scans: read the rows of `[start, end)` without holding the iterator across
/// calls (module docs of [`crate::rowstore`]).
pub fn kv_scan_vec(
    kv: &dyn KvRead,
    lower: std::ops::Bound<&[u8]>,
    upper: std::ops::Bound<&[u8]>,
    limit: usize,
) -> Result<Vec<(Vec<u8>, Vec<u8>)>> {
    let it: KvIter<'_> = kv.scan(lower, upper)?;
    let mut out = Vec::new();
    for item in it {
        out.push(item?);
        if out.len() >= limit {
            break;
        }
    }
    Ok(out)
}

impl ExecTxn for TxnKv<'_> {
    fn txn_id(&self) -> u64 {
        self.txn().id().0
    }
    fn isolation(&self) -> IsolationLevel {
        self.txn().isolation()
    }
    fn is_read_only(&self) -> bool {
        self.txn().is_read_only()
    }
    fn check_interrupts(&self) -> Result<()> {
        self.txn().check_interrupts()
    }
    fn check_writable(&self, command: &str) -> Result<()> {
        self.txn().check_not_failed()?;
        self.txn().check_writable(command)
    }

    fn alloc_rowids(&mut self, relid: RelId, n: u64) -> Result<Range<u64>> {
        pgarbor_core::alloc::next_rowids(self.txn(), relid, n)
    }
    fn put_fresh(&mut self, key: &[u8], value: &[u8]) -> Result<()> {
        TxnKv::put_fresh(self, key, value)
    }

    fn lock_and_read_latest(&mut self, key: &[u8], mode: LockMode, policy: WaitPolicy) -> Result<LatestRead> {
        self.txn().check_not_failed()?;
        let outcome = self.txn_mut().lock_key(key, mode, policy)?;
        let own: Option<Option<Vec<u8>>> = self.txn().overlay().get(key).map(|e| e.map(|v| v.to_vec()));
        match outcome {
            LockOutcome::NotAvailable => Ok(LatestRead::NotAvailable),
            LockOutcome::Unchanged => {
                let value = KvRead::get(self, key)?;
                Ok(LatestRead::Locked { value, changed: false })
            }
            LockOutcome::Changed(latest) => {
                if !self.txn().isolation().uses_statement_snapshot() {
                    return Err(if latest.is_some() {
                        pgarbor_core::error::serialization_concurrent_update()
                    } else {
                        pgarbor_core::error::serialization_concurrent_delete()
                    });
                }
                match own {
                    Some(mine) => Ok(LatestRead::Locked { value: mine, changed: false }),
                    None => Ok(LatestRead::Locked { value: latest, changed: true }),
                }
            }
        }
    }

    fn lock_relation(&mut self, relid: RelId, mode: LockMode, policy: WaitPolicy) -> Result<bool> {
        self.txn().check_not_failed()?;
        match self.txn_mut().lock_relation(relid, mode, policy)? {
            pgarbor_core::AcquireResult::Acquired { .. } => Ok(true),
            pgarbor_core::AcquireResult::NotAvailable => Ok(false),
        }
    }

    fn lock_ddl(&mut self) -> Result<()> {
        self.txn().check_not_failed()?;
        let key = pgarbor_catalog::ddl_lock_key();
        match self.txn_mut().lock_key(&key, LockMode::ForUpdate, WaitPolicy::Block)? {
            LockOutcome::Unchanged => Ok(()),
            LockOutcome::Changed(_) => Err(crate::error::catalog_retry()),
            LockOutcome::NotAvailable => Err(PgError::internal("DDL lock reported NotAvailable under Block policy")),
        }
    }

    fn advisory_lock(&mut self, key: AdvisoryKey, mode: AdvisoryMode, scope: AdvisoryScope, wait: bool) -> Result<bool> {
        pgarbor_core::locks::advisory::lock(self.txn(), key, mode, scope, wait)
    }
    fn advisory_unlock(&mut self, key: AdvisoryKey, mode: AdvisoryMode) -> Result<UnlockResult> {
        pgarbor_core::locks::advisory::unlock(self.txn(), key, mode)
    }
    fn advisory_unlock_all(&mut self) -> Result<()> {
        pgarbor_core::locks::advisory::unlock_all(self.txn())
    }

    fn alloc_oid(&mut self, seed: &dyn Fn(&dyn KvRead) -> Result<Oid>) -> Result<Oid> {
        pgarbor_core::alloc::next_oid(self.txn(), seed)
    }
    fn seq_nextval(&mut self, relid: RelId, params: &SequenceParams) -> Result<i64> {
        seqstore::nextval(self.txn_mut(), relid, params)
    }
    fn seq_setval(&mut self, relid: RelId, params: &SequenceParams, value: i64, is_called: bool) -> Result<i64> {
        seqstore::setval(self.txn_mut(), relid, params, value, is_called)
    }
    fn seq_currval(&self, relid: RelId, name: &str) -> Result<i64> {
        seqstore::currval(self.txn(), relid, name)
    }
    fn seq_lastval(&self) -> Result<i64> {
        seqstore::lastval(self.txn())
    }
    fn seq_state(&self, relid: RelId) -> Result<SequenceState> {
        seqstore::read_state(self.txn(), relid)
    }
    fn seq_create(&mut self, relid: RelId, params: &SequenceParams) -> Result<()> {
        seqstore::create(self.txn_mut(), relid, params)
    }
    fn seq_restart(&mut self, relid: RelId, value: i64) -> Result<()> {
        seqstore::restart(self.txn_mut(), relid, value)
    }
    fn seq_drop(&mut self, relid: RelId) -> Result<()> {
        seqstore::drop(self.txn_mut(), relid)
    }
    fn subxact_begin(&mut self) -> Result<usize> {
        self.txn_mut().begin_subxact()
    }
    fn subxact_release(&mut self, token: usize) -> Result<()> {
        self.txn_mut().release_subxact(token)
    }
    fn subxact_rollback(&mut self, token: usize) -> Result<()> {
        self.txn_mut().rollback_subxact(token)
    }
}

/// `CatalogStore` (= `KvWrite + OidSource`) over an [`ExecTxn`]: what `CatalogTxn::begin` needs
/// (catalog amendment 02). OIDs come from [`ExecTxn::alloc_oid`] seeded by
/// `pgarbor_catalog::persist::oid_seed`.
// CONTRACT
pub struct ExecCatalogStore<'a> {
    pub txn: &'a mut dyn ExecTxn,
}

impl<'a> ExecCatalogStore<'a> {
    pub fn new(txn: &'a mut dyn ExecTxn) -> ExecCatalogStore<'a> {
        ExecCatalogStore { txn }
    }
}

impl KvRead for ExecCatalogStore<'_> {
    fn get(&self, key: &[u8]) -> Result<Option<Vec<u8>>> {
        self.txn.get(key)
    }
    fn scan<'b>(&'b self, lower: std::ops::Bound<&[u8]>, upper: std::ops::Bound<&[u8]>) -> Result<KvIter<'b>> {
        self.txn.scan(lower, upper)
    }
}

impl KvWrite for ExecCatalogStore<'_> {
    fn put(&mut self, key: &[u8], value: &[u8]) -> Result<()> {
        self.txn.put(key, value)
    }
    fn delete(&mut self, key: &[u8]) -> Result<bool> {
        self.txn.delete(key)
    }
}

impl pgarbor_catalog::OidSource for ExecCatalogStore<'_> {
    fn next_oid(&mut self) -> Result<Oid> {
        self.txn.alloc_oid(&|kv| pgarbor_catalog::persist::oid_seed(kv))
    }
}
