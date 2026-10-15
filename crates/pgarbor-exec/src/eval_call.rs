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
use pgarbor_common::{PgError, Result};
use pgarbor_sql::ir::{BoundExpr, FuncCallExpr, OpExpr, OpRef, RowCompare, ScalarArrayOp};
use pgarbor_types::context::FuncCtx;
use pgarbor_types::func::FuncHandle;
use pgarbor_types::{Datum, TypeId};

use crate::context::ExecContext;
use crate::runtime::Executor;

/// Resolve (and cache in `ex.rt.funcs`) the builtin `pg_proc` implementation. User functions
/// (`oid >= 16384`) -> 0A000 via `error::user_function_unsupported`.
#[allow(unused_variables)]
pub fn resolve_handle(ex: &mut Executor, proc_oid: u32, name: &str) -> Result<FuncHandle> {
    Err(PgError::unimplemented("EXE-06", "function resolution (eval_call::resolve_handle)"))
}

/// Run `f` with a `FuncCtx` for a call whose argument expression types are `arg_types` and whose
/// result type is `result_type` (module docs). Only shared borrows of `cx` are needed.
#[allow(unused_variables)]
pub fn with_func_ctx<T>(
    cx: &ExecContext,
    arg_types: &[TypeId],
    result_type: TypeId,
    f: impl FnOnce(&FuncCtx) -> Result<T>,
) -> Result<T> {
    Err(PgError::unimplemented("EXE-06", "FuncCtx construction (eval_call::with_func_ctx)"))
}

/// Call a resolved scalar function with evaluated arguments (applies strictness, routes External
/// procs per the module docs).
#[allow(unused_variables)]
pub fn call_function(
    ex: &mut Executor,
    handle: &FuncHandle,
    args: &[Datum],
    arg_types: &[TypeId],
    result_type: TypeId,
) -> Result<Datum> {
    Err(PgError::unimplemented("EXE-06", "scalar function call (eval_call::call_function)"))
}

/// Evaluate a `FuncCall` expression node over `row`.
#[allow(unused_variables)]
pub fn eval_func_call(ex: &mut Executor, f: &FuncCallExpr, result_type: TypeId, row: &[Datum]) -> Result<Datum> {
    Err(PgError::unimplemented("EXE-06", "function call evaluation (eval_call::eval_func_call)"))
}

/// Evaluate an `Op` expression node over `row`.
#[allow(unused_variables)]
pub fn eval_op(ex: &mut Executor, o: &OpExpr, result_type: TypeId, row: &[Datum]) -> Result<Datum> {
    Err(PgError::unimplemented("EXE-06", "operator evaluation (eval_call::eval_op)"))
}

/// Apply operator `op` to evaluated arguments (`arg_types` parallel to `args`); strict operators
/// return NULL for any NULL argument.
#[allow(unused_variables)]
pub fn call_operator(ex: &mut Executor, op: &OpRef, args: &[Datum], arg_types: &[TypeId], result_type: TypeId) -> Result<Datum> {
    Err(PgError::unimplemented("EXE-06", "operator application (eval_call::call_operator)"))
}

/// Evaluate `x op ANY|ALL (array)` (module docs).
#[allow(unused_variables)]
pub fn eval_scalar_array_op(ex: &mut Executor, s: &ScalarArrayOp, row: &[Datum]) -> Result<Datum> {
    Err(PgError::unimplemented("EXE-06", "ANY/ALL array evaluation (eval_call::eval_scalar_array_op)"))
}

/// Evaluate a row-wise comparison (module docs).
#[allow(unused_variables)]
pub fn eval_row_compare(ex: &mut Executor, r: &RowCompare, row: &[Datum]) -> Result<Datum> {
    Err(PgError::unimplemented("EXE-06", "row comparison (eval_call::eval_row_compare)"))
}

/// Convenience for operators over bound expression operands (hash/merge join keys etc.).
#[allow(unused_variables)]
pub fn eval_operator_exprs(ex: &mut Executor, op: &OpRef, left: &BoundExpr, right: &BoundExpr, row: &[Datum]) -> Result<Datum> {
    Err(PgError::unimplemented("EXE-06", "operator over expressions (eval_call::eval_operator_exprs)"))
}
