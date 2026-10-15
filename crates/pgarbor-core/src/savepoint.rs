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
use pgarbor_common::{PgError, Result};

// CONTRACT
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Savepoint {
    pub name: String,
    pub mark: OverlayMark,
    pub subxact: SubXactId,
}

impl Txn {
    /// `SAVEPOINT name`.
    #[allow(unused_variables)]
    pub fn savepoint(&mut self, name: &str) -> Result<()> {
        Err(PgError::unimplemented("CORE-05", "SAVEPOINT"))
    }

    /// `RELEASE [SAVEPOINT] name`.
    #[allow(unused_variables)]
    pub fn release_savepoint(&mut self, name: &str) -> Result<()> {
        Err(PgError::unimplemented("CORE-05", "RELEASE SAVEPOINT"))
    }

    /// `ROLLBACK TO [SAVEPOINT] name`.
    #[allow(unused_variables)]
    pub fn rollback_to_savepoint(&mut self, name: &str) -> Result<()> {
        Err(PgError::unimplemented("CORE-05", "ROLLBACK TO SAVEPOINT"))
    }

    /// Number of active savepoints.
    pub fn savepoint_depth(&self) -> usize {
        self.savepoints.len()
    }
}
