//! Sub-plan evaluation (lease EXE-09, sonnet). PG references: `executor/nodeSubplan.c`
//! (`ExecSubPlan`, `ExecScanSubPlan`, `ExecHashSubPlan`), plan.rs `SubPlanDef` docs.
//!
//! `eval_subplan(ex, sp, row)` evaluates `ExprKind::SubPlan(sp)` for the evaluating node's input
//! `row`. The runtime form `ex.rt.subplans[sp.plan_id]` (a `SubPlanRt` built by `build::open_plan`)
//! holds the operator tree (take it out while driving it, see the runtime module docs).
//!
//! ## Parameters and caching
//! * `sp.args` are evaluated over `row` (correlation) and pushed as ONE `OuterRef` frame with
//!   `Executor::with_outer` around the whole probe; then `op.rescan(ex, &args)` and `next()` calls.
//! * Uncorrelated sub-plans (`SubPlanRt::is_initplan`): run once, store the result in
//!   `SubPlanRt::cache` (`SubPlanCache`) and reuse it for later probes of the same execution.
//!   Volatile uncorrelated sub-plans (`volatility == Volatile`) are re-run for every probe.
//!
//! ## Result per kind (PG semantics)
//! * `Exists`: TRUE iff the sub-plan yields at least one row (stop after the first row); never NULL.
//! * `Scalar`: the first column of the single row; no rows -> NULL; a second row ->
//!   `error::scalar_subquery_multiple_rows()` (21000).
//! * `Array`: `Datum::Array` of the first column of all rows (empty -> empty array `{}`), element type
//!   from `SubPlanRt::out[0].ty`; NULL elements kept.
//! * `Row`: the single row as `Datum::Record` (no rows -> NULL; more than one row -> 21000).
//! * `Any` (`test op ANY (subquery)`, also `IN`): evaluate the left operand `sp.test` (a `Row` for
//!   multi-column tests) over `row`; for every sub-plan row compute the AND over columns of
//!   `ops[i](test_i, col_i)` (three-valued; column operators from `sp.ops`); result: TRUE on the
//!   first TRUE row; else NULL if any row result or the test value produced NULL; else FALSE. No
//!   rows -> FALSE regardless of the test value. With `sp.use_hash` (uncorrelated single-column
//!   equality IN): build once a `HashSet` of `keycodec::encode_key` bytes of the non-NULL sub-plan
//!   values (`SubPlanCache::Hashed`, remember "set contained NULL"); probe: NULL test value -> NULL if
//!   the set is non-empty, FALSE if empty; found -> TRUE; not found -> NULL if the set contained a
//!   NULL else FALSE. (The test value and set values are coerced to the same type by the binder, so
//!   equal values have equal keys.)
//! * `All` (`test op ALL (subquery)`): TRUE for no rows; FALSE on the first FALSE row; NULL if no FALSE
//!   but a NULL occurred; else TRUE.
//! A sub-plan that errors aborts the statement; the frame stack and the taken-out operator are
//! restored before returning the error.
use std::collections::HashSet;
use std::rc::Rc;

use pgarbor_common::{PgError, Result};
use pgarbor_sql::ir::{OpRef, SubLinkKind, SubPlanRef};
use pgarbor_types::catalog_data::Volatility;
use pgarbor_types::keycodec::{encode_key_vec, KeyColumn};
use pgarbor_types::logic::and3;
use pgarbor_types::{Array, Datum, TypeId};

use crate::error;
use crate::eval;
use crate::eval_call;
use crate::runtime::{Executor, Row, SubPlanCache};

/// Evaluate a sub-plan reference (module docs).
///
/// Dispatch follows PG `nodeSubplan.c` (`ExecSubPlan` -> `ExecScanSubPlan` / `ExecHashSubPlan`):
/// the correlation arguments are evaluated over `row`, then the kind-specific driver runs the
/// sub-plan inside one `OuterRef` frame.
pub fn eval_subplan(ex: &mut Executor, sp: &SubPlanRef, row: &[Datum]) -> Result<Datum> {
    let idx = sp.plan_id as usize;
    let (volatile, is_initplan) = match ex.rt.subplans.get(idx) {
        Some(s) => (matches!(s.volatility, Volatility::Volatile), s.is_initplan),
        None => return Err(PgError::internal(format!("sub-plan {} is not in the runtime", sp.plan_id))),
    };
    // Uncorrelated, non-volatile sub-plans (PG InitPlan) are executed once per execution.
    let cacheable = is_initplan && !volatile;

    let mut args: Vec<Datum> = Vec::with_capacity(sp.args.len());
    for a in &sp.args {
        args.push(eval::eval(ex, a, row)?);
    }

    match sp.kind {
        SubLinkKind::Exists => eval_exists(ex, idx, args, cacheable),
        SubLinkKind::Scalar => eval_scalar(ex, idx, args, cacheable),
        SubLinkKind::Row => eval_row(ex, idx, args, cacheable),
        SubLinkKind::Array => eval_array(ex, idx, args, cacheable),
        SubLinkKind::Any => eval_quantified(ex, sp, idx, row, args, cacheable, false),
        SubLinkKind::All => eval_quantified(ex, sp, idx, row, args, cacheable, true),
    }
}

/// Cached result of sub-plan `idx`, if any.
fn get_cache<'x>(ex: &'x Executor, idx: usize) -> Option<&'x SubPlanCache> {
    ex.rt.subplans.get(idx).and_then(|s| s.cache.as_ref())
}

fn set_cache(ex: &mut Executor, idx: usize, c: SubPlanCache) {
    if let Some(s) = ex.rt.subplans.get_mut(idx) {
        s.cache = Some(c);
    }
}

/// Output column types of sub-plan `idx` (empty if unknown).
fn out_types(ex: &Executor, idx: usize) -> Vec<TypeId> {
    ex.rt.subplans.get(idx).map(|s| s.out.iter().map(|c| c.ty).collect::<Vec<TypeId>>()).unwrap_or_default()
}

/// First column of a sub-plan row.
fn first_column(r: &[Datum]) -> Result<Datum> {
    r.first().cloned().ok_or_else(|| PgError::internal("sub-plan row has no columns"))
}

/// Encode one value as a hash key of type `ty` (keycodec: equal SQL values give equal bytes).
fn encode_one(ex: &Executor, ty: TypeId, v: &Datum) -> Result<Vec<u8>> {
    encode_key_vec(&ex.se().env, &[KeyColumn::asc(ty)], std::slice::from_ref(v))
}

/// Run sub-plan `idx` for one probe (`nodeSubplan.c` `ExecScanSubPlan` loop shape). The operator is
/// taken out of the runtime while it runs, `args` become one `OuterRef` frame, and `visit` returns
/// `Ok(false)` to stop pulling rows. The frame and the operator are restored even on error.
fn drive_subplan<F>(ex: &mut Executor, idx: usize, args: Vec<Datum>, mut visit: F) -> Result<()>
where
    F: FnMut(&mut Executor, Row) -> Result<bool>,
{
    let mut op = match ex.rt.subplans.get_mut(idx).and_then(|s| s.op.take()) {
        Some(op) => op,
        None => return Err(PgError::internal("sub-plan operator is unavailable (re-entrant probe?)")),
    };
    let frame = Rc::new(args);
    let r = ex.with_outer(Rc::clone(&frame), |ex| {
        op.rescan(ex, frame.as_slice())?;
        while let Some(row) = op.next(ex)? {
            if !visit(ex, row)? {
                break;
            }
        }
        Ok(())
    });
    if let Some(slot) = ex.rt.subplans.get_mut(idx) {
        slot.op = Some(op);
    }
    r
}

/// `EXISTS`: TRUE iff at least one row; stops after the first row (`nodeSubplan.c` EXISTS_SUBLINK).
fn eval_exists(ex: &mut Executor, idx: usize, args: Vec<Datum>, cacheable: bool) -> Result<Datum> {
    if cacheable {
        if let Some(SubPlanCache::Exists(b)) = get_cache(ex, idx) {
            return Ok(Datum::Bool(*b));
        }
    }
    let mut found = false;
    drive_subplan(ex, idx, args, |_ex, _row| {
        found = true;
        Ok(false)
    })?;
    if cacheable {
        set_cache(ex, idx, SubPlanCache::Exists(found));
    }
    Ok(Datum::Bool(found))
}

/// Scalar subquery: first column of the single row; no rows -> NULL; a second row -> 21000.
fn eval_scalar(ex: &mut Executor, idx: usize, args: Vec<Datum>, cacheable: bool) -> Result<Datum> {
    if cacheable {
        if let Some(SubPlanCache::Value(v)) = get_cache(ex, idx) {
            return Ok(v.clone());
        }
    }
    let mut value: Option<Datum> = None;
    drive_subplan(ex, idx, args, |_ex, r| {
        if value.is_some() {
            return Err(error::scalar_subquery_multiple_rows());
        }
        value = Some(first_column(&r)?);
        Ok(true)
    })?;
    let v = value.unwrap_or(Datum::Null);
    if cacheable {
        set_cache(ex, idx, SubPlanCache::Value(v.clone()));
    }
    Ok(v)
}

/// `ROW(subquery)`: the single row as a record; no rows -> NULL; a second row -> 21000.
fn eval_row(ex: &mut Executor, idx: usize, args: Vec<Datum>, cacheable: bool) -> Result<Datum> {
    if cacheable {
        if let Some(SubPlanCache::Value(v)) = get_cache(ex, idx) {
            return Ok(v.clone());
        }
    }
    let mut value: Option<Datum> = None;
    drive_subplan(ex, idx, args, |_ex, r| {
        if value.is_some() {
            return Err(error::scalar_subquery_multiple_rows());
        }
        value = Some(Datum::Record(r));
        Ok(true)
    })?;
    let v = value.unwrap_or(Datum::Null);
    if cacheable {
        set_cache(ex, idx, SubPlanCache::Value(v.clone()));
    }
    Ok(v)
}

/// `ARRAY(subquery)`: 1-D array of the first column of all rows (empty -> `{}`), NULLs kept.
fn eval_array(ex: &mut Executor, idx: usize, args: Vec<Datum>, cacheable: bool) -> Result<Datum> {
    if cacheable {
        if let Some(SubPlanCache::Value(v)) = get_cache(ex, idx) {
            return Ok(v.clone());
        }
    }
    let elem = out_types(ex, idx)
        .first()
        .map(|t| t.oid)
        .ok_or_else(|| PgError::internal("ARRAY sub-plan has no output column"))?;
    let mut items: Vec<Datum> = Vec::new();
    drive_subplan(ex, idx, args, |_ex, r| {
        items.push(first_column(&r)?);
        Ok(true)
    })?;
    let v = Datum::array(Array::from_vec(elem, items));
    if cacheable {
        set_cache(ex, idx, SubPlanCache::Value(v.clone()));
    }
    Ok(v)
}

/// Split the evaluated ANY/ALL test into one value per compared column.
fn split_test(v: Datum, n: usize) -> Result<Vec<Datum>> {
    if n == 1 {
        return Ok(vec![v]);
    }
    match v {
        Datum::Record(vs) if vs.len() == n => Ok(vs),
        Datum::Null => Ok(vec![Datum::Null; n]),
        other => Err(PgError::internal(format!(
            "ANY/ALL test of {n} columns evaluated to {}",
            other.variant_name()
        ))),
    }
}

/// Three-valued truth of an operator result.
fn to_tv(d: &Datum) -> Result<Option<bool>> {
    match d {
        Datum::Null => Ok(None),
        Datum::Bool(b) => Ok(Some(*b)),
        other => Err(PgError::internal(format!("comparison in sub-plan returned {}", other.variant_name()))),
    }
}

/// Compare the test values with one sub-plan row: AND over columns of `ops[i](test_i, row_i)`
/// (three-valued). A NULL on either side of a column gives NULL for that column (the comparison
/// operators are strict), and the operator is not called for it.
fn row_matches(ex: &mut Executor, ops: &[OpRef], tys: &[TypeId], test: &[Datum], r: &[Datum]) -> Result<Option<bool>> {
    let n = ops.len();
    if tys.len() < n || test.len() < n || r.len() < n {
        return Err(PgError::internal("ANY/ALL sub-plan row width does not match its comparison operators"));
    }
    let mut acc = Some(true);
    for i in 0..n {
        let tv = if test[i].is_null() || r[i].is_null() {
            None
        } else {
            let args = [test[i].clone(), r[i].clone()];
            let arg_types = [tys[i], tys[i]];
            let d = eval_call::call_operator(ex, &ops[i], &args, &arg_types, TypeId::BOOL)?;
            to_tv(&d)?
        };
        acc = and3(acc, tv);
        if acc == Some(false) {
            break;
        }
    }
    Ok(acc)
}

/// Running state of an ANY/ALL probe.
#[derive(Default)]
struct QState {
    any_row: bool,
    saw_null: bool,
    /// Decisive result: TRUE for ANY (first TRUE row), FALSE for ALL (first FALSE row).
    decided: Option<bool>,
}

/// Process one sub-plan row; `Ok(false)` stops the probe.
fn quant_step(
    ex: &mut Executor,
    sp: &SubPlanRef,
    tys: &[TypeId],
    test: &[Datum],
    r: &[Datum],
    is_all: bool,
    st: &mut QState,
) -> Result<bool> {
    st.any_row = true;
    match row_matches(ex, &sp.ops, tys, test, r)? {
        Some(b) if b != is_all => {
            st.decided = Some(b);
            Ok(false)
        }
        Some(_) => Ok(true),
        None => {
            st.saw_null = true;
            Ok(true)
        }
    }
}

/// Final ANY/ALL result from the probe state (module docs).
fn quant_result(st: &QState, is_all: bool) -> Datum {
    if let Some(b) = st.decided {
        return Datum::Bool(b);
    }
    if !st.any_row {
        return Datum::Bool(is_all);
    }
    if st.saw_null {
        Datum::Null
    } else {
        Datum::Bool(is_all)
    }
}

/// Rows of an uncorrelated ANY/ALL sub-plan, executed once and cached.
fn cached_rows(ex: &mut Executor, idx: usize, args: Vec<Datum>) -> Result<Rc<Vec<Row>>> {
    if let Some(SubPlanCache::Rows(rows)) = get_cache(ex, idx) {
        return Ok(Rc::clone(rows));
    }
    let mut rows: Vec<Row> = Vec::new();
    drive_subplan(ex, idx, args, |_ex, r| {
        rows.push(r);
        Ok(true)
    })?;
    let rc = Rc::new(rows);
    set_cache(ex, idx, SubPlanCache::Rows(Rc::clone(&rc)));
    Ok(rc)
}

/// Hashed `x IN (subquery)` (module docs): the set of non-NULL encoded values and whether a NULL was
/// seen, built once per execution.
fn eval_hashed_any(ex: &mut Executor, idx: usize, args: Vec<Datum>, test: &[Datum], tys: &[TypeId]) -> Result<Datum> {
    let ty = *tys.first().ok_or_else(|| PgError::internal("hashed ANY sub-plan has no output column"))?;
    let v = test.first().ok_or_else(|| PgError::internal("hashed ANY sub-plan without a test value"))?;

    if !matches!(get_cache(ex, idx), Some(SubPlanCache::Hashed { .. })) {
        let mut set: HashSet<Vec<u8>> = HashSet::new();
        let mut has_null = false;
        drive_subplan(ex, idx, args, |ex, r| {
            let x = first_column(&r)?;
            if x.is_null() {
                has_null = true;
            } else {
                set.insert(encode_one(ex, ty, &x)?);
            }
            Ok(true)
        })?;
        set_cache(ex, idx, SubPlanCache::Hashed { keys: Rc::new(set), has_null });
    }
    let (keys, has_null) = match get_cache(ex, idx) {
        Some(SubPlanCache::Hashed { keys, has_null }) => (Rc::clone(keys), *has_null),
        _ => return Err(PgError::internal("hashed ANY sub-plan cache missing")),
    };

    if v.is_null() {
        // No rows at all: FALSE even for a NULL test value; otherwise NULL.
        return Ok(if keys.is_empty() && !has_null { Datum::Bool(false) } else { Datum::Null });
    }
    let key = encode_one(ex, ty, v)?;
    Ok(if keys.contains(key.as_slice()) {
        Datum::Bool(true)
    } else if has_null {
        Datum::Null
    } else {
        Datum::Bool(false)
    })
}

/// `test op ANY (subquery)` and `test op ALL (subquery)` (`is_all`).
fn eval_quantified(
    ex: &mut Executor,
    sp: &SubPlanRef,
    idx: usize,
    row: &[Datum],
    args: Vec<Datum>,
    cacheable: bool,
    is_all: bool,
) -> Result<Datum> {
    let test_expr = sp
        .test
        .as_deref()
        .ok_or_else(|| PgError::internal("ANY/ALL sub-plan without a test expression"))?;
    let test_val = eval::eval(ex, test_expr, row)?;
    let n = sp.ops.len();
    let test = split_test(test_val, n)?;
    let tys = out_types(ex, idx);
    if tys.len() < n {
        return Err(PgError::internal("ANY/ALL sub-plan has fewer output columns than comparison operators"));
    }

    if sp.use_hash && cacheable && !is_all {
        return eval_hashed_any(ex, idx, args, &test, &tys);
    }

    let mut st = QState::default();
    if cacheable {
        let rows = cached_rows(ex, idx, args)?;
        for r in rows.iter() {
            if !quant_step(ex, sp, &tys, &test, r, is_all, &mut st)? {
                break;
            }
        }
    } else {
        drive_subplan(ex, idx, args, |ex, r| quant_step(ex, sp, &tys, &test, &r, is_all, &mut st))?;
    }
    Ok(quant_result(&st, is_all))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::Cell;

    use pgarbor_common::SqlState;
    use pgarbor_plan::plan::OutCol;
    use pgarbor_sql::ir::{BoundExpr, ExprKind};
    use pgarbor_types::context::IoConfig;

    use crate::context::{ExecContext, NoGuc, NoServices, SessionEnv};
    use crate::memtxn::MemTxn;
    use crate::runtime::{Runtime, RowSource, SubPlanRt};

    /// Test operator: yields `rows`, counts pulled rows and rescans.
    struct RowsOp {
        rows: Vec<Row>,
        pos: usize,
        pulled: Rc<Cell<u32>>,
        rescans: Rc<Cell<u32>>,
        fail: bool,
    }

    impl RowsOp {
        fn boxed(rows: Vec<Row>, pulled: &Rc<Cell<u32>>, rescans: &Rc<Cell<u32>>) -> Box<dyn RowSource> {
            Box::new(RowsOp { rows, pos: 0, pulled: Rc::clone(pulled), rescans: Rc::clone(rescans), fail: false })
        }
    }

    impl RowSource for RowsOp {
        fn next(&mut self, _ex: &mut Executor) -> Result<Option<Row>> {
            if self.fail {
                return Err(PgError::internal("boom"));
            }
            match self.rows.get(self.pos) {
                Some(r) => {
                    self.pos += 1;
                    self.pulled.set(self.pulled.get() + 1);
                    Ok(Some(r.clone()))
                }
                None => Ok(None),
            }
        }
        fn rescan(&mut self, _ex: &mut Executor, _params: &[Datum]) -> Result<()> {
            self.pos = 0;
            self.rescans.set(self.rescans.get() + 1);
            Ok(())
        }
    }

    fn sub_rt(op: Box<dyn RowSource>, kind: SubLinkKind, initplan: bool, volatility: Volatility) -> SubPlanRt {
        SubPlanRt {
            id: 0,
            kind,
            n_params: 0,
            is_initplan: initplan,
            volatility,
            out: vec![OutCol { name: "c".to_string(), ty: TypeId::INT4 }],
            op: Some(op),
            cache: None,
        }
    }

    fn sub_ref(kind: SubLinkKind) -> SubPlanRef {
        SubPlanRef { plan_id: 0, kind, test: None, ops: Vec::new(), args: Vec::new(), use_hash: false }
    }

    /// Run `f` with a fresh executor over an in-memory transaction and the bootstrapped catalog.
    fn with_exec<T>(f: impl FnOnce(&mut Executor) -> T) -> T {
        let mut txn = MemTxn::new();
        let cell = Cell::new(1u64);
        let guc = NoGuc;
        let services = NoServices;
        let (_, env) = pgarbor_catalog::testing::bootstrapped().expect("bootstrap catalog");
        let se = SessionEnv::new(env, IoConfig::default(), &guc, &services, &cell, 0, 0);
        let mut cx = ExecContext::new(&mut txn, se);
        let mut rt = Runtime::default();
        let mut ex = Executor::new(&mut rt, &mut cx);
        f(&mut ex)
    }

    fn int(v: i32) -> Datum {
        Datum::Int4(v)
    }

    #[test]
    fn exists_stops_after_first_row() {
        let pulled = Rc::new(Cell::new(0));
        let rescans = Rc::new(Cell::new(0));
        let rows = vec![vec![int(1)], vec![int(2)], vec![int(3)]];
        let op = RowsOp::boxed(rows, &pulled, &rescans);
        let r = with_exec(|ex| {
            ex.rt.subplans.push(sub_rt(op, SubLinkKind::Exists, false, Volatility::Volatile));
            eval_subplan(ex, &sub_ref(SubLinkKind::Exists), &[])
        });
        assert_eq!(r.ok(), Some(Datum::Bool(true)));
        assert_eq!(pulled.get(), 1);
        assert_eq!(rescans.get(), 1);
    }

    #[test]
    fn exists_no_rows_is_false() {
        let pulled = Rc::new(Cell::new(0));
        let rescans = Rc::new(Cell::new(0));
        let op = RowsOp::boxed(Vec::new(), &pulled, &rescans);
        let r = with_exec(|ex| {
            ex.rt.subplans.push(sub_rt(op, SubLinkKind::Exists, false, Volatility::Volatile));
            eval_subplan(ex, &sub_ref(SubLinkKind::Exists), &[])
        });
        assert_eq!(r.ok(), Some(Datum::Bool(false)));
    }

    #[test]
    fn scalar_two_rows_is_cardinality_error() {
        let pulled = Rc::new(Cell::new(0));
        let rescans = Rc::new(Cell::new(0));
        let op = RowsOp::boxed(vec![vec![int(1)], vec![int(2)]], &pulled, &rescans);
        let r = with_exec(|ex| {
            ex.rt.subplans.push(sub_rt(op, SubLinkKind::Scalar, false, Volatility::Volatile));
            eval_subplan(ex, &sub_ref(SubLinkKind::Scalar), &[])
        });
        assert!(matches!(&r, Err(e) if e.sqlstate == SqlState::CARDINALITY_VIOLATION));
    }

    #[test]
    fn scalar_no_rows_is_null_and_one_row_is_value() {
        let pulled = Rc::new(Cell::new(0));
        let rescans = Rc::new(Cell::new(0));
        let empty = RowsOp::boxed(Vec::new(), &pulled, &rescans);
        let one = RowsOp::boxed(vec![vec![int(7)]], &pulled, &rescans);
        let (a, b) = with_exec(|ex| {
            ex.rt.subplans.push(sub_rt(empty, SubLinkKind::Scalar, false, Volatility::Volatile));
            ex.rt.subplans.push(sub_rt(one, SubLinkKind::Scalar, false, Volatility::Volatile));
            let mut s1 = sub_ref(SubLinkKind::Scalar);
            let a = eval_subplan(ex, &s1, &[]);
            s1.plan_id = 1;
            let b = eval_subplan(ex, &s1, &[]);
            (a, b)
        });
        assert_eq!(a.ok(), Some(Datum::Null));
        assert_eq!(b.ok(), Some(int(7)));
    }

    #[test]
    fn initplan_runs_once_for_many_probes() {
        let pulled = Rc::new(Cell::new(0));
        let rescans = Rc::new(Cell::new(0));
        let op = RowsOp::boxed(vec![vec![int(5)]], &pulled, &rescans);
        let results = with_exec(|ex| {
            ex.rt.subplans.push(sub_rt(op, SubLinkKind::Scalar, true, Volatility::Stable));
            let sp = sub_ref(SubLinkKind::Scalar);
            let mut out = Vec::new();
            for _ in 0..3 {
                out.push(eval_subplan(ex, &sp, &[]));
            }
            out
        });
        assert_eq!(rescans.get(), 1);
        for r in results {
            assert_eq!(r.ok(), Some(int(5)));
        }
    }

    #[test]
    fn volatile_uncorrelated_subplan_reruns_per_probe() {
        let pulled = Rc::new(Cell::new(0));
        let rescans = Rc::new(Cell::new(0));
        let op = RowsOp::boxed(vec![vec![int(5)]], &pulled, &rescans);
        with_exec(|ex| {
            ex.rt.subplans.push(sub_rt(op, SubLinkKind::Scalar, true, Volatility::Volatile));
            let sp = sub_ref(SubLinkKind::Scalar);
            for _ in 0..3 {
                let _ = eval_subplan(ex, &sp, &[]);
            }
        });
        assert_eq!(rescans.get(), 3);
    }

    #[test]
    fn error_pops_frame_and_restores_operator() {
        let pulled = Rc::new(Cell::new(0));
        let rescans = Rc::new(Cell::new(0));
        let op = Box::new(RowsOp { rows: Vec::new(), pos: 0, pulled, rescans, fail: true });
        let (r, frames, restored) = with_exec(|ex| {
            ex.rt.subplans.push(sub_rt(op, SubLinkKind::Exists, false, Volatility::Volatile));
            let r = eval_subplan(ex, &sub_ref(SubLinkKind::Exists), &[]);
            (r.is_err(), ex.rt.outer.len(), ex.rt.subplans[0].op.is_some())
        });
        assert!(r);
        assert_eq!(frames, 0);
        assert!(restored);
    }

    #[test]
    fn array_keeps_nulls_and_empty_is_empty_array() {
        let pulled = Rc::new(Cell::new(0));
        let rescans = Rc::new(Cell::new(0));
        let op = RowsOp::boxed(vec![vec![int(1)], vec![Datum::Null]], &pulled, &rescans);
        let op2 = RowsOp::boxed(Vec::new(), &pulled, &rescans);
        let (a, b) = with_exec(|ex| {
            ex.rt.subplans.push(sub_rt(op, SubLinkKind::Array, false, Volatility::Volatile));
            ex.rt.subplans.push(sub_rt(op2, SubLinkKind::Array, false, Volatility::Volatile));
            let a = eval_subplan(ex, &sub_ref(SubLinkKind::Array), &[]);
            let mut s2 = sub_ref(SubLinkKind::Array);
            s2.plan_id = 1;
            let b = eval_subplan(ex, &s2, &[]);
            (a, b)
        });
        assert_eq!(a.ok(), Some(Datum::array(Array::from_vec(TypeId::INT4.oid, vec![int(1), Datum::Null]))));
        assert_eq!(b.ok(), Some(Datum::array(Array::empty(TypeId::INT4.oid))));
    }

    #[test]
    fn row_subplan_returns_record_and_rejects_two_rows() {
        let pulled = Rc::new(Cell::new(0));
        let rescans = Rc::new(Cell::new(0));
        let mut rt_one = sub_rt(RowsOp::boxed(vec![vec![int(1), int(2)]], &pulled, &rescans), SubLinkKind::Row, false, Volatility::Volatile);
        rt_one.out = vec![OutCol { name: "a".to_string(), ty: TypeId::INT4 }];
        let op_two = RowsOp::boxed(vec![vec![int(1)], vec![int(2)]], &pulled, &rescans);
        let (a, b) = with_exec(|ex| {
            ex.rt.subplans.push(rt_one);
            ex.rt.subplans.push(sub_rt(op_two, SubLinkKind::Row, false, Volatility::Volatile));
            let a = eval_subplan(ex, &sub_ref(SubLinkKind::Row), &[]);
            let mut s2 = sub_ref(SubLinkKind::Row);
            s2.plan_id = 1;
            let b = eval_subplan(ex, &s2, &[]);
            (a, b)
        });
        assert_eq!(a.ok(), Some(Datum::Record(vec![int(1), int(2)])));
        assert!(b.is_err());
    }

    #[test]
    fn quantified_test_value_split_rules() {
        assert_eq!(split_test(int(3), 1).ok(), Some(vec![int(3)]));
        assert_eq!(split_test(Datum::Null, 2).ok(), Some(vec![Datum::Null, Datum::Null]));
        assert!(split_test(int(3), 2).is_err());
    }

    #[test]
    fn quant_result_rules() {
        let none = QState::default();
        assert_eq!(quant_result(&none, false), Datum::Bool(false));
        assert_eq!(quant_result(&none, true), Datum::Bool(true));
        let nul = QState { any_row: true, saw_null: true, decided: None };
        assert_eq!(quant_result(&nul, false), Datum::Null);
        assert_eq!(quant_result(&nul, true), Datum::Null);
        let t = QState { any_row: true, saw_null: true, decided: Some(true) };
        assert_eq!(quant_result(&t, false), Datum::Bool(true));
    }

    /// `1 IN (select ...)` with rows {2, NULL} is NULL, {1, NULL} TRUE, no rows FALSE.
    #[test]
    #[ignore = "needs EXE-06"]
    fn in_subquery_three_valued() {
        let pulled = Rc::new(Cell::new(0));
        let rescans = Rc::new(Cell::new(0));
        let mk_sp = |plan_id: u32| SubPlanRef {
            plan_id,
            kind: SubLinkKind::Any,
            test: Some(Box::new(BoundExpr { kind: ExprKind::Const(int(1)), ty: TypeId::INT4 })),
            ops: vec![OpRef {
                op: 96,
                proc: 65,
                name: "=".to_string(),
                volatility: Volatility::Immutable,
                strict: true,
            }],
            args: Vec::new(),
            use_hash: false,
        };
        let (a, b, c) = with_exec(|ex| {
            ex.rt.subplans.push(sub_rt(
                RowsOp::boxed(vec![vec![int(2)], vec![Datum::Null]], &pulled, &rescans),
                SubLinkKind::Any,
                false,
                Volatility::Volatile,
            ));
            ex.rt.subplans.push(sub_rt(
                RowsOp::boxed(vec![vec![int(1)], vec![Datum::Null]], &pulled, &rescans),
                SubLinkKind::Any,
                false,
                Volatility::Volatile,
            ));
            ex.rt.subplans.push(sub_rt(RowsOp::boxed(Vec::new(), &pulled, &rescans), SubLinkKind::Any, false, Volatility::Volatile));
            (eval_subplan(ex, &mk_sp(0), &[]), eval_subplan(ex, &mk_sp(1), &[]), eval_subplan(ex, &mk_sp(2), &[]))
        });
        assert_eq!(a.ok(), Some(Datum::Null));
        assert_eq!(b.ok(), Some(Datum::Bool(true)));
        assert_eq!(c.ok(), Some(Datum::Bool(false)));
    }

    /// `x > ALL (select ...)` over an empty sub-plan is TRUE.
    #[test]
    #[ignore = "needs EXE-06"]
    fn all_over_empty_is_true() {
        let pulled = Rc::new(Cell::new(0));
        let rescans = Rc::new(Cell::new(0));
        let sp = SubPlanRef {
            plan_id: 0,
            kind: SubLinkKind::All,
            test: Some(Box::new(BoundExpr { kind: ExprKind::Const(int(1)), ty: TypeId::INT4 })),
            ops: vec![OpRef {
                op: 521,
                proc: 147,
                name: ">".to_string(),
                volatility: Volatility::Immutable,
                strict: true,
            }],
            args: Vec::new(),
            use_hash: false,
        };
        let r = with_exec(|ex| {
            ex.rt.subplans.push(sub_rt(RowsOp::boxed(Vec::new(), &pulled, &rescans), SubLinkKind::All, false, Volatility::Volatile));
            eval_subplan(ex, &sp, &[])
        });
        assert_eq!(r.ok(), Some(Datum::Bool(true)));
    }

    /// An uncorrelated init-plan used by an ANY probe runs once for many probes.
    #[test]
    #[ignore = "needs EXE-06"]
    fn initplan_any_runs_once() {
        let pulled = Rc::new(Cell::new(0));
        let rescans = Rc::new(Cell::new(0));
        let sp = SubPlanRef {
            plan_id: 0,
            kind: SubLinkKind::Any,
            test: Some(Box::new(BoundExpr { kind: ExprKind::Const(int(2)), ty: TypeId::INT4 })),
            ops: vec![OpRef {
                op: 96,
                proc: 65,
                name: "=".to_string(),
                volatility: Volatility::Immutable,
                strict: true,
            }],
            args: Vec::new(),
            use_hash: false,
        };
        with_exec(|ex| {
            ex.rt.subplans.push(sub_rt(
                RowsOp::boxed(vec![vec![int(1)], vec![int(2)]], &pulled, &rescans),
                SubLinkKind::Any,
                true,
                Volatility::Stable,
            ));
            for _ in 0..4 {
                let _ = eval_subplan(ex, &sp, &[]);
            }
        });
        assert_eq!(rescans.get(), 1);
    }
}
