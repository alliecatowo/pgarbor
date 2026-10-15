//! SQL ordering, equality and hashing of Datums (TYPE-04, TYPE-11). Lease TYP-49.
//!
//! These are the semantics of each type's DEFAULT btree / hash opclass in PG18, used by ORDER BY,
//! DISTINCT, GROUP BY, UNION, joins, min/max, array/record comparison, unique checks and indexes
//! (`keycodec` must order bytes exactly like `cmp_datums`). Both operands have type `ty`
//! (cross-type comparisons are resolved to operator procs by the binder, not here).
//!
//! Per type (`ty.oid`, following domains to the base type):
//! - bool: false < true. ints: numeric. oid/reg*/xid/cid: UNSIGNED u32. "char": unsigned byte.
//! - float4/float8: total order with `-0 = 0`, NaN = NaN, NaN greater than everything (PG `float8_cmp_internal`).
//! - numeric: `Numeric::pg_cmp`.
//! - text/varchar/name/unknown: codepoint (= UTF-8 byte) order for every collation in v1 (the pinned
//!   oracle collation is C.UTF-8 / builtin). bpchar: compare with trailing spaces removed.
//! - bytea: memcmp then length. uuid: memcmp of the 16 bytes.
//! - date/time/timestamp(tz): integer order (infinities are MIN/MAX). timetz: by (time + zone*1e6) then
//!   zone (PG `timetz_cmp_internal`). interval: `Interval::cmp_span` (`'1 mon' = '30 days'`).
//! - jsonb: `json::ops::compare_jsonb`. json: no ordering → 42883 `could not identify an ordering
//!   operator for type json` (equality too: `could not identify an equality operator for type json`).
//! - arrays: element-wise via the element type (`array::funcs` docs), NULL elements last.
//! - enums: `enum_type::cmp_labels`. records/composites: field-wise with field types from
//!   `env.composite_fields` (NULL fields last).
//! - unsupported types → 0A000 via `PgError::unimplemented("TYP-49", ...)`.
//! NULL handling: `cmp_datums` orders NULL after every non-NULL value (NULLS LAST) and NULL = NULL;
//! `sql_eq` implements three-valued `=` (NULL operand → None); `not_distinct` implements
//! `IS NOT DISTINCT FROM` (NULLs equal).
//!
//! Hashing (`hash_datum`) must be consistent with equality (`1.0 = 1.00` hash the same; `-0.0` and `0.0`
//! hash the same; all NaNs hash the same; bpchar ignores trailing spaces; intervals hash by span). It
//! does NOT need to equal PG's hash values. PG-compatible hash values (`hashint4(1)` etc., needed by
//! the SQL-callable `hash*` functions and hash partitioning) come from `pg_hash_*` below, which port
//! `src/common/hashfn.c` (`hash_bytes`, `hash_bytes_uint32`, extended 64-bit variants) exactly.
//! Oracle fixtures: `hashint4(1)` = -1905060026, `hashint8(1)` = -1905060026, `hashtext('abc')` =
//! -785388649, `hashint4extended(1, 0)` = -3670598878359251130, `hashfloat8(0.0)` = `hashfloat8(-0.0)` = 0.
use crate::array::Array;
use crate::context::TypeEnv;
use crate::datum::{type_mismatch, Datum, TypeId};
use crate::enum_type;
use crate::json::ops::compare_jsonb;
use crate::json::Jsonb;
use crate::numeric::Numeric;
use crate::oids;
use crate::registry;
use pgarbor_common::{Oid, PgError, Result, SqlState};
use std::cmp::Ordering;
use std::hash::Hasher;

// ---------------------------------------------------------------------------------------------
// Type classification
// ---------------------------------------------------------------------------------------------

/// How values of a SQL type compare/hash (domains already resolved to their base type).
enum Kind {
    Bool,
    /// int2/int4/int8/money: signed integers.
    Int,
    /// xid8, pg_lsn: `Datum::Int8` bits compared as u64.
    UInt64,
    Float4,
    Float8,
    Numeric,
    Text,
    Bpchar,
    Bytea,
    Uuid,
    Date,
    Time,
    TimeTz,
    Timestamp,
    Interval,
    Json,
    Jsonb,
    /// oid, reg*, xid, cid (unsigned 32-bit).
    Oid,
    Char,
    Enum(Oid),
    Array(Box<Kind>),
    Composite(Oid),
    /// Anonymous `record`: field types are inferred from the runtime values.
    Record,
    Unsupported(String),
}

impl Kind {
    fn name(&self) -> &'static str {
        match self {
            Kind::Bool => "Bool",
            Kind::Int => "Int",
            Kind::UInt64 => "Int8",
            Kind::Float4 => "Float4",
            Kind::Float8 => "Float8",
            Kind::Numeric => "Numeric",
            Kind::Text | Kind::Bpchar | Kind::Enum(_) => "Text",
            Kind::Bytea => "Bytea",
            Kind::Uuid => "Uuid",
            Kind::Date => "Date",
            Kind::Time => "Time",
            Kind::TimeTz => "TimeTz",
            Kind::Timestamp => "Timestamp",
            Kind::Interval => "Interval",
            Kind::Json => "Json",
            Kind::Jsonb => "Jsonb",
            Kind::Oid => "Oid",
            Kind::Char => "Char",
            Kind::Array(_) => "Array",
            Kind::Composite(_) | Kind::Record => "Record",
            Kind::Unsupported(_) => "unsupported",
        }
    }
}

fn classify(env: &dyn TypeEnv, oid: Oid) -> Result<Kind> {
    Ok(match oid {
        oids::BOOL => Kind::Bool,
        oids::INT2 | oids::INT4 | oids::INT8 | oids::MONEY => Kind::Int,
        oids::XID8 | oids::PG_LSN => Kind::UInt64,
        oids::FLOAT4 => Kind::Float4,
        oids::FLOAT8 => Kind::Float8,
        oids::NUMERIC => Kind::Numeric,
        oids::TEXT | oids::VARCHAR | oids::NAME | oids::UNKNOWN => Kind::Text,
        oids::BPCHAR => Kind::Bpchar,
        oids::BYTEA => Kind::Bytea,
        oids::UUID => Kind::Uuid,
        oids::DATE => Kind::Date,
        oids::TIME => Kind::Time,
        oids::TIMETZ => Kind::TimeTz,
        oids::TIMESTAMP | oids::TIMESTAMPTZ => Kind::Timestamp,
        oids::INTERVAL => Kind::Interval,
        oids::JSON => Kind::Json,
        oids::JSONB => Kind::Jsonb,
        oids::OID
        | oids::REGPROC
        | oids::REGPROCEDURE
        | oids::REGOPER
        | oids::REGOPERATOR
        | oids::REGCLASS
        | oids::REGTYPE
        | oids::REGROLE
        | oids::REGNAMESPACE
        | oids::REGCOLLATION
        | oids::REGCONFIG
        | oids::REGDICTIONARY
        | oids::XID
        | oids::CID => Kind::Oid,
        oids::CHAR => Kind::Char,
        oids::RECORD => Kind::Record,
        _ => {
            let info = registry::type_info(env, oid)?;
            if info.is_domain() {
                if info.basetype == 0 || info.basetype == oid {
                    return Err(PgError::internal(format!("domain {oid} has no base type")));
                }
                classify(env, info.basetype)?
            } else if info.is_enum() {
                Kind::Enum(oid)
            } else if info.is_array() {
                Kind::Array(Box::new(classify(env, info.elem)?))
            } else if info.is_composite() {
                Kind::Composite(oid)
            } else {
                Kind::Unsupported(info.name.into_owned())
            }
        }
    })
}

/// Kind implied by a runtime value (anonymous record fields).
fn infer_kind(env: &dyn TypeEnv, d: &Datum) -> Result<Kind> {
    Ok(match d {
        Datum::Null => Kind::Text,
        Datum::Bool(_) => Kind::Bool,
        Datum::Int2(_) | Datum::Int4(_) | Datum::Int8(_) => Kind::Int,
        Datum::Float4(_) => Kind::Float4,
        Datum::Float8(_) => Kind::Float8,
        Datum::Numeric(_) => Kind::Numeric,
        Datum::Text(_) => Kind::Text,
        Datum::Bytea(_) => Kind::Bytea,
        Datum::Uuid(_) => Kind::Uuid,
        Datum::Date(_) => Kind::Date,
        Datum::Time(_) => Kind::Time,
        Datum::TimeTz(..) => Kind::TimeTz,
        Datum::Timestamp(_) | Datum::TimestampTz(_) => Kind::Timestamp,
        Datum::Interval(_) => Kind::Interval,
        Datum::Json(_) => Kind::Json,
        Datum::Jsonb(_) => Kind::Jsonb,
        Datum::Array(a) => {
            if a.elem == 0 {
                Kind::Array(Box::new(Kind::Text))
            } else {
                Kind::Array(Box::new(classify(env, a.elem)?))
            }
        }
        Datum::Oid(_) => Kind::Oid,
        Datum::Char(_) => Kind::Char,
        Datum::Record(_) => Kind::Record,
    })
}

fn json_err(eq: bool) -> PgError {
    let what = if eq { "an equality" } else { "an ordering" };
    PgError::new(SqlState::UNDEFINED_FUNCTION, format!("could not identify {what} operator for type json"))
}

fn unsupported_err(name: &str) -> PgError {
    PgError::unimplemented("TYP-49", format!("comparison of type {name}"))
}

// ---------------------------------------------------------------------------------------------
// Comparison
// ---------------------------------------------------------------------------------------------

fn f64_cmp(a: f64, b: f64) -> Ordering {
    match (a.is_nan(), b.is_nan()) {
        (true, true) => Ordering::Equal,
        (true, false) => Ordering::Greater,
        (false, true) => Ordering::Less,
        (false, false) => {
            if a < b {
                Ordering::Less
            } else if a > b {
                Ordering::Greater
            } else {
                Ordering::Equal
            }
        }
    }
}

fn trim_pad(s: &str) -> &str {
    s.trim_end_matches(' ')
}

/// NULL-aware comparison: NULL = NULL, NULL after everything else.
fn cmp_nullable(env: &dyn TypeEnv, kind: &Kind, a: &Datum, b: &Datum, eq: bool) -> Result<Ordering> {
    match (a.is_null(), b.is_null()) {
        (true, true) => Ok(Ordering::Equal),
        (true, false) => Ok(Ordering::Greater),
        (false, true) => Ok(Ordering::Less),
        (false, false) => cmp_kind(env, kind, a, b, eq),
    }
}

/// Compare two non-NULL values of `kind`.
fn cmp_kind(env: &dyn TypeEnv, kind: &Kind, a: &Datum, b: &Datum, eq: bool) -> Result<Ordering> {
    match kind {
        Kind::Json => return Err(json_err(eq)),
        Kind::Unsupported(name) => return Err(unsupported_err(name)),
        _ => {}
    }
    match (a, b) {
        (Datum::Bool(x), Datum::Bool(y)) => Ok(x.cmp(y)),
        (Datum::Int2(x), Datum::Int2(y)) => Ok(x.cmp(y)),
        (Datum::Int4(x), Datum::Int4(y)) => Ok(x.cmp(y)),
        (Datum::Int8(x), Datum::Int8(y)) => {
            if matches!(kind, Kind::UInt64) {
                Ok((*x as u64).cmp(&(*y as u64)))
            } else {
                Ok(x.cmp(y))
            }
        }
        (Datum::Float4(x), Datum::Float4(y)) => Ok(f64_cmp(*x as f64, *y as f64)),
        (Datum::Float8(x), Datum::Float8(y)) => Ok(f64_cmp(*x, *y)),
        (Datum::Numeric(x), Datum::Numeric(y)) => Ok(x.pg_cmp(y)),
        (Datum::Text(x), Datum::Text(y)) => match kind {
            Kind::Enum(oid) => enum_type::cmp_labels(env, *oid, x, y),
            Kind::Bpchar => Ok(trim_pad(x).as_bytes().cmp(trim_pad(y).as_bytes())),
            _ => Ok(x.as_bytes().cmp(y.as_bytes())),
        },
        (Datum::Bytea(x), Datum::Bytea(y)) => Ok(x.as_slice().cmp(y.as_slice())),
        (Datum::Uuid(x), Datum::Uuid(y)) => Ok(x.cmp(y)),
        (Datum::Date(x), Datum::Date(y)) => Ok(x.cmp(y)),
        (Datum::Time(x), Datum::Time(y)) => Ok(x.cmp(y)),
        (Datum::TimeTz(t1, z1), Datum::TimeTz(t2, z2)) => {
            let u1 = *t1 as i128 + *z1 as i128 * 1_000_000;
            let u2 = *t2 as i128 + *z2 as i128 * 1_000_000;
            Ok(u1.cmp(&u2).then_with(|| z1.cmp(z2)))
        }
        (Datum::Timestamp(x), Datum::Timestamp(y)) => Ok(x.cmp(y)),
        (Datum::TimestampTz(x), Datum::TimestampTz(y)) => Ok(x.cmp(y)),
        (Datum::Interval(x), Datum::Interval(y)) => Ok(x.cmp_span().cmp(&y.cmp_span())),
        (Datum::Json(_), Datum::Json(_)) => Err(json_err(eq)),
        (Datum::Jsonb(x), Datum::Jsonb(y)) => compare_jsonb(x, y),
        (Datum::Array(x), Datum::Array(y)) => {
            let elem = match kind {
                Kind::Array(k) => k.as_ref(),
                _ => return Err(type_mismatch(kind.name(), a)),
            };
            array_cmp(env, elem, x, y, eq)
        }
        (Datum::Oid(x), Datum::Oid(y)) => Ok(x.cmp(y)),
        (Datum::Char(x), Datum::Char(y)) => Ok(x.cmp(y)),
        (Datum::Record(x), Datum::Record(y)) => record_cmp(env, kind, x, y, eq),
        _ => Err(type_mismatch(kind.name(), a)),
    }
}

/// PG `array_cmp`: elements first (NULLs last, equal to each other), then item count, ndims, dims, lbounds.
fn array_cmp(env: &dyn TypeEnv, elem: &Kind, x: &Array, y: &Array, eq: bool) -> Result<Ordering> {
    if matches!(elem, Kind::Json) {
        return Err(json_err(eq));
    }
    let n = x.items.len().min(y.items.len());
    for i in 0..n {
        let o = cmp_nullable(env, elem, &x.items[i], &y.items[i], eq)?;
        if o != Ordering::Equal {
            return Ok(o);
        }
    }
    if x.items.len() != y.items.len() {
        return Ok(x.items.len().cmp(&y.items.len()));
    }
    if x.dims.len() != y.dims.len() {
        return Ok(x.dims.len().cmp(&y.dims.len()));
    }
    for (dx, dy) in x.dims.iter().zip(y.dims.iter()) {
        if dx.0 != dy.0 {
            return Ok(dx.0.cmp(&dy.0));
        }
    }
    for (dx, dy) in x.dims.iter().zip(y.dims.iter()) {
        if dx.1 != dy.1 {
            return Ok(dx.1.cmp(&dy.1));
        }
    }
    Ok(Ordering::Equal)
}

/// Live (non-dropped) field type OIDs of composite `oid`, paired with the matching values.
fn composite_pairs<'a>(env: &dyn TypeEnv, oid: Oid, vals: &'a [Datum]) -> Result<Vec<(Oid, &'a Datum)>> {
    let fields = env
        .composite_fields(oid)
        .ok_or_else(|| PgError::internal(format!("cache lookup failed for composite type {oid}")))?;
    let live = fields.iter().filter(|f| !f.dropped).count();
    let mut out = Vec::with_capacity(live);
    if vals.len() == fields.len() {
        for (f, v) in fields.iter().zip(vals.iter()) {
            if !f.dropped {
                out.push((f.type_id.oid, v));
            }
        }
    } else if vals.len() == live {
        for (f, v) in fields.iter().filter(|f| !f.dropped).zip(vals.iter()) {
            out.push((f.type_id.oid, v));
        }
    } else {
        return Err(PgError::internal(format!(
            "record value has {} fields but composite type {oid} has {} columns",
            vals.len(),
            live
        )));
    }
    Ok(out)
}

fn record_cmp(env: &dyn TypeEnv, kind: &Kind, x: &[Datum], y: &[Datum], eq: bool) -> Result<Ordering> {
    if let Kind::Composite(oid) = kind {
        let px = composite_pairs(env, *oid, x)?;
        let py = composite_pairs(env, *oid, y)?;
        for ((toid, a), (_, b)) in px.iter().zip(py.iter()) {
            if a.is_null() && b.is_null() {
                continue;
            }
            let k = classify(env, *toid)?;
            let o = cmp_nullable(env, &k, a, b, eq)?;
            if o != Ordering::Equal {
                return Ok(o);
            }
        }
        return Ok(Ordering::Equal);
    }
    if x.len() != y.len() {
        return Err(PgError::new(
            SqlState::DATATYPE_MISMATCH,
            "cannot compare record types with different numbers of columns",
        ));
    }
    for (a, b) in x.iter().zip(y.iter()) {
        if a.is_null() && b.is_null() {
            continue;
        }
        let k = infer_kind(env, if a.is_null() { b } else { a })?;
        let o = cmp_nullable(env, &k, a, b, eq)?;
        if o != Ordering::Equal {
            return Ok(o);
        }
    }
    Ok(Ordering::Equal)
}

/// Total order of two values of type `ty` (NULLS LAST, NULL = NULL).
// CONTRACT
pub fn cmp_datums(env: &dyn TypeEnv, ty: TypeId, a: &Datum, b: &Datum) -> Result<Ordering> {
    match (a.is_null(), b.is_null()) {
        (true, true) => return Ok(Ordering::Equal),
        (true, false) => return Ok(Ordering::Greater),
        (false, true) => return Ok(Ordering::Less),
        (false, false) => {}
    }
    let kind = classify(env, ty.oid)?;
    cmp_kind(env, &kind, a, b, false)
}

/// Ordering for ORDER BY with explicit direction and NULLS placement.
/// `desc` reverses the non-NULL order; `nulls_first` places NULLs first (independently of `desc`).
// CONTRACT
pub fn cmp_sort(env: &dyn TypeEnv, ty: TypeId, a: &Datum, b: &Datum, desc: bool, nulls_first: bool) -> Result<Ordering> {
    match (a.is_null(), b.is_null()) {
        (true, true) => Ok(Ordering::Equal),
        (true, false) => Ok(if nulls_first { Ordering::Less } else { Ordering::Greater }),
        (false, true) => Ok(if nulls_first { Ordering::Greater } else { Ordering::Less }),
        (false, false) => {
            let o = cmp_datums(env, ty, a, b)?;
            Ok(if desc { o.reverse() } else { o })
        }
    }
}

/// SQL `=` with three-valued logic: `None` when either side is NULL.
// CONTRACT
pub fn sql_eq(env: &dyn TypeEnv, ty: TypeId, a: &Datum, b: &Datum) -> Result<Option<bool>> {
    if a.is_null() || b.is_null() {
        return Ok(None);
    }
    let kind = classify(env, ty.oid)?;
    Ok(Some(cmp_kind(env, &kind, a, b, true)? == Ordering::Equal))
}

/// `a IS NOT DISTINCT FROM b` (NULLs compare equal; never NULL).
// CONTRACT
pub fn not_distinct(env: &dyn TypeEnv, ty: TypeId, a: &Datum, b: &Datum) -> Result<bool> {
    match (a.is_null(), b.is_null()) {
        (true, true) => return Ok(true),
        (true, false) | (false, true) => return Ok(false),
        (false, false) => {}
    }
    let kind = classify(env, ty.oid)?;
    Ok(cmp_kind(env, &kind, a, b, true)? == Ordering::Equal)
}

// ---------------------------------------------------------------------------------------------
// Hashing (consistent with equality, not PG-valued)
// ---------------------------------------------------------------------------------------------

fn hash_f64(v: f64, state: &mut dyn Hasher) {
    let bits = if v.is_nan() {
        0x7ff8_0000_0000_0000u64
    } else if v == 0.0 {
        0u64
    } else {
        v.to_bits()
    };
    state.write_u64(bits);
}

fn hash_numeric(n: &Numeric, state: &mut dyn Hasher) {
    match n {
        Numeric::NaN => state.write_u8(1),
        Numeric::PosInf => state.write_u8(2),
        Numeric::NegInf => state.write_u8(3),
        Numeric::Finite(d) => {
            state.write_u8(0);
            if bigdecimal::Zero::is_zero(d) {
                state.write_u8(0);
            } else {
                let norm = d.normalized();
                let (bi, exp) = norm.as_bigint_and_exponent();
                state.write_u8(1);
                state.write(&bi.to_signed_bytes_le());
                state.write_i64(exp);
            }
        }
    }
}

fn hash_str(s: &str, state: &mut dyn Hasher) {
    state.write(s.as_bytes());
    state.write_u8(0xff);
}

fn hash_jsonb(j: &Jsonb, state: &mut dyn Hasher) {
    match j {
        Jsonb::Null => state.write_u8(0),
        Jsonb::Bool(b) => {
            state.write_u8(1);
            state.write_u8(*b as u8);
        }
        Jsonb::Number(n) => {
            state.write_u8(2);
            hash_numeric(n, state);
        }
        Jsonb::String(s) => {
            state.write_u8(3);
            hash_str(s, state);
        }
        Jsonb::Array(v) => {
            state.write_u8(4);
            state.write_usize(v.len());
            for e in v {
                hash_jsonb(e, state);
            }
        }
        Jsonb::Object(kv) => {
            state.write_u8(5);
            state.write_usize(kv.len());
            for (k, v) in kv {
                hash_str(k, state);
                hash_jsonb(v, state);
            }
        }
    }
}

fn hash_nullable(env: &dyn TypeEnv, kind: &Kind, d: &Datum, state: &mut dyn Hasher) -> Result<()> {
    if d.is_null() {
        state.write_u8(0);
        return Ok(());
    }
    state.write_u8(1);
    hash_kind(env, kind, d, state)
}

fn hash_kind(env: &dyn TypeEnv, kind: &Kind, d: &Datum, state: &mut dyn Hasher) -> Result<()> {
    match kind {
        Kind::Json => return Err(json_err(true)),
        Kind::Unsupported(name) => return Err(unsupported_err(name)),
        _ => {}
    }
    match d {
        Datum::Null => state.write_u8(0),
        Datum::Bool(v) => state.write_u8(*v as u8),
        Datum::Int2(v) => state.write_i64(*v as i64),
        Datum::Int4(v) => state.write_i64(*v as i64),
        Datum::Int8(v) => state.write_i64(*v),
        Datum::Float4(v) => hash_f64(*v as f64, state),
        Datum::Float8(v) => hash_f64(*v, state),
        Datum::Numeric(n) => hash_numeric(n, state),
        Datum::Text(s) => match kind {
            Kind::Bpchar => hash_str(trim_pad(s), state),
            _ => hash_str(s, state),
        },
        Datum::Bytea(b) => {
            state.write(b);
            state.write_u8(0xff);
        }
        Datum::Uuid(u) => state.write(u),
        Datum::Date(v) => state.write_i32(*v),
        Datum::Time(v) => state.write_i64(*v),
        Datum::TimeTz(t, z) => {
            state.write_i64(*t);
            state.write_i32(*z);
        }
        Datum::Timestamp(v) | Datum::TimestampTz(v) => state.write_i64(*v),
        Datum::Interval(i) => state.write_i128(i.cmp_span()),
        Datum::Json(_) => return Err(json_err(true)),
        Datum::Jsonb(j) => hash_jsonb(j, state),
        Datum::Array(a) => {
            let elem = match kind {
                Kind::Array(k) => k.as_ref(),
                _ => return Err(type_mismatch(kind.name(), d)),
            };
            if matches!(elem, Kind::Json) {
                return Err(json_err(true));
            }
            state.write_usize(a.items.len());
            state.write_usize(a.dims.len());
            for (len, lb) in &a.dims {
                state.write_i32(*len);
                state.write_i32(*lb);
            }
            for it in &a.items {
                hash_nullable(env, elem, it, state)?;
            }
        }
        Datum::Oid(v) => state.write_u32(*v),
        Datum::Char(v) => state.write_u8(*v),
        Datum::Record(vals) => match kind {
            Kind::Composite(oid) => {
                let pairs = composite_pairs(env, *oid, vals)?;
                state.write_usize(pairs.len());
                for (toid, v) in pairs {
                    let k = classify(env, toid)?;
                    hash_nullable(env, &k, v, state)?;
                }
            }
            Kind::Record => {
                state.write_usize(vals.len());
                for v in vals {
                    if v.is_null() {
                        state.write_u8(0);
                    } else {
                        let k = infer_kind(env, v)?;
                        hash_nullable(env, &k, v, state)?;
                    }
                }
            }
            _ => return Err(type_mismatch(kind.name(), d)),
        },
    }
    Ok(())
}

/// Feed a hash of `d` (type `ty`) consistent with `not_distinct` equality into `state`.
// CONTRACT
pub fn hash_datum(env: &dyn TypeEnv, ty: TypeId, d: &Datum, state: &mut dyn Hasher) -> Result<()> {
    if d.is_null() {
        state.write_u8(0);
        return Ok(());
    }
    let kind = classify(env, ty.oid)?;
    state.write_u8(1);
    hash_kind(env, &kind, d, state)
}

fn kind_orderable(env: &dyn TypeEnv, kind: &Kind) -> Result<bool> {
    Ok(match kind {
        Kind::Json | Kind::Unsupported(_) => false,
        Kind::Array(k) => kind_orderable(env, k)?,
        Kind::Composite(oid) => {
            let fields = env
                .composite_fields(*oid)
                .ok_or_else(|| PgError::internal(format!("cache lookup failed for composite type {oid}")))?;
            for f in fields.iter().filter(|f| !f.dropped) {
                let k = classify(env, f.type_id.oid)?;
                if !kind_orderable(env, &k)? {
                    return Ok(false);
                }
            }
            true
        }
        _ => true,
    })
}

/// True if values of `ty` can be ordered (default btree opclass exists: false for json, xml, point...).
// CONTRACT
pub fn is_orderable(env: &dyn TypeEnv, ty: TypeId) -> Result<bool> {
    let kind = classify(env, ty.oid)?;
    kind_orderable(env, &kind)
}

// ---------------------------------------------------------------------------------------------
// PostgreSQL hash functions (src/common/hashfn.c, Bob Jenkins lookup3)
// ---------------------------------------------------------------------------------------------

#[inline]
fn jmix(mut a: u32, mut b: u32, mut c: u32) -> (u32, u32, u32) {
    a = a.wrapping_sub(c);
    a ^= c.rotate_left(4);
    c = c.wrapping_add(b);
    b = b.wrapping_sub(a);
    b ^= a.rotate_left(6);
    a = a.wrapping_add(c);
    c = c.wrapping_sub(b);
    c ^= b.rotate_left(8);
    b = b.wrapping_add(a);
    a = a.wrapping_sub(c);
    a ^= c.rotate_left(16);
    c = c.wrapping_add(b);
    b = b.wrapping_sub(a);
    b ^= a.rotate_left(19);
    a = a.wrapping_add(c);
    c = c.wrapping_sub(b);
    c ^= b.rotate_left(4);
    b = b.wrapping_add(a);
    (a, b, c)
}

#[inline]
fn jfinal(mut a: u32, mut b: u32, mut c: u32) -> (u32, u32, u32) {
    c ^= b;
    c = c.wrapping_sub(b.rotate_left(14));
    a ^= c;
    a = a.wrapping_sub(c.rotate_left(11));
    b ^= a;
    b = b.wrapping_sub(a.rotate_left(25));
    c ^= b;
    c = c.wrapping_sub(b.rotate_left(16));
    a ^= c;
    a = a.wrapping_sub(c.rotate_left(4));
    b ^= a;
    b = b.wrapping_sub(a.rotate_left(14));
    c ^= b;
    c = c.wrapping_sub(b.rotate_left(24));
    (a, b, c)
}

/// Shared body of `hash_bytes` / `hash_bytes_extended`; returns the final (b, c). Seed 0 = no seed.
fn hash_bytes_core(k: &[u8], seed: u64) -> (u32, u32) {
    let len = k.len() as u32;
    let init = 0x9e37_79b9u32.wrapping_add(len).wrapping_add(3_923_095);
    let (mut a, mut b, mut c) = (init, init, init);
    if seed != 0 {
        a = a.wrapping_add((seed >> 32) as u32);
        b = b.wrapping_add(seed as u32);
        let m = jmix(a, b, c);
        a = m.0;
        b = m.1;
        c = m.2;
    }
    let mut chunks = k.chunks_exact(12);
    for ch in &mut chunks {
        a = a.wrapping_add(u32::from_le_bytes([ch[0], ch[1], ch[2], ch[3]]));
        b = b.wrapping_add(u32::from_le_bytes([ch[4], ch[5], ch[6], ch[7]]));
        c = c.wrapping_add(u32::from_le_bytes([ch[8], ch[9], ch[10], ch[11]]));
        let m = jmix(a, b, c);
        a = m.0;
        b = m.1;
        c = m.2;
    }
    // Tail: the lowest byte of c is reserved for the length.
    for (i, &byte) in chunks.remainder().iter().enumerate() {
        let v = byte as u32;
        if i < 4 {
            a = a.wrapping_add(v << (8 * i));
        } else if i < 8 {
            b = b.wrapping_add(v << (8 * (i - 4)));
        } else {
            c = c.wrapping_add(v << (8 * (i - 8) + 8));
        }
    }
    let f = jfinal(a, b, c);
    (f.1, f.2)
}

fn hash_u32_core(v: u32, seed: u64) -> (u32, u32) {
    let init = 0x9e37_79b9u32.wrapping_add(4).wrapping_add(3_923_095);
    let (mut a, mut b, mut c) = (init, init, init);
    if seed != 0 {
        a = a.wrapping_add((seed >> 32) as u32);
        b = b.wrapping_add(seed as u32);
        let m = jmix(a, b, c);
        a = m.0;
        b = m.1;
        c = m.2;
    }
    a = a.wrapping_add(v);
    let f = jfinal(a, b, c);
    (f.1, f.2)
}

/// PG `hash_bytes_uint32(v)` (Bob Jenkins lookup3 as in `src/common/hashfn.c`) as i32 bits.
// CONTRACT
pub fn pg_hash_uint32(v: u32) -> Result<u32> {
    Ok(hash_u32_core(v, 0).1)
}

/// PG `hash_bytes(k, len)`.
// CONTRACT
pub fn pg_hash_bytes(k: &[u8]) -> Result<u32> {
    Ok(hash_bytes_core(k, 0).1)
}

/// PG `hash_bytes_uint32_extended(v, seed)`.
// CONTRACT
pub fn pg_hash_uint32_extended(v: u32, seed: u64) -> Result<u64> {
    let (b, c) = hash_u32_core(v, seed);
    Ok(((b as u64) << 32) | c as u64)
}

/// PG `hash_bytes_extended(k, len, seed)`.
// CONTRACT
pub fn pg_hash_bytes_extended(k: &[u8], seed: u64) -> Result<u64> {
    let (b, c) = hash_bytes_core(k, seed);
    Ok(((b as u64) << 32) | c as u64)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::datum::Interval;
    use crate::numeric::bigdecimal::BigDecimal;
    use std::collections::hash_map::DefaultHasher;
    use std::str::FromStr;

    fn env() -> &'static dyn TypeEnv {
        registry::builtin_env()
    }
    fn num(s: &str) -> Datum {
        Datum::Numeric(Numeric::from_decimal(BigDecimal::from_str(s).unwrap()))
    }
    fn h(ty: TypeId, d: &Datum) -> u64 {
        let mut s = DefaultHasher::new();
        hash_datum(env(), ty, d, &mut s).unwrap();
        s.finish()
    }

    #[test]
    fn float_order() {
        let t = TypeId::FLOAT8;
        let nan = Datum::Float8(f64::NAN);
        let inf = Datum::Float8(f64::INFINITY);
        assert_eq!(cmp_datums(env(), t, &nan, &inf).unwrap(), Ordering::Greater);
        assert_eq!(cmp_datums(env(), t, &nan, &nan).unwrap(), Ordering::Equal);
        assert_eq!(cmp_datums(env(), t, &Datum::Float8(-0.0), &Datum::Float8(0.0)).unwrap(), Ordering::Equal);
        assert_eq!(h(t, &Datum::Float8(-0.0)), h(t, &Datum::Float8(0.0)));
        assert_eq!(h(t, &nan), h(t, &Datum::Float8(-f64::NAN)));
    }

    #[test]
    fn numeric_eq_and_hash() {
        let t = TypeId::NUMERIC;
        assert_eq!(sql_eq(env(), t, &num("1.0"), &num("1.00")).unwrap(), Some(true));
        assert_eq!(h(t, &num("1.0")), h(t, &num("1.00")));
        assert_eq!(h(t, &num("0.0")), h(t, &num("0")));
        assert_ne!(h(t, &num("1.0")), h(t, &num("1.1")));
    }

    #[test]
    fn bpchar_and_interval() {
        let b = TypeId::BPCHAR;
        assert!(not_distinct(env(), b, &Datum::text("a "), &Datum::text("a")).unwrap());
        assert_eq!(h(b, &Datum::text("a ")), h(b, &Datum::text("a")));
        let t = TypeId::TEXT;
        assert!(!not_distinct(env(), t, &Datum::text("a "), &Datum::text("a")).unwrap());
        let i = TypeId::INTERVAL;
        let m = Datum::Interval(Interval::new(1, 0, 0));
        let d = Datum::Interval(Interval::new(0, 30, 0));
        assert!(not_distinct(env(), i, &m, &d).unwrap());
        assert_eq!(h(i, &m), h(i, &d));
    }

    #[test]
    fn oid_unsigned_and_nulls() {
        assert_eq!(
            cmp_datums(env(), TypeId::OID, &Datum::Oid(4_294_967_295), &Datum::Oid(1)).unwrap(),
            Ordering::Greater
        );
        let t = TypeId::INT4;
        let n = Datum::Null;
        let one = Datum::Int4(1);
        assert_eq!(cmp_datums(env(), t, &n, &one).unwrap(), Ordering::Greater);
        assert_eq!(cmp_datums(env(), t, &n, &n).unwrap(), Ordering::Equal);
        assert_eq!(sql_eq(env(), t, &n, &one).unwrap(), None);
        assert!(not_distinct(env(), t, &n, &n).unwrap());
        assert_eq!(cmp_sort(env(), t, &n, &one, false, true).unwrap(), Ordering::Less);
        assert_eq!(cmp_sort(env(), t, &n, &one, true, true).unwrap(), Ordering::Less);
        assert_eq!(cmp_sort(env(), t, &n, &one, true, false).unwrap(), Ordering::Greater);
        assert_eq!(cmp_sort(env(), t, &Datum::Int4(2), &one, true, false).unwrap(), Ordering::Less);
    }

    #[test]
    fn json_has_no_ordering() {
        let j = Datum::Json("1".into());
        let e = cmp_datums(env(), TypeId::JSON, &j, &j).unwrap_err();
        assert_eq!(e.sqlstate.code(), "42883");
        assert_eq!(e.message, "could not identify an ordering operator for type json");
        let e = sql_eq(env(), TypeId::JSON, &j, &j).unwrap_err();
        assert_eq!(e.message, "could not identify an equality operator for type json");
        assert!(!is_orderable(env(), TypeId::JSON).unwrap());
        assert!(is_orderable(env(), TypeId::INT4).unwrap());
        assert!(is_orderable(env(), TypeId::TEXT_ARRAY).unwrap());
    }

    #[test]
    fn arrays() {
        let t = TypeId::INT4_ARRAY;
        let a = |v: Vec<Datum>| Datum::array(Array::from_vec(23, v));
        let i = |v: i32| Datum::Int4(v);
        assert_eq!(cmp_datums(env(), t, &a(vec![i(1), i(2)]), &a(vec![i(1), i(2), i(0)])).unwrap(), Ordering::Less);
        assert_eq!(
            cmp_datums(env(), t, &a(vec![i(1), Datum::Null]), &a(vec![i(1), i(2)])).unwrap(),
            Ordering::Greater
        );
        assert!(not_distinct(env(), t, &a(vec![i(1), Datum::Null]), &a(vec![i(1), Datum::Null])).unwrap());
        assert_eq!(h(t, &a(vec![i(1), i(2)])), h(t, &a(vec![i(1), i(2)])));
    }

    #[test]
    fn anonymous_records() {
        let t = TypeId::RECORD;
        let r = |v: Vec<Datum>| Datum::Record(v);
        assert_eq!(
            cmp_datums(
                env(),
                t,
                &r(vec![Datum::Int4(1), Datum::Null]),
                &r(vec![Datum::Int4(1), Datum::Text("x".into())])
            )
            .unwrap(),
            Ordering::Greater
        );
        assert!(not_distinct(env(), t, &r(vec![Datum::Int4(1)]), &r(vec![Datum::Int4(1)])).unwrap());
    }

    #[test]
    fn timetz_and_u64() {
        let t = TypeId::TIMETZ;
        // same instant: ordered by zone
        let a = Datum::TimeTz(0, -3600);
        let b = Datum::TimeTz(-3_600_000_000, 0);
        assert_eq!(cmp_datums(env(), t, &a, &b).unwrap(), Ordering::Less);
        let x = TypeId::of(oids::XID8);
        assert_eq!(cmp_datums(env(), x, &Datum::Int8(-1), &Datum::Int8(1)).unwrap(), Ordering::Greater);
    }

    #[test]
    #[ignore = "needs TYP-34"]
    fn jsonb_order() {
        let a = Datum::Jsonb(Jsonb::Null);
        let b = Datum::Jsonb(Jsonb::Bool(false));
        assert_eq!(cmp_datums(env(), TypeId::JSONB, &a, &b).unwrap(), Ordering::Less);
    }

    #[test]
    #[ignore = "needs TYP-41"]
    fn enum_order_needs_labels() {
        // BuiltinTypeEnv has no user enums; exercised centrally with a catalog-backed env.
        assert!(is_orderable(env(), TypeId::BOOL).unwrap());
    }

    #[test]
    fn pg_hash_fixtures() {
        assert_eq!(pg_hash_uint32(1).unwrap() as i32, -1905060026);
        assert_eq!(pg_hash_bytes(b"abc").unwrap() as i32, -785388649);
        assert_eq!(pg_hash_uint32_extended(1, 0).unwrap() as i64, -3670598878359251130);
        let _ = pg_hash_bytes(b"hello world, this is longer than twelve").unwrap();
        assert_ne!(pg_hash_bytes_extended(b"abc", 7).unwrap(), pg_hash_bytes_extended(b"abc", 0).unwrap());
        assert_eq!(pg_hash_bytes_extended(b"abc", 0).unwrap() as u32, pg_hash_bytes(b"abc").unwrap());
    }
}
