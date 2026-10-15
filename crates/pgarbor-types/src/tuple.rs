//! Row (tuple) storage encoding, versioned (ARCHITECTURE §8). Lease TYP-51 (sonnet).
//!
//! Table rows are stored as `[relid][rowid u64 BE]` → `encode_row(values)`. The encoding is
//! SELF-DESCRIBING (each value carries a tag), so decoding needs no catalog: dropped columns, binary-
//! compatible type changes and ADD COLUMN all keep working. ON-DISK FORMAT: frozen at v1.
//!
//! Layout v1 (little-endian for fixed-width fields; `varint` = unsigned LEB128 u64, minimal length):
//! ```text
//! row    := 0x01 (format version) varint(ncols) value*ncols
//! value  := tag payload
//! tag 0x00 Null           (no payload)
//!     0x01 Bool false     0x02 Bool true
//!     0x03 Int2  i16 LE   0x04 Int4 i32 LE   0x05 Int8 i64 LE
//!     0x06 Float4 u32 LE (to_bits)          0x07 Float8 u64 LE (to_bits)  -- NaN bits preserved
//!     0x08 Numeric numeric_payload
//!     0x09 Text  varint(len) utf8-bytes
//!     0x0A Bytea varint(len) bytes
//!     0x0B Uuid  16 bytes
//!     0x0C Date  i32 LE   0x0D Time i64 LE   0x0E TimeTz i64 LE (µs) + i32 LE (zone)
//!     0x0F Timestamp i64 LE   0x10 TimestampTz i64 LE
//!     0x11 Interval i32 LE months, i32 LE days, i64 LE micros
//!     0x12 Json  varint(len) utf8-bytes
//!     0x13 Jsonb jsonb_payload
//!     0x14 Array varint(elem oid) u8(ndims) [i32 LE len, i32 LE lbound]*ndims varint(nitems) value*nitems
//!     0x15 Oid   u32 LE
//!     0x16 Char  u8
//!     0x17 Record varint(nfields) value*nfields
//! numeric_payload := u8 kind (0 finite, 1 NaN, 2 +Inf, 3 -Inf)
//!                    [finite: varint(scale) varint(len) bytes]   -- bytes = BigInt::to_signed_bytes_be of the
//!                                                                   unscaled integer (value = int × 10^-scale)
//! jsonb_payload   := u8 jkind: 0 null | 1 false | 2 true | 3 number numeric_payload | 4 string varint(len) utf8
//!                    | 5 array varint(n) jsonb_payload*n | 6 object varint(n) (varint(klen) key jsonb_payload)*n
//! ```
//! Decoding errors (truncated input, unknown tag, bad UTF-8, varint overflow, trailing bytes) →
//! XX001 `PgError::corruption("invalid tuple encoding: <what>")`. Unknown version byte → XX001
//! `unsupported tuple format version N`. Decoded Numerics must satisfy the Numeric invariant.
//! `encode_row(decode_row(b)) == b` for every valid `b` (canonical encoding), tested with proptest.
use crate::array::{Array, MAXDIM};
use crate::datum::{Datum, Interval};
use crate::json::{jsonb_key_cmp, Jsonb};
use crate::numeric::{Numeric, NUMERIC_MAX_DSCALE};
use bigdecimal::BigDecimal;
use num_bigint::BigInt;
use pgarbor_common::{PgError, Result, SqlState};
use std::cmp::Ordering;

/// Current tuple format version byte.
pub const TUPLE_FORMAT_V1: u8 = 0x01;

/// Maximum nesting depth (records / arrays / jsonb) accepted by the encoder and decoder. Keeps
/// recursion bounded on hostile input; encoder and decoder agree so every encoded row decodes.
/// Sized for debug-build frames (~4-8 KiB per level on the record/jsonb paths) on a 2 MiB thread
/// stack: 512 overflowed in debug builds before the recursive arms were split out of line.
const MAX_DEPTH: usize = 128;

const T_NULL: u8 = 0x00;
const T_FALSE: u8 = 0x01;
const T_TRUE: u8 = 0x02;
const T_INT2: u8 = 0x03;
const T_INT4: u8 = 0x04;
const T_INT8: u8 = 0x05;
const T_FLOAT4: u8 = 0x06;
const T_FLOAT8: u8 = 0x07;
const T_NUMERIC: u8 = 0x08;
const T_TEXT: u8 = 0x09;
const T_BYTEA: u8 = 0x0A;
const T_UUID: u8 = 0x0B;
const T_DATE: u8 = 0x0C;
const T_TIME: u8 = 0x0D;
const T_TIMETZ: u8 = 0x0E;
const T_TIMESTAMP: u8 = 0x0F;
const T_TIMESTAMPTZ: u8 = 0x10;
const T_INTERVAL: u8 = 0x11;
const T_JSON: u8 = 0x12;
const T_JSONB: u8 = 0x13;
const T_ARRAY: u8 = 0x14;
const T_OID: u8 = 0x15;
const T_CHAR: u8 = 0x16;
const T_RECORD: u8 = 0x17;

fn bad(what: &str) -> PgError {
    PgError::corruption(format!("invalid tuple encoding: {what}"))
}

fn depth_exceeded() -> PgError {
    PgError::new(SqlState::PROGRAM_LIMIT_EXCEEDED, "value nesting is too deep to store in a row")
}

// ---------------------------------------------------------------------------------------------
// Encoding
// ---------------------------------------------------------------------------------------------

fn put_varint(out: &mut Vec<u8>, mut v: u64) {
    while v >= 0x80 {
        out.push((v as u8 & 0x7f) | 0x80);
        v >>= 7;
    }
    out.push(v as u8);
}

fn put_bytes(out: &mut Vec<u8>, b: &[u8]) {
    put_varint(out, b.len() as u64);
    out.extend_from_slice(b);
}

fn put_numeric(n: &Numeric, out: &mut Vec<u8>) -> Result<()> {
    match n {
        Numeric::Finite(d) => {
            let (mut int, mut scale) = d.as_bigint_and_exponent();
            if scale < 0 {
                // Non-normalized value: re-express with scale 0 (same numeric value).
                let (i, _) = d.with_scale(0).as_bigint_and_exponent();
                int = i;
                scale = 0;
            }
            if scale > NUMERIC_MAX_DSCALE {
                return Err(PgError::internal(format!("numeric display scale {scale} exceeds the storable maximum")));
            }
            out.push(0);
            put_varint(out, scale as u64);
            put_bytes(out, &int.to_signed_bytes_be());
        }
        Numeric::NaN => out.push(1),
        Numeric::PosInf => out.push(2),
        Numeric::NegInf => out.push(3),
    }
    Ok(())
}

fn put_jsonb(j: &Jsonb, out: &mut Vec<u8>, depth: usize) -> Result<()> {
    if depth > MAX_DEPTH {
        return Err(depth_exceeded());
    }
    match j {
        Jsonb::Null => out.push(0),
        Jsonb::Bool(false) => out.push(1),
        Jsonb::Bool(true) => out.push(2),
        Jsonb::Number(n) => {
            if !n.is_finite() {
                return Err(PgError::internal("jsonb number must be finite"));
            }
            out.push(3);
            put_numeric(n, out)?;
        }
        Jsonb::String(s) => {
            out.push(4);
            put_bytes(out, s.as_bytes());
        }
        Jsonb::Array(items) => {
            out.push(5);
            put_varint(out, items.len() as u64);
            for it in items {
                put_jsonb(it, out, depth + 1)?;
            }
        }
        Jsonb::Object(pairs) => {
            out.push(6);
            put_varint(out, pairs.len() as u64);
            for (k, v) in pairs {
                put_bytes(out, k.as_bytes());
                put_jsonb(v, out, depth + 1)?;
            }
        }
    }
    Ok(())
}

fn put_value(value: &Datum, out: &mut Vec<u8>, depth: usize) -> Result<()> {
    if depth > MAX_DEPTH {
        return Err(depth_exceeded());
    }
    match value {
        Datum::Null => out.push(T_NULL),
        Datum::Bool(false) => out.push(T_FALSE),
        Datum::Bool(true) => out.push(T_TRUE),
        Datum::Int2(v) => {
            out.push(T_INT2);
            out.extend_from_slice(&v.to_le_bytes());
        }
        Datum::Int4(v) => {
            out.push(T_INT4);
            out.extend_from_slice(&v.to_le_bytes());
        }
        Datum::Int8(v) => {
            out.push(T_INT8);
            out.extend_from_slice(&v.to_le_bytes());
        }
        Datum::Float4(v) => {
            out.push(T_FLOAT4);
            out.extend_from_slice(&v.to_bits().to_le_bytes());
        }
        Datum::Float8(v) => {
            out.push(T_FLOAT8);
            out.extend_from_slice(&v.to_bits().to_le_bytes());
        }
        Datum::Numeric(n) => {
            out.push(T_NUMERIC);
            put_numeric(n, out)?;
        }
        Datum::Text(s) => {
            out.push(T_TEXT);
            put_bytes(out, s.as_bytes());
        }
        Datum::Bytea(b) => {
            out.push(T_BYTEA);
            put_bytes(out, b);
        }
        Datum::Uuid(u) => {
            out.push(T_UUID);
            out.extend_from_slice(u);
        }
        Datum::Date(v) => {
            out.push(T_DATE);
            out.extend_from_slice(&v.to_le_bytes());
        }
        Datum::Time(v) => {
            out.push(T_TIME);
            out.extend_from_slice(&v.to_le_bytes());
        }
        Datum::TimeTz(t, z) => {
            out.push(T_TIMETZ);
            out.extend_from_slice(&t.to_le_bytes());
            out.extend_from_slice(&z.to_le_bytes());
        }
        Datum::Timestamp(v) => {
            out.push(T_TIMESTAMP);
            out.extend_from_slice(&v.to_le_bytes());
        }
        Datum::TimestampTz(v) => {
            out.push(T_TIMESTAMPTZ);
            out.extend_from_slice(&v.to_le_bytes());
        }
        Datum::Interval(iv) => {
            out.push(T_INTERVAL);
            out.extend_from_slice(&iv.months.to_le_bytes());
            out.extend_from_slice(&iv.days.to_le_bytes());
            out.extend_from_slice(&iv.micros.to_le_bytes());
        }
        Datum::Json(s) => {
            out.push(T_JSON);
            put_bytes(out, s.as_bytes());
        }
        Datum::Jsonb(j) => {
            out.push(T_JSONB);
            put_jsonb(j, out, depth + 1)?;
        }
        Datum::Array(a) => {
            if a.dims.len() > MAXDIM {
                return Err(PgError::internal("array has too many dimensions to store"));
            }
            if a.dims.iter().any(|(l, _)| *l < 0) || a.items.len() != a.expected_len() {
                return Err(PgError::internal("array dimensions do not match its element count"));
            }
            out.push(T_ARRAY);
            put_varint(out, a.elem as u64);
            out.push(a.dims.len() as u8);
            for (len, lb) in &a.dims {
                out.extend_from_slice(&len.to_le_bytes());
                out.extend_from_slice(&lb.to_le_bytes());
            }
            put_varint(out, a.items.len() as u64);
            for it in &a.items {
                put_value(it, out, depth + 1)?;
            }
        }
        Datum::Oid(v) => {
            out.push(T_OID);
            out.extend_from_slice(&v.to_le_bytes());
        }
        Datum::Char(c) => {
            out.push(T_CHAR);
            out.push(*c);
        }
        Datum::Record(fields) => {
            out.push(T_RECORD);
            put_varint(out, fields.len() as u64);
            for f in fields {
                put_value(f, out, depth + 1)?;
            }
        }
    }
    Ok(())
}

/// Encode a full row.
// CONTRACT
pub fn encode_row(values: &[Datum]) -> Result<Vec<u8>> {
    let mut out = Vec::with_capacity(2 + values.len() * 9);
    out.push(TUPLE_FORMAT_V1);
    put_varint(&mut out, values.len() as u64);
    for v in values {
        put_value(v, &mut out, 0)?;
    }
    Ok(out)
}

/// Append one value (tag + payload) — also used by `record_image_*` comparisons and `pg_column_size`.
// CONTRACT
pub fn encode_value(value: &Datum, out: &mut Vec<u8>) -> Result<()> {
    let start = out.len();
    let r = put_value(value, out, 0);
    if r.is_err() {
        out.truncate(start);
    }
    r
}

// ---------------------------------------------------------------------------------------------
// Decoding
// ---------------------------------------------------------------------------------------------

struct Reader<'a> {
    buf: &'a [u8],
    pos: usize,
}

impl<'a> Reader<'a> {
    fn remaining(&self) -> usize {
        self.buf.len() - self.pos
    }

    fn u8(&mut self) -> Result<u8> {
        match self.buf.get(self.pos) {
            Some(b) => {
                self.pos += 1;
                Ok(*b)
            }
            None => Err(bad("truncated input")),
        }
    }

    fn take(&mut self, n: usize) -> Result<&'a [u8]> {
        if n > self.remaining() {
            return Err(bad("truncated input"));
        }
        let s = &self.buf[self.pos..self.pos + n];
        self.pos += n;
        Ok(s)
    }

    fn array<const N: usize>(&mut self) -> Result<[u8; N]> {
        let s = self.take(N)?;
        let mut a = [0u8; N];
        a.copy_from_slice(s);
        Ok(a)
    }

    fn varint(&mut self) -> Result<u64> {
        let mut v: u64 = 0;
        for i in 0..10u32 {
            let b = self.u8()?;
            let low = (b & 0x7f) as u64;
            if i == 9 && low > 1 {
                return Err(bad("varint overflow"));
            }
            v |= low << (7 * i);
            if b & 0x80 == 0 {
                if i > 0 && b == 0 {
                    return Err(bad("non-minimal varint"));
                }
                return Ok(v);
            }
        }
        Err(bad("varint overflow"))
    }

    /// A varint used as a byte length; must fit in the remaining input.
    fn byte_len(&mut self) -> Result<usize> {
        let n = self.varint()?;
        match usize::try_from(n) {
            Ok(n) if n <= self.remaining() => Ok(n),
            _ => Err(bad("truncated input")),
        }
    }

    /// A varint element count; every element needs at least one byte, so it is bounded by the
    /// remaining input (this also bounds allocations).
    fn count(&mut self) -> Result<usize> {
        self.byte_len()
    }

    fn string(&mut self) -> Result<String> {
        let n = self.byte_len()?;
        let b = self.take(n)?;
        match std::str::from_utf8(b) {
            Ok(s) => Ok(s.to_owned()),
            Err(_) => Err(bad("invalid UTF-8 in string")),
        }
    }

    fn i16(&mut self) -> Result<i16> {
        Ok(i16::from_le_bytes(self.array()?))
    }
    fn i32(&mut self) -> Result<i32> {
        Ok(i32::from_le_bytes(self.array()?))
    }
    fn i64(&mut self) -> Result<i64> {
        Ok(i64::from_le_bytes(self.array()?))
    }
    fn u32(&mut self) -> Result<u32> {
        Ok(u32::from_le_bytes(self.array()?))
    }
    fn u64(&mut self) -> Result<u64> {
        Ok(u64::from_le_bytes(self.array()?))
    }

    fn finish(&self) -> Result<()> {
        if self.pos != self.buf.len() {
            return Err(bad("trailing bytes"));
        }
        Ok(())
    }
}

/// Parse the version byte and column count.
fn read_header(bytes: &[u8]) -> Result<(Reader<'_>, usize)> {
    let mut r = Reader { buf: bytes, pos: 0 };
    let ver = r.u8()?;
    if ver != TUPLE_FORMAT_V1 {
        return Err(PgError::corruption(format!("unsupported tuple format version {ver}")));
    }
    let n = r.count()?;
    Ok((r, n))
}

/// Read a numeric payload. `allow_special` is false inside jsonb numbers.
fn read_numeric(r: &mut Reader<'_>, allow_special: bool) -> Result<Numeric> {
    let kind = r.u8()?;
    match kind {
        0 => {
            let scale = r.varint()?;
            if scale > NUMERIC_MAX_DSCALE as u64 {
                return Err(bad("numeric scale out of range"));
            }
            let n = r.byte_len()?;
            let b = r.take(n)?;
            if b.is_empty() {
                return Err(bad("empty numeric digits"));
            }
            let int = BigInt::from_signed_bytes_be(b);
            if int.to_signed_bytes_be() != b {
                return Err(bad("non-canonical numeric digits"));
            }
            Ok(Numeric::Finite(BigDecimal::new(int, scale as i64)))
        }
        1 | 2 | 3 if allow_special => Ok(match kind {
            1 => Numeric::NaN,
            2 => Numeric::PosInf,
            _ => Numeric::NegInf,
        }),
        _ => Err(bad("unknown numeric kind")),
    }
}

fn skip_numeric(r: &mut Reader<'_>) -> Result<()> {
    match r.u8()? {
        0 => {
            r.varint()?;
            let n = r.byte_len()?;
            r.take(n)?;
            Ok(())
        }
        1..=3 => Ok(()),
        _ => Err(bad("unknown numeric kind")),
    }
}

fn read_jsonb(r: &mut Reader<'_>, depth: usize) -> Result<Jsonb> {
    if depth > MAX_DEPTH {
        return Err(bad("nesting too deep"));
    }
    Ok(match r.u8()? {
        0 => Jsonb::Null,
        1 => Jsonb::Bool(false),
        2 => Jsonb::Bool(true),
        3 => Jsonb::Number(read_numeric(r, false)?),
        4 => Jsonb::String(r.string()?),
        5 => read_jsonb_array(r, depth + 1)?,
        6 => read_jsonb_object(r, depth + 1)?,
        _ => return Err(bad("unknown jsonb kind")),
    })
}

// Recursive jsonb arms live out of line so `read_jsonb`'s frame stays small (see `read_leaf`).
#[inline(never)]
fn read_jsonb_array(r: &mut Reader<'_>, depth: usize) -> Result<Jsonb> {
    let n = r.count()?;
    let mut items = Vec::with_capacity(n);
    for _ in 0..n {
        items.push(read_jsonb(r, depth)?);
    }
    Ok(Jsonb::Array(items))
}

#[inline(never)]
fn read_jsonb_object(r: &mut Reader<'_>, depth: usize) -> Result<Jsonb> {
    let n = r.count()?;
    let mut pairs: Vec<(String, Jsonb)> = Vec::with_capacity(n);
    for _ in 0..n {
        let k = r.string()?;
        if let Some((prev, _)) = pairs.last() {
            if jsonb_key_cmp(prev, &k) != Ordering::Less {
                return Err(bad("jsonb object keys not sorted and unique"));
            }
        }
        let v = read_jsonb(r, depth)?;
        pairs.push((k, v));
    }
    Ok(Jsonb::Object(pairs))
}

fn skip_jsonb(r: &mut Reader<'_>, depth: usize) -> Result<()> {
    if depth > MAX_DEPTH {
        return Err(bad("nesting too deep"));
    }
    match r.u8()? {
        0..=2 => {}
        3 => skip_numeric(r)?,
        4 => {
            let n = r.byte_len()?;
            r.take(n)?;
        }
        5 => skip_jsonb_array(r, depth + 1)?,
        6 => skip_jsonb_object(r, depth + 1)?,
        _ => return Err(bad("unknown jsonb kind")),
    }
    Ok(())
}

#[inline(never)]
fn skip_jsonb_array(r: &mut Reader<'_>, depth: usize) -> Result<()> {
    let n = r.count()?;
    for _ in 0..n {
        skip_jsonb(r, depth)?;
    }
    Ok(())
}

#[inline(never)]
fn skip_jsonb_object(r: &mut Reader<'_>, depth: usize) -> Result<()> {
    let n = r.count()?;
    for _ in 0..n {
        let k = r.byte_len()?;
        r.take(k)?;
        skip_jsonb(r, depth)?;
    }
    Ok(())
}

/// Read the array header; returns (elem oid, dims, nitems).
fn read_array_header(r: &mut Reader<'_>) -> Result<(u32, Vec<(i32, i32)>, usize)> {
    let elem = u32::try_from(r.varint()?).map_err(|_| bad("array element oid out of range"))?;
    let ndims = r.u8()? as usize;
    if ndims > MAXDIM {
        return Err(bad("too many array dimensions"));
    }
    let mut dims = Vec::with_capacity(ndims);
    let mut product: u64 = if ndims == 0 { 0 } else { 1 };
    for _ in 0..ndims {
        let len = r.i32()?;
        let lb = r.i32()?;
        if len < 0 {
            return Err(bad("negative array dimension"));
        }
        product = product.checked_mul(len as u64).ok_or_else(|| bad("array size overflow"))?;
        dims.push((len, lb));
    }
    let n = r.count()?;
    if n as u64 != product {
        return Err(bad("array element count does not match dimensions"));
    }
    Ok((elem, dims, n))
}

fn read_value(r: &mut Reader<'_>, depth: usize) -> Result<Datum> {
    if depth > MAX_DEPTH {
        return Err(bad("nesting too deep"));
    }
    let tag = r.u8()?;
    match tag {
        T_JSONB => Ok(Datum::Jsonb(read_jsonb(r, depth + 1)?)),
        T_ARRAY => read_array(r, depth + 1),
        T_RECORD => read_record(r, depth + 1),
        _ => read_leaf(r, tag),
    }
}

/// Non-recursive scalar decode. Kept out of line so the recursive frames (`read_value`,
/// `read_array`, `read_record`) stay small: an inlined copy of this whole match made debug-build
/// frames large enough that MAX_DEPTH nested records overflowed a 2 MiB thread stack.
#[inline(never)]
fn read_leaf(r: &mut Reader<'_>, tag: u8) -> Result<Datum> {
    Ok(match tag {
        T_NULL => Datum::Null,
        T_FALSE => Datum::Bool(false),
        T_TRUE => Datum::Bool(true),
        T_INT2 => Datum::Int2(r.i16()?),
        T_INT4 => Datum::Int4(r.i32()?),
        T_INT8 => Datum::Int8(r.i64()?),
        T_FLOAT4 => Datum::Float4(f32::from_bits(r.u32()?)),
        T_FLOAT8 => Datum::Float8(f64::from_bits(r.u64()?)),
        T_NUMERIC => Datum::Numeric(read_numeric(r, true)?),
        T_TEXT => Datum::Text(r.string()?),
        T_BYTEA => {
            let n = r.byte_len()?;
            Datum::Bytea(r.take(n)?.to_vec())
        }
        T_UUID => Datum::Uuid(r.array::<16>()?),
        T_DATE => Datum::Date(r.i32()?),
        T_TIME => Datum::Time(r.i64()?),
        T_TIMETZ => {
            let t = r.i64()?;
            let z = r.i32()?;
            Datum::TimeTz(t, z)
        }
        T_TIMESTAMP => Datum::Timestamp(r.i64()?),
        T_TIMESTAMPTZ => Datum::TimestampTz(r.i64()?),
        T_INTERVAL => {
            let months = r.i32()?;
            let days = r.i32()?;
            let micros = r.i64()?;
            Datum::Interval(Interval { months, days, micros })
        }
        T_JSON => Datum::Json(r.string()?),
        T_OID => Datum::Oid(r.u32()?),
        T_CHAR => Datum::Char(r.u8()?),
        _ => return Err(bad(&format!("unknown value tag 0x{tag:02x}"))),
    })
}

#[inline(never)]
fn read_array(r: &mut Reader<'_>, depth: usize) -> Result<Datum> {
    let (elem, dims, n) = read_array_header(r)?;
    let mut items = Vec::with_capacity(n);
    for _ in 0..n {
        items.push(read_value(r, depth)?);
    }
    Ok(Datum::Array(Box::new(Array { elem, dims, items })))
}

#[inline(never)]
fn read_record(r: &mut Reader<'_>, depth: usize) -> Result<Datum> {
    let n = r.count()?;
    let mut fields = Vec::with_capacity(n);
    for _ in 0..n {
        fields.push(read_value(r, depth)?);
    }
    Ok(Datum::Record(fields))
}

/// Advance past one value without materializing it (still bounds-checked and structure-checked).
fn skip_value(r: &mut Reader<'_>, depth: usize) -> Result<()> {
    if depth > MAX_DEPTH {
        return Err(bad("nesting too deep"));
    }
    let tag = r.u8()?;
    match tag {
        T_JSONB => skip_jsonb(r, depth + 1),
        T_ARRAY => skip_array(r, depth + 1),
        T_RECORD => skip_record(r, depth + 1),
        _ => skip_leaf(r, tag),
    }
}

/// Non-recursive skip of a scalar tag; out of line to keep `skip_value`'s recursive frame small.
#[inline(never)]
fn skip_leaf(r: &mut Reader<'_>, tag: u8) -> Result<()> {
    match tag {
        T_NULL | T_FALSE | T_TRUE => {}
        T_INT2 => {
            r.take(2)?;
        }
        T_INT4 | T_FLOAT4 | T_DATE | T_OID => {
            r.take(4)?;
        }
        T_INT8 | T_FLOAT8 | T_TIME | T_TIMESTAMP | T_TIMESTAMPTZ => {
            r.take(8)?;
        }
        T_NUMERIC => skip_numeric(r)?,
        T_TEXT | T_BYTEA | T_JSON => {
            let n = r.byte_len()?;
            r.take(n)?;
        }
        T_UUID => {
            r.take(16)?;
        }
        T_TIMETZ => {
            r.take(12)?;
        }
        T_INTERVAL => {
            r.take(16)?;
        }
        T_CHAR => {
            r.u8()?;
        }
        _ => return Err(bad(&format!("unknown value tag 0x{tag:02x}"))),
    }
    Ok(())
}

#[inline(never)]
fn skip_array(r: &mut Reader<'_>, depth: usize) -> Result<()> {
    let (_, _, n) = read_array_header(r)?;
    for _ in 0..n {
        skip_value(r, depth)?;
    }
    Ok(())
}

#[inline(never)]
fn skip_record(r: &mut Reader<'_>, depth: usize) -> Result<()> {
    let n = r.count()?;
    for _ in 0..n {
        skip_value(r, depth)?;
    }
    Ok(())
}

/// Decode a full row (as many values as were stored; callers pad missing trailing columns with the
/// column default / NULL after ADD COLUMN).
// CONTRACT
pub fn decode_row(bytes: &[u8]) -> Result<Vec<Datum>> {
    let (mut r, n) = read_header(bytes)?;
    let mut out = Vec::with_capacity(n);
    for _ in 0..n {
        out.push(read_value(&mut r, 0)?);
    }
    r.finish()?;
    Ok(out)
}

/// Decode only columns whose index is in `wanted` (ascending), skipping the others cheaply; returns
/// `Datum::Null` for columns not wanted or not stored. Result length = `ncols_hint.max(stored)`.
// CONTRACT
pub fn decode_columns(bytes: &[u8], wanted: &[usize], ncols_hint: usize) -> Result<Vec<Datum>> {
    let (mut r, n) = read_header(bytes)?;
    let mut out = vec![Datum::Null; ncols_hint.max(n)];
    let mut wi = 0usize;
    for i in 0..n {
        while wi < wanted.len() && wanted[wi] < i {
            wi += 1;
        }
        if wi < wanted.len() && wanted[wi] == i {
            out[i] = read_value(&mut r, 0)?;
        } else {
            skip_value(&mut r, 0)?;
        }
    }
    r.finish()?;
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    fn dec(s: &str, scale: i64) -> Numeric {
        Numeric::Finite(BigDecimal::new(BigInt::parse_bytes(s.as_bytes(), 10).unwrap(), scale))
    }

    fn sample_values() -> Vec<Datum> {
        vec![
            Datum::Null,
            Datum::Bool(true),
            Datum::Bool(false),
            Datum::Int2(-5),
            Datum::Int4(i32::MIN),
            Datum::Int8(i64::MAX),
            Datum::Float4(1.5),
            Datum::Float4(f32::NAN),
            Datum::Float8(-0.0),
            Datum::Float8(f64::INFINITY),
            Datum::Numeric(dec("150", 2)),
            Datum::Numeric(dec("-12345678901234567890123", 5)),
            Datum::Numeric(Numeric::zero()),
            Datum::Numeric(Numeric::NaN),
            Datum::Numeric(Numeric::PosInf),
            Datum::Numeric(Numeric::NegInf),
            Datum::text("héllo"),
            Datum::text(""),
            Datum::Bytea(vec![0, 1, 255]),
            Datum::Uuid([7; 16]),
            Datum::Date(i32::MAX),
            Datum::Time(86_400_000_000),
            Datum::TimeTz(5, -19800),
            Datum::Timestamp(i64::MIN),
            Datum::TimestampTz(42),
            Datum::Interval(Interval::new(1, -2, 3)),
            Datum::Json("{ \"a\" : 1 }".to_string()),
            Datum::Jsonb(Jsonb::object_from_pairs(vec![
                ("b".into(), Jsonb::Array(vec![Jsonb::Null, Jsonb::Bool(true), Jsonb::Number(dec("150", 2))])),
                ("aa".into(), Jsonb::String("x".into())),
            ])),
            Datum::array(Array::from_vec(23, vec![Datum::Int4(1), Datum::Null, Datum::Int4(3)])),
            Datum::array(Array::empty(25)),
            Datum::array(Array {
                elem: 23,
                dims: vec![(2, 1), (2, 0)],
                items: vec![Datum::Int4(1), Datum::Int4(2), Datum::Int4(3), Datum::Int4(4)],
            }),
            Datum::Oid(u32::MAX),
            Datum::Char(0),
            Datum::Record(vec![Datum::Int4(1), Datum::Record(vec![Datum::text("x"), Datum::Null])]),
        ]
    }

    #[test]
    fn acceptance_int4() {
        assert_eq!(encode_row(&[Datum::Int4(1)]).unwrap(), vec![1, 1, 4, 1, 0, 0, 0]);
    }

    #[test]
    fn roundtrip_all_variants() {
        let vals = sample_values();
        let bytes = encode_row(&vals).unwrap();
        let back = decode_row(&bytes).unwrap();
        assert_eq!(back.len(), vals.len());
        assert_eq!(encode_row(&back).unwrap(), bytes);
        for (a, b) in vals.iter().zip(&back) {
            match a {
                Datum::Float4(f) if f.is_nan() => assert!(matches!(b, Datum::Float4(g) if g.is_nan())),
                Datum::Numeric(n) => {
                    assert_eq!(a, b);
                    assert_eq!(n.dscale(), b.as_numeric().unwrap().dscale());
                }
                _ => assert_eq!(a, b),
            }
        }
    }

    #[test]
    fn version_2_rejected() {
        let e = decode_row(&[2, 0]).unwrap_err();
        assert_eq!(e.sqlstate.code(), "XX001");
        assert!(e.message.contains("unsupported tuple format version 2"));
    }

    #[test]
    fn truncated_rejected() {
        let bytes = encode_row(&sample_values()).unwrap();
        for cut in 0..bytes.len() {
            let e = decode_row(&bytes[..cut]).unwrap_err();
            assert_eq!(e.sqlstate.code(), "XX001");
        }
        let mut extra = bytes.clone();
        extra.push(0);
        assert_eq!(decode_row(&extra).unwrap_err().sqlstate.code(), "XX001");
    }

    #[test]
    fn projection_skips_columns() {
        let vals = sample_values();
        let bytes = encode_row(&vals).unwrap();
        let all = decode_row(&bytes).unwrap();
        let got = decode_columns(&bytes, &[1, 16, 28], vals.len() + 2).unwrap();
        assert_eq!(got.len(), vals.len() + 2);
        for (i, d) in got.iter().enumerate() {
            if [1usize, 16, 28].contains(&i) {
                assert_eq!(d, &all[i]);
            } else {
                assert!(d.is_null());
            }
        }
        let none = decode_columns(&bytes, &[], 0).unwrap();
        assert_eq!(none.len(), vals.len());
        assert!(none.iter().all(|d| d.is_null()));
    }

    #[test]
    fn non_minimal_varint_rejected() {
        assert_eq!(decode_row(&[1, 0x80, 0x00]).unwrap_err().sqlstate.code(), "XX001");
    }

    #[test]
    fn deep_nesting_rejected_not_overflowed() {
        let mut bytes = vec![1u8, 1];
        for _ in 0..100_000 {
            bytes.extend_from_slice(&[T_RECORD, 1]);
        }
        assert_eq!(decode_row(&bytes).unwrap_err().sqlstate.code(), "XX001");
    }

    fn arb_numeric() -> impl Strategy<Value = Numeric> {
        prop_oneof![
            (any::<i64>(), 0i64..40).prop_map(|(i, s)| Numeric::Finite(BigDecimal::new(BigInt::from(i), s))),
            Just(Numeric::NaN),
            Just(Numeric::PosInf),
            Just(Numeric::NegInf),
        ]
    }

    fn arb_jsonb() -> impl Strategy<Value = Jsonb> {
        let leaf = prop_oneof![
            Just(Jsonb::Null),
            any::<bool>().prop_map(Jsonb::Bool),
            (any::<i64>(), 0i64..10)
                .prop_map(|(i, s)| Jsonb::Number(Numeric::Finite(BigDecimal::new(BigInt::from(i), s)))),
            ".{0,8}".prop_map(Jsonb::String),
        ];
        leaf.prop_recursive(3, 24, 4, |inner| {
            prop_oneof![
                prop::collection::vec(inner.clone(), 0..4).prop_map(Jsonb::Array),
                prop::collection::vec((".{0,4}", inner), 0..4).prop_map(Jsonb::object_from_pairs),
            ]
        })
    }

    fn arb_datum() -> impl Strategy<Value = Datum> {
        let leaf = prop_oneof![
            Just(Datum::Null),
            any::<bool>().prop_map(Datum::Bool),
            any::<i16>().prop_map(Datum::Int2),
            any::<i32>().prop_map(Datum::Int4),
            any::<i64>().prop_map(Datum::Int8),
            any::<u32>().prop_map(|b| Datum::Float4(f32::from_bits(b))),
            any::<u64>().prop_map(|b| Datum::Float8(f64::from_bits(b))),
            arb_numeric().prop_map(Datum::Numeric),
            ".{0,12}".prop_map(Datum::Text),
            prop::collection::vec(any::<u8>(), 0..12).prop_map(Datum::Bytea),
            any::<[u8; 16]>().prop_map(Datum::Uuid),
            any::<i32>().prop_map(Datum::Date),
            any::<i64>().prop_map(Datum::Time),
            (any::<i64>(), any::<i32>()).prop_map(|(t, z)| Datum::TimeTz(t, z)),
            any::<i64>().prop_map(Datum::Timestamp),
            any::<i64>().prop_map(Datum::TimestampTz),
            (any::<i32>(), any::<i32>(), any::<i64>())
                .prop_map(|(m, d, u)| Datum::Interval(Interval::new(m, d, u))),
            ".{0,12}".prop_map(Datum::Json),
            arb_jsonb().prop_map(Datum::Jsonb),
            any::<u32>().prop_map(Datum::Oid),
            any::<u8>().prop_map(Datum::Char),
        ];
        leaf.prop_recursive(3, 32, 4, |inner| {
            prop_oneof![
                prop::collection::vec(inner.clone(), 0..4).prop_map(Datum::Record),
                (any::<u32>(), prop::collection::vec(inner, 0..4))
                    .prop_map(|(e, items)| Datum::array(Array::from_vec(e, items))),
            ]
        })
    }

    proptest! {
        #[test]
        fn canonical_roundtrip(vals in prop::collection::vec(arb_datum(), 0..6)) {
            let bytes = encode_row(&vals).unwrap();
            let back = decode_row(&bytes).unwrap();
            prop_assert_eq!(encode_row(&back).unwrap(), bytes.clone());
            let cols = decode_columns(&bytes, &[0, 2], 0).unwrap();
            prop_assert_eq!(cols.len(), vals.len());
        }

        #[test]
        fn arbitrary_bytes_never_panic(bytes in prop::collection::vec(any::<u8>(), 0..64)) {
            match decode_row(&bytes) {
                Ok(vals) => prop_assert_eq!(encode_row(&vals).unwrap(), bytes.clone()),
                Err(e) => prop_assert_eq!(e.sqlstate.code(), "XX001"),
            }
            if let Err(e) = decode_columns(&bytes, &[0, 1], 3) {
                prop_assert_eq!(e.sqlstate.code(), "XX001");
            }
        }
    }
}
