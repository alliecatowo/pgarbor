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
use crate::error;
use crate::txn::Txn;
use crate::types::CommitOutcome;
use pgarbor_branches::record::BranchState;
use pgarbor_branches::{directory, ops, OpCtx};
use pgarbor_common::{CommitSeq, Oid, RelId, Result, SpaceId};
use pgarbor_storage::WriteTxn;
use std::sync::Arc;
use pgarbor_common::time::{SystemTime, UNIX_EPOCH};

fn now_unix_ms() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_millis() as u64).unwrap_or(0)
}

/// Relation ids of the sequence-state keys (space 2, 8-byte keys) present in the overlay.
fn sequence_relids<'a>(keys: impl Iterator<Item = &'a Vec<u8>>) -> Vec<RelId> {
    let prefix = SpaceId::SEQUENCES.prefix();
    let mut out = Vec::new();
    for k in keys {
        if k.len() >= 8 && k[0..4] == prefix {
            let mut b = [0u8; 4];
            b.copy_from_slice(&k[4..8]);
            out.push(RelId::from_be_bytes(b));
        }
    }
    out
}

impl Txn {
    /// Clear the session activity fields owned by the transaction.
    fn clear_activity(&self) {
        self.session.update_activity(|a| {
            a.xact_start_ms = None;
            a.txn = None;
            a.backend_xid = None;
        });
    }

    /// Roll back after a failed commit attempt: release everything, clear activity.
    fn abandon_commit(&mut self) {
        let _ = self.release_all();
        self.clear_activity();
    }

    /// COMMIT (module docs). Consumes the transaction.
    pub fn commit(mut self) -> Result<CommitOutcome> {
        // 1. COMMIT of an aborted block rolls back.
        if self.state == crate::types::TxnState::Failed {
            let r = self.release_all();
            self.clear_activity();
            r?;
            return Ok(CommitOutcome::RolledBack);
        }
        // 2. Nothing to write: no storage access at all.
        if self.overlay.is_empty() {
            let r = self.release_all();
            self.clear_activity();
            r?;
            return Ok(CommitOutcome::Committed { seq: None });
        }
        // 3. SSI hook (inactive in v1).
        let engine = Arc::clone(&self.engine);
        if engine.ssi().is_active() {
            if let Err(e) = engine.ssi().pre_commit(&self) {
                self.abandon_commit();
                return Err(e);
            }
        }
        let seq_relids = sequence_relids(self.overlay.as_map().keys());

        // 4. Apply onto the current head inside the single writer and make it durable.
        let mut w = match engine.container().begin_write() {
            Ok(w) => w,
            Err(e) => {
                self.abandon_commit();
                return Err(e);
            }
        };
        if let Err(e) = apply_commit(&self, &mut w) {
            w.abort();
            self.abandon_commit();
            return Err(e);
        }
        let seq = match w.commit() {
            Ok(s) => s,
            Err(e) => {
                self.abandon_commit();
                return Err(e);
            }
        };

        // 5. Durable: invalidate cached sequence state, then release locks.
        if !seq_relids.is_empty() {
            engine.sequences().invalidate_keys(&self.branch, self.database, &seq_relids);
        }
        // The commit is durable; a lock-release failure cannot undo it.
        let _ = self.release_all();
        // 6. Session activity.
        self.clear_activity();
        // 7.
        Ok(CommitOutcome::Committed { seq: Some(seq) })
    }
}

/// Steps 4a-4d inside an open storage write (shared with tests that inject faults). Returns the new
/// database root; does not commit `w`.
pub(crate) fn apply_commit(txn: &Txn, w: &mut WriteTxn<'_>) -> Result<pgarbor_common::PageId> {
    let dir = w.directory_root();
    let branch = txn.branch;
    let db: Oid = txn.database;
    let loaded = directory::load_record(&w.tree(dir), &branch)?;
    let rec = match loaded {
        Some(r) => r,
        None => return Err(pgarbor_branches::error::branch_id_not_found(&branch)),
    };
    if rec.state == BranchState::Deleted || rec.generation != txn.generation {
        return Err(error::branch_changed_under_transaction(&txn.branch_name));
    }
    let old_root = match rec.database_root(db) {
        Some(r) => r,
        None => return Err(error::database_not_in_branch(db, &txn.branch_name)),
    };
    let batch = txn.overlay.to_batch()?;
    let new_root = w.apply(old_root, &batch)?;
    let ctx = OpCtx { commit_seq: w.commit_seq(), now_unix_ms: now_unix_ms() };
    let new_dir = {
        let mut t = w.tree(dir);
        ops::advance_head(&mut t, &ctx, &branch, txn.generation, db, new_root)?;
        t.root()
    };
    w.set_directory_root(new_dir);
    Ok(new_root)
}

/// Durable sequence number of the last commit a txn produced (diagnostics helper used by tests).
pub fn describe_commit(seq: Option<CommitSeq>) -> String {
    match seq {
        Some(s) => format!("committed at {}", s.0),
        None => "committed (no changes)".to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn describe() {
        assert_eq!(describe_commit(None), "committed (no changes)");
        assert_eq!(describe_commit(Some(CommitSeq(7))), "committed at 7");
    }

    #[test]
    fn sequence_keys_extracted() {
        let mut k = SpaceId::SEQUENCES.prefix().to_vec();
        k.extend_from_slice(&16400u32.to_be_bytes());
        let other = vec![0u8, 0, 0, 1, 0, 0, 0, 9];
        let short = vec![0u8, 0, 0, 2];
        let keys = vec![k, other, short];
        assert_eq!(sequence_relids(keys.iter()), vec![16400]);
    }
}
