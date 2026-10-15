//! Type registry (TYPE-02): `TypeInfo`, the builtin `TypeEnv`, and small lookup helpers used by
//! every other module. Builtin rows come from the generated `catalog_data::TYPES` table.
//!
//! ```ignore
//! use pgarbor_types::registry::{lookup_type, type_by_name, builtin_env, array_type_of};
//! assert_eq!(lookup_type(23).unwrap().name, "int4");
//! assert_eq!(type_by_name("int4").unwrap().oid, 23);
//! assert_eq!(array_type_of(builtin_env(), 23), Some(1007));
//! ```
//!
//! CONTRACT: all public items are frozen (v1).
use crate::catalog_data::{self, BuiltinType};
use crate::context::TypeEnv;
use crate::oids;
use pgarbor_common::{Oid, PgError, Result};
use std::borrow::Cow;
use std::collections::HashMap;
use std::sync::OnceLock;

/// Owned type metadata (builtin or user-defined). Field meanings = `catalog_data::BuiltinType`.
// CONTRACT
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TypeInfo {
    pub oid: Oid,
    pub name: Cow<'static, str>,
    pub namespace: Oid,
    pub len: i16,
    pub byval: bool,
    pub typtype: u8,
    pub category: u8,
    pub preferred: bool,
    pub delim: u8,
    pub relid: Oid,
    pub elem: Oid,
    pub array: Oid,
    /// I/O routine names. User types reuse builtin routines: enums `enum_in/enum_out/enum_recv/enum_send`,
    /// domains `domain_in/<base out>/domain_recv/<base send>`, composites `record_in/record_out/...`,
    /// arrays `array_in/array_out/array_recv/array_send`.
    pub input: &'static str,
    pub output: &'static str,
    pub receive: &'static str,
    pub send: &'static str,
    pub align: u8,
    pub storage: u8,
    pub basetype: Oid,
    pub typmod: i32,
    pub ndims: i32,
    pub collation: Oid,
}

impl From<&BuiltinType> for TypeInfo {
    fn from(t: &BuiltinType) -> TypeInfo {
        TypeInfo {
            oid: t.oid,
            name: Cow::Borrowed(t.name),
            namespace: t.namespace,
            len: t.len,
            byval: t.byval,
            typtype: t.typtype,
            category: t.category,
            preferred: t.preferred,
            delim: t.delim,
            relid: t.relid,
            elem: t.elem,
            array: t.array,
            input: t.input,
            output: t.output,
            receive: t.receive,
            send: t.send,
            align: t.align,
            storage: t.storage,
            basetype: t.basetype,
            typmod: t.typmod,
            ndims: t.ndims,
            collation: t.collation,
        }
    }
}

impl TypeInfo {
    /// Array type in PG's sense (category 'A' with an element type), e.g. `_int4`.
    pub fn is_array(&self) -> bool {
        self.category == b'A' && self.elem != 0
    }
    pub fn is_enum(&self) -> bool {
        self.typtype == b'e'
    }
    pub fn is_domain(&self) -> bool {
        self.typtype == b'd'
    }
    pub fn is_composite(&self) -> bool {
        self.typtype == b'c'
    }
    pub fn is_pseudo(&self) -> bool {
        self.typtype == b'p'
    }
    /// Collatable (`typcollation != 0`).
    pub fn is_collatable(&self) -> bool {
        self.collation != 0
    }
}

/// `TypeEnv` that knows only the compiled-in PG18 builtin types (search_path = pg_catalog).
// CONTRACT
#[derive(Clone, Copy, Debug, Default)]
pub struct BuiltinTypeEnv;

impl TypeEnv for BuiltinTypeEnv {
    fn type_info(&self, oid: Oid) -> Option<TypeInfo> {
        lookup_type(oid).map(TypeInfo::from)
    }
    fn type_by_name(&self, schema: Option<&str>, typname: &str) -> Option<Oid> {
        match schema {
            None | Some("pg_catalog") => type_by_name(typname).map(|t| t.oid),
            Some("information_schema") => catalog_data::TYPES
                .iter()
                .find(|t| t.namespace == oids::INFORMATION_SCHEMA_NAMESPACE && t.name == typname)
                .map(|t| t.oid),
            Some(_) => None,
        }
    }
}

static BUILTIN_ENV: BuiltinTypeEnv = BuiltinTypeEnv;

/// The process-wide builtin-only environment.
pub fn builtin_env() -> &'static BuiltinTypeEnv {
    &BUILTIN_ENV
}

/// Builtin `pg_type` row by OID (binary search).
pub fn lookup_type(oid: Oid) -> Option<&'static BuiltinType> {
    catalog_data::builtin_type(oid)
}

/// Builtin `pg_type` row by exact catalog `typname` in pg_catalog (e.g. "int4", "_text", "bpchar").
/// SQL spellings like "integer" or "character varying" are handled by `crate::names`.
pub fn type_by_name(typname: &str) -> Option<&'static BuiltinType> {
    static BY_NAME: OnceLock<HashMap<&'static str, usize>> = OnceLock::new();
    let map = BY_NAME.get_or_init(|| {
        let mut m = HashMap::new();
        for (i, t) in catalog_data::TYPES.iter().enumerate() {
            if t.namespace == oids::PG_CATALOG_NAMESPACE {
                m.entry(t.name).or_insert(i);
            }
        }
        m
    });
    map.get(typname).map(|i| &catalog_data::TYPES[*i])
}

/// Metadata via `env`, or PG's internal error `cache lookup failed for type N` (XX000).
pub fn type_info(env: &dyn TypeEnv, oid: Oid) -> Result<TypeInfo> {
    env.type_info(oid).ok_or_else(|| PgError::internal(format!("cache lookup failed for type {oid}")))
}

/// `typarray` of `elem` (e.g. 23 → 1007), or None if the type has no array type.
pub fn array_type_of(env: &dyn TypeEnv, elem: Oid) -> Option<Oid> {
    env.type_info(elem).map(|t| t.array).filter(|a| *a != 0)
}

/// Element type of an array type (e.g. 1007 → 23), or None if `oid` is not an array type
/// (PG `get_element_type`; `name` and `point` have typelem but are not arrays: returns None).
pub fn element_type_of(env: &dyn TypeEnv, oid: Oid) -> Option<Oid> {
    env.type_info(oid).filter(|t| t.is_array()).map(|t| t.elem)
}

/// True if `oid` is an array type.
pub fn is_array_type(env: &dyn TypeEnv, oid: Oid) -> bool {
    element_type_of(env, oid).is_some()
}

/// `typcategory` (b'X' for unknown OIDs, like PG's TYPCATEGORY_INVALID handling of `unknown`).
pub fn type_category(env: &dyn TypeEnv, oid: Oid) -> u8 {
    env.type_info(oid).map(|t| t.category).unwrap_or(b'X')
}

/// `typispreferred`.
pub fn is_preferred_type(env: &dyn TypeEnv, oid: Oid) -> bool {
    env.type_info(oid).map(|t| t.preferred).unwrap_or(false)
}

/// Follow domains to the base type (PG `getBaseType`); non-domains return themselves.
pub fn base_type(env: &dyn TypeEnv, oid: Oid) -> Oid {
    base_type_and_typmod(env, oid, -1).0
}

/// Follow domains to the base type, also returning the innermost domain typmod (PG `getBaseTypeAndTypmod`).
pub fn base_type_and_typmod(env: &dyn TypeEnv, oid: Oid, typmod: i32) -> (Oid, i32) {
    let mut cur = oid;
    let mut tm = typmod;
    for _ in 0..64 {
        match env.type_info(cur) {
            Some(t) if t.typtype == b'd' => {
                tm = t.typmod;
                cur = t.basetype;
            }
            _ => break,
        }
    }
    (cur, tm)
}

/// Polymorphic pseudo-types: anyelement, anyarray, anynonarray, anyenum, anyrange, anymultirange,
/// anycompatible, anycompatiblearray, anycompatiblenonarray, anycompatiblerange, anycompatiblemultirange.
pub fn is_polymorphic(oid: Oid) -> bool {
    matches!(
        oid,
        oids::ANYELEMENT
            | oids::ANYARRAY
            | oids::ANYNONARRAY
            | oids::ANYENUM
            | oids::ANYRANGE
            | oids::ANYMULTIRANGE
            | oids::ANYCOMPATIBLE
            | oids::ANYCOMPATIBLEARRAY
            | oids::ANYCOMPATIBLENONARRAY
            | oids::ANYCOMPATIBLERANGE
            | oids::ANYCOMPATIBLEMULTIRANGE
    )
}

/// String-category types that are binary compatible with text (text, varchar, bpchar, name, unknown).
pub fn is_text_like(oid: Oid) -> bool {
    matches!(oid, oids::TEXT | oids::VARCHAR | oids::BPCHAR | oids::NAME | oids::UNKNOWN)
}
