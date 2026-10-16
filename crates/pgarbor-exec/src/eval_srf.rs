//! Set-returning function evaluation (lease EXE-08). Used by `node_func` (`FunctionScan`,
//! `ProjectSet`). PG references: `executor/nodeFunctionscan.c`, `nodeProjectSet.c`,
//! `execSRF.c` (`ExecMakeFunctionResultSet`).
//!
//! ## Opening a set-returning call
//! [`open_srf`] evaluates the `FuncCall`'s arguments over `row`, resolves the handle
//! (`eval_call::resolve_handle`) and returns a LAZY iterator ([`SrfIter`]):
//! * `FuncImpl::Set(f)` (`generate_series`, `unnest`, `jsonb_each`, `regexp_matches`, `string_to_table`,
//!   ...): `handle.call_set(args, &FuncCtx)`. Strict procs with a NULL argument yield an empty
//!   iterator without calling (`FuncHandle::call_set` does it).
//! * `FuncImpl::External` with `retset` (`pg_listening_channels`, `pg_get_keywords`,
//!   `pg_options_to_table`, `pg_show_all_settings`, `pg_stat_get_activity`, `pg_lock_status`, ...):
//!   exec-owned ones through `session_fns::call_set`; catalog ones through
//!   `pgarbor_catalog::sysfuncs::call_set` (returns a `Vec<Datum>`; wrap in an iterator).
//! * A scalar function in a FROM item (`FROM now()`, `ProjectSet` of a non-SRF expression): evaluate
//!   once, yield exactly one row.
//! The `FuncCtx` borrows only `ex.cx`; the returned iterator must not borrow anything ('static).
//!
//! ## Row shape
//! A SRF whose result type is a scalar yields one value per row. A SRF returning a composite
//! (`FunctionItem::columns.len() > 1`, `Datum::Record`) is EXPANDED by [`expand_record`] into one
//! value per column; a NULL record expands to NULLs. `columns.len() == 1` with a record-typed column
//! is NOT expanded (`SELECT * FROM unnest(array[row(1,2)])` keeps one column).
use pgarbor_catalog::sysfuncs::{self, CatalogCallCtx};
use pgarbor_common::{PgError, Result, SqlState};
use pgarbor_sql::ir::{BoundExpr, ExprKind, FuncCallExpr};
use pgarbor_types::func::FuncImpl;
use pgarbor_types::{Datum, TypeId};

use crate::context::ExecContext;
use crate::eval;
use crate::eval_call;
use crate::exec_session::ExecSessionInfo;
use crate::runtime::Executor;
use crate::session_fns;

/// Lazy rows of one set-returning call.
pub type SrfIter = Box<dyn Iterator<Item = Result<Datum>>>;

/// Evaluate arguments and start a SRF (module docs). `result_type` is the call's static type.
pub fn open_srf(ex: &mut Executor, call: &FuncCallExpr, result_type: TypeId, row: &[Datum]) -> Result<SrfIter> {
    let mut args: Vec<Datum> = Vec::with_capacity(call.args.len());
    let mut arg_types: Vec<TypeId> = Vec::with_capacity(call.args.len());
    for a in &call.args {
        args.push(eval::eval(ex, a, row)?);
        arg_types.push(a.ty);
    }

    if call.func >= crate::plfunc::FIRST_USER_OID {
        let out = crate::plfunc::call_user_function(&mut *ex.cx, call.func, args, arg_types, result_type, call.retset)?;
        return Ok(values_iter(out.into_set()));
    }
    let handle = eval_call::resolve_handle(ex, call.func, &call.name)?;
    let proc = handle.proc;

    // A non-set function in a FROM item (or ProjectSet position): exactly one row.
    if !proc.retset {
        let v = eval_call::call_function(ex, &handle, &args, &arg_types, result_type)?;
        return Ok(Box::new(std::iter::once(Ok::<Datum, PgError>(v))));
    }

    match handle.imp {
        FuncImpl::Set(_) => {
            // The iterator is 'static: the FuncCtx only lives for the duration of `call_set`.
            let it = eval_call::with_func_ctx(&*ex.cx, &arg_types, result_type, |fctx| handle.call_set(&args, fctx))?;
            Ok(it)
        }
        FuncImpl::External => {
            // Strict procs with a NULL argument return an empty set (FuncHandle::call_set does this
            // for the builtin modules; the exec-owned and catalog dispatch below do not).
            if proc.strict && args.iter().any(|a| a.is_null()) {
                return Ok(Box::new(std::iter::empty::<Result<Datum>>()));
            }
            if session_fns::handles(proc) {
                let vals = session_fns::call_set(ex, proc, &args)?;
                return Ok(values_iter(vals));
            }
            if sysfuncs::handles(proc) {
                let cx: &ExecContext = &*ex.cx;
                let sess = ExecSessionInfo::new(&cx.se, &*cx.txn);
                let cctx = CatalogCallCtx { env: &cx.se.env, session: &sess, io_config: &cx.se.io };
                let vals = sysfuncs::call_set(&cctx, proc, &args)?;
                return Ok(values_iter(vals));
            }
            Err(PgError::new(SqlState::FEATURE_NOT_SUPPORTED, format!("function {} is not supported", proc.name)))
        }
        FuncImpl::Aggregate => Err(PgError::internal(format!(
            "aggregate function {} called in a set-returning position",
            proc.name
        ))),
        FuncImpl::Window => Err(PgError::internal(format!(
            "window function {} called in a set-returning position",
            proc.name
        ))),
        FuncImpl::Scalar(_) => Err(PgError::internal(format!(
            "scalar function {} marked as set-returning",
            proc.name
        ))),
    }
}

/// Lazy iterator over already materialized SRF output values.
fn values_iter(vals: Vec<Datum>) -> SrfIter {
    Box::new(vals.into_iter().map(Ok::<Datum, PgError>))
}

/// Expand a SRF output `value` into `ncols` column values (module docs, "Row shape").
pub fn expand_record(value: Datum, ncols: usize) -> Result<Vec<Datum>> {
    if ncols == 1 {
        // A single column is never expanded, even when it holds a record.
        return Ok(vec![value]);
    }
    match value {
        Datum::Null => Ok(vec![Datum::Null; ncols]),
        Datum::Record(fields) => {
            if fields.len() == ncols {
                Ok(fields)
            } else {
                Err(PgError::internal(format!(
                    "set-returning function produced a record of {} fields, expected {}",
                    fields.len(),
                    ncols
                )))
            }
        }
        other => Err(PgError::internal(format!(
            "set-returning function produced a non-record value {:?} for {} columns",
            other, ncols
        ))),
    }
}

/// True if `e` is, or contains outside any sub-plan, a `FuncCall` with `retset` (the
/// `ProjectSet` lock-step rule in plan.rs). Pure tree walk (`BoundExpr::walk`).
pub fn contains_srf(e: &BoundExpr) -> Result<bool> {
    let mut found = false;
    e.walk(&mut |n: &BoundExpr| {
        if found {
            return false;
        }
        match &n.kind {
            // Sub-plans are evaluated by their own operator tree: do not descend.
            ExprKind::SubPlan(_) => false,
            ExprKind::FuncCall(f) => {
                if f.retset {
                    found = true;
                }
                !found
            }
            _ => true,
        }
    });
    Ok(found)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::context::{NoGuc, NoServices, SessionEnv};
    use crate::memtxn::MemTxn;
    use crate::runtime::Runtime;
    use pgarbor_catalog::testing;
    use pgarbor_types::catalog_data::Volatility;
    use pgarbor_types::context::IoConfig;
    use pgarbor_types::oids;
    use pgarbor_types::Array;
    use std::cell::Cell;

    /// `generate_series(int4, int4)` (pg_proc 1067).
    const GENERATE_SERIES_INT4: u32 = 1067;
    /// `unnest(anyarray)` (pg_proc 2331).
    const UNNEST_ANYARRAY: u32 = 2331;

    fn func_call(func: u32, name: &str, args: Vec<BoundExpr>, retset: bool) -> FuncCallExpr {
        FuncCallExpr {
            func,
            name: name.to_string(),
            args,
            variadic: false,
            retset,
            volatility: Volatility::Immutable,
            strict: true,
            is_cast_call: false,
        }
    }

    fn int4(v: i32) -> BoundExpr {
        BoundExpr::constant(Datum::Int4(v), TypeId::INT4)
    }

    /// Run `f` with an executor over an empty in-memory transaction and a bootstrapped catalog.
    fn with_executor<T>(f: impl FnOnce(&mut Executor) -> T) -> T {
        let (_kv, env) = testing::bootstrapped().unwrap();
        let rng = Cell::new(1u64);
        let mut txn = MemTxn::new();
        let se = SessionEnv::new(env, IoConfig::default(), &NoGuc, &NoServices, &rng, 0, 0);
        let mut cx = ExecContext::new(&mut txn, se);
        let mut rt = Runtime::default();
        let mut ex = Executor::new(&mut rt, &mut cx);
        f(&mut ex)
    }

    #[test]
    #[ignore = "needs EXE-06"]
    fn generate_series_yields_rows() {
        with_executor(|ex| {
            let call = func_call(GENERATE_SERIES_INT4, "generate_series", vec![int4(1), int4(3)], true);
            let it = open_srf(ex, &call, TypeId::INT4, &[]).unwrap();
            let rows: Vec<Datum> = it.collect::<Result<Vec<Datum>>>().unwrap();
            assert_eq!(rows, vec![Datum::Int4(1), Datum::Int4(2), Datum::Int4(3)]);
        });
    }

    #[test]
    #[ignore = "needs EXE-06"]
    fn generate_series_is_lazy() {
        with_executor(|ex| {
            let call = func_call(GENERATE_SERIES_INT4, "generate_series", vec![int4(1), int4(1_000_000_000)], true);
            let it = open_srf(ex, &call, TypeId::INT4, &[]).unwrap();
            let rows: Vec<Datum> = it.take(2).collect::<Result<Vec<Datum>>>().unwrap();
            assert_eq!(rows, vec![Datum::Int4(1), Datum::Int4(2)]);
        });
    }

    #[test]
    #[ignore = "needs EXE-06"]
    fn generate_series_null_argument_yields_no_rows() {
        with_executor(|ex| {
            let null_arg = BoundExpr::null(TypeId::INT4);
            let call = func_call(GENERATE_SERIES_INT4, "generate_series", vec![int4(1), null_arg], true);
            let it = open_srf(ex, &call, TypeId::INT4, &[]).unwrap();
            let rows: Vec<Datum> = it.collect::<Result<Vec<Datum>>>().unwrap();
            assert!(rows.is_empty());
        });
    }

    #[test]
    #[ignore = "needs EXE-06"]
    fn unnest_array_yields_elements() {
        with_executor(|ex| {
            let arr = Array::from_vec(oids::INT4, vec![Datum::Int4(1), Datum::Int4(2)]);
            let arg = BoundExpr::constant(Datum::Array(Box::new(arr)), TypeId::INT4_ARRAY);
            let call = func_call(UNNEST_ANYARRAY, "unnest", vec![arg], true);
            let it = open_srf(ex, &call, TypeId::INT4, &[]).unwrap();
            let rows: Vec<Datum> = it.collect::<Result<Vec<Datum>>>().unwrap();
            assert_eq!(rows, vec![Datum::Int4(1), Datum::Int4(2)]);
        });
    }

    #[test]
    fn expand_record_yields_fields() {
        let out = expand_record(Datum::Record(vec![Datum::Int4(1), Datum::Text("a".to_string())]), 2).unwrap();
        assert_eq!(out, vec![Datum::Int4(1), Datum::Text("a".to_string())]);
    }

    #[test]
    fn expand_record_null_gives_nulls() {
        let out = expand_record(Datum::Null, 2).unwrap();
        assert_eq!(out, vec![Datum::Null, Datum::Null]);
    }

    #[test]
    fn expand_record_single_column_not_expanded() {
        let rec = Datum::Record(vec![Datum::Int4(1), Datum::Int4(2)]);
        let out = expand_record(rec.clone(), 1).unwrap();
        assert_eq!(out, vec![rec]);
    }

    #[test]
    fn expand_record_width_mismatch_is_error() {
        assert!(expand_record(Datum::Record(vec![Datum::Int4(1)]), 2).is_err());
    }

    #[test]
    fn contains_srf_detects_top_level_and_nested() {
        let srf = BoundExpr::new(
            ExprKind::FuncCall(Box::new(func_call(GENERATE_SERIES_INT4, "generate_series", vec![int4(1), int4(2)], true))),
            TypeId::INT4,
        );
        assert!(contains_srf(&srf).unwrap());

        let scalar_call = BoundExpr::new(
            ExprKind::FuncCall(Box::new(func_call(GENERATE_SERIES_INT4, "f", vec![int4(1)], false))),
            TypeId::INT4,
        );
        assert!(!contains_srf(&scalar_call).unwrap());
        assert!(!contains_srf(&int4(5)).unwrap());
    }

    #[test]
    fn contains_srf_nested_in_argument() {
        let srf = BoundExpr::new(
            ExprKind::FuncCall(Box::new(func_call(GENERATE_SERIES_INT4, "generate_series", vec![int4(1), int4(2)], true))),
            TypeId::INT4,
        );
        let outer = BoundExpr::new(ExprKind::FuncCall(Box::new(func_call(1, "abs", vec![srf], false))), TypeId::INT4);
        assert!(contains_srf(&outer).unwrap());
    }
}
