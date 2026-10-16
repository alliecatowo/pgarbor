//! Expression simplification: constant folding, boolean normalization, qual splitting (PLN-02).
//! PG reference: `optimizer/util/clauses.c` (`eval_const_expressions`, `simplify_function`,
//! `simplify_op_expr`, `simplify_boolean_equality`, `negate_clause`, `make_ands_explicit`,
//! `make_ands_implicit`), `prepqual.c` (`canonicalize_qual`, `find_duplicate_ors`),
//! `optimizer/util/predtest.c` is out of scope.
//!
//! ## `simplify_expr(st, e)` rules (bottom-up; semantics preserving; evaluation errors propagate):
//! 1. **Folding**: a `FuncCall`/`Op`/`Cast`/`Coalesce`/`NullIf`/`MinMax`/`Case`/`Array`/`Row`/`FieldSelect`/
//!    `Subscript`/`NullTest`/`BooleanTest`/`DistinctFrom`/`ScalarArrayOp` node whose arguments are ALL
//!    `Const` and whose volatility is `Immutable` is replaced by the `Const` it evaluates to
//!    (`pgarbor_types::func::implementation(oid)?.call(..)` with a `FuncCtx` built from
//!    `TypeEnv`=`st.ctx.env`, default `IoConfig`, and a `TestSession`; casts via `cast::apply_cast`).
//!    Errors during folding (`1/0` -> 22012) are returned as-is (PG does the same at plan time).
//!    `Stable` and `Volatile` functions are NOT folded (`now()` stays; PG folds stable functions only
//!    for estimation). Strict functions with a NULL constant argument fold to NULL without calling.
//!    Never fold set-returning functions or anything containing `Param`/`Column`/`OuterRef`/sub-plans.
//!    `Param(n)` nodes become `Const` only when `st.ctx.param_values` is `Some` (custom plan).
//! 2. **Boolean**: flatten nested AND/OR; drop `true` conjuncts / `false` disjuncts; `x AND false` ->
//!    `false`; `x OR true` -> `true`; AND/OR with one remaining operand -> that operand; `NOT NOT x` ->
//!    `x`; `NOT (a = b)` -> `a <> b` and the other comparison negations via the operator's `negator`
//!    (PG `negate_clause` does this for btree comparison operators `=,<>,<,<=,>,>=` of the same type); `NOT (x IS NULL)` -> `x IS NOT NULL`;
//!    `Const(NULL)` operands: `NULL AND false` -> false, `NULL AND true` -> NULL, `NULL OR true` -> true.
//!    `CASE WHEN true THEN a ... END` -> `a`; leading `WHEN false` arms removed; `COALESCE(NULL, x)` ->
//!    `x`; `x IS NULL` on a non-nullable constant -> false.
//! 3. **Casts**: `Cast` of `Const` folded; stacked relabels collapsed; identity casts removed.
//!
//! `simplify_expr` must NOT change the result type of any node or reorder side-effecting operands.
//!
//! ## Qual helpers (`split_conjuncts`, `make_and`) are used by pushdown/index/join: fully pure.
//!
//! Implementation note: PG's `eval_const_expressions_mutator` is not a plain bottom-up walk for
//! CASE and COALESCE: a WHEN arm whose condition folds to FALSE/NULL is dropped without simplifying
//! its result, and COALESCE stops at the first non-NULL constant. This module mirrors that laziness
//! (so `CASE WHEN false THEN 1/0 ELSE 1 END` yields 1, as in PG), and it substitutes a constant
//! simple-CASE operand into the `CaseTestValue` placeholders (PG `case_val`).
use std::cmp::Ordering;

use pgarbor_common::{Oid, Result};
use pgarbor_sql::ir::{
    BoolExpr, BoolOp, BoolTestKind, BooleanTest, BoundExpr, CaseArm, CaseExpr, CastExpr, DistinctFrom,
    ExprKind, NullTest, OpExpr, OpRef, ScalarArrayOp,
};
use pgarbor_types::array::subscript::{self, Subscript};
use pgarbor_types::cast::{apply_cast, CastPlan, CastStep};
use pgarbor_types::catalog_data::{builtin_operator, builtin_proc, Volatility};
use pgarbor_types::compare::cmp_datums;
use pgarbor_types::context::{FuncCtx, IoConfig, TestSession};
use pgarbor_types::func::{self, FuncHandle, FuncImpl};
use pgarbor_types::{Datum, TypeId};

use crate::context::{PlanContext, PlannerState};

/// Simplify one expression (module docs).
pub fn simplify_expr(st: &mut PlannerState, e: BoundExpr) -> Result<BoundExpr> {
    let ctx = st.ctx;
    simp(ctx, e, None)
}

/// Simplify a list of AND-ed quals: split top-level ANDs into conjuncts, simplify each, drop constant
/// TRUE conjuncts; a constant FALSE/NULL conjunct collapses the list to the single `false` constant
/// (callers then emit a `Result` with one-time filter false).
pub fn simplify_quals(st: &mut PlannerState, quals: Vec<BoundExpr>) -> Result<Vec<BoundExpr>> {
    let mut out: Vec<BoundExpr> = Vec::with_capacity(quals.len());
    for q in quals {
        for c in split_conjuncts(q) {
            let s = simplify_expr(st, c)?;
            for part in split_conjuncts(s) {
                if part.is_true_const() {
                    continue;
                }
                if part.is_false_const() || part.is_null_const() {
                    return Ok(vec![BoundExpr::bool_const(false)]);
                }
                out.push(part);
            }
        }
    }
    Ok(out)
}

/// Split an expression into its top-level AND conjuncts (`a AND (b AND c)` -> `[a, b, c]`; any other
/// expression -> one element). Fully implemented.
pub fn split_conjuncts(e: BoundExpr) -> Vec<BoundExpr> {
    use pgarbor_sql::ir::{BoolOp, ExprKind};
    match e.kind {
        ExprKind::Bool(b) if b.op == BoolOp::And => {
            let mut out = Vec::new();
            for a in b.args {
                out.extend(split_conjuncts(a));
            }
            out
        }
        _ => vec![e],
    }
}

/// AND a list back together (`BoundExpr::and_all`), `None` for the empty list.
pub fn make_and(quals: Vec<BoundExpr>) -> Option<BoundExpr> {
    if quals.is_empty() {
        None
    } else {
        Some(BoundExpr::and_all(quals))
    }
}

// ---------------------------------------------------------------------------------------------
// Recursive driver
// ---------------------------------------------------------------------------------------------

/// Simplify `e` bottom-up. `case_val` is the constant simple-CASE operand that `CaseTestValue`
/// placeholders read (PG `context->case_val`); `None` outside a constant-operand CASE.
fn simp(ctx: &PlanContext, e: BoundExpr, case_val: Option<&Datum>) -> Result<BoundExpr> {
    let BoundExpr { kind, ty } = e;
    match kind {
        ExprKind::CaseTestValue => Ok(match case_val {
            Some(d) => BoundExpr::constant(d.clone(), ty),
            None => BoundExpr { kind: ExprKind::CaseTestValue, ty },
        }),
        ExprKind::Case(c) => simp_case(ctx, *c, ty, case_val),
        ExprKind::Coalesce(args) => simp_coalesce(ctx, args, ty, case_val),
        kind => {
            let e = BoundExpr { kind, ty }.map_children(&mut |c| simp(ctx, c, case_val))?;
            if let Some(d) = try_fold(ctx, &e)? {
                return Ok(BoundExpr::constant(d, ty));
            }
            Ok(rewrite(e))
        }
    }
}

/// PG `eval_const_expressions` CASE handling (clauses.c, `T_CaseExpr`).
fn simp_case(ctx: &PlanContext, c: CaseExpr, ty: TypeId, case_val: Option<&Datum>) -> Result<BoundExpr> {
    let CaseExpr { arg, arms, default } = c;
    let new_arg: Option<BoundExpr> = match arg {
        Some(a) => Some(simp(ctx, *a, case_val)?),
        None => None,
    };
    // A constant operand is substituted into the arms and dropped from the CASE.
    let inner_const: Option<Datum> = match &new_arg {
        Some(a) => a.as_const().cloned(),
        None => None,
    };
    let (arg_out, inner_val): (Option<BoundExpr>, Option<Datum>) = match inner_const {
        Some(d) => (None, Some(d)),
        None => (new_arg, None),
    };
    let inner_ref: Option<&Datum> = inner_val.as_ref();

    let mut kept: Vec<CaseArm> = Vec::with_capacity(arms.len());
    let mut true_result: Option<BoundExpr> = None;
    for arm in arms {
        let CaseArm { cond, result } = arm;
        let cond = simp(ctx, cond, inner_ref)?;
        // FALSE or NULL: the arm is dropped without simplifying its result (PG).
        let const_cond: Option<bool> = match cond.as_const() {
            Some(Datum::Bool(b)) => Some(*b),
            Some(_) => Some(false),
            None => None,
        };
        match const_cond {
            Some(false) => continue,
            Some(true) => {
                true_result = Some(simp(ctx, result, inner_ref)?);
                break;
            }
            None => {
                let result = simp(ctx, result, inner_ref)?;
                kept.push(CaseArm { cond, result });
            }
        }
    }

    let default_out: Option<BoundExpr> = match true_result {
        Some(r) => Some(r),
        None => match default {
            Some(d) => Some(simp(ctx, *d, inner_ref)?),
            None => None,
        },
    };

    if kept.is_empty() {
        return Ok(match default_out {
            Some(d) => d,
            None => BoundExpr::null(ty),
        });
    }
    Ok(BoundExpr {
        kind: ExprKind::Case(Box::new(CaseExpr {
            arg: arg_out.map(Box::new),
            arms: kept,
            default: default_out.map(Box::new),
        })),
        ty,
    })
}

/// PG `eval_const_expressions` COALESCE handling: NULL constants are dropped, the first non-NULL
/// constant ends the list (later arguments are not simplified at all).
fn simp_coalesce(ctx: &PlanContext, args: Vec<BoundExpr>, ty: TypeId, case_val: Option<&Datum>) -> Result<BoundExpr> {
    let mut out: Vec<BoundExpr> = Vec::with_capacity(args.len());
    for a in args {
        let s = simp(ctx, a, case_val)?;
        // 0 = NULL constant, 1 = non-NULL constant, 2 = anything else.
        let class: u8 = match s.as_const() {
            Some(d) if d.is_null() => 0,
            Some(_) => 1,
            None => 2,
        };
        match class {
            0 => continue,
            1 => {
                if out.is_empty() {
                    return Ok(s);
                }
                out.push(s);
                break;
            }
            _ => out.push(s),
        }
    }
    if out.is_empty() {
        return Ok(BoundExpr::null(ty));
    }
    if out.len() == 1 {
        return Ok(out.remove(0));
    }
    Ok(BoundExpr { kind: ExprKind::Coalesce(out), ty })
}

// ---------------------------------------------------------------------------------------------
// Constant folding
// ---------------------------------------------------------------------------------------------

/// Evaluate `e` when it is a foldable constant expression. `Ok(None)` = not foldable (keep the node).
/// Only the node itself is inspected: its children were already simplified.
fn try_fold(ctx: &PlanContext, e: &BoundExpr) -> Result<Option<Datum>> {
    match &e.kind {
        ExprKind::Param(n) => Ok(ctx
            .param_values
            .as_ref()
            .and_then(|vals| vals.get((*n as usize).wrapping_sub(1)).cloned())),
        ExprKind::FuncCall(f) => {
            // `f(VARIADIC arr)` is left to the executor, which unpacks the array for VARIADIC "any"
            // procs (`eval_call::eval_func_call`); `fold_call` does not do that.
            if f.retset || f.variadic || f.volatility != Volatility::Immutable {
                return Ok(None);
            }
            fold_call(ctx, f.func, &f.args, e.ty)
        }
        ExprKind::Op(o) => {
            if o.op.volatility != Volatility::Immutable {
                return Ok(None);
            }
            fold_call(ctx, o.op.proc, &o.args, e.ty)
        }
        ExprKind::DistinctFrom(d) => {
            if d.eq.volatility != Volatility::Immutable {
                return Ok(None);
            }
            let (l, r) = match (d.left.as_const(), d.right.as_const()) {
                (Some(l), Some(r)) => (l, r),
                _ => return Ok(None),
            };
            let distinct = if l.is_null() && r.is_null() {
                false
            } else if l.is_null() || r.is_null() {
                true
            } else {
                let h = match scalar_handle(d.eq.proc) {
                    Some(h) => h,
                    None => return Ok(None),
                };
                let eq = invoke(ctx, &h, &[l.clone(), r.clone()], &[d.left.ty, d.right.ty], TypeId::BOOL)?;
                !matches!(eq, Datum::Bool(true))
            };
            Ok(Some(Datum::Bool(if d.negated { !distinct } else { distinct })))
        }
        ExprKind::NullIf(n) => {
            if n.eq.volatility != Volatility::Immutable {
                return Ok(None);
            }
            let (l, r) = match (n.left.as_const(), n.right.as_const()) {
                (Some(l), Some(r)) => (l, r),
                _ => return Ok(None),
            };
            if l.is_null() {
                return Ok(Some(Datum::Null));
            }
            if r.is_null() {
                return Ok(Some(l.clone()));
            }
            let h = match scalar_handle(n.eq.proc) {
                Some(h) => h,
                None => return Ok(None),
            };
            let eq = invoke(ctx, &h, &[l.clone(), r.clone()], &[n.left.ty, n.right.ty], TypeId::BOOL)?;
            Ok(Some(if matches!(eq, Datum::Bool(true)) { Datum::Null } else { l.clone() }))
        }
        ExprKind::ScalarArrayOp(s) => fold_saop(ctx, s),
        ExprKind::Cast(c) => {
            let v = match c.arg.as_const() {
                Some(v) => v,
                None => return Ok(None),
            };
            if !cast_foldable(&c.plan) {
                return Ok(None);
            }
            with_ctx(ctx, &[c.arg.ty], e.ty, |fc| apply_cast(&c.plan, v.clone(), fc)).map(Some)
        }
        ExprKind::Array(a) => {
            let items = match const_values(&a.elements) {
                Some(v) => v,
                None => return Ok(None),
            };
            // Without multidims, array-valued elements would need dimension merging: leave those alone.
            if !a.multidims && items.iter().any(|d| matches!(d, Datum::Array(_))) {
                return Ok(None);
            }
            let arr = subscript::construct(a.elem_type, items, a.multidims)?;
            Ok(Some(Datum::Array(Box::new(arr))))
        }
        ExprKind::Row(r) => {
            if e.ty.oid != TypeId::RECORD.oid {
                return Ok(None);
            }
            Ok(const_values(&r.args).map(Datum::Record))
        }
        ExprKind::FieldSelect(f) => match f.arg.as_const() {
            None => Ok(None),
            Some(Datum::Null) => Ok(Some(Datum::Null)),
            Some(Datum::Record(items)) => Ok(items.get((f.field as usize).wrapping_sub(1)).cloned()),
            Some(_) => Ok(None),
        },
        ExprKind::Subscript(s) => {
            if s.is_slice {
                return Ok(None);
            }
            let arr = match s.array.as_const() {
                Some(v) => v,
                None => return Ok(None),
            };
            let mut subs: Vec<Option<Subscript>> = Vec::with_capacity(s.subscripts.len());
            for item in &s.subscripts {
                if item.upper.is_some() {
                    return Ok(None);
                }
                let idx = match item.lower.as_ref().and_then(|x| x.as_const()) {
                    Some(v) => v,
                    None => return Ok(None),
                };
                match idx {
                    Datum::Null => subs.push(None),
                    Datum::Int4(i) => subs.push(Some(Subscript::Index(*i))),
                    _ => return Ok(None),
                }
            }
            match arr {
                Datum::Null => Ok(Some(Datum::Null)),
                Datum::Array(a) => subscript::fetch(a, &subs).map(Some),
                _ => Ok(None),
            }
        }
        ExprKind::MinMax(m) => {
            let vals = match const_values(&m.args) {
                Some(v) => v,
                None => return Ok(None),
            };
            // NULL arguments are ignored; the result is NULL only when every argument is NULL.
            let mut best: Option<Datum> = None;
            for v in vals {
                if v.is_null() {
                    continue;
                }
                best = match best {
                    None => Some(v),
                    Some(cur) => {
                        let ord = cmp_datums(ctx.env, e.ty, &v, &cur)?;
                        let take_new = if m.greatest { ord == Ordering::Greater } else { ord == Ordering::Less };
                        Some(if take_new { v } else { cur })
                    }
                };
            }
            Ok(Some(best.unwrap_or(Datum::Null)))
        }
        ExprKind::NullTest(n) => {
            if n.row_semantics {
                return Ok(None);
            }
            match n.arg.as_const() {
                Some(d) => Ok(Some(Datum::Bool(if n.is_null { d.is_null() } else { !d.is_null() }))),
                None => Ok(None),
            }
        }
        ExprKind::BooleanTest(b) => {
            let v: Option<bool> = match b.arg.as_const() {
                Some(Datum::Bool(x)) => Some(*x),
                Some(Datum::Null) => None,
                _ => return Ok(None),
            };
            let r = match b.test {
                BoolTestKind::IsTrue => v == Some(true),
                BoolTestKind::IsNotTrue => v != Some(true),
                BoolTestKind::IsFalse => v == Some(false),
                BoolTestKind::IsNotFalse => v != Some(false),
                BoolTestKind::IsUnknown => v.is_none(),
                BoolTestKind::IsNotUnknown => v.is_some(),
            };
            Ok(Some(Datum::Bool(r)))
        }
        _ => Ok(None),
    }
}

/// Fold a call of immutable builtin `proc` whose arguments are all constants.
fn fold_call(ctx: &PlanContext, proc: Oid, args: &[BoundExpr], result_ty: TypeId) -> Result<Option<Datum>> {
    let vals = match const_values(args) {
        Some(v) => v,
        None => return Ok(None),
    };
    let h = match scalar_handle(proc) {
        Some(h) => h,
        None => return Ok(None),
    };
    let types: Vec<TypeId> = args.iter().map(|a| a.ty).collect();
    invoke(ctx, &h, &vals, &types, result_ty).map(Some)
}

/// `x op ANY/ALL (array)` with constant operands (PG `ExecEvalScalarArrayOp` semantics).
fn fold_saop(ctx: &PlanContext, s: &ScalarArrayOp) -> Result<Option<Datum>> {
    if s.op.volatility != Volatility::Immutable {
        return Ok(None);
    }
    let lv = match s.left.as_const() {
        Some(v) => v,
        None => return Ok(None),
    };
    let rv = match s.right.as_const() {
        Some(v) => v,
        None => return Ok(None),
    };
    let arr = match rv {
        Datum::Null => return Ok(Some(Datum::Null)),
        Datum::Array(a) => a,
        _ => return Ok(None),
    };
    if lv.is_null() && s.op.strict {
        return Ok(Some(Datum::Null));
    }
    if arr.items.is_empty() {
        return Ok(Some(Datum::Bool(!s.use_or)));
    }
    let h = match scalar_handle(s.op.proc) {
        Some(h) => h,
        None => return Ok(None),
    };
    let types = [s.left.ty, TypeId::of(arr.elem)];
    let mut has_null = false;
    for item in &arr.items {
        let r = invoke(ctx, &h, &[lv.clone(), item.clone()], &types, TypeId::BOOL)?;
        match r {
            Datum::Null => has_null = true,
            Datum::Bool(b) => {
                if b == s.use_or {
                    return Ok(Some(Datum::Bool(s.use_or)));
                }
            }
            _ => return Ok(None),
        }
    }
    Ok(Some(if has_null { Datum::Null } else { Datum::Bool(!s.use_or) }))
}

/// Datum values of `args` when every argument is a constant.
fn const_values(args: &[BoundExpr]) -> Option<Vec<Datum>> {
    args.iter().map(|a| a.as_const().cloned()).collect()
}

/// The builtin implementation of `proc` when it is a plain scalar function (else `None`: the node
/// is left for the executor, which reports the same error at run time).
fn scalar_handle(proc: Oid) -> Option<FuncHandle> {
    match func::implementation(proc) {
        Ok(h) => {
            let is_scalar = matches!(h.imp, FuncImpl::Scalar(_));
            if is_scalar {
                Some(h)
            } else {
                None
            }
        }
        Err(_) => None,
    }
}

/// Run `f` with a function context for constant evaluation: catalog types from the planner's
/// environment, pinned I/O settings, and a deterministic session.
fn with_ctx<R>(
    ctx: &PlanContext,
    arg_types: &[TypeId],
    result_type: TypeId,
    f: impl FnOnce(&FuncCtx) -> Result<R>,
) -> Result<R> {
    let io = IoConfig::default();
    let session = TestSession::default();
    let fctx = FuncCtx { env: ctx.env, io: &io, session: &session, arg_types, result_type, collation: 0 };
    f(&fctx)
}

fn invoke(
    ctx: &PlanContext,
    h: &FuncHandle,
    args: &[Datum],
    arg_types: &[TypeId],
    result_type: TypeId,
) -> Result<Datum> {
    with_ctx(ctx, arg_types, result_type, |fc| h.call(args, fc))
}

/// A cast can be folded when evaluating it at plan time cannot differ from run time: every
/// function is immutable, no domain constraint is pending, and no text-to-clock input (which reads
/// the transaction clock for 'now', 'today', ...) is involved.
fn cast_foldable(plan: &CastPlan) -> bool {
    plan.steps.iter().all(step_foldable)
}

fn step_foldable(step: &CastStep) -> bool {
    match step {
        CastStep::Relabel | CastStep::Typmod { .. } => true,
        CastStep::DomainCheck { .. } | CastStep::RecordCoerce { .. } => false,
        CastStep::Func { proc, .. } => match builtin_proc(*proc) {
            Some(p) => p.volatility == Volatility::Immutable,
            None => false,
        },
        CastStep::ViaIo { from, to } => !(is_text_like(from.oid) && is_clock_type(to.oid)),
        CastStep::ArrayCoerce { elem_plan } => cast_foldable(elem_plan),
    }
}

fn is_text_like(oid: Oid) -> bool {
    [TypeId::TEXT.oid, TypeId::VARCHAR.oid, TypeId::BPCHAR.oid, TypeId::NAME.oid, TypeId::UNKNOWN.oid]
        .contains(&oid)
}

fn is_clock_type(oid: Oid) -> bool {
    [TypeId::DATE.oid, TypeId::TIME.oid, TypeId::TIMETZ.oid, TypeId::TIMESTAMP.oid, TypeId::TIMESTAMPTZ.oid]
        .contains(&oid)
}

// ---------------------------------------------------------------------------------------------
// Structural rewrites (no evaluation)
// ---------------------------------------------------------------------------------------------

/// Structural rewrites of a node whose children are already simplified.
fn rewrite(e: BoundExpr) -> BoundExpr {
    let BoundExpr { kind, ty } = e;
    match kind {
        ExprKind::Bool(b) => {
            let BoolExpr { op, args } = *b;
            match op {
                BoolOp::And => simplify_bool(args, ty, true),
                BoolOp::Or => simplify_bool(args, ty, false),
                BoolOp::Not => {
                    if args.len() == 1 {
                        let mut args = args;
                        negate(args.remove(0))
                    } else {
                        BoundExpr { kind: ExprKind::Bool(Box::new(BoolExpr { op, args })), ty }
                    }
                }
            }
        }
        ExprKind::Cast(c) => rewrite_cast(*c, ty),
        kind => BoundExpr { kind, ty },
    }
}

/// AND (`is_and`) or OR of `args`: flatten same-operator children, absorb constants.
fn simplify_bool(args: Vec<BoundExpr>, ty: TypeId, is_and: bool) -> BoundExpr {
    let mut out: Vec<BoundExpr> = Vec::with_capacity(args.len());
    let mut have_null = false;
    let mut forced = false;
    for a in args {
        absorb(a, is_and, &mut out, &mut have_null, &mut forced);
    }
    if forced {
        // AND absorbs to FALSE, OR to TRUE.
        return BoundExpr::constant(Datum::Bool(!is_and), ty);
    }
    if have_null {
        out.push(BoundExpr::null(ty));
    }
    match out.len() {
        0 => BoundExpr::constant(Datum::Bool(is_and), ty),
        1 => out.remove(0),
        _ => BoundExpr {
            kind: ExprKind::Bool(Box::new(BoolExpr {
                op: if is_and { BoolOp::And } else { BoolOp::Or },
                args: out,
            })),
            ty,
        },
    }
}

fn absorb(e: BoundExpr, is_and: bool, out: &mut Vec<BoundExpr>, have_null: &mut bool, forced: &mut bool) {
    let BoundExpr { kind, ty } = e;
    let same_op = if is_and { BoolOp::And } else { BoolOp::Or };
    match kind {
        ExprKind::Const(Datum::Null) => *have_null = true,
        ExprKind::Const(Datum::Bool(v)) => {
            // The absorbing element of AND is FALSE, of OR is TRUE; the identity is dropped.
            if v == !is_and {
                *forced = true;
            }
        }
        ExprKind::Bool(b) if b.op == same_op => {
            for x in b.args {
                absorb(x, is_and, out, have_null, forced);
            }
        }
        kind => out.push(BoundExpr { kind, ty }),
    }
}

/// `NOT x` for a simplified boolean `x`, pushing the negation into the node where that is exact
/// (PG `negate_clause`).
fn negate(x: BoundExpr) -> BoundExpr {
    let BoundExpr { kind, ty } = x;
    let kind = match kind {
        ExprKind::Const(Datum::Bool(v)) => return BoundExpr::constant(Datum::Bool(!v), ty),
        ExprKind::Const(Datum::Null) => return BoundExpr::constant(Datum::Null, ty),
        ExprKind::Bool(b) => {
            let BoolExpr { op, args } = *b;
            match op {
                BoolOp::Not if args.len() == 1 => {
                    let mut args = args;
                    return args.remove(0);
                }
                BoolOp::And | BoolOp::Or => {
                    let flipped = if op == BoolOp::And { BoolOp::Or } else { BoolOp::And };
                    let negated: Vec<BoundExpr> = args.into_iter().map(negate).collect();
                    return BoundExpr {
                        kind: ExprKind::Bool(Box::new(BoolExpr { op: flipped, args: negated })),
                        ty,
                    };
                }
                BoolOp::Not => ExprKind::Bool(Box::new(BoolExpr { op, args })),
            }
        }
        ExprKind::Op(o) => {
            let OpExpr { op, args } = *o;
            if args.len() == 2 {
                if let Some(nop) = negated_op(&op, args[0].ty.oid, Some(args[1].ty.oid)) {
                    return BoundExpr { kind: ExprKind::Op(Box::new(OpExpr { op: nop, args })), ty };
                }
            }
            ExprKind::Op(Box::new(OpExpr { op, args }))
        }
        ExprKind::ScalarArrayOp(s) => {
            let ScalarArrayOp { op, use_or, left, right } = *s;
            if let Some(nop) = negated_op(&op, left.ty.oid, None) {
                return BoundExpr {
                    kind: ExprKind::ScalarArrayOp(Box::new(ScalarArrayOp {
                        op: nop,
                        use_or: !use_or,
                        left,
                        right,
                    })),
                    ty,
                };
            }
            ExprKind::ScalarArrayOp(Box::new(ScalarArrayOp { op, use_or, left, right }))
        }
        ExprKind::NullTest(n) => {
            let NullTest { arg, is_null, row_semantics } = *n;
            // `NOT (row IS NULL)` is not `row IS NOT NULL` under row semantics.
            if !row_semantics {
                return BoundExpr {
                    kind: ExprKind::NullTest(Box::new(NullTest { arg, is_null: !is_null, row_semantics })),
                    ty,
                };
            }
            ExprKind::NullTest(Box::new(NullTest { arg, is_null, row_semantics }))
        }
        ExprKind::BooleanTest(b) => {
            let BooleanTest { arg, test } = *b;
            let flipped = match test {
                BoolTestKind::IsTrue => BoolTestKind::IsNotTrue,
                BoolTestKind::IsNotTrue => BoolTestKind::IsTrue,
                BoolTestKind::IsFalse => BoolTestKind::IsNotFalse,
                BoolTestKind::IsNotFalse => BoolTestKind::IsFalse,
                BoolTestKind::IsUnknown => BoolTestKind::IsNotUnknown,
                BoolTestKind::IsNotUnknown => BoolTestKind::IsUnknown,
            };
            return BoundExpr { kind: ExprKind::BooleanTest(Box::new(BooleanTest { arg, test: flipped })), ty };
        }
        ExprKind::DistinctFrom(d) => {
            // The result is never NULL, so `NOT (a IS DISTINCT FROM b)` is `a IS NOT DISTINCT FROM b`.
            let DistinctFrom { left, right, eq, negated } = *d;
            return BoundExpr {
                kind: ExprKind::DistinctFrom(Box::new(DistinctFrom { left, right, eq, negated: !negated })),
                ty,
            };
        }
        other => other,
    };
    BoundExpr::not(BoundExpr { kind, ty })
}

/// The operator that computes `NOT (x op y)` for a builtin comparison: its `negator`, provided
/// the operand types match exactly and the negator returns boolean. `right = None` skips the right
/// operand check (ScalarArrayOp, whose element type is the operator's right type).
fn negated_op(op: &OpRef, left: Oid, right: Option<Oid>) -> Option<OpRef> {
    let bo = builtin_operator(op.op)?;
    if bo.negator == 0 || bo.result != TypeId::BOOL.oid || bo.left != left {
        return None;
    }
    if let Some(r) = right {
        if bo.right != r {
            return None;
        }
    }
    let neg = builtin_operator(bo.negator)?;
    if neg.result != TypeId::BOOL.oid || neg.left != bo.left || neg.right != bo.right {
        return None;
    }
    let p = builtin_proc(neg.proc)?;
    Some(OpRef {
        op: neg.oid,
        proc: neg.proc,
        name: neg.name.to_string(),
        volatility: p.volatility,
        strict: p.strict,
    })
}

/// Cast cleanup for a node whose children are already simplified: identity casts disappear and a
/// relabel over a relabel is collapsed into one cast from the innermost operand.
fn rewrite_cast(c: CastExpr, ty: TypeId) -> BoundExpr {
    let CastExpr { arg, plan, form } = c;
    if !plan.is_noop() {
        return BoundExpr { kind: ExprKind::Cast(Box::new(CastExpr { arg, plan, form })), ty };
    }
    let BoundExpr { kind: akind, ty: aty } = arg;
    let arg = match akind {
        ExprKind::Cast(inner) if inner.plan.is_noop() => {
            let CastExpr { arg: inner_arg, .. } = *inner;
            inner_arg
        }
        other => BoundExpr { kind: other, ty: aty },
    };
    if arg.ty == ty {
        return arg;
    }
    let CastPlan { to, context, steps, .. } = plan;
    let plan = CastPlan { from: arg.ty, to, context, steps };
    BoundExpr { kind: ExprKind::Cast(Box::new(CastExpr { arg, plan, form })), ty }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::context::PlanOptions;
    use pgarbor_catalog::{testing, CatalogEnv};
    use pgarbor_common::SqlState;
    use pgarbor_sql::ir::{FuncCallExpr, NullTest as IrNullTest};
    use pgarbor_types::catalog_data::{operators_named, CastContext};
    use pgarbor_types::cast::plan_cast;
    use std::collections::HashMap;

    fn int(v: i32) -> BoundExpr {
        BoundExpr::constant(Datum::Int4(v), TypeId::INT4)
    }

    fn col(attnum: i16, ty: TypeId) -> BoundExpr {
        BoundExpr::column(0, 1, attnum, ty)
    }

    fn find_op(name: &str, left: TypeId, right: TypeId) -> OpRef {
        let bo = operators_named(name)
            .find(|o| o.left == left.oid && o.right == right.oid)
            .expect("builtin operator for test");
        let p = builtin_proc(bo.proc).expect("builtin proc for test");
        OpRef { op: bo.oid, proc: bo.proc, name: bo.name.to_string(), volatility: p.volatility, strict: p.strict }
    }

    fn binop(name: &str, l: BoundExpr, r: BoundExpr, ty: TypeId) -> BoundExpr {
        let o = find_op(name, l.ty, r.ty);
        BoundExpr { kind: ExprKind::Op(Box::new(OpExpr { op: o, args: vec![l, r] })), ty }
    }

    fn simplify_with(env: &CatalogEnv, e: BoundExpr, params: Option<Vec<Datum>>) -> Result<BoundExpr> {
        let ctx = PlanContext {
            env,
            param_types: Vec::new(),
            param_values: params,
            options: PlanOptions::default(),
            table_rows: HashMap::new(),
        };
        let mut st = PlannerState::new(&ctx);
        simplify_expr(&mut st, e)
    }

    fn simplify(e: BoundExpr) -> Result<BoundExpr> {
        let (_kv, env) = testing::bootstrapped()?;
        simplify_with(&env, e, None)
    }

    #[test]
    fn folds_int_arithmetic() -> Result<()> {
        let out = simplify(binop("+", int(1), int(2), TypeId::INT4))?;
        assert_eq!(out, int(3));
        Ok(())
    }

    #[test]
    fn folds_inside_comparison() -> Result<()> {
        let input = binop(">", col(1, TypeId::INT4), binop("+", int(1), int(1), TypeId::INT4), TypeId::BOOL);
        let expected = binop(">", col(1, TypeId::INT4), int(2), TypeId::BOOL);
        assert_eq!(simplify(input)?, expected);
        Ok(())
    }

    #[test]
    fn boolean_constants_absorb() -> Result<()> {
        let x = col(1, TypeId::BOOL);
        assert_eq!(simplify(BoundExpr::and_all(vec![BoundExpr::bool_const(true), x.clone()]))?, x);
        assert_eq!(
            simplify(BoundExpr::and_all(vec![BoundExpr::bool_const(false), x.clone()]))?,
            BoundExpr::bool_const(false)
        );
        assert_eq!(
            simplify(BoundExpr::or_all(vec![x.clone(), BoundExpr::bool_const(true)]))?,
            BoundExpr::bool_const(true)
        );
        Ok(())
    }

    #[test]
    fn null_operands_in_boolean_ops() -> Result<()> {
        let x = col(1, TypeId::BOOL);
        let null = BoundExpr::null(TypeId::BOOL);
        assert_eq!(
            simplify(BoundExpr::and_all(vec![null.clone(), BoundExpr::bool_const(false)]))?,
            BoundExpr::bool_const(false)
        );
        assert_eq!(simplify(BoundExpr::and_all(vec![null.clone(), BoundExpr::bool_const(true)]))?, null);
        assert_eq!(
            simplify(BoundExpr::or_all(vec![null.clone(), BoundExpr::bool_const(true)]))?,
            BoundExpr::bool_const(true)
        );
        // NULL AND x keeps the node.
        let kept = simplify(BoundExpr::and_all(vec![null.clone(), x.clone()]))?;
        assert!(matches!(kept.kind, ExprKind::Bool(ref b) if b.op == BoolOp::And && b.args.len() == 2));
        Ok(())
    }

    #[test]
    fn stable_function_is_not_folded() -> Result<()> {
        let call = BoundExpr::new(
            ExprKind::FuncCall(Box::new(FuncCallExpr {
                func: 1299,
                name: "now".to_string(),
                args: Vec::new(),
                variadic: false,
                retset: false,
                volatility: Volatility::Stable,
                strict: true,
                is_cast_call: false,
            })),
            TypeId::of(1184),
        );
        let out = simplify(call)?;
        assert!(matches!(out.kind, ExprKind::FuncCall(_)));
        Ok(())
    }

    #[test]
    fn division_by_zero_surfaces_22012() {
        let r = simplify(binop("/", int(1), int(0), TypeId::INT4));
        assert!(matches!(r, Err(ref e) if e.sqlstate == SqlState::DIVISION_BY_ZERO));
    }

    #[test]
    fn searched_case_with_true_condition() -> Result<()> {
        let case = BoundExpr::new(
            ExprKind::Case(Box::new(CaseExpr {
                arg: None,
                arms: vec![CaseArm { cond: BoundExpr::bool_const(true), result: int(1) }],
                default: Some(Box::new(int(2))),
            })),
            TypeId::INT4,
        );
        assert_eq!(simplify(case)?, int(1));
        Ok(())
    }

    #[test]
    fn dead_case_arm_is_not_evaluated() -> Result<()> {
        // CASE WHEN false THEN 1/0 ELSE 1 END -> 1 (PG never folds the dead result).
        let case = BoundExpr::new(
            ExprKind::Case(Box::new(CaseExpr {
                arg: None,
                arms: vec![CaseArm {
                    cond: BoundExpr::bool_const(false),
                    result: binop("/", int(1), int(0), TypeId::INT4),
                }],
                default: Some(Box::new(int(1))),
            })),
            TypeId::INT4,
        );
        assert_eq!(simplify(case)?, int(1));
        Ok(())
    }

    #[test]
    fn simple_case_substitutes_constant_operand() -> Result<()> {
        // CASE 0 WHEN 0 THEN 1 ELSE 1/0 END -> 1
        let test_value = BoundExpr::new(ExprKind::CaseTestValue, TypeId::INT4);
        let case = BoundExpr::new(
            ExprKind::Case(Box::new(CaseExpr {
                arg: Some(Box::new(int(0))),
                arms: vec![CaseArm {
                    cond: binop("=", test_value, int(0), TypeId::BOOL),
                    result: int(1),
                }],
                default: Some(Box::new(binop("/", int(1), int(0), TypeId::INT4))),
            })),
            TypeId::INT4,
        );
        assert_eq!(simplify(case)?, int(1));
        Ok(())
    }

    #[test]
    fn coalesce_drops_leading_null() -> Result<()> {
        let x = col(1, TypeId::INT4);
        let c = BoundExpr::new(
            ExprKind::Coalesce(vec![BoundExpr::null(TypeId::INT4), x.clone()]),
            TypeId::INT4,
        );
        assert_eq!(simplify(c)?, x);
        Ok(())
    }

    #[test]
    fn negation_uses_negator_operator() -> Result<()> {
        let input = BoundExpr::not(binop("<", col(1, TypeId::INT4), int(1), TypeId::BOOL));
        let expected = binop(">=", col(1, TypeId::INT4), int(1), TypeId::BOOL);
        assert_eq!(simplify(input)?, expected);
        Ok(())
    }

    #[test]
    fn double_negation_cancels() -> Result<()> {
        let x = col(1, TypeId::BOOL);
        assert_eq!(simplify(BoundExpr::not(BoundExpr::not(x.clone())))?, x);
        Ok(())
    }

    #[test]
    fn negated_null_test_flips() -> Result<()> {
        let x = col(1, TypeId::INT4);
        let is_null = BoundExpr::new(
            ExprKind::NullTest(Box::new(IrNullTest { arg: x.clone(), is_null: true, row_semantics: false })),
            TypeId::BOOL,
        );
        let expected = BoundExpr::new(
            ExprKind::NullTest(Box::new(IrNullTest { arg: x, is_null: false, row_semantics: false })),
            TypeId::BOOL,
        );
        assert_eq!(simplify(BoundExpr::not(is_null))?, expected);
        Ok(())
    }

    #[test]
    fn null_test_on_constant_folds() -> Result<()> {
        let t = BoundExpr::new(
            ExprKind::NullTest(Box::new(IrNullTest { arg: int(1), is_null: true, row_semantics: false })),
            TypeId::BOOL,
        );
        assert_eq!(simplify(t)?, BoundExpr::bool_const(false));
        Ok(())
    }

    #[test]
    fn params_fold_only_with_values() -> Result<()> {
        let (_kv, env) = testing::bootstrapped()?;
        let p = BoundExpr::param(1, TypeId::INT4);
        assert_eq!(simplify_with(&env, p.clone(), Some(vec![Datum::Int4(5)]))?, int(5));
        assert_eq!(simplify_with(&env, p.clone(), None)?, p);
        Ok(())
    }

    #[test]
    fn constant_cast_folds() -> Result<()> {
        let (_kv, env) = testing::bootstrapped()?;
        let plan = plan_cast(&env, TypeId::INT4, TypeId::INT8, CastContext::Explicit)?;
        let cast = BoundExpr::new(
            ExprKind::Cast(Box::new(CastExpr { arg: int(1), plan, form: pgarbor_sql::ir::CoercionForm::Explicit })),
            TypeId::INT8,
        );
        assert_eq!(simplify_with(&env, cast, None)?, BoundExpr::constant(Datum::Int8(1), TypeId::INT8));
        Ok(())
    }

    #[test]
    fn quals_drop_true_and_collapse_on_false() -> Result<()> {
        let (_kv, env) = testing::bootstrapped()?;
        let ctx = PlanContext {
            env: &env,
            param_types: Vec::new(),
            param_values: None,
            options: PlanOptions::default(),
            table_rows: HashMap::new(),
        };
        let mut st = PlannerState::new(&ctx);
        let cmp = binop("=", col(1, TypeId::INT4), int(1), TypeId::BOOL);
        let kept = simplify_quals(
            &mut st,
            vec![BoundExpr::bool_const(true), cmp.clone(), col(2, TypeId::BOOL)],
        )?;
        assert_eq!(kept, vec![cmp.clone(), col(2, TypeId::BOOL)]);
        let collapsed = simplify_quals(&mut st, vec![cmp, BoundExpr::bool_const(false)])?;
        assert_eq!(collapsed, vec![BoundExpr::bool_const(false)]);
        Ok(())
    }

    #[test]
    fn split_and_make_round_trip() {
        let a = col(1, TypeId::BOOL);
        let b = col(2, TypeId::BOOL);
        let c = col(3, TypeId::BOOL);
        let all = BoundExpr::and_all(vec![a.clone(), BoundExpr::and_all(vec![b.clone(), c.clone()])]);
        assert_eq!(split_conjuncts(all), vec![a.clone(), b, c]);
        assert_eq!(make_and(Vec::new()), None);
        assert_eq!(make_and(vec![a.clone()]), Some(a));
    }
}
