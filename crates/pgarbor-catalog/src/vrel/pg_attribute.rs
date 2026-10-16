//! `pg_attribute` and `pg_attrdef`. Lease CAT-29. PG sources: `catalog/pg_attribute.h`,
//! `pg_attrdef.h`, `catalog/heap.c` (`SystemAttributeDefinition`, `AddNewAttributeTuples`),
//! `utils/cache/relcache.c`. Oracle-checked against PG 18.6.
//!
//! ## pg_attribute rows (order: `attrelid` ascending, then `attnum` ascending; PG's physical order is
//! heap order, tools use ORDER BY). Fast path: eq on `attrelid` (col 0) - REQUIRED to be fast because
//! every driver introspection query filters by it.
//! Relations covered: builtin system relations (columns from `SystemRel::columns`, tables get system
//! columns, views and indexes do not), user relations (tables, matviews, partitioned tables,
//! views, composite types), user indexes (one row per key/include column), sequences (the three
//! columns `last_value bigint`, `log_cnt bigint`, `is_called boolean` + system columns).
//! System columns (relations with `RelKind::has_system_columns()`, i.e. tables, matviews, sequences,
//! partitioned/foreign tables; NOT views, indexes, composite types), attnums -1..-6:
//! | attnum | attname | atttypid | attlen | attbyval | attalign |
//! |---|---|---|---|---|---|
//! | -1 | ctid | tid (27) | 6 | false | 's' |
//! | -2 | xmin | xid (28) | 4 | true | 'i' |
//! | -3 | cmin | cid (29) | 4 | true | 'i' |
//! | -4 | xmax | xid (28) | 4 | true | 'i' |
//! | -5 | cmax | cid (29) | 4 | true | 'i' |
//! | -6 | tableoid | oid (26) | 4 | true | 'i' |
//! all with `attstorage` 'p', `attnotnull` true, `attislocal` true, everything else default.
//! Ordinary column derivation (`Column` -> row): `attname`; `atttypid` = `type_id.oid`; `atttypmod` =
//! `type_id.typmod`; `attlen/attbyval/attalign` from `env.type_info(oid)` (`TypeInfo` len, byval,
//! align); `attstorage` = `Column::storage` or the type's `typstorage` (text 'x', int 'p');
//! `attcompression` = `Column::compression` or 0 (`Char(0)`); `attndims`; `attnotnull`;
//! `atthasdef` = `default.is_some()` (generated columns too); `atthasmissing` =
//! `missing_value.is_some()`; `attmissingval` = a one-element array of the column's type built by
//! `io::input(ctx.io_ctx(), array type of the column, "{<escaped value>}")` (NULL when absent or no
//! array type exists); `attidentity` = `IdentityKind::as_char` or 0; `attgenerated` =
//! `GeneratedKind::as_char` or 0; `attisdropped`; `attislocal`; `attinhcount`; `attcollation`;
//! `attstattarget` = `stat_target` as Int2 or NULL (PG 18 NULL = default); `attacl`; `attoptions`
//! (text[] or NULL); `attfdwoptions` NULL.
//! Dropped columns: `attname` = `Column::dropped_name`, `atttypid` 0, `attlen` 0, `attbyval` false,
//! `attalign` 'i', `attstorage` 'p', `attisdropped` true, `attnotnull` false (PG keeps the old
//! `attlen`/`attalign`; PGArbor does not retain them: documented divergence).
//! Index rows: per key `IndexKey` (`attnum` = 1..n): `attname` = `IndexKey::name`, `atttypid`/typmod
//! from the key, `attcollation` = key collation, `attnotnull` false, storage/len/align from the type,
//! no system columns. Sequence rows: attnum 1..3 as above, `attnotnull` true.
//!
//! ## pg_attrdef
//! One row per column with a default or generation expression, ordered by `adrelid, adnum`:
//! `oid` = `ColumnDefault::oid`, `adrelid`, `adnum` (attnum), `adbin` = the expression text
//! (`StoredExpr::sql`; PG stores a node tree: `pg_get_expr(adbin, adrelid)` returns text unchanged,
//! see `sysfuncs::info`). Fast path: eq on `adrelid` (col 1).
use super::rowbuild::{d, RowBuilder};
use super::{Row, ScanContext, ScanRequest, VirtualRelation};
use crate::builtin::{self, relid, SystemRel};
use crate::errors;
use crate::model::{Column, Index, Relation};
use crate::snapshot::{CatalogSnapshot, ClassRef};
use pgarbor_common::{Oid, Result};
use pgarbor_types::registry::TypeInfo;
use pgarbor_types::{io, Datum, TypeEnv, TypeId};

/// Producer for `pg_attribute`.
#[derive(Clone, Copy, Debug)]
pub struct PgAttribute;

impl VirtualRelation for PgAttribute {
    fn oid(&self) -> Oid {
        relid::PG_ATTRIBUTE
    }
    fn scan(&self, ctx: &ScanContext, req: &ScanRequest) -> Result<Vec<Row>> {
        let schema = self.schema()?;
        let mut out = Vec::new();
        for oid in candidate_relids(ctx.env.snapshot(), req.oid_eq_on(0)) {
            push_attribute_rows(ctx, schema, oid, &mut out)?;
        }
        Ok(out)
    }
}

/// Producer for `pg_attrdef`.
#[derive(Clone, Copy, Debug)]
pub struct PgAttrdef;

impl VirtualRelation for PgAttrdef {
    fn oid(&self) -> Oid {
        relid::PG_ATTRDEF
    }
    fn scan(&self, ctx: &ScanContext, req: &ScanRequest) -> Result<Vec<Row>> {
        let schema = self.schema()?;
        let snap = ctx.env.snapshot();
        let mut out = Vec::new();
        match req.oid_eq_on(1) {
            Some(adrelid) => {
                if let Some(rel) = snap.relation(adrelid) {
                    push_attrdef_rows(schema, rel, &mut out)?;
                }
            }
            None => {
                for rel in snap.user_relations() {
                    push_attrdef_rows(schema, rel, &mut out)?;
                }
            }
        }
        Ok(out)
    }
}


/// Registry slice.
pub static RELATIONS: &[&dyn VirtualRelation] = &[&PgAttribute, &PgAttrdef];

/// The six system columns (module docs table): `(attnum, name, type oid, len, byval, align)`.
// CONTRACT
pub const SYSTEM_COLUMN_DEFS: [(i16, &str, Oid, i16, bool, u8); 6] = [
    (-1, "ctid", 27, 6, false, b's'),
    (-2, "xmin", 28, 4, true, b'i'),
    (-3, "cmin", 29, 4, true, b'i'),
    (-4, "xmax", 28, 4, true, b'i'),
    (-5, "cmax", 29, 4, true, b'i'),
    (-6, "tableoid", 26, 4, true, b'i'),
];

/// Every value of one `pg_attribute` row (catalog order is applied by `attr_row`).
struct AttrFields<'a> {
    relid: Oid,
    name: &'a str,
    attnum: i16,
    type_oid: Oid,
    typmod: i32,
    len: i16,
    byval: bool,
    align: u8,
    storage: u8,
    compression: u8,
    ndims: i16,
    not_null: bool,
    has_def: bool,
    has_missing: bool,
    missing: Datum,
    identity: u8,
    generated: u8,
    dropped: bool,
    is_local: bool,
    inh_count: i16,
    collation: Oid,
    stat_target: Option<i16>,
    acl: Datum,
    options: Datum,
}

/// Build one `pg_attribute` row by column name (the schema order is PG's).
fn attr_row(schema: &'static SystemRel, a: AttrFields<'_>) -> Result<Row> {
    let mut b = RowBuilder::new(schema);
    b.set("attrelid", d::oid(a.relid))
        .set("attname", d::name(a.name))
        .set("atttypid", d::oid(a.type_oid))
        .set("attlen", d::int2(a.len))
        .set("attnum", d::int2(a.attnum))
        .set("atttypmod", d::int4(a.typmod))
        .set("attndims", d::int2(a.ndims))
        .set("attbyval", d::bool_(a.byval))
        .set("attalign", d::char_(a.align))
        .set("attstorage", d::char_(a.storage))
        .set("attcompression", d::char_(a.compression))
        .set("attnotnull", d::bool_(a.not_null))
        .set("atthasdef", d::bool_(a.has_def))
        .set("atthasmissing", d::bool_(a.has_missing))
        .set("attidentity", d::char_(a.identity))
        .set("attgenerated", d::char_(a.generated))
        .set("attisdropped", d::bool_(a.dropped))
        .set("attislocal", d::bool_(a.is_local))
        .set("attinhcount", d::int2(a.inh_count))
        .set("attcollation", d::oid(a.collation))
        .set("attstattarget", a.stat_target.map(d::int2).unwrap_or(Datum::Null))
        .set("attacl", a.acl)
        .set("attoptions", a.options)
        .set("attfdwoptions", Datum::Null)
        .set("attmissingval", a.missing);
    b.finish()
}

/// Type metadata of a column/key type; a missing entry is a corrupt catalog (cache lookup failure).
fn type_meta(ctx: &ScanContext, oid: Oid) -> Result<TypeInfo> {
    ctx.env.type_info(oid).ok_or_else(|| errors::cache_lookup_failed("type", oid))
}

/// PG array literal holding exactly one element: `{"..."}`, with `"` and `\` backslash-escaped.
fn array_literal(elem: &str) -> String {
    let mut s = String::with_capacity(elem.len() + 4);
    s.push_str("{\"");
    for c in elem.chars() {
        if c == '"' || c == '\\' {
            s.push('\\');
        }
        s.push(c);
    }
    s.push_str("\"}");
    s
}

/// Relation OIDs to emit, ascending. A filter on `attrelid` narrows to that one OID.
fn candidate_relids(snap: &CatalogSnapshot, only: Option<Oid>) -> Vec<Oid> {
    if let Some(oid) = only {
        return vec![oid];
    }
    let data = snap.data();
    let mut oids: Vec<Oid> = builtin::SYSTEM_RELS
        .iter()
        .filter(|r| matches!(r.kind, b'r' | b'v' | b'i'))
        .map(|r| r.oid)
        .collect();
    oids.extend(data.relations.keys().copied());
    oids.extend(data.indexes.keys().copied());
    oids.extend(data.sequences.keys().copied());
    oids.sort_unstable();
    oids.dedup();
    oids
}

/// Rows of one pg_class OID (nothing for an unknown OID).
fn push_attribute_rows(ctx: &ScanContext, schema: &'static SystemRel, oid: Oid, out: &mut Vec<Row>) -> Result<()> {
    let snap = ctx.env.snapshot();
    match snap.class(oid) {
        Some(ClassRef::Relation(o)) => {
            if let Some(rel) = snap.relation(o) {
                push_relation_rows(ctx, schema, rel, out)?;
            }
        }
        Some(ClassRef::Index(o)) => {
            if let Some(ix) = snap.index(o) {
                push_index_rows(ctx, schema, ix, out)?;
            }
        }
        Some(ClassRef::Sequence(o)) => {
            if snap.sequence(o).is_some() {
                push_sequence_rows(ctx, schema, o, out)?;
            }
        }
        Some(ClassRef::SystemIndex(o)) => {
            if let Some(sr) = builtin::system_rel(o) {
                push_system_index_rows(ctx, schema, sr, out)?;
            }
        }
        None => {}
    }
    Ok(())
}

/// The six system columns (attnum -6 first, as PG orders them), then the declared columns.
fn push_relation_rows(ctx: &ScanContext, schema: &'static SystemRel, rel: &Relation, out: &mut Vec<Row>) -> Result<()> {
    if rel.kind.has_system_columns() {
        for def in SYSTEM_COLUMN_DEFS.iter().rev() {
            out.push(attr_row(schema, system_column_fields(rel.oid, *def))?);
        }
    }
    for col in &rel.columns {
        out.push(attr_row(schema, column_fields(ctx, rel.oid, col)?)?);
    }
    Ok(())
}

fn column_fields<'a>(ctx: &ScanContext, relid: Oid, col: &'a Column) -> Result<AttrFields<'a>> {
    if col.dropped {
        // PG heap.c RemoveAttributeById clears the type, not-null, missing, statistics, acl and
        // options; PGArbor does not keep the old attlen/attalign (module docs).
        return Ok(AttrFields {
            relid,
            name: col.name.as_str(),
            attnum: col.attnum,
            type_oid: 0,
            typmod: col.type_id.typmod,
            len: 0,
            byval: false,
            align: b'i',
            storage: b'p',
            compression: 0,
            ndims: col.ndims as i16,
            not_null: false,
            has_def: false,
            has_missing: false,
            missing: Datum::Null,
            identity: 0,
            generated: 0,
            dropped: true,
            is_local: col.is_local,
            inh_count: col.inh_count,
            collation: col.collation,
            stat_target: None,
            acl: Datum::Null,
            options: Datum::Null,
        });
    }
    let info = type_meta(ctx, col.type_id.oid)?;
    // attmissingval: a one-element array of the column type, when the array type exists.
    let missing = match &col.missing_value {
        Some(text) if info.array != 0 => io::input(&ctx.io_ctx(), TypeId::new(info.array, -1), &array_literal(text))?,
        _ => Datum::Null,
    };
    let acl = d::acl_array(col.acl.as_ref(), ctx.env.snapshot());
    let options = if col.options.is_empty() { Datum::Null } else { d::text_array(&col.options) };
    Ok(AttrFields {
        relid,
        name: col.name.as_str(),
        attnum: col.attnum,
        type_oid: col.type_id.oid,
        typmod: col.type_id.typmod,
        len: info.len,
        byval: info.byval,
        align: info.align,
        storage: col.storage.unwrap_or(info.storage),
        compression: col.compression.unwrap_or(0),
        ndims: col.ndims as i16,
        not_null: col.not_null,
        has_def: col.default.is_some(),
        has_missing: col.missing_value.is_some(),
        missing,
        identity: col.identity.map_or(0, |i| i.kind.as_char()),
        generated: col.generated.map_or(0, |g| g.as_char()),
        dropped: false,
        is_local: col.is_local,
        inh_count: col.inh_count,
        collation: col.collation,
        stat_target: col.stat_target,
        acl,
        options,
    })
}

/// The six system columns of a relation (module docs table): all `attnotnull`, storage 'p'.
fn system_column_fields(relid: Oid, def: (i16, &'static str, Oid, i16, bool, u8)) -> AttrFields<'static> {
    let (attnum, name, type_oid, len, byval, align) = def;
    AttrFields {
        relid,
        name,
        attnum,
        type_oid,
        typmod: -1,
        len,
        byval,
        align,
        storage: b'p',
        compression: 0,
        ndims: 0,
        not_null: true,
        has_def: false,
        has_missing: false,
        missing: Datum::Null,
        identity: 0,
        generated: 0,
        dropped: false,
        is_local: true,
        inh_count: 0,
        collation: 0,
        stat_target: None,
        acl: Datum::Null,
        options: Datum::Null,
    }
}

/// Sequence state columns (attnum 1..3), all NOT NULL.
fn push_sequence_rows(ctx: &ScanContext, schema: &'static SystemRel, relid: Oid, out: &mut Vec<Row>) -> Result<()> {
    const SEQUENCE_COLUMNS: [(&str, Oid); 3] = [("last_value", 20), ("log_cnt", 20), ("is_called", 16)];
    for (i, &(name, type_oid)) in SEQUENCE_COLUMNS.iter().enumerate() {
        let info = type_meta(ctx, type_oid)?;
        out.push(attr_row(
            schema,
            AttrFields {
                relid,
                name,
                attnum: (i + 1) as i16,
                type_oid,
                typmod: -1,
                len: info.len,
                byval: info.byval,
                align: info.align,
                storage: info.storage,
                compression: 0,
                ndims: 0,
                not_null: true,
                has_def: false,
                has_missing: false,
                missing: Datum::Null,
                identity: 0,
                generated: 0,
                dropped: false,
                is_local: true,
                inh_count: 0,
                collation: info.collation,
                stat_target: None,
                acl: Datum::Null,
                options: Datum::Null,
            },
        )?);
    }
    Ok(())
}

/// One row per index key and INCLUDE column (attnum 1..n), never NOT NULL.
fn push_index_rows(ctx: &ScanContext, schema: &'static SystemRel, ix: &Index, out: &mut Vec<Row>) -> Result<()> {
    for (i, key) in ix.keys.iter().enumerate() {
        let info = type_meta(ctx, key.type_id.oid)?;
        out.push(attr_row(
            schema,
            AttrFields {
                relid: ix.oid,
                name: key.name.as_str(),
                attnum: (i + 1) as i16,
                type_oid: key.type_id.oid,
                typmod: key.type_id.typmod,
                len: info.len,
                byval: info.byval,
                align: info.align,
                storage: info.storage,
                compression: 0,
                ndims: 0,
                not_null: false,
                has_def: false,
                has_missing: false,
                missing: Datum::Null,
                identity: 0,
                generated: 0,
                dropped: false,
                is_local: true,
                inh_count: 0,
                collation: key.collation,
                stat_target: None,
                acl: Datum::Null,
                options: Datum::Null,
            },
        )?);
    }
    Ok(())
}

/// Builtin system index: one row per declared column (`SysCol`).
fn push_system_index_rows(ctx: &ScanContext, schema: &'static SystemRel, sr: &SystemRel, out: &mut Vec<Row>) -> Result<()> {
    for c in sr.columns {
        let info = type_meta(ctx, c.type_oid)?;
        out.push(attr_row(
            schema,
            AttrFields {
                relid: sr.oid,
                name: c.name,
                attnum: c.attnum,
                type_oid: c.type_oid,
                typmod: c.typmod,
                len: info.len,
                byval: info.byval,
                align: info.align,
                storage: info.storage,
                compression: 0,
                ndims: 0,
                not_null: c.not_null,
                has_def: false,
                has_missing: false,
                missing: Datum::Null,
                identity: 0,
                generated: 0,
                dropped: false,
                is_local: true,
                inh_count: 0,
                collation: info.collation,
                stat_target: None,
                acl: Datum::Null,
                options: Datum::Null,
            },
        )?);
    }
    Ok(())
}

/// One `pg_attrdef` row per column with a default or generation expression (attnum order).
fn push_attrdef_rows(schema: &'static SystemRel, rel: &Relation, out: &mut Vec<Row>) -> Result<()> {
    for col in &rel.columns {
        if let Some(def) = &col.default {
            let mut b = RowBuilder::new(schema);
            b.set("oid", d::oid(def.oid))
                .set("adrelid", d::oid(rel.oid))
                .set("adnum", d::int2(col.attnum))
                .set("adbin", d::text(&def.expr.sql));
            out.push(b.finish()?);
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ddl::spec::{ColumnSpec, CreateTableSpec};
    use crate::env::CatalogEnv;
    use crate::model::{Persistence, StoredExpr};
    use crate::session::NullSessionInfo;
    use crate::testing;
    use pgarbor_common::PgError;
    use pgarbor_types::IoConfig;

    fn cell<'r>(schema: &SystemRel, row: &'r Row, name: &str) -> Result<&'r Datum> {
        let i = schema
            .columns
            .iter()
            .position(|c| c.name == name)
            .ok_or_else(|| PgError::internal(format!("no column {name}")))?;
        row.get(i).ok_or_else(|| PgError::internal("short row"))
    }

    fn table_spec(name: &str) -> CreateTableSpec {
        let mut a = ColumnSpec::new("a", TypeId::new(23, -1));
        a.default = Some(StoredExpr::new("42"));
        CreateTableSpec {
            namespace: builtin::PUBLIC_NAMESPACE,
            name: name.to_string(),
            owner: builtin::BOOTSTRAP_SUPERUSER,
            kind: crate::model::RelKind::Table,
            persistence: Persistence::Permanent,
            columns: vec![ColumnSpec::new("id", TypeId::new(23, -1)), a, ColumnSpec::new("b", TypeId::new(25, -1))],
            constraints: vec![],
            options: vec![],
            if_not_exists: false,
            inherits: vec![],
            extra: Default::default(),
        }
    }

    #[test]
    fn builtin_table_has_system_columns_first() -> Result<()> {
        let env = CatalogEnv::with_default_path(CatalogSnapshot::builtin_only()?);
        let session = NullSessionInfo;
        let io = IoConfig::default();
        let ctx = ScanContext { env: &env, session: &session, io_config: &io };
        let schema = PgAttribute.schema()?;
        let rel = builtin::system_rel(relid::PG_CLASS).ok_or_else(|| PgError::internal("pg_class"))?;
        let req = ScanRequest { eq: vec![(0, d::oid(relid::PG_CLASS))] };
        let rows = PgAttribute.scan(&ctx, &req)?;
        assert_eq!(rows.len(), rel.columns.len() + 6);
        assert_eq!(cell(schema, &rows[0], "attname")?, &Datum::Text("tableoid".into()));
        assert_eq!(cell(schema, &rows[0], "attnum")?, &Datum::Int2(-6));
        assert_eq!(cell(schema, &rows[5], "attname")?, &Datum::Text("ctid".into()));
        assert_eq!(cell(schema, &rows[5], "attlen")?, &Datum::Int2(6));
        assert_eq!(cell(schema, &rows[5], "attalign")?, &Datum::Char(b's'));
        assert_eq!(cell(schema, &rows[6], "attnum")?, &Datum::Int2(1));
        Ok(())
    }

    #[test]
    fn views_have_no_system_columns() -> Result<()> {
        let env = CatalogEnv::with_default_path(CatalogSnapshot::builtin_only()?);
        let session = NullSessionInfo;
        let io = IoConfig::default();
        let ctx = ScanContext { env: &env, session: &session, io_config: &io };
        let view = builtin::SYSTEM_RELS.iter().find(|r| r.kind == b'v').ok_or_else(|| PgError::internal("no view"))?;
        let rows = PgAttribute.scan(&ctx, &ScanRequest { eq: vec![(0, d::oid(view.oid))] })?;
        assert_eq!(rows.len(), view.columns.len());
        Ok(())
    }

    #[test]
    fn unknown_relid_has_no_rows() -> Result<()> {
        let env = CatalogEnv::with_default_path(CatalogSnapshot::builtin_only()?);
        let session = NullSessionInfo;
        let io = IoConfig::default();
        let ctx = ScanContext { env: &env, session: &session, io_config: &io };
        let rows = PgAttribute.scan(&ctx, &ScanRequest { eq: vec![(0, d::oid(999_999))] })?;
        assert!(rows.is_empty());
        Ok(())
    }

    #[test]
    fn user_table_columns_and_defaults() -> Result<()> {
        let (mut kv, env) = testing::bootstrapped()?;
        let (_res, env) = testing::run(&mut kv, &env, |t| t.create_table(&table_spec("t")).map(|_| ()))?;
        let oid = env
            .snapshot()
            .class_in_namespace(builtin::PUBLIC_NAMESPACE, "t")
            .map(|c| c.oid())
            .ok_or_else(|| PgError::internal("table t missing"))?;
        let session = NullSessionInfo;
        let io = IoConfig::default();
        let ctx = ScanContext { env: &env, session: &session, io_config: &io };
        let schema = PgAttribute.schema()?;
        let rows = PgAttribute.scan(&ctx, &ScanRequest { eq: vec![(0, d::oid(oid))] })?;
        assert_eq!(rows.len(), 6 + 3);
        assert_eq!(cell(schema, &rows[6], "attname")?, &Datum::Text("id".into()));
        assert_eq!(cell(schema, &rows[8], "attname")?, &Datum::Text("b".into()));
        assert_eq!(cell(schema, &rows[8], "attlen")?, &Datum::Int2(-1));
        assert_eq!(cell(schema, &rows[8], "attstorage")?, &Datum::Char(b'x'));
        assert_eq!(cell(schema, &rows[7], "atthasdef")?, &Datum::Bool(true));
        assert_eq!(cell(schema, &rows[7], "attstattarget")?, &Datum::Null);

        let defs = PgAttrdef.scan(&ctx, &ScanRequest { eq: vec![(1, d::oid(oid))] })?;
        let ddef = PgAttrdef.schema()?;
        assert_eq!(defs.len(), 1);
        assert_eq!(cell(ddef, &defs[0], "adnum")?, &Datum::Int2(2));
        assert_eq!(cell(ddef, &defs[0], "adbin")?, &Datum::Text("42".into()));
        Ok(())
    }
}
