//! Serializable Snapshot Isolation (TXN-11). Lease CORE-16 (opus). NOT active in v1.
//!
//! Until this lease lands, SERIALIZABLE runs with REPEATABLE READ snapshots + write-conflict rules
//! (ARCHITECTURE §7 KNOWN INCOMPATIBILITY; inventory `serializable_ssi = UNIMPLEMENTED`; recorded
//! in docs/compatibility/KNOWN_GAPS.md). `SsiTracker::is_active()` returns false, so no caller
//! invokes the hooks below; enabling them is part of this lease (flip `is_active` and wire the
//! hooks in txn_kv.rs/commit.rs via a follow-up amendment to those files' owners).
//!
//! Target design (PG src/backend/storage/lmgr/predicate.c, README-SSI; Cahill/Ports & Grittner):
//! - SIREAD "predicate locks" on key RANGES read by SERIALIZABLE transactions (point gets = single
//!   key range; scans = their bounds), per (branch, database), retained past commit until all
//!   overlapping transactions finish.
//! - rw-antidependency edges: writer W of key k overlapping a SIREAD range of reader R (R → W).
//! - Dangerous structure T1 → T2 → T3 with T3 committed first (and T1 not read-only-safe) → abort
//!   one with 40001 `could not serialize access due to read/write dependencies among transactions`,
//!   detail `Reason code: Canceled on identification as a pivot, during commit attempt.` (or the
//!   other reason codes of predicate.c), hint `The transaction might succeed if retried.`
//! - READ ONLY DEFERRABLE: wait for a safe snapshot.
use crate::txn::Txn;
use pgarbor_common::{PgError, Result};
use std::ops::Bound;

// CONTRACT
#[derive(Debug, Default)]
pub struct SsiTracker {}

impl SsiTracker {
    pub fn new() -> SsiTracker {
        SsiTracker {}
    }

    /// False until CORE-16 is implemented (see module docs; this is a recorded known gap, not a
    /// silent stub: SERIALIZABLE degrades to the documented RR+write-conflict behavior).
    pub fn is_active(&self) -> bool {
        false
    }

    /// Record a SIREAD range for a SERIALIZABLE txn.
    #[allow(unused_variables)]
    pub fn on_read(&self, txn: &Txn, lower: Bound<&[u8]>, upper: Bound<&[u8]>) -> Result<()> {
        Err(PgError::unimplemented("CORE-16", "serializable predicate lock"))
    }

    /// Record a write and create rw-conflict edges.
    #[allow(unused_variables)]
    pub fn on_write(&self, txn: &Txn, key: &[u8]) -> Result<()> {
        Err(PgError::unimplemented("CORE-16", "serializable write conflict tracking"))
    }

    /// Dangerous-structure check at commit.
    #[allow(unused_variables)]
    pub fn pre_commit(&self, txn: &Txn) -> Result<()> {
        Err(PgError::unimplemented("CORE-16", "serializable commit check"))
    }

    /// Transaction finished (commit or abort): release / age out its SIREAD state.
    #[allow(unused_variables)]
    pub fn on_finish(&self, txn: &Txn, committed: bool) -> Result<()> {
        Err(PgError::unimplemented("CORE-16", "serializable cleanup"))
    }
}
