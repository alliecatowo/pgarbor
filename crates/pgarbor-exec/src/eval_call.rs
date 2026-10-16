//! Expression evaluator, part 2: function calls, operators, `ANY/ALL (array)`, row comparison
//! (lease EXE-06, north-star).
//!
//! ## Function and operator calls
//! `FuncCall(f)`: resolve the builtin with [`resolve_handle`] (cached in `rt.funcs`); a proc OID
//! `>= 16384` is a user function -> `error::user_function_unsupported("EXE-06", name)` (0A000).
//! Evaluate `f.args` left to right (no short circuit, PG evaluates all arguments of an ordinary
//! function), then:
//! * **strict** procs (`handle.proc.strict` or `f.strict`): any NULL argument -> NULL without calling;
//! * `FuncImpl::Scalar` -> `handle.call(args, &FuncCtx)`;
//! * `FuncImpl::External`: FIRST ask [`crate::session_fns::handles`]`(proc)`; exec-owned procs
//!   (`nextval`, `setval`, `currval`, `lastval`, advisory locks, `set_config`, ...) run through
//!   `session_fns::call(ex, proc, &args, arg_types)` because they need `&mut` transaction access.
//!   Otherwise `sysfuncs::handles(proc)` -> `handle.call` (the `SessionContext` forwards to the catalog
//!   functions); otherwise 0A000 `function {name} is not supported`;
//! * `FuncImpl::Set` / retset=true in a scalar position: XX000 (the planner puts SRFs in `ProjectSet`
//!   / `FunctionScan`, see `eval_srf`);
//! * `FuncImpl::Aggregate` / `Window`: XX000 `aggregate function called as a plain function`.
//! `FuncCtx` = `{env: &cx.se.env, io: &cx.se.io, session: &ExecSessionContext, arg_types (the static
//! types of the argument expressions), result_type: e.ty, collation: 0}`; build it with
//! [`with_func_ctx`].
//!
//! `Op(o)`: identical, using `o.op.proc` as the function and `o.args` (1 or 2 arguments).
//! [`call_operator`] evaluates an `OpRef` over ALREADY evaluated datums (used by NULLIF, row
//! comparison, hash/merge join residuals, sort `USING`, ANY/ALL).
//!
//! ## ScalarArrayOp (`x op ANY|ALL (array)`) - PG `ExecEvalScalarArrayOp`
//! Evaluate `left`, then `right` (array); NULL array -> NULL. NULL left with a non-empty array -> NULL
//! (when `op` is strict). Walk elements in order calling `op` on `(left, elem)` (element type =
//! operator right type): `use_or` (ANY): first TRUE -> TRUE; remember NULL results; end -> NULL if any
//! NULL seen else FALSE. ALL: first FALSE -> FALSE; NULL results remembered; end -> NULL if any NULL
//! else TRUE. Empty array: ANY -> FALSE, ALL -> TRUE (even for NULL left). `IN (list)` arrives as
//! `ScalarArrayOp` over `Array` literal expressions; optimize `Const` arrays of equality tests on
//! hashable types with a set only if it keeps results identical (optional).
//!
//! ## RowCompare (`(a,b) < (c,d)`) - PG `ExecEvalRowCompare`
//! For i in 0..n: `r = eq_ops[i](left[i], right[i])` for i < n-1 style: compare columns left to right;
//! if the pair is NULL on either side the whole comparison is NULL unless an EARLIER pair was decisive;
//! the first column pair that is not equal decides using `ops[i]` of that pair (`<`, `<=`, `>`, `>=`
//! per `kind`); if all earlier pairs are equal and the last pair decides with `ops[n-1]`. All pairs
//! equal: `Le`/`Ge` -> TRUE, `Lt`/`Gt` -> FALSE. A NULL in a pair examined before a decisive result
//! yields NULL.
use pgarbor_catalog::sysfuncs;
use pgarbor_common::{PgError, Result, SqlState};
use pgarbor_sql::ir::{BoundExpr, FuncCallExpr, OpExpr, OpRef, RowCompare, RowCompareKind, ScalarArrayOp};
use pgarbor_types::context::FuncCtx;
use pgarbor_types::func::{self, FuncHandle, FuncImpl};
use pgarbor_types::{Datum, TypeId};

use crate::context::ExecContext;
use crate::error;
use crate::eval;
use crate::exec_session::ExecSessionContext;
use crate::runtime::Executor;
use crate::session_fns;

/// Lease that owns this file (used for `unimplemented` / user-function errors).
const LEASE: &str = "EXE-06";

/// First OID handed out to user-created objects; smaller OIDs are catalog builtins.
const FIRST_USER_OID: u32 = 16384;

/// Resolve (and cache in `ex.rt.funcs`) the builtin `pg_proc` implementation. User functions
/// (`oid >= 16384`) -> 0A000 via `error::user_function_unsupported`.
pub fn resolve_handle(ex: &mut Executor, proc_oid: u32, name: &str) -> Result<FuncHandle> {
    if proc_oid >= FIRST_USER_OID {
        return Err(error::user_function_unsupported(LEASE, name));
    }
    if let Some(h) = ex.rt.funcs.get(&proc_oid) {
        return Ok(*h);
    }
    let h = func::implementation(proc_oid)?;
    ex.rt.funcs.insert(proc_oid, h);
    Ok(h)
}

/// Run `f` with a `FuncCtx` for a call whose argument expression types are `arg_types` and whose
/// result type is `result_type` (module docs). Only shared borrows of `cx` are needed.
pub fn with_func_ctx<T>(
    cx: &ExecContext,
    arg_types: &[TypeId],
    result_type: TypeId,
    f: impl FnOnce(&FuncCtx) -> Result<T>,
) -> Result<T> {
    let session = ExecSessionContext::new(&cx.se, &*cx.txn);
    let ctx = FuncCtx {
        env: &cx.se.env,
        io: &cx.se.io,
        session: &session,
        arg_types,
        result_type,
        collation: 0,
    };
    f(&ctx)
}

/// XX000 for a set-returning function reached in a scalar position (the planner must route it to
/// `ProjectSet` / `FunctionScan`).
fn set_in_scalar_context() -> PgError {
    PgError::internal("set-valued function called in context that cannot accept a set")
}

/// Shared core of every scalar invocation: checks the implementation kind, applies strictness
/// (`strict` is the call-site flag; the proc's own `proisstrict` is honored too), then dispatches
/// exec-owned, catalog-owned or builtin implementations.
fn invoke(
    ex: &mut Executor,
    handle: &FuncHandle,
    strict: bool,
    args: &[Datum],
    arg_types: &[TypeId],
    result_type: TypeId,
) -> Result<Datum> {
    let proc = handle.proc;
    match handle.imp {
        FuncImpl::Scalar(_) | FuncImpl::External => {}
        FuncImpl::Set(_) => return Err(set_in_scalar_context()),
        FuncImpl::Aggregate => return Err(PgError::internal("aggregate function called as a plain function")),
        FuncImpl::Window => return Err(PgError::internal("window function called as a plain function")),
    }
    if proc.retset {
        return Err(set_in_scalar_context());
    }
    if (strict || proc.strict) && args.iter().any(|a| a.is_null()) {
        return Ok(Datum::Null);
    }
    if let FuncImpl::External = handle.imp {
        if session_fns::handles(proc) {
            return session_fns::call(ex, proc, args, arg_types);
        }
        if !sysfuncs::handles(proc) {
            return Err(PgError::new(
                SqlState::FEATURE_NOT_SUPPORTED,
                format!("function {} is not supported", proc.name),
            ));
        }
    }
    with_func_ctx(&*ex.cx, arg_types, result_type, |ctx| handle.call(args, ctx))
}

/// Evaluate expressions left to right, collecting their values and static types.
fn eval_args(ex: &mut Executor, exprs: &[BoundExpr], row: &[Datum]) -> Result<(Vec<Datum>, Vec<TypeId>)> {
    let mut vals = Vec::with_capacity(exprs.len());
    let mut types = Vec::with_capacity(exprs.len());
    for e in exprs {
        vals.push(eval::eval(ex, e, row)?);
        types.push(e.ty);
    }
    Ok((vals, types))
}

/// Call a resolved scalar function with evaluated arguments (applies strictness, routes External
/// procs per the module docs).
pub fn call_function(
    ex: &mut Executor,
    handle: &FuncHandle,
    args: &[Datum],
    arg_types: &[TypeId],
    result_type: TypeId,
) -> Result<Datum> {
    invoke(ex, handle, false, args, arg_types, result_type)
}

/// Evaluate a `FuncCall` expression node over `row`.
pub fn eval_func_call(ex: &mut Executor, f: &FuncCallExpr, result_type: TypeId, row: &[Datum]) -> Result<Datum> {
    if f.func >= FIRST_USER_OID {
        let (args, arg_types) = eval_args(ex, &f.args, row)?;
        return crate::plfunc::call_user_function(&mut *ex.cx, f.func, args, arg_types, result_type, false)
            .map(|o| o.into_scalar());
    }
    let handle = resolve_handle(ex, f.func, &f.name)?;
    let (mut args, mut arg_types) = eval_args(ex, &f.args, row)?;
    if f.variadic && handle.proc.variadic == pgarbor_types::oids::ANY {
        // `f(VARIADIC arr)` on a `VARIADIC "any"` proc: PG hands the array over with funcvariadic set
        // and the proc unpacks it. The builtins here take the elements one by one, so unpack first.
        match args.pop().zip(arg_types.pop()) {
            Some((Datum::Null, _)) => {
                // PG `extract_variadic_args` returns -1 for a NULL array: these procs return NULL.
                // `format` instead sees no variadic values.
                let null_yields_null = matches!(
                    handle.proc.name,
                    "concat" | "concat_ws" | "json_build_object" | "json_build_array" | "jsonb_build_object" | "jsonb_build_array" | "num_nulls" | "num_nonnulls"
                );
                if null_yields_null {
                    return Ok(Datum::Null);
                }
            }
            Some((Datum::Array(arr), _)) => {
                let elem = TypeId::of(arr.elem);
                for item in arr.items.iter() {
                    args.push(item.clone());
                    arg_types.push(elem);
                }
            }
            Some((other, ty)) => {
                args.push(other);
                arg_types.push(ty);
            }
            None => {}
        }
    }
    invoke(ex, &handle, f.strict, &args, &arg_types, result_type)
}

/// Evaluate an `Op` expression node over `row`.
pub fn eval_op(ex: &mut Executor, o: &OpExpr, result_type: TypeId, row: &[Datum]) -> Result<Datum> {
    if o.op.proc >= FIRST_USER_OID {
        let (args, arg_types) = eval_args(ex, &o.args, row)?;
        if o.op.strict && args.iter().any(|a| a.is_null()) {
            return Ok(Datum::Null);
        }
        return crate::plfunc::call_user_function(&mut *ex.cx, o.op.proc, args, arg_types, result_type, false)
            .map(|o| o.into_scalar());
    }
    let handle = resolve_handle(ex, o.op.proc, &o.op.name)?;
    let (args, arg_types) = eval_args(ex, &o.args, row)?;
    invoke(ex, &handle, o.op.strict, &args, &arg_types, result_type)
}

/// Apply operator `op` to evaluated arguments (`arg_types` parallel to `args`); strict operators
/// return NULL for any NULL argument.
pub fn call_operator(ex: &mut Executor, op: &OpRef, args: &[Datum], arg_types: &[TypeId], result_type: TypeId) -> Result<Datum> {
    if op.proc >= FIRST_USER_OID {
        if op.strict && args.iter().any(|a| a.is_null()) {
            return Ok(Datum::Null);
        }
        return crate::plfunc::call_user_function(&mut *ex.cx, op.proc, args.to_vec(), arg_types.to_vec(), result_type, false)
            .map(|o| o.into_scalar());
    }
    let handle = resolve_handle(ex, op.proc, &op.name)?;
    invoke(ex, &handle, op.strict, args, arg_types, result_type)
}

/// `left op ANY|ALL (right)` over already evaluated operands (module docs, ScalarArrayOp).
fn any_all_values(
    ex: &mut Executor,
    op: &OpRef,
    use_or: bool,
    left: &Datum,
    left_ty: TypeId,
    right: &Datum,
) -> Result<Datum> {
    let arr = match right {
        Datum::Null => return Ok(Datum::Null),
        Datum::Array(a) => a,
        _ => return Err(PgError::internal("ANY/ALL right operand is not an array")),
    };
    // An empty array decides the result before the operator (and the NULL left operand) matter.
    if arr.items.is_empty() {
        return Ok(Datum::Bool(!use_or));
    }
    if left.is_null() && op.strict {
        return Ok(Datum::Null);
    }
    let handle = resolve_handle(ex, op.proc, &op.name)?;
    let types = [left_ty, TypeId::of(arr.elem)];
    let mut saw_null = false;
    let mut ticks: u32 = 0;
    for item in arr.items.iter() {
        ex.tick(&mut ticks)?;
        let args = [left.clone(), item.clone()];
        match invoke(ex, &handle, op.strict, &args, &types, TypeId::BOOL)? {
            // ANY: first TRUE decides. ALL: first FALSE decides.
            Datum::Bool(b) => {
                if b == use_or {
                    return Ok(Datum::Bool(b));
                }
            }
            Datum::Null => saw_null = true,
            _ => return Err(PgError::internal("operator in ANY/ALL did not return a boolean")),
        }
    }
    Ok(if saw_null { Datum::Null } else { Datum::Bool(!use_or) })
}

/// Evaluate `x op ANY|ALL (array)` (module docs).
pub fn eval_scalar_array_op(ex: &mut Executor, s: &ScalarArrayOp, row: &[Datum]) -> Result<Datum> {
    let left = eval::eval(ex, &s.left, row)?;
    let right = eval::eval(ex, &s.right, row)?;
    any_all_values(ex, &s.op, s.use_or, &left, s.left.ty, &right)
}

/// Row comparison core (module docs, RowCompare). `operand(ex, i)` yields the values of column pair
/// `i` on the left and right; it is called lazily, left to right, and stops as soon as a pair decides.
fn row_compare_with(
    ex: &mut Executor,
    r: &RowCompare,
    mut operand: impl FnMut(&mut Executor, usize) -> Result<(Datum, Datum)>,
) -> Result<Datum> {
    let n = r.left.len();
    if r.right.len() != n || r.ops.len() != n {
        return Err(PgError::internal("row comparison operands have different lengths"));
    }
    if n == 0 {
        // Vacuously equal rows.
        return Ok(Datum::Bool(matches!(r.kind, RowCompareKind::Le | RowCompareKind::Ge)));
    }
    for i in 0..n {
        let (lv, rv) = operand(&mut *ex, i)?;
        let types = [r.left[i].ty, r.right[i].ty];
        let op = &r.ops[i];
        if i + 1 == n {
            // The last pair decides with its own comparison operator (NULL allowed).
            let h = resolve_handle(ex, op.proc, &op.name)?;
            return invoke(ex, &h, op.strict, &[lv, rv], &types, TypeId::BOOL);
        }
        let eq = r
            .eq_ops
            .get(i)
            .ok_or_else(|| PgError::internal("row comparison is missing an equality operator"))?;
        let eq_h = resolve_handle(ex, eq.proc, &eq.name)?;
        match invoke(ex, &eq_h, eq.strict, &[lv.clone(), rv.clone()], &types, TypeId::BOOL)? {
            // Equal so far: continue with the next column pair.
            Datum::Bool(true) => {}
            // First unequal pair decides with the column's ordering operator.
            Datum::Bool(false) => {
                let h = resolve_handle(ex, op.proc, &op.name)?;
                return invoke(ex, &h, op.strict, &[lv, rv], &types, TypeId::BOOL);
            }
            // Undecided at this column: the whole comparison is NULL.
            Datum::Null => return Ok(Datum::Null),
            _ => return Err(PgError::internal("row comparison equality did not return a boolean")),
        }
    }
    // Unreachable: the last pair always returns above. Kept total for safety.
    Ok(Datum::Bool(matches!(r.kind, RowCompareKind::Le | RowCompareKind::Ge)))
}

/// Evaluate a row-wise comparison (module docs).
pub fn eval_row_compare(ex: &mut Executor, r: &RowCompare, row: &[Datum]) -> Result<Datum> {
    row_compare_with(ex, r, |ex, i| {
        let l = eval::eval(ex, &r.left[i], row)?;
        let rv = eval::eval(ex, &r.right[i], row)?;
        Ok((l, rv))
    })
}

/// Convenience for operators over bound expression operands (hash/merge join keys etc.).
pub fn eval_operator_exprs(ex: &mut Executor, op: &OpRef, left: &BoundExpr, right: &BoundExpr, row: &[Datum]) -> Result<Datum> {
    let l = eval::eval(ex, left, row)?;
    let r = eval::eval(ex, right, row)?;
    let handle = resolve_handle(ex, op.proc, &op.name)?;
    // The declared result type of the implementing proc (exact for non-polymorphic operators).
    invoke(ex, &handle, op.strict, &[l, r], &[left.ty, right.ty], TypeId::of(handle.proc.ret))
}

#[cfg(test)]
mod tests {
    use std::cell::Cell;

    use super::*;
    use crate::context::{NoGuc, NoServices, SessionEnv};
    use crate::memtxn::MemTxn;
    use crate::runtime::Runtime;
    use pgarbor_sql::ir::ExprKind;
    use pgarbor_types::array::Array;
    use pgarbor_types::catalog_data::Volatility;
    use pgarbor_types::context::IoConfig;
    use pgarbor_types::oids;

    // Catalog OIDs (contracts/reference/pg18/pg_proc.csv, pg_operator.csv).
    const INT4PL_OPER: u32 = 551;
    const INT4PL_PROC: u32 = 177;
    const INT4EQ_OPER: u32 = 96;
    const INT4EQ_PROC: u32 = 65;
    const INT4NE_OPER: u32 = 518;
    const INT4NE_PROC: u32 = 144;
    const INT4LT_PROC: u32 = 66;
    const INT4GT_PROC: u32 = 147;
    const INT4LE_PROC: u32 = 149;
    const INT4GE_PROC: u32 = 150;

    fn op(oper: u32, proc: u32, name: &str, strict: bool) -> OpRef {
        OpRef { op: oper, proc, name: name.to_string(), volatility: Volatility::Immutable, strict }
    }

    fn dummy_expr() -> BoundExpr {
        BoundExpr { kind: ExprKind::Const(Datum::Null), ty: TypeId::INT4 }
    }

    fn int_array(vals: Vec<Datum>) -> Datum {
        Datum::Array(Box::new(Array::from_vec(oids::INT4, vals)))
    }

    /// Run `f` with an executor over an in-memory transaction and the bootstrapped catalog.
    fn with_exec<T>(f: impl FnOnce(&mut Executor) -> Result<T>) -> Result<T> {
        let (_kv, env) = pgarbor_catalog::testing::bootstrapped()?;
        let mut txn = MemTxn::new();
        let guc = NoGuc;
        let services = NoServices;
        let rng = Cell::new(1u64);
        let se = SessionEnv::new(env, IoConfig::default(), &guc, &services, &rng, 0, 0);
        let mut cx = ExecContext::new(&mut txn, se);
        let mut rt = Runtime::default();
        let mut ex = Executor::new(&mut rt, &mut cx);
        f(&mut ex)
    }

    fn row_cmp(kind: RowCompareKind, n: usize) -> RowCompare {
        let (lt_proc, name) = match kind {
            RowCompareKind::Lt => (INT4LT_PROC, "<"),
            RowCompareKind::Le => (INT4LE_PROC, "<="),
            RowCompareKind::Gt => (INT4GT_PROC, ">"),
            RowCompareKind::Ge => (INT4GE_PROC, ">="),
        };
        RowCompare {
            kind,
            left: (0..n).map(|_| dummy_expr()).collect(),
            right: (0..n).map(|_| dummy_expr()).collect(),
            ops: (0..n).map(|_| op(0, lt_proc, name, true)).collect(),
            eq_ops: (0..n).map(|_| op(0, INT4EQ_PROC, "=", true)).collect(),
        }
    }

    fn run_row_cmp(kind: RowCompareKind, l: Vec<Datum>, r: Vec<Datum>) -> Datum {
        let rc = row_cmp(kind, l.len());
        with_exec(|ex| row_compare_with(ex, &rc, |_, i| Ok((l[i].clone(), r[i].clone())))).expect("row compare")
    }

    #[test]
    fn int4pl_through_operator_call() {
        let plus = op(INT4PL_OPER, INT4PL_PROC, "+", true);
        let v = with_exec(|ex| {
            call_operator(ex, &plus, &[Datum::Int4(1), Datum::Int4(2)], &[TypeId::INT4, TypeId::INT4], TypeId::INT4)
        })
        .expect("int4pl");
        assert_eq!(v, Datum::Int4(3));
    }

    #[test]
    fn strict_operator_returns_null_for_null_argument() {
        let plus = op(INT4PL_OPER, INT4PL_PROC, "+", true);
        let v = with_exec(|ex| {
            call_operator(ex, &plus, &[Datum::Null, Datum::Int4(2)], &[TypeId::INT4, TypeId::INT4], TypeId::INT4)
        })
        .expect("null + 2");
        assert_eq!(v, Datum::Null);
    }

    #[test]
    fn user_function_oid_is_unsupported() {
        let err = with_exec(|ex| resolve_handle(ex, 16384, "myfunc")).err().expect("user function must fail");
        assert!(err.sqlstate == SqlState::FEATURE_NOT_SUPPORTED);
    }

    #[test]
    fn any_is_three_valued() {
        let eq = op(INT4EQ_OPER, INT4EQ_PROC, "=", true);
        let arr = int_array(vec![Datum::Int4(1), Datum::Null, Datum::Int4(2)]);
        let ty = TypeId::INT4;
        let two = with_exec(|ex| any_all_values(ex, &eq, true, &Datum::Int4(2), ty, &arr)).expect("2 = any");
        assert_eq!(two, Datum::Bool(true));
        let three = with_exec(|ex| any_all_values(ex, &eq, true, &Datum::Int4(3), ty, &arr)).expect("3 = any");
        assert_eq!(three, Datum::Null);
        let empty = Datum::Array(Box::new(Array::empty(oids::INT4)));
        let none = with_exec(|ex| any_all_values(ex, &eq, true, &Datum::Int4(3), ty, &empty)).expect("3 = any('{}')");
        assert_eq!(none, Datum::Bool(false));
    }

    #[test]
    fn all_with_not_equal() {
        let ne = op(INT4NE_OPER, INT4NE_PROC, "<>", true);
        let arr = int_array(vec![Datum::Int4(1), Datum::Int4(2)]);
        let v = with_exec(|ex| any_all_values(ex, &ne, false, &Datum::Int4(3), TypeId::INT4, &arr)).expect("3 <> all");
        assert_eq!(v, Datum::Bool(true));
    }

    #[test]
    fn row_compare_lexicographic_with_nulls() {
        assert_eq!(run_row_cmp(RowCompareKind::Lt, vec![Datum::Int4(1), Datum::Int4(2)], vec![Datum::Int4(1), Datum::Int4(3)]), Datum::Bool(true));
        assert_eq!(run_row_cmp(RowCompareKind::Lt, vec![Datum::Int4(1), Datum::Null], vec![Datum::Int4(2), Datum::Int4(3)]), Datum::Bool(true));
        assert_eq!(run_row_cmp(RowCompareKind::Lt, vec![Datum::Int4(1), Datum::Null], vec![Datum::Int4(1), Datum::Int4(3)]), Datum::Null);
        assert_eq!(run_row_cmp(RowCompareKind::Le, vec![Datum::Int4(1), Datum::Int4(2)], vec![Datum::Int4(1), Datum::Int4(2)]), Datum::Bool(true));
        assert_eq!(run_row_cmp(RowCompareKind::Gt, vec![Datum::Int4(1), Datum::Int4(2)], vec![Datum::Int4(1), Datum::Int4(2)]), Datum::Bool(false));
    }

    #[test]
    fn row_compare_decisive_first_column_uses_kind_operator() {
        // (5,0) > (1,0): the first column decides with the `>` operator of its type.
        let rc = row_cmp(RowCompareKind::Gt, 2);
        let v = with_exec(|ex| {
            row_compare_with(ex, &rc, |_, i| {
                Ok(if i == 0 { (Datum::Int4(5), Datum::Int4(1)) } else { (Datum::Int4(0), Datum::Int4(0)) })
            })
        })
        .expect("(5,0) > (1,0)");
        assert_eq!(v, Datum::Bool(true));
    }

    #[test]
    #[ignore = "needs EXE-05"]
    fn op_expr_evaluates_constants() {
        let plus = op(INT4PL_OPER, INT4PL_PROC, "+", true);
        let c = |d: Datum| BoundExpr { kind: ExprKind::Const(d), ty: TypeId::INT4 };
        let o = OpExpr { op: plus, args: vec![c(Datum::Int4(1)), c(Datum::Int4(2))] };
        let v = with_exec(|ex| eval_op(ex, &o, TypeId::INT4, &[])).expect("1 + 2");
        assert_eq!(v, Datum::Int4(3));
        let n = OpExpr { op: op(INT4PL_OPER, INT4PL_PROC, "+", true), args: vec![c(Datum::Null), c(Datum::Int4(2))] };
        assert_eq!(with_exec(|ex| eval_op(ex, &n, TypeId::INT4, &[])).expect("NULL + 2"), Datum::Null);
    }

    #[test]
    #[ignore = "needs EXE-05"]
    fn row_compare_expression_with_nulls() {
        let c = |d: Datum| BoundExpr { kind: ExprKind::Const(d), ty: TypeId::INT4 };
        let mut rc = row_cmp(RowCompareKind::Lt, 2);
        rc.left = vec![c(Datum::Int4(1)), c(Datum::Null)];
        rc.right = vec![c(Datum::Int4(2)), c(Datum::Int4(3))];
        assert_eq!(with_exec(|ex| eval_row_compare(ex, &rc, &[])).expect("(1,NULL) < (2,3)"), Datum::Bool(true));
    }
}
