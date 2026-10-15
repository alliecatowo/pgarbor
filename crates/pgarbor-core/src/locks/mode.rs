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
use pgarbor_common::Result;

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

// Conflict bitmasks indexed by position. Bit `j` of `TABLE_CONFLICTS[i]` is set when table mode
// `i` (TABLE_MODES order) conflicts with table mode `j`. Transcribed from storage/lmgr/lock.c
// `LockConflicts` (entry index = lock mode number - 1).
const TABLE_CONFLICTS: [u8; 8] = [
    0x80, // AccessShare:           AccessExclusive
    0xC0, // RowShare:              Exclusive, AccessExclusive
    0xF0, // RowExclusive:          Share, ShareRowExclusive, Exclusive, AccessExclusive
    0xF8, // ShareUpdateExclusive:  itself, Share, ShareRowExclusive, Exclusive, AccessExclusive
    0xEC, // Share:                 RowExclusive, ShareUpdateExclusive, ShareRowExclusive, Exclusive, AccessExclusive
    0xFC, // ShareRowExclusive:     RowExclusive, ShareUpdateExclusive, Share, itself, Exclusive, AccessExclusive
    0xFE, // Exclusive:             every mode except AccessShare
    0xFF, // AccessExclusive:       all eight
];

// Row-level conflicts (PostgreSQL docs Table 13.3, "Conflicting Row-Level Locks"), indexed as
// ROW_MODES: ForKeyShare, ForShare, ForNoKeyUpdate, ForUpdate.
const ROW_CONFLICTS: [u8; 4] = [
    0x08, // ForKeyShare:    ForUpdate
    0x0C, // ForShare:       ForNoKeyUpdate, ForUpdate
    0x0E, // ForNoKeyUpdate: ForShare, itself, ForUpdate
    0x0F, // ForUpdate:      all four
];

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
    pub fn conflicts_with(self, other: LockMode) -> Result<bool> {
        // Discriminants: table modes are 0..8 (TABLE_MODES order), row modes are 8..12.
        let a = self as usize;
        let b = other as usize;
        let conflict = match (a < 8, b < 8) {
            (true, true) => (TABLE_CONFLICTS[a] & (1u8 << b)) != 0,
            (false, false) => (ROW_CONFLICTS[a - 8] & (1u8 << (b - 8))) != 0,
            _ => false,
        };
        Ok(conflict)
    }

    /// `pg_locks.mode` text. Table modes: `AccessShareLock`, `RowShareLock`, `RowExclusiveLock`,
    /// `ShareUpdateExclusiveLock`, `ShareLock`, `ShareRowExclusiveLock`, `ExclusiveLock`,
    /// `AccessExclusiveLock`. Row modes are shown as the heavyweight tuple-lock mode PG uses for
    /// them (heapam.c `tupleLockExtraInfo`): ForKeyShare → `AccessShareLock`, ForShare →
    /// `RowShareLock`, ForNoKeyUpdate → `ExclusiveLock`, ForUpdate → `AccessExclusiveLock`.
    pub fn pg_name(self) -> Result<&'static str> {
        Ok(match self {
            LockMode::AccessShare => "AccessShareLock",
            LockMode::RowShare => "RowShareLock",
            LockMode::RowExclusive => "RowExclusiveLock",
            LockMode::ShareUpdateExclusive => "ShareUpdateExclusiveLock",
            LockMode::Share => "ShareLock",
            LockMode::ShareRowExclusive => "ShareRowExclusiveLock",
            LockMode::Exclusive => "ExclusiveLock",
            LockMode::AccessExclusive => "AccessExclusiveLock",
            LockMode::ForKeyShare => "AccessShareLock",
            LockMode::ForShare => "RowShareLock",
            LockMode::ForNoKeyUpdate => "ExclusiveLock",
            LockMode::ForUpdate => "AccessExclusiveLock",
        })
    }

    /// Parse `LOCK TABLE ... IN <mode> MODE` words (case-insensitive, single spaces):
    /// `ACCESS SHARE`, `ROW SHARE`, `ROW EXCLUSIVE`, `SHARE UPDATE EXCLUSIVE`, `SHARE`,
    /// `SHARE ROW EXCLUSIVE`, `EXCLUSIVE`, `ACCESS EXCLUSIVE`. Unknown → None.
    pub fn parse_table_mode(words: &str) -> Result<Option<LockMode>> {
        // Normalize: uppercase, words joined by single spaces. Extra whitespace is tolerated;
        // any unknown or extra word makes the exact match below fail.
        let mut norm = String::with_capacity(words.len());
        for (i, w) in words.split_ascii_whitespace().enumerate() {
            if i > 0 {
                norm.push(' ');
            }
            norm.push_str(&w.to_ascii_uppercase());
        }
        Ok(match norm.as_str() {
            "ACCESS SHARE" => Some(LockMode::AccessShare),
            "ROW SHARE" => Some(LockMode::RowShare),
            "ROW EXCLUSIVE" => Some(LockMode::RowExclusive),
            "SHARE UPDATE EXCLUSIVE" => Some(LockMode::ShareUpdateExclusive),
            "SHARE" => Some(LockMode::Share),
            "SHARE ROW EXCLUSIVE" => Some(LockMode::ShareRowExclusive),
            "EXCLUSIVE" => Some(LockMode::Exclusive),
            "ACCESS EXCLUSIVE" => Some(LockMode::AccessExclusive),
            _ => None,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::LockMode;
    use super::LockMode::*;

    /// Expected table-level conflicts, transcribed by hand from storage/lmgr/lock.c
    /// `LockConflicts` (independent of the bitmasks above).
    fn expected_table(m: LockMode) -> &'static [LockMode] {
        match m {
            AccessShare => &[AccessExclusive],
            RowShare => &[Exclusive, AccessExclusive],
            RowExclusive => &[Share, ShareRowExclusive, Exclusive, AccessExclusive],
            ShareUpdateExclusive => &[ShareUpdateExclusive, Share, ShareRowExclusive, Exclusive, AccessExclusive],
            Share => &[RowExclusive, ShareUpdateExclusive, ShareRowExclusive, Exclusive, AccessExclusive],
            ShareRowExclusive => {
                &[RowExclusive, ShareUpdateExclusive, Share, ShareRowExclusive, Exclusive, AccessExclusive]
            }
            Exclusive => &[
                RowShare,
                RowExclusive,
                ShareUpdateExclusive,
                Share,
                ShareRowExclusive,
                Exclusive,
                AccessExclusive,
            ],
            AccessExclusive => &[
                AccessShare,
                RowShare,
                RowExclusive,
                ShareUpdateExclusive,
                Share,
                ShareRowExclusive,
                Exclusive,
                AccessExclusive,
            ],
            _ => &[],
        }
    }

    /// Expected row-level conflicts (PostgreSQL docs Table 13.3).
    fn expected_row(m: LockMode) -> &'static [LockMode] {
        match m {
            ForKeyShare => &[ForUpdate],
            ForShare => &[ForNoKeyUpdate, ForUpdate],
            ForNoKeyUpdate => &[ForShare, ForNoKeyUpdate, ForUpdate],
            ForUpdate => &[ForKeyShare, ForShare, ForNoKeyUpdate, ForUpdate],
            _ => &[],
        }
    }

    fn all_modes() -> Vec<LockMode> {
        LockMode::TABLE_MODES.iter().chain(LockMode::ROW_MODES.iter()).copied().collect()
    }

    #[test]
    fn conflict_matrix_is_symmetric_over_all_pairs() {
        let all = all_modes();
        assert_eq!(all.len(), 12);
        let mut pairs = 0;
        for &a in &all {
            for &b in &all {
                assert_eq!(a.conflicts_with(b).unwrap(), b.conflicts_with(a).unwrap(), "{a:?} vs {b:?}");
                pairs += 1;
            }
        }
        assert_eq!(pairs, 144);
    }

    #[test]
    fn table_matrix_matches_lock_c() {
        for &a in &LockMode::TABLE_MODES {
            for &b in &LockMode::TABLE_MODES {
                let want = expected_table(a).contains(&b);
                assert_eq!(a.conflicts_with(b).unwrap(), want, "{a:?} vs {b:?}");
            }
        }
    }

    #[test]
    fn row_matrix_matches_pg_docs() {
        for &a in &LockMode::ROW_MODES {
            for &b in &LockMode::ROW_MODES {
                let want = expected_row(a).contains(&b);
                assert_eq!(a.conflicts_with(b).unwrap(), want, "{a:?} vs {b:?}");
            }
        }
    }

    #[test]
    fn mixed_table_and_row_pairs_do_not_conflict() {
        for &a in &LockMode::TABLE_MODES {
            for &b in &LockMode::ROW_MODES {
                assert!(!a.conflicts_with(b).unwrap());
                assert!(!b.conflicts_with(a).unwrap());
            }
        }
    }

    #[test]
    fn spot_checks() {
        assert!(AccessShare.conflicts_with(AccessExclusive).unwrap());
        assert!(!AccessShare.conflicts_with(AccessShare).unwrap());
        assert!(!AccessShare.conflicts_with(Exclusive).unwrap());
        assert!(!RowExclusive.conflicts_with(RowExclusive).unwrap());
        assert!(!ForKeyShare.conflicts_with(ForNoKeyUpdate).unwrap());
        assert!(ForKeyShare.conflicts_with(ForUpdate).unwrap());
        assert!(!ForShare.conflicts_with(ForShare).unwrap());
        assert!(ForNoKeyUpdate.conflicts_with(ForNoKeyUpdate).unwrap());
    }

    #[test]
    fn pg_names() {
        let names: Vec<&str> = LockMode::TABLE_MODES.iter().map(|m| m.pg_name().unwrap()).collect();
        assert_eq!(
            names,
            [
                "AccessShareLock",
                "RowShareLock",
                "RowExclusiveLock",
                "ShareUpdateExclusiveLock",
                "ShareLock",
                "ShareRowExclusiveLock",
                "ExclusiveLock",
                "AccessExclusiveLock",
            ]
        );
        assert_eq!(ForKeyShare.pg_name().unwrap(), "AccessShareLock");
        assert_eq!(ForShare.pg_name().unwrap(), "RowShareLock");
        assert_eq!(ForNoKeyUpdate.pg_name().unwrap(), "ExclusiveLock");
        assert_eq!(ForUpdate.pg_name().unwrap(), "AccessExclusiveLock");
    }

    #[test]
    fn parse_table_mode_words() {
        assert_eq!(LockMode::parse_table_mode("access share").unwrap(), Some(AccessShare));
        assert_eq!(LockMode::parse_table_mode("ROW SHARE").unwrap(), Some(RowShare));
        assert_eq!(LockMode::parse_table_mode("Row Exclusive").unwrap(), Some(RowExclusive));
        assert_eq!(LockMode::parse_table_mode("SHARE UPDATE EXCLUSIVE").unwrap(), Some(ShareUpdateExclusive));
        assert_eq!(LockMode::parse_table_mode("SHARE").unwrap(), Some(Share));
        assert_eq!(LockMode::parse_table_mode("SHARE ROW EXCLUSIVE").unwrap(), Some(ShareRowExclusive));
        assert_eq!(LockMode::parse_table_mode("EXCLUSIVE").unwrap(), Some(Exclusive));
        assert_eq!(LockMode::parse_table_mode("ACCESS EXCLUSIVE").unwrap(), Some(AccessExclusive));
        assert_eq!(LockMode::parse_table_mode("ROW").unwrap(), None);
        assert_eq!(LockMode::parse_table_mode("SHARE EXCLUSIVE").unwrap(), None);
        assert_eq!(LockMode::parse_table_mode("SHARE ROW EXCLUSIVE EXTRA").unwrap(), None);
        assert_eq!(LockMode::parse_table_mode("").unwrap(), None);
        assert_eq!(LockMode::parse_table_mode("FOR UPDATE").unwrap(), None);
    }
}
