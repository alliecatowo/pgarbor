//! information_schema, part 2 (columns and types): `columns`, `element_types`, `domains`,
//! `domain_constraints`, `column_domain_usage`, `column_udt_usage`, `domain_udt_usage`,
//! `column_column_usage`. Lease CAT-41. Authoritative semantics: the CREATE VIEW of each name in
//! PG 18.6 `src/backend/catalog/information_schema.sql`; visibility, domain/Datum conventions and the
//! `RoleGraph` privilege test: see `is_core.rs`.
//!
//! ## Shared helper [`describe_type_for_info_schema`]
//! Everything that prints a column/parameter/attribute type uses the same derivation (PG's
//! `_pg_char_max_length`, `_pg_char_octet_length`, `_pg_numeric_precision(_radix)`,
//! `_pg_numeric_scale`, `_pg_datetime_precision`, `_pg_interval_type`, `_pg_truetypid/_pg_truetypmod`,
//! and the `data_type`/`udt_*` CASE expressions). Oracle examples (PG 18.6, `information_schema.columns`):
//! | declared type | data_type | char_max_len | char_octet_len | num_prec | radix | scale | udt_name |
//! |---|---|---|---|---|---|---|---|
//! | `int` | integer | | | 32 | 2 | 0 | int4 |
//! | `varchar(10)` | character varying | 10 | 40 | | | | varchar |
//! | `text` | text | | 1073741824 | | | | text |
//! | `numeric(10,2)` | numeric | | | 10 | 10 | 2 | numeric |
//! | `int[]` | ARRAY | | | | | | _int4 |
//! | enum `mood` | USER-DEFINED | | | | | | mood (udt_schema = enum's schema) |
//! | domain `pos` over int | integer (the BASE type) | | | 32 | 2 | 0 | int4 (+ domain_catalog/schema/name filled) |
//! | `timestamp(3)` | timestamp without time zone | | | | | | datetime_precision 3 |
//! `udt_schema` is the type's schema (`pg_catalog` for builtins); `udt_catalog` the database. Octet
//! length is `4 * n` for UTF8 `varchar(n)`/`char(n)` and 1073741824 for unbounded text. Typmod decoding
//! lives in `pgarbor_types::typmod`. `collation_name` is filled ONLY when the column's collation
//! differs from its type's default collation (`NULL` for default text columns - oracle).
//!
//! ## Relations
//! - **columns**: one row per non-dropped, non-system column of tables/views/matviews?/composite? -
//!   PG: relkind IN ('r','v','f','p') (NOT matviews, NOT composite), visible per `is_core.rs`,
//!   `attnum > 0`. Oracle for `create table t(id serial ..., a varchar(10) not null default 'x', ...)`:
//!   `ordinal_position` = attnum (dropped columns leave gaps), `column_default` = the default text
//!   (`nextval('t_id_seq'::regclass)`, `'x'::character varying`) - NULL for generated columns,
//!   `is_nullable` `NO`/`YES` from `not_null`, `dtd_identifier` = attnum as text,
//!   `is_self_referencing` `NO`, `is_identity` `YES`/`NO` with `identity_generation`
//!   (`ALWAYS`/`BY DEFAULT`), `identity_start/increment/maximum/minimum` (text of the identity sequence
//!   values) and `identity_cycle` `YES`/`NO` (NO for non-identity), `is_generated` `ALWAYS`/`NEVER`,
//!   `generation_expression` (the generation expression text, e.g. `((a)::text || 'y'::text)`),
//!   `is_updatable` `YES` for base-table columns (`YES` for views only if updatable: `NO`).
//!   `scope_*`, `maximum_cardinality`, `character_set_*` NULL (`character_set_name` is set for
//!   character types in PG: `character_set_catalog/schema` NULL, `character_set_name` = `UTF8`).
//! - **element_types**: one row per array-typed column/parameter/attribute: `object_catalog,
//!   object_schema, object_name, object_type` (`TABLE`, `USER-DEFINED TYPE`, `ROUTINE`, ...),
//!   `collection_type_identifier` (= `dtd_identifier`), `data_type` of the ELEMENT (+ the same
//!   detail columns), `udt_*` of the element, `dtd_identifier` = `a`-style as PG prints.
//! - **domains**: `domain_catalog/schema/name, data_type` (base type) + type-detail columns,
//!   `domain_default` (default text), `udt_*` of the base type, `dtd_identifier` `1`. Visible by
//!   `pg_has_role(typowner, 'USAGE') OR has_type_privilege(oid, 'USAGE')`.
//! - **domain_constraints**: every `Constraint` with `domain != 0`: `constraint_catalog/schema/name,
//!   domain_catalog/schema/name, is_deferrable` `NO`, `initially_deferred` `NO` (includes PG 18
//!   `NOT NULL` domain constraints).
//! - **column_domain_usage**: columns whose declared type is a domain: `domain_catalog/schema/name,
//!   table_catalog/schema/name, column_name`.
//! - **column_udt_usage**: every column of tables/views with its `udt_catalog/schema/name` (base type
//!   name for domains): oracle shows `int4`, `text`, ... for all columns.
//! - **domain_udt_usage**: `udt_catalog/schema/name` of each domain's base type + domain identity.
//! - **column_column_usage**: for each generated column, one row per column its expression depends
//!   on: `table_catalog/schema/name, column_name, dependent_column` (from `Column::default.expr.depends_on`
//!   column addresses of the same table).
//!
//! ## Implementation notes (PG 18.6 divergences and assumptions)
//! - `character_set_name` is NULL: the 18.6 `columns` view says `CAST(null AS sql_identifier)`.
//! - System relations (OID < 16384) with no stored ACL are treated as `SELECT`-able by PUBLIC (initdb
//!   grants `=r/postgres` on every system relation; the builtin `Relation`s carry no ACL).
//! - `is_updatable` is `YES` for tables/partitioned tables only (no auto-updatable views in v1).
//! - Builtin types are owned by the bootstrap superuser, which matters for the views that test
//!   `pg_has_role(typowner, 'USAGE')` only (`column_udt_usage`, `domain_udt_usage`, ...).
use super::rowbuild::{d, RowBuilder};
use super::{Row, ScanContext, ScanRequest, VirtualRelation};
use crate::acl::{self, AclObject, RoleGraph};
use crate::builtin::{self, relid, BOOTSTRAP_SUPERUSER};
use crate::env::CatalogEnv;
use crate::model::{
    acl_mode, Acl, Column, FuncKind, IdentityKind, ObjClass, RelKind, Relation, StoredExpr, TypeDef, TypeObj,
};
use crate::snapshot::{CatalogSnapshot, UserTypeRef};
use pgarbor_common::{Oid, PgError, Result, FIRST_NORMAL_OID};
use pgarbor_types::catalog_data::{self, ProcKind};
use pgarbor_types::typmod as pgtypmod;
use pgarbor_types::{names, oids, registry, Datum, TypeEnv};
use std::collections::HashMap;

/// Type-derived columns shared by columns/attributes/parameters/domains/element_types
/// (module docs table). `None` = SQL NULL.
// CONTRACT
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct InfoSchemaType {
    pub data_type: String,
    pub character_maximum_length: Option<i32>,
    pub character_octet_length: Option<i32>,
    pub numeric_precision: Option<i32>,
    pub numeric_precision_radix: Option<i32>,
    pub numeric_scale: Option<i32>,
    pub datetime_precision: Option<i32>,
    pub interval_type: Option<String>,
    pub interval_precision: Option<i32>,
    /// Schema and name of the (base) UDT: `pg_catalog`/`int4`.
    pub udt_schema: String,
    pub udt_name: String,
    /// Set when the declared type is a domain.
    pub domain_schema: Option<String>,
    pub domain_name: Option<String>,
    /// Element type description for arrays (`data_type == "ARRAY"`).
    pub element: Option<Box<InfoSchemaType>>,
}

/// PG `pg_encoding_max_length` for the encodings a database can have.
fn encoding_max_length(encoding: i32) -> i32 {
    match encoding {
        // SQL_ASCII
        0 => 1,
        // EUC_JP, EUC_CN, EUC_KR, JOHAB
        1 | 2 | 3 | 5 => 3,
        // LATIN*, ISO_8859_*, WIN*, KOI8*
        8..=29 => 1,
        // UTF8, EUC_TW, MULE_INTERNAL, GB18030, ...
        _ => 4,
    }
}

/// `_pg_char_max_length`.
fn pg_char_max_length(type_oid: Oid, typmod: i32) -> Option<i32> {
    if typmod == -1 {
        return None;
    }
    match type_oid {
        oids::BPCHAR | oids::VARCHAR => Some(typmod - 4),
        oids::BIT | oids::VARBIT => Some(typmod),
        _ => None,
    }
}

/// `_pg_char_octet_length`.
fn pg_char_octet_length(type_oid: Oid, typmod: i32, enc_max: i32) -> Option<i32> {
    match type_oid {
        oids::TEXT | oids::BPCHAR | oids::VARCHAR => {
            if typmod == -1 {
                Some(1 << 30)
            } else {
                pg_char_max_length(type_oid, typmod).map(|n| n.saturating_mul(enc_max))
            }
        }
        _ => None,
    }
}

/// `_pg_numeric_precision`.
fn pg_numeric_precision(type_oid: Oid, typmod: i32) -> Option<i32> {
    match type_oid {
        oids::INT2 => Some(16),
        oids::INT4 => Some(32),
        oids::INT8 => Some(64),
        oids::NUMERIC => {
            if typmod == -1 {
                None
            } else {
                Some(((typmod - 4) >> 16) & 0xFFFF)
            }
        }
        oids::FLOAT4 => Some(24),
        oids::FLOAT8 => Some(53),
        _ => None,
    }
}

/// `_pg_numeric_precision_radix`.
fn pg_numeric_precision_radix(type_oid: Oid) -> Option<i32> {
    match type_oid {
        oids::INT2 | oids::INT4 | oids::INT8 | oids::FLOAT4 | oids::FLOAT8 => Some(2),
        oids::NUMERIC => Some(10),
        _ => None,
    }
}

/// `_pg_numeric_scale`.
fn pg_numeric_scale(type_oid: Oid, typmod: i32) -> Option<i32> {
    match type_oid {
        oids::INT2 | oids::INT4 | oids::INT8 => Some(0),
        oids::NUMERIC => {
            if typmod == -1 {
                None
            } else {
                Some((typmod - 4) & 0xFFFF)
            }
        }
        _ => None,
    }
}

/// `_pg_datetime_precision`.
fn pg_datetime_precision(type_oid: Oid, typmod: i32) -> Option<i32> {
    match type_oid {
        oids::DATE => Some(0),
        oids::TIME | oids::TIMESTAMP | oids::TIMESTAMPTZ | oids::TIMETZ => Some(if typmod < 0 { 6 } else { typmod }),
        oids::INTERVAL => Some(if typmod < 0 || typmod & 0xFFFF == 0xFFFF { 6 } else { typmod & 0xFFFF }),
        _ => None,
    }
}

/// `_pg_interval_type`: `upper(substring(format_type(interval, typmod) similar 'interval[()0-9]* #"%#"'
/// escape '#'))`, i.e. the text after `interval`, any `(`/`)`/digits and one space; NULL when the
/// type output has no field list.
fn pg_interval_type(type_oid: Oid, typmod: i32) -> Result<Option<String>> {
    if type_oid != oids::INTERVAL {
        return Ok(None);
    }
    let suffix = pgtypmod::typmod_out(oids::INTERVAL, typmod)?;
    let rest = suffix.trim_start_matches(|c: char| c == '(' || c == ')' || c.is_ascii_digit());
    Ok(rest.strip_prefix(' ').map(|r| r.to_uppercase()))
}

fn namespace_name(snap: &CatalogSnapshot, oid: Oid) -> Result<&str> {
    snap.namespace(oid)
        .map(|n| n.name.as_str())
        .ok_or_else(|| PgError::internal(format!("cache lookup failed for namespace {oid}")))
}

/// `data_type` of a type with PG's `CASE` (ARRAY / pg_catalog name / USER-DEFINED).
fn data_type_text(env: &CatalogEnv, oid: Oid, info: &registry::TypeInfo) -> Result<String> {
    if info.elem != 0 && info.len == -1 {
        Ok("ARRAY".to_string())
    } else if info.namespace == oids::PG_CATALOG_NAMESPACE {
        names::format_type(env, oid, -1)
    } else {
        Ok("USER-DEFINED".to_string())
    }
}

/// Derive [`InfoSchemaType`] for `(type_oid, typmod)` (module docs). Unknown type -> XX000
/// `cache lookup failed for type <oid>`.
// CONTRACT
pub fn describe_type_for_info_schema(env: &CatalogEnv, type_oid: Oid, typmod: i32) -> Result<InfoSchemaType> {
    let snap = env.snapshot();
    let info = registry::type_info(env, type_oid)?;
    let is_domain = info.typtype == b'd';
    // `_pg_truetypid` / `_pg_truetypmod`, and the type that decides data_type / udt_*.
    let (true_oid, true_mod, base) = if is_domain {
        let b = registry::type_info(env, info.basetype)?;
        (info.basetype, info.typmod, Some(b))
    } else {
        (type_oid, typmod, None)
    };
    let shape = base.as_ref().unwrap_or(&info);

    let data_type = data_type_text(env, true_oid, shape)?;
    let enc_max = snap.database(env.database()).map(|db| encoding_max_length(db.encoding)).unwrap_or(4);

    let element = if data_type == "ARRAY" {
        let elem_info = registry::type_info(env, shape.elem)?;
        let elem_data_type = if elem_info.namespace == oids::PG_CATALOG_NAMESPACE {
            names::format_type(env, shape.elem, -1)?
        } else {
            "USER-DEFINED".to_string()
        };
        Some(Box::new(InfoSchemaType {
            data_type: elem_data_type,
            udt_schema: namespace_name(snap, elem_info.namespace)?.to_string(),
            udt_name: elem_info.name.to_string(),
            ..InfoSchemaType::default()
        }))
    } else {
        None
    };

    Ok(InfoSchemaType {
        data_type,
        character_maximum_length: pg_char_max_length(true_oid, true_mod),
        character_octet_length: pg_char_octet_length(true_oid, true_mod, enc_max),
        numeric_precision: pg_numeric_precision(true_oid, true_mod),
        numeric_precision_radix: pg_numeric_precision_radix(true_oid),
        numeric_scale: pg_numeric_scale(true_oid, true_mod),
        datetime_precision: pg_datetime_precision(true_oid, true_mod),
        interval_type: pg_interval_type(true_oid, true_mod)?,
        interval_precision: None,
        udt_schema: namespace_name(snap, shape.namespace)?.to_string(),
        udt_name: shape.name.to_string(),
        domain_schema: if is_domain { Some(namespace_name(snap, info.namespace)?.to_string()) } else { None },
        domain_name: if is_domain { Some(info.name.to_string()) } else { None },
        element,
    })
}

// ---------------------------------------------------------------------------------------------
// shared scan machinery
// ---------------------------------------------------------------------------------------------

/// Per-scan memo of [`describe_type_for_info_schema`].
#[derive(Default)]
struct TypeCache {
    map: HashMap<(Oid, i32), InfoSchemaType>,
}

impl TypeCache {
    fn get(&mut self, env: &CatalogEnv, oid: Oid, typmod: i32) -> Result<&InfoSchemaType> {
        let key = (oid, typmod);
        if !self.map.contains_key(&key) {
            let v = describe_type_for_info_schema(env, oid, typmod)?;
            self.map.insert(key, v);
        }
        match self.map.get(&key) {
            Some(v) => Ok(v),
            None => Err(PgError::internal("information_schema type cache lost an entry")),
        }
    }
}

/// Current-user view of the catalog shared by all producers of this file.
struct Cx<'a> {
    env: &'a CatalogEnv,
    snap: &'a CatalogSnapshot,
    graph: RoleGraph,
    user: Oid,
    db: String,
}

impl<'a> Cx<'a> {
    fn new(ctx: &'a ScanContext<'a>) -> Result<Cx<'a>> {
        let env: &'a CatalogEnv = ctx.env;
        let snap = env.snapshot();
        let db = snap
            .database(env.database())
            .map(|d| d.name.clone())
            .ok_or_else(|| PgError::internal(format!("cache lookup failed for database {}", env.database())))?;
        Ok(Cx { env, snap, graph: RoleGraph::build(snap, env.database())?, user: env.current_user(), db })
    }

    fn ns(&self, oid: Oid) -> Result<&'a str> {
        namespace_name(self.snap, oid)
    }

    /// `pg_has_role(owner, 'USAGE')`.
    fn has_role(&self, owner: Oid) -> Result<bool> {
        self.graph.has_privs_of_role(self.user, owner)
    }

    /// `pg_has_role(owner, 'USAGE') OR has_type_privilege(.., 'USAGE')`.
    fn type_usable(&self, owner: Oid, acl: Option<&Acl>) -> Result<bool> {
        if self.has_role(owner)? {
            return Ok(true);
        }
        let a = acl::effective_acl(AclObject::Type, owner, acl)?;
        Ok(acl::privilege_mask(&self.graph, self.user, owner, &a, acl_mode::USAGE, false)? != 0)
    }

    /// `pg_has_role(owner, 'USAGE') OR has_function_privilege(.., 'EXECUTE')`.
    fn function_executable(&self, owner: Oid, acl: Option<&Acl>) -> Result<bool> {
        if self.has_role(owner)? {
            return Ok(true);
        }
        let a = acl::effective_acl(AclObject::Function, owner, acl)?;
        Ok(acl::privilege_mask(&self.graph, self.user, owner, &a, acl_mode::EXECUTE, false)? != 0)
    }

    /// Does the current user hold any of SELECT/INSERT/UPDATE/REFERENCES on the whole relation?
    fn table_any_privilege(&self, rel: &Relation) -> Result<bool> {
        if rel.oid < FIRST_NORMAL_OID && rel.acl.is_none() {
            // initdb grants SELECT on every system relation to PUBLIC.
            return Ok(true);
        }
        let a = acl::effective_acl(AclObject::Table, rel.owner, rel.acl.as_ref())?;
        let mask = acl_mode::SELECT | acl_mode::INSERT | acl_mode::UPDATE | acl_mode::REFERENCES;
        Ok(acl::privilege_mask(&self.graph, self.user, rel.owner, &a, mask, false)? != 0)
    }

    /// Column-level privilege (the table-level one is checked by the caller).
    fn column_any_privilege(&self, rel: &Relation, col: &Column) -> Result<bool> {
        match &col.acl {
            Some(a) => {
                let mask = acl_mode::SELECT | acl_mode::INSERT | acl_mode::UPDATE | acl_mode::REFERENCES;
                Ok(acl::privilege_mask(&self.graph, self.user, rel.owner, a, mask, false)? != 0)
            }
            None => Ok(false),
        }
    }

    /// `pg_has_role(relowner) OR <table-level column privilege>`: true when every column of `rel`
    /// is visible without looking at column ACLs.
    fn relation_columns_visible(&self, rel: &Relation) -> Result<bool> {
        Ok(self.has_role(rel.owner)? || self.table_any_privilege(rel)?)
    }

    /// Owner of a type OID (builtin types: the bootstrap superuser).
    fn type_owner(&self, oid: Oid) -> Oid {
        match self.snap.user_type(oid) {
            Some(UserTypeRef::Type(o)) | Some(UserTypeRef::TypeArray(o)) => {
                self.snap.type_obj(o).map(|t| t.owner).unwrap_or(BOOTSTRAP_SUPERUSER)
            }
            Some(UserTypeRef::RowType(r)) | Some(UserTypeRef::RowTypeArray(r)) => {
                self.snap.relation(r).map(|r| r.owner).unwrap_or(BOOTSTRAP_SUPERUSER)
            }
            None => BOOTSTRAP_SUPERUSER,
        }
    }

    /// `coalesce(bt.typowner, t.typowner)`: the base type's owner for a domain.
    fn udt_owner(&self, oid: Oid) -> Result<Oid> {
        let info = registry::type_info(self.env, oid)?;
        if info.typtype == b'd' {
            Ok(self.type_owner(info.basetype))
        } else {
            Ok(self.type_owner(oid))
        }
    }

    /// `(collation schema, collation name)` unless the collation is NULL/default.
    fn collation(&self, oid: Oid) -> Result<Option<(&'a str, &'static str)>> {
        if oid == 0 || oid == 100 {
            return Ok(None);
        }
        match builtin::builtin_collation(oid) {
            Some(c) => {
                if c.namespace == oids::PG_CATALOG_NAMESPACE && c.name == "default" {
                    Ok(None)
                } else {
                    Ok(Some((self.ns(c.namespace)?, c.name)))
                }
            }
            None => Ok(None),
        }
    }

    /// Builtin then user relations, ascending OID.
    fn relations(&self) -> Vec<&'a Relation> {
        let mut v: Vec<&'a Relation> = builtin::builtin_relations().iter().collect();
        v.extend(self.snap.user_relations());
        v
    }

    /// `t.typtype = 'd' AND t.typnotnull` for a column type.
    fn domain_not_null(&self, oid: Oid) -> bool {
        match self.snap.type_obj(oid) {
            Some(TypeObj { def: TypeDef::Domain { not_null, .. }, .. }) => *not_null,
            _ => false,
        }
    }
}

fn is_column_relkind(k: RelKind) -> bool {
    matches!(k, RelKind::Table | RelKind::View | RelKind::ForeignTable | RelKind::PartitionedTable)
}

fn yes_no(b: bool) -> &'static str {
    if b {
        "YES"
    } else {
        "NO"
    }
}

fn int_opt(v: Option<i32>) -> Datum {
    d::opt_int4(v)
}

/// Text equality filter on column `name`, if the request carries one.
fn text_filter<'r>(req: &'r ScanRequest, schema: &builtin::SystemRel, name: &str) -> Option<&'r str> {
    let idx = schema.columns.iter().position(|c| c.name == name)?;
    match req.eq_on(idx) {
        Some(Datum::Text(s)) => Some(s.as_str()),
        _ => None,
    }
}

/// Sets the type-detail columns shared by columns/domains/element_types/attributes.
fn put_type(b: &mut RowBuilder, t: &InfoSchemaType) {
    b.set("data_type", d::text(&t.data_type))
        .set("character_maximum_length", int_opt(t.character_maximum_length))
        .set("character_octet_length", int_opt(t.character_octet_length))
        .set("numeric_precision", int_opt(t.numeric_precision))
        .set("numeric_precision_radix", int_opt(t.numeric_precision_radix))
        .set("numeric_scale", int_opt(t.numeric_scale))
        .set("datetime_precision", int_opt(t.datetime_precision))
        .set("interval_type", d::opt_text(t.interval_type.as_deref()))
        .set("interval_precision", int_opt(t.interval_precision));
}

fn put_collation(b: &mut RowBuilder, db: &str, coll: Option<(&str, &str)>) {
    if let Some((schema, name)) = coll {
        b.set("collation_catalog", d::text(db))
            .set("collation_schema", d::text(schema))
            .set("collation_name", d::text(name));
    }
}

// ---------------------------------------------------------------------------------------------
// columns
// ---------------------------------------------------------------------------------------------

/// Producer for `information_schema.columns`.
#[derive(Clone, Copy, Debug)]
pub struct IsColumns;

impl VirtualRelation for IsColumns {
    fn oid(&self) -> Oid {
        relid::IS_COLUMNS
    }
    fn scan(&self, ctx: &ScanContext, req: &ScanRequest) -> Result<Vec<Row>> {
        let schema = self.schema()?;
        let cx = Cx::new(ctx)?;
        let want_table = text_filter(req, schema, "table_name");
        let want_schema = text_filter(req, schema, "table_schema");
        let mut types = TypeCache::default();
        let mut rows = Vec::new();
        for rel in cx.relations() {
            if !is_column_relkind(rel.kind) {
                continue;
            }
            if want_table.is_some_and(|w| w != rel.name) {
                continue;
            }
            let nsname = cx.ns(rel.namespace)?;
            if want_schema.is_some_and(|w| w != nsname) {
                continue;
            }
            let all_visible = cx.relation_columns_visible(rel)?;
            for col in &rel.columns {
                if col.dropped || col.attnum <= 0 {
                    continue;
                }
                if !all_visible && !cx.column_any_privilege(rel, col)? {
                    continue;
                }
                let t = types.get(cx.env, col.type_id.oid, col.type_id.typmod)?;
                let mut b = RowBuilder::new(schema);
                b.set("table_catalog", d::text(&cx.db))
                    .set("table_schema", d::text(nsname))
                    .set("table_name", d::text(&rel.name))
                    .set("column_name", d::text(&col.name))
                    .set("ordinal_position", d::int4(i32::from(col.attnum)));
                let expr = col.default.as_ref().map(|cd| cd.expr.sql.as_str());
                if col.generated.is_none() {
                    b.set("column_default", d::opt_text(expr));
                } else {
                    b.set("generation_expression", d::opt_text(expr));
                }
                let not_null = col.not_null || (t.domain_name.is_some() && cx.domain_not_null(col.type_id.oid));
                b.set("is_nullable", d::text(yes_no(!not_null)));
                put_type(&mut b, t);
                put_collation(&mut b, &cx.db, cx.collation(col.collation)?);
                if let (Some(ds), Some(dn)) = (&t.domain_schema, &t.domain_name) {
                    b.set("domain_catalog", d::text(&cx.db))
                        .set("domain_schema", d::text(ds))
                        .set("domain_name", d::text(dn));
                }
                b.set("udt_catalog", d::text(&cx.db))
                    .set("udt_schema", d::text(&t.udt_schema))
                    .set("udt_name", d::text(&t.udt_name))
                    .set("dtd_identifier", d::text(&col.attnum.to_string()))
                    .set("is_self_referencing", d::text("NO"));
                match &col.identity {
                    Some(idn) => {
                        let generation = match idn.kind {
                            IdentityKind::Always => "ALWAYS",
                            IdentityKind::ByDefault => "BY DEFAULT",
                        };
                        b.set("is_identity", d::text("YES")).set("identity_generation", d::text(generation));
                        match cx.snap.sequence(idn.sequence) {
                            Some(seq) => {
                                b.set("identity_start", d::text(&seq.start.to_string()))
                                    .set("identity_increment", d::text(&seq.increment.to_string()))
                                    .set("identity_maximum", d::text(&seq.max.to_string()))
                                    .set("identity_minimum", d::text(&seq.min.to_string()))
                                    .set("identity_cycle", d::text(yes_no(seq.cycle)));
                            }
                            None => {
                                b.set("identity_cycle", d::text("NO"));
                            }
                        }
                    }
                    None => {
                        b.set("is_identity", d::text("NO")).set("identity_cycle", d::text("NO"));
                    }
                }
                b.set("is_generated", d::text(if col.generated.is_some() { "ALWAYS" } else { "NEVER" }))
                    .set(
                        "is_updatable",
                        d::text(yes_no(matches!(rel.kind, RelKind::Table | RelKind::PartitionedTable))),
                    );
                rows.push(b.finish()?);
            }
        }
        Ok(rows)
    }
}

// ---------------------------------------------------------------------------------------------
// domains (shared by domains, domain_constraints, domain_udt_usage, element_types)
// ---------------------------------------------------------------------------------------------

struct DomainRef<'a> {
    oid: Oid,
    name: &'a str,
    namespace: Oid,
    owner: Oid,
    acl: Option<&'a Acl>,
    default: Option<&'a str>,
}

/// Every domain: builtin (information_schema's five) then user, ascending OID.
fn all_domains(snap: &CatalogSnapshot) -> Vec<DomainRef<'_>> {
    let mut v = Vec::new();
    for t in catalog_data::TYPES.iter().filter(|t| t.typtype == b'd') {
        // `time_stamp` is the only builtin domain with a default (`DEFAULT current_timestamp(2)`).
        let default = if t.name == "time_stamp" && t.namespace == oids::INFORMATION_SCHEMA_NAMESPACE {
            Some("CURRENT_TIMESTAMP(2)")
        } else {
            None
        };
        v.push(DomainRef { oid: t.oid, name: t.name, namespace: t.namespace, owner: BOOTSTRAP_SUPERUSER, acl: None, default });
    }
    for t in snap.data().types.values() {
        if let TypeDef::Domain { default, .. } = &t.def {
            v.push(DomainRef {
                oid: t.oid,
                name: t.name.as_str(),
                namespace: t.namespace,
                owner: t.owner,
                acl: t.acl.as_ref(),
                default: default.as_ref().map(|e: &StoredExpr| e.sql.as_str()),
            });
        }
    }
    v
}

/// Producer for `information_schema.domains`.
#[derive(Clone, Copy, Debug)]
pub struct IsDomains;

impl VirtualRelation for IsDomains {
    fn oid(&self) -> Oid {
        relid::IS_DOMAINS
    }
    fn scan(&self, ctx: &ScanContext, _req: &ScanRequest) -> Result<Vec<Row>> {
        let schema = self.schema()?;
        let cx = Cx::new(ctx)?;
        let mut rows = Vec::new();
        for dom in all_domains(cx.snap) {
            if !cx.type_usable(dom.owner, dom.acl)? {
                continue;
            }
            let info = registry::type_info(cx.env, dom.oid)?;
            let t = describe_type_for_info_schema(cx.env, dom.oid, -1)?;
            let mut b = RowBuilder::new(schema);
            b.set("domain_catalog", d::text(&cx.db))
                .set("domain_schema", d::text(cx.ns(dom.namespace)?))
                .set("domain_name", d::text(dom.name));
            put_type(&mut b, &t);
            put_collation(&mut b, &cx.db, cx.collation(info.collation)?);
            b.set("domain_default", d::opt_text(dom.default))
                .set("udt_catalog", d::text(&cx.db))
                .set("udt_schema", d::text(&t.udt_schema))
                .set("udt_name", d::text(&t.udt_name))
                .set("dtd_identifier", d::text("1"));
            rows.push(b.finish()?);
        }
        Ok(rows)
    }
}

/// Producer for `information_schema.domain_constraints`.
#[derive(Clone, Copy, Debug)]
pub struct IsDomainConstraints;

impl VirtualRelation for IsDomainConstraints {
    fn oid(&self) -> Oid {
        relid::IS_DOMAIN_CONSTRAINTS
    }
    fn scan(&self, ctx: &ScanContext, _req: &ScanRequest) -> Result<Vec<Row>> {
        let schema = self.schema()?;
        let cx = Cx::new(ctx)?;
        let mut rows = Vec::new();
        let domains = all_domains(cx.snap);
        let push = |rows: &mut Vec<Row>,
                    con_schema: &str,
                    con_name: &str,
                    dom: &DomainRef<'_>,
                    deferrable: bool,
                    deferred: bool|
         -> Result<()> {
            let mut b = RowBuilder::new(schema);
            b.set("constraint_catalog", d::text(&cx.db))
                .set("constraint_schema", d::text(con_schema))
                .set("constraint_name", d::text(con_name))
                .set("domain_catalog", d::text(&cx.db))
                .set("domain_schema", d::text(cx.ns(dom.namespace)?))
                .set("domain_name", d::text(dom.name))
                .set("is_deferrable", d::text(yes_no(deferrable)))
                .set("initially_deferred", d::text(yes_no(deferred)));
            rows.push(b.finish()?);
            Ok(())
        };
        // information_schema's own domain CHECK constraints (pg_constraint rows in PG).
        for (dom_name, con_name) in
            [("cardinal_number", "cardinal_number_domain_check"), ("yes_or_no", "yes_or_no_check")]
        {
            if let Some(dom) =
                domains.iter().find(|x| x.name == dom_name && x.namespace == oids::INFORMATION_SCHEMA_NAMESPACE)
            {
                if cx.type_usable(dom.owner, dom.acl)? {
                    let con_schema = cx.ns(oids::INFORMATION_SCHEMA_NAMESPACE)?;
                    push(&mut rows, con_schema, con_name, dom, false, false)?;
                }
            }
        }
        for c in cx.snap.data().constraints.values() {
            if c.domain == 0 {
                continue;
            }
            let dom = match domains.iter().find(|x| x.oid == c.domain) {
                Some(x) => x,
                None => continue,
            };
            if !cx.type_usable(dom.owner, dom.acl)? {
                continue;
            }
            push(&mut rows, cx.ns(c.namespace)?, &c.name, dom, c.deferrable, c.initially_deferred)?;
        }
        Ok(rows)
    }
}

/// Producer for `information_schema.domain_udt_usage`.
#[derive(Clone, Copy, Debug)]
pub struct IsDomainUdtUsage;

impl VirtualRelation for IsDomainUdtUsage {
    fn oid(&self) -> Oid {
        relid::IS_DOMAIN_UDT_USAGE
    }
    fn scan(&self, ctx: &ScanContext, _req: &ScanRequest) -> Result<Vec<Row>> {
        let schema = self.schema()?;
        let cx = Cx::new(ctx)?;
        let mut rows = Vec::new();
        for dom in all_domains(cx.snap) {
            let info = registry::type_info(cx.env, dom.oid)?;
            let base = registry::type_info(cx.env, info.basetype)?;
            if !cx.has_role(cx.type_owner(info.basetype))? {
                continue;
            }
            let mut b = RowBuilder::new(schema);
            b.set("udt_catalog", d::text(&cx.db))
                .set("udt_schema", d::text(cx.ns(base.namespace)?))
                .set("udt_name", d::text(&base.name))
                .set("domain_catalog", d::text(&cx.db))
                .set("domain_schema", d::text(cx.ns(dom.namespace)?))
                .set("domain_name", d::text(dom.name));
            rows.push(b.finish()?);
        }
        Ok(rows)
    }
}

// ---------------------------------------------------------------------------------------------
// column_domain_usage / column_udt_usage / column_column_usage
// ---------------------------------------------------------------------------------------------

/// Producer for `information_schema.column_domain_usage`.
#[derive(Clone, Copy, Debug)]
pub struct IsColumnDomainUsage;

impl VirtualRelation for IsColumnDomainUsage {
    fn oid(&self) -> Oid {
        relid::IS_COLUMN_DOMAIN_USAGE
    }
    fn scan(&self, ctx: &ScanContext, req: &ScanRequest) -> Result<Vec<Row>> {
        let schema = self.schema()?;
        let cx = Cx::new(ctx)?;
        let want_table = text_filter(req, schema, "table_name");
        let mut rows = Vec::new();
        for rel in cx.relations() {
            if !is_column_relkind(rel.kind) || want_table.is_some_and(|w| w != rel.name) {
                continue;
            }
            let nsname = cx.ns(rel.namespace)?;
            for col in &rel.columns {
                if col.dropped || col.attnum <= 0 {
                    continue;
                }
                let info = registry::type_info(cx.env, col.type_id.oid)?;
                if info.typtype != b'd' || !cx.has_role(cx.type_owner(col.type_id.oid))? {
                    continue;
                }
                let mut b = RowBuilder::new(schema);
                b.set("domain_catalog", d::text(&cx.db))
                    .set("domain_schema", d::text(cx.ns(info.namespace)?))
                    .set("domain_name", d::text(&info.name))
                    .set("table_catalog", d::text(&cx.db))
                    .set("table_schema", d::text(nsname))
                    .set("table_name", d::text(&rel.name))
                    .set("column_name", d::text(&col.name));
                rows.push(b.finish()?);
            }
        }
        Ok(rows)
    }
}

/// Producer for `information_schema.column_udt_usage`.
#[derive(Clone, Copy, Debug)]
pub struct IsColumnUdtUsage;

impl VirtualRelation for IsColumnUdtUsage {
    fn oid(&self) -> Oid {
        relid::IS_COLUMN_UDT_USAGE
    }
    fn scan(&self, ctx: &ScanContext, req: &ScanRequest) -> Result<Vec<Row>> {
        let schema = self.schema()?;
        let cx = Cx::new(ctx)?;
        let want_table = text_filter(req, schema, "table_name");
        let mut rows = Vec::new();
        for rel in cx.relations() {
            if !is_column_relkind(rel.kind) || want_table.is_some_and(|w| w != rel.name) {
                continue;
            }
            let nsname = cx.ns(rel.namespace)?;
            for col in &rel.columns {
                if col.dropped || col.attnum <= 0 {
                    continue;
                }
                if !cx.has_role(cx.udt_owner(col.type_id.oid)?)? {
                    continue;
                }
                let info = registry::type_info(cx.env, col.type_id.oid)?;
                let shape = if info.typtype == b'd' { registry::type_info(cx.env, info.basetype)? } else { info };
                let mut b = RowBuilder::new(schema);
                b.set("udt_catalog", d::text(&cx.db))
                    .set("udt_schema", d::text(cx.ns(shape.namespace)?))
                    .set("udt_name", d::text(&shape.name))
                    .set("table_catalog", d::text(&cx.db))
                    .set("table_schema", d::text(nsname))
                    .set("table_name", d::text(&rel.name))
                    .set("column_name", d::text(&col.name));
                rows.push(b.finish()?);
            }
        }
        Ok(rows)
    }
}

/// Producer for `information_schema.column_column_usage`.
#[derive(Clone, Copy, Debug)]
pub struct IsColumnColumnUsage;

impl VirtualRelation for IsColumnColumnUsage {
    fn oid(&self) -> Oid {
        relid::IS_COLUMN_COLUMN_USAGE
    }
    fn scan(&self, ctx: &ScanContext, req: &ScanRequest) -> Result<Vec<Row>> {
        let schema = self.schema()?;
        let cx = Cx::new(ctx)?;
        let want_table = text_filter(req, schema, "table_name");
        let mut rows = Vec::new();
        // Builtin relations have no generated columns, so only user relations matter.
        for rel in cx.snap.user_relations() {
            if want_table.is_some_and(|w| w != rel.name) {
                continue;
            }
            if !rel.columns.iter().any(|c| c.generated.is_some() && !c.dropped) || !cx.has_role(rel.owner)? {
                continue;
            }
            let nsname = cx.ns(rel.namespace)?;
            let mut seen: Vec<(i16, i16)> = Vec::new();
            for ad in &rel.columns {
                if ad.dropped || ad.generated.is_none() {
                    continue;
                }
                let cd = match &ad.default {
                    Some(cd) => cd,
                    None => continue,
                };
                for dep in &cd.expr.depends_on {
                    if dep.class != ObjClass::Relation || dep.oid != rel.oid || dep.sub_id <= 0 {
                        continue;
                    }
                    let ac = match rel.columns.iter().find(|c| i32::from(c.attnum) == dep.sub_id) {
                        Some(c) if !c.dropped && c.attnum != ad.attnum => c,
                        _ => continue,
                    };
                    if seen.contains(&(ac.attnum, ad.attnum)) {
                        continue;
                    }
                    seen.push((ac.attnum, ad.attnum));
                    let mut b = RowBuilder::new(schema);
                    b.set("table_catalog", d::text(&cx.db))
                        .set("table_schema", d::text(nsname))
                        .set("table_name", d::text(&rel.name))
                        .set("column_name", d::text(&ac.name))
                        .set("dependent_column", d::text(&ad.name));
                    rows.push(b.finish()?);
                }
            }
        }
        Ok(rows)
    }
}

// ---------------------------------------------------------------------------------------------
// element_types
// ---------------------------------------------------------------------------------------------

/// PG `nameconcatoid(name, oid)`: `<name>_<oid>`, the name clipped (on a char boundary) so the
/// result fits `NAMEDATALEN - 1` bytes.
fn name_concat_oid(name: &str, oid: Oid) -> String {
    let suffix = format!("_{oid}");
    let avail = 63usize.saturating_sub(suffix.len());
    let mut end = name.len().min(avail);
    while end > 0 && !name.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}{}", &name[..end], suffix)
}

/// Element type of an array type, looking through domains (PG copies `typelem` into domains).
fn array_elem_of(env: &CatalogEnv, oid: Oid) -> Option<Oid> {
    let mut info = env.type_info(oid)?;
    for _ in 0..16 {
        if info.typtype != b'd' {
            break;
        }
        info = env.type_info(info.basetype)?;
    }
    if info.elem != 0 && info.len == -1 {
        Some(info.elem)
    } else {
        None
    }
}

/// One row of the inner UNION of the `element_types` view.
struct ElemSource<'a> {
    schema: &'a str,
    name: String,
    object_type: &'static str,
    dtd: i32,
    type_oid: Oid,
    collation: Oid,
}

/// Producer for `information_schema.element_types`.
#[derive(Clone, Copy, Debug)]
pub struct IsElementTypes;

impl VirtualRelation for IsElementTypes {
    fn oid(&self) -> Oid {
        relid::IS_ELEMENT_TYPES
    }
    fn scan(&self, ctx: &ScanContext, req: &ScanRequest) -> Result<Vec<Row>> {
        let schema = self.schema()?;
        let cx = Cx::new(ctx)?;
        let want_name = text_filter(req, schema, "object_name");
        let mut sources: Vec<ElemSource<'_>> = Vec::new();

        // columns and attributes
        for rel in cx.relations() {
            let composite = rel.kind == RelKind::CompositeType;
            if !composite && !is_column_relkind(rel.kind) {
                continue;
            }
            if want_name.is_some_and(|w| w != rel.name) {
                continue;
            }
            let all_visible =
                if composite { cx.type_usable(rel.owner, None)? } else { cx.relation_columns_visible(rel)? };
            let nsname = cx.ns(rel.namespace)?;
            for col in &rel.columns {
                if col.dropped || col.attnum <= 0 {
                    continue;
                }
                if !all_visible && !cx.column_any_privilege(rel, col)? {
                    continue;
                }
                sources.push(ElemSource {
                    schema: nsname,
                    name: rel.name.clone(),
                    object_type: if composite { "USER-DEFINED TYPE" } else { "TABLE" },
                    dtd: i32::from(col.attnum),
                    type_oid: col.type_id.oid,
                    collation: col.collation,
                });
            }
        }

        // domains
        for dom in all_domains(cx.snap) {
            if want_name.is_some_and(|w| w != dom.name) || !cx.type_usable(dom.owner, dom.acl)? {
                continue;
            }
            let info = registry::type_info(cx.env, dom.oid)?;
            sources.push(ElemSource {
                schema: cx.ns(dom.namespace)?,
                name: dom.name.to_string(),
                object_type: "DOMAIN",
                dtd: 1,
                type_oid: info.basetype,
                collation: info.collation,
            });
        }

        // routine parameters and result types
        for f in cx.snap.data().functions.values() {
            let name = name_concat_oid(&f.name, f.oid);
            if want_name.is_some_and(|w| w != name) || !cx.function_executable(f.owner, f.acl.as_ref())? {
                continue;
            }
            let nsname = cx.ns(f.namespace)?;
            for (i, a) in f.args.iter().enumerate() {
                sources.push(ElemSource {
                    schema: nsname,
                    name: name.clone(),
                    object_type: "ROUTINE",
                    dtd: i as i32 + 1,
                    type_oid: a.type_oid,
                    collation: 0,
                });
            }
            if f.kind != FuncKind::Procedure {
                sources.push(ElemSource {
                    schema: nsname,
                    name,
                    object_type: "ROUTINE",
                    dtd: 0,
                    type_oid: f.ret,
                    collation: 0,
                });
            }
        }
        for p in catalog_data::PROCS.iter() {
            let args: &[Oid] = if p.all_arg_types.is_empty() { p.arg_types } else { p.all_arg_types };
            let has_result = p.kind != ProcKind::Procedure;
            // Cheap pre-filter: skip routines whose types are all non-arrays before building names.
            if !args.iter().any(|t| array_elem_of(cx.env, *t).is_some())
                && !(has_result && array_elem_of(cx.env, p.ret).is_some())
            {
                continue;
            }
            let name = name_concat_oid(p.name, p.oid);
            if want_name.is_some_and(|w| w != name) {
                continue;
            }
            let nsname = cx.ns(p.namespace)?;
            for (i, t) in args.iter().enumerate() {
                sources.push(ElemSource {
                    schema: nsname,
                    name: name.clone(),
                    object_type: "ROUTINE",
                    dtd: i as i32 + 1,
                    type_oid: *t,
                    collation: 0,
                });
            }
            if has_result {
                sources.push(ElemSource {
                    schema: nsname,
                    name,
                    object_type: "ROUTINE",
                    dtd: 0,
                    type_oid: p.ret,
                    collation: 0,
                });
            }
        }

        let mut rows = Vec::new();
        for s in sources {
            let elem = match array_elem_of(cx.env, s.type_oid) {
                Some(e) => e,
                None => continue,
            };
            let einfo = registry::type_info(cx.env, elem)?;
            let data_type = if einfo.namespace == oids::PG_CATALOG_NAMESPACE {
                names::format_type(cx.env, elem, -1)?
            } else {
                "USER-DEFINED".to_string()
            };
            let mut b = RowBuilder::new(schema);
            b.set("object_catalog", d::text(&cx.db))
                .set("object_schema", d::text(s.schema))
                .set("object_name", d::text(&s.name))
                .set("object_type", d::text(s.object_type))
                .set("collection_type_identifier", d::text(&s.dtd.to_string()))
                .set("data_type", d::text(&data_type));
            put_collation(&mut b, &cx.db, cx.collation(s.collation)?);
            b.set("udt_catalog", d::text(&cx.db))
                .set("udt_schema", d::text(cx.ns(einfo.namespace)?))
                .set("udt_name", d::text(&einfo.name))
                .set("dtd_identifier", d::text(&format!("a{}", s.dtd)));
            rows.push(b.finish()?);
        }
        Ok(rows)
    }
}

/// Registry slice.
pub static RELATIONS: &[&dyn VirtualRelation] = &[
    &IsColumns,
    &IsElementTypes,
    &IsDomains,
    &IsDomainConstraints,
    &IsColumnDomainUsage,
    &IsColumnUdtUsage,
    &IsDomainUdtUsage,
    &IsColumnColumnUsage,
];

#[cfg(test)]
mod tests {
    use super::*;
    use crate::data::CatalogData;
    use crate::model::{
        ColumnDefault, Constraint, ConstraintKind, GeneratedKind, Identity, ObjectAddress, Persistence, Sequence,
    };
    use crate::session::NullSessionInfo;
    use crate::testing;
    use pgarbor_types::context::IoConfig;
    use pgarbor_types::datetime::interval_fields as iv;
    use pgarbor_types::TypeId;

    const T_OID: Oid = 16400;
    const SEQ_OID: Oid = 16401;
    const DOM_OID: Oid = 16410;

    fn base_env() -> CatalogEnv {
        testing::bootstrapped().expect("bootstrap").1
    }

    #[test]
    fn describe_integer_and_text() {
        let env = base_env();
        let t = describe_type_for_info_schema(&env, oids::INT4, -1).unwrap();
        assert_eq!(t.data_type, "integer");
        assert_eq!(t.numeric_precision, Some(32));
        assert_eq!(t.numeric_precision_radix, Some(2));
        assert_eq!(t.numeric_scale, Some(0));
        assert_eq!(t.udt_schema, "pg_catalog");
        assert_eq!(t.udt_name, "int4");
        assert!(t.character_maximum_length.is_none() && t.domain_name.is_none() && t.element.is_none());

        let t = describe_type_for_info_schema(&env, oids::TEXT, -1).unwrap();
        assert_eq!(t.data_type, "text");
        assert_eq!(t.character_octet_length, Some(1_073_741_824));
        assert_eq!(t.character_maximum_length, None);
    }

    #[test]
    fn describe_varchar_numeric_array_timestamp() {
        let env = base_env();
        let t = describe_type_for_info_schema(&env, oids::VARCHAR, 14).unwrap();
        assert_eq!(t.data_type, "character varying");
        assert_eq!(t.character_maximum_length, Some(10));
        assert_eq!(t.character_octet_length, Some(40));
        assert_eq!(t.udt_name, "varchar");

        let t = describe_type_for_info_schema(&env, oids::NUMERIC, pgtypmod::numeric_typmod(10, 2)).unwrap();
        assert_eq!(t.data_type, "numeric");
        assert_eq!((t.numeric_precision, t.numeric_precision_radix, t.numeric_scale), (Some(10), Some(10), Some(2)));

        let t = describe_type_for_info_schema(&env, oids::INT4_ARRAY, -1).unwrap();
        assert_eq!(t.data_type, "ARRAY");
        assert_eq!(t.udt_name, "_int4");
        assert_eq!(t.element.as_ref().map(|e| e.data_type.as_str()), Some("integer"));
        assert_eq!(t.element.as_ref().map(|e| e.udt_name.as_str()), Some("int4"));

        let t = describe_type_for_info_schema(&env, oids::TIMESTAMP, 3).unwrap();
        assert_eq!(t.data_type, "timestamp without time zone");
        assert_eq!(t.datetime_precision, Some(3));
        let t = describe_type_for_info_schema(&env, oids::TIMESTAMP, -1).unwrap();
        assert_eq!(t.datetime_precision, Some(6));
        let t = describe_type_for_info_schema(&env, oids::DATE, -1).unwrap();
        assert_eq!(t.datetime_precision, Some(0));
    }

    #[test]
    fn describe_interval_type() {
        let env = base_env();
        let tm = ((iv::DAY | iv::HOUR | iv::MINUTE | iv::SECOND) << 16) | 3;
        let t = describe_type_for_info_schema(&env, oids::INTERVAL, tm).unwrap();
        assert_eq!(t.interval_type.as_deref(), Some("DAY TO SECOND(3)"));
        assert_eq!(t.datetime_precision, Some(3));
        let t = describe_type_for_info_schema(&env, oids::INTERVAL, -1).unwrap();
        assert_eq!(t.interval_type, None);
        assert_eq!(t.datetime_precision, Some(6));
    }

    #[test]
    fn describe_builtin_domain_and_unknown_type() {
        let env = base_env();
        // information_schema.sql_identifier is a domain over name.
        let dom = catalog_data::TYPES
            .iter()
            .find(|t| t.name == "sql_identifier")
            .map(|t| t.oid)
            .expect("sql_identifier");
        let t = describe_type_for_info_schema(&env, dom, -1).unwrap();
        assert_eq!(t.data_type, "name");
        assert_eq!(t.domain_schema.as_deref(), Some("information_schema"));
        assert_eq!(t.domain_name.as_deref(), Some("sql_identifier"));
        assert_eq!(t.udt_name, "name");

        let err = describe_type_for_info_schema(&env, 4_000_000_000, -1).unwrap_err();
        assert!(err.to_string().contains("cache lookup failed for type 4000000000"));
    }

    #[test]
    fn name_concat_oid_clips_long_names() {
        assert_eq!(name_concat_oid("f", 16400), "f_16400");
        let long = "x".repeat(80);
        let n = name_concat_oid(&long, 16400);
        assert_eq!(n.len(), 63);
        assert!(n.ends_with("_16400"));
    }

    /// Env over the bootstrapped catalog plus table `public.t`, domain `public.pos`, a constraint and
    /// an identity sequence, all owned by postgres.
    fn populated_env() -> CatalogEnv {
        let env = base_env();
        let mut data: CatalogData = env.snapshot().data().clone();

        let mut id = Column::new(1, "id", TypeId::new(oids::INT4, -1), 0);
        id.not_null = true;
        id.default = Some(ColumnDefault {
            oid: 16402,
            expr: StoredExpr::new("nextval('t_id_seq'::regclass)"),
        });
        let mut a = Column::new(2, "a", TypeId::new(oids::VARCHAR, 14), 100);
        a.not_null = true;
        let mut g = Column::new(3, "g", TypeId::new(oids::TEXT, -1), 100);
        g.generated = Some(GeneratedKind::Stored);
        g.default = Some(ColumnDefault {
            oid: 16403,
            expr: StoredExpr::new("((a)::text || 'y'::text)")
                .with_deps(vec![ObjectAddress::column(T_OID, 2)]),
        });
        let mut dropped = Column::new(4, Column::dropped_name(4), TypeId::new(0, -1), 0);
        dropped.dropped = true;
        let n = Column::new(5, "n", TypeId::new(oids::NUMERIC, pgtypmod::numeric_typmod(10, 2)), 0);
        let arr = Column::new(6, "arr", TypeId::new(oids::INT4_ARRAY, -1), 0);
        let mut idn = Column::new(7, "ident", TypeId::new(oids::INT8, -1), 0);
        idn.identity = Some(Identity { kind: IdentityKind::Always, sequence: SEQ_OID });
        let dcol = Column::new(8, "p", TypeId::new(DOM_OID, -1), 0);

        let mut rel = builtin::builtin_relation(relid::PG_CLASS).expect("pg_class").clone();
        rel.oid = T_OID;
        rel.name = "t".to_string();
        rel.namespace = crate::builtin::PUBLIC_NAMESPACE;
        rel.kind = RelKind::Table;
        rel.owner = BOOTSTRAP_SUPERUSER;
        rel.persistence = Persistence::Permanent;
        rel.columns = vec![id, a, g, dropped, n, arr, idn, dcol];
        rel.rowtype = T_OID + 4;
        rel.array_type = T_OID + 5;
        rel.view = None;
        data.relations.insert(T_OID, rel);

        data.sequences.insert(
            SEQ_OID,
            Sequence {
                oid: SEQ_OID,
                name: "t_ident_seq".to_string(),
                namespace: crate::builtin::PUBLIC_NAMESPACE,
                owner: BOOTSTRAP_SUPERUSER,
                persistence: Persistence::Permanent,
                type_oid: oids::INT8,
                start: 1,
                increment: 1,
                min: 1,
                max: i64::MAX,
                cache: 1,
                cycle: false,
                owned_by: Some((T_OID, 7)),
                identity: true,
                acl: None,
            },
        );

        data.types.insert(
            DOM_OID,
            TypeObj {
                oid: DOM_OID,
                name: "pos".to_string(),
                namespace: crate::builtin::PUBLIC_NAMESPACE,
                owner: BOOTSTRAP_SUPERUSER,
                array_type: DOM_OID + 1,
                array_type_name: None,
                def: TypeDef::Domain {
                    base: TypeId::new(oids::INT4, -1),
                    not_null: true,
                    default: Some(StoredExpr::new("1")),
                    collation: 0,
                    ndims: 0,
                },
                acl: None,
            },
        );
        data.constraints.insert(
            DOM_OID + 5,
            Constraint {
                oid: DOM_OID + 5,
                name: "pos_check".to_string(),
                namespace: crate::builtin::PUBLIC_NAMESPACE,
                kind: ConstraintKind::Check,
                table: 0,
                domain: DOM_OID,
                columns: Vec::new(),
                index: 0,
                deferrable: false,
                initially_deferred: false,
                validated: true,
                enforced: true,
                is_local: true,
                inh_count: 0,
                no_inherit: false,
                period: false,
                fk: None,
                check: Some(StoredExpr::new("(VALUE > 0)")),
                parent: 0,
            },
        );

        let snap = CatalogSnapshot::build(env.snapshot().version(), data).expect("build");
        env.with_snapshot(snap)
    }

    fn scan_rel(env: &CatalogEnv, rel: &dyn VirtualRelation) -> Vec<Row> {
        let cfg = IoConfig::default();
        let session = NullSessionInfo;
        let ctx = ScanContext { env, session: &session, io_config: &cfg };
        rel.scan(&ctx, &ScanRequest::all()).expect("scan")
    }

    fn col_index(rel: &dyn VirtualRelation, name: &str) -> usize {
        rel.schema().unwrap().columns.iter().position(|c| c.name == name).expect("column")
    }

    fn txt<'r>(row: &'r Row, i: usize) -> Option<&'r str> {
        match &row[i] {
            Datum::Text(s) => Some(s.as_str()),
            _ => None,
        }
    }

    fn int(row: &Row, i: usize) -> Option<i32> {
        match &row[i] {
            Datum::Int4(v) => Some(*v),
            _ => None,
        }
    }

    fn find_row<'r>(rows: &'r [Row], rel: &dyn VirtualRelation, table: &str, column: &str) -> &'r Row {
        let ti = col_index(rel, "table_name");
        let ci = col_index(rel, "column_name");
        rows.iter()
            .find(|r| txt(r, ti) == Some(table) && txt(r, ci) == Some(column))
            .expect("row present")
    }

    #[test]
    fn columns_rows_for_user_table() {
        let env = populated_env();
        let rel = IsColumns;
        let rows = scan_rel(&env, &rel);
        let c = |name: &str| col_index(&rel, name);

        let id = find_row(&rows, &rel, "t", "id");
        assert_eq!(txt(id, c("table_catalog")), Some("postgres"));
        assert_eq!(txt(id, c("table_schema")), Some("public"));
        assert_eq!(int(id, c("ordinal_position")), Some(1));
        assert_eq!(txt(id, c("column_default")), Some("nextval('t_id_seq'::regclass)"));
        assert_eq!(txt(id, c("is_nullable")), Some("NO"));
        assert_eq!(txt(id, c("data_type")), Some("integer"));
        assert_eq!(int(id, c("numeric_precision")), Some(32));
        assert_eq!(int(id, c("numeric_precision_radix")), Some(2));
        assert_eq!(int(id, c("numeric_scale")), Some(0));
        assert_eq!(txt(id, c("udt_name")), Some("int4"));
        assert_eq!(txt(id, c("dtd_identifier")), Some("1"));
        assert_eq!(txt(id, c("is_identity")), Some("NO"));
        assert_eq!(txt(id, c("is_generated")), Some("NEVER"));
        assert_eq!(txt(id, c("is_updatable")), Some("YES"));

        let a = find_row(&rows, &rel, "t", "a");
        assert_eq!(txt(a, c("data_type")), Some("character varying"));
        assert_eq!(int(a, c("character_maximum_length")), Some(10));
        assert_eq!(int(a, c("character_octet_length")), Some(40));
        assert_eq!(txt(a, c("collation_name")), None);

        let g = find_row(&rows, &rel, "t", "g");
        assert_eq!(txt(g, c("is_generated")), Some("ALWAYS"));
        assert_eq!(txt(g, c("generation_expression")), Some("((a)::text || 'y'::text)"));
        assert_eq!(txt(g, c("column_default")), None);

        // dropped column leaves a gap in ordinal_position
        let n = find_row(&rows, &rel, "t", "n");
        assert_eq!(int(n, c("ordinal_position")), Some(5));
        assert_eq!(txt(n, c("dtd_identifier")), Some("5"));
        assert_eq!(
            (int(n, c("numeric_precision")), int(n, c("numeric_precision_radix")), int(n, c("numeric_scale"))),
            (Some(10), Some(10), Some(2))
        );
        let ti = c("table_name");
        assert_eq!(rows.iter().filter(|r| txt(r, ti) == Some("t")).count(), 7);

        let arr = find_row(&rows, &rel, "t", "arr");
        assert_eq!(txt(arr, c("data_type")), Some("ARRAY"));
        assert_eq!(txt(arr, c("udt_name")), Some("_int4"));

        let idn = find_row(&rows, &rel, "t", "ident");
        assert_eq!(txt(idn, c("is_identity")), Some("YES"));
        assert_eq!(txt(idn, c("identity_generation")), Some("ALWAYS"));
        assert_eq!(txt(idn, c("identity_start")), Some("1"));
        assert_eq!(txt(idn, c("identity_increment")), Some("1"));
        assert_eq!(txt(idn, c("identity_maximum")), Some("9223372036854775807"));
        assert_eq!(txt(idn, c("identity_cycle")), Some("NO"));

        let p = find_row(&rows, &rel, "t", "p");
        assert_eq!(txt(p, c("data_type")), Some("integer"));
        assert_eq!(txt(p, c("domain_catalog")), Some("postgres"));
        assert_eq!(txt(p, c("domain_schema")), Some("public"));
        assert_eq!(txt(p, c("domain_name")), Some("pos"));
        assert_eq!(txt(p, c("udt_name")), Some("int4"));
        assert_eq!(txt(p, c("is_nullable")), Some("NO"));
    }

    #[test]
    fn columns_include_system_views() {
        let env = base_env();
        let rel = IsColumns;
        let rows = scan_rel(&env, &rel);
        let r = find_row(&rows, &rel, "columns", "table_name");
        assert_eq!(txt(r, col_index(&rel, "table_schema")), Some("information_schema"));
        assert_eq!(txt(r, col_index(&rel, "data_type")), Some("name"));
        assert_eq!(txt(r, col_index(&rel, "domain_name")), Some("sql_identifier"));
        assert_eq!(txt(r, col_index(&rel, "is_updatable")), Some("NO"));
    }

    #[test]
    fn domain_views() {
        let env = populated_env();

        let rel = IsDomains;
        let rows = scan_rel(&env, &rel);
        let dn = col_index(&rel, "domain_name");
        let pos = rows.iter().find(|r| txt(r, dn) == Some("pos")).expect("pos");
        assert_eq!(txt(pos, col_index(&rel, "data_type")), Some("integer"));
        assert_eq!(txt(pos, col_index(&rel, "domain_default")), Some("1"));
        assert_eq!(int(pos, col_index(&rel, "numeric_precision")), Some(32));
        assert_eq!(txt(pos, col_index(&rel, "dtd_identifier")), Some("1"));
        let ts = rows.iter().find(|r| txt(r, dn) == Some("time_stamp")).expect("time_stamp");
        assert_eq!(int(ts, col_index(&rel, "datetime_precision")), Some(2));

        let rel = IsDomainConstraints;
        let rows = scan_rel(&env, &rel);
        let cn = col_index(&rel, "constraint_name");
        assert!(rows.iter().any(|r| txt(r, cn) == Some("pos_check")));
        assert!(rows.iter().any(|r| txt(r, cn) == Some("yes_or_no_check")));
        let r = rows.iter().find(|r| txt(r, cn) == Some("pos_check")).expect("pos_check");
        assert_eq!(txt(r, col_index(&rel, "is_deferrable")), Some("NO"));
        assert_eq!(txt(r, col_index(&rel, "domain_name")), Some("pos"));

        let rel = IsColumnDomainUsage;
        let rows = scan_rel(&env, &rel);
        let tn = col_index(&rel, "table_name");
        let usage = rows.iter().find(|r| txt(r, tn) == Some("t")).expect("usage");
        assert_eq!(txt(usage, col_index(&rel, "domain_name")), Some("pos"));
        assert_eq!(txt(usage, col_index(&rel, "column_name")), Some("p"));

        let rel = IsDomainUdtUsage;
        let rows = scan_rel(&env, &rel);
        let dn = col_index(&rel, "domain_name");
        let r = rows.iter().find(|r| txt(r, dn) == Some("pos")).expect("pos");
        assert_eq!(txt(r, col_index(&rel, "udt_name")), Some("int4"));
        assert_eq!(txt(r, col_index(&rel, "udt_schema")), Some("pg_catalog"));
    }

    #[test]
    fn column_udt_and_column_column_usage() {
        let env = populated_env();

        let rel = IsColumnUdtUsage;
        let rows = scan_rel(&env, &rel);
        let tn = col_index(&rel, "table_name");
        let cn = col_index(&rel, "column_name");
        let id = rows.iter().find(|r| txt(r, tn) == Some("t") && txt(r, cn) == Some("id")).expect("id");
        assert_eq!(txt(id, col_index(&rel, "udt_name")), Some("int4"));
        let p = rows.iter().find(|r| txt(r, tn) == Some("t") && txt(r, cn) == Some("p")).expect("p");
        assert_eq!(txt(p, col_index(&rel, "udt_name")), Some("int4"));
        assert!(rows.iter().all(|r| txt(r, cn) != Some("........pg.dropped.4........")));

        let rel = IsColumnColumnUsage;
        let rows = scan_rel(&env, &rel);
        assert_eq!(rows.len(), 1);
        assert_eq!(txt(&rows[0], col_index(&rel, "table_name")), Some("t"));
        assert_eq!(txt(&rows[0], col_index(&rel, "column_name")), Some("a"));
        assert_eq!(txt(&rows[0], col_index(&rel, "dependent_column")), Some("g"));
    }

    #[test]
    fn element_types_rows() {
        let env = populated_env();
        let rel = IsElementTypes;
        let rows = scan_rel(&env, &rel);
        let on = col_index(&rel, "object_name");
        let r = rows.iter().find(|r| txt(r, on) == Some("t")).expect("t.arr");
        assert_eq!(txt(r, col_index(&rel, "object_type")), Some("TABLE"));
        assert_eq!(txt(r, col_index(&rel, "collection_type_identifier")), Some("6"));
        assert_eq!(txt(r, col_index(&rel, "data_type")), Some("integer"));
        assert_eq!(txt(r, col_index(&rel, "udt_name")), Some("int4"));
        assert_eq!(txt(r, col_index(&rel, "dtd_identifier")), Some("a6"));
        // builtin routines with array parameters appear too
        assert!(rows.iter().any(|r| txt(r, col_index(&rel, "object_type")) == Some("ROUTINE")));
    }
}
