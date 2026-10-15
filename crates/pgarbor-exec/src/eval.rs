//! Expression evaluator, part 1: dispatcher and the "control-flow" expression kinds (lease EXE-05,
//! north-star). Related files: `eval_call` (functions, operators, ANY/ALL, row comparison),
//! `eval_cast` (casts, domains, constructors, subscripts), `subplan` (SubPlan), `eval_srf`
//! (set-returning functions).
//!
//! The evaluated tree is an EXECUTABLE `BoundExpr` (plan.rs module docs): no `Column`, `SubLink`,
//! `Aggref`, `WindowFunc`, `SetToDefault`; `Grouping` only with `mask_slot: Some`. Meeting one of
//! those is XX000 via `error::not_executable("<kind>")`. The row argument is the evaluating node's
//! INPUT ROW (`InputRef(i)` = `row[i]`); `OuterRef` reads `Executor::outer_value`.
//!
//! ## Semantics per kind (PG: `executor/execExprInterp.c`, `nodes/primnodes.h`)
//! * `Const(d)`: `d.clone()`. `Param(n)` (1-based): `ex.cx.se.params[n-1].clone()`; `n` beyond the
//!   supplied values -> 08P01 `bind message supplies {have} parameters, but prepared statement
//!   requires {n}`.
//! * `InputRef(i)`: `row[i]` (out of range -> XX000 `input reference {i} out of range`).
//! * `Bool`: three-valued with PG's evaluation order and SHORT-CIRCUITING: `AND` evaluates arguments
//!   left to right and stops at the first FALSE (a later erroring argument is never evaluated);
//!   NULL is remembered and yields NULL only if no FALSE is found; `OR` stops at the first TRUE;
//!   `NOT` of NULL is NULL. Arguments must be boolean (`logic::datum_to_bool3`).
//! * `NullTest`: scalar: `arg IS [NOT] NULL`. `row_semantics` (composite `Datum::Record`): `IS NULL`
//!   is true iff EVERY field is NULL; `IS NOT NULL` is true iff EVERY field is non-NULL (a record with
//!   one NULL field is neither). A NULL record itself counts as all-NULL. Never returns NULL.
//! * `BooleanTest`: `IS TRUE/NOT TRUE/FALSE/NOT FALSE/UNKNOWN/NOT UNKNOWN`; never NULL.
//! * `DistinctFrom`: `compare::not_distinct(env, left.ty, l, r)` (types are equal after binding);
//!   `negated` flips: IS NOT DISTINCT FROM. Never NULL.
//! * `Case`: if `arg` is present evaluate it once and push it on `rt.case_stack` while evaluating the
//!   arms' conditions (`CaseTestValue` reads the top). Arms evaluated in order, the first whose
//!   condition is TRUE returns its result (lazy: other results are not evaluated); no match -> the
//!   `default` or NULL.
//! * `Coalesce`: first non-NULL argument (lazy). `NullIf(a, b)`: evaluate `a`; if NULL -> NULL;
//!   evaluate `b`; `a = b` via the `eq` operator (`eval_call::call_operator`) TRUE -> NULL else `a`.
//! * `MinMax` (GREATEST/LEAST): NULL arguments are ignored; NULL only if all are NULL; comparison with
//!   `compare::cmp_datums(env, e.ty, ..)`; ties keep the first.
//! * `Collate(arg)`: evaluates `arg` (v1 sorts by code point).
//! * `SqlValue`: `CurrentDate` (date of `txn_ts` in the session TimeZone), `CurrentTime` (timetz),
//!   `CurrentTimestamp` (timestamptz = `txn_ts`; apply the precision in `e.ty.typmod` by casting to
//!   `e.ty`), `LocalTime` (time), `LocalTimestamp` (timestamp), use `cast::cast_datum` from
//!   timestamptz with the session `IoConfig` time zone; `CurrentUser`/`CurrentRole`/`User` -> name of
//!   `env.current_user()`, `SessionUser` -> `env.session_user()`, `CurrentCatalog` ->
//!   `se.services.database_name()`, `CurrentSchema` -> name of the first existing schema of the
//!   search path (NULL if none). Names are `Datum::Text`.
//! * `Grouping{ args, mask_slot: Some(s) }`: `mask = row[s]` (Int8); result Int4: for each arg `a`
//!   (position in `AggregateNode::group_keys`) in order, `result = (result << 1) | ((mask >> a) & 1)`.
//! * `FuncCall`/`Op`/`ScalarArrayOp`/`RowCompare` -> `eval_call`; `Cast`/`CoerceToDomain`/`Array`/`Row`/
//!   `FieldSelect`/`Subscript` -> `eval_cast`; `SubPlan` -> `subplan::eval_subplan`.
//! * `eval` checks `ex.check_interrupts()` only in loops (callers do it per row), not per node.
use std::cmp::Ordering;

use pgarbor_common::{Oid, PgError, Result, SqlState};
use pgarbor_sql::ir::{
    BoolExpr, BoolOp, BooleanTest, BoolTestKind, BoundExpr, CaseExpr, DistinctFrom, ExprKind, GroupingExpr,
    MinMaxExpr, NullIfExpr, NullTest, SqlValueOp, SubPlanRef,
};
use pgarbor_types::cast::{cast_datum, CastContext};
use pgarbor_types::compare::{cmp_datums, not_distinct};
use pgarbor_types::logic::{and3, bool3_to_datum, datum_to_bool3, is_false, is_true, is_unknown, not3, or3, qualifies};
use pgarbor_types::{Datum, TypeId};

use crate::context::ExecContext;
use crate::eval_call::{self, with_func_ctx};
use crate::eval_cast;
use crate::runtime::{Executor, Runtime};
use crate::subplan;

/// Evaluate `e` over `row` (module docs).
pub fn eval(ex: &mut Executor, e: &BoundExpr, row: &[Datum]) -> Result<Datum> {
    match &e.kind {
        ExprKind::Const(d) => Ok(d.clone()),
        ExprKind::Param(n) => eval_param(ex, *n),
        ExprKind::InputRef(i) => row
            .get(*i as usize)
            .cloned()
            .ok_or_else(|| PgError::internal(format!("input reference {i} out of range"))),
        ExprKind::OuterRef(o) => ex.outer_value(o.levels_up, o.index),
        ExprKind::Column(_) => Err(crate::error::not_executable("Column")),
        ExprKind::Aggref(_) => Err(crate::error::not_executable("Aggref")),
        ExprKind::WindowFunc(_) => Err(crate::error::not_executable("WindowFunc")),
        ExprKind::SubLink(_) => Err(crate::error::not_executable("SubLink")),
        ExprKind::SetToDefault { .. } => Err(crate::error::not_executable("SetToDefault")),
        ExprKind::Grouping(g) => eval_grouping(ex, g, row),
        ExprKind::FuncCall(f) => eval_call::eval_func_call(ex, f, e.ty, row),
        ExprKind::Op(o) => eval_call::eval_op(ex, o, e.ty, row),
        ExprKind::Bool(b) => Ok(bool3_to_datum(eval_connective(ex, b, row)?)),
        ExprKind::NullTest(n) => eval_null_test(ex, n, row),
        ExprKind::BooleanTest(t) => eval_boolean_test(ex, t, row),
        ExprKind::DistinctFrom(d) => eval_distinct_from(ex, d, row),
        ExprKind::Case(c) => eval_case(ex, c, row),
        ExprKind::CaseTestValue => ex
            .rt
            .case_stack
            .last()
            .cloned()
            .ok_or_else(|| PgError::internal("CASE test value used outside of a CASE expression")),
        ExprKind::Coalesce(args) => eval_coalesce(ex, args, row),
        ExprKind::NullIf(n) => eval_nullif(ex, n, row),
        ExprKind::MinMax(m) => eval_minmax(ex, m, e.ty, row),
        ExprKind::Cast(c) => eval_cast::eval_cast(ex, c, e.ty, row),
        ExprKind::CoerceToDomain(c) => eval_cast::eval_coerce_to_domain(ex, c, e.ty, row),
        ExprKind::Collate(c) => eval(ex, &c.arg, row),
        ExprKind::Array(a) => eval_cast::eval_array(ex, a, row),
        ExprKind::Row(r) => eval_cast::eval_row(ex, r, row),
        ExprKind::FieldSelect(f) => eval_cast::eval_field_select(ex, f, row),
        ExprKind::Subscript(s) => eval_cast::eval_subscript(ex, s, e.ty, row),
        ExprKind::ScalarArrayOp(s) => eval_call::eval_scalar_array_op(ex, s, row),
        ExprKind::RowCompare(r) => eval_call::eval_row_compare(ex, r, row),
        ExprKind::SubPlan(sp) => eval_sub_plan(ex, sp, row),
        ExprKind::SqlValue(op) => eval_sql_value(ex, *op, e.ty),
    }
}

/// `$n` (1-based) from the session parameter values (module docs).
fn eval_param(ex: &Executor, n: u32) -> Result<Datum> {
    if n == 0 {
        return Err(PgError::internal("parameter number 0 is not valid"));
    }
    let se = ex.se();
    match se.params.get((n - 1) as usize) {
        Some(v) => Ok(v.clone()),
        None => Err(PgError::new(
            SqlState::PROTOCOL_VIOLATION,
            format!("bind message supplies {} parameters, but prepared statement requires {}", se.params.len(), n),
        )),
    }
}

/// AND / OR / NOT with PG's left-to-right short-circuit evaluation (module docs).
fn eval_connective(ex: &mut Executor, b: &BoolExpr, row: &[Datum]) -> Result<Option<bool>> {
    match b.op {
        BoolOp::And => {
            let mut acc = Some(true);
            for a in &b.args {
                acc = and3(acc, eval_bool3(ex, a, row)?);
                if acc == Some(false) {
                    break;
                }
            }
            Ok(acc)
        }
        BoolOp::Or => {
            let mut acc = Some(false);
            for a in &b.args {
                acc = or3(acc, eval_bool3(ex, a, row)?);
                if acc == Some(true) {
                    break;
                }
            }
            Ok(acc)
        }
        BoolOp::Not => match b.args.first() {
            Some(a) => Ok(not3(eval_bool3(ex, a, row)?)),
            None => Err(PgError::internal("NOT expression without an argument")),
        },
    }
}

/// `x IS [NOT] NULL` (module docs for row semantics).
fn eval_null_test(ex: &mut Executor, n: &NullTest, row: &[Datum]) -> Result<Datum> {
    let v = eval(ex, &n.arg, row)?;
    let r = if !n.row_semantics {
        v.is_null() == n.is_null
    } else {
        match &v {
            Datum::Record(fields) => {
                if n.is_null {
                    fields.iter().all(|f| f.is_null())
                } else {
                    fields.iter().all(|f| !f.is_null())
                }
            }
            Datum::Null => n.is_null,
            _ => v.is_null() == n.is_null,
        }
    };
    Ok(Datum::Bool(r))
}

/// `x IS [NOT] TRUE|FALSE|UNKNOWN` (never NULL).
fn eval_boolean_test(ex: &mut Executor, t: &BooleanTest, row: &[Datum]) -> Result<Datum> {
    let v = eval(ex, &t.arg, row)?;
    let b = datum_to_bool3(&v)?;
    let r = match t.test {
        BoolTestKind::IsTrue => is_true(b),
        BoolTestKind::IsNotTrue => !is_true(b),
        BoolTestKind::IsFalse => is_false(b),
        BoolTestKind::IsNotFalse => !is_false(b),
        BoolTestKind::IsUnknown => is_unknown(b),
        BoolTestKind::IsNotUnknown => !is_unknown(b),
    };
    Ok(Datum::Bool(r))
}

/// `a IS [NOT] DISTINCT FROM b` (never NULL): `IS NOT DISTINCT` is `not_distinct`, `IS DISTINCT`
/// its negation.
fn eval_distinct_from(ex: &mut Executor, d: &DistinctFrom, row: &[Datum]) -> Result<Datum> {
    let l = eval(ex, &d.left, row)?;
    let r = eval(ex, &d.right, row)?;
    let same = not_distinct(&ex.se().env, d.left.ty, &l, &r)?;
    Ok(Datum::Bool(same == d.negated))
}

/// Searched and simple CASE (module docs). The simple operand is evaluated once and kept on
/// `rt.case_stack` while the arm conditions run.
fn eval_case(ex: &mut Executor, c: &CaseExpr, row: &[Datum]) -> Result<Datum> {
    let pushed = match &c.arg {
        Some(arg) => {
            let v = eval(ex, arg, row)?;
            ex.rt.case_stack.push(v);
            true
        }
        None => false,
    };
    let result = eval_case_arms(ex, c, row);
    if pushed {
        ex.rt.case_stack.pop();
    }
    result
}

fn eval_case_arms(ex: &mut Executor, c: &CaseExpr, row: &[Datum]) -> Result<Datum> {
    for arm in &c.arms {
        if eval_bool3(ex, &arm.cond, row)? == Some(true) {
            return eval(ex, &arm.result, row);
        }
    }
    match &c.default {
        Some(d) => eval(ex, d, row),
        None => Ok(Datum::Null),
    }
}

/// First non-NULL argument; later arguments are not evaluated.
fn eval_coalesce(ex: &mut Executor, args: &[BoundExpr], row: &[Datum]) -> Result<Datum> {
    for a in args {
        let v = eval(ex, a, row)?;
        if !v.is_null() {
            return Ok(v);
        }
    }
    Ok(Datum::Null)
}

/// `NULLIF(a, b)`: NULL when `a` is NULL (`b` is not evaluated then); otherwise NULL when `a = b`
/// is TRUE, else `a`.
fn eval_nullif(ex: &mut Executor, n: &NullIfExpr, row: &[Datum]) -> Result<Datum> {
    let l = eval(ex, &n.left, row)?;
    if l.is_null() {
        return Ok(Datum::Null);
    }
    let r = eval(ex, &n.right, row)?;
    let eq = eval_call::call_operator(ex, &n.eq, &[l.clone(), r], &[n.left.ty, n.right.ty], TypeId::BOOL)?;
    if datum_to_bool3(&eq)? == Some(true) {
        Ok(Datum::Null)
    } else {
        Ok(l)
    }
}

/// GREATEST / LEAST: NULLs are skipped; ties keep the first non-NULL argument.
fn eval_minmax(ex: &mut Executor, m: &MinMaxExpr, ty: TypeId, row: &[Datum]) -> Result<Datum> {
    let mut best: Option<Datum> = None;
    for a in &m.args {
        let v = eval(ex, a, row)?;
        if v.is_null() {
            continue;
        }
        let replace = match &best {
            None => true,
            Some(cur) => {
                let ord = cmp_datums(&ex.se().env, ty, &v, cur)?;
                if m.greatest {
                    ord == Ordering::Greater
                } else {
                    ord == Ordering::Less
                }
            }
        };
        if replace {
            best = Some(v);
        }
    }
    Ok(best.unwrap_or(Datum::Null))
}

/// `GROUPING(args)` from the mask column of the Aggregate node (module docs).
fn eval_grouping(_ex: &mut Executor, g: &GroupingExpr, row: &[Datum]) -> Result<Datum> {
    let slot = match g.mask_slot {
        Some(s) => s as usize,
        None => return Err(crate::error::not_executable("Grouping")),
    };
    let mask: i64 = match row.get(slot) {
        Some(Datum::Int8(m)) => *m,
        _ => return Err(PgError::internal(format!("grouping mask slot {slot} is not int8"))),
    };
    let mut result: i64 = 0;
    for &a in &g.args {
        let bit = mask.checked_shr(a as u32).unwrap_or(0) & 1;
        result = (result << 1) | bit;
    }
    Ok(Datum::Int4(result as i32))
}

/// A sub-plan reference is only meaningful inside a plan whose runtime holds the sub-plan.
fn eval_sub_plan(ex: &mut Executor, sp: &SubPlanRef, row: &[Datum]) -> Result<Datum> {
    if sp.plan_id as usize >= ex.rt.subplans.len() {
        return Err(PgError::internal(format!("sub-plan {} is not available in this expression", sp.plan_id)));
    }
    subplan::eval_subplan(ex, sp, row)
}

/// The SQL-standard value functions (module docs).
fn eval_sql_value(ex: &mut Executor, op: SqlValueOp, ty: TypeId) -> Result<Datum> {
    match op {
        SqlValueOp::CurrentDate
        | SqlValueOp::CurrentTime
        | SqlValueOp::CurrentTimestamp
        | SqlValueOp::LocalTime
        | SqlValueOp::LocalTimestamp => {
            let ts = ex.se().txn_ts;
            with_func_ctx(&*ex.cx, &[TypeId::TIMESTAMPTZ], ty, |ctx| {
                cast_datum(Datum::TimestampTz(ts), TypeId::TIMESTAMPTZ, ty, CastContext::Explicit, ctx)
            })
        }
        SqlValueOp::CurrentRole | SqlValueOp::CurrentUser | SqlValueOp::User => {
            let oid = ex.se().env.current_user();
            Ok(Datum::Text(role_name(ex, oid)?))
        }
        SqlValueOp::SessionUser => {
            let oid = ex.se().env.session_user();
            Ok(Datum::Text(role_name(ex, oid)?))
        }
        SqlValueOp::CurrentCatalog => Ok(Datum::Text(ex.se().services.database_name())),
        SqlValueOp::CurrentSchema => {
            let se = ex.se();
            let env = &se.env;
            for oid in &env.path().explicit {
                if let Some(ns) = env.snapshot().namespace(*oid) {
                    return Ok(Datum::Text(ns.name.clone()));
                }
            }
            Ok(Datum::Null)
        }
    }
}

fn role_name(ex: &Executor, oid: Oid) -> Result<String> {
    match ex.se().env.snapshot().role_name(oid) {
        Some(name) => Ok(name.to_string()),
        None => Err(PgError::internal(format!("role with OID {oid} does not exist"))),
    }
}

/// Evaluate a boolean expression to three-valued logic (`None` = NULL). A non-boolean result is
/// XX000 `expression of type X used as a qualification`.
pub fn eval_bool3(ex: &mut Executor, e: &BoundExpr, row: &[Datum]) -> Result<Option<bool>> {
    if let ExprKind::Bool(b) = &e.kind {
        return eval_connective(ex, b, row);
    }
    match eval(ex, e, row)? {
        Datum::Null => Ok(None),
        Datum::Bool(b) => Ok(Some(b)),
        _ => {
            let name = pgarbor_types::names::format_type(&ex.se().env, e.ty.oid, e.ty.typmod)?;
            Err(PgError::internal(format!("expression of type {name} used as a qualification")))
        }
    }
}

/// WHERE/ON/HAVING semantics: true only for TRUE (NULL and FALSE both reject).
pub fn eval_qual(ex: &mut Executor, e: &BoundExpr, row: &[Datum]) -> Result<bool> {
    Ok(qualifies(eval_bool3(ex, e, row)?))
}

/// `eval_qual` for an optional qualification (`None` = TRUE).
pub fn eval_opt_qual(ex: &mut Executor, e: &Option<BoundExpr>, row: &[Datum]) -> Result<bool> {
    match e {
        Some(q) => eval_qual(ex, q, row),
        None => Ok(true),
    }
}

/// Evaluate a list of expressions (projection) over one row.
pub fn eval_exprs(ex: &mut Executor, exprs: &[BoundExpr], row: &[Datum]) -> Result<Vec<Datum>> {
    let mut out = Vec::with_capacity(exprs.len());
    for e in exprs {
        out.push(eval(ex, e, row)?);
    }
    Ok(out)
}

/// Evaluate an expression that contains no sub-plans outside any plan (column defaults in DDL
/// paths, CHECK validation, index builds): creates a throw-away `Runtime` and `Executor` over `cx`.
/// Sub-plans inside the expression are XX000.
pub fn eval_standalone(cx: &mut ExecContext, e: &BoundExpr, row: &[Datum]) -> Result<Datum> {
    let mut rt = Runtime::default();
    let mut ex = Executor::new(&mut rt, cx);
    eval(&mut ex, e, row)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::context::{NoGuc, NoServices, SessionEnv};
    use crate::memtxn::MemTxn;
    use pgarbor_sql::ir::{CaseArm, OpRef, SubLinkKind};
    use pgarbor_types::catalog_data::Volatility;
    use pgarbor_types::context::IoConfig;
    use std::cell::Cell;

    /// Evaluate `e` over `row` with `params` as the bound parameter values.
    fn run_with(e: &BoundExpr, row: &[Datum], params: Vec<Datum>) -> Result<Datum> {
        let (_kv, env) = pgarbor_catalog::testing::bootstrapped()?;
        let guc = NoGuc;
        let services = NoServices;
        let rng = Cell::new(1u64);
        let mut se = SessionEnv::new(env, IoConfig::default(), &guc, &services, &rng, 0, 0);
        se.params = params;
        let mut txn = MemTxn::new();
        let mut cx = ExecContext::new(&mut txn, se);
        let mut rt = Runtime::default();
        let mut ex = Executor::new(&mut rt, &mut cx);
        eval(&mut ex, e, row)
    }

    fn run(e: &BoundExpr) -> Result<Datum> {
        run_with(e, &[], Vec::new())
    }

    fn i4(v: i32) -> BoundExpr {
        BoundExpr::constant(Datum::Int4(v), TypeId::INT4)
    }
    fn b(v: bool) -> BoundExpr {
        BoundExpr::bool_const(v)
    }
    fn null_b() -> BoundExpr {
        BoundExpr::null(TypeId::BOOL)
    }
    fn null_i4() -> BoundExpr {
        BoundExpr::null(TypeId::INT4)
    }
    /// Fails when evaluated: slot 99 does not exist in the empty test row.
    fn boom_b() -> BoundExpr {
        BoundExpr::input_ref(99, TypeId::BOOL)
    }
    fn boom_i4() -> BoundExpr {
        BoundExpr::input_ref(99, TypeId::INT4)
    }
    fn minmax(greatest: bool, args: Vec<BoundExpr>) -> BoundExpr {
        BoundExpr::new(ExprKind::MinMax(Box::new(MinMaxExpr { greatest, args })), TypeId::INT4)
    }
    fn null_test(arg: BoundExpr, is_null: bool, row_semantics: bool) -> BoundExpr {
        BoundExpr::new(ExprKind::NullTest(Box::new(NullTest { arg, is_null, row_semantics })), TypeId::BOOL)
    }
    fn eq_op() -> OpRef {
        OpRef { op: 96, proc: 65, name: "=".to_string(), volatility: Volatility::Immutable, strict: true }
    }

    #[test]
    fn and_or_not_three_valued_with_short_circuit() {
        assert_eq!(run(&BoundExpr::and_all(vec![b(false), boom_b()])).unwrap(), Datum::Bool(false));
        assert_eq!(run(&BoundExpr::and_all(vec![null_b(), b(false)])).unwrap(), Datum::Bool(false));
        assert_eq!(run(&BoundExpr::and_all(vec![null_b(), b(true)])).unwrap(), Datum::Null);
        assert_eq!(run(&BoundExpr::or_all(vec![null_b(), b(true)])).unwrap(), Datum::Bool(true));
        assert_eq!(run(&BoundExpr::or_all(vec![b(true), boom_b()])).unwrap(), Datum::Bool(true));
        assert_eq!(run(&BoundExpr::or_all(vec![b(false), null_b()])).unwrap(), Datum::Null);
        assert_eq!(run(&BoundExpr::not(null_b())).unwrap(), Datum::Null);
        assert_eq!(run(&BoundExpr::not(b(false))).unwrap(), Datum::Bool(true));
        // TRUE AND <erroring> must evaluate the erroring argument and fail.
        assert!(run(&BoundExpr::and_all(vec![b(true), boom_b()])).is_err());
    }

    #[test]
    fn case_first_match_is_lazy() {
        let e = BoundExpr::new(
            ExprKind::Case(Box::new(CaseExpr {
                arg: None,
                arms: vec![
                    CaseArm { cond: b(true), result: i4(1) },
                    CaseArm { cond: boom_b(), result: boom_i4() },
                ],
                default: None,
            })),
            TypeId::INT4,
        );
        assert_eq!(run(&e).unwrap(), Datum::Int4(1));
    }

    #[test]
    fn case_without_match_uses_default_or_null() {
        let arm = CaseArm { cond: b(false), result: i4(1) };
        let with_default = BoundExpr::new(
            ExprKind::Case(Box::new(CaseExpr { arg: None, arms: vec![arm.clone()], default: Some(Box::new(i4(9))) })),
            TypeId::INT4,
        );
        assert_eq!(run(&with_default).unwrap(), Datum::Int4(9));
        let no_default = BoundExpr::new(
            ExprKind::Case(Box::new(CaseExpr { arg: None, arms: vec![arm], default: None })),
            TypeId::INT4,
        );
        assert_eq!(run(&no_default).unwrap(), Datum::Null);
    }

    #[test]
    fn simple_case_reads_case_test_value() {
        let test_val = BoundExpr::new(ExprKind::CaseTestValue, TypeId::BOOL);
        let cond = BoundExpr::new(
            ExprKind::BooleanTest(Box::new(BooleanTest { arg: test_val, test: BoolTestKind::IsTrue })),
            TypeId::BOOL,
        );
        let e = BoundExpr::new(
            ExprKind::Case(Box::new(CaseExpr {
                arg: Some(Box::new(b(true))),
                arms: vec![CaseArm { cond, result: i4(7) }],
                default: Some(Box::new(i4(0))),
            })),
            TypeId::INT4,
        );
        assert_eq!(run(&e).unwrap(), Datum::Int4(7));
    }

    #[test]
    fn case_test_value_outside_case_is_error() {
        let e = BoundExpr::new(ExprKind::CaseTestValue, TypeId::BOOL);
        assert!(run(&e).is_err());
    }

    #[test]
    fn coalesce_stops_at_first_non_null() {
        let e = BoundExpr::new(ExprKind::Coalesce(vec![null_i4(), i4(2), boom_i4()]), TypeId::INT4);
        assert_eq!(run(&e).unwrap(), Datum::Int4(2));
    }

    #[test]
    fn nullif_null_left_skips_right() {
        let e = BoundExpr::new(
            ExprKind::NullIf(Box::new(NullIfExpr { left: null_i4(), right: boom_i4(), eq: eq_op() })),
            TypeId::INT4,
        );
        assert_eq!(run(&e).unwrap(), Datum::Null);
    }

    #[test]
    fn greatest_least_ignore_nulls() {
        assert_eq!(run(&minmax(true, vec![i4(1), null_i4(), i4(3)])).unwrap(), Datum::Int4(3));
        assert_eq!(run(&minmax(true, vec![null_i4(), null_i4()])).unwrap(), Datum::Null);
        assert_eq!(run(&minmax(false, vec![i4(3), null_i4(), i4(1)])).unwrap(), Datum::Int4(1));
    }

    #[test]
    fn null_tests_scalar_and_row_semantics() {
        assert_eq!(run(&null_test(null_i4(), true, false)).unwrap(), Datum::Bool(true));
        assert_eq!(run(&null_test(i4(1), true, false)).unwrap(), Datum::Bool(false));
        assert_eq!(run(&null_test(i4(1), false, false)).unwrap(), Datum::Bool(true));
        // ROW(1, NULL): neither IS NULL nor IS NOT NULL.
        let row = BoundExpr::constant(Datum::Record(vec![Datum::Int4(1), Datum::Null]), TypeId::RECORD);
        assert_eq!(run(&null_test(row.clone(), true, true)).unwrap(), Datum::Bool(false));
        assert_eq!(run(&null_test(row, false, true)).unwrap(), Datum::Bool(false));
        // ROW(NULL, NULL) IS NULL is true.
        let all_null = BoundExpr::constant(Datum::Record(vec![Datum::Null, Datum::Null]), TypeId::RECORD);
        assert_eq!(run(&null_test(all_null, true, true)).unwrap(), Datum::Bool(true));
    }

    #[test]
    fn boolean_tests_never_return_null() {
        let t = |arg: BoundExpr, test: BoolTestKind| {
            BoundExpr::new(ExprKind::BooleanTest(Box::new(BooleanTest { arg, test })), TypeId::BOOL)
        };
        assert_eq!(run(&t(null_b(), BoolTestKind::IsUnknown)).unwrap(), Datum::Bool(true));
        assert_eq!(run(&t(null_b(), BoolTestKind::IsNotTrue)).unwrap(), Datum::Bool(true));
        assert_eq!(run(&t(null_b(), BoolTestKind::IsFalse)).unwrap(), Datum::Bool(false));
        assert_eq!(run(&t(b(true), BoolTestKind::IsNotUnknown)).unwrap(), Datum::Bool(true));
    }

    #[test]
    fn distinct_from_treats_nulls_as_equal() {
        let d = |left: BoundExpr, right: BoundExpr, negated: bool| {
            BoundExpr::new(
                ExprKind::DistinctFrom(Box::new(DistinctFrom { left, right, eq: eq_op(), negated })),
                TypeId::BOOL,
            )
        };
        assert_eq!(run(&d(i4(1), null_i4(), false)).unwrap(), Datum::Bool(true));
        assert_eq!(run(&d(null_i4(), null_i4(), false)).unwrap(), Datum::Bool(false));
        assert_eq!(run(&d(null_i4(), null_i4(), true)).unwrap(), Datum::Bool(true));
        assert_eq!(run(&d(i4(1), i4(2), true)).unwrap(), Datum::Bool(false));
    }

    #[test]
    fn param_beyond_supplied_values_is_08p01() {
        let p2 = BoundExpr::param(2, TypeId::INT4);
        let err = run_with(&p2, &[], vec![Datum::Int4(1)]).unwrap_err();
        assert_eq!(err.sqlstate.code(), "08P01");
        assert_eq!(err.message, "bind message supplies 1 parameters, but prepared statement requires 2");
        let p1 = BoundExpr::param(1, TypeId::INT4);
        assert_eq!(run_with(&p1, &[], vec![Datum::Int4(9)]).unwrap(), Datum::Int4(9));
    }

    #[test]
    fn grouping_reads_mask_slot() {
        // GROUPING(k0, k1) with k1 not grouped (mask bit 1 set): 0*2 + 1 = 1.
        let g = BoundExpr::new(
            ExprKind::Grouping(Box::new(GroupingExpr { args: vec![0, 1], mask_slot: Some(0) })),
            TypeId::INT4,
        );
        assert_eq!(run_with(&g, &[Datum::Int8(0b10)], Vec::new()).unwrap(), Datum::Int4(1));
        let unresolved = BoundExpr::new(
            ExprKind::Grouping(Box::new(GroupingExpr { args: vec![0], mask_slot: None })),
            TypeId::INT4,
        );
        assert_eq!(run(&unresolved).unwrap_err().sqlstate.code(), "XX000");
    }

    #[test]
    fn bind_time_nodes_are_internal_errors() {
        let col = BoundExpr::column(0, 1, 1, TypeId::INT4);
        assert_eq!(run(&col).unwrap_err().sqlstate.code(), "XX000");
        let sp = BoundExpr::new(
            ExprKind::SubPlan(Box::new(SubPlanRef {
                plan_id: 0,
                kind: SubLinkKind::Scalar,
                test: None,
                ops: Vec::new(),
                args: Vec::new(),
                use_hash: false,
            })),
            TypeId::INT4,
        );
        assert_eq!(run(&sp).unwrap_err().sqlstate.code(), "XX000");
    }

    #[test]
    fn current_schema_and_user_are_text_or_null() {
        let cs = BoundExpr::new(ExprKind::SqlValue(SqlValueOp::CurrentSchema), TypeId::NAME);
        assert!(matches!(run(&cs), Ok(Datum::Text(_)) | Ok(Datum::Null)));
        let cu = BoundExpr::new(ExprKind::SqlValue(SqlValueOp::CurrentUser), TypeId::NAME);
        assert!(matches!(run(&cu), Ok(Datum::Text(_))));
        let cc = BoundExpr::new(ExprKind::SqlValue(SqlValueOp::CurrentCatalog), TypeId::NAME);
        assert!(matches!(run(&cc), Ok(Datum::Text(_))));
    }

    #[test]
    fn standalone_evaluates_constants_and_rejects_bind_nodes() {
        let (_kv, env) = pgarbor_catalog::testing::bootstrapped().unwrap();
        let guc = NoGuc;
        let services = NoServices;
        let rng = Cell::new(1u64);
        let se = SessionEnv::new(env, IoConfig::default(), &guc, &services, &rng, 0, 0);
        let mut txn = MemTxn::new();
        let mut cx = ExecContext::new(&mut txn, se);
        assert_eq!(eval_standalone(&mut cx, &i4(5), &[]).unwrap(), Datum::Int4(5));
        let col = BoundExpr::column(0, 1, 1, TypeId::INT4);
        assert!(eval_standalone(&mut cx, &col, &[]).is_err());
    }

    #[test]
    #[ignore = "needs EXE-06"]
    fn nullif_equal_values_is_null() {
        let e = BoundExpr::new(
            ExprKind::NullIf(Box::new(NullIfExpr { left: i4(1), right: i4(1), eq: eq_op() })),
            TypeId::INT4,
        );
        assert_eq!(run(&e).unwrap(), Datum::Null);
    }

    #[test]
    #[ignore = "needs EXE-06"]
    fn current_date_is_a_date() {
        let e = BoundExpr::new(ExprKind::SqlValue(SqlValueOp::CurrentDate), TypeId::DATE);
        assert!(matches!(run(&e), Ok(Datum::Date(_))));
    }
}
