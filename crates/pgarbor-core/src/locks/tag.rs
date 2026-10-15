//! Lockable object identities. Lease CORE-08 (`describe`, `locktype`, `wait_event`).
//!
//! Every tag carries the branch AND the logical database: two branches (or two databases of one
//! branch) never contend, matching PG where lock tags include the database OID.
use pgarbor_common::{BranchId, Oid, RelId, Result};

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
    pub fn locktype(&self) -> Result<&'static str> {
        Ok(match self {
            LockTag::Relation { .. } => "relation",
            LockTag::Key { .. } => "tuple",
            LockTag::Advisory { .. } => "advisory",
            LockTag::Object { .. } => "object",
        })
    }

    /// Object description in deadlock reports (lmgr.c DescribeLockTag):
    /// Relation → `relation <relid> of database <db>`;
    /// Advisory → `advisory lock [<db>,<key1>,<key2>,<field4>]`;
    /// Object → `object <objid> of class <classid> of database <db>`;
    /// Key → `tuple of relation <relid> of database <db>` (we have no ctid). NOTE: waits on Key
    /// tags are REPORTED as `ShareLock on transaction <holder txn id>` by the deadlock module, the
    /// way PostgreSQL reports row-lock waits; this description is used only for pg_locks/debug.
    pub fn describe(&self) -> Result<String> {
        Ok(match self {
            LockTag::Relation { database, relid, .. } => {
                format!("relation {relid} of database {database}")
            }
            LockTag::Key { database, .. } => {
                // Same relid as `pg_locks.relation`; 0 when the key has no normal relation prefix.
                let relid = self.key_relation().unwrap_or(0);
                format!("tuple of relation {relid} of database {database}")
            }
            LockTag::Advisory { database, key1, key2, field4, .. } => {
                format!("advisory lock [{database},{key1},{key2},{field4}]")
            }
            LockTag::Object { database, classid, objid, .. } => {
                format!("object {objid} of class {classid} of database {database}")
            }
        })
    }

    /// `pg_stat_activity.wait_event` while waiting on this tag: Relation → `relation`,
    /// Key → `transactionid`, Advisory → `advisory`, Object → `object` (type is always `Lock`).
    pub fn wait_event(&self) -> Result<&'static str> {
        Ok(match self {
            LockTag::Relation { .. } => "relation",
            LockTag::Key { .. } => "transactionid",
            LockTag::Advisory { .. } => "advisory",
            LockTag::Object { .. } => "object",
        })
    }
}

#[cfg(test)]
mod tests {
    use super::LockTag;
    use pgarbor_common::BranchId;

    fn branch() -> BranchId {
        BranchId([0u8; 16])
    }

    #[test]
    fn describe_relation() {
        let t = LockTag::Relation { branch: branch(), database: 5, relid: 16384 };
        assert_eq!(t.describe().unwrap(), "relation 16384 of database 5");
        assert_eq!(t.locktype().unwrap(), "relation");
        assert_eq!(t.wait_event().unwrap(), "relation");
    }

    #[test]
    fn describe_advisory() {
        let t = LockTag::Advisory { branch: branch(), database: 5, key1: 0, key2: 1, field4: 1 };
        assert_eq!(t.describe().unwrap(), "advisory lock [5,0,1,1]");
        assert_eq!(t.locktype().unwrap(), "advisory");
        assert_eq!(t.wait_event().unwrap(), "advisory");
    }

    #[test]
    fn describe_object() {
        let t = LockTag::Object { branch: branch(), database: 5, classid: 1259, objid: 2200, objsubid: 0 };
        assert_eq!(t.describe().unwrap(), "object 2200 of class 1259 of database 5");
        assert_eq!(t.locktype().unwrap(), "object");
        assert_eq!(t.wait_event().unwrap(), "object");
    }

    #[test]
    fn describe_key() {
        let mut key = 16384u32.to_be_bytes().to_vec();
        key.extend_from_slice(&[0, 0, 0, 7]);
        let t = LockTag::Key { branch: branch(), database: 5, key };
        assert_eq!(t.describe().unwrap(), "tuple of relation 16384 of database 5");
        assert_eq!(t.locktype().unwrap(), "tuple");
        assert_eq!(t.wait_event().unwrap(), "transactionid");
    }

    #[test]
    fn key_relation_threshold() {
        let low = LockTag::Key { branch: branch(), database: 5, key: 100u32.to_be_bytes().to_vec() };
        assert_eq!(low.key_relation(), None);
        assert_eq!(low.describe().unwrap(), "tuple of relation 0 of database 5");
        let short = LockTag::Key { branch: branch(), database: 5, key: vec![1, 2] };
        assert_eq!(short.key_relation(), None);
    }

    #[test]
    fn accessors() {
        let t = LockTag::Relation { branch: branch(), database: 9, relid: 16384 };
        assert_eq!(t.database(), 9);
        assert_eq!(t.branch(), branch());
        assert_eq!(t.key_relation(), Some(16384));
    }
}
