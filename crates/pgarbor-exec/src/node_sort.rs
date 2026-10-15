//! `Sort` and `Limit` operators (lease EXE-23, north-star). PG: `nodeSort.c`, `nodeLimit.c`.
//!
//! ## Sort
//! Blocking: on the first `next` drain the input, then emit in key order. Keys = `SortKeyRt::from_spec`
//! of `keys` (evaluated ONCE per row), compared with `sortutil::compare_keys`; `sortutil::sort_rows` for
//! the full sort. With `limit: Some(n)` (top-N: offset already added) keep only the best `n` rows with
//! `sortutil::TopN`. Output row = input row. `rescan(params)`: if `ex.epoch()` is unchanged since the
//! sort was done and the input is parameter-independent re-emit from the start without re-reading;
//! otherwise rescan the input and sort again.
//!
//! ## Limit
//! `count`/`offset` expressions are evaluated ONCE per scan over the empty row (`Param`/`Const`/stable):
//! `count` NULL = no limit; `offset` NULL = 0; negative `count` -> `error::limit_negative()` (2201W),
//! negative `offset` -> `error::offset_negative()` (2201X) (checked when the scan starts, before the
//! first row). Skip `offset` rows, then emit up to `count` rows. `count == 0` must NOT pull any input
//! row. `with_ties`: after `count` rows continue emitting while the next input row's `ties_keys` values
//! are equal (`compare_keys` == Equal) to the LAST emitted row's (input is sorted by them); `count == 0`
//! with ties emits nothing. Output row = input row. Int8 limit values (`Datum::Int8`).
use std::cmp::Ordering;

use pgarbor_common::{PgError, Result};
use pgarbor_plan::plan::{LimitNode, PlanNode, SortNode};
use pgarbor_sql::ir::BoundExpr;
use pgarbor_types::Datum;

use crate::build::BuildCtx;
use crate::error;
use crate::eval::eval;
use crate::runtime::{Executor, Row, RowSource};
use crate::sortutil::{compare_keys, eval_keys, sort_rows, SortKeyRt, TopN};

/// Sort operator state (module docs).
struct SortOp {
    input: Box<dyn RowSource>,
    keys: Vec<SortKeyRt>,
    /// Top-N bound (`SortNode::limit`), converted to `usize`.
    limit: Option<usize>,
    /// Sorted output of the current pass; valid when `filled`.
    rows: Vec<Row>,
    /// Next output position.
    pos: usize,
    filled: bool,
    /// Epoch observed when `rows` was built.
    filled_epoch: u64,
    /// Whether the pass that built `rows` had no parameters.
    filled_no_params: bool,
    /// Whether the current pass has no parameters (set by `rescan`; true before the first rescan).
    cur_no_params: bool,
    ticks: u32,
}

impl SortOp {
    /// Drain the input and sort it into `rows`.
    fn fill(&mut self, ex: &mut Executor) -> Result<()> {
        let limit = self.limit;
        let rows: Vec<Row> = match limit {
            None => {
                let mut all: Vec<Row> = Vec::new();
                while let Some(row) = self.input.next(ex)? {
                    ex.tick(&mut self.ticks)?;
                    all.push(row);
                }
                sort_rows(ex, &self.keys, &mut all)?;
                all
            }
            Some(n) => {
                let mut top = TopN::new(n);
                while let Some(row) = self.input.next(ex)? {
                    ex.tick(&mut self.ticks)?;
                    let kv = eval_keys(ex, &self.keys, &row)?;
                    top.push(ex, &self.keys, kv, row)?;
                }
                top.into_sorted(ex, &self.keys)?
            }
        };
        self.rows = rows;
        self.pos = 0;
        self.filled = true;
        self.filled_epoch = ex.epoch();
        self.filled_no_params = self.cur_no_params;
        Ok(())
    }
}

impl RowSource for SortOp {
    fn next(&mut self, ex: &mut Executor) -> Result<Option<Row>> {
        if !self.filled {
            self.fill(ex)?;
        }
        if self.pos >= self.rows.len() {
            return Ok(None);
        }
        // Clone instead of taking: a rescan may re-emit the buffered rows.
        let row = self.rows[self.pos].clone();
        self.pos += 1;
        Ok(Some(row))
    }

    fn rescan(&mut self, ex: &mut Executor, params: &[Datum]) -> Result<()> {
        let no_params = params.is_empty();
        let reuse = self.filled && self.filled_no_params && no_params && self.filled_epoch == ex.epoch();
        self.cur_no_params = no_params;
        self.pos = 0;
        if reuse {
            return Ok(());
        }
        self.input.rescan(ex, params)?;
        self.rows.clear();
        self.filled = false;
        Ok(())
    }
}

pub fn build_sort(node: &PlanNode, n: &SortNode, b: &mut BuildCtx) -> Result<Box<dyn RowSource>> {
    let _ = node;
    let input = b.child(&n.input)?;
    let limit = match n.limit {
        Some(l) => Some(usize::try_from(l).unwrap_or(usize::MAX)),
        None => None,
    };
    Ok(Box::new(SortOp {
        input,
        keys: n.keys.iter().map(SortKeyRt::from_spec).collect(),
        limit,
        rows: Vec::new(),
        pos: 0,
        filled: false,
        filled_epoch: 0,
        filled_no_params: true,
        cur_no_params: true,
        ticks: 0,
    }))
}

/// Limit operator state (module docs).
struct LimitOp {
    input: Box<dyn RowSource>,
    count: Option<BoundExpr>,
    offset: Option<BoundExpr>,
    with_ties: bool,
    ties_keys: Vec<SortKeyRt>,
    /// The scan has evaluated its bounds (reset by `rescan`).
    started: bool,
    /// The scan is over; further calls return `None` without pulling input.
    finished: bool,
    /// Effective row limit of this scan; `None` = no limit.
    limit: Option<u64>,
    /// Input rows still to discard for OFFSET.
    to_skip: u64,
    /// Rows emitted by this scan so far.
    emitted: u64,
    /// Tie keys of the last row counted by LIMIT (WITH TIES).
    last_keys: Vec<Datum>,
    ticks: u32,
}

/// Integer value of a LIMIT/OFFSET result.
fn int8_value(d: &Datum) -> Result<i64> {
    match d {
        Datum::Int8(v) => Ok(*v),
        Datum::Int4(v) => Ok(i64::from(*v)),
        Datum::Int2(v) => Ok(i64::from(*v)),
        _ => Err(PgError::internal("LIMIT/OFFSET value is not an integer")),
    }
}

impl LimitOp {
    /// Evaluate OFFSET and LIMIT for this scan (offset first, as PostgreSQL does).
    fn start(&mut self, ex: &mut Executor) -> Result<()> {
        let offset_value = match &self.offset {
            Some(e) => eval(ex, e, &[])?,
            None => Datum::Null,
        };
        let skip: u64 = match offset_value {
            Datum::Null => 0,
            d => {
                let v = int8_value(&d)?;
                if v < 0 {
                    return Err(error::offset_negative());
                }
                v as u64
            }
        };
        let count_value = match &self.count {
            Some(e) => eval(ex, e, &[])?,
            None => Datum::Null,
        };
        let limit: Option<u64> = match count_value {
            Datum::Null => None,
            d => {
                let v = int8_value(&d)?;
                if v < 0 {
                    return Err(error::limit_negative());
                }
                Some(v as u64)
            }
        };
        self.to_skip = skip;
        self.limit = limit;
        self.started = true;
        Ok(())
    }
}

impl RowSource for LimitOp {
    fn next(&mut self, ex: &mut Executor) -> Result<Option<Row>> {
        if self.finished {
            return Ok(None);
        }
        if !self.started {
            self.start(ex)?;
        }
        // LIMIT 0 (with or without ties) emits nothing and must not touch the input.
        if self.limit == Some(0) {
            self.finished = true;
            return Ok(None);
        }
        while self.to_skip > 0 {
            ex.tick(&mut self.ticks)?;
            match self.input.next(ex)? {
                Some(_) => self.to_skip -= 1,
                None => {
                    self.finished = true;
                    return Ok(None);
                }
            }
        }
        let reached = match self.limit {
            Some(n) => self.emitted >= n,
            None => false,
        };
        if !reached {
            return match self.input.next(ex)? {
                Some(row) => {
                    self.emitted += 1;
                    if self.with_ties && self.limit == Some(self.emitted) {
                        self.last_keys = eval_keys(ex, &self.ties_keys, &row)?;
                    }
                    Ok(Some(row))
                }
                None => {
                    self.finished = true;
                    Ok(None)
                }
            };
        }
        // The count is reached: only WITH TIES continues, while rows tie with the last counted row.
        if !self.with_ties {
            self.finished = true;
            return Ok(None);
        }
        match self.input.next(ex)? {
            None => {
                self.finished = true;
                Ok(None)
            }
            Some(row) => {
                let keys = eval_keys(ex, &self.ties_keys, &row)?;
                if compare_keys(ex, &self.ties_keys, &self.last_keys, &keys)? == Ordering::Equal {
                    Ok(Some(row))
                } else {
                    self.finished = true;
                    Ok(None)
                }
            }
        }
    }

    fn rescan(&mut self, ex: &mut Executor, params: &[Datum]) -> Result<()> {
        self.started = false;
        self.finished = false;
        self.limit = None;
        self.to_skip = 0;
        self.emitted = 0;
        self.last_keys.clear();
        self.input.rescan(ex, params)
    }
}

pub fn build_limit(node: &PlanNode, n: &LimitNode, b: &mut BuildCtx) -> Result<Box<dyn RowSource>> {
    let _ = node;
    let input = b.child(&n.input)?;
    Ok(Box::new(LimitOp {
        input,
        count: n.count.clone(),
        offset: n.offset.clone(),
        with_ties: n.with_ties,
        ties_keys: n.ties_keys.iter().map(SortKeyRt::from_spec).collect(),
        started: false,
        finished: false,
        limit: None,
        to_skip: 0,
        emitted: 0,
        last_keys: Vec::new(),
        ticks: 0,
    }))
}

#[cfg(test)]
mod tests {
    use std::cell::Cell;
    use std::collections::VecDeque;

    use pgarbor_sql::ir::{ExprKind, BoundExpr};
    use pgarbor_types::context::IoConfig;
    use pgarbor_types::TypeId;

    use super::*;
    use crate::context::{ExecContext, SessionEnv};
    use crate::memtxn::MemTxn;
    use crate::runtime::Runtime;
    use crate::{NoGuc, NoServices};

    /// Input operator that yields preset rows and counts how many were pulled.
    struct VecSource {
        rows: VecDeque<Row>,
        pulls: std::rc::Rc<Cell<u32>>,
    }

    impl RowSource for VecSource {
        fn next(&mut self, _ex: &mut Executor) -> Result<Option<Row>> {
            self.pulls.set(self.pulls.get() + 1);
            Ok(self.rows.pop_front())
        }
        fn rescan(&mut self, _ex: &mut Executor, _params: &[Datum]) -> Result<()> {
            Ok(())
        }
    }

    fn row(v: i64) -> Row {
        vec![Datum::Int8(v)]
    }

    fn int8_const(v: i64) -> BoundExpr {
        BoundExpr { kind: ExprKind::Const(Datum::Int8(v)), ty: TypeId::INT8 }
    }

    fn with_executor<T>(f: impl FnOnce(&mut Executor) -> Result<T>) -> Result<T> {
        let (_kv, env) = pgarbor_catalog::testing::bootstrapped()?;
        let guc = NoGuc;
        let services = NoServices;
        let rng = Cell::new(1u64);
        let se = SessionEnv::new(env, IoConfig::default(), &guc, &services, &rng, 0, 0);
        let mut txn = MemTxn::new();
        let mut cx = ExecContext::new(&mut txn, se);
        let mut rt = Runtime::default();
        let mut ex = Executor::new(&mut rt, &mut cx);
        f(&mut ex)
    }

    fn limit_op(input: Box<dyn RowSource>, count: Option<BoundExpr>, offset: Option<BoundExpr>) -> LimitOp {
        LimitOp {
            input,
            count,
            offset,
            with_ties: false,
            ties_keys: Vec::new(),
            started: false,
            finished: false,
            limit: None,
            to_skip: 0,
            emitted: 0,
            last_keys: Vec::new(),
            ticks: 0,
        }
    }

    fn drain(op: &mut dyn RowSource, ex: &mut Executor) -> Result<Vec<Row>> {
        let mut out = Vec::new();
        while let Some(r) = op.next(ex)? {
            out.push(r);
        }
        Ok(out)
    }

    #[test]
    fn limit_without_bounds_passes_rows_through() {
        let pulls = std::rc::Rc::new(Cell::new(0));
        let src = VecSource { rows: VecDeque::from(vec![row(1), row(2)]), pulls: pulls.clone() };
        let out = with_executor(|ex| {
            let mut op = limit_op(Box::new(src), None, None);
            drain(&mut op, ex)
        });
        assert_eq!(out.ok().expect("limit without bounds"), vec![row(1), row(2)]);
        assert_eq!(pulls.get(), 3);
    }

    #[test]
    #[ignore = "needs EXE-05"]
    fn limit_zero_pulls_no_input_row() {
        let pulls = std::rc::Rc::new(Cell::new(0));
        let src = VecSource { rows: VecDeque::from(vec![row(1), row(2)]), pulls: pulls.clone() };
        let out = with_executor(|ex| {
            let mut op = limit_op(Box::new(src), Some(int8_const(0)), None);
            drain(&mut op, ex)
        });
        assert!(out.ok().expect("limit 0").is_empty());
        assert_eq!(pulls.get(), 0);
    }

    #[test]
    #[ignore = "needs EXE-05"]
    fn negative_limit_is_2201w() {
        let src = VecSource { rows: VecDeque::from(vec![row(1)]), pulls: std::rc::Rc::new(Cell::new(0)) };
        let err = with_executor(|ex| {
            let mut op = limit_op(Box::new(src), Some(int8_const(-1)), None);
            drain(&mut op, ex)
        });
        assert!(err.is_err());
    }

    #[test]
    #[ignore = "needs EXE-05"]
    fn offset_skips_rows_then_limits() {
        let src = VecSource {
            rows: VecDeque::from(vec![row(1), row(2), row(3), row(4)]),
            pulls: std::rc::Rc::new(Cell::new(0)),
        };
        let out = with_executor(|ex| {
            let mut op = limit_op(Box::new(src), Some(int8_const(2)), Some(int8_const(1)));
            drain(&mut op, ex)
        });
        assert_eq!(out.ok().expect("offset"), vec![row(2), row(3)]);
    }
}
