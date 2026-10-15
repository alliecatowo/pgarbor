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
use crate::context::TypeEnv;
use crate::datum::{Datum, TypeId};
use pgarbor_common::{PgError, Result};

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
#[allow(unused_variables)]
pub fn encode_key(env: &dyn TypeEnv, cols: &[KeyColumn], values: &[Datum], out: &mut Vec<u8>) -> Result<()> {
    Err(PgError::unimplemented("TYP-50", "index key encoding"))
}

/// Convenience: encode into a new vector.
pub fn encode_key_vec(env: &dyn TypeEnv, cols: &[KeyColumn], values: &[Datum]) -> Result<Vec<u8>> {
    let mut out = Vec::new();
    encode_key(env, cols, values, &mut out)?;
    Ok(out)
}

/// Append the encoding of one column value (tag + body) to `out`.
// CONTRACT
#[allow(unused_variables)]
pub fn encode_value(env: &dyn TypeEnv, col: &KeyColumn, value: &Datum, out: &mut Vec<u8>) -> Result<()> {
    Err(PgError::unimplemented("TYP-50", "index key encoding"))
}
