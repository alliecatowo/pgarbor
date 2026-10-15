//! User-defined enum types: I/O, ordering, enum support functions (TYPE-10). Lease TYP-41.
//!
//! PG18 reference: `src/backend/utils/adt/enum.c`.
//! Representation: `Datum::Text(label)`; labels and sort order from `ctx.env.enum_labels(ty.oid)`
//! (sorted by `sort_order`). Ordering is by `sort_order`, NOT by label text or OID.
//!
//! I/O: input must exactly match a label (case-sensitive), else 22P02
//! `invalid input value for enum mood: "meh"` (type name as `format_type` prints it); output = label.
//! Binary: the label's UTF-8 bytes (recv validates like input).
//! Owned procs (ImplModule::Enum): `enum_eq enum_ne enum_lt enum_le enum_gt enum_ge enum_cmp enum_larger
//! enum_smaller hashenum(_extended)` (enum type from `ctx.arg_types[0]`), `enum_first enum_last`
//! (arg is a NULL of the enum type; type from `ctx.arg_types[0]`), `enum_range_all`
//! (`enum_range(null::mood)` = `{sad,ok,happy}`), `enum_range_bounds` (`enum_range('ok'::mood, null)` =
//! `{ok,happy}`; NULL bound = open).
use crate::catalog_data::BuiltinProc;
use crate::context::{IoCtx, TypeEnv};
use crate::datum::{Datum, TypeId};
use crate::func::FuncImpl;
use pgarbor_common::{Oid, PgError, Result};
use std::cmp::Ordering;

/// Compare two labels of enum type `enum_oid` by sort order. Unknown label → 22P02 (input-style error)
/// or XX000 if the type is not an enum.
// CONTRACT
#[allow(unused_variables)]
pub fn cmp_labels(env: &dyn TypeEnv, enum_oid: Oid, a: &str, b: &str) -> Result<Ordering> {
    Err(PgError::unimplemented("TYP-41", "enum comparison"))
}

/// Sort-order key of a label (used by `keycodec`).
// CONTRACT
#[allow(unused_variables)]
pub fn label_sort_order(env: &dyn TypeEnv, enum_oid: Oid, label: &str) -> Result<f32> {
    Err(PgError::unimplemented("TYP-41", "enum sort order"))
}

/// `enum_in`.
#[allow(unused_variables)]
pub fn input(ctx: &IoCtx, ty: TypeId, s: &str) -> Result<Datum> {
    Err(PgError::unimplemented("TYP-41", "enum input"))
}

/// `enum_out`.
#[allow(unused_variables)]
pub fn output(ctx: &IoCtx, ty: TypeId, d: &Datum) -> Result<String> {
    Err(PgError::unimplemented("TYP-41", "enum output"))
}

/// `enum_recv`.
#[allow(unused_variables)]
pub fn recv(ctx: &IoCtx, ty: TypeId, buf: &[u8]) -> Result<Datum> {
    Err(PgError::unimplemented("TYP-41", "enum binary input"))
}

/// `enum_send`.
#[allow(unused_variables)]
pub fn send(ctx: &IoCtx, ty: TypeId, d: &Datum) -> Result<Vec<u8>> {
    Err(PgError::unimplemented("TYP-41", "enum binary output"))
}

/// Function registry hook (see `crate::func`).
#[allow(unused_variables)]
pub(crate) fn lookup(proc: &BuiltinProc) -> Option<FuncImpl> {
    None
}

/// 0A000 for an Enum-module proc without implementation.
pub(crate) fn unsupported(proc: &BuiltinProc) -> PgError {
    PgError::unimplemented("TYP-41", format!("function {}", proc.name))
}
