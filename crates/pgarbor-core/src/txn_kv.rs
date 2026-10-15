//! `TxnKv`: the transaction's `KvRead + KvWrite` view (overlay over snapshot) that pgarbor-exec and
//! pgarbor-catalog read and write through (ARCHITECTURE §4, §7; TXN-03). Lease CORE-06.
//!
//! Reads: overlay entry wins (`Some(None)` = deleted → absent); else the snapshot tree. Requires a
//! snapshot (`Txn::begin_statement` first) — otherwise XX000 `no snapshot: begin_statement was not
//! called`. Scans merge with `overlay::merge_scan`.
//!
//! Writes (`put`/`delete`), in order:
//! 1. `txn.check_not_failed()`; read-only → `error::read_only_transaction(self.command)`.
//! 2. Key lock: `txn.lock_key(key, ForNoKeyUpdate if self.no_key_update else ForUpdate, Block)`;
//!    under RR/SERIALIZABLE a `Changed` outcome raises 40001 (update/delete wording per
//!    `Txn::lock_key_for_write`); under RC the write proceeds (callers that need EvalPlanQual call
//!    `lock_key` first and decide). Keys in space 2 (sequences) skip locking (non-transactional
//!    state, see sequence/store.rs).
//! 3. `overlay.put/delete`; `txn.wrote = true`; session `backend_xid = Some(txn.id)` on first write.
//! `delete` returns whether the key was visible in the merged view before deleting.
//!
//! `put_fresh`: insert of a key whose uniqueness is guaranteed by construction (a table row key
//! built from a rowid just returned by `alloc::next_rowid` for this txn) — skips step 2 (no lock
//! entry, no outcome check), keeping bulk INSERT memory O(rows written), not O(rows * lock state).
use crate::error;
use crate::locks::mode::LockMode;
use crate::txn::Txn;
use crate::types::{LockOutcome, WaitPolicy};
use pgarbor_common::{KvIter, KvRead, KvWrite, Result, SpaceId};
use std::ops::Bound;

// CONTRACT
pub struct TxnKv<'t> {
    pub(crate) txn: &'t mut Txn,
    /// Command name for 25006 messages (`INSERT`, `UPDATE`, `DELETE`, `CREATE TABLE`, ...).
    pub(crate) command: &'static str,
    /// Use ForNoKeyUpdate instead of ForUpdate for implicit write locks (UPDATE not touching key
    /// columns, PG semantics so FK checks' KEY SHARE locks do not block it).
    pub(crate) no_key_update: bool,
}

impl Txn {
    /// The read/write view for one statement. `command` names the statement for read-only errors.
    pub fn kv(&mut self, command: &'static str) -> TxnKv<'_> {
        TxnKv { txn: self, command, no_key_update: false }
    }
}

impl<'t> TxnKv<'t> {
    pub fn txn(&self) -> &Txn {
        self.txn
    }
    pub fn txn_mut(&mut self) -> &mut Txn {
        self.txn
    }
    pub fn set_no_key_update(&mut self, on: bool) {
        self.no_key_update = on;
    }

    /// Unlocked insert of a freshly allocated row key (module docs).
    pub fn put_fresh(&mut self, key: &[u8], value: &[u8]) -> Result<()> {
        self.prepare_write(key, false)?;
        self.txn.overlay.put(key, value)?;
        self.note_write();
        Ok(())
    }

    /// Steps 1-2 of the write protocol: state/read-only checks and (optionally) the implicit key lock.
    fn prepare_write(&mut self, key: &[u8], lock: bool) -> Result<()> {
        self.txn.check_not_failed()?;
        if self.txn.read_only {
            return Err(error::read_only_transaction(self.command));
        }
        if !lock || key.starts_with(&SpaceId::SEQUENCES.prefix()) {
            return Ok(());
        }
        let mode = if self.no_key_update { LockMode::ForNoKeyUpdate } else { LockMode::ForUpdate };
        let outcome = self.txn.lock_key(key, mode, WaitPolicy::Block)?;
        if !self.txn.isolation.uses_statement_snapshot() {
            match outcome {
                LockOutcome::Changed(Some(_)) => return Err(error::serialization_concurrent_update()),
                LockOutcome::Changed(None) => return Err(error::serialization_concurrent_delete()),
                LockOutcome::Unchanged | LockOutcome::NotAvailable => {}
            }
        }
        Ok(())
    }

    /// Bookkeeping after a successful overlay write.
    fn note_write(&mut self) {
        let first = !self.txn.wrote;
        self.txn.wrote = true;
        if first {
            let id = self.txn.id;
            self.txn.session.update_activity(|a| a.backend_xid = Some(id));
        }
    }
}

fn no_snapshot() -> pgarbor_common::PgError {
    error::internal("no snapshot: begin_statement was not called")
}

impl KvRead for TxnKv<'_> {
    fn get(&self, key: &[u8]) -> Result<Option<Vec<u8>>> {
        let snap = self.txn.snapshot.as_ref().ok_or_else(no_snapshot)?;
        match self.txn.overlay.get(key) {
            Some(entry) => Ok(entry.map(|v| v.to_vec())),
            None => snap.tree.get(key),
        }
    }
    fn scan<'a>(&'a self, lower: Bound<&[u8]>, upper: Bound<&[u8]>) -> Result<KvIter<'a>> {
        let snap = self.txn.snapshot.as_ref().ok_or_else(no_snapshot)?;
        let base = snap.tree.scan(lower, upper)?;
        crate::overlay::merge_scan(base, &self.txn.overlay, lower, upper)
    }
}

impl KvWrite for TxnKv<'_> {
    fn put(&mut self, key: &[u8], value: &[u8]) -> Result<()> {
        self.prepare_write(key, true)?;
        self.txn.overlay.put(key, value)?;
        self.note_write();
        Ok(())
    }
    fn delete(&mut self, key: &[u8]) -> Result<bool> {
        self.prepare_write(key, true)?;
        let existed = self.get(key)?.is_some();
        self.txn.overlay.delete(key)?;
        self.note_write();
        Ok(existed)
    }
}

#[cfg(test)]
mod tests {
    // A `Txn` needs a live `Engine`; behavioral tests (read-only put -> 25006, get before
    // begin_statement -> XX000, own writes visible in get/scan) live in tests/core-07.rs.
    use super::*;

    #[test]
    fn sequence_prefix_detection() {
        let mut k = SpaceId::SEQUENCES.prefix().to_vec();
        k.extend_from_slice(b"x");
        assert!(k.starts_with(&SpaceId::SEQUENCES.prefix()));
        assert!(!SpaceId::CATALOG.prefix().starts_with(&SpaceId::SEQUENCES.prefix()));
    }
}
