//! `bytea` I/O and functions (TYPE-07). Lease TYP-18.
//!
//! PG18 reference: `utils/adt/varlena.c` (`byteain`, `byteaout`, `bytearecv`, `byteasend`, bytea
//! functions), `utils/adt/encode.c` (`hex_encode/decode`, base64, escape formats).
//!
//! Text input (`byteain`):
//! - Hex format if the input starts with `\x` (or `\X`): pairs of hex digits (either case), whitespace
//!   allowed BETWEEN pairs; odd digit count → 22023 `invalid hexadecimal data: odd number of digits`;
//!   bad digit → 22023 `invalid hexadecimal digit: "Z"` (oracle: `'\xZZ'::bytea`).
//! - Otherwise escape format: bytes as-is, `\\` → backslash, `\ooo` (3 octal digits, first 0-3) → byte;
//!   any other backslash use → 22P02 `invalid input syntax for type bytea` (no value in the message).
//!   Non-ASCII characters are taken as their UTF-8 bytes (`'café'::bytea` = `\x636166c3a9`).
//! Text output: `bytea_output = hex` → `\x` + lowercase hex (`\x0102`; empty → `\x`);
//! `escape` → printable ASCII 0x20..=0x7E as-is except `\` → `\\`, others as `\ooo`.
//! Binary: raw bytes.
//!
//! Owned procs (ImplModule::Bytea): `byteaeq byteane bytealt byteale byteagt byteage byteacmp
//! bytea_larger bytea_smaller hashbytea(+extended)` (memcmp order, shorter prefix first), `byteacat` (`||`),
//! `byteaoctetlen` (length), SQL-language `bit_length(bytea)` (oid 1810), `bytea_substr(_no_len)`,
//! `byteaoverlay(_no_len)`, `byteapos`, `byteatrim bytealtrim byteartrim`, `byteaGetByte byteaSetByte
//! byteaGetBit byteaSetBit` (index out of range → 2202E `index %d out of valid range, 0..%d`),
//! `bytea_bit_count`, `bytea_reverse`, `binary_encode` (`encode(bytea, 'hex'|'base64'|'escape')`),
//! `binary_decode` (`decode(text, fmt)`; unknown format → 22023 `unrecognized encoding: "x"`;
//! base64 output wraps lines at 76 chars with `\n` exactly as PG), `int2_bytea int4_bytea int8_bytea`
//! (big-endian bytes) and `bytea_int2 bytea_int4 bytea_int8` (PG18 casts; too long → 22003
//! `smallint out of range` etc.).
use crate::catalog_data::BuiltinProc;
use crate::context::IoCtx;
use crate::datum::{Datum, TypeId};
use crate::func::FuncImpl;
use pgarbor_common::{PgError, Result};

/// Parse bytea text (hex or escape format).
// CONTRACT
#[allow(unused_variables)]
pub fn parse_bytea(s: &str) -> Result<Vec<u8>> {
    Err(PgError::unimplemented("TYP-18", "bytea input"))
}

/// Format bytea per `bytea_output`.
// CONTRACT
#[allow(unused_variables)]
pub fn format_bytea(b: &[u8], hex: bool) -> Result<String> {
    Err(PgError::unimplemented("TYP-18", "bytea output"))
}

/// `byteain`.
#[allow(unused_variables)]
pub fn input(ctx: &IoCtx, ty: TypeId, s: &str) -> Result<Datum> {
    Err(PgError::unimplemented("TYP-18", "bytea input"))
}

/// `byteaout` (uses `ctx.config.bytea_output`).
#[allow(unused_variables)]
pub fn output(ctx: &IoCtx, ty: TypeId, d: &Datum) -> Result<String> {
    Err(PgError::unimplemented("TYP-18", "bytea output"))
}

/// `bytearecv`.
#[allow(unused_variables)]
pub fn recv(ctx: &IoCtx, ty: TypeId, buf: &[u8]) -> Result<Datum> {
    Err(PgError::unimplemented("TYP-18", "bytea binary input"))
}

/// `byteasend`.
#[allow(unused_variables)]
pub fn send(ctx: &IoCtx, ty: TypeId, d: &Datum) -> Result<Vec<u8>> {
    Err(PgError::unimplemented("TYP-18", "bytea binary output"))
}

/// Function registry hook (see `crate::func`).
#[allow(unused_variables)]
pub(crate) fn lookup(proc: &BuiltinProc) -> Option<FuncImpl> {
    None
}

/// 0A000 for a Bytea-module proc without implementation.
pub(crate) fn unsupported(proc: &BuiltinProc) -> PgError {
    PgError::unimplemented("TYP-18", format!("function {}", proc.name))
}
