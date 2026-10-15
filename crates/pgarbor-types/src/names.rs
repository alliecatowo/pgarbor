//! SQL type names ↔ type OIDs: parsing type names written in SQL and PG's `format_type`
//! (TYPE-02). Lease TYP-01.
//!
//! PG18 reference: `src/backend/parser/gram.y` (SimpleTypename, GenericType, Numeric, Character,
//! ConstDatetime, ConstInterval, opt_float), `parser/parse_type.c` (`typenameTypeIdAndMod`,
//! `typenameTypeMod`), `utils/adt/format_type.c` (`format_type_extended`, `printTypmod`).
//!
//! `resolve_type_name(env, tn)`: maps the SQL spelling to a type OID, then computes the typmod with
//! `typmod::typmod_in`, then applies array bounds (`int[]`, `int[3][4]` → the array type; bounds are
//! ignored like PG). Accepted spellings (case-insensitive, already lower-cased by the binder unless quoted):
//! `smallint|int2`, `int|integer|int4`, `bigint|int8`, `real|float4`, `double precision|float8`,
//! `float` (no p → float8; `float(p)`: 1..=24 → float4, 25..=53 → float8, p < 1 → 22023
//! `precision for type float must be at least 1 bit`, p > 53 → 22023
//! `precision for type float must be less than 54 bits`), `numeric|decimal|dec[(p[,s])]`,
//! `boolean|bool`, `char|character[(n)]` → bpchar with DEFAULT LENGTH 1 when no n (typmod 5),
//! `char varying|character varying|varchar[(n)]`, `bpchar[(n)]` (no default length), `text`, `name`,
//! quoted `"char"` (`TypeName::quoted`) → the 1-byte "char" type (18), `bytea`, `date`,
//! `time[(p)] [without time zone]`, `time[(p)] with time zone|timetz`, `timestamp[(p)] [without time zone]`,
//! `timestamp[(p)] with time zone|timestamptz`, `interval [fields] [(p)]` (fields → `interval_fields`
//! range bits, e.g. YEAR → YEAR, DAY TO SECOND → DAY|HOUR|MINUTE|SECOND), `bit[(n)]` (default 1),
//! `bit varying|varbit`, `json`, `jsonb`, `uuid`, `oid`, `regclass`..., and any other catalog `typname`
//! via `env.type_by_name(schema, name)` (user enums/domains/composites, `_int4`, `int4range`...).
//! Unknown → 42704 `type "nosuchtype" does not exist`; typmod on a type without typmodin → 42601
//! `type modifier is not allowed for type "text"`. `serial`/`bigserial`/`smallserial` are NOT types:
//! the DDL binder expands them; here they resolve to 42704.
//!
//! `format_type(env, oid, typmod)` (PG `format_type_extended` with FORMAT_TYPE_TYPEMOD_GIVEN when
//! typmod >= 0), oracle-verified: varchar typmod 14 → `character varying(10)`; numeric (10,2) →
//! `numeric(10,2)`; timestamptz typmod 3 → `timestamp(3) with time zone`; `_int4` → `integer[]`;
//! bpchar typmod 5 → `character(1)`, bpchar no typmod → `bpchar`; interval YEAR → `interval year`;
//! float8 → `double precision`; time typmod 2 → `time(2) without time zone`; oid 0 → `-`; unknown oid →
//! `???`; `_varchar` typmod 14 → `character varying(10)[]`; bit typmod 5 → `bit(5)`; timetz →
//! `time with time zone`; `"char"` → `"char"`; int2 → `smallint`; int8 → `bigint`; bool → `boolean`;
//! float4 → `real`; timestamp → `timestamp without time zone`; varbit → `bit varying`. User types are
//! printed schema-qualified when not visible in the search path (`env.type_by_name(None, name) != Some(oid)`)
//! and quoted with `text::format::quote_identifier` when needed.
use crate::context::TypeEnv;
use crate::datum::TypeId;
use pgarbor_common::{Oid, PgError, Result};

/// A type name as written in SQL (produced by pgarbor-sql from sqlparser's AST).
// CONTRACT
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct TypeName {
    /// Explicit schema qualification, if any.
    pub schema: Option<String>,
    /// Lower-cased (unless quoted) multi-word SQL spelling, single spaces: `"double precision"`,
    /// `"character varying"`, `"timestamp with time zone"`, `"int4"`, `"mytype"`.
    pub name: String,
    /// The name was a quoted identifier (`"char"`, `"MyType"`): no SQL keyword aliasing.
    pub quoted: bool,
    /// Integer type modifiers in order (`varchar(10)` → [10]; `numeric(10,2)` → [10, 2]).
    pub typmods: Vec<i32>,
    /// Number of `[]` suffixes (0 = not an array).
    pub array_dims: u32,
    /// Interval field restriction as `datetime::interval_fields` range bits (None = full range).
    pub interval_range: Option<i32>,
}

/// Resolve a SQL type name (see module docs).
// CONTRACT
#[allow(unused_variables)]
pub fn resolve_type_name(env: &dyn TypeEnv, tn: &TypeName) -> Result<TypeId> {
    Err(PgError::unimplemented("TYP-01", "type name resolution"))
}

/// Parse a type name from text (regtype input, `to_regtype`, `format('%s'::regtype)`):
/// e.g. `"character varying(10)[]"`, `"timestamp(3) with time zone"`, `"public.mood"`, `"\"char\""`.
// CONTRACT
#[allow(unused_variables)]
pub fn parse_type_string(env: &dyn TypeEnv, s: &str) -> Result<TypeId> {
    Err(PgError::unimplemented("TYP-01", "type name parsing"))
}

/// PG `format_type` (see module docs for exact outputs). typmod < 0 = none.
// CONTRACT
#[allow(unused_variables)]
pub fn format_type(env: &dyn TypeEnv, oid: Oid, typmod: i32) -> Result<String> {
    Err(PgError::unimplemented("TYP-01", "format_type"))
}

/// Short display name used inside error messages for builtin types (PG uses `format_type_be`, i.e.
/// `format_type` without typmod): `integer`, `text`, `double precision`, `timestamp with time zone`.
pub fn display_name(env: &dyn TypeEnv, oid: Oid) -> String {
    format_type(env, oid, -1).unwrap_or_else(|_| {
        env.type_info(oid).map(|t| t.name.into_owned()).unwrap_or_else(|| oid.to_string())
    })
}
