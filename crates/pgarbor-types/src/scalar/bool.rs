//! `bool` I/O and boolean functions (TYPE-01, TYPE-04). Lease TYP-03.
//!
//! PG18 reference: `src/backend/utils/adt/bool.c`.
//!
//! Text input (`boolin`/`parse_bool_with_len`): leading/trailing whitespace (isspace: space,
//! \t, \n, \r, \v, \f) is trimmed; then case-insensitive match of any unambiguous prefix of
//! `true`/`false`/`yes`/`no` (`t`, `tr`, `tru`, `true`, `y`, `ye`, `yes`, `f`, `fa`..., `n`, `no`),
//! or `on`, `off` (at least 2 chars: `of`, `off`; `o` alone is ambiguous → error), or `1`/`0`.
//! Anything else → 22P02 `invalid input syntax for type boolean: "<input as given>"`.
//! Text output: `t` / `f`. Binary: 1 byte, 0x01 / 0x00 (recv: any non-zero byte is true;
//! wrong length → 22P03 via `io::invalid_binary`).
//!
//! Functions owned by this module (ImplModule::Bool, matched by `proc.prosrc`):
//! `booleq boolne boollt boolgt boolle boolge` (false < true), `btboolcmp` (int4 -1/0/1),
//! `booland_statefunc`/`boolor_statefunc` (plain AND/OR of two non-null bools),
//! `booltext` (bool → text 'true'/'false' — NOTE the cast to text yields the long words, unlike output),
//! `int4_bool` (0 → false, else true), `bool_int4` (false → 0, true → 1),
//! `hashbool`, `hashboolextended` (PG `hash_uint32` of 0/1, see `compare::pg_hash_uint32`).
use crate::catalog_data::BuiltinProc;
use crate::context::IoCtx;
use crate::datum::{Datum, TypeId};
use crate::func::FuncImpl;
use pgarbor_common::{PgError, Result};

/// Parse PG boolean text (see module docs). Shared by `boolin`, GUC parsing and casts from text.
/// Returns `None` for invalid input (callers build the error).
// CONTRACT
#[allow(unused_variables)]
pub fn parse_bool(s: &str) -> Result<Option<bool>> {
    Err(PgError::unimplemented("TYP-03", "boolean text parsing"))
}

/// `boolin`. Example: `input(ctx, TypeId::BOOL, " Yes ")` → `Datum::Bool(true)`.
#[allow(unused_variables)]
pub fn input(ctx: &IoCtx, ty: TypeId, s: &str) -> Result<Datum> {
    Err(PgError::unimplemented("TYP-03", "boolean input"))
}

/// `boolout`: `t` / `f`.
#[allow(unused_variables)]
pub fn output(ctx: &IoCtx, ty: TypeId, d: &Datum) -> Result<String> {
    Err(PgError::unimplemented("TYP-03", "boolean output"))
}

/// `boolrecv`: exactly 1 byte.
#[allow(unused_variables)]
pub fn recv(ctx: &IoCtx, ty: TypeId, buf: &[u8]) -> Result<Datum> {
    Err(PgError::unimplemented("TYP-03", "boolean binary input"))
}

/// `boolsend`: `[1]` / `[0]`.
#[allow(unused_variables)]
pub fn send(ctx: &IoCtx, ty: TypeId, d: &Datum) -> Result<Vec<u8>> {
    Err(PgError::unimplemented("TYP-03", "boolean binary output"))
}

/// Function registry hook: map `proc.prosrc` to an implementation (see `crate::func`).
#[allow(unused_variables)]
pub(crate) fn lookup(proc: &BuiltinProc) -> Option<FuncImpl> {
    None
}

/// 0A000 for a Bool-module proc without implementation.
pub(crate) fn unsupported(proc: &BuiltinProc) -> PgError {
    PgError::unimplemented("TYP-03", format!("function {}", proc.name))
}
