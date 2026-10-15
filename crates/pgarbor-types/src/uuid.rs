//! `uuid` I/O and functions (TYPE-07). Lease TYP-20.
//!
//! PG18 reference: `utils/adt/uuid.c` (`string_to_uuid`, `uuid_out`, `gen_random_uuid`, `uuidv7`,
//! `uuid_extract_version`, `uuid_extract_timestamp`).
//!
//! Text input (`string_to_uuid`): optional surrounding `{` `}`; 32 hex digits (either case); a `-`
//! may follow ANY group of 4 hex digits (after digit 4, 8, 12, ... — so `a0ee-bc99-9c0b-4ef8-bb6d-6bb9-bd38-0a11`
//! and `a0eebc99-9c0b4ef8-bb6d-6bb9bd380a11` are valid); no other characters, no surrounding whitespace.
//! Error → 22P02 `invalid input syntax for type uuid: "<input>"`.
//! Output: lowercase `8-4-4-4-12`. Binary: 16 raw bytes (recv: exactly 16, else 22P03).
//!
//! Owned procs (ImplModule::Uuid): `uuid_eq uuid_ne uuid_lt uuid_le uuid_gt uuid_ge uuid_cmp`
//! (memcmp order), `uuid_hash(_extended)`, `gen_random_uuid` and `uuidv4` (16 bytes from
//! `ctx.session.fill_random`, then set version nibble 4 (byte 6 = (b & 0x0f) | 0x40) and RFC 4122
//! variant (byte 8 = (b & 0x3f) | 0x80)), `uuidv7` / `uuidv7_interval` (PG18: 48-bit big-endian Unix ms
//! from `ctx.session.clock_timestamp()` (+ interval shift), 12-bit sub-millisecond fraction in
//! `rand_a` per PG's method, version 7, variant, random tail), `uuid_extract_version` (NULL unless RFC
//! variant), `uuid_extract_timestamp` (v1 and v7 only, else NULL; returns timestamptz).
use crate::catalog_data::BuiltinProc;
use crate::context::IoCtx;
use crate::datum::{Datum, TypeId};
use crate::func::FuncImpl;
use pgarbor_common::{PgError, Result};

/// Parse uuid text per module rules.
// CONTRACT
#[allow(unused_variables)]
pub fn parse_uuid(s: &str) -> Result<[u8; 16]> {
    Err(PgError::unimplemented("TYP-20", "uuid input"))
}

/// Canonical lowercase hyphenated form.
pub fn format_uuid(u: &[u8; 16]) -> String {
    let h: String = u.iter().map(|b| format!("{b:02x}")).collect();
    format!("{}-{}-{}-{}-{}", &h[0..8], &h[8..12], &h[12..16], &h[16..20], &h[20..32])
}

/// `uuid_in`.
#[allow(unused_variables)]
pub fn input(ctx: &IoCtx, ty: TypeId, s: &str) -> Result<Datum> {
    Err(PgError::unimplemented("TYP-20", "uuid input"))
}

/// `uuid_out`.
#[allow(unused_variables)]
pub fn output(ctx: &IoCtx, ty: TypeId, d: &Datum) -> Result<String> {
    Err(PgError::unimplemented("TYP-20", "uuid output"))
}

/// `uuid_recv`.
#[allow(unused_variables)]
pub fn recv(ctx: &IoCtx, ty: TypeId, buf: &[u8]) -> Result<Datum> {
    Err(PgError::unimplemented("TYP-20", "uuid binary input"))
}

/// `uuid_send`.
#[allow(unused_variables)]
pub fn send(ctx: &IoCtx, ty: TypeId, d: &Datum) -> Result<Vec<u8>> {
    Err(PgError::unimplemented("TYP-20", "uuid binary output"))
}

/// Function registry hook (see `crate::func`).
#[allow(unused_variables)]
pub(crate) fn lookup(proc: &BuiltinProc) -> Option<FuncImpl> {
    None
}

/// 0A000 for a Uuid-module proc without implementation.
pub(crate) fn unsupported(proc: &BuiltinProc) -> PgError {
    PgError::unimplemented("TYP-20", format!("function {}", proc.name))
}
