//! SAVEPOINT / RELEASE SAVEPOINT / ROLLBACK TO SAVEPOINT (TXN-06, PG xact.c DefineSavepoint,
//! ReleaseSavepoint, RollbackToSavepoint). Lease CORE-05.
//!
//! Semantics (PostgreSQL 18, oracle-verified where marked):
//! - All three require an explicit block: `TxnKind::Implicit` →
//!   `error::outside_transaction_block("SAVEPOINT" | "RELEASE SAVEPOINT" | "ROLLBACK TO SAVEPOINT")`
//!   (25P01, oracle).
//! - SAVEPOINT and RELEASE in a Failed transaction → 25P02 (oracle: RELEASE after an error gives
//!   `current transaction is aborted...`). ROLLBACK TO is allowed in Failed state.
//! - Names may repeat; lookups use the INNERMOST savepoint with that name. Unknown name →
//!   `error::savepoint_does_not_exist(name)` (3B001, oracle).
//! - SAVEPOINT: push `Savepoint{name, mark: overlay.mark(), subxact: SubXactId(++last_subxact)}`
//!   after `overlay.set_logging(true)` (so the mark is meaningful).
//! - RELEASE `s`: pops `s` and every savepoint above it; their locks are reassigned to the
//!   enclosing subxact (`locks.reassign_subxacts(session, txn, s.subxact, parent_subxact)`, and
//!   `held_keys` entries with subxact >= s.subxact get the parent's id); if no savepoints remain,
//!   `overlay.set_logging(false)`. Overlay contents are kept.
//! - ROLLBACK TO `s`: `overlay.rollback_to(s.mark)`; release locks taken since
//!   (`locks.release_subxacts(session, txn, s.subxact)` and drop matching `held_keys`; oracle:
//!   another session can then update the row without blocking); pop savepoints ABOVE `s` but KEEP
//!   `s` itself, re-arming it with a NEW subxact id (`++last_subxact`) and the same mark; state
//!   Failed → InProgress. The snapshot is unchanged (RR keeps its snapshot; RC refreshes next
//!   statement anyway). Sequence advances are not undone (their overlay writes are unlogged).
use crate::overlay::OverlayMark;
use crate::txn::Txn;
use crate::types::SubXactId;
use crate::error;
use crate::types::{TxnKind, TxnState};
use pgarbor_common::Result;

/// Name of the savepoint entries of internal subtransactions (cannot be written in SQL).
const INTERNAL_SUBXACT: &str = "\u{0}internal subtransaction";

// CONTRACT
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Savepoint {
    pub name: String,
    pub mark: OverlayMark,
    pub subxact: SubXactId,
}

impl Txn {
    /// `SAVEPOINT name`.
    pub fn savepoint(&mut self, name: &str) -> Result<()> {
        if self.kind == TxnKind::Implicit {
            return Err(error::outside_transaction_block("SAVEPOINT"));
        }
        self.check_not_failed()?;
        // Logging must be on before the mark is taken so the mark is meaningful.
        self.overlay.set_logging(true)?;
        self.last_subxact += 1;
        let sp = Savepoint { name: name.to_string(), mark: self.overlay.mark(), subxact: SubXactId(self.last_subxact) };
        self.savepoints.push(sp);
        Ok(())
    }

    /// `RELEASE [SAVEPOINT] name`.
    pub fn release_savepoint(&mut self, name: &str) -> Result<()> {
        if self.kind == TxnKind::Implicit {
            return Err(error::outside_transaction_block("RELEASE SAVEPOINT"));
        }
        self.check_not_failed()?;
        let idx = self
            .savepoints
            .iter()
            .rposition(|s| s.name == name)
            .ok_or_else(|| error::savepoint_does_not_exist(name))?;
        let from = self.savepoints[idx].subxact;
        let parent = if idx > 0 { self.savepoints[idx - 1].subxact } else { SubXactId::TOP };
        self.engine.locks().reassign_subxacts(self.session.id, self.id, from, parent)?;
        for h in self.held_keys.values_mut() {
            if h.subxact >= from {
                h.subxact = parent;
            }
        }
        self.savepoints.truncate(idx);
        if self.savepoints.is_empty() {
            self.overlay.set_logging(false)?;
        }
        Ok(())
    }

    /// `ROLLBACK TO [SAVEPOINT] name`.
    pub fn rollback_to_savepoint(&mut self, name: &str) -> Result<()> {
        if self.kind == TxnKind::Implicit {
            return Err(error::outside_transaction_block("ROLLBACK TO SAVEPOINT"));
        }
        let idx = self
            .savepoints
            .iter()
            .rposition(|s| s.name == name)
            .ok_or_else(|| error::savepoint_does_not_exist(name))?;
        let mark = self.savepoints[idx].mark;
        let from = self.savepoints[idx].subxact;
        self.overlay.rollback_to(mark)?;
        self.engine.locks().release_subxacts(self.session.id, self.id, from)?;
        self.held_keys.retain(|_, h| h.subxact < from);
        // Pop everything above the target; keep the target, re-armed with a fresh subxact id.
        self.savepoints.truncate(idx + 1);
        self.last_subxact += 1;
        self.savepoints[idx].subxact = SubXactId(self.last_subxact);
        if self.state == TxnState::Failed {
            self.state = TxnState::InProgress;
        }
        Ok(())
    }

    /// Number of active savepoints.
    pub fn savepoint_depth(&self) -> usize {
        self.savepoints.len()
    }

    /// Start an INTERNAL subtransaction (PL/pgSQL `BEGIN ... EXCEPTION` blocks, PG
    /// `BeginInternalSubTransaction`). Unlike `SAVEPOINT` it is legal in implicit transactions.
    /// Returns a token for [`Txn::release_subxact`] / [`Txn::rollback_subxact`] (amendment core-04).
    pub fn begin_subxact(&mut self) -> Result<usize> {
        self.check_not_failed()?;
        self.overlay.set_logging(true)?;
        self.last_subxact += 1;
        let sp = Savepoint { name: INTERNAL_SUBXACT.to_string(), mark: self.overlay.mark(), subxact: SubXactId(self.last_subxact) };
        self.savepoints.push(sp);
        Ok(self.savepoints.len())
    }

    /// Commit an internal subtransaction: its writes and locks move to the enclosing level.
    pub fn release_subxact(&mut self, token: usize) -> Result<()> {
        let idx = self.internal_subxact_index(token)?;
        let from = self.savepoints[idx].subxact;
        let parent = if idx > 0 { self.savepoints[idx - 1].subxact } else { SubXactId::TOP };
        self.engine.locks().reassign_subxacts(self.session.id, self.id, from, parent)?;
        for h in self.held_keys.values_mut() {
            if h.subxact >= from {
                h.subxact = parent;
            }
        }
        self.savepoints.truncate(idx);
        if self.savepoints.is_empty() {
            self.overlay.set_logging(false)?;
        }
        Ok(())
    }

    /// Abort an internal subtransaction: undo its writes, release its locks, and leave the
    /// transaction in progress (PG `RollbackAndReleaseCurrentSubTransaction`).
    pub fn rollback_subxact(&mut self, token: usize) -> Result<()> {
        let idx = self.internal_subxact_index(token)?;
        let mark = self.savepoints[idx].mark;
        let from = self.savepoints[idx].subxact;
        self.overlay.rollback_to(mark)?;
        self.engine.locks().release_subxacts(self.session.id, self.id, from)?;
        self.held_keys.retain(|_, h| h.subxact < from);
        self.savepoints.truncate(idx);
        if self.savepoints.is_empty() {
            self.overlay.set_logging(false)?;
        }
        if self.state == TxnState::Failed {
            self.state = TxnState::InProgress;
        }
        Ok(())
    }

    fn internal_subxact_index(&self, token: usize) -> Result<usize> {
        match token.checked_sub(1) {
            Some(idx) if self.savepoints.get(idx).is_some_and(|s| s.name == INTERNAL_SUBXACT) => Ok(idx),
            _ => Err(error::internal("internal subtransaction is not the expected savepoint level")),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::Engine;
    use crate::locks::mode::LockMode;
    use crate::options::{EngineOptions, SessionRequest};
    use crate::types::{TxnOptions, WaitPolicy};
        use pgarbor_storage::MemVfs;
    use std::sync::Arc;

    fn engine() -> Arc<Engine> {
        let opts = EngineOptions { vfs: Some(Arc::new(MemVfs::new())), ..EngineOptions::default() };
        Engine::create("sp-test.pga".as_ref(), &opts).unwrap()
    }

    fn session(e: &Arc<Engine>) -> crate::session::SessionHandle {
        e.attach_session(&SessionRequest::embedded("main"))
        .unwrap()
    }

    fn explicit() -> TxnOptions {
        TxnOptions { kind: TxnKind::Explicit, ..TxnOptions::default() }
    }

    #[test]
    fn implicit_requires_block() {
        let e = engine();
        let s = session(&e);
        let mut t = e.begin(&s, &TxnOptions::default()).unwrap();
        let err = t.savepoint("s1").unwrap_err();
        assert_eq!(err.message, "SAVEPOINT can only be used in transaction blocks");
        assert!(t.release_savepoint("s1").is_err());
        assert!(t.rollback_to_savepoint("s1").is_err());
    }

    #[test]
    fn rollback_to_undoes_overlay() {
        let e = engine();
        let s = session(&e);
        let mut t = e.begin(&s, &explicit()).unwrap();
        t.overlay.put(b"a", b"1").unwrap();
        t.savepoint("s1").unwrap();
        t.overlay.put(b"b", b"2").unwrap();
        t.rollback_to_savepoint("s1").unwrap();
        assert!(t.overlay.get(b"a").is_some());
        assert!(t.overlay.get(b"b").is_none());
        // Twice works and the savepoint survives.
        t.overlay.put(b"c", b"3").unwrap();
        t.rollback_to_savepoint("s1").unwrap();
        assert!(t.overlay.get(b"c").is_none());
        assert_eq!(t.savepoint_depth(), 1);
    }

    #[test]
    fn duplicate_names_use_innermost() {
        let e = engine();
        let s = session(&e);
        let mut t = e.begin(&s, &explicit()).unwrap();
        t.savepoint("x").unwrap();
        t.overlay.put(b"a", b"1").unwrap();
        t.savepoint("x").unwrap();
        t.overlay.put(b"b", b"2").unwrap();
        t.rollback_to_savepoint("x").unwrap();
        assert!(t.overlay.get(b"a").is_some());
        assert!(t.overlay.get(b"b").is_none());
        assert_eq!(t.savepoint_depth(), 2);
        t.release_savepoint("x").unwrap();
        assert_eq!(t.savepoint_depth(), 1);
        t.release_savepoint("x").unwrap();
        assert_eq!(t.savepoint_depth(), 0);
        assert!(t.overlay.get(b"a").is_some());
    }

    #[test]
    fn unknown_name_is_3b001() {
        let e = engine();
        let s = session(&e);
        let mut t = e.begin(&s, &explicit()).unwrap();
        let err = t.release_savepoint("nope").unwrap_err();
        assert_eq!(err.message, "savepoint \"nope\" does not exist");
        assert!(t.rollback_to_savepoint("nope").is_err());
    }

    #[test]
    fn failed_state_rules() {
        let e = engine();
        let s = session(&e);
        let mut t = e.begin(&s, &explicit()).unwrap();
        t.savepoint("s1").unwrap();
        t.mark_failed();
        assert!(t.release_savepoint("s1").is_err());
        assert!(t.savepoint("s2").is_err());
        t.rollback_to_savepoint("s1").unwrap();
        assert_eq!(t.state(), TxnState::InProgress);
    }

    #[test]
    fn locks_released_on_rollback_to() {
        let e = engine();
        let s1 = session(&e);
        let s2 = session(&e);
        let mut t1 = e.begin(&s1, &explicit()).unwrap();
        let mut t2 = e.begin(&s2, &explicit()).unwrap();
        t1.savepoint("s1").unwrap();
        t1.lock_key(b"k", LockMode::ForUpdate, WaitPolicy::Block).unwrap();
        assert!(t1.held_keys.contains_key(b"k".as_slice()));
        t1.rollback_to_savepoint("s1").unwrap();
        assert!(t1.held_keys.is_empty());
        let out = t2.lock_key(b"k", LockMode::ForUpdate, WaitPolicy::NoWait);
        assert!(out.is_ok());
    }
}
