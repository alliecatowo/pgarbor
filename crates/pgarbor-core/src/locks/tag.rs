//! Lockable object identities. Lease CORE-08 (`describe`, `locktype`, `wait_event`).
//!
//! Every tag carries the branch AND the logical database: two branches (or two databases of one
//! branch) never contend, matching PG where lock tags include the database OID.
use pgarbor_common::{BranchId, Oid, PgError, RelId, Result};

// CONTRACT
#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum LockTag {
    /// Table-level lock on a relation (DDL, LOCK TABLE, DML's RowExclusive...).
    Relation { branch: BranchId, database: Oid, relid: RelId },
    /// Row/key lock: an exact key of the branch data tree (table row key `[relid][rowid]`, unique
    /// index key, catalog row key). Row modes only.
    Key { branch: BranchId, database: Oid, key: Vec<u8> },
    /// pg_advisory_* (PG LOCKTAG_ADVISORY: key1 = classid, key2 = objid, field4 = objsubid:
    /// 1 for the bigint form, 2 for the (int4, int4) form).
    Advisory { branch: BranchId, database: Oid, key1: u32, key2: u32, field4: u16 },
    /// Non-relation catalog object (schema, type, ...) for DDL serialization (PG LOCKTAG_OBJECT).
    Object { branch: BranchId, database: Oid, classid: Oid, objid: Oid, objsubid: i32 },
}

impl LockTag {
    pub fn branch(&self) -> BranchId {
        match self {
            LockTag::Relation { branch, .. }
            | LockTag::Key { branch, .. }
            | LockTag::Advisory { branch, .. }
            | LockTag::Object { branch, .. } => *branch,
        }
    }

    pub fn database(&self) -> Oid {
        match self {
            LockTag::Relation { database, .. }
            | LockTag::Key { database, .. }
            | LockTag::Advisory { database, .. }
            | LockTag::Object { database, .. } => *database,
        }
    }

    /// Relation OID a Key tag belongs to: first 4 key bytes big-endian if >= 16384 (ARCHITECTURE §4).
    pub fn key_relation(&self) -> Option<RelId> {
        match self {
            LockTag::Key { key, .. } => {
                let p = key.get(0..4)?;
                let r = u32::from_be_bytes([p[0], p[1], p[2], p[3]]);
                (r >= pgarbor_common::FIRST_NORMAL_OID).then_some(r)
            }
            LockTag::Relation { relid, .. } => Some(*relid),
            _ => None,
        }
    }

    /// `pg_locks.locktype`: Relation → `relation`, Key → `tuple`, Advisory → `advisory`,
    /// Object → `object`.
    #[allow(unused_variables)]
    pub fn locktype(&self) -> Result<&'static str> {
        Err(PgError::unimplemented("CORE-08", "lock tag type names"))
    }

    /// Object description in deadlock reports (lmgr.c DescribeLockTag):
    /// Relation → `relation <relid> of database <db>`;
    /// Advisory → `advisory lock [<db>,<key1>,<key2>,<field4>]`;
    /// Object → `object <objid> of class <classid> of database <db>`;
    /// Key → `tuple of relation <relid> of database <db>` (we have no ctid). NOTE: waits on Key
    /// tags are REPORTED as `ShareLock on transaction <holder txn id>` by the deadlock module, the
    /// way PostgreSQL reports row-lock waits; this description is used only for pg_locks/debug.
    #[allow(unused_variables)]
    pub fn describe(&self) -> Result<String> {
        Err(PgError::unimplemented("CORE-08", "lock tag description"))
    }

    /// `pg_stat_activity.wait_event` while waiting on this tag: Relation → `relation`,
    /// Key → `transactionid`, Advisory → `advisory`, Object → `object` (type is always `Lock`).
    #[allow(unused_variables)]
    pub fn wait_event(&self) -> Result<&'static str> {
        Err(PgError::unimplemented("CORE-08", "lock wait event names"))
    }
}
