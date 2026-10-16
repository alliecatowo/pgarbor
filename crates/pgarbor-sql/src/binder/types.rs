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
use pgarbor_catalog::errors as catalog_errors;
use pgarbor_common::{PgError, Result};
use pgarbor_types::datetime::interval_fields as ivf;
use pgarbor_types::names::{resolve_type_name, TypeName};
use pgarbor_types::TypeId;
use sqlparser::ast::{
    ArrayElemTypeDef, CharLengthUnits, CharacterLength, DataType, ExactNumberInfo, IntervalFields, ObjectName,
    ObjectNamePart, TimezoneInfo,
};

use super::context::Binder;
use crate::diag;
use crate::names::QualName;

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
pub fn data_type_to_type_name(b: &mut Binder, dt: &DataType, pos: Option<u32>) -> Result<TypeName> {
    let tn = match dt {
        DataType::Int(l) | DataType::Integer(l) => {
            no_args(*l, pos)?;
            named("integer", Vec::new())
        }
        DataType::Int4(l) => {
            no_args(*l, pos)?;
            named("int4", Vec::new())
        }
        DataType::SmallInt(l) | DataType::Int2(l) => {
            no_args(*l, pos)?;
            named("smallint", Vec::new())
        }
        DataType::BigInt(l) | DataType::Int8(l) => {
            no_args(*l, pos)?;
            named("bigint", Vec::new())
        }
        DataType::Real | DataType::Float4 => named("real", Vec::new()),
        DataType::DoublePrecision | DataType::Float8 => named("double precision", Vec::new()),
        DataType::Double(info) => {
            exact_no_args(info, pos)?;
            named("double precision", Vec::new())
        }
        DataType::Float(info) => match info {
            ExactNumberInfo::None => named("double precision", Vec::new()),
            // The `float(p)` precision rules (1..=24 real, 25..=53 double, errors) live in
            // `resolve_type_name`, which handles the keyword "float" with one modifier.
            ExactNumberInfo::Precision(p) => named("float", vec![to_i32(*p)]),
            ExactNumberInfo::PrecisionAndScale(_, _) => return Err(diag::syntax_error_near(",", pos)),
        },
        DataType::Numeric(info) | DataType::Decimal(info) | DataType::Dec(info) => {
            named("numeric", exact_mods(info))
        }
        DataType::Bool | DataType::Boolean => named("boolean", Vec::new()),
        DataType::Char(l) | DataType::Character(l) => named("character", char_mods(l, pos)?),
        DataType::Varchar(l) | DataType::CharVarying(l) | DataType::CharacterVarying(l) => {
            named("character varying", char_mods(l, pos)?)
        }
        DataType::Text => named("text", Vec::new()),
        DataType::Bytea => named("bytea", Vec::new()),
        DataType::Uuid => named("uuid", Vec::new()),
        DataType::JSON => named("json", Vec::new()),
        DataType::JSONB => named("jsonb", Vec::new()),
        DataType::Regclass => named("regclass", Vec::new()),
        // sqlparser has dedicated variants for these; PG resolves them as ordinary catalog names.
        DataType::TsVector => named("tsvector", Vec::new()),
        DataType::TsQuery => named("tsquery", Vec::new()),
        DataType::Date => named("date", Vec::new()),
        DataType::Time(p, tz) => named(&time_name("time", tz), opt_mods(*p)),
        DataType::Timestamp(p, tz) => named(&time_name("timestamp", tz), opt_mods(*p)),
        DataType::Interval { fields, precision } => TypeName {
            name: "interval".to_string(),
            typmods: opt_mods(*precision),
            interval_range: fields.as_ref().map(interval_range_bits),
            ..TypeName::default()
        },
        DataType::Bit(n) => named("bit", opt_mods(*n)),
        DataType::BitVarying(n) | DataType::VarBit(n) => named("bit varying", opt_mods(*n)),
        DataType::Custom(name, mods) => custom_type_name(b, name, mods, pos)?,
        DataType::Array(elem) => array_element_name(b, elem, pos)?,
        // Other dialects' types and `unsigned`/`tinyint`/... are unknown words to PostgreSQL.
        _ => return Err(type_not_exist(&other_base_name(dt), pos)),
    };
    Ok(tn)
}

/// A `TypeName` with the given lower-case spelling and typmods.
fn named(name: &str, typmods: Vec<i32>) -> TypeName {
    TypeName { name: name.to_string(), typmods, ..TypeName::default() }
}

/// `u64` source value clamped to `i32` (a larger declared value then fails in `typmod_in` with the
/// usual range error rather than wrapping).
fn to_i32(v: u64) -> i32 {
    if v > i32::MAX as u64 {
        i32::MAX
    } else {
        v as i32
    }
}

/// Optional single length/precision as typmods.
fn opt_mods(v: Option<u64>) -> Vec<i32> {
    match v {
        Some(x) => vec![to_i32(x)],
        None => Vec::new(),
    }
}

/// Integer-family and similar names take no parenthesized argument in PostgreSQL.
fn no_args(len: Option<u64>, pos: Option<u32>) -> Result<()> {
    match len {
        None => Ok(()),
        Some(_) => Err(diag::syntax_error_near("(", pos)),
    }
}

fn exact_no_args(info: &ExactNumberInfo, pos: Option<u32>) -> Result<()> {
    match info {
        ExactNumberInfo::None => Ok(()),
        _ => Err(diag::syntax_error_near("(", pos)),
    }
}

/// `numeric(p)` / `numeric(p, s)` modifiers (scale may be negative since PG 15).
fn exact_mods(info: &ExactNumberInfo) -> Vec<i32> {
    match info {
        ExactNumberInfo::None => Vec::new(),
        ExactNumberInfo::Precision(p) => vec![to_i32(*p)],
        ExactNumberInfo::PrecisionAndScale(p, s) => {
            let scale = (*s).clamp(i32::MIN as i64, i32::MAX as i64) as i32;
            vec![to_i32(*p), scale]
        }
    }
}

/// `char(n)` / `varchar(n)` length. `MAX` and the CHARACTERS/OCTETS units are not PostgreSQL syntax.
fn char_mods(len: &Option<CharacterLength>, pos: Option<u32>) -> Result<Vec<i32>> {
    match len {
        None => Ok(Vec::new()),
        Some(CharacterLength::Max) => Err(diag::syntax_error_near("max", pos)),
        Some(CharacterLength::IntegerLength { length, unit }) => {
            match unit {
                None => {}
                Some(CharLengthUnits::Characters) => return Err(diag::syntax_error_near("characters", pos)),
                Some(CharLengthUnits::Octets) => return Err(diag::syntax_error_near("octets", pos)),
            }
            Ok(vec![to_i32(*length)])
        }
    }
}

/// `time`/`timestamp` base name with the zone clause (`timestamptz`/`timetz` shorthands included).
fn time_name(base: &str, tz: &TimezoneInfo) -> String {
    match tz {
        TimezoneInfo::WithTimeZone | TimezoneInfo::Tz => format!("{base} with time zone"),
        TimezoneInfo::None | TimezoneInfo::WithoutTimeZone => format!("{base} without time zone"),
    }
}

/// `datetime::interval_fields` range bits for `INTERVAL <fields>` (PG `INTERVAL_MASK` combinations).
fn interval_range_bits(fields: &IntervalFields) -> i32 {
    match fields {
        IntervalFields::Year => ivf::YEAR,
        IntervalFields::Month => ivf::MONTH,
        IntervalFields::Day => ivf::DAY,
        IntervalFields::Hour => ivf::HOUR,
        IntervalFields::Minute => ivf::MINUTE,
        IntervalFields::Second => ivf::SECOND,
        IntervalFields::YearToMonth => ivf::YEAR | ivf::MONTH,
        IntervalFields::DayToHour => ivf::DAY | ivf::HOUR,
        IntervalFields::DayToMinute => ivf::DAY | ivf::HOUR | ivf::MINUTE,
        IntervalFields::DayToSecond => ivf::DAY | ivf::HOUR | ivf::MINUTE | ivf::SECOND,
        IntervalFields::HourToMinute => ivf::HOUR | ivf::MINUTE,
        IntervalFields::HourToSecond => ivf::HOUR | ivf::MINUTE | ivf::SECOND,
        IntervalFields::MinuteToSecond => ivf::MINUTE | ivf::SECOND,
    }
}

/// Lower-case base word of a type that PostgreSQL does not know (`binary(10)` -> `binary`).
fn other_base_name(dt: &DataType) -> String {
    let text = dt.to_string().to_lowercase();
    let cut = text.find(|c: char| c == '(' || c == '<' || c == '[').unwrap_or(text.len());
    let base = text[..cut].trim();
    if base.is_empty() {
        text.trim().to_string()
    } else {
        base.to_string()
    }
}

/// 42704 `type "<name>" does not exist` at `pos`.
fn type_not_exist(name: &str, pos: Option<u32>) -> PgError {
    diag::with_position(catalog_errors::type_does_not_exist(name), pos)
}

/// Rejects a 3-part name whose database is not the connected one (`diag::cross_database`).
fn check_database(b: &Binder, qn: &QualName) -> Result<()> {
    match &qn.catalog {
        Some(c) if *c != b.ctx.session.database => {
            let mut full = c.clone();
            if let Some(s) = &qn.schema {
                full.push('.');
                full.push_str(s);
            }
            full.push('.');
            full.push_str(&qn.name);
            Err(diag::cross_database(&full, qn.pos))
        }
        _ => Ok(()),
    }
}

/// A user/catalog type name (`mood`, `public.mood`, `"MyType"`, `int4`, `nope(3)`). Modifiers must
/// be integers (`mood(abc)` -> 42601 at the modifier).
fn custom_type_name(b: &mut Binder, name: &ObjectName, mods: &[String], pos: Option<u32>) -> Result<TypeName> {
    let quoted = matches!(name.0.last(), Some(ObjectNamePart::Identifier(id)) if id.quote_style.is_some());
    let qn = b.qual(name)?;
    check_database(b, &qn)?;
    let mut typmods = Vec::with_capacity(mods.len());
    for m in mods {
        let v = m.trim().parse::<i32>().map_err(|_| diag::syntax_error_near(m, pos))?;
        typmods.push(v);
    }
    Ok(TypeName { schema: qn.schema, name: qn.name, quoted, typmods, array_dims: 0, interval_range: None })
}

/// `elem[]`, `elem ARRAY`, `elem ARRAY[n]`, `ARRAY<elem>`: the element name with one more dimension.
/// Declared sizes are ignored, as in PostgreSQL.
fn array_element_name(b: &mut Binder, elem: &ArrayElemTypeDef, pos: Option<u32>) -> Result<TypeName> {
    let inner = match elem {
        ArrayElemTypeDef::SquareBracket(inner, _)
        | ArrayElemTypeDef::AngleBracket(inner)
        | ArrayElemTypeDef::Qualified(inner, _) => inner,
        ArrayElemTypeDef::None => return Err(diag::syntax_error_near("array", pos)),
        ArrayElemTypeDef::Parenthesis(_) => return Err(type_not_exist("array", pos)),
    };
    let mut tn = data_type_to_type_name(b, inner, pos)?;
    tn.array_dims = tn.array_dims.saturating_add(1);
    Ok(tn)
}

/// `data_type_to_type_name` + `pgarbor_types::names::resolve_type_name(b.env, ..)`; errors from the
/// resolver (42704 `type "x" does not exist`, 22023 typmod errors, ...) get `pos` attached when they
/// have none. A multi-dimensional array type resolves to the 1-D array type of the element
/// (`int[][]` is `_int4`, `ndims = 2`).
pub fn resolve_data_type(b: &mut Binder, dt: &DataType, pos: Option<u32>) -> Result<ResolvedType> {
    if let DataType::Custom(name, _) = dt {
        if let Some(parts) = pct_type_parts(name) {
            return resolve_pct_type(b, &parts, pos);
        }
    }
    let tn = data_type_to_type_name(b, dt, pos)?;
    let ndims = tn.array_dims as i32;
    let id = resolve_type_name(b.env, &tn).map_err(|e| diag::with_position(e, pos))?;
    Ok(ResolvedType { id, ndims })
}

/// The parts of a `rel.col%TYPE` reference (`parser::pct_type_refs` writes it as one marker identifier).
fn pct_type_parts(name: &ObjectName) -> Option<Vec<String>> {
    match name.0.as_slice() {
        [ObjectNamePart::Identifier(id)] => id
            .value
            .strip_prefix(crate::parser::PCT_TYPE_MARKER)
            .map(|rest| rest.split('\u{1}').map(str::to_string).collect()),
        _ => None,
    }
}

/// `[schema.]table.column%TYPE`: the column's type (parse_type.c `typenameType` with `pct_type`), with
/// the NOTICE `type reference t.c%TYPE converted to integer`. 42P01 for a missing table, 42703 for a
/// missing column; the reference needs two or three names.
fn resolve_pct_type(b: &mut Binder, parts: &[String], pos: Option<u32>) -> Result<ResolvedType> {
    let written = format!("{}%TYPE", parts.join("."));
    let (schema, table, column) = match parts {
        [t, c] => (None, t.as_str(), c.as_str()),
        [s, t, c] => (Some(s.as_str()), t.as_str(), c.as_str()),
        _ => {
            return Err(diag::with_position(
                PgError::new(
                    pgarbor_common::SqlState::SYNTAX_ERROR,
                    format!("improper %TYPE reference (too many dotted names): {written}"),
                ),
                pos,
            ))
        }
    };
    let found = match b.env.lookup_class(schema, table)? {
        Some(pgarbor_catalog::snapshot::ClassRef::Relation(oid)) => b.env.snapshot().relation(oid),
        _ => None,
    };
    let Some(rel) = found else {
        return Err(diag::with_position(catalog_errors::relation_does_not_exist(schema, table), pos));
    };
    let Some(col) = rel.column_by_name(column) else {
        return Err(diag::with_position(
            catalog_errors::column_of_relation_does_not_exist(column, table),
            pos,
        ));
    };
    let ty = col.type_id;
    let ndims = col.ndims;
    let shown = pgarbor_types::names::format_type(b.env, ty.oid, -1)?;
    b.notice(
        PgError::new(
            pgarbor_common::SqlState::SUCCESSFUL_COMPLETION,
            format!("type reference {written} converted to {shown}"),
        )
        .with_severity(pgarbor_common::Severity::Notice),
    );
    Ok(ResolvedType { id: TypeId::new(ty.oid, -1), ndims })
}

/// Resolve a type referenced by a (possibly schema-qualified) name: `CREATE TABLE t(c mood)`,
/// `x::public.mood`, `DROP TYPE`. Uses `Binder::qual` then `b.env.lookup_type(schema, name)` ->
/// 42704 `type "mood" does not exist`; a relation name resolves to its rowtype (composite) via
/// `TypeEnv::type_by_name`. Shell/pseudo-type restrictions are the caller's.
pub fn resolve_type_by_name(b: &mut Binder, name: &ObjectName) -> Result<TypeId> {
    let qn = b.qual(name)?;
    check_database(b, &qn)?;
    match b.env.lookup_type(qn.schema.as_deref(), &qn.name)? {
        Some(oid) => Ok(TypeId::new(oid, -1)),
        None => Err(diag::with_position(catalog_errors::type_does_not_exist(&qn.display()), qn.pos)),
    }
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::binder::context::{BindContext, BindSession};
    use crate::source::SourceMap;
    use pgarbor_catalog::testing;
    use pgarbor_common::SqlState;
    use pgarbor_types::oids;
    use pgarbor_types::IoConfig;
    use sqlparser::ast::{Expr, SelectItem, SetExpr, Statement};
    use sqlparser::dialect::PostgreSqlDialect;
    use sqlparser::parser::Parser;

    /// The data type written in `SELECT CAST(x AS <sql_type>)` (sqlparser only; no SQL-01 needed).
    fn cast_type(sql_type: &str) -> DataType {
        let text = format!("SELECT CAST(x AS {sql_type})");
        let mut stmts = Parser::parse_sql(&PostgreSqlDialect {}, &text).expect("test SQL parses");
        let stmt = stmts.pop().expect("one statement");
        let Statement::Query(query) = stmt else { panic!("query expected") };
        let SetExpr::Select(select) = query.body.as_ref() else { panic!("select expected") };
        match select.projection.first() {
            Some(SelectItem::UnnamedExpr(Expr::Cast { data_type, .. })) => data_type.clone(),
            other => panic!("cast expected, got {other:?}"),
        }
    }

    /// Run `f` with a binder over the bootstrapped catalog (connected database `postgres`).
    fn with_binder<T>(f: impl FnOnce(&mut Binder) -> Result<T>) -> Result<T> {
        let (_kv, env) = testing::bootstrapped()?;
        let ctx = BindContext {
            env: &env,
            session: BindSession { database: "postgres".to_string() },
            io_config: IoConfig::default(),
            statement_timestamp: 0,
            declared_params: Vec::new(),
        };
        let src = SourceMap::new("x");
        let mut b = Binder::new(&ctx, &src);
        f(&mut b)
    }

    fn name_of(sql_type: &str) -> Result<TypeName> {
        let dt = cast_type(sql_type);
        with_binder(|b| data_type_to_type_name(b, &dt, Some(1)))
    }

    fn resolve(sql_type: &str) -> Result<ResolvedType> {
        let dt = cast_type(sql_type);
        with_binder(|b| resolve_data_type(b, &dt, Some(1)))
    }

    fn expect_err<T>(r: Result<T>) -> PgError {
        match r {
            Ok(_) => panic!("error expected"),
            Err(e) => e,
        }
    }

    #[test]
    fn integer_family_names() {
        assert_eq!(name_of("int").unwrap().name, "integer");
        assert_eq!(name_of("integer").unwrap().name, "integer");
        assert_eq!(name_of("int4").unwrap().name, "int4");
        assert_eq!(name_of("int2").unwrap().name, "smallint");
        assert_eq!(name_of("smallint").unwrap().name, "smallint");
        assert_eq!(name_of("int8").unwrap().name, "bigint");
        assert_eq!(name_of("bigint").unwrap().name, "bigint");
        assert_eq!(resolve("int").unwrap().id.oid, oids::INT4);
        assert_eq!(resolve("int8").unwrap().id.oid, oids::INT8);
    }

    #[test]
    fn integer_with_parenthesis_is_syntax_error() {
        let e = expect_err(name_of("int(11)"));
        assert!(e.sqlstate == SqlState::SYNTAX_ERROR);
    }

    #[test]
    fn float_family() {
        assert_eq!(resolve("real").unwrap().id.oid, oids::FLOAT4);
        assert_eq!(resolve("float4").unwrap().id.oid, oids::FLOAT4);
        assert_eq!(resolve("double precision").unwrap().id.oid, oids::FLOAT8);
        assert_eq!(resolve("float8").unwrap().id.oid, oids::FLOAT8);
        assert_eq!(resolve("float").unwrap().id.oid, oids::FLOAT8);
        assert_eq!(resolve("float(24)").unwrap().id.oid, oids::FLOAT4);
        assert_eq!(resolve("float(25)").unwrap().id.oid, oids::FLOAT8);
        assert_eq!(resolve("float(53)").unwrap().id.oid, oids::FLOAT8);
        let e = expect_err(resolve("float(54)"));
        assert!(e.sqlstate == SqlState::INVALID_PARAMETER_VALUE);
        assert_eq!(e.message, "precision for type float must be less than 54 bits");
        let e = expect_err(resolve("float(0)"));
        assert_eq!(e.message, "precision for type float must be at least 1 bit");
    }

    #[test]
    fn numeric_with_typmod() {
        let id = resolve("numeric(10,2)").unwrap().id;
        assert_eq!(id.oid, oids::NUMERIC);
        // ((precision << 16) | scale) + VARHDRSZ
        assert_eq!(id.typmod, (10 << 16 | 2) + 4);
        assert_eq!(name_of("decimal(7)").unwrap().typmods, vec![7]);
        assert_eq!(name_of("dec").unwrap().name, "numeric");
        assert_eq!(resolve("numeric").unwrap().id.typmod, -1);
    }

    #[test]
    fn text_search_types() {
        // sqlparser parses these as dedicated DataType variants, not as custom names.
        assert_eq!(resolve("tsvector").unwrap().id.oid, oids::TSVECTOR);
        assert_eq!(resolve("tsquery").unwrap().id.oid, oids::TSQUERY);
        assert_eq!(resolve("tsquery[]").unwrap().id.oid, oids::TSQUERY_ARRAY);
    }

    #[test]
    fn character_types() {
        let id = resolve("varchar(5)").unwrap().id;
        assert_eq!(id.oid, oids::VARCHAR);
        assert_eq!(id.typmod, 5 + 4);
        assert_eq!(resolve("varchar").unwrap().id.typmod, -1);
        assert_eq!(resolve("character varying(10)").unwrap().id.typmod, 14);
        // `char` without a length is `character(1)`: typmod 5.
        let id = resolve("char").unwrap().id;
        assert_eq!(id.oid, oids::BPCHAR);
        assert_eq!(id.typmod, 5);
        assert_eq!(resolve("character(3)").unwrap().id.typmod, 7);
        assert_eq!(resolve("text").unwrap().id.oid, oids::TEXT);
        // `"char"` (quoted) is the one-byte internal type.
        assert_eq!(resolve("\"char\"").unwrap().id.oid, 18);
        let e = expect_err(name_of("varchar(max)"));
        assert!(e.sqlstate == SqlState::SYNTAX_ERROR);
    }

    #[test]
    fn boolean_bytea_uuid_json() {
        assert_eq!(resolve("bool").unwrap().id.oid, oids::BOOL);
        assert_eq!(resolve("boolean").unwrap().id.oid, oids::BOOL);
        assert_eq!(resolve("bytea").unwrap().id.oid, oids::BYTEA);
        assert_eq!(resolve("uuid").unwrap().id.oid, oids::UUID);
        assert_eq!(resolve("json").unwrap().id.oid, oids::JSON);
        assert_eq!(resolve("jsonb").unwrap().id.oid, oids::JSONB);
        assert_eq!(resolve("date").unwrap().id.oid, oids::DATE);
    }

    #[test]
    fn time_and_timestamp_zones() {
        let id = resolve("timestamp(3) with time zone").unwrap().id;
        assert_eq!(id.oid, oids::TIMESTAMPTZ);
        assert_eq!(id.typmod, 3);
        assert_eq!(resolve("timestamp").unwrap().id.oid, oids::TIMESTAMP);
        assert_eq!(resolve("timestamptz").unwrap().id.oid, oids::TIMESTAMPTZ);
        assert_eq!(resolve("time with time zone").unwrap().id.oid, oids::TIMETZ);
        let id = resolve("time(2) without time zone").unwrap().id;
        assert_eq!(id.oid, oids::TIME);
        assert_eq!(id.typmod, 2);
    }

    #[test]
    fn interval_fields() {
        let tn = name_of("interval year to month").unwrap();
        assert_eq!(tn.name, "interval");
        assert_eq!(tn.interval_range, Some(ivf::YEAR | ivf::MONTH));
        let tn = name_of("interval day to second(3)").unwrap();
        assert_eq!(tn.interval_range, Some(ivf::DAY | ivf::HOUR | ivf::MINUTE | ivf::SECOND));
        assert_eq!(tn.typmods, vec![3]);
        assert_eq!(name_of("interval").unwrap().interval_range, None);
        let id = resolve("interval year to month").unwrap().id;
        assert_eq!(id.oid, oids::INTERVAL);
        assert_eq!(id.typmod, ((ivf::YEAR | ivf::MONTH) << 16) | 0xFFFF);
    }

    #[test]
    fn arrays_count_dimensions() {
        let r = resolve("int[]").unwrap();
        assert_eq!((r.id.oid, r.ndims), (oids::INT4_ARRAY, 1));
        let r = resolve("integer ARRAY").unwrap();
        assert_eq!((r.id.oid, r.ndims), (oids::INT4_ARRAY, 1));
        let r = resolve("int[3][4]").unwrap();
        assert_eq!((r.id.oid, r.ndims), (oids::INT4_ARRAY, 2));
        let r = resolve("int[][]").unwrap();
        assert_eq!((r.id.oid, r.ndims), (oids::INT4_ARRAY, 2));
        let r = resolve("int4").unwrap();
        assert_eq!(r.ndims, 0);
        // The element typmod is kept on the array type.
        let r = resolve("varchar(5)[]").unwrap();
        assert_eq!(r.ndims, 1);
        assert_eq!(r.id.typmod, 9);
        assert_ne!(r.id.oid, oids::VARCHAR);
    }

    #[test]
    fn custom_names() {
        let tn = name_of("public.mood").unwrap();
        assert_eq!(tn.schema.as_deref(), Some("public"));
        assert_eq!(tn.name, "mood");
        assert!(!tn.quoted);
        let tn = name_of("\"MyType\"").unwrap();
        assert_eq!(tn.name, "MyType");
        assert!(tn.quoted);
        let tn = name_of("MyType").unwrap();
        assert_eq!(tn.name, "mytype");
        let tn = name_of("postgres.public.t").unwrap();
        assert_eq!(tn.schema.as_deref(), Some("public"));
        assert_eq!(tn.name, "t");
        let e = expect_err(name_of("otherdb.public.t"));
        assert!(e.sqlstate == SqlState::FEATURE_NOT_SUPPORTED);
        assert_eq!(e.message, "cross-database references are not implemented: otherdb.public.t");
    }

    #[test]
    fn unknown_types_are_undefined_objects() {
        let e = expect_err(resolve("nope"));
        assert!(e.sqlstate == SqlState::UNDEFINED_OBJECT);
        assert_eq!(e.message, "type \"nope\" does not exist");
        assert_eq!(e.position, Some(1));
        let e = expect_err(resolve("tinyint"));
        assert!(e.sqlstate == SqlState::UNDEFINED_OBJECT);
        assert_eq!(e.message, "type \"tinyint\" does not exist");
        let e = expect_err(resolve("binary(10)"));
        assert!(e.sqlstate == SqlState::UNDEFINED_OBJECT);
        assert_eq!(e.message, "type \"binary\" does not exist");
    }

    #[test]
    fn non_integer_modifier_is_syntax_error() {
        let e = expect_err(name_of("mood(abc)"));
        assert!(e.sqlstate == SqlState::SYNTAX_ERROR);
    }

    #[test]
    fn resolve_type_by_name_builtin_and_missing() {
        let name = ObjectName::from(vec![sqlparser::ast::Ident::new("int4")]);
        let oid = with_binder(|b| resolve_type_by_name(b, &name)).unwrap().oid;
        assert_eq!(oid, oids::INT4);
        let missing = ObjectName::from(vec![sqlparser::ast::Ident::new("nosuchtype")]);
        let e = expect_err(with_binder(|b| resolve_type_by_name(b, &missing)));
        assert!(e.sqlstate == SqlState::UNDEFINED_OBJECT);
        assert_eq!(e.message, "type \"nosuchtype\" does not exist");
    }

    #[test]
    #[ignore = "needs SQL-35 (CREATE TYPE mood AS ENUM) to create the user type"]
    fn user_type_by_name() {
        let name = ObjectName::from(vec![sqlparser::ast::Ident::new("mood")]);
        let r = with_binder(|b| resolve_type_by_name(b, &name));
        assert!(r.is_err());
    }
}
