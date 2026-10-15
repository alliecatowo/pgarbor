//! Record / composite values: I/O and row comparison (TYPE-01, TYPE-15). Lease TYP-40.
//!
//! PG18 reference: `src/backend/utils/adt/rowtypes.c` (`record_in`, `record_out`, `record_recv`,
//! `record_send`, `record_cmp`, `record_eq`, `record_image_*`, `hash_record`).
//! Field types: for a named composite type `ty.oid` → `ctx.env.composite_fields(ty.oid)` (dropped
//! fields are stored as `Datum::Null` and skipped by I/O); for anonymous `record` (2249) the field types
//! cannot be known from the Datum: `output_record` takes them explicitly (the executor knows them),
//! and `input` of plain `record` → 0A000 `input of anonymous composite types is not implemented` (PG text).
//!
//! Text output: `(1,"a b",)` — fields separated by `,`, NULL = nothing, a field is double-quoted if it is
//! empty or contains `"` `\` `(` `)` `,` or whitespace; inside quotes `"` → `""` and `\` → `\\`.
//! Oracle: `row(1,'a b',null)` → `(1,"a b",)`.
//! Text input: `(f1,f2,...)`; unquoted field text is passed VERBATIM (whitespace included) to the field
//! type's input function (`'( 1 , x y )'` → a = 1 (int4in trims), b = `' x y '`); an empty unquoted
//! field is NULL, `""` is the empty string; inside quotes `""` is a quote and backslash escapes the next
//! char. Errors 22P02 `malformed record literal: "<input>"` with DETAIL `Too few columns.` /
//! `Too many columns.` / `Missing left parenthesis.` / `Junk after right parenthesis.` /
//! `Unexpected end of input.` (all oracle-verified).
//! Binary: `nfields: i32`, per non-dropped field `type_oid: u32, len: i32 (-1 NULL), bytes`.
//!
//! Owned procs (ImplModule::Record): `record_eq record_ne record_lt record_le record_gt record_ge
//! btrecordcmp record_larger record_smaller` (column-wise with the column types' default btree
//! ordering; NULLs sort after non-NULLs, NULL = NULL for ordering purposes; comparing records with
//! different column counts → 42804 `cannot compare record types with different numbers of columns`),
//! `record_image_*`/`btrecordimagecmp` (binary image comparison: compare `tuple::encode_value` bytes),
//! `hash_record(_extended)`. Field types come from `ctx.arg_types` (composite) — for anonymous records
//! the binder must supply a resolvable type; otherwise 42883 `could not identify an equality operator
//! for type unknown` (oracle text for `(1,null)::record = (1,null)::record`).
use crate::catalog_data::BuiltinProc;
use crate::context::IoCtx;
use crate::datum::{Datum, TypeId};
use crate::func::FuncImpl;
use pgarbor_common::{PgError, Result};

/// `record_out` with explicit field types (works for anonymous records).
// CONTRACT
#[allow(unused_variables)]
pub fn output_record(ctx: &IoCtx, field_types: &[TypeId], values: &[Datum]) -> Result<String> {
    Err(PgError::unimplemented("TYP-40", "record output"))
}

/// `record_in` for a named composite type.
#[allow(unused_variables)]
pub fn input(ctx: &IoCtx, ty: TypeId, s: &str) -> Result<Datum> {
    Err(PgError::unimplemented("TYP-40", "record input"))
}

/// `record_out` for a named composite type (fields from `ctx.env`).
#[allow(unused_variables)]
pub fn output(ctx: &IoCtx, ty: TypeId, d: &Datum) -> Result<String> {
    Err(PgError::unimplemented("TYP-40", "record output"))
}

/// `record_recv`.
#[allow(unused_variables)]
pub fn recv(ctx: &IoCtx, ty: TypeId, buf: &[u8]) -> Result<Datum> {
    Err(PgError::unimplemented("TYP-40", "record binary input"))
}

/// `record_send`.
#[allow(unused_variables)]
pub fn send(ctx: &IoCtx, ty: TypeId, d: &Datum) -> Result<Vec<u8>> {
    Err(PgError::unimplemented("TYP-40", "record binary output"))
}

/// Function registry hook (see `crate::func`).
#[allow(unused_variables)]
pub(crate) fn lookup(proc: &BuiltinProc) -> Option<FuncImpl> {
    None
}

/// 0A000 for a Record-module proc without implementation.
pub(crate) fn unsupported(proc: &BuiltinProc) -> PgError {
    PgError::unimplemented("TYP-40", format!("function {}", proc.name))
}
