//! `int2` / `int4` / `int8` text and binary I/O (TYPE-01, TYPE-15). Lease TYP-04.
//!
//! PG18 reference: `src/backend/utils/adt/numutils.c` (`pg_strtoint16/32/64_safe`), `int.c`, `int8.c`.
//!
//! Text input rules (PG16+ `pg_strtointNN_safe`):
//! - Leading and trailing whitespace (space, \t, \n, \r, \v, \f) allowed; optional `+`/`-` sign.
//! - Decimal digits, OR prefixed non-decimal literals `0x`/`0X` (hex), `0o`/`0O` (octal),
//!   `0b`/`0B` (binary) — PG16+ accepts these in input: `'0x1A'::int4` = 26.
//! - Underscores between digits are allowed (`'1_000'::int4` = 1000) but not leading, trailing,
//!   doubled, or directly after the prefix... (PG: `_` must be followed by a digit and preceded by a digit).
//! - Empty / only whitespace / junk → 22P02 `invalid input syntax for type integer: "<input>"`
//!   (type names in messages: int2 → `smallint`, int4 → `integer`, int8 → `bigint`).
//! - Out of range → 22003 `value "<input>" is out of range for type integer` (resp. smallint/bigint).
//!   Example (oracle): `'99999999999'::int4` → `value "99999999999" is out of range for type integer`.
//! - The `<input>` in messages is the ORIGINAL string (untrimmed).
//!
//! Text output: plain decimal (`-123`). Binary: big-endian two's complement of width 2/4/8;
//! recv requires exactly that many bytes.
use crate::context::IoCtx;
use crate::datum::{Datum, TypeId};
use pgarbor_common::{PgError, Result};

/// Parse an int2 per the module rules (errors carry PG messages).
// CONTRACT
#[allow(unused_variables)]
pub fn parse_int2(s: &str) -> Result<i16> {
    Err(PgError::unimplemented("TYP-04", "smallint input"))
}

/// Parse an int4 per the module rules.
// CONTRACT
#[allow(unused_variables)]
pub fn parse_int4(s: &str) -> Result<i32> {
    Err(PgError::unimplemented("TYP-04", "integer input"))
}

/// Parse an int8 per the module rules.
// CONTRACT
#[allow(unused_variables)]
pub fn parse_int8(s: &str) -> Result<i64> {
    Err(PgError::unimplemented("TYP-04", "bigint input"))
}

/// `int2in` / `int4in` / `int8in` by `ty.oid` → `Datum::Int2/Int4/Int8`.
#[allow(unused_variables)]
pub fn input(ctx: &IoCtx, ty: TypeId, s: &str) -> Result<Datum> {
    Err(PgError::unimplemented("TYP-04", "integer input"))
}

/// `int2out` / `int4out` / `int8out`.
#[allow(unused_variables)]
pub fn output(ctx: &IoCtx, ty: TypeId, d: &Datum) -> Result<String> {
    Err(PgError::unimplemented("TYP-04", "integer output"))
}

/// `int2recv` / `int4recv` / `int8recv` (2/4/8 bytes big-endian).
#[allow(unused_variables)]
pub fn recv(ctx: &IoCtx, ty: TypeId, buf: &[u8]) -> Result<Datum> {
    Err(PgError::unimplemented("TYP-04", "integer binary input"))
}

/// `int2send` / `int4send` / `int8send`.
#[allow(unused_variables)]
pub fn send(ctx: &IoCtx, ty: TypeId, d: &Datum) -> Result<Vec<u8>> {
    Err(PgError::unimplemented("TYP-04", "integer binary output"))
}
