//! `pg_namespace`, `pg_class`, `pg_inherits`, `pg_sequence`, `pg_rewrite`. Lease CAT-28.
//! PG sources: `catalog/pg_class.h`, `pg_namespace.h`, `pg_inherits.h`, `pg_sequence.h`,
//! `pg_rewrite.h`; values below were checked against the PG 18.6 oracle.
//! Column lists: `builtin::system_rel(relid::PG_CLASS)` etc. Use `rowbuild::RowBuilder` + `d::*`.
//! Rows are ordered by `oid` ascending (pg_inherits by `inhrelid, inhseqno`).
//!
//! ## pg_namespace
//! One row per `snapshot.namespaces()` (builtin pg_catalog 11, pg_toast 99, information_schema 13293
//! plus persisted incl. `public`): `oid`, `nspname`, `nspowner`, `nspacl` (`d::acl_array`).
//! Fast path: eq on `oid` (col 0) or `nspname` (col 1).
//!
//! ## pg_class
//! Rows for (a) every builtin system relation of `builtin::SYSTEM_RELS` (tables 'r', views 'v',
//! indexes 'i'), (b) every user `Relation` (`snapshot.user_relations()`), (c) every user `Index`,
//! (d) every `Sequence`. Fast paths: eq on `oid` (col 0), `relname` (col 1) [+ `relnamespace` col 2].
//! Column derivation (oracle values for a fresh PG 18.6 table `t`, view `v`, matview `mv`, composite
//! `c`, index, sequence, unlogged table):
//! | column | value |
//! |---|---|
//! | oid, relname, relnamespace, relowner | object fields (system relations: owner 10, namespace from the entry) |
//! | reltype | `Relation::rowtype` (system: `SystemRel::rowtype`); indexes and sequences 0 |
//! | reloftype | 0 |
//! | relam | tables/matviews 2 (heap); indexes `IndexMethod::am_oid` (system indexes 403); views, composite types, sequences 0 |
//! | relfilenode | the relation's OID for relkinds with storage (r, m, i, S, t); 0 for views and composite types (oracle) |
//! | reltablespace | 0 |
//! | relpages / reltuples | tables, matviews: 0 / -1.0; indexes: 1 / 0.0; sequences: 1 / 1.0; views, composite: 0 / -1.0 (`Float4`) |
//! | relallvisible, relallfrozen | 0 |
//! | reltoastrelid | 0 (PGArbor has no TOAST tables: documented divergence; PG shows non-zero for tables with varlena columns) |
//! | relhasindex | `!snapshot.indexes_of(oid).is_empty()` for tables/matviews; false otherwise (system tables: false - system indexes are not linked to tables here) |
//! | relisshared | true only for the shared catalogs `pg_authid, pg_auth_members, pg_database, pg_tablespace, pg_shdepend, pg_shdescription, pg_db_role_setting, pg_replication_origin, pg_subscription, pg_parameter_acl, pg_shseclabel` and their indexes (not detectable: false) |
//! | relpersistence | `Persistence::as_char`; system 'p' |
//! | relkind | `RelKind::as_char` (r i S v m c f p I t) |
//! | relnatts | `columns.len()` (dropped columns included; indexes `keys.len()`; sequences 3; system: `SystemRel::columns.len()`) |
//! | relchecks | number of `ConstraintKind::Check` constraints of the table (NOT NULL constraints excluded) |
//! | relhasrules | true for views and matviews (their `_RETURN` rule), else false |
//! | relhastriggers / relhassubclass | false |
//! | relrowsecurity / relforcerowsecurity | `Relation` fields |
//! | relispopulated | `ViewDef::populated` for matviews, true otherwise |
//! | relreplident | `ReplicaIdentity::as_char` for tables/matviews ('d' default); 'n' for views, indexes, sequences, composite types (oracle) |
//! | relispartition / relrewrite | false / 0 |
//! | relfrozenxid | `Oid(3)` for tables and matviews (FirstNormalTransactionId; PG shows the real xid), `Oid(0)` otherwise |
//! | relminmxid | `Oid(1)` for tables and matviews, `Oid(0)` otherwise |
//! | relacl | `d::acl_array(acl)` |
//! | reloptions | `d::text_array` of `options`, NULL when empty |
//! | relpartbound | NULL (`partition_of.1` text when partitioned later) |
//!
//! ## pg_inherits
//! `Relation::inherits[i]` -> `(inhrelid = rel, inhparent = parent, inhseqno = i + 1, false)`;
//! `partition_of = Some((parent, _))` -> `(rel, parent, 1, false)`. Empty in v1.
//!
//! ## pg_sequence
//! One row per `Sequence`: `seqrelid, seqtypid = type_oid, seqstart, seqincrement, seqmax, seqmin,
//! seqcache, seqcycle` (`Int8`, `Bool`). Fast path: eq on `seqrelid` (col 0).
//!
//! ## pg_rewrite
//! One `_RETURN` row per view/matview (user AND builtin system views): `oid` = the view's OID
//! (PGArbor does not allocate separate rule OIDs; documented), `rulename` "_RETURN", `ev_class` = view,
//! `ev_type` '1', `ev_enabled` 'O', `is_instead` true, `ev_qual` "<>", `ev_action` = the stored view
//! query text (`ViewDef::query.sql`; PG stores a serialized query tree - tools pass it only to
//! `pg_get_viewdef`/`pg_get_ruledef`, which this crate answers from the text).
use super::rowbuild::{d, RowBuilder};
use super::{Row, ScanContext, ScanRequest, VirtualRelation};
use crate::builtin::relid;
use pgarbor_common::{Oid, PgError, Result};

/// Producer for `pg_namespace`.
#[derive(Clone, Copy, Debug)]
pub struct PgNamespace;

impl VirtualRelation for PgNamespace {
    fn oid(&self) -> Oid {
        relid::PG_NAMESPACE
    }
    #[allow(unused_variables)]
    fn scan(&self, ctx: &ScanContext, req: &ScanRequest) -> Result<Vec<Row>> {
        Err(PgError::unimplemented("CAT-28", "pg_namespace rows"))
    }
}

/// Producer for `pg_class`.
#[derive(Clone, Copy, Debug)]
pub struct PgClass;

impl VirtualRelation for PgClass {
    fn oid(&self) -> Oid {
        relid::PG_CLASS
    }
    #[allow(unused_variables)]
    fn scan(&self, ctx: &ScanContext, req: &ScanRequest) -> Result<Vec<Row>> {
        Err(PgError::unimplemented("CAT-28", "pg_class rows"))
    }
}

/// Producer for `pg_inherits`.
#[derive(Clone, Copy, Debug)]
pub struct PgInherits;

impl VirtualRelation for PgInherits {
    fn oid(&self) -> Oid {
        relid::PG_INHERITS
    }
    #[allow(unused_variables)]
    fn scan(&self, ctx: &ScanContext, req: &ScanRequest) -> Result<Vec<Row>> {
        Err(PgError::unimplemented("CAT-28", "pg_inherits rows"))
    }
}

/// Producer for `pg_sequence`.
#[derive(Clone, Copy, Debug)]
pub struct PgSequence;

impl VirtualRelation for PgSequence {
    fn oid(&self) -> Oid {
        relid::PG_SEQUENCE
    }
    #[allow(unused_variables)]
    fn scan(&self, ctx: &ScanContext, req: &ScanRequest) -> Result<Vec<Row>> {
        Err(PgError::unimplemented("CAT-28", "pg_sequence rows"))
    }
}

/// Producer for `pg_rewrite`.
#[derive(Clone, Copy, Debug)]
pub struct PgRewrite;

impl VirtualRelation for PgRewrite {
    fn oid(&self) -> Oid {
        relid::PG_REWRITE
    }
    #[allow(unused_variables)]
    fn scan(&self, ctx: &ScanContext, req: &ScanRequest) -> Result<Vec<Row>> {
        Err(PgError::unimplemented("CAT-28", "pg_rewrite rows"))
    }
}


/// Registry slice.
pub static RELATIONS: &[&dyn VirtualRelation] = &[&PgNamespace, &PgClass, &PgInherits, &PgSequence, &PgRewrite];

/// Helper for tests/other producers: `pg_class.relisshared` set (module docs).
pub const SHARED_CATALOGS: &[Oid] = &[
    relid::PG_AUTHID,
    relid::PG_AUTH_MEMBERS,
    relid::PG_DATABASE,
    relid::PG_TABLESPACE,
    relid::PG_SHDEPEND,
    relid::PG_SHDESCRIPTION,
    relid::PG_DB_ROLE_SETTING,
    relid::PG_REPLICATION_ORIGIN,
    relid::PG_SUBSCRIPTION,
    relid::PG_PARAMETER_ACL,
    relid::PG_SHSECLABEL,
];

#[allow(dead_code)]
fn _uses(_: &RowBuilder) {
    let _ = d::oid(0);
}
