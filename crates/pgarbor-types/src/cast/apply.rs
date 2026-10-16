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
use super::lookup::{plan_cast, CastPlan, CastStep};
use crate::array::Array;
use crate::catalog_data::CastContext;
use crate::context::{CompositeField, FuncCtx};
use crate::datum::{type_mismatch, Datum, TypeId};
use crate::func;
use crate::io;
use crate::names::display_name;
use crate::registry::base_type;
use crate::oids;
use crate::typmod;
use pgarbor_common::{Oid, PgError, Result, SqlState};

/// Execute a planned cast.
// CONTRACT
pub fn apply_cast(plan: &CastPlan, value: Datum, ctx: &FuncCtx) -> Result<Datum> {
    if value.is_null() {
        return Ok(Datum::Null);
    }
    let mut value = value;
    for step in &plan.steps {
        if value.is_null() {
            // Every cast step is strict with respect to NULL; a NULL produced mid-plan stays NULL.
            break;
        }
        value = apply_step(plan, step, value, ctx)?;
    }
    Ok(value)
}

/// Plan + apply in one call (convenience for constant folding and tests).
pub fn cast_datum(value: Datum, from: TypeId, to: TypeId, context: CastContext, ctx: &FuncCtx) -> Result<Datum> {
    let plan = plan_cast(ctx.env, from, to, context)?;
    apply_cast(&plan, value, ctx)
}

/// Binary-coercible (relabel) casts keep the bits but may change the Datum variant: `int4 -> oid`,
/// `oid -> int4` and the `reg*` family (pg_cast castmethod 'b'). Every other relabel keeps its variant.
fn relabel_variant(target: Oid, value: Datum) -> Datum {
    match value {
        Datum::Int4(v) if OID_DATUM_TYPES.contains(&target) => Datum::Oid(v as u32),
        Datum::Oid(v) if target == oids::INT4 => Datum::Int4(v as i32),
        other => other,
    }
}

/// Types whose Datum variant is `Datum::Oid` (see the `datum` module table).
const OID_DATUM_TYPES: &[Oid] = &[
    oids::OID,
    24,   // regproc
    2202, // regprocedure
    2203, // regoper
    2204, // regoperator
    2205, // regclass
    2206, // regtype
    3734, // regconfig
    3769, // regdictionary
    4089, // regnamespace
    4096, // regrole
    4191, // regcollation
    28,   // xid
    29,   // cid
];

/// Run one step of `plan` on a non-NULL value.
fn apply_step(plan: &CastPlan, step: &CastStep, value: Datum, ctx: &FuncCtx) -> Result<Datum> {
    match step {
        CastStep::Relabel => Ok(relabel_variant(base_type(ctx.env, plan.to.oid), value)),
        CastStep::DomainCheck { .. } => Ok(value),
        CastStep::Func { proc, nargs } => apply_func(plan, *proc, *nargs, value, ctx),
        CastStep::ViaIo { from, to } => {
            let text = io::output(&ctx.io_ctx(), *from, &value)?;
            io::input(&ctx.io_ctx(), *to, &text)
        }
        CastStep::ArrayCoerce { elem_plan } => apply_array(elem_plan, value, ctx),
        CastStep::RecordCoerce { to } => apply_record(plan, *to, value, ctx),
        CastStep::Typmod { to, explicit } => typmod::enforce_typmod(ctx, *to, value, *explicit),
    }
}

/// `Func` step: call the cast function (PG `cast function`, or a length-coercion function when the
/// target carries a typmod).
fn apply_func(plan: &CastPlan, proc_oid: Oid, nargs: u8, value: Datum, ctx: &FuncCtx) -> Result<Datum> {
    let handle = func::implementation(proc_oid)?;
    let nargs = nargs as usize;
    // Declared argument types of the cast function; positions beyond the catalog row are the
    // typmod (int4) and explicit (bool) arguments that PG passes to length-coercion functions.
    let declared: Vec<TypeId> = (0..nargs)
        .map(|i| {
            let oid = match handle.proc.arg_types.get(i) {
                Some(o) => *o,
                None => match i {
                    0 => plan.from.oid,
                    1 => oids::INT4,
                    _ => oids::BOOL,
                },
            };
            TypeId::of(oid)
        })
        .collect();
    let fctx = ctx.with_types(&declared, plan.to);
    match nargs {
        0 | 1 => handle.call(std::slice::from_ref(&value), &fctx),
        2 => handle.call(&[value, Datum::Int4(plan.to.typmod)], &fctx),
        _ => {
            let explicit = plan.context == CastContext::Explicit;
            handle.call(&[value, Datum::Int4(plan.to.typmod), Datum::Bool(explicit)], &fctx)
        }
    }
}

/// `ArrayCoerce` step: element-wise cast, dimensions kept, element type set to the target's.
fn apply_array(elem_plan: &CastPlan, value: Datum, ctx: &FuncCtx) -> Result<Datum> {
    let arr = match value {
        Datum::Array(a) => *a,
        other => return Err(type_mismatch("Array", &other)),
    };
    let Array { dims, items: src, .. } = arr;
    let mut items = Vec::with_capacity(src.len());
    for item in src {
        items.push(apply_cast(elem_plan, item, ctx)?);
    }
    Ok(Datum::array(Array { elem: elem_plan.to.oid, dims, items }))
}

/// `RecordCoerce` step: field-wise coercion of a composite (or anonymous record) value.
///
/// Field values are indexed by attribute position, dropped attributes included. Dropped target
/// attributes come out as NULL.
fn apply_record(plan: &CastPlan, to: TypeId, value: Datum, ctx: &FuncCtx) -> Result<Datum> {
    let values = match value {
        Datum::Record(v) => v,
        other => return Err(type_mismatch("Record", &other)),
    };
    if to.oid == oids::RECORD {
        // Datum::Record does not retain a tuple descriptor for an anonymous result.
        return Err(PgError::unimplemented("TYP-53", "coercion to anonymous record without a tuple descriptor"));
    }
    let to_fields = ctx.env.composite_fields(to.oid).ok_or_else(|| {
        PgError::internal(format!("composite fields unknown for type {}", to.oid))
    })?;
    let from_fields = match ctx.env.composite_fields(plan.from.oid) {
        Some(f) => f,
        None if plan.from.oid == oids::RECORD => {
            // Field types of an anonymous record are not carried by the value or the plan, so the
            // per-field coercions cannot be chosen here.
            return Err(PgError::unimplemented(
                "TYP-53",
                "record coercion from an anonymous record value",
            ));
        }
        None => {
            return Err(PgError::internal(format!(
                "composite fields unknown for type {}",
                plan.from.oid
            )))
        }
    };
    if values.len() != from_fields.len() {
        return Err(PgError::corruption(format!(
            "record value has {} fields but type {} has {} attributes",
            values.len(),
            plan.from.oid,
            from_fields.len()
        )));
    }

    // Column-count check first (PG ExecEvalConvertRowtype), so no field is coerced for a bad row.
    let target_live = to_fields.iter().filter(|f| !f.dropped).count();
    let source_live = from_fields.iter().filter(|f| !f.dropped).count();
    if source_live < target_live {
        return Err(record_cast_error(ctx, plan.from, to, "Input has too few columns."));
    }
    if source_live > target_live {
        return Err(record_cast_error(ctx, plan.from, to, "Input has too many columns."));
    }

    let live_source: Vec<(&CompositeField, Datum)> = from_fields
        .iter()
        .zip(values)
        .filter(|(f, _)| !f.dropped)
        .collect();
    let mut live_source = live_source.into_iter();

    let mut out: Vec<Datum> = Vec::with_capacity(to_fields.len());
    for tf in to_fields.iter() {
        if tf.dropped {
            out.push(Datum::Null);
            continue;
        }
        // The counts were checked above, so a live target attribute always has a live source.
        let (sf, sv) = live_source.next().ok_or_else(|| {
            PgError::internal("record field count changed during coercion")
        })?;
        let field_plan = plan_cast(ctx.env, sf.type_id, tf.type_id, plan.context)?;
        out.push(apply_cast(&field_plan, sv, ctx)?);
    }
    Ok(Datum::Record(out))
}

/// 42846 `cannot cast type <from> to <to>` with the column-count DETAIL.
fn record_cast_error(ctx: &FuncCtx, from: TypeId, to: TypeId, detail: &str) -> PgError {
    PgError::new(
        SqlState::CANNOT_COERCE,
        format!("cannot cast type {} to {}", display_name(ctx.env, from.oid), display_name(ctx.env, to.oid)),
    )
    .with_detail(detail)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::context::{IoConfig, TestSession};
    use crate::registry::builtin_env;

    /// Run `f` with a fresh call context over the builtin type environment.
    fn with_ctx<R>(f: impl FnOnce(&FuncCtx) -> R) -> R {
        let cfg = IoConfig::default();
        let session = TestSession::default();
        let ctx = FuncCtx {
            env: builtin_env(),
            io: &cfg,
            session: &session,
            arg_types: &[],
            result_type: TypeId::UNKNOWN,
            collation: 0,
        };
        f(&ctx)
    }

    #[test]
    fn int4_to_int8_widens() {
        with_ctx(|ctx| {
            let plan = plan_cast(ctx.env, TypeId::INT4, TypeId::INT8, CastContext::Implicit).expect("plan");
            assert_eq!(apply_cast(&plan, Datum::Int4(5), ctx).expect("apply"), Datum::Int8(5));
        });
    }

    #[test]
    fn null_stays_null_through_every_step() {
        with_ctx(|ctx| {
            let plan = plan_cast(ctx.env, TypeId::INT4, TypeId::INT8, CastContext::Implicit).expect("plan");
            assert_eq!(apply_cast(&plan, Datum::Null, ctx).expect("apply"), Datum::Null);
            let plan = plan_cast(ctx.env, TypeId::TEXT, TypeId::INT4, CastContext::Explicit).expect("plan");
            assert_eq!(apply_cast(&plan, Datum::Null, ctx).expect("apply"), Datum::Null);
        });
    }

    #[test]
    fn text_to_int4_bad_input_is_22p02() {
        with_ctx(|ctx| {
            let plan = plan_cast(ctx.env, TypeId::TEXT, TypeId::INT4, CastContext::Explicit).expect("plan");
            let err = apply_cast(&plan, Datum::text("abc"), ctx).expect_err("must fail");
            assert_eq!(err.sqlstate, SqlState::INVALID_TEXT_REPRESENTATION);
            assert_eq!(err.message, "invalid input syntax for type integer: \"abc\"");
        });
    }

    #[test]
    fn int4_array_to_text_array_is_elementwise() {
        with_ctx(|ctx| {
            let input = Datum::array(Array::from_vec(
                oids::INT4,
                vec![Datum::Int4(1), Datum::Null, Datum::Int4(3)],
            ));
            let plan = plan_cast(
                ctx.env,
                TypeId::of(oids::INT4_ARRAY),
                TypeId::of(oids::TEXT_ARRAY),
                CastContext::Explicit,
            )
            .expect("plan");
            let out = apply_cast(&plan, input, ctx).expect("apply");
            let expected = Datum::array(Array {
                elem: oids::TEXT,
                dims: vec![(3, 1)],
                items: vec![Datum::text("1"), Datum::Null, Datum::text("3")],
            });
            assert_eq!(out, expected);
        });
    }

    #[test]
    fn explicit_text_to_varchar3_truncates() {
        with_ctx(|ctx| {
            // varchar(3) typmod encoding: n + 4.
            let target = TypeId::new(oids::VARCHAR, 3 + 4);
            let plan = plan_cast(ctx.env, TypeId::TEXT, target, CastContext::Explicit).expect("plan");
            assert_eq!(apply_cast(&plan, Datum::text("abcdef"), ctx).expect("apply"), Datum::text("abc"));
        });
    }

    #[test]
    fn assignment_text_to_varchar3_errors_22001() {
        with_ctx(|ctx| {
            let target = TypeId::new(oids::VARCHAR, 3 + 4);
            let plan = plan_cast(ctx.env, TypeId::TEXT, target, CastContext::Assignment).expect("plan");
            let err = apply_cast(&plan, Datum::text("abcdef"), ctx).expect_err("must fail");
            assert_eq!(err.sqlstate, SqlState::STRING_DATA_RIGHT_TRUNCATION);
        });
    }

    #[test]
    fn noop_plan_returns_value_unchanged() {
        with_ctx(|ctx| {
            let plan = plan_cast(ctx.env, TypeId::TEXT, TypeId::TEXT, CastContext::Explicit).expect("plan");
            assert!(plan.is_noop());
            assert_eq!(apply_cast(&plan, Datum::text("x"), ctx).expect("apply"), Datum::text("x"));
        });
    }
}
