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
    pub fn encode(&self) -> Result<Vec<u8>> {
        if let Some(msg) = self.validate() {
            return Err(PgError::internal(format!("invalid branch record: {msg}")));
        }
        let name_len = u16::try_from(self.name.len())
            .map_err(|_| PgError::internal("invalid branch record: name too long"))?;
        let snapshot_count = u32::try_from(self.snapshots.len())
            .map_err(|_| PgError::internal("invalid branch record: too many snapshots"))?;
        let database_count = u32::try_from(self.extra_databases.len())
            .map_err(|_| PgError::internal("invalid branch record: too many databases"))?;

        let mut out = Vec::with_capacity(
            FIXED_PREFIX_LEN
                + self.name.len()
                + 4
                + self.snapshots.len() * MIN_SNAPSHOT_LEN
                + self.snapshots.iter().map(|s| s.label.len()).sum::<usize>()
                + 4
                + self.extra_databases.len() * DATABASE_ENTRY_LEN,
        );
        out.push(RECORD_FORMAT_VERSION);
        out.extend_from_slice(&self.id.0);
        out.push(match self.state {
            BranchState::Active => 0,
            BranchState::Deleted => 1,
        });
        let mut flags = 0u8;
        if self.parent.is_some() {
            flags |= FLAG_HAS_PARENT;
        }
        if self.roots_retained {
            flags |= FLAG_ROOTS_RETAINED;
        }
        out.push(flags);
        match self.parent {
            Some(p) => out.extend_from_slice(&p.0),
            None => out.extend_from_slice(&[0u8; 16]),
        }
        out.extend_from_slice(&self.data_root.0.to_le_bytes());
        out.extend_from_slice(&self.created_at_seq.0.to_le_bytes());
        out.extend_from_slice(&self.head_seq.0.to_le_bytes());
        out.extend_from_slice(&self.created_unix_ms.to_le_bytes());
        out.extend_from_slice(&self.updated_unix_ms.to_le_bytes());
        out.extend_from_slice(&self.generation.to_le_bytes());
        out.extend_from_slice(&self.deleted_at_seq.0.to_le_bytes());
        out.extend_from_slice(&self.deleted_unix_ms.to_le_bytes());
        out.extend_from_slice(&name_len.to_le_bytes());
        out.extend_from_slice(self.name.as_bytes());

        out.extend_from_slice(&snapshot_count.to_le_bytes());
        for s in &self.snapshots {
            let label_len = u16::try_from(s.label.len())
                .map_err(|_| PgError::internal("invalid branch record: snapshot label too long"))?;
            out.extend_from_slice(&label_len.to_le_bytes());
            out.extend_from_slice(s.label.as_bytes());
            out.extend_from_slice(&s.root.0.to_le_bytes());
            out.extend_from_slice(&s.created_seq.0.to_le_bytes());
            out.extend_from_slice(&s.created_unix_ms.to_le_bytes());
            out.push(s.kind.to_byte());
        }

        out.extend_from_slice(&database_count.to_le_bytes());
        for d in &self.extra_databases {
            out.extend_from_slice(&d.db_oid.to_le_bytes());
            out.extend_from_slice(&d.root.0.to_le_bytes());
        }
        Ok(out)
    }

    /// Parse per the v1 byte format and decoding rules in the module docs.
    pub fn decode(bytes: &[u8]) -> Result<BranchRecord> {
        let Some(&version) = bytes.first() else {
            return Err(corrupt("empty branch record"));
        };
        if version != RECORD_FORMAT_VERSION {
            return Err(error::unsupported_record_format(version));
        }
        let mut c = Cursor::new(bytes);
        c.u8()?; // version, already checked
        let id = BranchId(c.array()?);
        let state = match c.u8()? {
            0 => BranchState::Active,
            1 => BranchState::Deleted,
            other => return Err(invalid_record(format!("unknown state {other}"))),
        };
        let flags = c.u8()?;
        if flags & !(FLAG_HAS_PARENT | FLAG_ROOTS_RETAINED) != 0 {
            return Err(invalid_record(format!("unknown flag bits {flags:#04x}")));
        }
        let parent_bytes: [u8; 16] = c.array()?;
        let parent = if flags & FLAG_HAS_PARENT != 0 {
            Some(BranchId(parent_bytes))
        } else if parent_bytes != [0u8; 16] {
            return Err(invalid_record("parent id is set but has_parent flag is clear"));
        } else {
            None
        };
        let roots_retained = flags & FLAG_ROOTS_RETAINED != 0;

        let data_root = PageId(c.u64()?);
        let created_at_seq = CommitSeq(c.u64()?);
        let head_seq = CommitSeq(c.u64()?);
        let created_unix_ms = c.u64()?;
        let updated_unix_ms = c.u64()?;
        let generation = c.u64()?;
        let deleted_at_seq = CommitSeq(c.u64()?);
        let deleted_unix_ms = c.u64()?;

        let name_len = usize::from(c.u16()?);
        let name = utf8_string(c.take(name_len)?, "name is not valid UTF-8")?;

        let snapshot_count = usize::try_from(c.u32()?).unwrap_or(usize::MAX);
        let mut snapshots = Vec::with_capacity(snapshot_count.min(c.remaining() / MIN_SNAPSHOT_LEN));
        for _ in 0..snapshot_count {
            let label_len = usize::from(c.u16()?);
            let label = utf8_string(c.take(label_len)?, "snapshot label is not valid UTF-8")?;
            let root = PageId(c.u64()?);
            let created_seq = CommitSeq(c.u64()?);
            let created_unix_ms = c.u64()?;
            let kind_byte = c.u8()?;
            let kind = SnapshotKind::from_byte(kind_byte)
                .ok_or_else(|| invalid_record(format!("unknown snapshot kind {kind_byte}")))?;
            snapshots.push(NamedSnapshot { label, root, created_seq, created_unix_ms, kind });
        }

        let database_count = usize::try_from(c.u32()?).unwrap_or(usize::MAX);
        let mut extra_databases = Vec::with_capacity(database_count.min(c.remaining() / DATABASE_ENTRY_LEN));
        for _ in 0..database_count {
            let db_oid = c.u32()?;
            let root = PageId(c.u64()?);
            extra_databases.push(DatabaseRoot { db_oid, root });
        }

        if c.remaining() != 0 {
            return Err(invalid_record("trailing bytes after the last field"));
        }

        let record = BranchRecord {
            id,
            name,
            state,
            parent,
            data_root,
            created_at_seq,
            head_seq,
            created_unix_ms,
            updated_unix_ms,
            generation,
            deleted_at_seq,
            deleted_unix_ms,
            roots_retained,
            snapshots,
            extra_databases,
        };
        // Rules 4 and 5 (active-with-deletion fields, bad or duplicate extra database oids).
        if let Some(msg) = record.validate() {
            return Err(invalid_record(msg));
        }
        Ok(record)
    }
}

/// Shorthand used by decoders: XX001 `branch directory is corrupt: <what>`.
pub fn corrupt(what: impl AsRef<str>) -> PgError {
    error::directory_corrupt(what)
}

/// Fixed-size prefix of a v1 record up to and excluding `name_len` (version..deleted_unix_ms).
const FIXED_PREFIX_LEN: usize = 99;
/// Smallest possible snapshot entry: label_len(2) + root(8) + created_seq(8) + created_unix_ms(8) + kind(1).
const MIN_SNAPSHOT_LEN: usize = 27;
/// One extra-database entry: db_oid(4) + root(8).
const DATABASE_ENTRY_LEN: usize = 12;

/// XX001 `branch directory is corrupt: invalid branch record: <what>`.
fn invalid_record(what: impl AsRef<str>) -> PgError {
    corrupt(format!("invalid branch record: {}", what.as_ref()))
}

fn truncated() -> PgError {
    corrupt("truncated branch record")
}

fn utf8_string(bytes: &[u8], what: &str) -> Result<String> {
    std::str::from_utf8(bytes).map(str::to_owned).map_err(|_| invalid_record(what))
}

/// Bounds-checked little-endian reader. Never indexes; every short read is XX001.
struct Cursor<'a> {
    buf: &'a [u8],
    pos: usize,
}

impl<'a> Cursor<'a> {
    fn new(buf: &'a [u8]) -> Cursor<'a> {
        Cursor { buf, pos: 0 }
    }

    fn remaining(&self) -> usize {
        self.buf.len().saturating_sub(self.pos)
    }

    fn take(&mut self, n: usize) -> Result<&'a [u8]> {
        let buf: &'a [u8] = self.buf;
        let end = self.pos.checked_add(n).ok_or_else(truncated)?;
        let slice = buf.get(self.pos..end).ok_or_else(truncated)?;
        self.pos = end;
        Ok(slice)
    }

    fn array<const N: usize>(&mut self) -> Result<[u8; N]> {
        let slice = self.take(N)?;
        <[u8; N]>::try_from(slice).map_err(|_| truncated())
    }

    fn u8(&mut self) -> Result<u8> {
        let [b] = self.array::<1>()?;
        Ok(b)
    }

    fn u16(&mut self) -> Result<u16> {
        Ok(u16::from_le_bytes(self.array()?))
    }

    fn u32(&mut self) -> Result<u32> {
        Ok(u32::from_le_bytes(self.array()?))
    }

    fn u64(&mut self) -> Result<u64> {
        Ok(u64::from_le_bytes(self.array()?))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pgarbor_common::SqlState;

    fn bid(b: u8) -> BranchId {
        BranchId([b; 16])
    }

    fn minimal_record() -> BranchRecord {
        BranchRecord::new_active(BranchId([1; 16]), "main", None, PageId(7), CommitSeq(2), 1000)
    }

    fn full_record() -> BranchRecord {
        let mut r = BranchRecord::new_active(bid(1), "feature/x-1", Some(bid(9)), PageId(42), CommitSeq(5), 1000);
        r.head_seq = CommitSeq(12);
        r.updated_unix_ms = 1500;
        r.generation = 4;
        r.snapshots = vec![
            NamedSnapshot {
                label: "before".into(),
                root: PageId(10),
                created_seq: CommitSeq(6),
                created_unix_ms: 1100,
                kind: SnapshotKind::User,
            },
            NamedSnapshot {
                label: "reset.1".into(),
                root: PageId(11),
                created_seq: CommitSeq(7),
                created_unix_ms: 1200,
                kind: SnapshotKind::ResetRecovery,
            },
            NamedSnapshot {
                label: "inherit/é".into(),
                root: PageId(12),
                created_seq: CommitSeq(8),
                created_unix_ms: 1300,
                kind: SnapshotKind::Inherited,
            },
        ];
        r.extra_databases = vec![
            DatabaseRoot { db_oid: 16384, root: PageId(20) },
            DatabaseRoot { db_oid: 16385, root: PageId(0) },
        ];
        r
    }

    fn deleted_retained_record() -> BranchRecord {
        let mut r = full_record();
        r.state = BranchState::Deleted;
        r.deleted_at_seq = CommitSeq(13);
        r.deleted_unix_ms = 1600;
        r.roots_retained = true;
        r
    }

    fn deleted_dropped_record() -> BranchRecord {
        let mut r = deleted_retained_record();
        r.roots_retained = false;
        r.data_root = PageId::NONE;
        r.snapshots.clear();
        r.extra_databases.clear();
        r
    }

    fn roundtrip(r: &BranchRecord) {
        let bytes = r.encode().expect("encode");
        let back = BranchRecord::decode(&bytes).expect("decode");
        assert_eq!(&back, r);
    }

    #[test]
    fn roundtrip_minimal() {
        roundtrip(&minimal_record());
    }

    #[test]
    fn roundtrip_with_parent_snapshots_and_databases() {
        roundtrip(&full_record());
    }

    #[test]
    fn roundtrip_deleted_retained() {
        roundtrip(&deleted_retained_record());
    }

    #[test]
    fn roundtrip_deleted_dropped() {
        roundtrip(&deleted_dropped_record());
    }

    #[test]
    fn minimal_byte_layout() {
        let b = minimal_record().encode().expect("encode");
        assert_eq!(b.len(), 99 + 2 + 4 + 4 + 4);
        assert_eq!(b[0], 1);
        assert_eq!(&b[1..17], &[1u8; 16]);
        assert_eq!(b[17], 0);
        assert_eq!(b[18], 0);
        assert_eq!(&b[19..35], &[0u8; 16]);
        assert_eq!(&b[35..43], &7u64.to_le_bytes());
        assert_eq!(&b[43..51], &2u64.to_le_bytes());
        assert_eq!(&b[51..59], &2u64.to_le_bytes());
        assert_eq!(&b[75..83], &1u64.to_le_bytes());
        assert_eq!(&b[83..99], &[0u8; 16]);
        assert_eq!(&b[99..101], &4u16.to_le_bytes());
        assert_eq!(&b[101..105], b"main");
        assert_eq!(&b[105..109], &0u32.to_le_bytes());
        assert_eq!(&b[109..113], &0u32.to_le_bytes());
    }

    #[test]
    fn every_strict_prefix_is_xx001() {
        for r in [minimal_record(), full_record(), deleted_retained_record()] {
            let bytes = r.encode().expect("encode");
            for len in 0..bytes.len() {
                let err = BranchRecord::decode(&bytes[..len]).expect_err("prefix must not decode");
                assert_eq!(err.sqlstate, SqlState::DATA_CORRUPTED, "prefix length {len}");
            }
        }
    }

    #[test]
    fn wrong_version_message() {
        let mut b = minimal_record().encode().expect("encode");
        b[0] = 2;
        let err = BranchRecord::decode(&b).expect_err("version 2");
        assert_eq!(err.sqlstate, SqlState::DATA_CORRUPTED);
        assert_eq!(err.message, "unsupported branch record format version 2");
    }

    #[test]
    fn empty_input_rejected() {
        let err = BranchRecord::decode(&[]).expect_err("empty");
        assert_eq!(err.sqlstate, SqlState::DATA_CORRUPTED);
        assert_eq!(err.message, "branch directory is corrupt: empty branch record");
    }

    #[test]
    fn trailing_byte_rejected() {
        let mut b = full_record().encode().expect("encode");
        b.push(0);
        let err = BranchRecord::decode(&b).expect_err("trailing");
        assert_eq!(err.sqlstate, SqlState::DATA_CORRUPTED);
    }

    #[test]
    fn bad_state_flags_and_parent_rejected() {
        let good = minimal_record().encode().expect("encode");

        let mut b = good.clone();
        b[17] = 2;
        assert_eq!(BranchRecord::decode(&b).expect_err("state").sqlstate, SqlState::DATA_CORRUPTED);

        let mut b = good.clone();
        b[18] = 0x04;
        assert_eq!(BranchRecord::decode(&b).expect_err("unknown flag").sqlstate, SqlState::DATA_CORRUPTED);

        // roots_retained on an active record
        let mut b = good.clone();
        b[18] = FLAG_ROOTS_RETAINED;
        assert_eq!(BranchRecord::decode(&b).expect_err("active retained").sqlstate, SqlState::DATA_CORRUPTED);

        // parent bytes without has_parent
        let mut b = good.clone();
        b[19] = 1;
        assert_eq!(BranchRecord::decode(&b).expect_err("stray parent").sqlstate, SqlState::DATA_CORRUPTED);

        // active record carrying deleted_at_seq
        let mut b = good.clone();
        b[83] = 1;
        assert_eq!(BranchRecord::decode(&b).expect_err("active deleted_at").sqlstate, SqlState::DATA_CORRUPTED);
    }

    #[test]
    fn bad_snapshot_kind_rejected() {
        let mut r = minimal_record();
        r.snapshots.push(NamedSnapshot {
            label: "s".into(),
            root: PageId(3),
            created_seq: CommitSeq(3),
            created_unix_ms: 1,
            kind: SnapshotKind::User,
        });
        let mut b = r.encode().expect("encode");
        // kind byte is the 5th byte from the end (no extra databases follow the count of 0 at the end).
        let kind_index = b.len() - 5;
        assert_eq!(b[kind_index], 0);
        b[kind_index] = 3;
        assert_eq!(BranchRecord::decode(&b).expect_err("kind").sqlstate, SqlState::DATA_CORRUPTED);
    }

    #[test]
    fn non_utf8_name_rejected() {
        let mut b = minimal_record().encode().expect("encode");
        b[101] = 0xff;
        assert_eq!(BranchRecord::decode(&b).expect_err("utf8").sqlstate, SqlState::DATA_CORRUPTED);
    }

    #[test]
    fn duplicate_and_postgres_database_oids_rejected() {
        let head = minimal_record().encode().expect("encode");
        let head = &head[..109];

        let mut dup = head.to_vec();
        dup.extend_from_slice(&2u32.to_le_bytes());
        for _ in 0..2 {
            dup.extend_from_slice(&16384u32.to_le_bytes());
            dup.extend_from_slice(&1u64.to_le_bytes());
        }
        assert_eq!(BranchRecord::decode(&dup).expect_err("dup").sqlstate, SqlState::DATA_CORRUPTED);

        let mut pg = head.to_vec();
        pg.extend_from_slice(&1u32.to_le_bytes());
        pg.extend_from_slice(&POSTGRES_DATABASE_OID.to_le_bytes());
        pg.extend_from_slice(&1u64.to_le_bytes());
        assert_eq!(BranchRecord::decode(&pg).expect_err("oid 5").sqlstate, SqlState::DATA_CORRUPTED);
    }

    #[test]
    fn encode_rejects_invalid_records() {
        let mut r = minimal_record();
        r.roots_retained = true;
        let err = r.encode().expect_err("active retained");
        assert_eq!(err.sqlstate, SqlState::INTERNAL_ERROR);
        assert!(err.message.starts_with("invalid branch record: "));
    }
}
