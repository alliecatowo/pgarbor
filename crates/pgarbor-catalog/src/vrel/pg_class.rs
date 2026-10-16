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
use crate::builtin::{self, relid, SystemRel, SYSTEM_RELS};
use crate::model::constraint::ConstraintKind;
use crate::model::index::{Index, IndexMethod};
use crate::model::relation::{RelKind, Relation};
use crate::model::sequence::Sequence;
use crate::snapshot::CatalogSnapshot;
use pgarbor_common::{Oid, Result};
use pgarbor_types::Datum;

/// Producer for `pg_namespace`.
#[derive(Clone, Copy, Debug)]
pub struct PgNamespace;

impl VirtualRelation for PgNamespace {
    fn oid(&self) -> Oid {
        relid::PG_NAMESPACE
    }
    fn scan(&self, ctx: &ScanContext, req: &ScanRequest) -> Result<Vec<Row>> {
        pg_namespace_rows(ctx, req)
    }
}

/// Producer for `pg_class`.
#[derive(Clone, Copy, Debug)]
pub struct PgClass;

impl VirtualRelation for PgClass {
    fn oid(&self) -> Oid {
        relid::PG_CLASS
    }
    fn scan(&self, ctx: &ScanContext, req: &ScanRequest) -> Result<Vec<Row>> {
        let schema = self.schema()?;
        let snap = ctx.env.snapshot();
        class_entries(snap, req).iter().map(|e| class_row(snap, e, schema)).collect()
    }
}

/// Producer for `pg_inherits`.
#[derive(Clone, Copy, Debug)]
pub struct PgInherits;

impl VirtualRelation for PgInherits {
    fn oid(&self) -> Oid {
        relid::PG_INHERITS
    }
    fn scan(&self, ctx: &ScanContext, req: &ScanRequest) -> Result<Vec<Row>> {
        pg_inherits_rows(ctx, req)
    }
}

/// Producer for `pg_sequence`.
#[derive(Clone, Copy, Debug)]
pub struct PgSequence;

impl VirtualRelation for PgSequence {
    fn oid(&self) -> Oid {
        relid::PG_SEQUENCE
    }
    fn scan(&self, ctx: &ScanContext, req: &ScanRequest) -> Result<Vec<Row>> {
        pg_sequence_rows(ctx, req)
    }
}

/// Producer for `pg_rewrite`.
#[derive(Clone, Copy, Debug)]
pub struct PgRewrite;

impl VirtualRelation for PgRewrite {
    fn oid(&self) -> Oid {
        relid::PG_REWRITE
    }
    fn scan(&self, ctx: &ScanContext, req: &ScanRequest) -> Result<Vec<Row>> {
        pg_rewrite_rows(ctx, req)
    }
}


// ------------------------------------------------------------------ shared helpers

/// `pg_am` OID of the heap access method (tables and matviews).
const HEAP_AM: Oid = 2;
/// `FirstNormalTransactionId`: the constant `relfrozenxid` of tables and matviews.
const FIRST_NORMAL_XID: Oid = 3;
/// `FirstMultiXactId`: the constant `relminmxid` of tables and matviews.
const FIRST_MULTIXACT_ID: Oid = 1;

/// Keeps a row whose `oid` column `idx` passes every equality filter on that column. Filters of
/// another Datum type are ignored: the executor re-applies the WHERE clause, so a pre-filter may only
/// drop rows that cannot match.
fn keep_oid(req: &ScanRequest, idx: usize, value: Oid) -> bool {
    req.eq.iter().all(|(i, datum)| *i != idx || !matches!(datum, Datum::Oid(o) if *o != value))
}

/// `name`/`text` variant of [`keep_oid`].
fn keep_name(req: &ScanRequest, idx: usize, value: &str) -> bool {
    req.eq.iter().all(|(i, datum)| *i != idx || !matches!(datum, Datum::Text(t) if t.as_str() != value))
}

/// Catalog counts saturate instead of wrapping (PG caps columns far below these limits).
fn count16(n: usize) -> i16 {
    i16::try_from(n).unwrap_or(i16::MAX)
}

fn count32(n: usize) -> i32 {
    i32::try_from(n).unwrap_or(i32::MAX)
}

/// `text[]` of a reloptions-like list, NULL when empty.
fn text_or_null(values: &[String]) -> Datum {
    if values.is_empty() {
        Datum::Null
    } else {
        d::text_array(values)
    }
}

/// `pg_namespace`: every schema in OID order.
fn pg_namespace_rows(ctx: &ScanContext, req: &ScanRequest) -> Result<Vec<Row>> {
    let snap = ctx.env.snapshot();
    let schema = PgNamespace.schema()?;
    let mut rows = Vec::new();
    for ns in snap.namespaces() {
        if !keep_oid(req, 0, ns.oid) || !keep_name(req, 1, &ns.name) {
            continue;
        }
        let mut b = RowBuilder::new(schema);
        b.set("oid", d::oid(ns.oid));
        b.set("nspname", d::name(&ns.name));
        b.set("nspowner", d::oid(ns.owner));
        b.set("nspacl", d::acl_array(ns.acl.as_ref(), snap));
        rows.push(b.finish()?);
    }
    Ok(rows)
}

/// One `pg_class` entry before its row is built.
#[derive(Clone, Copy)]
enum ClassEntry<'a> {
    /// A builtin system table, view or index (`SYSTEM_RELS`).
    System(&'static SystemRel),
    /// A user table, view, matview, composite type (or a builtin relation object).
    Relation(&'a Relation),
    Index(&'a Index),
    Sequence(&'a Sequence),
}

impl ClassEntry<'_> {
    fn oid(&self) -> Oid {
        match self {
            ClassEntry::System(r) => r.oid,
            ClassEntry::Relation(r) => r.oid,
            ClassEntry::Index(i) => i.oid,
            ClassEntry::Sequence(s) => s.oid,
        }
    }

    fn name(&self) -> &str {
        match self {
            ClassEntry::System(r) => r.name,
            ClassEntry::Relation(r) => r.name.as_str(),
            ClassEntry::Index(i) => i.name.as_str(),
            ClassEntry::Sequence(s) => s.name.as_str(),
        }
    }

    fn namespace(&self) -> Oid {
        match self {
            ClassEntry::System(r) => r.namespace,
            ClassEntry::Relation(r) => r.namespace,
            ClassEntry::Index(i) => i.namespace,
            ClassEntry::Sequence(s) => s.namespace,
        }
    }
}

/// Every `pg_class` entry that passes the request's filters, sorted by OID. The fast paths are
/// `oid` (direct lookups instead of a full walk), `relname` and `relnamespace`.
fn class_entries<'a>(snap: &'a CatalogSnapshot, req: &ScanRequest) -> Vec<ClassEntry<'a>> {
    let data = snap.data();
    let mut out: Vec<ClassEntry<'a>> = Vec::new();
    match req.oid_eq_on(0) {
        Some(oid) => {
            out.extend(builtin::system_rel(oid).map(ClassEntry::System));
            out.extend(data.relations.get(&oid).map(ClassEntry::Relation));
            out.extend(data.indexes.get(&oid).map(ClassEntry::Index));
            out.extend(data.sequences.get(&oid).map(ClassEntry::Sequence));
        }
        None => {
            out.extend(SYSTEM_RELS.iter().map(ClassEntry::System));
            out.extend(snap.user_relations().map(ClassEntry::Relation));
            out.extend(data.indexes.values().map(ClassEntry::Index));
            out.extend(data.sequences.values().map(ClassEntry::Sequence));
        }
    }
    out.retain(|e| keep_oid(req, 0, e.oid()) && keep_name(req, 1, e.name()) && keep_oid(req, 2, e.namespace()));
    out.sort_by_key(|e| e.oid());
    out
}

/// The 34 `pg_class` columns of one entry. [`ClassCols::new`] supplies the defaults shared by every
/// kind; the kind-specific producers override the fields that differ.
struct ClassCols {
    oid: Oid,
    relname: String,
    relnamespace: Oid,
    reltype: Oid,
    reloftype: Oid,
    relowner: Oid,
    relam: Oid,
    relfilenode: Oid,
    reltablespace: Oid,
    relpages: i32,
    reltuples: f32,
    relallvisible: i32,
    relallfrozen: i32,
    reltoastrelid: Oid,
    relhasindex: bool,
    relisshared: bool,
    relpersistence: u8,
    relkind: u8,
    relnatts: i16,
    relchecks: i16,
    relhasrules: bool,
    relhastriggers: bool,
    relhassubclass: bool,
    relrowsecurity: bool,
    relforcerowsecurity: bool,
    relispopulated: bool,
    relreplident: u8,
    relispartition: bool,
    relrewrite: Oid,
    relfrozenxid: Oid,
    relminmxid: Oid,
    relacl: Datum,
    reloptions: Datum,
    relpartbound: Datum,
}

impl ClassCols {
    /// Defaults: no storage estimates (-1 / 0), no toast, no indexes, permanent, not shared,
    /// populated, replica identity 'n', every counter and xid 0, ACL/options/bound NULL.
    fn new(oid: Oid, name: &str, namespace: Oid, owner: Oid, relkind: u8) -> ClassCols {
        ClassCols {
            oid,
            relname: name.to_string(),
            relnamespace: namespace,
            reltype: 0,
            reloftype: 0,
            relowner: owner,
            relam: 0,
            relfilenode: 0,
            reltablespace: 0,
            relpages: 0,
            reltuples: -1.0,
            relallvisible: 0,
            relallfrozen: 0,
            reltoastrelid: 0,
            relhasindex: false,
            relisshared: false,
            relpersistence: b'p',
            relkind,
            relnatts: 0,
            relchecks: 0,
            relhasrules: false,
            relhastriggers: false,
            relhassubclass: false,
            relrowsecurity: false,
            relforcerowsecurity: false,
            relispopulated: true,
            relreplident: b'n',
            relispartition: false,
            relrewrite: 0,
            relfrozenxid: 0,
            relminmxid: 0,
            relacl: Datum::Null,
            reloptions: Datum::Null,
            relpartbound: Datum::Null,
        }
    }

    fn into_row(self, schema: &'static SystemRel) -> Result<Row> {
        let mut b = RowBuilder::new(schema);
        b.set("oid", d::oid(self.oid));
        b.set("relname", d::name(&self.relname));
        b.set("relnamespace", d::oid(self.relnamespace));
        b.set("reltype", d::oid(self.reltype));
        b.set("reloftype", d::oid(self.reloftype));
        b.set("relowner", d::oid(self.relowner));
        b.set("relam", d::oid(self.relam));
        b.set("relfilenode", d::oid(self.relfilenode));
        b.set("reltablespace", d::oid(self.reltablespace));
        b.set("relpages", d::int4(self.relpages));
        b.set("reltuples", d::float4(self.reltuples));
        b.set("relallvisible", d::int4(self.relallvisible));
        b.set("relallfrozen", d::int4(self.relallfrozen));
        b.set("reltoastrelid", d::oid(self.reltoastrelid));
        b.set("relhasindex", d::bool_(self.relhasindex));
        b.set("relisshared", d::bool_(self.relisshared));
        b.set("relpersistence", d::char_(self.relpersistence));
        b.set("relkind", d::char_(self.relkind));
        b.set("relnatts", d::int2(self.relnatts));
        b.set("relchecks", d::int2(self.relchecks));
        b.set("relhasrules", d::bool_(self.relhasrules));
        b.set("relhastriggers", d::bool_(self.relhastriggers));
        b.set("relhassubclass", d::bool_(self.relhassubclass));
        b.set("relrowsecurity", d::bool_(self.relrowsecurity));
        b.set("relforcerowsecurity", d::bool_(self.relforcerowsecurity));
        b.set("relispopulated", d::bool_(self.relispopulated));
        b.set("relreplident", d::char_(self.relreplident));
        b.set("relispartition", d::bool_(self.relispartition));
        b.set("relrewrite", d::oid(self.relrewrite));
        b.set("relfrozenxid", d::oid(self.relfrozenxid));
        b.set("relminmxid", d::oid(self.relminmxid));
        b.set("relacl", self.relacl);
        b.set("reloptions", self.reloptions);
        b.set("relpartbound", self.relpartbound);
        b.finish()
    }
}

/// Columns of a builtin system table, view or index. Owner is the bootstrap superuser (10).
fn system_class_cols(r: &SystemRel) -> ClassCols {
    let mut c = ClassCols::new(r.oid, r.name, r.namespace, builtin::BOOTSTRAP_SUPERUSER, r.kind);
    c.reltype = r.rowtype;
    c.relfilenode = if r.kind == b'v' { 0 } else { r.oid };
    c.relnatts = count16(r.columns.len());
    c.relam = match r.kind {
        b'r' => HEAP_AM,
        b'i' => IndexMethod::Btree.am_oid(),
        _ => 0,
    };
    c.relhasrules = r.kind == b'v';
    // Only the shared catalogs themselves are flagged; their indexes are not detectable (false).
    c.relisshared = r.kind == b'r' && SHARED_CATALOGS.contains(&r.oid);
    c.relreplident = if r.kind == b'r' { b'd' } else { b'n' };
    if r.kind == b'r' {
        c.relfrozenxid = FIRST_NORMAL_XID;
        c.relminmxid = FIRST_MULTIXACT_ID;
    }
    if r.kind == b'i' {
        c.relpages = 1;
        c.reltuples = 0.0;
    }
    c
}

/// Columns of a user `Relation` (table, view, matview, composite type, ...).
fn relation_class_cols(snap: &CatalogSnapshot, rel: &Relation) -> ClassCols {
    let storage = matches!(rel.kind, RelKind::Table | RelKind::MatView);
    let mut c = ClassCols::new(rel.oid, &rel.name, rel.namespace, rel.owner, rel.kind.as_char());
    c.reltype = rel.rowtype;
    c.relam = if storage { HEAP_AM } else { 0 };
    c.relfilenode = if rel.kind.has_storage() { rel.oid } else { 0 };
    c.relpersistence = rel.persistence.as_char();
    c.relnatts = count16(rel.columns.len());
    c.relchecks = count16(snap.constraints_of(rel.oid).into_iter().filter(|k| k.kind == ConstraintKind::Check).count());
    c.relhasindex = storage && !snap.indexes_of(rel.oid).is_empty();
    c.relhasrules = matches!(rel.kind, RelKind::View | RelKind::MatView);
    c.relrowsecurity = rel.row_security;
    c.relforcerowsecurity = rel.force_row_security;
    c.relispopulated = match (&rel.kind, &rel.view) {
        (RelKind::MatView, Some(v)) => v.populated,
        _ => true,
    };
    c.relreplident = if matches!(rel.kind, RelKind::Table | RelKind::MatView | RelKind::PartitionedTable) {
        rel.replica_identity.as_char()
    } else {
        b'n'
    };
    if storage {
        c.relfrozenxid = FIRST_NORMAL_XID;
        c.relminmxid = FIRST_MULTIXACT_ID;
    }
    c.relacl = d::acl_array(rel.acl.as_ref(), snap);
    c.reloptions = text_or_null(&rel.options);
    c.relpartbound = match &rel.partition_of {
        Some((_, bound)) => d::text(bound),
        None => Datum::Null,
    };
    c
}

/// Columns of a user index (`relkind` 'i', relnatts = all index columns including INCLUDE).
fn index_class_cols(idx: &Index) -> ClassCols {
    let mut c = ClassCols::new(idx.oid, &idx.name, idx.namespace, idx.owner, RelKind::Index.as_char());
    c.relam = idx.method.am_oid();
    c.relfilenode = idx.oid;
    c.relpages = 1;
    c.reltuples = 0.0;
    c.relpersistence = idx.persistence.as_char();
    c.relnatts = count16(idx.keys.len());
    c.reloptions = text_or_null(&idx.options);
    c
}

/// Columns of a sequence (fixed relnatts 3: last_value, log_cnt, is_called).
fn sequence_class_cols(snap: &CatalogSnapshot, seq: &Sequence) -> ClassCols {
    let mut c = ClassCols::new(seq.oid, &seq.name, seq.namespace, seq.owner, RelKind::Sequence.as_char());
    c.relfilenode = seq.oid;
    c.relpages = 1;
    c.reltuples = 1.0;
    c.relnatts = 3;
    c.relpersistence = seq.persistence.as_char();
    c.relacl = d::acl_array(seq.acl.as_ref(), snap);
    c
}

/// One `pg_class` row for an entry.
fn class_row(snap: &CatalogSnapshot, entry: &ClassEntry<'_>, schema: &'static SystemRel) -> Result<Row> {
    let cols = match *entry {
        ClassEntry::System(r) => system_class_cols(r),
        ClassEntry::Relation(rel) => relation_class_cols(snap, rel),
        ClassEntry::Index(idx) => index_class_cols(idx),
        ClassEntry::Sequence(seq) => sequence_class_cols(snap, seq),
    };
    cols.into_row(schema)
}

/// `pg_inherits`: one row per parent edge (`inhseqno` 1-based, in `inherits` order), then the
/// partition edge; ordered by (`inhrelid`, `inhseqno`).
fn pg_inherits_rows(ctx: &ScanContext, req: &ScanRequest) -> Result<Vec<Row>> {
    let snap = ctx.env.snapshot();
    let schema = PgInherits.schema()?;
    let mut edges: Vec<(Oid, i32, Oid)> = Vec::new();
    for rel in snap.user_relations() {
        for (i, parent) in rel.inherits.iter().enumerate() {
            edges.push((rel.oid, count32(i + 1), *parent));
        }
        if let Some((parent, _)) = &rel.partition_of {
            edges.push((rel.oid, 1, *parent));
        }
    }
    edges.sort_by_key(|(child, seqno, _)| (*child, *seqno));
    let mut rows = Vec::new();
    for (child, seqno, parent) in edges {
        if !keep_oid(req, 0, child) || !keep_oid(req, 1, parent) {
            continue;
        }
        let mut b = RowBuilder::new(schema);
        b.set("inhrelid", d::oid(child));
        b.set("inhparent", d::oid(parent));
        b.set("inhseqno", d::int4(seqno));
        b.set("inhdetachpending", d::bool_(false));
        rows.push(b.finish()?);
    }
    Ok(rows)
}

/// `pg_sequence`: one row per sequence definition, in OID order.
fn pg_sequence_rows(ctx: &ScanContext, req: &ScanRequest) -> Result<Vec<Row>> {
    let snap = ctx.env.snapshot();
    let schema = PgSequence.schema()?;
    let mut rows = Vec::new();
    for seq in snap.data().sequences.values() {
        if !keep_oid(req, 0, seq.oid) {
            continue;
        }
        let mut b = RowBuilder::new(schema);
        b.set("seqrelid", d::oid(seq.oid));
        b.set("seqtypid", d::oid(seq.type_oid));
        b.set("seqstart", d::int8(seq.start));
        b.set("seqincrement", d::int8(seq.increment));
        b.set("seqmax", d::int8(seq.max));
        b.set("seqmin", d::int8(seq.min));
        b.set("seqcache", d::int8(seq.cache));
        b.set("seqcycle", d::bool_(seq.cycle));
        rows.push(b.finish()?);
    }
    Ok(rows)
}

/// `pg_rewrite`: one `_RETURN` row per view or matview (builtin system views and user views),
/// in OID order. PGArbor reuses the view's OID as the rule OID.
fn pg_rewrite_rows(ctx: &ScanContext, req: &ScanRequest) -> Result<Vec<Row>> {
    let snap = ctx.env.snapshot();
    let schema = PgRewrite.schema()?;
    let mut views: Vec<(Oid, &str)> = Vec::new();
    for rel in builtin::builtin_relations() {
        if let Some(v) = &rel.view {
            views.push((rel.oid, v.query.sql.as_str()));
        }
    }
    for rel in snap.user_relations() {
        if let Some(v) = &rel.view {
            views.push((rel.oid, v.query.sql.as_str()));
        }
    }
    views.sort_by_key(|(oid, _)| *oid);
    let mut rows = Vec::new();
    for (oid, query) in views {
        if !keep_oid(req, 0, oid) || !keep_oid(req, 2, oid) {
            continue;
        }
        let mut b = RowBuilder::new(schema);
        b.set("oid", d::oid(oid));
        b.set("rulename", d::name("_RETURN"));
        b.set("ev_class", d::oid(oid));
        b.set("ev_type", d::char_(b'1'));
        b.set("ev_enabled", d::char_(b'O'));
        b.set("is_instead", d::bool_(true));
        b.set("ev_qual", d::text("<>"));
        b.set("ev_action", d::text(query));
        rows.push(b.finish()?);
    }
    Ok(rows)
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::data::CatalogData;
    use crate::env::CatalogEnv;
    use crate::model::common::StoredExpr;
    use crate::model::relation::{Column, Persistence, ReplicaIdentity, ViewDef};
    use crate::model::{CatalogObject, IndexKey, Namespace};
    use crate::persist::CatalogVersion;
    use crate::session::NullSessionInfo;
    use crate::testing;
    use pgarbor_types::{IoConfig, TypeId};

    /// The value of column `name` in `row` (the schema is the relation's column list).
    fn cell<'r>(schema: &SystemRel, row: &'r Row, name: &str) -> &'r Datum {
        let i = schema.columns.iter().position(|c| c.name == name).expect("column exists");
        &row[i]
    }

    /// The row whose `oid` column is `oid`.
    fn find<'r>(schema: &SystemRel, rows: &'r [Row], oid: Oid) -> &'r Row {
        rows.iter()
            .find(|r| matches!(cell(schema, r, "oid"), Datum::Oid(o) if *o == oid))
            .expect("row exists")
    }

    fn scan_rows(env: &CatalogEnv, rel: &dyn VirtualRelation, req: &ScanRequest) -> Result<Vec<Row>> {
        let session = NullSessionInfo;
        let io = IoConfig::default();
        let ctx = ScanContext { env, session: &session, io_config: &io };
        rel.scan(&ctx, req)
    }

    fn user_env(data: CatalogData) -> Result<CatalogEnv> {
        let snap = CatalogSnapshot::build(CatalogVersion::ZERO, data)?;
        Ok(CatalogEnv::with_default_path(snap))
    }

    fn column(attnum: i16, name: &str) -> Column {
        Column::new(attnum, name, TypeId::new(23, -1), 0)
    }

    fn relation(oid: Oid, name: &str, kind: RelKind, ncols: i16) -> Relation {
        Relation {
            oid,
            name: name.to_string(),
            namespace: 2200,
            kind,
            owner: 10,
            persistence: Persistence::Permanent,
            columns: (1..=ncols).map(|n| column(n, &format!("c{n}"))).collect(),
            rowtype: 0,
            array_type: 0,
            array_type_name: None,
            options: Vec::new(),
            acl: None,
            replica_identity: ReplicaIdentity::Default,
            row_security: false,
            force_row_security: false,
            policies: Vec::new(),
            rules: Vec::new(),
            view: None,
            partition_key: None,
            partition_of: None,
            inherits: Vec::new(),
        }
    }

    #[test]
    fn pg_class_builtin_table_and_index() -> Result<()> {
        let (_kv, env) = testing::bootstrapped()?;
        let schema = PgClass.schema()?;

        let rows = scan_rows(&env, &PgClass, &ScanRequest { eq: vec![(0, Datum::Oid(relid::PG_CLASS))] })?;
        assert_eq!(rows.len(), 1, "oid filter returns exactly one row");
        let row = &rows[0];
        assert_eq!(row.len(), schema.columns.len());
        assert!(matches!(cell(schema, row, "relkind"), Datum::Char(b'r')));
        assert!(matches!(cell(schema, row, "relnatts"), Datum::Int2(34)));
        assert!(matches!(cell(schema, row, "relam"), Datum::Oid(2)));
        assert!(matches!(cell(schema, row, "relpersistence"), Datum::Char(b'p')));

        let idx = builtin::system_rel_by_name(builtin::PG_CATALOG_NAMESPACE, "pg_class_oid_index")
            .expect("system index exists");
        let rows = scan_rows(&env, &PgClass, &ScanRequest { eq: vec![(0, Datum::Oid(idx.oid))] })?;
        assert_eq!(rows.len(), 1);
        assert!(matches!(cell(schema, &rows[0], "relkind"), Datum::Char(b'i')));
        assert!(matches!(cell(schema, &rows[0], "relam"), Datum::Oid(403)));
        Ok(())
    }

    #[test]
    fn pg_class_user_objects() -> Result<()> {
        let table = relation(16384, "t", RelKind::Table, 2);
        let mut view = relation(16386, "v", RelKind::View, 1);
        view.view = Some(ViewDef {
            query: StoredExpr::new(" SELECT t.c1\n   FROM t;"),
            check_option: None,
            populated: true,
        });
        let mut unlogged = relation(16387, "u", RelKind::Table, 1);
        unlogged.persistence = Persistence::Unlogged;
        let index = Index {
            oid: 16388,
            name: "t_c1_idx".to_string(),
            namespace: 2200,
            table: 16384,
            owner: 10,
            method: IndexMethod::Btree,
            keys: vec![IndexKey {
                column: Some(1),
                expr: None,
                name: "c1".to_string(),
                type_id: TypeId::new(23, -1),
                collation: 0,
                opclass: 1978,
                descending: false,
                nulls_first: false,
            }],
            n_key_columns: 1,
            unique: false,
            nulls_not_distinct: false,
            primary: false,
            exclusion: false,
            immediate: true,
            clustered: false,
            valid: true,
            replident: false,
            predicate: None,
            constraint: None,
            options: Vec::new(),
            persistence: Persistence::Permanent,
        };
        let seq = Sequence {
            oid: 16389,
            name: "s".to_string(),
            namespace: 2200,
            owner: 10,
            persistence: Persistence::Permanent,
            type_oid: 20,
            start: 1,
            increment: 1,
            min: 1,
            max: i64::MAX,
            cache: 1,
            cycle: false,
            owned_by: None,
            identity: false,
            acl: None,
        };
        let mut data = CatalogData::default();
        // `public` (2200) is persisted, not builtin: the snapshot rejects objects whose namespace is absent.
        data.insert(CatalogObject::Namespace(Namespace { oid: 2200, name: "public".into(), owner: 6171, acl: None }));
        data.insert(CatalogObject::Relation(table));
        data.insert(CatalogObject::Relation(view));
        data.insert(CatalogObject::Relation(unlogged));
        data.insert(CatalogObject::Index(index));
        data.insert(CatalogObject::Sequence(seq));
        let env = user_env(data)?;

        let schema = PgClass.schema()?;
        let rows = scan_rows(&env, &PgClass, &ScanRequest::all())?;
        for row in &rows {
            assert_eq!(row.len(), schema.columns.len(), "every row has the schema's column count");
        }

        let t = find(schema, &rows, 16384);
        assert!(matches!(cell(schema, t, "relkind"), Datum::Char(b'r')));
        assert!(matches!(cell(schema, t, "relnatts"), Datum::Int2(2)));
        assert!(matches!(cell(schema, t, "relpages"), Datum::Int4(0)));
        assert!(matches!(cell(schema, t, "reltuples"), Datum::Float4(v) if *v == -1.0));
        assert!(matches!(cell(schema, t, "relhasindex"), Datum::Bool(true)));
        assert!(matches!(cell(schema, t, "relreplident"), Datum::Char(b'd')));
        assert!(matches!(cell(schema, t, "relam"), Datum::Oid(2)));
        assert!(matches!(cell(schema, t, "relfilenode"), Datum::Oid(16384)));

        let u = find(schema, &rows, 16387);
        assert!(matches!(cell(schema, u, "relpersistence"), Datum::Char(b'u')));

        let v = find(schema, &rows, 16386);
        assert!(matches!(cell(schema, v, "relkind"), Datum::Char(b'v')));
        assert!(matches!(cell(schema, v, "relhasrules"), Datum::Bool(true)));
        assert!(matches!(cell(schema, v, "relam"), Datum::Oid(0)));
        assert!(matches!(cell(schema, v, "relfilenode"), Datum::Oid(0)));
        assert!(matches!(cell(schema, v, "relreplident"), Datum::Char(b'n')));

        let i = find(schema, &rows, 16388);
        assert!(matches!(cell(schema, i, "relkind"), Datum::Char(b'i')));
        assert!(matches!(cell(schema, i, "relam"), Datum::Oid(403)));
        assert!(matches!(cell(schema, i, "relnatts"), Datum::Int2(1)));
        assert!(matches!(cell(schema, i, "relhasindex"), Datum::Bool(false)));

        let s = find(schema, &rows, 16389);
        assert!(matches!(cell(schema, s, "relkind"), Datum::Char(b'S')));
        assert!(matches!(cell(schema, s, "relpages"), Datum::Int4(1)));
        assert!(matches!(cell(schema, s, "reltuples"), Datum::Float4(v) if *v == 1.0));
        assert!(matches!(cell(schema, s, "relnatts"), Datum::Int2(3)));
        assert!(matches!(cell(schema, s, "relam"), Datum::Oid(0)));

        // pg_rewrite: one _RETURN row for the user view.
        let rw_schema = PgRewrite.schema()?;
        let rw = scan_rows(&env, &PgRewrite, &ScanRequest::all())?;
        let ret = find(rw_schema, &rw, 16386);
        assert!(matches!(cell(rw_schema, ret, "rulename"), Datum::Text(s) if s.as_str() == "_RETURN"));
        assert!(matches!(cell(rw_schema, ret, "ev_class"), Datum::Oid(16386)));

        // pg_sequence: definition of the sequence.
        let sq_schema = PgSequence.schema()?;
        let sq = scan_rows(&env, &PgSequence, &ScanRequest { eq: vec![(0, Datum::Oid(16389))] })?;
        assert_eq!(sq.len(), 1);
        assert!(matches!(cell(sq_schema, &sq[0], "seqtypid"), Datum::Oid(20)));
        assert!(matches!(cell(sq_schema, &sq[0], "seqmax"), Datum::Int8(m) if *m == i64::MAX));
        assert!(matches!(cell(sq_schema, &sq[0], "seqcycle"), Datum::Bool(false)));

        // pg_inherits is empty in v1.
        assert!(scan_rows(&env, &PgInherits, &ScanRequest::all())?.is_empty());
        Ok(())
    }

    #[test]
    fn pg_namespace_bootstrapped_order_and_filter() -> Result<()> {
        let (_kv, env) = testing::bootstrapped()?;
        let schema = PgNamespace.schema()?;
        let rows = scan_rows(&env, &PgNamespace, &ScanRequest::all())?;
        let oids: Vec<Oid> = rows
            .iter()
            .filter_map(|r| match cell(schema, r, "oid") {
                Datum::Oid(o) => Some(*o),
                _ => None,
            })
            .collect();
        assert_eq!(oids, vec![11, 99, 2200, 13293]);

        let by_name = ScanRequest { eq: vec![(1, Datum::Text("public".to_string()))] };
        let rows = scan_rows(&env, &PgNamespace, &by_name)?;
        assert_eq!(rows.len(), 1);
        assert!(matches!(cell(schema, &rows[0], "oid"), Datum::Oid(2200)));
        Ok(())
    }
}
