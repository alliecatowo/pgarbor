//! Lock modes and conflict tables. Lease CORE-08 (`conflicts_with`, `pg_name`).
//!
//! Table-level modes are PostgreSQL's eight heavyweight modes (storage/lmgr/lock.c
//! `LockConflicts`, docs "13.3.1 Table-Level Locks"); they are used for `LockTag::Relation`,
//! `LockTag::Object` and `LockTag::Advisory` (advisory locks use `Exclusive` and `Share`).
//! Row-level modes are the four tuple lock strengths (docs "13.3.2 Row-Level Locks",
//! heapam.c `tupleLockExtraInfo`) and are used ONLY for `LockTag::Key`.
//!
//! Table-level conflicts (symmetric; lock.c LockConflicts):
//! - AccessShare: AccessExclusive
//! - RowShare: Exclusive, AccessExclusive
//! - RowExclusive: Share, ShareRowExclusive, Exclusive, AccessExclusive
//! - ShareUpdateExclusive: ShareUpdateExclusive, Share, ShareRowExclusive, Exclusive, AccessExclusive
//! - Share: RowExclusive, ShareUpdateExclusive, ShareRowExclusive, Exclusive, AccessExclusive
//! - ShareRowExclusive: RowExclusive, ShareUpdateExclusive, Share, ShareRowExclusive, Exclusive,
//!   AccessExclusive
//! - Exclusive: RowShare, RowExclusive, ShareUpdateExclusive, Share, ShareRowExclusive, Exclusive,
//!   AccessExclusive
//! - AccessExclusive: all eight
//!
//! Row-level conflict matrix (X = conflict):
//! ```text
//!                   KeyShare  Share  NoKeyUpdate  Update
//! ForKeyShare                                       X
//! ForShare                             X            X
//! ForNoKeyUpdate              X        X            X
//! ForUpdate          X        X        X            X
//! ```
//! A table-level mode never conflicts with a row-level mode (they are never used on the same tag;
//! `conflicts_with` returns false for mixed pairs and the manager rejects mixed use with XX000).
use pgarbor_common::{PgError, Result};

// CONTRACT
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, serde::Serialize)]
pub enum LockMode {
    AccessShare,
    RowShare,
    RowExclusive,
    ShareUpdateExclusive,
    Share,
    ShareRowExclusive,
    Exclusive,
    AccessExclusive,
    ForKeyShare,
    ForShare,
    ForNoKeyUpdate,
    ForUpdate,
}

impl LockMode {
    pub const TABLE_MODES: [LockMode; 8] = [
        LockMode::AccessShare,
        LockMode::RowShare,
        LockMode::RowExclusive,
        LockMode::ShareUpdateExclusive,
        LockMode::Share,
        LockMode::ShareRowExclusive,
        LockMode::Exclusive,
        LockMode::AccessExclusive,
    ];
    pub const ROW_MODES: [LockMode; 4] =
        [LockMode::ForKeyShare, LockMode::ForShare, LockMode::ForNoKeyUpdate, LockMode::ForUpdate];

    pub fn is_row_mode(self) -> bool {
        matches!(self, LockMode::ForKeyShare | LockMode::ForShare | LockMode::ForNoKeyUpdate | LockMode::ForUpdate)
    }

    /// Conflict per the matrices above (symmetric). Mixed table/row pairs → false.
    #[allow(unused_variables)]
    pub fn conflicts_with(self, other: LockMode) -> Result<bool> {
        Err(PgError::unimplemented("CORE-08", "lock conflict matrix"))
    }

    /// `pg_locks.mode` text. Table modes: `AccessShareLock`, `RowShareLock`, `RowExclusiveLock`,
    /// `ShareUpdateExclusiveLock`, `ShareLock`, `ShareRowExclusiveLock`, `ExclusiveLock`,
    /// `AccessExclusiveLock`. Row modes are shown as the heavyweight tuple-lock mode PG uses for
    /// them (heapam.c `tupleLockExtraInfo`): ForKeyShare → `AccessShareLock`, ForShare →
    /// `RowShareLock`, ForNoKeyUpdate → `ExclusiveLock`, ForUpdate → `AccessExclusiveLock`.
    #[allow(unused_variables)]
    pub fn pg_name(self) -> Result<&'static str> {
        Err(PgError::unimplemented("CORE-08", "lock mode names"))
    }

    /// Parse `LOCK TABLE ... IN <mode> MODE` words (case-insensitive, single spaces):
    /// `ACCESS SHARE`, `ROW SHARE`, `ROW EXCLUSIVE`, `SHARE UPDATE EXCLUSIVE`, `SHARE`,
    /// `SHARE ROW EXCLUSIVE`, `EXCLUSIVE`, `ACCESS EXCLUSIVE`. Unknown → None.
    #[allow(unused_variables)]
    pub fn parse_table_mode(words: &str) -> Result<Option<LockMode>> {
        Err(PgError::unimplemented("CORE-08", "LOCK TABLE mode parsing"))
    }
}
