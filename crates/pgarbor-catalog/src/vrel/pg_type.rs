//! `pg_type`, `pg_enum`, `pg_range`. Lease CAT-30. PG sources: `catalog/pg_type.h`, `pg_enum.h`,
//! `pg_range.h`. Oracle-checked.
//!
//! ## pg_type
//! Rows ordered by `oid`: (1) every builtin type of `pgarbor_types::catalog_data::TYPES` (622 rows:
//! base, array, pseudo, composite catalog rowtypes, information_schema domains), (2) user types.
//! Fast path: eq on `oid` (col 0) and `typname` (col 1, + `typnamespace` col 2).
//! Builtin row from `BuiltinType`: `oid`, `typname`=name, `typnamespace`, `typowner` 10 (bootstrap
//! superuser), `typlen`, `typbyval`, `typtype`, `typcategory`, `typispreferred`, `typisdefined` true,
//! `typdelim`, `typrelid`, `typelem`, `typarray`, `typalign`, `typstorage`, `typnotnull` false,
//! `typbasetype`, `typtypmod`, `typndims`, `typcollation`, `typdefaultbin` NULL, `typdefault` NULL,
//! `typacl` NULL. The regproc columns hold the proc OID (`Oid`) of the named function: `typinput`,
//! `typoutput`, `typreceive`, `typsend` via `regproc_oid(name)` = the builtin proc with that
//! `proname` in pg_catalog (unique for I/O functions; 0 when the name is "-" or unknown);
//! `typsubscript` = `regproc_oid("array_subscript_handler")` for every type of
//! `typcategory` 'A' (true arrays AND `int2vector`/`oidvector`), `regproc_oid("raw_array_subscript_handler")`
//! for `name`, `point`, `box`, `line`, `lseg`, `regproc_oid("jsonb_subscript_handler")` for `jsonb`, else 0
//! (oracle `select typname, typsubscript from pg_type`); `typmodin`/`typmodout` 0 (not recorded in
//! `BuiltinType`, amendment 03), `typanalyze` = `regproc_oid("array_typanalyze")` for true arrays (category
//! 'A' types that are some type's `typarray`), `range_typanalyze` for range types, `multirange_typanalyze`
//! for multirange types, `ts_typanalyze` for `tsvector`, else 0. (Never hard-code these OIDs: look the
//! names up with `regproc_oid`.)
//! User rows come from `CatalogEnv::user_type_info` (synthesized shapes documented in `typeenv.rs`) for
//! every `UserTypeRef` of `snapshot.indexes().user_types` sorted by OID, adding: `typowner` (TypeObj
//! owner / relation owner), `typacl`, domains: `typnotnull`, `typdefault` = default expr text and
//! `typdefaultbin` = same text, `typcollation`. Oracle facts: a table's rowtype has `typtype` 'c',
//! `typcategory` 'C', `typrelid` = the table, `typarray` = its array; an array type has `typtype` 'b',
//! category 'A', `typelem` = element, `typinput` `array_in`, `typalign` 'd' when the element's align
//! is 'd' (else 'i'), `typstorage` 'x', `typcollation` = the element's.
//!
//! ## pg_enum
//! One row per `EnumLabelDef`: `oid`, `enumtypid`, `enumsortorder` (`Float4`), `enumlabel`; ordered by
//! (`enumtypid`, `enumsortorder`). Fast path: eq on `enumtypid` (col 1). No builtin enums.
//!
//! ## pg_range
//! The builtin range types from `builtin::RANGES` (`TextRows`, PG text-output form): convert each
//! cell with `io::input(ctx.io_ctx(), TypeId::new(column type, typmod), text)`; NULL -> `Datum::Null`.
//! Ordered by `rngtypid`. No user ranges (CREATE TYPE ... AS RANGE is unsupported).
use super::rowbuild::{d, RowBuilder};
use super::{Row, ScanContext, ScanRequest, VirtualRelation};
use crate::builtin::{self, relid, SystemRel};
use crate::model::{EnumLabelDef, TypeDef, TypeObj};
use crate::snapshot::{CatalogSnapshot, UserTypeRef};
use pgarbor_common::{Oid, PgError, Result};
use pgarbor_types::catalog_data::{BuiltinType, TYPES};
use pgarbor_types::registry::TypeInfo;
use pgarbor_types::{io, Datum, TypeId};

/// Depth limit when following domain base chains (corrupt data must not recurse forever).
const MAX_DOMAIN_DEPTH: u32 = 64;

/// Producer for `pg_type`.
#[derive(Clone, Copy, Debug)]
pub struct PgType;

impl VirtualRelation for PgType {
    fn oid(&self) -> Oid {
        relid::PG_TYPE
    }

    fn scan(&self, ctx: &ScanContext, req: &ScanRequest) -> Result<Vec<Row>> {
        let schema = self.schema()?;
        let snapshot = ctx.env.snapshot();
        let want_oid = req.oid_eq_on(0);
        let mut rows = Vec::new();
        for t in TYPES.iter() {
            if !keep_oid(want_oid, t.oid) || !keep_name(req, 1, t.name) || !keep_oid_col(req, 2, t.namespace) {
                continue;
            }
            rows.push(builtin_row(schema, t)?);
        }
        let mut users: Vec<(Oid, UserTypeRef)> =
            snapshot.indexes().user_types.iter().map(|(oid, uref)| (*oid, *uref)).collect();
        users.sort_by_key(|(oid, _)| *oid);
        for (oid, uref) in users {
            if !keep_oid(want_oid, oid) {
                continue;
            }
            let info = ctx
                .env
                .user_type_info(oid)?
                .ok_or_else(|| PgError::internal(format!("user type {oid} has no pg_type shape")))?;
            let name: &str = &info.name;
            if !keep_name(req, 1, name) || !keep_oid_col(req, 2, info.namespace) {
                continue;
            }
            rows.push(user_row(schema, snapshot, oid, uref, &info)?);
        }
        Ok(rows)
    }
}

/// Producer for `pg_enum`.
#[derive(Clone, Copy, Debug)]
pub struct PgEnum;

impl VirtualRelation for PgEnum {
    fn oid(&self) -> Oid {
        relid::PG_ENUM
    }

    fn scan(&self, ctx: &ScanContext, req: &ScanRequest) -> Result<Vec<Row>> {
        let schema = self.schema()?;
        let want_type = req.oid_eq_on(1);
        let mut rows = Vec::new();
        // BTreeMap iteration: ascending enum type OID; labels sorted by enumsortorder within a type.
        for (type_oid, obj) in ctx.env.snapshot().data().types.iter() {
            if !keep_oid(want_type, *type_oid) {
                continue;
            }
            let labels = match &obj.def {
                TypeDef::Enum { labels } => labels,
                TypeDef::Domain { .. } => continue,
            };
            let mut sorted: Vec<&EnumLabelDef> = labels.iter().collect();
            sorted.sort_by(|a, b| a.sort_order.total_cmp(&b.sort_order));
            for l in sorted {
                let mut b = RowBuilder::new(schema);
                b.set("oid", d::oid(l.oid))
                    .set("enumtypid", d::oid(*type_oid))
                    .set("enumsortorder", d::float4(l.sort_order))
                    .set("enumlabel", d::name(&l.label));
                rows.push(b.finish()?);
            }
        }
        Ok(rows)
    }
}

/// Producer for `pg_range`.
#[derive(Clone, Copy, Debug)]
pub struct PgRange;

impl VirtualRelation for PgRange {
    fn oid(&self) -> Oid {
        relid::PG_RANGE
    }

    fn scan(&self, ctx: &ScanContext, req: &ScanRequest) -> Result<Vec<Row>> {
        let schema = self.schema()?;
        let io_ctx = ctx.io_ctx();
        let want_oid = req.oid_eq_on(0);
        let mut rows: Vec<Row> = Vec::new();
        for cells in builtin::RANGES.rows {
            if cells.len() != schema.columns.len() {
                return Err(PgError::internal(format!(
                    "pg_range builtin row has {} cells, expected {}",
                    cells.len(),
                    schema.columns.len()
                )));
            }
            let mut row: Row = Vec::with_capacity(cells.len());
            for (cell, col) in cells.iter().zip(schema.columns.iter()) {
                let value = match cell {
                    None => Datum::Null,
                    Some(text) => io::input(&io_ctx, TypeId::new(col.type_oid, col.typmod), text)?,
                };
                row.push(value);
            }
            let rngtypid = match row.first() {
                Some(Datum::Oid(o)) => *o,
                _ => 0,
            };
            if keep_oid(want_oid, rngtypid) {
                rows.push(row);
            }
        }
        rows.sort_by_key(|r| match r.first() {
            Some(Datum::Oid(o)) => *o,
            _ => 0,
        });
        Ok(rows)
    }
}

/// Every `pg_type` column, by name, for one row (values already shaped).
struct TypeRow<'a> {
    oid: Oid,
    name: &'a str,
    namespace: Oid,
    owner: Oid,
    len: i16,
    byval: bool,
    typtype: u8,
    category: u8,
    preferred: bool,
    delim: u8,
    relid: Oid,
    subscript: Oid,
    elem: Oid,
    array: Oid,
    input: Oid,
    output: Oid,
    receive: Oid,
    send: Oid,
    analyze: Oid,
    align: u8,
    storage: u8,
    notnull: bool,
    basetype: Oid,
    typmod: i32,
    ndims: i32,
    collation: Oid,
    default: Option<&'a str>,
    acl: Datum,
}

impl TypeRow<'_> {
    fn into_row(self, schema: &'static SystemRel) -> Result<Row> {
        let mut b = RowBuilder::new(schema);
        b.set("oid", d::oid(self.oid))
            .set("typname", d::name(self.name))
            .set("typnamespace", d::oid(self.namespace))
            .set("typowner", d::oid(self.owner))
            .set("typlen", d::int2(self.len))
            .set("typbyval", d::bool_(self.byval))
            .set("typtype", d::char_(self.typtype))
            .set("typcategory", d::char_(self.category))
            .set("typispreferred", d::bool_(self.preferred))
            .set("typisdefined", d::bool_(true))
            .set("typdelim", d::char_(self.delim))
            .set("typrelid", d::oid(self.relid))
            .set("typsubscript", d::oid(self.subscript))
            .set("typelem", d::oid(self.elem))
            .set("typarray", d::oid(self.array))
            .set("typinput", d::oid(self.input))
            .set("typoutput", d::oid(self.output))
            .set("typreceive", d::oid(self.receive))
            .set("typsend", d::oid(self.send))
            .set("typmodin", d::oid(0))
            .set("typmodout", d::oid(0))
            .set("typanalyze", d::oid(self.analyze))
            .set("typalign", d::char_(self.align))
            .set("typstorage", d::char_(self.storage))
            .set("typnotnull", d::bool_(self.notnull))
            .set("typbasetype", d::oid(self.basetype))
            .set("typtypmod", d::int4(self.typmod))
            .set("typndims", d::int4(self.ndims))
            .set("typcollation", d::oid(self.collation))
            .set("typdefaultbin", d::opt_text(self.default))
            .set("typdefault", d::opt_text(self.default))
            .set("typacl", self.acl);
        b.finish()
    }
}

/// One builtin `pg_type` row (module docs: builtin row rules).
fn builtin_row(schema: &'static SystemRel, t: &BuiltinType) -> Result<Row> {
    TypeRow {
        oid: t.oid,
        name: t.name,
        namespace: t.namespace,
        owner: builtin::BOOTSTRAP_SUPERUSER,
        len: t.len,
        byval: t.byval,
        typtype: t.typtype,
        category: t.category,
        preferred: t.preferred,
        delim: t.delim,
        relid: t.relid,
        subscript: builtin_subscript(t),
        elem: t.elem,
        array: t.array,
        input: regproc_oid(t.input),
        output: regproc_oid(t.output),
        receive: regproc_oid(t.receive),
        send: regproc_oid(t.send),
        analyze: builtin_typanalyze(t),
        align: t.align,
        storage: t.storage,
        notnull: false,
        basetype: t.basetype,
        typmod: t.typmod,
        ndims: t.ndims,
        collation: t.collation,
        default: None,
        acl: Datum::Null,
    }
    .into_row(schema)
}

/// One user `pg_type` row: shape from `CatalogEnv::user_type_info`, owner/acl/domain fields from the
/// stored object.
fn user_row(
    schema: &'static SystemRel,
    snapshot: &CatalogSnapshot,
    oid: Oid,
    uref: UserTypeRef,
    info: &TypeInfo,
) -> Result<Row> {
    let missing = |dep: Oid| PgError::corruption(format!("user type {oid} refers to missing object {dep}"));
    let is_array = matches!(uref, UserTypeRef::TypeArray(_) | UserTypeRef::RowTypeArray(_));
    let (owner, own) = match uref {
        UserTypeRef::Type(t) => {
            let obj = snapshot.type_obj(t).ok_or_else(|| missing(t))?;
            (obj.owner, Some(obj))
        }
        UserTypeRef::TypeArray(elem) => {
            let obj = snapshot.type_obj(elem).ok_or_else(|| missing(elem))?;
            (obj.owner, None)
        }
        UserTypeRef::RowType(rel) | UserTypeRef::RowTypeArray(rel) => {
            let r = snapshot.relation(rel).ok_or_else(|| missing(rel))?;
            (r.owner, None)
        }
    };
    let mut acl = Datum::Null;
    let mut notnull = false;
    let mut default: Option<&str> = None;
    let mut subscript = 0;
    let mut analyze = 0;
    if is_array {
        subscript = regproc_oid("array_subscript_handler");
        analyze = regproc_oid("array_typanalyze");
    }
    if let Some(obj) = own {
        acl = d::acl_array(obj.acl.as_ref(), snapshot);
        if let TypeDef::Domain { not_null, default: def, .. } = &obj.def {
            notnull = *not_null;
            default = def.as_ref().map(|e| e.sql.as_str());
            // Domains never get typsubscript; typanalyze is inherited from the base type.
            analyze = analyze_of(snapshot, info.basetype, 0);
        }
    }
    let name: &str = &info.name;
    TypeRow {
        oid,
        name,
        namespace: info.namespace,
        owner,
        len: info.len,
        byval: info.byval,
        typtype: info.typtype,
        category: info.category,
        preferred: info.preferred,
        delim: info.delim,
        relid: info.relid,
        subscript,
        elem: info.elem,
        array: info.array,
        input: regproc_oid(info.input),
        output: regproc_oid(info.output),
        receive: regproc_oid(info.receive),
        send: regproc_oid(info.send),
        analyze,
        align: info.align,
        storage: info.storage,
        notnull,
        basetype: info.basetype,
        typmod: info.typmod,
        ndims: info.ndims,
        collation: info.collation,
        default,
        acl,
    }
    .into_row(schema)
}

fn keep_oid(want: Option<Oid>, oid: Oid) -> bool {
    want.map_or(true, |w| w == oid)
}

fn keep_oid_col(req: &ScanRequest, col: usize, oid: Oid) -> bool {
    keep_oid(req.oid_eq_on(col), oid)
}

fn keep_name(req: &ScanRequest, col: usize, name: &str) -> bool {
    match req.eq_on(col) {
        Some(Datum::Text(s)) => s == name,
        _ => true,
    }
}

fn builtin_type(oid: Oid) -> Option<&'static BuiltinType> {
    TYPES.binary_search_by_key(&oid, |t| t.oid).ok().map(|i| &TYPES[i])
}

/// Category 'A' types, plus pseudo-array types with an element (`_record`), which PG also treats as
/// arrays for subscripting and analysis.
fn is_array_like(t: &BuiltinType) -> bool {
    t.category == b'A' || (t.typtype == b'p' && t.elem != 0)
}

/// True when some builtin type names `oid` as its `typarray` (a "true" array type).
fn is_typarray_target(oid: Oid) -> bool {
    TYPES.iter().any(|t| t.array == oid)
}

/// `typsubscript` of a builtin type (module docs).
fn builtin_subscript(t: &BuiltinType) -> Oid {
    if is_array_like(t) {
        return regproc_oid("array_subscript_handler");
    }
    match t.name {
        "name" | "point" | "box" | "line" | "lseg" => regproc_oid("raw_array_subscript_handler"),
        "jsonb" => regproc_oid("jsonb_subscript_handler"),
        _ => 0,
    }
}

/// `typanalyze` of a builtin type (module docs).
fn builtin_typanalyze(t: &BuiltinType) -> Oid {
    if is_array_like(t) && is_typarray_target(t.oid) {
        return regproc_oid("array_typanalyze");
    }
    match t.typtype {
        b'r' => regproc_oid("range_typanalyze"),
        b'm' => regproc_oid("multirange_typanalyze"),
        _ if t.name == "tsvector" => regproc_oid("ts_typanalyze"),
        _ => 0,
    }
}

/// `typanalyze` a domain inherits from `oid` (the base type), following user domains.
fn analyze_of(snapshot: &CatalogSnapshot, oid: Oid, depth: u32) -> Oid {
    if let Some(t) = builtin_type(oid) {
        return builtin_typanalyze(t);
    }
    if depth > MAX_DOMAIN_DEPTH {
        return 0;
    }
    match snapshot.user_type(oid) {
        Some(UserTypeRef::TypeArray(_)) | Some(UserTypeRef::RowTypeArray(_)) => regproc_oid("array_typanalyze"),
        Some(UserTypeRef::Type(_)) => match snapshot.type_obj(oid) {
            Some(TypeObj { def: TypeDef::Domain { base, .. }, .. }) => analyze_of(snapshot, base.oid, depth + 1),
            _ => 0,
        },
        _ => 0,
    }
}

/// Registry slice.
pub static RELATIONS: &[&dyn VirtualRelation] = &[&PgType, &PgEnum, &PgRange];

/// OID of the builtin pg_catalog function named `name` (first match), 0 for `"-"`/unknown.
/// Shared by `pg_type`, `pg_operator`, `pg_aggregate` and `pg_am` producers.
// CONTRACT
pub fn regproc_oid(name: &str) -> Oid {
    static MAP: std::sync::OnceLock<std::collections::HashMap<&'static str, Oid>> = std::sync::OnceLock::new();
    if name.is_empty() || name == "-" {
        return 0;
    }
    let map = MAP.get_or_init(|| {
        let mut m = std::collections::HashMap::new();
        for p in pgarbor_types::catalog_data::PROCS.iter() {
            if p.namespace == crate::builtin::PG_CATALOG_NAMESPACE {
                m.entry(p.name).or_insert(p.oid);
            }
        }
        m
    });
    map.get(name).copied().unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ddl::spec::CreateEnumSpec;
    use crate::session::NullSessionInfo;
    use crate::testing;
    use pgarbor_types::IoConfig;

    /// The named cell of a row of system relation `rel`.
    fn cell<'a>(rel: Oid, row: &'a Row, name: &str) -> &'a Datum {
        let schema = builtin::system_rel(rel).expect("system relation");
        let idx = schema.columns.iter().position(|c| c.name == name).expect("column");
        &row[idx]
    }

    #[test]
    fn builtin_int4_row() -> Result<()> {
        let (_kv, env) = testing::bootstrapped()?;
        let session = NullSessionInfo;
        let cfg = IoConfig::default();
        let ctx = ScanContext { env: &env, session: &session, io_config: &cfg };
        let rows = PgType.scan(&ctx, &ScanRequest { eq: vec![(0, Datum::Oid(23))] })?;
        assert_eq!(rows.len(), 1);
        let r = &rows[0];
        assert_eq!(cell(relid::PG_TYPE, r, "typname"), &Datum::Text("int4".into()));
        assert_eq!(cell(relid::PG_TYPE, r, "typlen"), &Datum::Int2(4));
        assert_eq!(cell(relid::PG_TYPE, r, "typbyval"), &Datum::Bool(true));
        assert_eq!(cell(relid::PG_TYPE, r, "typinput"), &Datum::Oid(regproc_oid("int4in")));
        assert_eq!(cell(relid::PG_TYPE, r, "typsubscript"), &Datum::Oid(0));
        assert_eq!(cell(relid::PG_TYPE, r, "typisdefined"), &Datum::Bool(true));
        assert_eq!(cell(relid::PG_TYPE, r, "typowner"), &Datum::Oid(builtin::BOOTSTRAP_SUPERUSER));
        Ok(())
    }

    #[test]
    fn builtin_array_row_subscript_and_analyze() -> Result<()> {
        let (_kv, env) = testing::bootstrapped()?;
        let session = NullSessionInfo;
        let cfg = IoConfig::default();
        let ctx = ScanContext { env: &env, session: &session, io_config: &cfg };
        // _int4
        let rows = PgType.scan(&ctx, &ScanRequest { eq: vec![(0, Datum::Oid(1007))] })?;
        assert_eq!(rows.len(), 1);
        let r = &rows[0];
        assert_eq!(cell(relid::PG_TYPE, r, "typsubscript"), &Datum::Oid(6179));
        assert_eq!(cell(relid::PG_TYPE, r, "typanalyze"), &Datum::Oid(regproc_oid("array_typanalyze")));
        assert_eq!(cell(relid::PG_TYPE, r, "typelem"), &Datum::Oid(23));
        assert_eq!(cell(relid::PG_TYPE, r, "typcategory"), &Datum::Char(b'A'));
        assert_eq!(cell(relid::PG_TYPE, r, "typtype"), &Datum::Char(b'b'));
        // _record (pseudo array, category P) is still an array for subscripting
        let rec = PgType.scan(&ctx, &ScanRequest { eq: vec![(0, Datum::Oid(2287))] })?;
        assert_eq!(cell(relid::PG_TYPE, &rec[0], "typsubscript"), &Datum::Oid(6179));
        Ok(())
    }

    #[test]
    fn unknown_type_oid_yields_no_rows() -> Result<()> {
        let (_kv, env) = testing::bootstrapped()?;
        let session = NullSessionInfo;
        let cfg = IoConfig::default();
        let ctx = ScanContext { env: &env, session: &session, io_config: &cfg };
        let rows = PgType.scan(&ctx, &ScanRequest { eq: vec![(0, Datum::Oid(999_999))] })?;
        assert!(rows.is_empty());
        Ok(())
    }

    #[test]
    fn builtin_pg_type_row_count() -> Result<()> {
        let (_kv, env) = testing::bootstrapped()?;
        let session = NullSessionInfo;
        let cfg = IoConfig::default();
        let ctx = ScanContext { env: &env, session: &session, io_config: &cfg };
        let rows = PgType.scan(&ctx, &ScanRequest::all())?;
        // every builtin type plus the user types of a fresh bootstrap (none)
        assert_eq!(rows.len(), TYPES.len());
        assert!(rows.iter().all(|r| r.len() == 32));
        Ok(())
    }

    #[test]
    fn user_enum_and_array_rows_and_pg_enum() -> Result<()> {
        let (mut kv, env) = testing::bootstrapped()?;
        let mut mood = 0;
        let spec = CreateEnumSpec {
            namespace: builtin::PUBLIC_NAMESPACE,
            name: "mood".into(),
            owner: builtin::BOOTSTRAP_SUPERUSER,
            labels: vec!["sad".into(), "ok".into(), "happy".into()],
        };
        let (_res, env2) = testing::run(&mut kv, &env, |t| {
            mood = t.create_enum(&spec)?;
            Ok(())
        })?;
        let arr = env2.snapshot().type_obj(mood).expect("enum type").array_type;
        let session = NullSessionInfo;
        let cfg = IoConfig::default();
        let ctx = ScanContext { env: &env2, session: &session, io_config: &cfg };

        let rows = PgType.scan(&ctx, &ScanRequest { eq: vec![(0, Datum::Oid(mood))] })?;
        assert_eq!(rows.len(), 1);
        assert_eq!(cell(relid::PG_TYPE, &rows[0], "typtype"), &Datum::Char(b'e'));
        assert_eq!(cell(relid::PG_TYPE, &rows[0], "typcategory"), &Datum::Char(b'E'));

        let arows = PgType.scan(&ctx, &ScanRequest { eq: vec![(0, Datum::Oid(arr))] })?;
        assert_eq!(arows.len(), 1);
        assert_eq!(cell(relid::PG_TYPE, &arows[0], "typname"), &Datum::Text("_mood".into()));
        assert_eq!(cell(relid::PG_TYPE, &arows[0], "typelem"), &Datum::Oid(mood));
        assert_eq!(cell(relid::PG_TYPE, &arows[0], "typtype"), &Datum::Char(b'b'));
        assert_eq!(cell(relid::PG_TYPE, &arows[0], "typcategory"), &Datum::Char(b'A'));
        assert_eq!(cell(relid::PG_TYPE, &arows[0], "typsubscript"), &Datum::Oid(6179));

        let erows = PgEnum.scan(&ctx, &ScanRequest { eq: vec![(1, Datum::Oid(mood))] })?;
        assert_eq!(erows.len(), 3);
        let labels: Vec<&Datum> = erows.iter().map(|r| cell(relid::PG_ENUM, r, "enumlabel")).collect();
        assert_eq!(
            labels,
            vec![
                &Datum::Text("sad".into()),
                &Datum::Text("ok".into()),
                &Datum::Text("happy".into())
            ]
        );
        assert_eq!(cell(relid::PG_ENUM, &erows[0], "enumsortorder"), &Datum::Float4(1.0));
        assert_eq!(cell(relid::PG_ENUM, &erows[2], "enumsortorder"), &Datum::Float4(3.0));
        assert_eq!(cell(relid::PG_ENUM, &erows[0], "enumtypid"), &Datum::Oid(mood));
        Ok(())
    }

    #[test]
    fn pg_range_rows_from_builtin_text() -> Result<()> {
        let (_kv, env) = testing::bootstrapped()?;
        let session = NullSessionInfo;
        let cfg = IoConfig::default();
        let ctx = ScanContext { env: &env, session: &session, io_config: &cfg };
        let rows = PgRange.scan(&ctx, &ScanRequest::all())?;
        assert_eq!(rows.len(), builtin::RANGES.rows.len());
        let r = &rows[0];
        assert_eq!(cell(relid::PG_RANGE, r, "rngtypid"), &Datum::Oid(3904));
        assert_eq!(cell(relid::PG_RANGE, r, "rngsubtype"), &Datum::Oid(23));
        assert_eq!(cell(relid::PG_RANGE, r, "rngmultitypid"), &Datum::Oid(4451));
        assert_eq!(
            cell(relid::PG_RANGE, r, "rngcanonical"),
            &Datum::Oid(regproc_oid("int4range_canonical"))
        );
        // "-" in a regproc column is InvalidOid
        let one = PgRange.scan(&ctx, &ScanRequest { eq: vec![(0, Datum::Oid(3906))] })?;
        assert_eq!(one.len(), 1);
        assert_eq!(cell(relid::PG_RANGE, &one[0], "rngcanonical"), &Datum::Oid(0));
        Ok(())
    }

    #[test]
    fn regproc_oid_lookup() {
        assert_eq!(regproc_oid(""), 0);
        assert_eq!(regproc_oid("-"), 0);
        assert_eq!(regproc_oid("no_such_function_xyz"), 0);
        assert_eq!(regproc_oid("array_subscript_handler"), 6179);
    }
}
