//! SQL type names -> `TypeId` (SQL-07). PG references: `parse_type.c` (`typenameTypeId`,
//! `typenameTypeIdAndMod`, `LookupTypeName`), `gram.y` `Typename`/`SimpleTypename`/`GenericType`/
//! `Numeric`/`Character`/`ConstDatetime`/`ConstInterval`, `format_type.c`.
//!
//! The heavy lifting is `pgarbor_types::names::resolve_type_name(env, &TypeName)`; this module only
//! converts `sqlparser::ast::DataType` / `ObjectName` into [`pgarbor_types::names::TypeName`]:
//!
//! | sqlparser `DataType` | `TypeName.name` (lower-case, single spaces) | typmods |
//! |---|---|---|
//! | `Int(None)`/`Integer(None)`/`Int4` | `integer` / `int4` as written | - |
//! | `SmallInt`/`Int2`, `BigInt`/`Int8`, `Real`/`Float4`, `DoublePrecision`/`Float8`/`Double`, `Float(None)` | `smallint`, `bigint`, `real`, `double precision`, `double precision` | - |
//! | `Float(Some(p))` | PG: p<=24 -> `real`, 25..=53 -> `double precision`, else 22023 `precision for type float must be less than 54 bits`, p<1 -> `precision for type float must be at least 1 bit` | - |
//! | `Numeric(info)`/`Decimal(info)`/`Dec` | `numeric` | `[p]` or `[p, s]` from `ExactNumberInfo` |
//! | `Varchar(len)`/`CharVarying`/`CharacterVarying` | `character varying` | `[n]` (`CharacterLength::IntegerLength`); `MAX` is not PG -> 42601 |
//! | `Char`/`Character`/`Nchar` (no length) | `character` (bpchar, length 1) | `[n]` |
//! | `Text`, `String`? | `text` | - |
//! | `Boolean`/`Bool` | `boolean` | - |
//! | `Date`, `Time(prec, tz)`, `Timestamp(prec, tz)`, `Datetime`? | `date`, `time without time zone` / `time with time zone`, `timestamp without time zone` / `timestamp with time zone` (`TimezoneInfo::{None,WithoutTimeZone,Tz,WithTimeZone}`) | `[prec]` |
//! | `Interval { fields, precision }` | `interval` | `interval_range` from `fields` (YEAR, MONTH, DAY, HOUR, MINUTE, SECOND, YEAR TO MONTH, DAY TO HOUR, ... as the `datetime::interval_fields` bits) and `[precision]` |
//! | `Bytea`, `Uuid`, `JSON`, `JSONB`, `Regclass`, `Oid`? | `bytea`, `uuid`, `json`, `jsonb`, `regclass` | - |
//! | `Array(ArrayElemTypeDef::SquareBracket(inner, size))`, `AngleBracket` | element's TypeName with `array_dims += 1` (declared sizes are ignored like PG; `int[3]` is `int[]`) | - |
//! | `Custom(ObjectName, modifiers: Vec<String>)` | the (schema-qualified) name; `quoted` = first identifier had a quote style; numeric modifiers parsed to i32 (non-numeric -> 42601) | |
//! | anything else (`Binary`, `Varbinary`, `Blob`, `Clob`, `Tinyint`, `Unsigned`, `Struct`, `Map`, `Nested`, `Enum` inline, `Set`, `Datetime64`, `LowCardinality`, ... = other dialects) | - | 42704 `type "<lower-case spelling>" does not exist` with position (PG's grammar treats unknown words as generic type names, so `tinyint` is a missing type, not a syntax error) |
//!
//! `%TYPE` references (`DataType::Custom` with `%TYPE`?) are 0A000 (`%TYPE`).
//! Serial pseudo-types (`serial`, `bigserial`, `smallserial` and `serial2/4/8`) are NOT resolved here:
//! `ddl_table` intercepts them before calling this module.
use pgarbor_common::{PgError, Result};
use pgarbor_types::names::TypeName;
use pgarbor_types::TypeId;
use sqlparser::ast::{DataType, ObjectName};

use super::context::Binder;

/// A resolved column/cast type: id plus the declared array dimension count (`attndims`).
// CONTRACT
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ResolvedType {
    pub id: TypeId,
    /// Number of `[]` suffixes written (0 for non-array declarations; `int[][]` -> 2).
    pub ndims: i32,
}

/// Convert a sqlparser data type to a `TypeName` (table above). `pos` is the type's position for
/// errors. Does not touch the catalog.
#[allow(unused_variables)]
pub fn data_type_to_type_name(b: &mut Binder, dt: &DataType, pos: Option<u32>) -> Result<TypeName> {
    Err(PgError::unimplemented("SQL-07", "DataType conversion (types::data_type_to_type_name)"))
}

/// `data_type_to_type_name` + `pgarbor_types::names::resolve_type_name(b.env, ..)`; errors from the
/// resolver (42704 `type "x" does not exist`, 22023 typmod errors, ...) get `pos` attached when they
/// have none. A multi-dimensional array type resolves to the 1-D array type of the element
/// (`int[][]` is `_int4`, `ndims = 2`).
#[allow(unused_variables)]
pub fn resolve_data_type(b: &mut Binder, dt: &DataType, pos: Option<u32>) -> Result<ResolvedType> {
    Err(PgError::unimplemented("SQL-07", "data type resolution (types::resolve_data_type)"))
}

/// Resolve a type referenced by a (possibly schema-qualified) name: `CREATE TABLE t(c mood)`,
/// `x::public.mood`, `DROP TYPE`. Uses `Binder::qual` then `b.env.lookup_type(schema, name)` ->
/// 42704 `type "mood" does not exist`; a relation name resolves to its rowtype (composite) via
/// `TypeEnv::type_by_name`. Shell/pseudo-type restrictions are the caller's.
#[allow(unused_variables)]
pub fn resolve_type_by_name(b: &mut Binder, name: &ObjectName) -> Result<TypeId> {
    Err(PgError::unimplemented("SQL-07", "type lookup by name (types::resolve_type_by_name)"))
}

/// True for the pseudo-types that cannot be column types (`record`, `anyelement`, `void`, `unknown`,
/// `cstring`, `internal`, ...): `CREATE TABLE t (c anyelement)` -> 42P16 `column "c" has
/// pseudo-type anyelement`. Helper for DDL binders. Fully implemented.
pub fn is_pseudo_type(oid: pgarbor_common::Oid) -> bool {
    use pgarbor_types::oids;
    matches!(
        oid,
        oids::RECORD
            | oids::UNKNOWN
            | oids::VOID
            | oids::CSTRING
            | oids::INTERNAL
            | oids::ANYELEMENT
            | oids::ANYARRAY
            | oids::ANYNONARRAY
            | oids::ANYENUM
    )
}
