//! Coercion pathway lookup and cast planning (TYPE-03). Lease TYP-52 (sonnet).
//!
//! PG18 reference: `src/backend/parser/parse_coerce.c` — `find_coercion_pathway`,
//! `find_typmod_coercion_function`, `IsBinaryCoercible`, `coerce_type` (structure only), and the
//! `pg_cast` rows (`catalog_data::CASTS`, plus `TypeEnv::user_cast`).
//!
//! `find_cast(env, source, target, ctx)` (PG `find_coercion_pathway`), in order:
//! 1. `source == target` → `Relabel`.
//! 2. Domains: replace a domain source by its base type (`registry::base_type`); a domain target is
//!    handled by `plan_cast` (cast to the base, then `CastStep::DomainCheck`).
//! 3. `pg_cast` row (builtin or user) with `context <= ctx`: method Function → `Func(castfunc)`,
//!    Binary → `Relabel`, InOut → `ViaIo`. A row whose context is too strong means "no pathway in this
//!    context" (return `None`; do NOT fall through to IO for that pair, PG semantics).
//! 4. No row: if both source and target are array types and `find_cast(elem_src, elem_dst, ctx)`
//!    returns a pathway P → `ArrayCoerce(P)` (`'{1,2}'::int[]::text[]`, `int4[] → int8[]` implicitly).
//! 5. Automatic I/O conversion: target in string category ('S') and `ctx >= Assignment` → `ViaIo`;
//!    source in string category and `ctx == Explicit` → `ViaIo`. (E.g. `int4 → text` is assignment-only:
//!    `INSERT INTO t(textcol) VALUES (1)` works, `'1'::text + 1` does not coerce.)
//! 6. Composite/record → record and record → composite by field-wise coercion: `RecordCoerce`.
//! 7. Otherwise `None`.
//! Unknown-type literals (`unknown`, OID 705) are NOT handled here: see `resolve::coerce`.
//!
//! `plan_cast(env, from, to, ctx)` produces the executable `CastPlan`: pathway steps, then a
//! `Typmod` step when `to.typmod >= 0` and the target has a length-coercion function
//! (varchar, bpchar, numeric, time, timetz, timestamp, timestamptz, interval, bit, varbit) and the
//! typmods differ, then a `DomainCheck` for domain targets. No pathway → 42846
//! `cannot cast type integer to interval` (type names via `names::format_type`; oracle-verified for
//! `x::interval` with x int). `Explicit` controls typmod semantics (explicit `::varchar(3)` truncates,
//! assignment errors 22001).
use crate::catalog_data::{builtin_cast, builtin_proc, BuiltinCast, CastContext, CastMethod};
use crate::context::TypeEnv;
use crate::datum::TypeId;
use crate::names::display_name;
use crate::oids;
use crate::registry::{base_type, base_type_and_typmod, element_type_of, type_category};
use pgarbor_common::{Oid, PgError, Result, SqlState};

/// PG `CoercionPathType`.
// CONTRACT
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CastPathway {
    /// Binary coercible: no runtime work (e.g. varchar → text, text → varchar).
    Relabel,
    /// Call a cast function (pg_proc OID); it may take 1, 2 (typmod) or 3 (typmod, is_explicit) args.
    Func(Oid),
    /// Output with the source type's output function, input with the target's input function.
    ViaIo,
    /// Element-wise cast of an array.
    ArrayCoerce(Box<CastPathway>),
    /// Field-wise record/composite coercion.
    RecordCoerce,
}

/// One runtime step of a cast.
// CONTRACT
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CastStep {
    Relabel,
    /// Call cast function `proc` with `nargs` arguments: (value[, target typmod int4[, explicit bool]]).
    Func { proc: Oid, nargs: u8 },
    /// I/O conversion from `from` to `to` (typmod applied by the input function).
    ViaIo { from: TypeId, to: TypeId },
    /// Element-wise application of `elem_plan` producing an array of `elem_plan.to.oid`.
    ArrayCoerce { elem_plan: Box<CastPlan> },
    /// Field-wise coercion to the target composite / record.
    RecordCoerce { to: TypeId },
    /// Length / precision coercion (`typmod::enforce_typmod`).
    Typmod { to: TypeId, explicit: bool },
    /// Domain constraint check for `domain` (NOT NULL / CHECK): performed by pgarbor-exec; the type
    /// crate only marks where it must happen (`apply_cast` treats it as identity).
    DomainCheck { domain: Oid },
}

/// An executable cast.
// CONTRACT
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CastPlan {
    pub from: TypeId,
    pub to: TypeId,
    pub context: CastContext,
    pub steps: Vec<CastStep>,
}

impl CastPlan {
    /// True when the cast does nothing at runtime.
    pub fn is_noop(&self) -> bool {
        self.steps.iter().all(|s| matches!(s, CastStep::Relabel))
    }
}

/// Maximum array-nesting depth followed when looking for element pathways (guards against
/// malformed environments only; arrays of arrays are not distinct types in PG).
const MAX_DEPTH: u8 = 8;

const ANYCOMPATIBLEARRAY: Oid = 5078;
const ANYCOMPATIBLENONARRAY: Oid = 5079;
const ANYCOMPATIBLERANGE: Oid = 5080;
const ANYCOMPATIBLEMULTIRANGE: Oid = 4538;

/// The `pg_cast` row for (source, target): user casts first, then builtin rows.
fn cast_row(env: &dyn TypeEnv, source: Oid, target: Oid) -> Option<BuiltinCast> {
    env.user_cast(source, target).or_else(|| builtin_cast(source, target).copied())
}

fn pathway_of_row(row: &BuiltinCast) -> CastPathway {
    match row.method {
        CastMethod::Function => CastPathway::Func(row.func),
        CastMethod::Binary => CastPathway::Relabel,
        CastMethod::InOut => CastPathway::ViaIo,
    }
}

fn typtype_of(env: &dyn TypeEnv, oid: Oid) -> u8 {
    env.type_info(oid).map(|t| t.typtype).unwrap_or(0)
}

/// PG `find_coercion_pathway` (see module docs). `Ok(None)` = no pathway in this context.
// CONTRACT
pub fn find_cast(env: &dyn TypeEnv, source: Oid, target: Oid, ctx: CastContext) -> Result<Option<CastPathway>> {
    Ok(find_cast_depth(env, source, target, ctx, 0))
}

fn find_cast_depth(env: &dyn TypeEnv, source: Oid, target: Oid, ctx: CastContext, depth: u8) -> Option<CastPathway> {
    if source == target {
        return Some(CastPathway::Relabel);
    }
    // Domains: look through to base types (PG getBaseType on both sides).
    let source = base_type(env, source);
    let target = base_type(env, target);
    if source == target {
        return Some(CastPathway::Relabel);
    }

    // Explicit pg_cast row; a row whose context is too strong means "no pathway".
    if let Some(row) = cast_row(env, source, target) {
        return if row.context <= ctx { Some(pathway_of_row(&row)) } else { None };
    }

    // Array pair: element-wise coercion (int2vector / oidvector targets excluded, as in PG).
    if depth < MAX_DEPTH && target != oids::OIDVECTOR && target != oids::INT2VECTOR {
        if let (Some(se), Some(te)) = (element_type_of(env, source), element_type_of(env, target)) {
            if let Some(p) = find_cast_depth(env, se, te, ctx, depth + 1) {
                return Some(CastPathway::ArrayCoerce(Box::new(p)));
            }
        }
    }

    // Automatic I/O conversion.
    if ctx >= CastContext::Assignment && type_category(env, target) == b'S' {
        return Some(CastPathway::ViaIo);
    }
    if ctx >= CastContext::Explicit && type_category(env, source) == b'S' {
        return Some(CastPathway::ViaIo);
    }

    // record <-> composite.
    let st = typtype_of(env, source);
    let tt = typtype_of(env, target);
    if (source == oids::RECORD && tt == b'c') || (target == oids::RECORD && st == b'c') {
        return Some(CastPathway::RecordCoerce);
    }
    None
}

/// Does `oid` (a base type) have a typmod length-coercion function (`pg_cast` row oid -> oid), or
/// is it an array of such a type?
fn has_typmod_coercion(env: &dyn TypeEnv, oid: Oid, depth: u8) -> bool {
    if let Some(row) = cast_row(env, oid, oid) {
        return row.method == CastMethod::Function && row.func != 0;
    }
    if depth < MAX_DEPTH {
        if let Some(e) = element_type_of(env, oid) {
            return has_typmod_coercion(env, e, depth + 1);
        }
    }
    false
}

fn func_nargs(proc: Oid) -> u8 {
    builtin_proc(proc).map(|p| p.nargs.min(3) as u8).unwrap_or(1)
}

/// True when the pathway does no runtime work (nested array relabels included).
fn pathway_is_relabel(p: &CastPathway) -> bool {
    match p {
        CastPathway::Relabel => true,
        CastPathway::ArrayCoerce(inner) => pathway_is_relabel(inner),
        _ => false,
    }
}

fn pathway_steps(
    env: &dyn TypeEnv,
    pw: &CastPathway,
    from: TypeId,
    to: TypeId,
    ctx: CastContext,
    steps: &mut Vec<CastStep>,
) -> Result<()> {
    match pw {
        CastPathway::Relabel => steps.push(CastStep::Relabel),
        CastPathway::Func(p) => steps.push(CastStep::Func { proc: *p, nargs: func_nargs(*p) }),
        // The typmod (if any) is applied by a separate Typmod step so that explicit casts truncate.
        CastPathway::ViaIo => steps.push(CastStep::ViaIo { from: TypeId::of(from.oid), to: TypeId::of(to.oid) }),
        CastPathway::RecordCoerce => steps.push(CastStep::RecordCoerce { to: TypeId::of(to.oid) }),
        CastPathway::ArrayCoerce(inner) => {
            if pathway_is_relabel(inner) {
                steps.push(CastStep::Relabel);
            } else {
                let se = element_type_of(env, from.oid)
                    .ok_or_else(|| PgError::internal(format!("type {} is not an array", from.oid)))?;
                let te = element_type_of(env, to.oid)
                    .ok_or_else(|| PgError::internal(format!("type {} is not an array", to.oid)))?;
                let ef = TypeId::new(se, from.typmod);
                let et = TypeId::of(te);
                let mut inner_steps = Vec::new();
                pathway_steps(env, inner, ef, et, ctx, &mut inner_steps)?;
                steps.push(CastStep::ArrayCoerce {
                    elem_plan: Box::new(CastPlan { from: ef, to: et, context: ctx, steps: inner_steps }),
                });
            }
        }
    }
    Ok(())
}

/// Build an executable plan (see module docs). Errors: 42846 `cannot cast type X to Y`.
// CONTRACT
pub fn plan_cast(env: &dyn TypeEnv, from: TypeId, to: TypeId, ctx: CastContext) -> Result<CastPlan> {
    let no_path = || {
        PgError::new(
            SqlState::CANNOT_COERCE,
            format!("cannot cast type {} to {}", display_name(env, from.oid), display_name(env, to.oid)),
        )
    };
    let from_base = base_type(env, from.oid);
    let to_is_domain = typtype_of(env, to.oid) == b'd';
    let (to_base, dom_typmod) = base_type_and_typmod(env, to.oid, -1);
    let to_tm = if to.typmod >= 0 {
        to.typmod
    } else if to_is_domain {
        dom_typmod
    } else {
        -1
    };
    let src = TypeId::new(from_base, from.typmod);
    let dst = TypeId::new(to_base, to_tm);

    let pw = find_cast_depth(env, from_base, to_base, ctx, 0).ok_or_else(no_path)?;
    let mut steps = Vec::new();
    pathway_steps(env, &pw, src, dst, ctx, &mut steps)?;

    if to_tm >= 0 && (src.oid != dst.oid || src.typmod != dst.typmod) && has_typmod_coercion(env, to_base, 0) {
        let explicit = ctx == CastContext::Explicit;
        match element_type_of(env, to_base) {
            Some(te) if cast_row(env, to_base, to_base).is_none() => {
                let ef = TypeId::of(te);
                let et = TypeId::new(te, to_tm);
                steps.push(CastStep::ArrayCoerce {
                    elem_plan: Box::new(CastPlan {
                        from: ef,
                        to: et,
                        context: ctx,
                        steps: vec![CastStep::Typmod { to: et, explicit }],
                    }),
                });
            }
            _ => steps.push(CastStep::Typmod { to: dst, explicit }),
        }
    }
    if to_is_domain {
        steps.push(CastStep::DomainCheck { domain: to.oid });
    }
    Ok(CastPlan { from, to, context: ctx, steps })
}

/// PG `IsBinaryCoercible(source, target)`.
// CONTRACT
pub fn is_binary_coercible(env: &dyn TypeEnv, source: Oid, target: Oid) -> Result<bool> {
    if source == target {
        return Ok(true);
    }
    if target == oids::ANY || target == oids::ANYELEMENT || target == oids::ANYCOMPATIBLE {
        return Ok(true);
    }
    let source = base_type(env, source);
    if source == target {
        return Ok(true);
    }
    let src_elem = element_type_of(env, source);
    let is_arr = src_elem.is_some();
    let st = typtype_of(env, source);
    if (target == oids::ANYARRAY || target == ANYCOMPATIBLEARRAY) && is_arr {
        return Ok(true);
    }
    if (target == oids::ANYNONARRAY || target == ANYCOMPATIBLENONARRAY) && !is_arr {
        return Ok(true);
    }
    if target == oids::ANYENUM && st == b'e' {
        return Ok(true);
    }
    if (target == oids::ANYRANGE || target == ANYCOMPATIBLERANGE) && st == b'r' {
        return Ok(true);
    }
    if (target == oids::ANYMULTIRANGE || target == ANYCOMPATIBLEMULTIRANGE) && st == b'm' {
        return Ok(true);
    }
    if target == oids::RECORD && st == b'c' {
        return Ok(true);
    }
    if target == oids::RECORD_ARRAY {
        if let Some(e) = src_elem {
            if typtype_of(env, e) == b'c' {
                return Ok(true);
            }
        }
    }
    Ok(match cast_row(env, source, target) {
        Some(row) => row.method == CastMethod::Binary && row.context == CastContext::Implicit,
        None => false,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::registry::builtin_env;

    #[test]
    fn widening_and_narrowing() {
        let env = builtin_env();
        assert_eq!(find_cast(env, oids::INT4, oids::INT8, CastContext::Implicit).unwrap(), Some(CastPathway::Func(481)));
        assert_eq!(find_cast(env, oids::INT8, oids::INT4, CastContext::Implicit).unwrap(), None);
        assert_eq!(find_cast(env, oids::INT8, oids::INT4, CastContext::Assignment).unwrap(), Some(CastPathway::Func(480)));
    }

    #[test]
    fn relabel_and_io() {
        let env = builtin_env();
        assert_eq!(find_cast(env, oids::VARCHAR, oids::TEXT, CastContext::Implicit).unwrap(), Some(CastPathway::Relabel));
        assert_eq!(find_cast(env, oids::INT4, oids::TEXT, CastContext::Assignment).unwrap(), Some(CastPathway::ViaIo));
        assert_eq!(find_cast(env, oids::INT4, oids::TEXT, CastContext::Implicit).unwrap(), None);
        assert_eq!(find_cast(env, oids::TEXT, oids::INT4, CastContext::Explicit).unwrap(), Some(CastPathway::ViaIo));
        assert_eq!(find_cast(env, oids::TEXT, oids::INT4, CastContext::Assignment).unwrap(), None);
    }

    #[test]
    fn array_elementwise() {
        let env = builtin_env();
        let a8 = crate::registry::array_type_of(env, oids::INT8).unwrap();
        assert_eq!(
            find_cast(env, oids::INT4_ARRAY, a8, CastContext::Implicit).unwrap(),
            Some(CastPathway::ArrayCoerce(Box::new(CastPathway::Func(481))))
        );
    }

    #[test]
    fn plan_errors_and_typmod() {
        let env = builtin_env();
        let e = plan_cast(env, TypeId::INT4, TypeId::INTERVAL, CastContext::Explicit).unwrap_err();
        assert_eq!(e.sqlstate.code(), "42846");
        assert_eq!(e.message, "cannot cast type integer to interval");
        let p = plan_cast(env, TypeId::TEXT, TypeId::new(oids::VARCHAR, 7), CastContext::Explicit).unwrap();
        assert!(p.steps.iter().any(|s| matches!(s, CastStep::Typmod { explicit: true, .. })));
        let p = plan_cast(env, TypeId::INT4, TypeId::INT8, CastContext::Implicit).unwrap();
        assert_eq!(p.steps, vec![CastStep::Func { proc: 481, nargs: 1 }]);
        assert!(plan_cast(env, TypeId::VARCHAR, TypeId::TEXT, CastContext::Implicit).unwrap().is_noop());
    }

    #[test]
    fn binary_coercible() {
        let env = builtin_env();
        assert!(is_binary_coercible(env, oids::VARCHAR, oids::TEXT).unwrap());
        assert!(!is_binary_coercible(env, oids::INT4, oids::INT8).unwrap());
        assert!(is_binary_coercible(env, oids::INT4, oids::ANYELEMENT).unwrap());
        assert!(is_binary_coercible(env, oids::INT4_ARRAY, oids::ANYARRAY).unwrap());
        assert!(!is_binary_coercible(env, oids::INT4, oids::ANYARRAY).unwrap());
    }
}
