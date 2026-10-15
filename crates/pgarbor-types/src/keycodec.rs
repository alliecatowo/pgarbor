//! Memcomparable index-key encoding (ARCHITECTURE §4/§8, TYPE-11). Lease TYP-50 (sonnet).
//!
//! `encode_key` turns a tuple of typed values into bytes whose bytewise (memcmp) order equals the SQL
//! order `compare::cmp_sort` defines for each column's direction and NULLS placement. Index entries in
//! the KV store are `[index relid u32 BE][encoded key][rowid u64 BE if non-unique]` (ARCHITECTURE §4).
//! The encoding is NOT required to be decodable: values that compare equal in SQL MUST encode
//! identically (`1.0`/`1.00`, `'1 mon'`/`'30 days'`, bpchar `'a '`/`'a'`, `-0.0`/`0.0`) — the row
//! itself is fetched by rowid. ON-DISK FORMAT: frozen at v1; changing it requires an index rebuild and
//! an amendment.
//!
//! Byte format, per column (tags are never inverted):
//! - NULL: one byte `0x00` if `nulls_first` else `0xFF`.
//! - non-NULL: one byte `0x80`, then BODY; if `desc`, every byte of BODY is bitwise inverted (`!b`).
//!   All BODY encodings are prefix-free, so inversion keeps the order correct.
//! BODY by type (after following domains to the base type):
//! - bool: `0x00` / `0x01`.
//! - int2/int4/int8: big-endian two's complement with the sign bit flipped (2/4/8 bytes).
//! - oid & OID-like (`Datum::Oid`): u32 big-endian. "char": the byte. xid8/pg_lsn (`Int8` as u64): u64 BE.
//! - float4/float8: map -0.0 → 0.0 and every NaN → canonical NaN (`f64::NAN` bits `0x7ff8000000000000`,
//!   float4 `0x7fc00000`); bits = to_bits(); if sign bit set → `!bits` else `bits ^ SIGN`; big-endian.
//!   (Gives -inf < finite < +inf < NaN.)
//! - numeric: one class byte: NegInf `0x01`, negative `0x02`, zero `0x03`, positive `0x04`, PosInf `0x05`,
//!   NaN `0x06`. For nonzero finite values strip trailing fractional zeros, write the value as
//!   `0.d1d2d3... × 10^E` with d1 != 0, then: positive → `E` as i32 BE with sign bit flipped, followed by
//!   digit pairs `(10*d_i + d_{i+1}) + 2` (one byte each; odd count padded with 0 as the second digit),
//!   followed by terminator `0x01`; negative → `-E` encoded the same way (i32 BE, sign flipped) and
//!   every digit byte replaced by `0xFF - byte`, terminator `0xFE`.
//! - text/varchar/name/unknown and bytea: the bytes (UTF-8 for text) with every
//!   `0x00` written as `0x00 0xFF`, then terminator `0x00 0x01`. bpchar: trailing spaces stripped first.
//! - uuid: the 16 bytes.
//! - date: i32 BE sign-flipped. time, timestamp, timestamptz: i64 BE sign-flipped.
//! - timetz: i64 BE sign-flipped of `time + zone*1_000_000`, then i32 BE sign-flipped of `zone`.
//! - interval: `Interval::cmp_span()` as i128 BE sign-flipped (16 bytes).
//! - enum: `enum_type::label_sort_order` encoded as float4 above.
//! - jsonb: type rank byte (ranks follow `json::ops::compare_jsonb`: empty top-level array 0x00,
//!   Null 0x10, String 0x20, Number 0x30, Bool 0x40, Array 0x50, Object 0x60), then for scalars the
//!   scalar body (string as text, number as numeric, bool as bool); arrays: element count u32 BE then
//!   elements recursively (with their rank bytes); objects: pair count u32 BE then for each pair (storage
//!   order) key as text BODY and value recursively. (Matches compare_jsonb ordering.)
//! - arrays: for each element in row-major order: `0x02` then (NULL element → `0xFF`; else `0x80` + element
//!   BODY ascending), then end marker `0x01`, then ndims (u8), then per dim length and lower bound as
//!   i32 BE sign-flipped. (Matches btarraycmp: elements first, then count, then dims.)
//! - record/composite: each field encoded as a full column (tag + body, NULLS LAST, ascending) using
//!   `env.composite_fields` types.
//! - json, xml, point, other unorderable types → 42883 `could not identify an ordering operator for type json`.
//! - other unsupported types → 0A000 (`PgError::unimplemented("TYP-50", ...)`).
use crate::array::Array;
use crate::context::TypeEnv;
use crate::datum::{type_mismatch, Datum, TypeId};
use crate::enum_type;
use crate::json::Jsonb;
use crate::numeric::Numeric;
use crate::{oids, registry};
use pgarbor_common::{Oid, PgError, Result, SqlState};

/// Encoding of one key column.
// CONTRACT
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct KeyColumn {
    pub ty: TypeId,
    pub desc: bool,
    pub nulls_first: bool,
}

impl KeyColumn {
    /// `ASC NULLS LAST` (PG default for ASC).
    pub fn asc(ty: TypeId) -> KeyColumn {
        KeyColumn { ty, desc: false, nulls_first: false }
    }
    /// `DESC NULLS FIRST` (PG default for DESC).
    pub fn desc(ty: TypeId) -> KeyColumn {
        KeyColumn { ty, desc: true, nulls_first: true }
    }
}

/// Append the encoding of `values` (parallel to `cols`) to `out`.
/// `values.len()` may be SHORTER than `cols` (prefix keys for range scans).
// CONTRACT
pub fn encode_key(env: &dyn TypeEnv, cols: &[KeyColumn], values: &[Datum], out: &mut Vec<u8>) -> Result<()> {
    if values.len() > cols.len() {
        return Err(PgError::internal(format!(
            "index key has {} values but only {} key columns",
            values.len(),
            cols.len()
        )));
    }
    for (col, value) in cols.iter().zip(values.iter()) {
        encode_value(env, col, value, out)?;
    }
    Ok(())
}

/// Convenience: encode into a new vector.
pub fn encode_key_vec(env: &dyn TypeEnv, cols: &[KeyColumn], values: &[Datum]) -> Result<Vec<u8>> {
    let mut out = Vec::new();
    encode_key(env, cols, values, &mut out)?;
    Ok(out)
}

/// Append the encoding of one column value (tag + body) to `out`.
// CONTRACT
pub fn encode_value(env: &dyn TypeEnv, col: &KeyColumn, value: &Datum, out: &mut Vec<u8>) -> Result<()> {
    if value.is_null() {
        out.push(if col.nulls_first { TAG_NULL_FIRST } else { TAG_NULL_LAST });
        return Ok(());
    }
    let tag_pos = out.len();
    out.push(TAG_VALUE);
    if let Err(e) = encode_body(env, col.ty.oid, value, out) {
        out.truncate(tag_pos);
        return Err(e);
    }
    if col.desc {
        for b in &mut out[tag_pos + 1..] {
            *b = !*b;
        }
    }
    Ok(())
}

const TAG_NULL_FIRST: u8 = 0x00;
const TAG_NULL_LAST: u8 = 0xFF;
const TAG_VALUE: u8 = 0x80;
const SIGN32: u32 = 0x8000_0000;
const SIGN64: u64 = 0x8000_0000_0000_0000;
const SIGN128: u128 = 1u128 << 127;

#[inline]
fn put_i16(out: &mut Vec<u8>, v: i16) {
    out.extend_from_slice(&((v as u16) ^ 0x8000).to_be_bytes());
}
#[inline]
fn put_i32(out: &mut Vec<u8>, v: i32) {
    out.extend_from_slice(&((v as u32) ^ SIGN32).to_be_bytes());
}
#[inline]
fn put_i64(out: &mut Vec<u8>, v: i64) {
    out.extend_from_slice(&((v as u64) ^ SIGN64).to_be_bytes());
}

fn put_f64(out: &mut Vec<u8>, v: f64) {
    let v = if v.is_nan() {
        f64::from_bits(0x7ff8_0000_0000_0000)
    } else if v == 0.0 {
        0.0
    } else {
        v
    };
    let bits = v.to_bits();
    let key = if bits & SIGN64 != 0 { !bits } else { bits ^ SIGN64 };
    out.extend_from_slice(&key.to_be_bytes());
}

fn put_f32(out: &mut Vec<u8>, v: f32) {
    let v = if v.is_nan() {
        f32::from_bits(0x7fc0_0000)
    } else if v == 0.0 {
        0.0
    } else {
        v
    };
    let bits = v.to_bits();
    let key = if bits & SIGN32 != 0 { !bits } else { bits ^ SIGN32 };
    out.extend_from_slice(&key.to_be_bytes());
}

/// Escaped, terminated byte string (text, bytea).
fn put_bytes(out: &mut Vec<u8>, bytes: &[u8]) {
    out.reserve(bytes.len() + 2);
    for &b in bytes {
        if b == 0 {
            out.extend_from_slice(&[0x00, 0xFF]);
        } else {
            out.push(b);
        }
    }
    out.extend_from_slice(&[0x00, 0x01]);
}

fn put_numeric(out: &mut Vec<u8>, n: &Numeric) {
    let d = match n {
        Numeric::NegInf => {
            out.push(0x01);
            return;
        }
        Numeric::PosInf => {
            out.push(0x05);
            return;
        }
        Numeric::NaN => {
            out.push(0x06);
            return;
        }
        Numeric::Finite(d) => d,
    };
    let (int, scale) = d.as_bigint_and_exponent();
    let text = int.to_string();
    let neg = text.starts_with('-');
    let digits = text.trim_start_matches('-').as_bytes();
    if digits == b"0" {
        out.push(0x03);
        return;
    }
    // value = D * 10^-scale = 0.d1..dL * 10^(L - scale); trailing zero digits do not change 0.d1..dL.
    let exp = digits.len() as i64 - scale;
    let mut n_sig = digits.len();
    while n_sig > 1 && digits[n_sig - 1] == b'0' {
        n_sig -= 1;
    }
    let exp = exp.clamp(i32::MIN as i64 + 1, i32::MAX as i64) as i32;
    if neg {
        out.push(0x02);
        put_i32(out, -exp);
    } else {
        out.push(0x04);
        put_i32(out, exp);
    }
    for pair in digits[..n_sig].chunks(2) {
        let hi = pair[0] - b'0';
        let lo = if pair.len() == 2 { pair[1] - b'0' } else { 0 };
        let byte = hi * 10 + lo + 2;
        out.push(if neg { 0xFF - byte } else { byte });
    }
    out.push(if neg { 0xFE } else { 0x01 });
}

fn unorderable(env: &dyn TypeEnv, oid: Oid) -> PgError {
    let name = env.type_info(oid).map(|t| t.name.into_owned()).unwrap_or_else(|| oid.to_string());
    PgError::new(
        SqlState::UNDEFINED_FUNCTION,
        format!("could not identify an ordering operator for type {name}"),
    )
}

/// Encode the BODY of a non-NULL value of type `oid` (domains are followed here).
fn encode_body(env: &dyn TypeEnv, oid: Oid, d: &Datum, out: &mut Vec<u8>) -> Result<()> {
    let base = registry::base_type(env, oid);
    match base {
        oids::BOOL => match d {
            Datum::Bool(v) => out.push(*v as u8),
            _ => return Err(type_mismatch("bool", d)),
        },
        oids::INT2 => match d {
            Datum::Int2(v) => put_i16(out, *v),
            _ => return Err(type_mismatch("int2", d)),
        },
        oids::INT4 => match d {
            Datum::Int4(v) => put_i32(out, *v),
            _ => return Err(type_mismatch("int4", d)),
        },
        oids::INT8 => match d {
            Datum::Int8(v) => put_i64(out, *v),
            _ => return Err(type_mismatch("int8", d)),
        },
        oids::XID8 | oids::PG_LSN => match d {
            Datum::Int8(v) => out.extend_from_slice(&(*v as u64).to_be_bytes()),
            _ => return Err(type_mismatch("int8", d)),
        },
        oids::CHAR => match d {
            Datum::Char(v) => out.push(*v),
            _ => return Err(type_mismatch("char", d)),
        },
        oids::FLOAT4 => match d {
            Datum::Float4(v) => put_f32(out, *v),
            _ => return Err(type_mismatch("float4", d)),
        },
        oids::FLOAT8 => match d {
            Datum::Float8(v) => put_f64(out, *v),
            _ => return Err(type_mismatch("float8", d)),
        },
        oids::NUMERIC => match d {
            Datum::Numeric(n) => put_numeric(out, n),
            _ => return Err(type_mismatch("numeric", d)),
        },
        oids::TEXT | oids::VARCHAR | oids::NAME | oids::UNKNOWN | oids::CSTRING | oids::PG_NODE_TREE
        | oids::ACLITEM => match d {
            Datum::Text(s) => put_bytes(out, s.as_bytes()),
            _ => return Err(type_mismatch("text", d)),
        },
        oids::BPCHAR => match d {
            Datum::Text(s) => put_bytes(out, s.trim_end_matches(' ').as_bytes()),
            _ => return Err(type_mismatch("bpchar", d)),
        },
        oids::BYTEA => match d {
            Datum::Bytea(b) => put_bytes(out, b),
            _ => return Err(type_mismatch("bytea", d)),
        },
        oids::UUID => match d {
            Datum::Uuid(u) => out.extend_from_slice(u),
            _ => return Err(type_mismatch("uuid", d)),
        },
        oids::DATE => match d {
            Datum::Date(v) => put_i32(out, *v),
            _ => return Err(type_mismatch("date", d)),
        },
        oids::TIME => match d {
            Datum::Time(v) => put_i64(out, *v),
            _ => return Err(type_mismatch("time", d)),
        },
        oids::TIMETZ => match d {
            Datum::TimeTz(t, zone) => {
                put_i64(out, t.saturating_add((*zone as i64) * 1_000_000));
                put_i32(out, *zone);
            }
            _ => return Err(type_mismatch("timetz", d)),
        },
        oids::TIMESTAMP => match d {
            Datum::Timestamp(v) => put_i64(out, *v),
            _ => return Err(type_mismatch("timestamp", d)),
        },
        oids::TIMESTAMPTZ => match d {
            Datum::TimestampTz(v) => put_i64(out, *v),
            _ => return Err(type_mismatch("timestamptz", d)),
        },
        oids::INTERVAL => match d {
            Datum::Interval(iv) => {
                out.extend_from_slice(&((iv.cmp_span() as u128) ^ SIGN128).to_be_bytes());
            }
            _ => return Err(type_mismatch("interval", d)),
        },
        oids::JSONB => match d {
            Datum::Jsonb(j) => put_jsonb(out, j, true),
            _ => return Err(type_mismatch("jsonb", d)),
        },
        oids::OID | oids::REGPROC | oids::REGPROCEDURE | oids::REGOPER | oids::REGOPERATOR | oids::REGCLASS
        | oids::REGTYPE | oids::REGROLE | oids::REGNAMESPACE | oids::REGCOLLATION | oids::REGCONFIG
        | oids::REGDICTIONARY | oids::XID | oids::CID => match d {
            Datum::Oid(v) => out.extend_from_slice(&v.to_be_bytes()),
            _ => return Err(type_mismatch("oid", d)),
        },
        oids::JSON | oids::XML | oids::POINT | oids::LSEG | oids::PATH | oids::BOX | oids::POLYGON | oids::LINE
        | oids::CIRCLE => return Err(unorderable(env, base)),
        oids::RECORD => {
            return Err(PgError::unimplemented("TYP-50", "index key encoding of anonymous record type"));
        }
        _ => {
            let info = match env.type_info(base) {
                Some(i) => i,
                None => return Err(PgError::internal(format!("cache lookup failed for type {base}"))),
            };
            if info.is_enum() {
                match d {
                    Datum::Text(label) => {
                        let order = enum_type::label_sort_order(env, base, label)?;
                        put_f32(out, order);
                    }
                    _ => return Err(type_mismatch("enum label", d)),
                }
            } else if info.is_array() {
                match d {
                    Datum::Array(a) => put_array(env, a, info.elem, out)?,
                    _ => return Err(type_mismatch("array", d)),
                }
            } else if info.is_composite() {
                match d {
                    Datum::Record(fields) => put_record(env, base, fields, out)?,
                    _ => return Err(type_mismatch("record", d)),
                }
            } else {
                return Err(PgError::unimplemented(
                    "TYP-50",
                    format!("index key encoding of type {}", info.name),
                ));
            }
        }
    }
    Ok(())
}

fn put_array(env: &dyn TypeEnv, a: &Array, type_elem: Oid, out: &mut Vec<u8>) -> Result<()> {
    let elem = if a.elem != 0 { a.elem } else { type_elem };
    for item in &a.items {
        out.push(0x02);
        if item.is_null() {
            out.push(0xFF);
        } else {
            out.push(0x80);
            encode_body(env, elem, item, out)?;
        }
    }
    out.push(0x01);
    out.push(a.dims.len().min(255) as u8);
    // PG array_cmp: all dimension lengths first, then all lower bounds.
    for (len, _) in &a.dims {
        put_i32(out, *len);
    }
    for (_, lb) in &a.dims {
        put_i32(out, *lb);
    }
    Ok(())
}

fn put_record(env: &dyn TypeEnv, type_oid: Oid, fields: &[Datum], out: &mut Vec<u8>) -> Result<()> {
    let defs = env
        .composite_fields(type_oid)
        .ok_or_else(|| PgError::internal(format!("cache lookup failed for composite type {type_oid}")))?;
    let live = defs.iter().filter(|f| !f.dropped).count();
    let include_dropped = fields.len() == defs.len();
    if !include_dropped && fields.len() != live {
        return Err(PgError::internal(format!(
            "record value has {} fields but composite type {} has {}",
            fields.len(),
            type_oid,
            live
        )));
    }
    let mut vals = fields.iter();
    for def in defs.iter() {
        if def.dropped && !include_dropped {
            continue;
        }
        let v = vals.next().ok_or_else(|| PgError::internal("record field count mismatch"))?;
        if def.dropped {
            continue;
        }
        encode_value(env, &KeyColumn::asc(def.type_id), v, out)?;
    }
    Ok(())
}

const J_EMPTY_ARRAY: u8 = 0x00;
const J_NULL: u8 = 0x10;
const J_STRING: u8 = 0x20;
const J_NUMBER: u8 = 0x30;
const J_BOOL: u8 = 0x40;
const J_ARRAY: u8 = 0x50;
const J_OBJECT: u8 = 0x60;

fn put_jsonb(out: &mut Vec<u8>, j: &Jsonb, top: bool) {
    match j {
        Jsonb::Null => out.push(J_NULL),
        Jsonb::String(s) => {
            out.push(J_STRING);
            put_bytes(out, s.as_bytes());
        }
        Jsonb::Number(n) => {
            out.push(J_NUMBER);
            put_numeric(out, n);
        }
        Jsonb::Bool(b) => {
            out.push(J_BOOL);
            out.push(*b as u8);
        }
        Jsonb::Array(items) => {
            out.push(if top && items.is_empty() { J_EMPTY_ARRAY } else { J_ARRAY });
            out.extend_from_slice(&(items.len() as u32).to_be_bytes());
            for it in items {
                put_jsonb(out, it, false);
            }
        }
        Jsonb::Object(pairs) => {
            out.push(J_OBJECT);
            out.extend_from_slice(&(pairs.len() as u32).to_be_bytes());
            for (k, v) in pairs {
                put_bytes(out, k.as_bytes());
                put_jsonb(out, v, false);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::compare::cmp_sort;
    use crate::datum::Interval;
    use crate::numeric::bigdecimal::BigDecimal;
    use proptest::prelude::*;
    use std::cmp::Ordering;

    fn env() -> &'static dyn TypeEnv {
        registry::builtin_env()
    }

    fn enc(ty: TypeId, d: &Datum, desc: bool, nf: bool) -> Vec<u8> {
        let col = KeyColumn { ty, desc, nulls_first: nf };
        encode_key_vec(env(), &[col], std::slice::from_ref(d)).unwrap()
    }

    fn num(s: &str) -> Datum {
        Datum::Numeric(Numeric::from_decimal(s.parse::<BigDecimal>().unwrap()))
    }

    fn check_pair(ty: TypeId, a: &Datum, b: &Datum) {
        for &(desc, nf) in &[(false, false), (false, true), (true, false), (true, true)] {
            let want = cmp_sort(env(), ty, a, b, desc, nf).unwrap();
            let ea = enc(ty, a, desc, nf);
            let eb = enc(ty, b, desc, nf);
            assert_eq!(ea.cmp(&eb), want, "{a:?} vs {b:?} desc={desc} nf={nf}");
        }
        let want = cmp_sort(env(), ty, a, b, false, false).unwrap();
        assert_eq!(enc(ty, a, false, false) == enc(ty, b, false, false), want == Ordering::Equal);
    }

    #[test]
    fn acceptance_basics() {
        assert_eq!(enc(TypeId::INT4, &Datum::Int4(-1), false, false), vec![0x80, 0x7f, 0xff, 0xff, 0xff]);
        assert_eq!(enc(TypeId::INT4, &Datum::Null, false, false), vec![0xFF]);
        assert_eq!(enc(TypeId::INT4, &Datum::Null, false, true), vec![0x00]);
        assert_eq!(enc(TypeId::NUMERIC, &num("1.0"), false, false), enc(TypeId::NUMERIC, &num("1.00"), false, false));
        assert_eq!(enc(TypeId::NUMERIC, &num("100"), false, false), enc(TypeId::NUMERIC, &num("1E+2"), false, false));
        let a = enc(TypeId::TEXT, &Datum::text("a"), false, false);
        let a0 = enc(TypeId::TEXT, &Datum::text("a\0"), false, false);
        let ab = enc(TypeId::TEXT, &Datum::text("ab"), false, false);
        assert!(a < a0 && a0 < ab);
        let lo = enc(TypeId::INT4, &Datum::Int4(1), true, false);
        let hi = enc(TypeId::INT4, &Datum::Int4(2), true, false);
        assert!(hi < lo);
    }

    #[test]
    fn equal_values_encode_equal() {
        let f = |v: f64| enc(TypeId::FLOAT8, &Datum::Float8(v), false, false);
        assert_eq!(f(-0.0), f(0.0));
        assert_eq!(f(f64::NAN), f(-f64::NAN));
        assert!(f(f64::INFINITY) < f(f64::NAN));
        let iv = |m, d| enc(TypeId::INTERVAL, &Datum::Interval(Interval::new(m, d, 0)), false, false);
        assert_eq!(iv(1, 0), iv(0, 30));
        let bp = |s: &str| enc(TypeId::BPCHAR, &Datum::text(s), false, false);
        assert_eq!(bp("a "), bp("a"));
    }

    #[test]
    fn numeric_order() {
        let vals = ["-Infinity", "-100.5", "-1", "-0.01", "0", "0.001", "0.5", "1", "1.5", "10", "123456", "Infinity", "NaN"];
        let mk = |s: &str| match s {
            "-Infinity" => Datum::Numeric(Numeric::NegInf),
            "Infinity" => Datum::Numeric(Numeric::PosInf),
            "NaN" => Datum::Numeric(Numeric::NaN),
            _ => num(s),
        };
        let encs: Vec<Vec<u8>> = vals.iter().map(|s| enc(TypeId::NUMERIC, &mk(s), false, false)).collect();
        for w in encs.windows(2) {
            assert!(w[0] < w[1]);
        }
    }

    #[test]
    fn unorderable_json() {
        let col = KeyColumn::asc(TypeId::JSON);
        let e = encode_key_vec(env(), &[col], &[Datum::Json("1".into())]).unwrap_err();
        assert_eq!(e.sqlstate, SqlState::UNDEFINED_FUNCTION);
        assert_eq!(e.message, "could not identify an ordering operator for type json");
    }

    #[test]
    fn prefix_keys() {
        let cols = [KeyColumn::asc(TypeId::INT4), KeyColumn::desc(TypeId::TEXT)];
        let full = encode_key_vec(env(), &cols, &[Datum::Int4(1), Datum::text("x")]).unwrap();
        let pre = encode_key_vec(env(), &cols, &[Datum::Int4(1)]).unwrap();
        assert!(full.starts_with(&pre));
        assert!(encode_key_vec(env(), &cols[..1], &[Datum::Int4(1), Datum::Int4(2)]).is_err());
    }

    fn arb_f64() -> impl Strategy<Value = f64> {
        prop_oneof![
            any::<f64>(),
            Just(0.0),
            Just(-0.0),
            Just(f64::NAN),
            Just(f64::INFINITY),
            Just(f64::NEG_INFINITY),
            (-5i32..5).prop_map(|v| v as f64),
        ]
    }

    fn nullable(s: impl Strategy<Value = Datum> + 'static) -> impl Strategy<Value = Datum> {
        prop_oneof![1 => Just(Datum::Null), 9 => s]
    }

    proptest! {
        #[test]
        fn ints(a in nullable(any::<i32>().prop_map(Datum::Int4)), b in nullable(any::<i32>().prop_map(Datum::Int4))) {
            check_pair(TypeId::INT4, &a, &b);
        }

        #[test]
        fn int8s(a in nullable(any::<i64>().prop_map(Datum::Int8)), b in nullable(any::<i64>().prop_map(Datum::Int8))) {
            check_pair(TypeId::INT8, &a, &b);
        }

        #[test]
        fn floats(a in nullable(arb_f64().prop_map(Datum::Float8)), b in nullable(arb_f64().prop_map(Datum::Float8))) {
            check_pair(TypeId::FLOAT8, &a, &b);
        }

        #[test]
        fn numerics(
            ma in -100000i64..100000, sa in 0i64..6,
            mb in -100000i64..100000, sb in 0i64..6,
        ) {
            let mk = |m: i64, s: i64| Datum::Numeric(Numeric::from_decimal(BigDecimal::from(m).with_scale(s)));
            check_pair(TypeId::NUMERIC, &mk(ma, sa), &mk(mb, sb));
        }

        #[test]
        fn texts(
            a in prop::collection::vec(prop::sample::select(vec!['a', 'b', '\0', 'é', '\u{1F600}']), 0..5),
            b in prop::collection::vec(prop::sample::select(vec!['a', 'b', '\0', 'é', '\u{1F600}']), 0..5),
        ) {
            let a = Datum::Text(a.into_iter().collect());
            let b = Datum::Text(b.into_iter().collect());
            check_pair(TypeId::TEXT, &a, &b);
        }

        #[test]
        fn timestamps(a in any::<i64>(), b in any::<i64>()) {
            check_pair(TypeId::TIMESTAMP, &Datum::Timestamp(a), &Datum::Timestamp(b));
            check_pair(TypeId::TIMESTAMPTZ, &Datum::TimestampTz(a), &Datum::TimestampTz(b));
        }

        #[test]
        fn intervals(
            a in (-1000i32..1000, -1000i32..1000, -1_000_000_000_000i64..1_000_000_000_000),
            b in (-1000i32..1000, -1000i32..1000, -1_000_000_000_000i64..1_000_000_000_000),
        ) {
            check_pair(
                TypeId::INTERVAL,
                &Datum::Interval(Interval::new(a.0, a.1, a.2)),
                &Datum::Interval(Interval::new(b.0, b.1, b.2)),
            );
        }

        #[test]
        fn int_arrays(
            a in prop::collection::vec(prop::option::of(-3i32..3), 0..4),
            b in prop::collection::vec(prop::option::of(-3i32..3), 0..4),
        ) {
            let mk = |v: Vec<Option<i32>>| {
                Datum::array(Array::from_vec(
                    oids::INT4,
                    v.into_iter().map(|x| x.map(Datum::Int4).unwrap_or(Datum::Null)).collect(),
                ))
            };
            check_pair(TypeId::INT4_ARRAY, &mk(a), &mk(b));
        }
    }
}
