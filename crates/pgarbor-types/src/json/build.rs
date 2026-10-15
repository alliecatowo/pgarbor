//! JSON constructors, SQL-value → JSON conversion, jsonb mutation and inspection (TYPE-08).
//! Lease TYP-35.
//!
//! PG18 reference: `utils/adt/json.c` (`datum_to_json_internal`, `json_build_object`, `to_json`,
//! `row_to_json`, `array_to_json`, `json_object`), `jsonb.c` (`datum_to_jsonb_internal`, `to_jsonb`,
//! `jsonb_build_*`, jsonb casts to scalar types), `jsonfuncs.c` (`jsonb_set`, `jsonb_set_lax`,
//! `jsonb_insert`, `jsonb_strip_nulls`, `json_strip_nulls`, `jsonb_pretty`, `json(b)_typeof`,
//! `json(b)_array_length`).
//! Owned procs (ImplModule::JsonBuild): `to_json to_jsonb json_build_object json_build_array
//! json_build_object_noargs json_build_array_noargs jsonb_build_object jsonb_build_array
//! jsonb_build_object_noargs jsonb_build_array_noargs json_object json_object_two_arg jsonb_object
//! jsonb_object_two_arg row_to_json row_to_json_pretty array_to_json array_to_json_pretty
//! json_strip_nulls jsonb_strip_nulls jsonb_set jsonb_set_lax jsonb_insert jsonb_pretty json_typeof
//! jsonb_typeof json_array_length jsonb_array_length jsonb_bool jsonb_int2 jsonb_int4 jsonb_int8
//! jsonb_float4 jsonb_float8 jsonb_numeric`.
//!
//! Value conversion (`datum_to_jsonb` / `datum_to_json_text`) by SQL type (`ctx.arg_types`):
//! NULL → null; bool → true/false; int/float/numeric → number, except NaN/±Infinity → STRING
//! (`to_jsonb('NaN'::float8)` = `"NaN"`, `"Infinity"`); date/timestamp/timestamptz/time → ISO 8601
//! strings with `T` and offsets as `+00:00` (`to_jsonb(timestamptz '2024-01-01 00:00+00')` =
//! `"2024-01-01T00:00:00+00:00"`, PG `JsonEncodeDateTime`); json/jsonb → embedded as-is; arrays →
//! JSON arrays (multi-dim nested); records/composites → objects with field names (anonymous ROW()
//! fields are `f1`, `f2`, ...); everything else → its text output as a JSON string.
//! Oracle: `to_json(row(1,'a'))` = `{"f1":1,"f2":"a"}` (json text: NO spaces); `json_build_object('a',1,'b',null)`
//! = `{"a" : 1, "b" : null}` (json_build_* use `" : "` and `", "`); `jsonb_build_array(1,'a',null,true)`
//! = `[1, "a", null, true]`; `json_object('{a,1,b,2}')` = `{"a" : "1", "b" : "2"}`.
//! Errors: `json_build_object` odd arg count → 22023 `argument list must have even number of elements`
//! with HINT `The arguments of json_build_object() must consist of alternating keys and values.`; null key →
//! 22023 `null value not allowed for object key` (oracle-verified); `json_array_length('{}')` → 22023
//! `cannot get array length of a non-array`.
//! jsonb → scalar casts: `('{"a":1}'::jsonb -> 'a')::int` = 1, `'1.5'::jsonb::int` = 2 (numeric rounding);
//! wrong kind → 22023 `cannot cast jsonb string to type integer` (kinds: string/numeric/boolean/object/array/null).
use super::io as jio;
use super::parse::{self, JsonValue};
use super::{jsonb_key_cmp, Jsonb};
use crate::array::Array;
use crate::catalog_data::BuiltinProc;
use crate::context::{FuncCtx, TypeEnv};
use crate::datetime::calendar::{j2date, timestamp2tm};
use crate::datetime::tz::offset_at_utc;
use crate::datetime::{Tm, POSTGRES_EPOCH_JDATE};
use crate::datum::{
    type_mismatch, Datum, TypeId, DATE_INFINITY, DATE_NEG_INFINITY, TIMESTAMP_INFINITY,
    TIMESTAMP_NEG_INFINITY,
};
use crate::func::{FuncImpl, ScalarFn};
use crate::io as tio;
use crate::numeric::funcs::{to_f64, to_i16, to_i32, to_i64};
use crate::numeric::io::{format_numeric, parse_numeric};
use crate::numeric::Numeric;
use crate::oids;
use crate::registry;
use pgarbor_common::{PgError, Result, SqlState};

/// Convert a SQL value of type `ty` to jsonb (`to_jsonb`).
// CONTRACT
#[allow(unused_variables)]
pub fn datum_to_jsonb(d: &Datum, ty: TypeId, ctx: &FuncCtx) -> Result<Jsonb> {
    to_jsonb_value(d, ty, ctx)
}

/// Convert a SQL value of type `ty` to json text (`to_json`), compact form.
// CONTRACT
#[allow(unused_variables)]
pub fn datum_to_json_text(d: &Datum, ty: TypeId, ctx: &FuncCtx) -> Result<String> {
    let mut out = String::new();
    write_json(&mut out, d, ty, ctx, false)?;
    Ok(out)
}

/// Function registry hook (see `crate::func`).
#[allow(unused_variables)]
pub(crate) fn lookup(proc: &BuiltinProc) -> Option<FuncImpl> {
    let f: ScalarFn = match proc.prosrc {
        "to_json" => to_json_fn,
        "to_jsonb" => to_jsonb_fn,
        "json_build_object" => json_build_object_fn,
        "json_build_object_noargs" => json_build_object_noargs_fn,
        "json_build_array" => json_build_array_fn,
        "json_build_array_noargs" => json_build_array_noargs_fn,
        "jsonb_build_object" => jsonb_build_object_fn,
        "jsonb_build_object_noargs" => jsonb_build_object_noargs_fn,
        "jsonb_build_array" => jsonb_build_array_fn,
        "jsonb_build_array_noargs" => jsonb_build_array_noargs_fn,
        "json_object" => json_object_fn,
        "json_object_two_arg" => json_object_two_arg_fn,
        "jsonb_object" => jsonb_object_fn,
        "jsonb_object_two_arg" => jsonb_object_two_arg_fn,
        "row_to_json" => row_to_json_fn,
        "row_to_json_pretty" => row_to_json_pretty_fn,
        "array_to_json" => array_to_json_fn,
        "array_to_json_pretty" => array_to_json_pretty_fn,
        "json_strip_nulls" => json_strip_nulls_fn,
        "jsonb_strip_nulls" => jsonb_strip_nulls_fn,
        "jsonb_set" => jsonb_set_fn,
        "jsonb_set_lax" => jsonb_set_lax_fn,
        "jsonb_insert" => jsonb_insert_fn,
        "jsonb_pretty" => jsonb_pretty_fn,
        "json_typeof" => json_typeof_fn,
        "jsonb_typeof" => jsonb_typeof_fn,
        "json_array_length" => json_array_length_fn,
        "jsonb_array_length" => jsonb_array_length_fn,
        "jsonb_bool" => jsonb_bool_fn,
        "jsonb_int2" => jsonb_int2_fn,
        "jsonb_int4" => jsonb_int4_fn,
        "jsonb_int8" => jsonb_int8_fn,
        "jsonb_float4" => jsonb_float4_fn,
        "jsonb_float8" => jsonb_float8_fn,
        "jsonb_numeric" => jsonb_numeric_fn,
        _ => return None,
    };
    Some(FuncImpl::Scalar(f))
}

/// 0A000 for a JsonBuild-module proc without implementation.
pub(crate) fn unsupported(proc: &BuiltinProc) -> PgError {
    PgError::unimplemented("TYP-35", format!("function {}", proc.name))
}

// ---------------------------------------------------------------------------------------------
// Shared helpers
// ---------------------------------------------------------------------------------------------

/// Category of a scalar type for JSON conversion (PG `json_categorize_type`, domains looked through).
#[derive(Clone, Copy, PartialEq, Eq)]
enum Cat {
    Bool,
    Numeric,
    Date,
    Timestamp,
    TimestampTz,
    Other,
}

fn categorize(env: &dyn TypeEnv, ty: TypeId) -> Cat {
    match registry::base_type(env, ty.oid) {
        oids::BOOL => Cat::Bool,
        oids::INT2 | oids::INT4 | oids::INT8 | oids::FLOAT4 | oids::FLOAT8 | oids::NUMERIC => Cat::Numeric,
        oids::DATE => Cat::Date,
        oids::TIMESTAMP => Cat::Timestamp,
        oids::TIMESTAMPTZ => Cat::TimestampTz,
        _ => Cat::Other,
    }
}

/// Type used for a field whose declared type is unknown (anonymous `ROW(...)` values), derived
/// from the datum variant.
fn variant_type(d: &Datum) -> TypeId {
    match d {
        Datum::Null | Datum::Array(_) => TypeId::UNKNOWN,
        Datum::Bool(_) => TypeId::BOOL,
        Datum::Int2(_) => TypeId::INT2,
        Datum::Int4(_) => TypeId::INT4,
        Datum::Int8(_) => TypeId::INT8,
        Datum::Float4(_) => TypeId::FLOAT4,
        Datum::Float8(_) => TypeId::FLOAT8,
        Datum::Numeric(_) => TypeId::NUMERIC,
        Datum::Text(_) => TypeId::TEXT,
        Datum::Bytea(_) => TypeId::BYTEA,
        Datum::Uuid(_) => TypeId::UUID,
        Datum::Date(_) => TypeId::DATE,
        Datum::Time(_) => TypeId::TIME,
        Datum::TimeTz(..) => TypeId::TIMETZ,
        Datum::Timestamp(_) => TypeId::TIMESTAMP,
        Datum::TimestampTz(_) => TypeId::TIMESTAMPTZ,
        Datum::Interval(_) => TypeId::INTERVAL,
        Datum::Json(_) => TypeId::JSON,
        Datum::Jsonb(_) => TypeId::JSONB,
        Datum::Oid(_) => TypeId::OID,
        Datum::Char(_) => TypeId::CHAR,
        Datum::Record(_) => TypeId::RECORD,
    }
}

fn arg(args: &[Datum], i: usize) -> Result<&Datum> {
    args.get(i).ok_or_else(|| PgError::internal("function called with too few arguments"))
}

fn jsonb_arg(d: &Datum) -> Result<&Jsonb> {
    d.as_jsonb().ok_or_else(|| type_mismatch("jsonb", d))
}

fn array_arg(d: &Datum) -> Result<&Array> {
    d.as_array().ok_or_else(|| type_mismatch("array", d))
}

fn text_of(d: &Datum) -> Result<&str> {
    d.as_str().ok_or_else(|| type_mismatch("text", d))
}

fn bool_of(d: &Datum) -> Result<bool> {
    d.as_bool().ok_or_else(|| type_mismatch("bool", d))
}

/// A text[] element: NULL → None, text → Some.
fn text_item(d: &Datum) -> Result<Option<&str>> {
    match d {
        Datum::Null => Ok(None),
        other => other.as_str().map(Some).ok_or_else(|| type_mismatch("text", other)),
    }
}

fn wrong_subscripts() -> PgError {
    PgError::new(SqlState::ARRAY_SUBSCRIPT_ERROR, "wrong number of array subscripts")
}

fn null_key_error() -> PgError {
    PgError::new(SqlState::NULL_VALUE_NOT_ALLOWED, "null value not allowed for object key")
}

fn output_text(d: &Datum, ty: TypeId, ctx: &FuncCtx) -> Result<String> {
    tio::output(&ctx.io_ctx(), ty, d)
}

fn dim_len(a: &Array, dim: usize) -> Result<usize> {
    a.dims
        .get(dim)
        .map(|d| d.0.max(0) as usize)
        .ok_or_else(|| PgError::internal("array dimension out of range"))
}

fn next_item<'a>(a: &'a Array, idx: &mut usize) -> Result<&'a Datum> {
    let d = a.items.get(*idx).ok_or_else(|| PgError::internal("array item count mismatch"))?;
    *idx += 1;
    Ok(d)
}

/// JSON number grammar (PG `IsValidJsonNumber`): decides whether a numeric text is emitted raw.
fn is_valid_json_number(s: &str) -> bool {
    let b = s.as_bytes();
    let len = b.len();
    let mut i = 0usize;
    if i < len && b[i] == b'-' {
        i += 1;
    }
    if i >= len {
        return false;
    }
    if b[i] == b'0' {
        i += 1;
    } else if b[i].is_ascii_digit() {
        while i < len && b[i].is_ascii_digit() {
            i += 1;
        }
    } else {
        return false;
    }
    if i < len && b[i] == b'.' {
        i += 1;
        let start = i;
        while i < len && b[i].is_ascii_digit() {
            i += 1;
        }
        if i == start {
            return false;
        }
    }
    if i < len && (b[i] == b'e' || b[i] == b'E') {
        i += 1;
        if i < len && (b[i] == b'+' || b[i] == b'-') {
            i += 1;
        }
        let start = i;
        while i < len && b[i].is_ascii_digit() {
            i += 1;
        }
        if i == start {
            return false;
        }
    }
    i == len
}

// ---------------------------------------------------------------------------------------------
// Datetime encoding (PG JsonEncodeDateTime, USE_XSD_DATES)
// ---------------------------------------------------------------------------------------------

/// Appends `YYYY-MM-DD` (display year) and returns true when the date is BC.
fn push_date(out: &mut String, year: i32, mon: i32, mday: i32) -> bool {
    let (disp, bc) = if year > 0 { (year, false) } else { (1 - year, true) };
    out.push_str(&format!("{:04}-{:02}-{:02}", disp, mon, mday));
    bc
}

/// XSD zone suffix `+HH:MM` (`:SS` only when seconds are nonzero); `west` is seconds WEST of UTC.
fn push_xsd_zone(out: &mut String, west: i32) {
    let east = west.wrapping_neg();
    out.push(if east >= 0 { '+' } else { '-' });
    let a = east.unsigned_abs();
    let (hour, min, sec) = (a / 3600, (a % 3600) / 60, a % 60);
    out.push_str(&format!("{:02}:{:02}", hour, min));
    if sec != 0 {
        out.push_str(&format!(":{:02}", sec));
    }
}

fn xsd_tm(tm: &Tm, tz_west: Option<i32>) -> String {
    let mut s = String::with_capacity(32);
    let bc = push_date(&mut s, tm.year, tm.mon, tm.mday);
    s.push('T');
    s.push_str(&format!("{:02}:{:02}:{:02}", tm.hour, tm.min, tm.sec));
    if tm.fsec != 0 {
        let frac = format!("{:06}", tm.fsec);
        s.push('.');
        s.push_str(frac.trim_end_matches('0'));
    }
    if let Some(west) = tz_west {
        push_xsd_zone(&mut s, west);
    }
    if bc {
        s.push_str(" BC");
    }
    s
}

/// Unquoted XSD text of a date / timestamp / timestamptz datum ("infinity" for the infinities).
fn xsd_datetime(d: &Datum, ctx: &FuncCtx) -> Result<String> {
    match d {
        Datum::Date(v) => {
            if *v == DATE_INFINITY {
                return Ok("infinity".to_string());
            }
            if *v == DATE_NEG_INFINITY {
                return Ok("-infinity".to_string());
            }
            let jd = (*v)
                .checked_add(POSTGRES_EPOCH_JDATE)
                .ok_or_else(|| PgError::new(SqlState::DATETIME_VALUE_OUT_OF_RANGE, "date out of range"))?;
            let (y, m, dd) = j2date(jd);
            let mut s = String::with_capacity(16);
            let bc = push_date(&mut s, y, m, dd);
            if bc {
                s.push_str(" BC");
            }
            Ok(s)
        }
        Datum::Timestamp(t) => {
            if *t == TIMESTAMP_INFINITY {
                return Ok("infinity".to_string());
            }
            if *t == TIMESTAMP_NEG_INFINITY {
                return Ok("-infinity".to_string());
            }
            let tm = timestamp2tm(*t, None)?;
            Ok(xsd_tm(&tm, None))
        }
        Datum::TimestampTz(t) => {
            if *t == TIMESTAMP_INFINITY {
                return Ok("infinity".to_string());
            }
            if *t == TIMESTAMP_NEG_INFINITY {
                return Ok("-infinity".to_string());
            }
            let east = offset_at_utc(&ctx.io.time_zone, *t)?;
            let west = east.wrapping_neg();
            let tm = timestamp2tm(*t, Some(west))?;
            Ok(xsd_tm(&tm, Some(west)))
        }
        other => Err(type_mismatch("date/time", other)),
    }
}

// ---------------------------------------------------------------------------------------------
// json text output (PG datum_to_json_internal, compact unless `pretty`)
// ---------------------------------------------------------------------------------------------

/// Members of a record value as (field name, field type, value); dropped attributes are skipped.
fn record_members<'d>(ctx: &FuncCtx, ty: TypeId, vals: &'d [Datum]) -> Result<Vec<(String, TypeId, &'d Datum)>> {
    let fields = if ty.oid == oids::RECORD { None } else { ctx.env.composite_fields(ty.oid) };
    match fields {
        Some(fields) => {
            if fields.len() != vals.len() {
                return Err(PgError::internal(format!(
                    "record of type {} has {} values for {} attributes",
                    ty.oid,
                    vals.len(),
                    fields.len()
                )));
            }
            Ok(fields
                .iter()
                .zip(vals.iter())
                .filter(|(f, _)| !f.dropped)
                .map(|(f, v)| (f.name.clone(), f.type_id, v))
                .collect())
        }
        None => Ok(vals
            .iter()
            .enumerate()
            .map(|(i, v)| (format!("f{}", i + 1), variant_type(v), v))
            .collect()),
    }
}

fn write_json(out: &mut String, d: &Datum, ty: TypeId, ctx: &FuncCtx, pretty: bool) -> Result<()> {
    match d {
        Datum::Null => out.push_str("null"),
        Datum::Json(s) => out.push_str(s),
        Datum::Jsonb(j) => out.push_str(&jio::format_jsonb(j)?),
        Datum::Array(a) => write_array(out, a, ctx, pretty)?,
        Datum::Record(vals) => {
            let members = record_members(ctx, ty, vals)?;
            write_composite(out, &members, ctx, pretty)?;
        }
        _ => write_scalar_json(out, d, ty, ctx)?,
    }
    Ok(())
}

fn write_scalar_json(out: &mut String, d: &Datum, ty: TypeId, ctx: &FuncCtx) -> Result<()> {
    match categorize(ctx.env, ty) {
        Cat::Bool => {
            out.push_str(if bool_of(d)? { "true" } else { "false" });
        }
        Cat::Numeric => {
            let t = output_text(d, ty, ctx)?;
            if is_valid_json_number(&t) {
                out.push_str(&t);
            } else {
                out.push_str(&parse::escape_json(&t));
            }
        }
        Cat::Date | Cat::Timestamp | Cat::TimestampTz => {
            out.push_str(&parse::escape_json(&xsd_datetime(d, ctx)?));
        }
        Cat::Other => {
            out.push_str(&parse::escape_json(&output_text(d, ty, ctx)?));
        }
    }
    Ok(())
}

fn write_composite(
    out: &mut String,
    members: &[(String, TypeId, &Datum)],
    ctx: &FuncCtx,
    pretty: bool,
) -> Result<()> {
    let sep = if pretty { ",\n " } else { "," };
    out.push('{');
    for (i, (name, ty, v)) in members.iter().enumerate() {
        if i > 0 {
            out.push_str(sep);
        }
        out.push_str(&parse::escape_json(name));
        out.push(':');
        write_json(out, v, *ty, ctx, false)?;
    }
    out.push('}');
    Ok(())
}

fn write_array(out: &mut String, a: &Array, ctx: &FuncCtx, pretty: bool) -> Result<()> {
    if a.items.is_empty() {
        out.push_str("[]");
        return Ok(());
    }
    let mut idx = 0usize;
    write_dim(out, a, 0, &mut idx, ctx, pretty)
}

/// PG `array_dim_to_json`: nested brackets per dimension; line feeds only between outer elements.
fn write_dim(out: &mut String, a: &Array, dim: usize, idx: &mut usize, ctx: &FuncCtx, pretty: bool) -> Result<()> {
    let n = dim_len(a, dim)?;
    let sep = if pretty { ",\n " } else { "," };
    let elem = TypeId::of(a.elem);
    out.push('[');
    for i in 0..n {
        if i > 0 {
            out.push_str(sep);
        }
        if dim + 1 == a.dims.len() {
            let d = next_item(a, idx)?;
            write_json(out, d, elem, ctx, false)?;
        } else {
            write_dim(out, a, dim + 1, idx, ctx, false)?;
        }
    }
    out.push(']');
    Ok(())
}

// ---------------------------------------------------------------------------------------------
// jsonb value conversion (PG datum_to_jsonb_internal)
// ---------------------------------------------------------------------------------------------

fn to_jsonb_value(d: &Datum, ty: TypeId, ctx: &FuncCtx) -> Result<Jsonb> {
    Ok(match d {
        Datum::Null => Jsonb::Null,
        Datum::Json(s) => parse::parse_jsonb(s)?,
        Datum::Jsonb(j) => j.clone(),
        Datum::Array(a) => array_to_jsonb(a, ctx)?,
        Datum::Record(vals) => {
            let members = record_members(ctx, ty, vals)?;
            let mut pairs = Vec::with_capacity(members.len());
            for (name, t, v) in members {
                pairs.push((name, to_jsonb_value(v, t, ctx)?));
            }
            Jsonb::object_from_pairs(pairs)
        }
        _ => scalar_to_jsonb(d, ty, ctx)?,
    })
}

fn scalar_to_jsonb(d: &Datum, ty: TypeId, ctx: &FuncCtx) -> Result<Jsonb> {
    match categorize(ctx.env, ty) {
        Cat::Bool => Ok(Jsonb::Bool(bool_of(d)?)),
        Cat::Numeric => numeric_to_jsonb(d, ty, ctx),
        Cat::Date | Cat::Timestamp | Cat::TimestampTz => Ok(Jsonb::String(xsd_datetime(d, ctx)?)),
        Cat::Other => Ok(Jsonb::String(output_text(d, ty, ctx)?)),
    }
}

fn numeric_to_jsonb(d: &Datum, ty: TypeId, ctx: &FuncCtx) -> Result<Jsonb> {
    match d {
        Datum::Numeric(n) => {
            if n.is_finite() {
                Ok(Jsonb::Number(n.clone()))
            } else {
                Ok(Jsonb::String(format_numeric(n)?))
            }
        }
        Datum::Int2(v) => Ok(Jsonb::Number(Numeric::from_i64(i64::from(*v)))),
        Datum::Int4(v) => Ok(Jsonb::Number(Numeric::from_i64(i64::from(*v)))),
        Datum::Int8(v) => Ok(Jsonb::Number(Numeric::from_i64(*v))),
        Datum::Float4(_) | Datum::Float8(_) => {
            let t = output_text(d, ty, ctx)?;
            if is_valid_json_number(&t) {
                Ok(Jsonb::Number(parse_numeric(&t)?))
            } else {
                Ok(Jsonb::String(t))
            }
        }
        other => Err(type_mismatch("numeric", other)),
    }
}

fn array_to_jsonb(a: &Array, ctx: &FuncCtx) -> Result<Jsonb> {
    if a.items.is_empty() {
        return Ok(Jsonb::Array(Vec::new()));
    }
    let mut idx = 0usize;
    jsonb_dim(a, 0, &mut idx, ctx)
}

fn jsonb_dim(a: &Array, dim: usize, idx: &mut usize, ctx: &FuncCtx) -> Result<Jsonb> {
    let n = dim_len(a, dim)?;
    let elem = TypeId::of(a.elem);
    let mut out = Vec::with_capacity(n);
    for _ in 0..n {
        if dim + 1 == a.dims.len() {
            let d = next_item(a, idx)?;
            out.push(to_jsonb_value(d, elem, ctx)?);
        } else {
            out.push(jsonb_dim(a, dim + 1, idx, ctx)?);
        }
    }
    Ok(Jsonb::Array(out))
}

/// Object key text (PG key handling: scalars only; booleans and datetimes as plain text).
fn key_text(d: &Datum, ty: TypeId, ctx: &FuncCtx) -> Result<String> {
    match d {
        Datum::Json(_) | Datum::Jsonb(_) | Datum::Array(_) | Datum::Record(_) => Err(PgError::new(
            SqlState::INVALID_PARAMETER_VALUE,
            "key value must be scalar, not array, composite, or json",
        )),
        _ => match categorize(ctx.env, ty) {
            Cat::Bool => Ok(if bool_of(d)? { "true".to_string() } else { "false".to_string() }),
            Cat::Date | Cat::Timestamp | Cat::TimestampTz => xsd_datetime(d, ctx),
            _ => output_text(d, ty, ctx),
        },
    }
}

fn check_even(n: usize, func: &str) -> Result<()> {
    if n % 2 != 0 {
        return Err(PgError::new(SqlState::INVALID_PARAMETER_VALUE, "argument list must have even number of elements")
            .with_hint(format!("The arguments of {func} must consist of alternating keys and values.")));
    }
    Ok(())
}

// ---------------------------------------------------------------------------------------------
// to_json / to_jsonb / row_to_json / array_to_json
// ---------------------------------------------------------------------------------------------

fn to_json_fn(args: &[Datum], ctx: &FuncCtx) -> Result<Datum> {
    Ok(Datum::Json(datum_to_json_text(arg(args, 0)?, ctx.arg_type(0), ctx)?))
}

fn to_jsonb_fn(args: &[Datum], ctx: &FuncCtx) -> Result<Datum> {
    Ok(Datum::Jsonb(datum_to_jsonb(arg(args, 0)?, ctx.arg_type(0), ctx)?))
}

fn row_to_json_fn(args: &[Datum], ctx: &FuncCtx) -> Result<Datum> {
    let mut out = String::new();
    write_json(&mut out, arg(args, 0)?, ctx.arg_type(0), ctx, false)?;
    Ok(Datum::Json(out))
}

fn row_to_json_pretty_fn(args: &[Datum], ctx: &FuncCtx) -> Result<Datum> {
    let pretty = bool_of(arg(args, 1)?)?;
    let mut out = String::new();
    write_json(&mut out, arg(args, 0)?, ctx.arg_type(0), ctx, pretty)?;
    Ok(Datum::Json(out))
}

fn array_to_json_fn(args: &[Datum], ctx: &FuncCtx) -> Result<Datum> {
    let mut out = String::new();
    write_json(&mut out, arg(args, 0)?, ctx.arg_type(0), ctx, false)?;
    Ok(Datum::Json(out))
}

fn array_to_json_pretty_fn(args: &[Datum], ctx: &FuncCtx) -> Result<Datum> {
    let pretty = bool_of(arg(args, 1)?)?;
    let mut out = String::new();
    write_json(&mut out, arg(args, 0)?, ctx.arg_type(0), ctx, pretty)?;
    Ok(Datum::Json(out))
}

// ---------------------------------------------------------------------------------------------
// json_build_* / jsonb_build_*
// ---------------------------------------------------------------------------------------------

fn json_build_object_fn(args: &[Datum], ctx: &FuncCtx) -> Result<Datum> {
    check_even(args.len(), "json_build_object()")?;
    let mut out = String::from("{");
    for (p, kv) in args.chunks(2).enumerate() {
        if p > 0 {
            out.push_str(", ");
        }
        if let [k, v] = kv {
            if k.is_null() {
                return Err(null_key_error());
            }
            let ktext = key_text(k, ctx.arg_type(2 * p), ctx)?;
            out.push_str(&parse::escape_json(&ktext));
            out.push_str(" : ");
            write_json(&mut out, v, ctx.arg_type(2 * p + 1), ctx, false)?;
        }
    }
    out.push('}');
    Ok(Datum::Json(out))
}

fn json_build_object_noargs_fn(_args: &[Datum], _ctx: &FuncCtx) -> Result<Datum> {
    Ok(Datum::Json("{}".to_string()))
}

fn json_build_array_fn(args: &[Datum], ctx: &FuncCtx) -> Result<Datum> {
    let mut out = String::from("[");
    for (i, v) in args.iter().enumerate() {
        if i > 0 {
            out.push_str(", ");
        }
        write_json(&mut out, v, ctx.arg_type(i), ctx, false)?;
    }
    out.push(']');
    Ok(Datum::Json(out))
}

fn json_build_array_noargs_fn(_args: &[Datum], _ctx: &FuncCtx) -> Result<Datum> {
    Ok(Datum::Json("[]".to_string()))
}

fn jsonb_build_object_fn(args: &[Datum], ctx: &FuncCtx) -> Result<Datum> {
    check_even(args.len(), "jsonb_build_object()")?;
    let mut pairs = Vec::with_capacity(args.len() / 2);
    for (p, kv) in args.chunks(2).enumerate() {
        if let [k, v] = kv {
            if k.is_null() {
                return Err(PgError::new(
                    SqlState::INVALID_PARAMETER_VALUE,
                    format!("argument {}: key must not be null", 2 * p + 1),
                ));
            }
            let key = key_text(k, ctx.arg_type(2 * p), ctx)?;
            let val = to_jsonb_value(v, ctx.arg_type(2 * p + 1), ctx)?;
            pairs.push((key, val));
        }
    }
    Ok(Datum::Jsonb(Jsonb::object_from_pairs(pairs)))
}

fn jsonb_build_object_noargs_fn(_args: &[Datum], _ctx: &FuncCtx) -> Result<Datum> {
    Ok(Datum::Jsonb(Jsonb::Object(Vec::new())))
}

fn jsonb_build_array_fn(args: &[Datum], ctx: &FuncCtx) -> Result<Datum> {
    let mut items = Vec::with_capacity(args.len());
    for (i, v) in args.iter().enumerate() {
        items.push(to_jsonb_value(v, ctx.arg_type(i), ctx)?);
    }
    Ok(Datum::Jsonb(Jsonb::Array(items)))
}

fn jsonb_build_array_noargs_fn(_args: &[Datum], _ctx: &FuncCtx) -> Result<Datum> {
    Ok(Datum::Jsonb(Jsonb::Array(Vec::new())))
}

// ---------------------------------------------------------------------------------------------
// json_object / jsonb_object (text[] forms)
// ---------------------------------------------------------------------------------------------

type KvPairs<'a> = Vec<(Option<&'a str>, Option<&'a str>)>;

/// Key/value pairs of a text[] in the `json_object(text[])` layout: 1-D with an even count, or
/// 2-D with two columns (PG `json_object`).
fn kv_pairs(a: &Array) -> Result<KvPairs<'_>> {
    match a.dims.len() {
        0 => return Ok(Vec::new()),
        1 => {
            if a.dims[0].0 % 2 != 0 {
                return Err(PgError::new(
                    SqlState::ARRAY_SUBSCRIPT_ERROR,
                    "array must have even number of elements",
                ));
            }
        }
        2 => {
            if a.dims[1].0 != 2 {
                return Err(PgError::new(SqlState::ARRAY_SUBSCRIPT_ERROR, "array must have two columns"));
            }
        }
        _ => return Err(wrong_subscripts()),
    }
    let mut out = Vec::with_capacity(a.items.len() / 2);
    for pair in a.items.chunks(2) {
        if let [k, v] = pair {
            out.push((text_item(k)?, text_item(v)?));
        }
    }
    Ok(out)
}

fn kv_json_text(pairs: &[(Option<&str>, Option<&str>)]) -> Result<String> {
    let mut out = String::from("{");
    for (i, &(k, v)) in pairs.iter().enumerate() {
        if i > 0 {
            out.push_str(", ");
        }
        let key = k.ok_or_else(null_key_error)?;
        out.push_str(&parse::escape_json(key));
        out.push_str(" : ");
        match v {
            Some(s) => out.push_str(&parse::escape_json(s)),
            None => out.push_str("null"),
        }
    }
    out.push('}');
    Ok(out)
}

fn kv_jsonb(pairs: &[(Option<&str>, Option<&str>)]) -> Result<Jsonb> {
    let mut out = Vec::with_capacity(pairs.len());
    for &(k, v) in pairs {
        let key = k.ok_or_else(null_key_error)?;
        let val = match v {
            Some(s) => Jsonb::String(s.to_string()),
            None => Jsonb::Null,
        };
        out.push((key.to_string(), val));
    }
    Ok(Jsonb::object_from_pairs(out))
}

fn json_object_fn(args: &[Datum], _ctx: &FuncCtx) -> Result<Datum> {
    let a = array_arg(arg(args, 0)?)?;
    let pairs = kv_pairs(a)?;
    Ok(Datum::Json(kv_json_text(&pairs)?))
}

fn json_object_two_arg_fn(args: &[Datum], _ctx: &FuncCtx) -> Result<Datum> {
    match two_arg_pairs(args)? {
        None => Ok(Datum::Json("{}".to_string())),
        Some(pairs) => Ok(Datum::Json(kv_json_text(&pairs)?)),
    }
}

fn jsonb_object_fn(args: &[Datum], _ctx: &FuncCtx) -> Result<Datum> {
    let a = array_arg(arg(args, 0)?)?;
    let pairs = kv_pairs(a)?;
    Ok(Datum::Jsonb(kv_jsonb(&pairs)?))
}

fn jsonb_object_two_arg_fn(args: &[Datum], _ctx: &FuncCtx) -> Result<Datum> {
    match two_arg_pairs(args)? {
        None => Ok(Datum::Jsonb(Jsonb::Object(Vec::new()))),
        Some(pairs) => Ok(Datum::Jsonb(kv_jsonb(&pairs)?)),
    }
}

/// `(keys text[], values text[])` → pairs; `None` for the zero-dimensional (`{}`) case.
fn two_arg_pairs(args: &[Datum]) -> Result<Option<KvPairs<'_>>> {
    let ka = array_arg(arg(args, 0)?)?;
    let va = array_arg(arg(args, 1)?)?;
    let nk = ka.dims.len();
    let nv = va.dims.len();
    if nk > 1 || nk != nv {
        return Err(wrong_subscripts());
    }
    if nk == 0 {
        return Ok(None);
    }
    if ka.items.len() != va.items.len() {
        return Err(PgError::new(SqlState::ARRAY_SUBSCRIPT_ERROR, "mismatched array dimensions"));
    }
    let mut pairs = Vec::with_capacity(ka.items.len());
    for (k, v) in ka.items.iter().zip(va.items.iter()) {
        pairs.push((text_item(k)?, text_item(v)?));
    }
    Ok(Some(pairs))
}

// ---------------------------------------------------------------------------------------------
// json_strip_nulls / jsonb_strip_nulls / jsonb_pretty
// ---------------------------------------------------------------------------------------------

fn strip_json_value(v: JsonValue, in_arrays: bool) -> JsonValue {
    match v {
        JsonValue::Object(pairs) => JsonValue::Object(
            pairs
                .into_iter()
                .filter(|(_, v)| !matches!(v, JsonValue::Null))
                .map(|(k, v)| (k, strip_json_value(v, in_arrays)))
                .collect(),
        ),
        JsonValue::Array(items) => JsonValue::Array(
            items
                .into_iter()
                .filter(|v| !(in_arrays && matches!(v, JsonValue::Null)))
                .map(|v| strip_json_value(v, in_arrays))
                .collect(),
        ),
        other => other,
    }
}

fn write_json_value_tree(out: &mut String, v: &JsonValue) {
    match v {
        JsonValue::Null => out.push_str("null"),
        JsonValue::Bool(true) => out.push_str("true"),
        JsonValue::Bool(false) => out.push_str("false"),
        JsonValue::Number(lit) => out.push_str(lit),
        JsonValue::String(s) => out.push_str(&parse::escape_json(s)),
        JsonValue::Array(items) => {
            out.push('[');
            for (i, item) in items.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                write_json_value_tree(out, item);
            }
            out.push(']');
        }
        JsonValue::Object(pairs) => {
            out.push('{');
            for (i, (k, value)) in pairs.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                out.push_str(&parse::escape_json(k));
                out.push(':');
                write_json_value_tree(out, value);
            }
            out.push('}');
        }
    }
}

fn json_strip_nulls_fn(args: &[Datum], _ctx: &FuncCtx) -> Result<Datum> {
    let s = text_of(arg(args, 0)?)?;
    let strip_in_arrays = bool_of(arg(args, 1)?)?;
    let v = strip_json_value(parse::parse_json_value(s)?, strip_in_arrays);
    let mut out = String::with_capacity(s.len());
    write_json_value_tree(&mut out, &v);
    Ok(Datum::Json(out))
}

fn strip_jsonb(j: Jsonb, in_arrays: bool) -> Jsonb {
    match j {
        Jsonb::Object(pairs) => Jsonb::Object(
            pairs
                .into_iter()
                .filter(|(_, v)| !matches!(v, Jsonb::Null))
                .map(|(k, v)| (k, strip_jsonb(v, in_arrays)))
                .collect(),
        ),
        Jsonb::Array(items) => Jsonb::Array(
            items
                .into_iter()
                .filter(|v| !(in_arrays && matches!(v, Jsonb::Null)))
                .map(|v| strip_jsonb(v, in_arrays))
                .collect(),
        ),
        other => other,
    }
}

fn jsonb_strip_nulls_fn(args: &[Datum], _ctx: &FuncCtx) -> Result<Datum> {
    let j = jsonb_arg(arg(args, 0)?)?.clone();
    let strip_in_arrays = bool_of(arg(args, 1)?)?;
    Ok(Datum::Jsonb(strip_jsonb(j, strip_in_arrays)))
}

fn jsonb_pretty_fn(args: &[Datum], _ctx: &FuncCtx) -> Result<Datum> {
    Ok(Datum::Text(jio::format_jsonb_pretty(jsonb_arg(arg(args, 0)?)?)?))
}

// ---------------------------------------------------------------------------------------------
// jsonb_set / jsonb_set_lax / jsonb_insert (PG jsonfuncs.c setPath)
// ---------------------------------------------------------------------------------------------

#[derive(Clone, Copy, PartialEq, Eq)]
enum PathOp {
    Create,
    Replace,
    Delete,
    InsertBefore,
    InsertAfter,
}

impl PathOp {
    /// PG `JB_PATH_CREATE_OR_INSERT`.
    fn creates(self) -> bool {
        matches!(self, PathOp::Create | PathOp::InsertBefore | PathOp::InsertAfter)
    }
}

/// PG `strtoint(s, base 10)` acceptance: optional leading whitespace and sign, then digits, nothing after.
fn parse_path_index(s: &str, level: usize) -> Result<i64> {
    let t = s.trim_start_matches([' ', '\t', '\n', '\r', '\u{0b}', '\u{0c}']);
    let (neg, digits) = match t.as_bytes().first() {
        Some(b'-') => (true, &t[1..]),
        Some(b'+') => (false, &t[1..]),
        _ => (false, t),
    };
    let not_int = || {
        PgError::new(
            SqlState::INVALID_TEXT_REPRESENTATION,
            format!("path element at position {} is not an integer: \"{}\"", level + 1, s),
        )
    };
    if digits.is_empty() || !digits.bytes().all(|b| b.is_ascii_digit()) {
        return Err(not_int());
    }
    let magnitude: i64 = digits.parse::<i64>().map_err(|_| not_int())?;
    let v = if neg { -magnitude } else { magnitude };
    if v < i64::from(i32::MIN) || v > i64::from(i32::MAX) {
        return Err(not_int());
    }
    Ok(v)
}

fn null_path_error(level: usize) -> PgError {
    PgError::new(
        SqlState::NULL_VALUE_NOT_ALLOWED,
        format!("path element at position {} is null", level + 1),
    )
}

fn path_elems(path: &Array) -> Result<Vec<Option<&str>>> {
    path.items
        .iter()
        .map(|d| match d {
            Datum::Null => Ok(None),
            other => other.as_str().map(Some).ok_or_else(|| type_mismatch("text", other)),
        })
        .collect()
}

fn container_len(j: &Jsonb) -> usize {
    match j {
        Jsonb::Object(p) => p.len(),
        Jsonb::Array(a) => a.len(),
        _ => 0,
    }
}

/// PG `setPath`: applies `op` at `level` of `path` below `node`. Scalars are returned unchanged;
/// a missing object key or out-of-range index is a no-op unless the operation creates it.
fn set_path(node: Jsonb, path: &[Option<&str>], level: usize, newval: &Jsonb, op: PathOp) -> Result<Jsonb> {
    let elem = match path.get(level) {
        Some(Some(s)) => *s,
        Some(None) => return Err(null_path_error(level)),
        None => return Err(PgError::internal("path level out of range")),
    };
    let last = level + 1 == path.len();
    match node {
        Jsonb::Object(mut pairs) => {
            match pairs.binary_search_by(|(k, _)| jsonb_key_cmp(k, elem)) {
                Ok(i) => {
                    if last {
                        match op {
                            PathOp::Create | PathOp::Replace => pairs[i].1 = newval.clone(),
                            PathOp::Delete => {
                                pairs.remove(i);
                            }
                            PathOp::InsertBefore | PathOp::InsertAfter => {
                                return Err(PgError::new(
                                    SqlState::INVALID_PARAMETER_VALUE,
                                    "cannot replace existing key",
                                )
                                .with_hint("Try using the function jsonb_set to replace key value."));
                            }
                        }
                    } else {
                        let child = std::mem::replace(&mut pairs[i].1, Jsonb::Null);
                        pairs[i].1 = set_path(child, path, level + 1, newval, op)?;
                    }
                }
                Err(i) => {
                    if last && op.creates() {
                        pairs.insert(i, (elem.to_string(), newval.clone()));
                    }
                }
            }
            Ok(Jsonb::Object(pairs))
        }
        Jsonb::Array(items) => {
            let nelems = items.len() as i64;
            let mut idx = parse_path_index(elem, level)?;
            let mut front = false;
            if idx < 0 {
                if -idx > nelems {
                    // PG INT_MIN sentinel: out-of-range negative index inserts at the front.
                    front = true;
                } else {
                    idx += nelems;
                }
            }
            if !front && idx > nelems {
                idx = nelems;
            }
            let mut out: Vec<Jsonb> = Vec::with_capacity(items.len() + 1);
            let mut done = false;
            if (front || nelems == 0) && last && op.creates() {
                out.push(newval.clone());
                done = true;
            }
            for (i, item) in items.into_iter().enumerate() {
                if !front && i as i64 == idx {
                    done = true;
                    if last {
                        match op {
                            PathOp::Create | PathOp::Replace => out.push(newval.clone()),
                            PathOp::InsertBefore => {
                                out.push(newval.clone());
                                out.push(item);
                            }
                            PathOp::InsertAfter => {
                                out.push(item);
                                out.push(newval.clone());
                            }
                            PathOp::Delete => {}
                        }
                    } else {
                        out.push(set_path(item, path, level + 1, newval, op)?);
                    }
                } else {
                    out.push(item);
                }
            }
            if op.creates() && !done && last {
                out.push(newval.clone());
            }
            Ok(Jsonb::Array(out))
        }
        scalar => Ok(scalar),
    }
}

fn jsonb_set_core(target: &Jsonb, path: &Array, newval: &Jsonb, create: bool) -> Result<Jsonb> {
    if path.dims.len() > 1 {
        return Err(wrong_subscripts());
    }
    if target.is_scalar() {
        return Err(PgError::new(SqlState::INVALID_PARAMETER_VALUE, "cannot set path in scalar"));
    }
    if container_len(target) == 0 && !create {
        return Ok(target.clone());
    }
    let elems = path_elems(path)?;
    if elems.is_empty() {
        return Ok(target.clone());
    }
    let op = if create { PathOp::Create } else { PathOp::Replace };
    set_path(target.clone(), &elems, 0, newval, op)
}

fn jsonb_delete_core(target: &Jsonb, path: &Array) -> Result<Jsonb> {
    if path.dims.len() > 1 {
        return Err(wrong_subscripts());
    }
    if target.is_scalar() {
        return Err(PgError::new(SqlState::INVALID_PARAMETER_VALUE, "cannot delete path in scalar"));
    }
    if container_len(target) == 0 {
        return Ok(target.clone());
    }
    let elems = path_elems(path)?;
    if elems.is_empty() {
        return Ok(target.clone());
    }
    set_path(target.clone(), &elems, 0, &Jsonb::Null, PathOp::Delete)
}

fn jsonb_insert_core(target: &Jsonb, path: &Array, newval: &Jsonb, after: bool) -> Result<Jsonb> {
    if path.dims.len() > 1 {
        return Err(wrong_subscripts());
    }
    if target.is_scalar() {
        return Err(PgError::new(SqlState::INVALID_PARAMETER_VALUE, "cannot set path in scalar"));
    }
    let elems = path_elems(path)?;
    if elems.is_empty() {
        return Ok(target.clone());
    }
    let op = if after { PathOp::InsertAfter } else { PathOp::InsertBefore };
    set_path(target.clone(), &elems, 0, newval, op)
}

fn jsonb_set_fn(args: &[Datum], _ctx: &FuncCtx) -> Result<Datum> {
    let target = jsonb_arg(arg(args, 0)?)?;
    let path = array_arg(arg(args, 1)?)?;
    let newval = jsonb_arg(arg(args, 2)?)?;
    let create = bool_of(arg(args, 3)?)?;
    Ok(Datum::Jsonb(jsonb_set_core(target, path, newval, create)?))
}

fn jsonb_set_lax_fn(args: &[Datum], ctx: &FuncCtx) -> Result<Datum> {
    let (target_d, path_d, new_d, create_d, treat_d) =
        (arg(args, 0)?, arg(args, 1)?, arg(args, 2)?, arg(args, 3)?, arg(args, 4)?);
    if target_d.is_null() || path_d.is_null() || create_d.is_null() {
        return Ok(Datum::Null);
    }
    let bad_treatment = || {
        PgError::new(
            SqlState::INVALID_PARAMETER_VALUE,
            "null_value_treatment must be \"delete_key\", \"return_target\", \"use_json_null\", or \"raise_exception\"",
        )
    };
    let treatment = match treat_d.as_str() {
        Some(s) => s,
        None => return Err(bad_treatment()),
    };
    if !new_d.is_null() {
        return jsonb_set_fn(args, ctx);
    }
    let target = jsonb_arg(target_d)?;
    let path = array_arg(path_d)?;
    let create = bool_of(create_d)?;
    match treatment {
        "raise_exception" => Err(PgError::new(SqlState::NULL_VALUE_NOT_ALLOWED, "JSON value must not be null")
            .with_detail("Exception was raised because null_value_treatment is \"raise_exception\".")
            .with_hint("To avoid, either change the null_value_treatment argument or ensure that an SQL NULL is not passed.")),
        "use_json_null" => Ok(Datum::Jsonb(jsonb_set_core(target, path, &Jsonb::Null, create)?)),
        "delete_key" => Ok(Datum::Jsonb(jsonb_delete_core(target, path)?)),
        "return_target" => Ok(Datum::Jsonb(target.clone())),
        _ => Err(bad_treatment()),
    }
}

fn jsonb_insert_fn(args: &[Datum], _ctx: &FuncCtx) -> Result<Datum> {
    let target = jsonb_arg(arg(args, 0)?)?;
    let path = array_arg(arg(args, 1)?)?;
    let newval = jsonb_arg(arg(args, 2)?)?;
    let after = bool_of(arg(args, 3)?)?;
    Ok(Datum::Jsonb(jsonb_insert_core(target, path, newval, after)?))
}

// ---------------------------------------------------------------------------------------------
// json_typeof / jsonb_typeof / array lengths
// ---------------------------------------------------------------------------------------------

fn json_value_type_name(v: &JsonValue) -> &'static str {
    match v {
        JsonValue::Null => "null",
        JsonValue::Bool(_) => "boolean",
        JsonValue::Number(_) => "number",
        JsonValue::String(_) => "string",
        JsonValue::Array(_) => "array",
        JsonValue::Object(_) => "object",
    }
}

fn json_typeof_fn(args: &[Datum], _ctx: &FuncCtx) -> Result<Datum> {
    let s = text_of(arg(args, 0)?)?;
    let v = parse::parse_json_value(s)?;
    Ok(Datum::Text(json_value_type_name(&v).to_string()))
}

fn jsonb_typeof_fn(args: &[Datum], _ctx: &FuncCtx) -> Result<Datum> {
    Ok(Datum::Text(jsonb_arg(arg(args, 0)?)?.type_name().to_string()))
}

fn array_length_result(n: usize) -> Result<Datum> {
    i32::try_from(n)
        .map(Datum::Int4)
        .map_err(|_| PgError::new(SqlState::PROGRAM_LIMIT_EXCEEDED, "array length out of range"))
}

fn json_array_length_fn(args: &[Datum], _ctx: &FuncCtx) -> Result<Datum> {
    let s = text_of(arg(args, 0)?)?;
    match parse::parse_json_value(s)? {
        JsonValue::Array(items) => array_length_result(items.len()),
        JsonValue::Object(_) => Err(PgError::new(
            SqlState::INVALID_PARAMETER_VALUE,
            "cannot get array length of a non-array",
        )),
        _ => Err(PgError::new(SqlState::INVALID_PARAMETER_VALUE, "cannot get array length of a scalar")),
    }
}

fn jsonb_array_length_fn(args: &[Datum], _ctx: &FuncCtx) -> Result<Datum> {
    match jsonb_arg(arg(args, 0)?)? {
        Jsonb::Array(items) => array_length_result(items.len()),
        Jsonb::Object(_) => Err(PgError::new(
            SqlState::INVALID_PARAMETER_VALUE,
            "cannot get array length of a non-array",
        )),
        _ => Err(PgError::new(SqlState::INVALID_PARAMETER_VALUE, "cannot get array length of a scalar")),
    }
}

// ---------------------------------------------------------------------------------------------
// jsonb → scalar casts (PG jsonb.c / numeric.c); a jsonb null casts to SQL NULL
// ---------------------------------------------------------------------------------------------

fn cannot_cast(j: &Jsonb, target: &str) -> PgError {
    let kind = match j {
        Jsonb::Null => "null",
        Jsonb::Bool(_) => "boolean",
        Jsonb::Number(_) => "numeric",
        Jsonb::String(_) => "string",
        Jsonb::Array(_) => "array",
        Jsonb::Object(_) => "object",
    };
    PgError::new(SqlState::INVALID_PARAMETER_VALUE, format!("cannot cast jsonb {kind} to type {target}"))
}

/// `Ok(None)` for a jsonb null (SQL NULL result).
fn jsonb_numeric_of(d: &Datum, target: &str) -> Result<Option<Numeric>> {
    match jsonb_arg(d)? {
        Jsonb::Null => Ok(None),
        Jsonb::Number(n) => Ok(Some(n.clone())),
        other => Err(cannot_cast(other, target)),
    }
}

fn jsonb_bool_fn(args: &[Datum], _ctx: &FuncCtx) -> Result<Datum> {
    match jsonb_arg(arg(args, 0)?)? {
        Jsonb::Null => Ok(Datum::Null),
        Jsonb::Bool(b) => Ok(Datum::Bool(*b)),
        other => Err(cannot_cast(other, "boolean")),
    }
}

fn jsonb_int2_fn(args: &[Datum], _ctx: &FuncCtx) -> Result<Datum> {
    match jsonb_numeric_of(arg(args, 0)?, "smallint")? {
        None => Ok(Datum::Null),
        Some(n) => Ok(Datum::Int2(to_i16(&n)?)),
    }
}

fn jsonb_int4_fn(args: &[Datum], _ctx: &FuncCtx) -> Result<Datum> {
    match jsonb_numeric_of(arg(args, 0)?, "integer")? {
        None => Ok(Datum::Null),
        Some(n) => Ok(Datum::Int4(to_i32(&n)?)),
    }
}

fn jsonb_int8_fn(args: &[Datum], _ctx: &FuncCtx) -> Result<Datum> {
    match jsonb_numeric_of(arg(args, 0)?, "bigint")? {
        None => Ok(Datum::Null),
        Some(n) => Ok(Datum::Int8(to_i64(&n)?)),
    }
}

fn jsonb_float4_fn(args: &[Datum], _ctx: &FuncCtx) -> Result<Datum> {
    match jsonb_numeric_of(arg(args, 0)?, "real")? {
        None => Ok(Datum::Null),
        Some(n) => {
            let v = to_f64(&n)?;
            let f = v as f32;
            if f.is_infinite() && v.is_finite() {
                return Err(PgError::new(SqlState::NUMERIC_VALUE_OUT_OF_RANGE, "value out of range: overflow"));
            }
            Ok(Datum::Float4(f))
        }
    }
}

fn jsonb_float8_fn(args: &[Datum], _ctx: &FuncCtx) -> Result<Datum> {
    match jsonb_numeric_of(arg(args, 0)?, "double precision")? {
        None => Ok(Datum::Null),
        Some(n) => Ok(Datum::Float8(to_f64(&n)?)),
    }
}

fn jsonb_numeric_fn(args: &[Datum], _ctx: &FuncCtx) -> Result<Datum> {
    match jsonb_numeric_of(arg(args, 0)?, "numeric")? {
        None => Ok(Datum::Null),
        Some(n) => Ok(Datum::Numeric(n)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::context::{IoConfig, TestSession};
    use crate::numeric::io::parse_numeric;
    use crate::registry::builtin_env;

    fn with_ctx<R>(tys: &[TypeId], f: impl FnOnce(&FuncCtx) -> R) -> R {
        let cfg = IoConfig::default();
        let sess = TestSession::default();
        let ctx = FuncCtx {
            env: builtin_env(),
            io: &cfg,
            session: &sess,
            arg_types: tys,
            result_type: TypeId::UNKNOWN,
            collation: 0,
        };
        f(&ctx)
    }

    fn text(s: &str) -> Datum {
        Datum::Text(s.to_string())
    }

    #[test]
    fn to_jsonb_numeric_keeps_scale() {
        let n = parse_numeric("1.50").expect("numeric");
        let got = with_ctx(&[TypeId::NUMERIC], |ctx| {
            datum_to_jsonb(&Datum::Numeric(n.clone()), TypeId::NUMERIC, ctx)
        })
        .expect("to_jsonb");
        assert_eq!(got, Jsonb::Number(n));
    }

    #[test]
    fn nan_float_is_a_string() {
        let got = with_ctx(&[TypeId::FLOAT8], |ctx| datum_to_jsonb(&Datum::Float8(f64::NAN), TypeId::FLOAT8, ctx))
            .expect("to_jsonb");
        assert_eq!(got, Jsonb::String("NaN".to_string()));
    }

    #[test]
    fn timestamptz_xsd_format() {
        // 2024-01-01 00:00:00 UTC is 757382400 s after 2000-01-01.
        let ts = 757_382_400_i64 * 1_000_000;
        let got = with_ctx(&[TypeId::TIMESTAMPTZ], |ctx| {
            datum_to_jsonb(&Datum::TimestampTz(ts), TypeId::TIMESTAMPTZ, ctx)
        })
        .expect("to_jsonb");
        assert_eq!(got, Jsonb::String("2024-01-01T00:00:00+00:00".to_string()));
    }

    #[test]
    fn to_json_anonymous_row_has_no_spaces() {
        let row = Datum::Record(vec![Datum::Int4(1), text("a")]);
        let got = with_ctx(&[TypeId::RECORD], |ctx| datum_to_json_text(&row, TypeId::RECORD, ctx)).expect("to_json");
        assert_eq!(got, "{\"f1\":1,\"f2\":\"a\"}");
    }

    #[test]
    fn json_build_object_spacing_and_null() {
        let args = [text("a"), Datum::Int4(1), text("b"), Datum::Null];
        let tys = [TypeId::TEXT, TypeId::INT4, TypeId::TEXT, TypeId::UNKNOWN];
        let got = with_ctx(&tys, |ctx| json_build_object_fn(&args, ctx)).expect("json_build_object");
        assert_eq!(got, Datum::Json("{\"a\" : 1, \"b\" : null}".to_string()));
    }

    #[test]
    fn json_build_object_odd_args_is_22023() {
        let args = [text("a")];
        let tys = [TypeId::TEXT];
        let err = with_ctx(&tys, |ctx| json_build_object_fn(&args, ctx)).err().expect("error");
        assert_eq!(err.sqlstate, SqlState::INVALID_PARAMETER_VALUE);
    }

    #[test]
    fn jsonb_set_adds_missing_last_key() {
        let target = Jsonb::Object(vec![("a".to_string(), Jsonb::Number(parse_numeric("1").expect("n")))]);
        let path = Array::from_vec(oids::TEXT, vec![text("b")]);
        let newval = Jsonb::Number(parse_numeric("2").expect("n"));
        let got = jsonb_set_core(&target, &path, &newval, true).expect("jsonb_set");
        assert_eq!(
            got,
            Jsonb::Object(vec![
                ("a".to_string(), Jsonb::Number(parse_numeric("1").expect("n"))),
                ("b".to_string(), Jsonb::Number(parse_numeric("2").expect("n"))),
            ])
        );
    }

    #[test]
    fn jsonb_insert_before_index_zero() {
        let target = Jsonb::Array(vec![
            Jsonb::Number(parse_numeric("1").expect("n")),
            Jsonb::Number(parse_numeric("2").expect("n")),
        ]);
        let path = Array::from_vec(oids::TEXT, vec![text("0")]);
        let newval = Jsonb::Number(parse_numeric("0").expect("n"));
        let got = jsonb_insert_core(&target, &path, &newval, false).expect("jsonb_insert");
        assert_eq!(
            got,
            Jsonb::Array(vec![
                Jsonb::Number(parse_numeric("0").expect("n")),
                Jsonb::Number(parse_numeric("1").expect("n")),
                Jsonb::Number(parse_numeric("2").expect("n")),
            ])
        );
    }

    #[test]
    fn jsonb_string_to_int_is_22023() {
        let j = Datum::Jsonb(Jsonb::String("x".to_string()));
        let err = with_ctx(&[TypeId::JSONB], |ctx| jsonb_int4_fn(&[j.clone()], ctx)).err().expect("error");
        assert_eq!(err.sqlstate, SqlState::INVALID_PARAMETER_VALUE);
        assert_eq!(err.message, "cannot cast jsonb string to type integer");
    }

    #[test]
    fn json_object_text_array_pairs() {
        let arr = Array::from_vec(oids::TEXT, vec![text("a"), text("1"), text("b"), text("2")]);
        let pairs = kv_pairs(&arr).expect("pairs");
        assert_eq!(kv_json_text(&pairs).expect("text"), "{\"a\" : \"1\", \"b\" : \"2\"}");
    }
}
