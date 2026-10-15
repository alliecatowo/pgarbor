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
use crate::datum::Datum;
use pgarbor_common::{PgError, Result};

/// Current tuple format version byte.
pub const TUPLE_FORMAT_V1: u8 = 0x01;

/// Encode a full row.
// CONTRACT
#[allow(unused_variables)]
pub fn encode_row(values: &[Datum]) -> Result<Vec<u8>> {
    Err(PgError::unimplemented("TYP-51", "row encoding"))
}

/// Decode a full row (as many values as were stored; callers pad missing trailing columns with the
/// column default / NULL after ADD COLUMN).
// CONTRACT
#[allow(unused_variables)]
pub fn decode_row(bytes: &[u8]) -> Result<Vec<Datum>> {
    Err(PgError::unimplemented("TYP-51", "row decoding"))
}

/// Decode only columns whose index is in `wanted` (ascending), skipping the others cheaply; returns
/// `Datum::Null` for columns not wanted or not stored. Result length = `ncols_hint.max(stored)`.
// CONTRACT
#[allow(unused_variables)]
pub fn decode_columns(bytes: &[u8], wanted: &[usize], ncols_hint: usize) -> Result<Vec<Datum>> {
    Err(PgError::unimplemented("TYP-51", "row projection decoding"))
}

/// Append one value (tag + payload) — also used by `record_image_*` comparisons and `pg_column_size`.
// CONTRACT
#[allow(unused_variables)]
pub fn encode_value(value: &Datum, out: &mut Vec<u8>) -> Result<()> {
    Err(PgError::unimplemented("TYP-51", "value encoding"))
}
