//! COMMIT protocol (TXN-04, ARCHITECTURE §7). Lease CORE-07 (opus).
//!
//! `Txn::commit(self)`:
//! 1. State Failed → `release_all()`, session activity cleared → `Ok(CommitOutcome::RolledBack)`
//!    (PG: COMMIT of an aborted block reports ROLLBACK).
//! 2. Empty overlay (read-only work) → NO storage write at all (no `begin_write`, no fsync, no
//!    CommitSeq bump): `release_all()` → `Committed{seq: None}`.
//! 3. SSI: if `engine.ssi().is_active()` → `ssi.pre_commit(&self)` (may raise 40001). In v1
//!    `is_active()` is false.
//! 4. `w = container.begin_write()` (the container's single writer; held briefly). Then
//!    `apply_commit(&mut w, ..)`:
//!    a. `rec = directory::load_record(&w.tree(dir) .. )` — read the CURRENT branch record inside
//!       the write txn (heads may have moved since our snapshot; keys we wrote are locked by us,
//!       so applying our overlay to the newer head loses no update).
//!    b. Deleted, or `rec.generation != self.generation` → abort `w`, `release_all()`, return
//!       `error::branch_changed_under_transaction(name)` (never apply onto a foreign root).
//!    c. `old_root = rec.database_root(db)`; `new_root = w.apply(old_root, &overlay.to_batch()?)`.
//!    d. `ops::advance_head(&mut w.tree(dir), &OpCtx{commit_seq: w.commit_seq(), now},
//!       &branch, generation, db, new_root)`; `w.set_directory_root(new_dir)`.
//!    e. `seq = w.commit()?` — durable when Ok. Any Err (53100 disk full, 58030 I/O, poisoned) →
//!       the transaction is rolled back (`release_all()`) and the error returned (PG reports the
//!       error; nothing became visible).
//! 5. AFTER durability: `engine.sequences().invalidate_keys(branch, db, overlay space-2 keys)`
//!    (DROP SEQUENCE / ALTER SEQUENCE committed), then `release_all()` — releasing key locks only
//!    now guarantees a woken waiter's fresh head already contains our commit.
//! 6. Session activity: `xact_start_ms = None`, `txn = None`, `backend_xid = None`.
//! 7. `Ok(Committed{seq: Some(seq)})`.
//!
//! Group commit / fsync batching is out of scope for v1 (2 fsyncs per writing commit).
use crate::txn::Txn;
use crate::types::CommitOutcome;
use pgarbor_common::{CommitSeq, PgError, Result};
use pgarbor_storage::WriteTxn;

impl Txn {
    /// COMMIT (module docs). Consumes the transaction.
    #[allow(unused_variables)]
    pub fn commit(mut self) -> Result<CommitOutcome> {
        Err(PgError::unimplemented("CORE-07", "transaction commit"))
    }
}

/// Steps 4a-4d inside an open storage write (shared with tests that inject faults). Returns the new
/// database root; does not commit `w`.
#[allow(unused_variables)]
pub(crate) fn apply_commit(txn: &Txn, w: &mut WriteTxn<'_>) -> Result<pgarbor_common::PageId> {
    Err(PgError::unimplemented("CORE-07", "overlay application to branch head"))
}

/// Durable sequence number of the last commit a txn produced (diagnostics helper used by tests).
pub fn describe_commit(seq: Option<CommitSeq>) -> String {
    match seq {
        Some(s) => format!("committed at {}", s.0),
        None => "committed (no changes)".to_string(),
    }
}
