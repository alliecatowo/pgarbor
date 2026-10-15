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
//!
//! Deviations from PG that follow from the representation: the values are labels, not pg_enum OIDs,
//! so `enum_eq`/`enum_ne` compare label text (labels are unique within an enum type, so this is the
//! same answer as comparing OIDs). The "unsafe use of a not-yet-committed enum value" check (55P04)
//! is not modelled because enum labels in this engine are always committed.
use crate::array::Array;
use crate::catalog_data::BuiltinProc;
use crate::compare;
use crate::context::{EnumLabel, FuncCtx, IoCtx, TypeEnv};
use crate::datum::{Datum, TypeId};
use crate::func::{FuncImpl, ScalarFn};
use crate::names;
use pgarbor_common::{Oid, PgError, Result, SqlState};
use std::cmp::Ordering;
use std::sync::Arc;

/// Labels of enum type `enum_oid` in sort order. XX000 when the type is not an enum.
fn labels_of(env: &dyn TypeEnv, enum_oid: Oid) -> Result<Arc<[EnumLabel]>> {
    match env.enum_labels(enum_oid) {
        Some(labels) => Ok(labels),
        None => Err(PgError::internal(format!(
            "type {} is not an enum",
            names::format_type(env, enum_oid, -1)?
        ))),
    }
}

/// Position-independent lookup of a label by its text (case-sensitive).
fn find_label<'a>(labels: &'a [EnumLabel], label: &str) -> Option<&'a EnumLabel> {
    labels.iter().find(|l| l.label == label)
}

/// The 22P02 error of `enum_in` for a string that is not a label of the enum.
fn invalid_label(env: &dyn TypeEnv, enum_oid: Oid, label: &str) -> Result<PgError> {
    Ok(PgError::new(
        SqlState::INVALID_TEXT_REPRESENTATION,
        format!("invalid input value for enum {}: \"{}\"", names::format_type(env, enum_oid, -1)?, label),
    ))
}

/// Label lookup that reports the `enum_in` error when the label is not a member of the enum.
fn lookup_label<'a>(
    env: &dyn TypeEnv,
    enum_oid: Oid,
    labels: &'a [EnumLabel],
    label: &str,
) -> Result<&'a EnumLabel> {
    match find_label(labels, label) {
        Some(found) => Ok(found),
        None => Err(invalid_label(env, enum_oid, label)?),
    }
}

/// Sort-order comparison (`<` / `>` semantics of PG's `compare_values_of_enum`).
fn sort_cmp(a: f32, b: f32) -> Ordering {
    a.partial_cmp(&b).unwrap_or(Ordering::Equal)
}

/// Compare two labels of enum type `enum_oid` by sort order. Unknown label → 22P02 (input-style error)
/// or XX000 if the type is not an enum.
// CONTRACT
pub fn cmp_labels(env: &dyn TypeEnv, enum_oid: Oid, a: &str, b: &str) -> Result<Ordering> {
    let labels = labels_of(env, enum_oid)?;
    let la = lookup_label(env, enum_oid, &labels, a)?;
    let lb = lookup_label(env, enum_oid, &labels, b)?;
    Ok(sort_cmp(la.sort_order, lb.sort_order))
}

/// Sort-order key of a label (used by `keycodec`).
// CONTRACT
pub fn label_sort_order(env: &dyn TypeEnv, enum_oid: Oid, label: &str) -> Result<f32> {
    let labels = labels_of(env, enum_oid)?;
    Ok(lookup_label(env, enum_oid, &labels, label)?.sort_order)
}

/// Label text of an enum-typed datum.
fn label_of(d: &Datum) -> Result<&str> {
    match d {
        Datum::Text(s) => Ok(s.as_str()),
        other => Err(PgError::internal(format!("enum value is not a label: {other:?}"))),
    }
}

/// `enum_in`.
// CONTRACT
pub fn input(ctx: &IoCtx, ty: TypeId, s: &str) -> Result<Datum> {
    let labels = labels_of(ctx.env, ty.oid)?;
    let found = lookup_label(ctx.env, ty.oid, &labels, s)?;
    Ok(Datum::Text(found.label.clone()))
}

/// `enum_out`.
// CONTRACT
pub fn output(ctx: &IoCtx, ty: TypeId, d: &Datum) -> Result<String> {
    let s = label_of(d)?;
    let labels = labels_of(ctx.env, ty.oid)?;
    match find_label(&labels, s) {
        Some(_) => Ok(s.to_owned()),
        None => Err(PgError::new(
            SqlState::INVALID_BINARY_REPRESENTATION,
            format!("invalid internal value for enum: \"{s}\""),
        )),
    }
}

/// `enum_recv`.
// CONTRACT
pub fn recv(ctx: &IoCtx, ty: TypeId, buf: &[u8]) -> Result<Datum> {
    let s = std::str::from_utf8(buf).map_err(|_| crate::io::invalid_binary(""))?;
    input(ctx, ty, s)
}

/// `enum_send`.
// CONTRACT
pub fn send(ctx: &IoCtx, ty: TypeId, d: &Datum) -> Result<Vec<u8>> {
    Ok(output(ctx, ty, d)?.into_bytes())
}

/// Enum type OID of a call: the actual type of argument 0 (PG `get_fn_expr_argtype(0)`).
fn enum_type_of(ctx: &FuncCtx) -> Result<Oid> {
    match ctx.arg_types.first() {
        Some(t) if t.oid != 0 => Ok(t.oid),
        _ => Err(PgError::new(SqlState::FEATURE_NOT_SUPPORTED, "could not determine actual enum type")),
    }
}

fn arg(args: &[Datum], i: usize) -> Result<&Datum> {
    args.get(i).ok_or_else(|| PgError::internal(format!("enum function missing argument {i}")))
}

/// Two non-NULL label arguments (strict callers guarantee non-NULL).
fn two_labels(args: &[Datum]) -> Result<(&str, &str)> {
    Ok((label_of(arg(args, 0)?)?, label_of(arg(args, 1)?)?))
}

/// Sort-order comparison of two label arguments of this call's enum type.
fn cmp_args(args: &[Datum], ctx: &FuncCtx) -> Result<Ordering> {
    let oid = enum_type_of(ctx)?;
    let (a, b) = two_labels(args)?;
    cmp_labels(ctx.env, oid, a, b)
}

fn enum_eq(args: &[Datum], _ctx: &FuncCtx) -> Result<Datum> {
    let (a, b) = two_labels(args)?;
    Ok(Datum::Bool(a == b))
}

fn enum_ne(args: &[Datum], _ctx: &FuncCtx) -> Result<Datum> {
    let (a, b) = two_labels(args)?;
    Ok(Datum::Bool(a != b))
}

fn enum_lt(args: &[Datum], ctx: &FuncCtx) -> Result<Datum> {
    Ok(Datum::Bool(cmp_args(args, ctx)? == Ordering::Less))
}

fn enum_le(args: &[Datum], ctx: &FuncCtx) -> Result<Datum> {
    Ok(Datum::Bool(cmp_args(args, ctx)? != Ordering::Greater))
}

fn enum_gt(args: &[Datum], ctx: &FuncCtx) -> Result<Datum> {
    Ok(Datum::Bool(cmp_args(args, ctx)? == Ordering::Greater))
}

fn enum_ge(args: &[Datum], ctx: &FuncCtx) -> Result<Datum> {
    Ok(Datum::Bool(cmp_args(args, ctx)? != Ordering::Less))
}

fn enum_cmp(args: &[Datum], ctx: &FuncCtx) -> Result<Datum> {
    let v = match cmp_args(args, ctx)? {
        Ordering::Less => -1,
        Ordering::Equal => 0,
        Ordering::Greater => 1,
    };
    Ok(Datum::Int4(v))
}

/// `enum_smaller`: `a` if a < b, else `b`.
fn enum_smaller(args: &[Datum], ctx: &FuncCtx) -> Result<Datum> {
    let (a, b) = two_labels(args)?;
    let pick = if cmp_labels(ctx.env, enum_type_of(ctx)?, a, b)? == Ordering::Less { a } else { b };
    Ok(Datum::Text(pick.to_owned()))
}

/// `enum_larger`: `a` if a > b, else `b`.
fn enum_larger(args: &[Datum], ctx: &FuncCtx) -> Result<Datum> {
    let (a, b) = two_labels(args)?;
    let pick = if cmp_labels(ctx.env, enum_type_of(ctx)?, a, b)? == Ordering::Greater { a } else { b };
    Ok(Datum::Text(pick.to_owned()))
}

/// 55000 for `enum_first`/`enum_last` on an enum without labels.
fn empty_enum(env: &dyn TypeEnv, enum_oid: Oid) -> Result<PgError> {
    Ok(PgError::new(
        SqlState::OBJECT_NOT_IN_PREREQUISITE_STATE,
        format!("enum {} contains no values", names::format_type(env, enum_oid, -1)?),
    ))
}

/// `enum_first(anyenum)`: first label in sort order; the argument value is never examined.
fn enum_first(_args: &[Datum], ctx: &FuncCtx) -> Result<Datum> {
    let oid = enum_type_of(ctx)?;
    let labels = labels_of(ctx.env, oid)?;
    match labels.first() {
        Some(l) => Ok(Datum::Text(l.label.clone())),
        None => Err(empty_enum(ctx.env, oid)?),
    }
}

/// `enum_last(anyenum)`: last label in sort order; the argument value is never examined.
fn enum_last(_args: &[Datum], ctx: &FuncCtx) -> Result<Datum> {
    let oid = enum_type_of(ctx)?;
    let labels = labels_of(ctx.env, oid)?;
    match labels.last() {
        Some(l) => Ok(Datum::Text(l.label.clone())),
        None => Err(empty_enum(ctx.env, oid)?),
    }
}

/// A `Datum::Array` of enum labels with element type `enum_oid`.
fn label_array<'a>(enum_oid: Oid, items: impl Iterator<Item = &'a str>) -> Datum {
    let values: Vec<Datum> = items.map(|s| Datum::Text(s.to_owned())).collect();
    Datum::Array(Box::new(Array::from_vec(enum_oid, values)))
}

/// `enum_range_all(anyenum)`: every label in sort order.
fn enum_range_all(_args: &[Datum], ctx: &FuncCtx) -> Result<Datum> {
    let oid = enum_type_of(ctx)?;
    let labels = labels_of(ctx.env, oid)?;
    Ok(label_array(oid, labels.iter().map(|l| l.label.as_str())))
}

/// `Option<&str>` view of a bound argument: NULL = open end of the range.
fn optional_label(d: &Datum) -> Result<Option<&str>> {
    match d {
        Datum::Null => Ok(None),
        other => Ok(Some(label_of(other)?)),
    }
}

/// `enum_range_bounds(anyenum, anyenum)`: labels from `lower` to `upper`, both inclusive, in sort order.
/// NULL bounds are open. As in PG's `enum_range_internal`, a lower bound that is not a label yields an
/// empty array, and an upper bound that is never reached leaves the range open at the top.
fn enum_range_bounds(args: &[Datum], ctx: &FuncCtx) -> Result<Datum> {
    let oid = enum_type_of(ctx)?;
    let lower = optional_label(arg(args, 0)?)?;
    let upper = optional_label(arg(args, 1)?)?;
    let labels = labels_of(ctx.env, oid)?;
    let mut left_found = lower.is_none();
    let mut picked: Vec<&str> = Vec::new();
    for l in labels.iter() {
        let text = l.label.as_str();
        if !left_found && lower == Some(text) {
            left_found = true;
        }
        if left_found {
            picked.push(text);
        }
        if upper == Some(text) {
            break;
        }
    }
    Ok(label_array(oid, picked.into_iter()))
}

/// OID of the pg_enum row of a label argument (what `hashenum` hashes).
fn label_oid(args: &[Datum], ctx: &FuncCtx) -> Result<Oid> {
    let oid = enum_type_of(ctx)?;
    let labels = labels_of(ctx.env, oid)?;
    let text = label_of(arg(args, 0)?)?;
    Ok(lookup_label(ctx.env, oid, &labels, text)?.oid)
}

/// `hashenum(anyenum)` = PG `hashoid` of the label's pg_enum OID.
fn hashenum(args: &[Datum], ctx: &FuncCtx) -> Result<Datum> {
    let v = compare::pg_hash_uint32(label_oid(args, ctx)?)?;
    Ok(Datum::Int4(v as i32))
}

/// `hashenumextended(anyenum, int8)` = PG `hashoidextended` of the label's pg_enum OID.
fn hashenumextended(args: &[Datum], ctx: &FuncCtx) -> Result<Datum> {
    let seed = arg(args, 1)?
        .as_i64()
        .ok_or_else(|| PgError::internal("hashenumextended seed must be int8"))?;
    let v = compare::pg_hash_uint32_extended(label_oid(args, ctx)?, seed as u64)?;
    Ok(Datum::Int8(v as i64))
}

/// Function registry hook (see `crate::func`).
pub(crate) fn lookup(proc: &BuiltinProc) -> Option<FuncImpl> {
    let f: ScalarFn = match proc.prosrc {
        "enum_eq" => enum_eq,
        "enum_ne" => enum_ne,
        "enum_lt" => enum_lt,
        "enum_le" => enum_le,
        "enum_gt" => enum_gt,
        "enum_ge" => enum_ge,
        "enum_cmp" => enum_cmp,
        "enum_smaller" => enum_smaller,
        "enum_larger" => enum_larger,
        "enum_first" => enum_first,
        "enum_last" => enum_last,
        "enum_range_bounds" => enum_range_bounds,
        "enum_range_all" => enum_range_all,
        "hashenum" => hashenum,
        "hashenumextended" => hashenumextended,
        _ => return None,
    };
    Some(FuncImpl::Scalar(f))
}

/// 0A000 for an Enum-module proc without implementation.
pub(crate) fn unsupported(proc: &BuiltinProc) -> PgError {
    PgError::unimplemented("TYP-41", format!("function {}", proc.name))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::context::{IoConfig, TestSession};
    use crate::registry;
    use std::borrow::Cow;

    /// A user enum `mood` with a sort order that differs from label text order.
    const MOOD: Oid = pgarbor_common::FIRST_NORMAL_OID;

    struct MoodEnv;

    fn mood_labels() -> Arc<[EnumLabel]> {
        vec![
            EnumLabel { oid: 16385, label: "sad".to_string(), sort_order: 1.0 },
            EnumLabel { oid: 16386, label: "ok".to_string(), sort_order: 2.0 },
            EnumLabel { oid: 16387, label: "happy".to_string(), sort_order: 3.0 },
        ]
        .into()
    }

    impl TypeEnv for MoodEnv {
        fn type_info(&self, oid: Oid) -> Option<registry::TypeInfo> {
            if oid != MOOD {
                return registry::builtin_env().type_info(oid);
            }
            let mut info = registry::builtin_env().type_info(crate::oids::TEXT)?;
            info.oid = MOOD;
            info.name = Cow::Borrowed("mood");
            info.typtype = b'e';
            info.category = b'E';
            info.input = "enum_in";
            info.output = "enum_out";
            info.receive = "enum_recv";
            info.send = "enum_send";
            Some(info)
        }
        fn type_by_name(&self, schema: Option<&str>, typname: &str) -> Option<Oid> {
            if typname == "mood" && matches!(schema, None | Some("public")) {
                return Some(MOOD);
            }
            registry::builtin_env().type_by_name(schema, typname)
        }
        fn enum_labels(&self, oid: Oid) -> Option<Arc<[EnumLabel]>> {
            (oid == MOOD).then(mood_labels)
        }
    }

    fn label(s: &str) -> Datum {
        Datum::Text(s.to_string())
    }

    fn mood_array(items: &[&str]) -> Datum {
        label_array(MOOD, items.iter().copied())
    }

    #[test]
    fn input_accepts_only_exact_labels() {
        let env = MoodEnv;
        let cfg = IoConfig::default();
        let ctx = IoCtx::new(&env, &cfg);
        assert_eq!(input(&ctx, TypeId::of(MOOD), "ok").unwrap(), label("ok"));
        let err = input(&ctx, TypeId::of(MOOD), "meh").unwrap_err();
        assert_eq!(err.sqlstate, SqlState::INVALID_TEXT_REPRESENTATION);
        assert_eq!(err.message, "invalid input value for enum mood: \"meh\"");
        assert!(input(&ctx, TypeId::of(MOOD), "OK").is_err());
    }

    #[test]
    fn output_and_binary_round_trip() {
        let env = MoodEnv;
        let cfg = IoConfig::default();
        let ctx = IoCtx::new(&env, &cfg);
        assert_eq!(output(&ctx, TypeId::of(MOOD), &label("happy")).unwrap(), "happy");
        assert_eq!(send(&ctx, TypeId::of(MOOD), &label("happy")).unwrap(), b"happy".to_vec());
        assert_eq!(recv(&ctx, TypeId::of(MOOD), b"sad").unwrap(), label("sad"));
        assert!(recv(&ctx, TypeId::of(MOOD), b"nope").is_err());
        assert!(recv(&ctx, TypeId::of(MOOD), &[0xff]).is_err());
    }

    #[test]
    fn ordering_uses_sort_order_not_text() {
        let env = MoodEnv;
        assert_eq!(cmp_labels(&env, MOOD, "happy", "sad").unwrap(), Ordering::Greater);
        assert_eq!(cmp_labels(&env, MOOD, "ok", "ok").unwrap(), Ordering::Equal);
        assert_eq!(label_sort_order(&env, MOOD, "ok").unwrap(), 2.0);
        assert!(cmp_labels(&env, MOOD, "ok", "meh").is_err());
    }

    #[test]
    fn range_and_endpoint_functions() {
        let env = MoodEnv;
        let cfg = IoConfig::default();
        let sess = TestSession::default();
        let types = [TypeId::of(MOOD)];
        let ctx = FuncCtx {
            env: &env,
            io: &cfg,
            session: &sess,
            arg_types: &types,
            result_type: TypeId::of(MOOD),
            collation: 0,
        };
        let null = Datum::Null;

        assert_eq!(enum_range_all(&[null.clone()], &ctx).unwrap(), mood_array(&["sad", "ok", "happy"]));
        assert_eq!(
            enum_range_bounds(&[label("ok"), null.clone()], &ctx).unwrap(),
            mood_array(&["ok", "happy"])
        );
        assert_eq!(enum_range_bounds(&[null.clone(), label("ok")], &ctx).unwrap(), mood_array(&["sad", "ok"]));
        assert_eq!(
            enum_range_bounds(&[label("ok"), label("ok")], &ctx).unwrap(),
            mood_array(&["ok"])
        );
        assert_eq!(enum_range_bounds(&[label("happy"), label("sad")], &ctx).unwrap(), mood_array(&[]));
        assert_eq!(enum_first(&[null.clone()], &ctx).unwrap(), label("sad"));
        assert_eq!(enum_last(&[null.clone()], &ctx).unwrap(), label("happy"));
    }

    #[test]
    fn comparison_functions() {
        let env = MoodEnv;
        let cfg = IoConfig::default();
        let sess = TestSession::default();
        let types = [TypeId::of(MOOD), TypeId::of(MOOD)];
        let ctx = FuncCtx {
            env: &env,
            io: &cfg,
            session: &sess,
            arg_types: &types,
            result_type: TypeId::of(MOOD),
            collation: 0,
        };
        assert_eq!(enum_lt(&[label("sad"), label("happy")], &ctx).unwrap(), Datum::Bool(true));
        assert_eq!(enum_gt(&[label("sad"), label("happy")], &ctx).unwrap(), Datum::Bool(false));
        assert_eq!(enum_cmp(&[label("happy"), label("sad")], &ctx).unwrap(), Datum::Int4(1));
        assert_eq!(enum_smaller(&[label("happy"), label("sad")], &ctx).unwrap(), label("sad"));
        assert_eq!(enum_larger(&[label("happy"), label("sad")], &ctx).unwrap(), label("happy"));
        assert_eq!(enum_eq(&[label("ok"), label("ok")], &ctx).unwrap(), Datum::Bool(true));
        assert_eq!(enum_ne(&[label("ok"), label("sad")], &ctx).unwrap(), Datum::Bool(true));
    }

    #[test]
    fn lookup_covers_owned_procs() {
        let names = [
            "enum_eq", "enum_ne", "enum_lt", "enum_le", "enum_gt", "enum_ge", "enum_cmp", "enum_smaller",
            "enum_larger", "enum_first", "enum_last", "enum_range_bounds", "enum_range_all", "hashenum",
            "hashenumextended",
        ];
        for p in crate::catalog_data::PROCS.iter().filter(|p| p.module == crate::catalog_data::ImplModule::Enum) {
            assert!(names.contains(&p.prosrc), "unexpected enum proc {}", p.prosrc);
            assert!(lookup(p).is_some(), "no implementation for {}", p.prosrc);
        }
    }
}
