//! `BranchRecord`: the value stored under directory key `B` + branch_id (ARCHITECTURE §6).
//! Lease BR-01 (`encode`, `decode`). Everything else here is implemented.
//!
//! # Byte format v1 (CONTRACT, little-endian, no padding)
//!
//! ```text
//! off  len  field
//! 0    1    record_version = 1 (RECORD_FORMAT_VERSION)
//! 1    16   id
//! 17   1    state: 0 = Active, 1 = Deleted
//! 18   1    flags: bit0 = has_parent, bit1 = roots_retained (Deleted only; see below); other bits 0
//! 19   16   parent id (all zero when !has_parent)
//! 35   8    data_root (PageId; 0 = empty tree) — the "postgres" database (oid 5) tree
//! 43   8    created_at_seq (CommitSeq of the commit that created the branch)
//! 51   8    head_seq (CommitSeq of the last commit that changed any root of this branch)
//! 59   8    created_unix_ms
//! 67   8    updated_unix_ms (last root/name/state change)
//! 75   8    generation (starts at 1; +1 on every reset, delete and restore)
//! 83   8    deleted_at_seq (0 when Active)
//! 91   8    deleted_unix_ms (0 when Active)
//! 99   2    name_len (u16), then name_len bytes UTF-8 name
//! ..   4    snapshot_count (u32), then per snapshot:
//!             2 label_len (u16) | label bytes | 8 root | 8 created_seq | 8 created_unix_ms | 1 kind
//! ..   4    database_count (u32), then per extra database:
//!             4 db_oid (u32) | 8 root
//! end       no trailing bytes allowed
//! ```
//!
//! Decoding rules (each violation → XX001 via `error::directory_corrupt("<reason>")`, never panic):
//! 1. empty input → "empty branch record"; version != 1 → `error::unsupported_record_format(v)`;
//! 2. truncated anywhere → "truncated branch record";
//! 3. state byte > 1, unknown flag bits, has_parent=0 with non-zero parent bytes,
//!    snapshot kind > 2, non-UTF-8 name/label, trailing bytes → "invalid branch record: <what>";
//! 4. Active with deleted_at_seq != 0, or roots_retained set while Active → invalid;
//! 5. extra database oid == 5 (`POSTGRES_DATABASE_OID`) or duplicated oid → invalid;
//! 6. `decode(encode(r)) == r` for every record satisfying `BranchRecord::validate`.
//!
//! Deleted branches: `roots_retained = true` means the tombstone still holds its roots (restorable;
//! `BranchRootResolver` keeps them live). `false` means the roots were dropped: `data_root` = 0,
//! no snapshots, no databases.
//!
//! Multiple PostgreSQL databases per branch (RFC 001 §9 "one branch may hold multiple logical PG
//! databases"): each database is its OWN data tree. `data_root` is database oid 5 ("postgres"),
//! which ALSO holds cluster-shared catalog rows (roles, pg_database) in v1. `extra_databases`
//! (always empty in v1) is reserved for CREATE DATABASE; reading it is supported now so the format
//! never needs a version bump for it. See `contracts/crates/pgarbor-branches.md` §Databases.
use crate::error;
use pgarbor_common::{BranchId, CommitSeq, Oid, PageId, PgError, Result};

/// Current record format version byte.
pub const RECORD_FORMAT_VERSION: u8 = 1;
/// PostgreSQL's `postgres` database OID (pg_database.oid in a fresh PG18 cluster).
pub const POSTGRES_DATABASE_OID: Oid = 5;
/// Per-branch named snapshot limit (54000 beyond it).
pub const MAX_SNAPSHOTS_PER_BRANCH: usize = 1024;

pub const FLAG_HAS_PARENT: u8 = 0x01;
pub const FLAG_ROOTS_RETAINED: u8 = 0x02;

// CONTRACT
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum BranchState {
    Active,
    Deleted,
}

impl BranchState {
    pub fn as_str(&self) -> &'static str {
        match self {
            BranchState::Active => "active",
            BranchState::Deleted => "deleted",
        }
    }
}

// CONTRACT
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SnapshotKind {
    /// `pgarbor branch snapshot <branch> <label>` (kind byte 0).
    User,
    /// Created by `reset` with `recovery_snapshot` (kind byte 1).
    ResetRecovery,
    /// Copied from the source branch at create (`copy_snapshots`) or export (kind byte 2).
    Inherited,
}

impl SnapshotKind {
    pub fn to_byte(self) -> u8 {
        match self {
            SnapshotKind::User => 0,
            SnapshotKind::ResetRecovery => 1,
            SnapshotKind::Inherited => 2,
        }
    }
    pub fn from_byte(b: u8) -> Option<SnapshotKind> {
        match b {
            0 => Some(SnapshotKind::User),
            1 => Some(SnapshotKind::ResetRecovery),
            2 => Some(SnapshotKind::Inherited),
            _ => None,
        }
    }
}

// CONTRACT
/// A named, immutable root kept alive by the branch (BRANCH-06).
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize)]
pub struct NamedSnapshot {
    pub label: String,
    /// Root of the "postgres" database tree at snapshot time.
    pub root: PageId,
    pub created_seq: CommitSeq,
    pub created_unix_ms: u64,
    pub kind: SnapshotKind,
}

// CONTRACT
/// An additional logical database's tree (reserved for CREATE DATABASE; empty in v1).
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize)]
pub struct DatabaseRoot {
    pub db_oid: Oid,
    pub root: PageId,
}

// CONTRACT
/// One branch (ARCHITECTURE §6 field list plus the amendment `pgarbor-branches-01` fields:
/// `updated_unix_ms`, `generation`, `deleted_at_seq`, `deleted_unix_ms`, `roots_retained`,
/// `extra_databases`, typed snapshots).
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize)]
pub struct BranchRecord {
    pub id: BranchId,
    pub name: String,
    pub state: BranchState,
    pub parent: Option<BranchId>,
    pub data_root: PageId,
    pub created_at_seq: CommitSeq,
    pub head_seq: CommitSeq,
    pub created_unix_ms: u64,
    pub updated_unix_ms: u64,
    /// Bumped by reset/delete/restore. pgarbor-core captures it at transaction start and refuses
    /// to commit onto a record whose generation changed (never apply an overlay to a foreign root).
    pub generation: u64,
    pub deleted_at_seq: CommitSeq,
    pub deleted_unix_ms: u64,
    pub roots_retained: bool,
    pub snapshots: Vec<NamedSnapshot>,
    pub extra_databases: Vec<DatabaseRoot>,
}

impl BranchRecord {
    /// A fresh active record (generation 1, no snapshots, no extra databases).
    pub fn new_active(
        id: BranchId,
        name: &str,
        parent: Option<BranchId>,
        data_root: PageId,
        seq: CommitSeq,
        now_unix_ms: u64,
    ) -> BranchRecord {
        BranchRecord {
            id,
            name: name.to_string(),
            state: BranchState::Active,
            parent,
            data_root,
            created_at_seq: seq,
            head_seq: seq,
            created_unix_ms: now_unix_ms,
            updated_unix_ms: now_unix_ms,
            generation: 1,
            deleted_at_seq: CommitSeq(0),
            deleted_unix_ms: 0,
            roots_retained: false,
            snapshots: Vec::new(),
            extra_databases: Vec::new(),
        }
    }

    pub fn is_active(&self) -> bool {
        self.state == BranchState::Active
    }

    /// Root of logical database `db_oid`: oid 5 → `data_root`; else the `extra_databases` entry.
    pub fn database_root(&self, db_oid: Oid) -> Option<PageId> {
        if db_oid == POSTGRES_DATABASE_OID {
            return Some(self.data_root);
        }
        self.extra_databases.iter().find(|d| d.db_oid == db_oid).map(|d| d.root)
    }

    /// Replace the root of database `db_oid`. Returns false (and changes nothing) when the
    /// database does not exist in this branch.
    pub fn set_database_root(&mut self, db_oid: Oid, root: PageId) -> bool {
        if db_oid == POSTGRES_DATABASE_OID {
            self.data_root = root;
            return true;
        }
        match self.extra_databases.iter_mut().find(|d| d.db_oid == db_oid) {
            Some(d) => {
                d.root = root;
                true
            }
            None => false,
        }
    }

    pub fn snapshot(&self, label: &str) -> Option<&NamedSnapshot> {
        self.snapshots.iter().find(|s| s.label == label)
    }

    /// Every non-NONE root this record keeps alive, with GC labels:
    /// `branch <name>`, `branch <name> database <oid>`, `branch <name> snapshot <label>`;
    /// for a retained tombstone the prefix is `deleted branch <name> (<id>)` instead of
    /// `branch <name>`. Dropped tombstones yield nothing.
    pub fn live_roots(&self) -> Vec<(String, PageId)> {
        if !self.is_active() && !self.roots_retained {
            return Vec::new();
        }
        let prefix = if self.is_active() {
            format!("branch {}", self.name)
        } else {
            format!("deleted branch {} ({})", self.name, self.id)
        };
        let mut out = Vec::new();
        if !self.data_root.is_none() {
            out.push((prefix.clone(), self.data_root));
        }
        for d in &self.extra_databases {
            if !d.root.is_none() {
                out.push((format!("{prefix} database {}", d.db_oid), d.root));
            }
        }
        for s in &self.snapshots {
            if !s.root.is_none() {
                out.push((format!("{prefix} snapshot {}", s.label), s.root));
            }
        }
        out
    }

    /// Structural invariants (the ones `decode` enforces) as an error message, `None` if valid.
    pub fn validate(&self) -> Option<String> {
        if self.name.len() > u16::MAX as usize {
            return Some("name too long".into());
        }
        if self.is_active() && (self.deleted_at_seq.0 != 0 || self.roots_retained) {
            return Some("active record with deletion fields".into());
        }
        let mut seen = std::collections::HashSet::new();
        for d in &self.extra_databases {
            if d.db_oid == POSTGRES_DATABASE_OID || !seen.insert(d.db_oid) {
                return Some(format!("bad extra database oid {}", d.db_oid));
            }
        }
        None
    }

    /// Serialize per the v1 byte format in the module docs.
    /// Errors: `validate()` failure → `PgError::internal("invalid branch record: <msg>")`.
    #[allow(unused_variables)]
    pub fn encode(&self) -> Result<Vec<u8>> {
        Err(PgError::unimplemented("BR-01", "branch record encoding"))
    }

    /// Parse per the v1 byte format and decoding rules in the module docs.
    #[allow(unused_variables)]
    pub fn decode(bytes: &[u8]) -> Result<BranchRecord> {
        Err(PgError::unimplemented("BR-01", "branch record decoding"))
    }
}

/// Shorthand used by decoders: XX001 `branch directory is corrupt: <what>`.
pub fn corrupt(what: impl AsRef<str>) -> PgError {
    error::directory_corrupt(what)
}
