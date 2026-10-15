//! Simple pipeline operators (lease EXE-13, north-star): `Filter`, `Project`, `Append`,
//! `Materialize`. Layouts: plan.rs.
//!
//! * **Filter**: pulls rows from `input`, passes those for which `eval_qual(predicate, row)` is TRUE.
//!   Output row = input row.
//! * **Project**: output row = `eval_exprs(exprs, input_row)` (width = `exprs.len()`). Expressions
//!   may contain sub-plans and volatile functions: evaluate exactly once per input row, in order.
//! * **Append** (UNION ALL): drains `children` in order; a child exhausted moves to the next.
//!   `rescan(params)` rescans every child and restarts at the first. Children have identical
//!   slot types, so rows are forwarded unchanged.
//! * **Materialize**: buffers the input rows on first use and replays them from memory on later
//!   scans. `rescan(params)`: if `ex.epoch()` equals the epoch recorded when the buffer was filled
//!   (no `with_outer` frame was pushed since), restart the replay position at 0 WITHOUT touching the
//!   child; otherwise discard the buffer, `child.rescan(params)` and refill lazily. The buffer fills
//!   incrementally as rows are pulled (a partially consumed Materialize that is rescanned must not
//!   lose already buffered rows: finish filling from the child on the next pull).
//! All of them call `ex.check_interrupts()` at least every 1024 rows pulled in a loop that can skip
//! rows (Filter).
//!
//! PG reference (read only, not copied): `src/backend/executor/nodeResult.c` (qual handling),
//! `nodeAppend.c` (child switching and rescan), `nodeMaterial.c` (tuplestore replay and rescan).
use pgarbor_common::Result;
use pgarbor_plan::plan::{AppendNode, FilterNode, MaterializeNode, PlanNode, ProjectNode};
use pgarbor_sql::ir::BoundExpr;
use pgarbor_types::Datum;

use crate::build::BuildCtx;
use crate::eval::{eval_exprs, eval_qual};
use crate::runtime::{Executor, Row, RowSource};

/// Filter operator: passes the input rows for which the predicate is TRUE.
struct FilterOp {
    input: Box<dyn RowSource>,
    predicate: BoundExpr,
    /// Rows pulled from `input`, for the interrupt poll (`Executor::tick`).
    ticks: u32,
}

impl RowSource for FilterOp {
    fn next(&mut self, ex: &mut Executor) -> Result<Option<Row>> {
        loop {
            let row = match self.input.next(ex)? {
                Some(r) => r,
                None => return Ok(None),
            };
            ex.tick(&mut self.ticks)?;
            if eval_qual(ex, &self.predicate, &row)? {
                return Ok(Some(row));
            }
        }
    }

    fn rescan(&mut self, ex: &mut Executor, params: &[Datum]) -> Result<()> {
        self.input.rescan(ex, params)
    }
}

/// Project operator: each output row is the list of expressions evaluated over one input row.
struct ProjectOp {
    input: Box<dyn RowSource>,
    exprs: Vec<BoundExpr>,
}

impl RowSource for ProjectOp {
    fn next(&mut self, ex: &mut Executor) -> Result<Option<Row>> {
        match self.input.next(ex)? {
            Some(row) => {
                // Each expression is evaluated exactly once for this input row (eval_exprs).
                let out = eval_exprs(ex, &self.exprs, &row)?;
                Ok(Some(out))
            }
            None => Ok(None),
        }
    }

    fn rescan(&mut self, ex: &mut Executor, params: &[Datum]) -> Result<()> {
        self.input.rescan(ex, params)
    }
}

/// Append operator (UNION ALL): drains the children in order.
struct AppendOp {
    children: Vec<Box<dyn RowSource>>,
    /// Index of the child currently being drained; `children.len()` once everything is exhausted.
    idx: usize,
}

impl RowSource for AppendOp {
    fn next(&mut self, ex: &mut Executor) -> Result<Option<Row>> {
        while self.idx < self.children.len() {
            match self.children[self.idx].next(ex)? {
                Some(row) => return Ok(Some(row)),
                None => self.idx += 1,
            }
        }
        Ok(None)
    }

    fn rescan(&mut self, ex: &mut Executor, params: &[Datum]) -> Result<()> {
        for child in self.children.iter_mut() {
            child.rescan(ex, params)?;
        }
        self.idx = 0;
        Ok(())
    }
}

/// Materialize operator: buffers the child's rows and replays them on rescans whose epoch is
/// unchanged (module docs).
struct MaterializeOp {
    child: Box<dyn RowSource>,
    /// Rows pulled from the child so far (kept across rescans while the epoch is unchanged).
    buf: Vec<Row>,
    /// Replay position: the next buffered row to return (`pos == buf.len()` means "pull from child").
    pos: usize,
    /// The child returned `None`; the buffer is complete.
    child_done: bool,
    /// Epoch recorded when the buffer started to be filled; `None` when the buffer is invalid
    /// (never filled, or discarded by a rescan).
    epoch: Option<u64>,
}

impl RowSource for MaterializeOp {
    fn next(&mut self, ex: &mut Executor) -> Result<Option<Row>> {
        if self.pos < self.buf.len() {
            let row = self.buf[self.pos].clone();
            self.pos += 1;
            return Ok(Some(row));
        }
        if self.child_done {
            return Ok(None);
        }
        if self.epoch.is_none() {
            self.epoch = Some(ex.epoch());
        }
        match self.child.next(ex)? {
            Some(row) => {
                self.buf.push(row.clone());
                self.pos += 1;
                Ok(Some(row))
            }
            None => {
                self.child_done = true;
                Ok(None)
            }
        }
    }

    fn rescan(&mut self, ex: &mut Executor, params: &[Datum]) -> Result<()> {
        if self.epoch == Some(ex.epoch()) {
            // Same parameter frame as when the buffer was filled: replay from memory. The child is
            // not touched; if it was only partially consumed, the next pull continues it.
            self.pos = 0;
            return Ok(());
        }
        self.buf.clear();
        self.pos = 0;
        self.child_done = false;
        self.epoch = None;
        self.child.rescan(ex, params)
    }
}

/// Builds a Filter operator (module docs).
pub fn build_filter(_node: &PlanNode, n: &FilterNode, b: &mut BuildCtx) -> Result<Box<dyn RowSource>> {
    let input = b.child(&n.input)?;
    Ok(Box::new(FilterOp { input, predicate: n.predicate.clone(), ticks: 0 }))
}

/// Builds a Project operator (module docs).
pub fn build_project(_node: &PlanNode, n: &ProjectNode, b: &mut BuildCtx) -> Result<Box<dyn RowSource>> {
    let input = b.child(&n.input)?;
    Ok(Box::new(ProjectOp { input, exprs: n.exprs.clone() }))
}

/// Builds an Append operator (module docs).
pub fn build_append(_node: &PlanNode, n: &AppendNode, b: &mut BuildCtx) -> Result<Box<dyn RowSource>> {
    let mut children: Vec<Box<dyn RowSource>> = Vec::with_capacity(n.children.len());
    for c in &n.children {
        children.push(b.child(c)?);
    }
    Ok(Box::new(AppendOp { children, idx: 0 }))
}

/// Builds a Materialize operator (module docs).
pub fn build_materialize(_node: &PlanNode, n: &MaterializeNode, b: &mut BuildCtx) -> Result<Box<dyn RowSource>> {
    let child = b.child(&n.input)?;
    Ok(Box::new(MaterializeOp { child, buf: Vec::new(), pos: 0, child_done: false, epoch: None }))
}

#[cfg(test)]
mod tests {
    use std::cell::Cell;
    use std::rc::Rc;

    use pgarbor_sql::ir::ExprKind;
    use pgarbor_types::context::IoConfig;
    use pgarbor_types::TypeId;

    use super::*;
    use crate::context::{ExecContext, NoGuc, NoServices, SessionEnv};
    use crate::memtxn::MemTxn;
    use crate::runtime::Runtime;

    /// Test source over a Vec of rows; counts pulls and rescans through shared cells.
    struct VecSource {
        rows: Vec<Row>,
        pos: usize,
        pulls: Rc<Cell<usize>>,
        rescans: Rc<Cell<usize>>,
    }

    impl VecSource {
        fn boxed(rows: Vec<Row>, pulls: &Rc<Cell<usize>>, rescans: &Rc<Cell<usize>>) -> Box<dyn RowSource> {
            Box::new(VecSource { rows, pos: 0, pulls: Rc::clone(pulls), rescans: Rc::clone(rescans) })
        }
    }

    impl RowSource for VecSource {
        fn next(&mut self, _ex: &mut Executor) -> Result<Option<Row>> {
            self.pulls.set(self.pulls.get() + 1);
            if self.pos < self.rows.len() {
                let r = self.rows[self.pos].clone();
                self.pos += 1;
                Ok(Some(r))
            } else {
                Ok(None)
            }
        }

        fn rescan(&mut self, _ex: &mut Executor, _params: &[Datum]) -> Result<()> {
            self.rescans.set(self.rescans.get() + 1);
            self.pos = 0;
            Ok(())
        }
    }

    fn int_row(v: i32) -> Row {
        vec![Datum::Int4(v)]
    }

    /// Runs `f` with a fresh executor over an in-memory transaction and bootstrapped catalog.
    fn with_ex<T>(f: impl FnOnce(&mut Executor) -> Result<T>) -> Result<T> {
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

    fn drain(op: &mut dyn RowSource, ex: &mut Executor) -> Result<Vec<Row>> {
        let mut out = Vec::new();
        while let Some(r) = op.next(ex)? {
            out.push(r);
        }
        Ok(out)
    }

    #[test]
    fn append_drains_children_in_order_and_rescans_all() -> Result<()> {
        with_ex(|ex| {
            let p1 = Rc::new(Cell::new(0usize));
            let r1 = Rc::new(Cell::new(0usize));
            let p2 = Rc::new(Cell::new(0usize));
            let r2 = Rc::new(Cell::new(0usize));
            let mut op = AppendOp {
                children: vec![
                    VecSource::boxed(vec![int_row(1), int_row(2)], &p1, &r1),
                    VecSource::boxed(vec![], &p2, &r2),
                    VecSource::boxed(vec![int_row(3)], &p2, &r2),
                ],
                idx: 0,
            };
            let first = drain(&mut op, ex)?;
            assert_eq!(first, vec![int_row(1), int_row(2), int_row(3)]);
            assert_eq!(op.next(ex)?, None);
            op.rescan(ex, &[])?;
            assert_eq!(r1.get(), 1);
            assert_eq!(r2.get(), 2);
            let second = drain(&mut op, ex)?;
            assert_eq!(second, first);
            Ok(())
        })
    }

    #[test]
    fn append_with_no_children_is_empty() -> Result<()> {
        with_ex(|ex| {
            let mut op = AppendOp { children: Vec::new(), idx: 0 };
            assert_eq!(op.next(ex)?, None);
            op.rescan(ex, &[])?;
            assert_eq!(op.next(ex)?, None);
            Ok(())
        })
    }

    #[test]
    fn materialize_full_drain_then_replay_does_not_touch_child() -> Result<()> {
        with_ex(|ex| {
            let pulls = Rc::new(Cell::new(0usize));
            let rescans = Rc::new(Cell::new(0usize));
            let mut op = MaterializeOp {
                child: VecSource::boxed(vec![int_row(1), int_row(2), int_row(3)], &pulls, &rescans),
                buf: Vec::new(),
                pos: 0,
                child_done: false,
                epoch: None,
            };
            let first = drain(&mut op, ex)?;
            assert_eq!(first, vec![int_row(1), int_row(2), int_row(3)]);
            let pulls_after_fill = pulls.get();
            // Same epoch (no with_outer frame pushed since): replay from memory.
            op.rescan(ex, &[])?;
            let second = drain(&mut op, ex)?;
            assert_eq!(second, first);
            assert_eq!(pulls.get(), pulls_after_fill);
            assert_eq!(rescans.get(), 0);
            Ok(())
        })
    }

    #[test]
    fn materialize_partial_consumption_keeps_buffer_on_rescan() -> Result<()> {
        with_ex(|ex| {
            let pulls = Rc::new(Cell::new(0usize));
            let rescans = Rc::new(Cell::new(0usize));
            let mut op = MaterializeOp {
                child: VecSource::boxed(vec![int_row(1), int_row(2), int_row(3)], &pulls, &rescans),
                buf: Vec::new(),
                pos: 0,
                child_done: false,
                epoch: None,
            };
            assert_eq!(op.next(ex)?, Some(int_row(1)));
            op.rescan(ex, &[])?;
            assert_eq!(rescans.get(), 0);
            let all = drain(&mut op, ex)?;
            assert_eq!(all, vec![int_row(1), int_row(2), int_row(3)]);
            assert_eq!(rescans.get(), 0);
            // Every child row was pulled exactly once, plus the terminating None.
            assert_eq!(pulls.get(), 4);
            Ok(())
        })
    }

    #[test]
    fn materialize_refills_after_epoch_change() -> Result<()> {
        with_ex(|ex| {
            let pulls = Rc::new(Cell::new(0usize));
            let rescans = Rc::new(Cell::new(0usize));
            let mut op = MaterializeOp {
                child: VecSource::boxed(vec![int_row(7)], &pulls, &rescans),
                buf: Vec::new(),
                pos: 0,
                child_done: false,
                epoch: None,
            };
            let first = drain(&mut op, ex)?;
            assert_eq!(first, vec![int_row(7)]);
            // A parameter frame is pushed: the epoch changes, so the child must be rescanned.
            ex.with_outer(Rc::new(vec![Datum::Int4(1)]), |ex| op.rescan(ex, &[Datum::Int4(1)]))?;
            assert_eq!(rescans.get(), 1);
            let second = drain(&mut op, ex)?;
            assert_eq!(second, vec![int_row(7)]);
            Ok(())
        })
    }

    #[test]
    fn materialize_empty_child_replays_nothing_new() -> Result<()> {
        with_ex(|ex| {
            let pulls = Rc::new(Cell::new(0usize));
            let rescans = Rc::new(Cell::new(0usize));
            let mut op = MaterializeOp {
                child: VecSource::boxed(Vec::new(), &pulls, &rescans),
                buf: Vec::new(),
                pos: 0,
                child_done: false,
                epoch: None,
            };
            assert_eq!(op.next(ex)?, None);
            let pulls_after_fill = pulls.get();
            op.rescan(ex, &[])?;
            assert_eq!(op.next(ex)?, None);
            assert_eq!(pulls.get(), pulls_after_fill);
            assert_eq!(rescans.get(), 0);
            Ok(())
        })
    }

    #[test]
    #[ignore = "needs EXE-05"]
    fn filter_keeps_only_true_rows() -> Result<()> {
        with_ex(|ex| {
            let pulls = Rc::new(Cell::new(0usize));
            let rescans = Rc::new(Cell::new(0usize));
            let predicate = BoundExpr { kind: ExprKind::InputRef(0), ty: TypeId::BOOL };
            let rows = vec![vec![Datum::Bool(true)], vec![Datum::Null], vec![Datum::Bool(false)]];
            let mut op = FilterOp { input: VecSource::boxed(rows, &pulls, &rescans), predicate, ticks: 0 };
            let out = drain(&mut op, ex)?;
            assert_eq!(out, vec![vec![Datum::Bool(true)]]);
            Ok(())
        })
    }

    #[test]
    #[ignore = "needs EXE-05"]
    fn project_evaluates_expressions_per_row() -> Result<()> {
        with_ex(|ex| {
            let pulls = Rc::new(Cell::new(0usize));
            let rescans = Rc::new(Cell::new(0usize));
            let exprs = vec![BoundExpr { kind: ExprKind::InputRef(0), ty: TypeId::INT4 }];
            let rows = vec![int_row(4), int_row(5)];
            let mut op = ProjectOp { input: VecSource::boxed(rows, &pulls, &rescans), exprs };
            let out = drain(&mut op, ex)?;
            assert_eq!(out, vec![int_row(4), int_row(5)]);
            Ok(())
        })
    }
}
