//! Type families and builtin procs PGArbor does not implement yet (TYPE-01 follow-ups). Lease TYP-42.
//!
//! Covers I/O for every type whose `typinput` is not routed by `crate::io` (geometric types, inet/cidr/
//! macaddr, money, bit/varbit, ranges/multiranges, tsvector/tsquery, xml, jsonpath, txid_snapshot,
//! pg_snapshot, aclitem, refcursor, pseudo types like anyarray/anyelement/cstring-as-output...) and
//! the procs classified `ImplModule::Unsupported` / `Window` / `External` that reach the scalar path.
//!
//! Every function here reports 0A000 via `PgError::unimplemented("TYP-42", ...)` naming the type or
//! function (e.g. `type point is not supported by PGArbor yet`). Most of these need new `Datum`
//! variants: see amendment request `contracts/amendments/pgarbor-types-03-datum-variants.md`. A worker
//! on this lease may implement families that fit the frozen `Datum` without an amendment:
//! `money` (Datum::Int8 cents; output `$1,234.56` per lc_monetary=C → `$1,234.56`), `aclitem`
//! (Datum::Text, canonical text in/out), `refcursor` (Datum::Text), `pseudo` outputs (`anyarray_out` =
//! array_out, `anyelement_out` = output of the actual type when known).
use crate::catalog_data::{BuiltinAggregate, BuiltinProc};
use crate::context::IoCtx;
use crate::datum::{Datum, TypeId};
use crate::func::FuncImpl;
use crate::registry;
use pgarbor_common::{PgError, Result};

fn type_name(ctx: &IoCtx, ty: TypeId) -> String {
    registry::type_info(ctx.env, ty.oid).map(|t| t.name.into_owned()).unwrap_or_else(|_| ty.oid.to_string())
}

/// Text input for unsupported types.
pub fn input(ctx: &IoCtx, ty: TypeId, _s: &str) -> Result<Datum> {
    Err(PgError::unimplemented("TYP-42", format!("type {}", type_name(ctx, ty))))
}

/// Text output for unsupported types.
pub fn output(ctx: &IoCtx, ty: TypeId, _d: &Datum) -> Result<String> {
    Err(PgError::unimplemented("TYP-42", format!("type {}", type_name(ctx, ty))))
}

/// Binary input for unsupported types.
pub fn recv(ctx: &IoCtx, ty: TypeId, _buf: &[u8]) -> Result<Datum> {
    Err(PgError::unimplemented("TYP-42", format!("type {}", type_name(ctx, ty))))
}

/// Binary output for unsupported types.
pub fn send(ctx: &IoCtx, ty: TypeId, _d: &Datum) -> Result<Vec<u8>> {
    Err(PgError::unimplemented("TYP-42", format!("type {}", type_name(ctx, ty))))
}

/// Function registry hook for `ImplModule::Unsupported` procs (none implemented).
#[allow(unused_variables)]
pub(crate) fn lookup(proc: &BuiltinProc) -> Option<FuncImpl> {
    None
}

/// 0A000 for unsupported procs.
pub(crate) fn unsupported(proc: &BuiltinProc) -> PgError {
    PgError::unimplemented("TYP-42", format!("function {}", proc.name))
}

/// 0A000 for unsupported aggregates (xmlagg, range_agg, range_intersect_agg).
pub(crate) fn unsupported_aggregate(agg: &BuiltinAggregate) -> PgError {
    PgError::unimplemented("TYP-42", format!("aggregate {}", agg.name))
}
