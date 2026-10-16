//! Function-driven operators (lease EXE-14): `FunctionScan` and `ProjectSet`. Evaluation of a single
//! set-returning call is `eval_srf::open_srf`. PG: `nodeFunctionscan.c`, `nodeProjectSet.c`.
//!
//! ## FunctionScan (`FROM f(...)`, `ROWS FROM (f(), g())`, `unnest(a, b)`, `WITH ORDINALITY`)
//! * On each (re)start evaluate every `FunctionItem::expr` (a `FuncCall`; a scalar non-SRF call yields
//!   exactly one row) over the EMPTY row (arguments may use `Param`/`OuterRef`) with
//!   `eval_srf::open_srf`.
//! * Zip the functions: row i = the i-th row of every function, a function that ran out contributes
//!   NULLs for all its `columns`; stop when ALL functions are exhausted. Each function value is expanded
//!   with `eval_srf::expand_record(value, item.columns.len())` when `columns.len() > 1`.
//! * Output row = concatenated function columns; with `with_ordinality` one trailing `Int8` (1-based
//!   row number). `filter` over the produced row.
//! * Strict SRFs with a NULL argument produce zero rows (handled inside `open_srf`).
//!
//! ## ProjectSet (`SELECT generate_series(1,3), a FROM t`, `SELECT unnest(arr) ...`)
//! For each input row evaluate `exprs`. An expression is a *set expression* if it is a top-level
//! `FuncCall` with `retset` (nested SRFs inside other expressions are rejected by the binder).
//! Non-set expressions are evaluated once per input row and repeated. Set expressions advance in LOCK
//! STEP (PG >= 10 semantics): output row k contains the k-th value of every set expression, finished
//! ones contribute NULL, and rows are produced until ALL set expressions are exhausted. An input row
//! for which every set expression yields zero rows produces no output at all. Composite results are
//! NOT expanded here (the planner puts a `FunctionScan` for `SELECT * FROM f()`).
//! Output width = `exprs.len()`.
use pgarbor_plan::plan::{FunctionItem, FunctionScan, PlanNode, ProjectSetNode};
use pgarbor_common::Result;
use pgarbor_sql::ir::{BoundExpr, ExprKind};
use pgarbor_types::{Datum, TypeEnv};

use crate::build::BuildCtx;
use crate::eval::{eval, eval_opt_qual};
use crate::eval_srf::{contains_srf, expand_record, open_srf, SrfIter};
use crate::runtime::{Executor, Row, RowSource};

/// `FunctionScan` operator. Owns copies of the function items and the filter (runtime docs).
struct FunctionScanOp {
    funcs: Vec<FunctionItem>,
    with_ordinality: bool,
    filter: Option<BoundExpr>,
    /// `None` = not started since the last (re)start; `Some` = one iterator per function, `None`
    /// inside once that function is exhausted. Taken out while a row is being pulled.
    iters: Option<Vec<Option<SrfIter>>>,
    /// Set once every function is exhausted: further pulls return `None`.
    done: bool,
    /// Rows produced before the filter (the ordinality counter).
    ordinal: i64,
    ticks: u32,
}

impl FunctionScanOp {
    /// Evaluate every function over the empty row (module docs).
    fn start(&self, ex: &mut Executor) -> Result<Vec<Option<SrfIter>>> {
        let mut iters: Vec<Option<SrfIter>> = Vec::with_capacity(self.funcs.len());
        for item in &self.funcs {
            let it: SrfIter = match &item.expr.kind {
                ExprKind::FuncCall(call) => open_srf(ex, call, item.expr.ty, &[])?,
                _ => {
                    // Not a call (defensive): a scalar expression yields exactly one row.
                    let v = eval(ex, &item.expr, &[])?;
                    let once: SrfIter = Box::new(std::iter::once(Ok(v)));
                    once
                }
            };
            iters.push(Some(it));
        }
        Ok(iters)
    }

    /// Zip the next row of every function (module docs), without the filter.
    fn pull(&mut self, ex: &mut Executor) -> Result<Option<Row>> {
        if self.done {
            return Ok(None);
        }
        let mut iters = match self.iters.take() {
            Some(v) => v,
            None => self.start(ex)?,
        };
        let mut row: Row = Vec::new();
        let mut live = false;
        for (item, slot) in self.funcs.iter().zip(iters.iter_mut()) {
            let width = item.columns.len();
            let got = match slot.as_mut() {
                Some(it) => it.next(),
                None => None,
            };
            match got {
                Some(value) => {
                    live = true;
                    let value = value?;
                    let composite = item.expr.ty.oid == pgarbor_types::oids::RECORD
                        || ex.se().env.composite_fields(item.expr.ty.oid).is_some();
                    if composite {
                        match value {
                            Datum::Record(fields) if fields.len() == width => row.extend(fields),
                            Datum::Null => row.extend(vec![Datum::Null; width]),
                            _ => return Err(pgarbor_common::PgError::internal("function composite result does not match its declared columns")),
                        }
                    } else if width > 1 {
                        row.extend(expand_record(value, width)?);
                    } else if width == 1 {
                        row.push(value);
                    }
                }
                None => {
                    *slot = None;
                    for _ in 0..width {
                        row.push(Datum::Null);
                    }
                }
            }
        }
        if !live {
            self.done = true;
            return Ok(None);
        }
        self.ordinal += 1;
        if self.with_ordinality {
            row.push(Datum::Int8(self.ordinal));
        }
        self.iters = Some(iters);
        Ok(Some(row))
    }
}

impl RowSource for FunctionScanOp {
    fn next(&mut self, ex: &mut Executor) -> Result<Option<Row>> {
        loop {
            if self.done {
                return Ok(None);
            }
            ex.tick(&mut self.ticks)?;
            let row = match self.pull(ex)? {
                Some(r) => r,
                None => return Ok(None),
            };
            if eval_opt_qual(ex, &self.filter, &row)? {
                return Ok(Some(row));
            }
        }
    }

    fn rescan(&mut self, _ex: &mut Executor, _params: &[Datum]) -> Result<()> {
        // The functions are re-evaluated on the next pull, so `OuterRef`s in their arguments see the
        // parameter frame in force then (runtime docs: the parent pushes it with `with_outer`).
        self.iters = None;
        self.done = false;
        self.ordinal = 0;
        Ok(())
    }
}

/// One output slot of a `ProjectSet` for the current input row.
enum Slot {
    /// Non-set expression: value computed once for the input row.
    Fixed(Datum),
    /// Set expression: its iterator (`None` once exhausted).
    Srf(Option<SrfIter>),
    /// Expression with set-returning calls nested inside it (`prepare_items`): the calls advance in
    /// lock step with the other slots, and the expression is evaluated over the input row extended
    /// with their current values (NULL once a call is exhausted).
    Nested { expr: BoundExpr, srfs: Vec<Option<SrfIter>>, values: Vec<Datum>, row: Row },
}

/// A target-list item of `ProjectSet`, prepared once at build time.
enum ProjItem {
    /// No set-returning call, or a top-level one (`Slot::Fixed` / `Slot::Srf`).
    Plain(BoundExpr),
    /// `expr` reads each set-returning call (`calls`, in order) from slot `width + j` of the extended row.
    Nested { expr: BoundExpr, calls: Vec<BoundExpr> },
}

/// Split each target expression into plain items and items with nested set-returning calls. Calls are
/// replaced by `InputRef(width + j)` and are not descended into (their arguments are evaluated when the
/// call is opened). Sub-plans are left alone: their own operator tree handles any set-returning call.
fn prepare_items(exprs: &[BoundExpr], width: usize) -> Result<Vec<ProjItem>> {
    let mut items = Vec::with_capacity(exprs.len());
    for e in exprs {
        let top_level_srf = matches!(&e.kind, ExprKind::FuncCall(c) if c.retset);
        if top_level_srf || !contains_srf(e)? {
            items.push(ProjItem::Plain(e.clone()));
            continue;
        }
        let mut calls: Vec<BoundExpr> = Vec::new();
        let expr = e.clone().transform_down(&mut |node: BoundExpr| {
            let is_srf = matches!(&node.kind, ExprKind::FuncCall(c) if c.retset);
            if is_srf {
                let idx = width + calls.len();
                let ty = node.ty;
                calls.push(node);
                Ok((BoundExpr { kind: ExprKind::InputRef(idx as u32), ty }, false))
            } else {
                let stop = matches!(&node.kind, ExprKind::SubPlan(_));
                Ok((node, !stop))
            }
        })?;
        items.push(ProjItem::Nested { expr, calls });
    }
    Ok(items)
}

/// Open the slots of one input row.
fn open_slots(ex: &mut Executor, items: &[ProjItem], row: &[Datum]) -> Result<Vec<Slot>> {
    let mut slots: Vec<Slot> = Vec::with_capacity(items.len());
    for item in items {
        match item {
            ProjItem::Plain(e) => match &e.kind {
                ExprKind::FuncCall(call) if call.retset => {
                    let it = open_srf(ex, call, e.ty, row)?;
                    slots.push(Slot::Srf(Some(it)));
                }
                _ => {
                    let v = eval(ex, e, row)?;
                    slots.push(Slot::Fixed(v));
                }
            },
            ProjItem::Nested { expr, calls, .. } => {
                let mut srfs: Vec<Option<SrfIter>> = Vec::with_capacity(calls.len());
                for c in calls {
                    match &c.kind {
                        ExprKind::FuncCall(call) => srfs.push(Some(open_srf(ex, call, c.ty, row)?)),
                        _ => return Err(pgarbor_common::PgError::internal("ProjectSet nested call is not a function")),
                    }
                }
                let values = vec![Datum::Null; calls.len()];
                slots.push(Slot::Nested { expr: expr.clone(), srfs, values, row: row.to_vec() });
            }
        }
    }
    Ok(slots)
}

/// Next value of one set-returning iterator (`None` once exhausted; the iterator is then dropped).
fn advance(it: &mut Option<SrfIter>) -> Result<Option<Datum>> {
    let got = match it.as_mut() {
        Some(i) => i.next(),
        None => None,
    };
    match got {
        Some(v) => Ok(Some(v?)),
        None => {
            *it = None;
            Ok(None)
        }
    }
}

/// `ProjectSet` operator (module docs).
struct ProjectSetOp {
    input: Box<dyn RowSource>,
    exprs: Vec<ProjItem>,
    /// Slots of the current input row; `None` when the next input row must be fetched.
    slots: Option<Vec<Slot>>,
    done: bool,
    ticks: u32,
}

impl RowSource for ProjectSetOp {
    fn next(&mut self, ex: &mut Executor) -> Result<Option<Row>> {
        loop {
            if self.done {
                return Ok(None);
            }
            ex.tick(&mut self.ticks)?;
            let mut slots = match self.slots.take() {
                Some(s) => s,
                None => match self.input.next(ex)? {
                    Some(input_row) => open_slots(ex, &self.exprs, &input_row)?,
                    None => {
                        self.done = true;
                        return Ok(None);
                    }
                },
            };
            let any_srf = slots.iter().any(|s| !matches!(s, Slot::Fixed(_)));
            let mut out: Row = Vec::with_capacity(slots.len());
            // Output positions of nested items, evaluated once every set has been advanced.
            let mut pending: Vec<(usize, usize)> = Vec::new();
            let mut live = false;
            for (si, slot) in slots.iter_mut().enumerate() {
                match slot {
                    Slot::Fixed(v) => out.push(v.clone()),
                    Slot::Srf(it) => match advance(it)? {
                        Some(v) => {
                            out.push(v);
                            live = true;
                        }
                        None => out.push(Datum::Null),
                    },
                    Slot::Nested { srfs, values, .. } => {
                        for (j, it) in srfs.iter_mut().enumerate() {
                            match advance(it)? {
                                Some(v) => {
                                    values[j] = v;
                                    live = true;
                                }
                                None => values[j] = Datum::Null,
                            }
                        }
                        pending.push((si, out.len()));
                        out.push(Datum::Null);
                    }
                }
            }
            // Without any SRF the input row produces exactly one output row; with SRFs, the input
            // row is finished once every one of them is exhausted (the loop then fetches the next).
            if live || !any_srf {
                for (si, oi) in pending {
                    if let Slot::Nested { expr, values, row, .. } = &slots[si] {
                        let mut extended: Row = row.clone();
                        extended.extend(values.iter().cloned());
                        out[oi] = eval(ex, expr, &extended)?;
                    }
                }
                if any_srf {
                    self.slots = Some(slots);
                }
                return Ok(Some(out));
            }
        }
    }

    fn rescan(&mut self, ex: &mut Executor, params: &[Datum]) -> Result<()> {
        self.input.rescan(ex, params)?;
        self.slots = None;
        self.done = false;
        Ok(())
    }
}

pub fn build_function_scan(node: &PlanNode, n: &FunctionScan, b: &mut BuildCtx) -> Result<Box<dyn RowSource>> {
    // A leaf: no children to build (and no node numbering beyond this node's own).
    let _ = node;
    let _ = b;
    Ok(Box::new(FunctionScanOp {
        funcs: n.funcs.clone(),
        with_ordinality: n.with_ordinality,
        filter: n.filter.clone(),
        iters: None,
        done: false,
        ordinal: 0,
        ticks: 0,
    }))
}

pub fn build_project_set(node: &PlanNode, n: &ProjectSetNode, b: &mut BuildCtx) -> Result<Box<dyn RowSource>> {
    let _ = node;
    let input = b.child(&n.input)?;
    let exprs = prepare_items(&n.exprs, n.input.out.len())?;
    Ok(Box::new(ProjectSetOp { input, exprs, slots: None, done: false, ticks: 0 }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::Cell;

    use pgarbor_sql::ir::FuncCallExpr;
    use pgarbor_types::catalog_data::Volatility;
    use pgarbor_types::context::IoConfig;
    use pgarbor_types::TypeId;

    use crate::context::{ExecContext, NoGuc, NoServices, SessionEnv};
    use crate::memtxn::MemTxn;
    use crate::runtime::Runtime;

    /// OID of `generate_series(integer, integer)` (contracts/reference/pg18/pg_proc.csv).
    const GENERATE_SERIES_INT4: u32 = 1067;
    /// OID of `unnest(anyarray)` (contracts/reference/pg18/pg_proc.csv).
    const UNNEST_ANYARRAY: u32 = 2331;

    fn int4(v: i32) -> BoundExpr {
        BoundExpr { kind: ExprKind::Const(Datum::Int4(v)), ty: TypeId::INT4 }
    }

    fn null_of(ty: TypeId) -> BoundExpr {
        BoundExpr { kind: ExprKind::Const(Datum::Null), ty }
    }

    fn srf(oid: u32, name: &str, args: Vec<BoundExpr>, ty: TypeId) -> BoundExpr {
        BoundExpr {
            kind: ExprKind::FuncCall(Box::new(FuncCallExpr {
                func: oid,
                name: name.to_string(),
                args,
                variadic: false,
                retset: true,
                volatility: Volatility::Immutable,
                strict: true,
                is_cast_call: false,
            })),
            ty,
        }
    }

    fn gen_series(a: i32, b: i32) -> BoundExpr {
        srf(GENERATE_SERIES_INT4, "generate_series", vec![int4(a), int4(b)], TypeId::INT4)
    }

    /// Input of exactly one empty row (a source for ProjectSet tests).
    struct OneRow {
        done: bool,
    }

    impl RowSource for OneRow {
        fn next(&mut self, _ex: &mut Executor) -> Result<Option<Row>> {
            if self.done {
                return Ok(None);
            }
            self.done = true;
            Ok(Some(Vec::new()))
        }
        fn rescan(&mut self, _ex: &mut Executor, _params: &[Datum]) -> Result<()> {
            self.done = false;
            Ok(())
        }
    }

    fn with_ex<T>(f: impl FnOnce(&mut Executor) -> T) -> T {
        let mut txn = MemTxn::new();
        let cell = Cell::new(1u64);
        let (_kv, env) = pgarbor_catalog::testing::bootstrapped().expect("bootstrap catalog");
        let se = SessionEnv::new(env, IoConfig::default(), &NoGuc, &NoServices, &cell, 0, 0);
        let mut cx = ExecContext::new(&mut txn, se);
        let mut rt = Runtime::default();
        let mut ex = Executor::new(&mut rt, &mut cx);
        f(&mut ex)
    }

    fn drain(op: &mut impl RowSource, ex: &mut Executor) -> Vec<Row> {
        let mut out = Vec::new();
        while let Some(r) = op.next(ex).expect("next row") {
            out.push(r);
        }
        out
    }

    fn func_scan(funcs: Vec<FunctionItem>, with_ordinality: bool) -> FunctionScanOp {
        FunctionScanOp {
            funcs,
            with_ordinality,
            filter: None,
            iters: None,
            done: false,
            ordinal: 0,
            ticks: 0,
        }
    }

    fn item(expr: BoundExpr) -> FunctionItem {
        FunctionItem { expr, columns: vec![TypeId::INT4] }
    }

    #[test]
    #[ignore = "needs EXE-05 and EXE-08"]
    fn project_set_lock_step_pads_with_null() {
        with_ex(|ex| {
            let mut op = ProjectSetOp {
                input: Box::new(OneRow { done: false }),
                exprs: prepare_items(&[gen_series(1, 3), gen_series(1, 2)], 0).expect("items"),
                slots: None,
                done: false,
                ticks: 0,
            };
            let rows = drain(&mut op, ex);
            assert_eq!(
                rows,
                vec![
                    vec![Datum::Int4(1), Datum::Int4(1)],
                    vec![Datum::Int4(2), Datum::Int4(2)],
                    vec![Datum::Int4(3), Datum::Null],
                ]
            );
        });
    }

    #[test]
    #[ignore = "needs EXE-05 and EXE-08"]
    fn project_set_repeats_non_set_expression() {
        with_ex(|ex| {
            let mut op = ProjectSetOp {
                input: Box::new(OneRow { done: false }),
                exprs: prepare_items(&[int4(7), gen_series(1, 2)], 0).expect("items"),
                slots: None,
                done: false,
                ticks: 0,
            };
            let rows = drain(&mut op, ex);
            assert_eq!(rows, vec![vec![Datum::Int4(7), Datum::Int4(1)], vec![Datum::Int4(7), Datum::Int4(2)]]);
        });
    }

    #[test]
    #[ignore = "needs EXE-05 and EXE-08"]
    fn project_set_unnest_of_null_array_yields_no_rows() {
        with_ex(|ex| {
            let arr_ty = TypeId::INT4_ARRAY;
            let mut op = ProjectSetOp {
                input: Box::new(OneRow { done: false }),
                exprs: prepare_items(&[srf(UNNEST_ANYARRAY, "unnest", vec![null_of(arr_ty)], TypeId::INT4)], 0).expect("items"),
                slots: None,
                done: false,
                ticks: 0,
            };
            assert!(drain(&mut op, ex).is_empty());
        });
    }

    #[test]
    #[ignore = "needs EXE-05 and EXE-08"]
    fn function_scan_rows_from_pads_with_null() {
        with_ex(|ex| {
            let mut op = func_scan(vec![item(gen_series(1, 3)), item(gen_series(1, 2))], false);
            let rows = drain(&mut op, ex);
            assert_eq!(
                rows,
                vec![
                    vec![Datum::Int4(1), Datum::Int4(1)],
                    vec![Datum::Int4(2), Datum::Int4(2)],
                    vec![Datum::Int4(3), Datum::Null],
                ]
            );
        });
    }

    #[test]
    #[ignore = "needs EXE-05 and EXE-08"]
    fn function_scan_with_ordinality_numbers_rows() {
        with_ex(|ex| {
            let mut op = func_scan(vec![item(gen_series(1, 2))], true);
            let rows = drain(&mut op, ex);
            assert_eq!(
                rows,
                vec![vec![Datum::Int4(1), Datum::Int8(1)], vec![Datum::Int4(2), Datum::Int8(2)]]
            );
        });
    }

    #[test]
    #[ignore = "needs EXE-05 and EXE-08"]
    fn function_scan_strict_null_argument_yields_no_rows() {
        with_ex(|ex| {
            let call = srf(GENERATE_SERIES_INT4, "generate_series", vec![null_of(TypeId::INT4), int4(3)], TypeId::INT4);
            let mut op = func_scan(vec![item(call)], false);
            assert!(drain(&mut op, ex).is_empty());
        });
    }

    #[test]
    #[ignore = "needs EXE-05 and EXE-08"]
    fn function_scan_rescan_restarts_and_resets_ordinality() {
        with_ex(|ex| {
            let mut op = func_scan(vec![item(gen_series(1, 2))], true);
            let first = drain(&mut op, ex);
            op.rescan(ex, &[]).expect("rescan");
            let second = drain(&mut op, ex);
            assert_eq!(first, second);
            assert_eq!(second.len(), 2);
        });
    }
}
