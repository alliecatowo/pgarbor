//! Cast execution (TYPE-03). Lease TYP-53.
//!
//! `apply_cast(plan, value, ctx)` runs `plan.steps` in order:
//! - NULL input → NULL output (all steps skipped; domain NOT NULL is the executor's job).
//! - `Relabel`: identity, except that the Datum variant must match the target's mapping — the only
//!   relabel that changes variants is none in v1 (text/varchar/bpchar/name are all `Datum::Text`;
//!   oid/regclass/... are all `Datum::Oid`), so this is a no-op.
//! - `Func { proc, nargs }`: `func::implementation(proc)?.call(args)` with args `[value]`,
//!   `[value, Int4(target typmod)]` or `[value, Int4(typmod), Bool(explicit)]`, `FuncCtx::with_types`
//!   set to the cast function's declared arg types and the target result type.
//! - `ViaIo { from, to }`: `io::output(from, value)` then `io::input(to, text)` (target typmod applied
//!   by input). Errors are those of the input function (`'abc'::text::int4` → 22P02
//!   `invalid input syntax for type integer: "abc"`).
//! - `ArrayCoerce { elem_plan }`: apply `elem_plan` to every element, keep dims, set `elem` to the target
//!   element type.
//! - `RecordCoerce { to }`: field-wise via `plan_cast` of each field to the target field type
//!   (`ctx.env.composite_fields`); field count mismatch → 42846 `cannot cast type record to x`
//!   with DETAIL `Input has too few columns.` / `Input has too many columns.`.
//! - `Typmod { to, explicit }`: `typmod::enforce_typmod(ctx, to, value, explicit)`.
//! - `DomainCheck`: identity here.
use super::lookup::{plan_cast, CastPlan};
use crate::catalog_data::CastContext;
use crate::context::FuncCtx;
use crate::datum::{Datum, TypeId};
use pgarbor_common::{PgError, Result};

/// Execute a planned cast.
// CONTRACT
#[allow(unused_variables)]
pub fn apply_cast(plan: &CastPlan, value: Datum, ctx: &FuncCtx) -> Result<Datum> {
    Err(PgError::unimplemented("TYP-53", "cast execution"))
}

/// Plan + apply in one call (convenience for constant folding and tests).
pub fn cast_datum(value: Datum, from: TypeId, to: TypeId, context: CastContext, ctx: &FuncCtx) -> Result<Datum> {
    let plan = plan_cast(ctx.env, from, to, context)?;
    apply_cast(&plan, value, ctx)
}
