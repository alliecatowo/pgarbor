//! Nested loop join (lease EXE-16, north-star). PG: `nodeNestloop.c`. Layout: plan.rs `NestedLoopNode`.
//!
//! For each outer row `o`:
//! 1. evaluate `inner_params` over `o` -> a parameter vector (empty for uncorrelated inner sides);
//! 2. restart the inner side: `Executor::with_outer(Rc::new(params), |ex| inner.rescan(ex, &params))`
//!    (when `inner_params` is empty and the inner side is not yet started, a plain first scan; an
//!    unparameterized inner is rescanned with `rescan(&[])` per outer row - it is normally wrapped in a
//!    `Materialize` by the planner, which replays its buffer);
//! 3. every inner `next` call is wrapped in `with_outer(params.clone(), ...)` as well, so `OuterRef`
//!    inside the inner subtree resolves while it iterates lazily (runtime module docs);
//! 4. join semantics with `qual` evaluated over `o ++ inner_row` (`eval_opt_qual`):
//!    * `Inner`: emit `o ++ inner` for every inner row satisfying `qual`.
//!    * `Left`: like Inner, but if NO inner row satisfied `qual` emit `o ++ NULLs` (NULL per inner slot,
//!      width = `inner.width()`).
//!    * `Semi`: emit `o` (outer slots only) at the FIRST satisfying inner row, then stop the inner scan.
//!    * `Anti`: emit `o` if NO inner row satisfied `qual`.
//!    * `Right`/`Full`: not supported by nested loops (the planner swaps or uses hash/merge): XX000
//!      `nested loop does not support {type} joins`.
//! The outer side is pulled lazily one row at a time. NULL padding uses `Datum::Null`.
//!
//! Implementation notes:
//! * The outer side is rescanned with the parameters this node itself receives in `rescan`, under
//!   `with_outer`, which is how a parent exposes its parameters to a child subtree. Outer `next` calls
//!   need no extra frame: the parent's `with_outer` around this node's `next` is already on the stack.
//! * `qual` is evaluated outside the inner parameter frame: it belongs to this join node, not to the
//!   inner subtree.
use std::rc::Rc;

use pgarbor_common::{PgError, Result};
use pgarbor_plan::plan::{JoinType, NestedLoopNode, PlanNode};
use pgarbor_sql::ir::BoundExpr;
use pgarbor_types::Datum;

use crate::build::BuildCtx;
use crate::eval::{eval_exprs, eval_qual};
use crate::runtime::{Executor, Row, RowSource};

/// Build the nested loop operator (module docs). Children are built outer first, then inner, which
/// matches the pre-order numbering of `PlanNode::children()`.
pub fn build_nested_loop(node: &PlanNode, n: &NestedLoopNode, b: &mut BuildCtx) -> Result<Box<dyn RowSource>> {
    let _ = node;
    check_join_type(n.join_type)?;
    let outer = b.child(&n.outer)?;
    let inner = b.child(&n.inner)?;
    Ok(Box::new(NestLoop::new(
        n.join_type,
        outer,
        inner,
        n.qual.clone(),
        n.inner_params.clone(),
        n.inner.width(),
    )))
}

/// Reject the join types a nested loop cannot run (module docs).
fn check_join_type(join_type: JoinType) -> Result<()> {
    match join_type {
        JoinType::Inner | JoinType::Left | JoinType::Semi | JoinType::Anti => Ok(()),
        JoinType::Right | JoinType::Full => Err(unsupported_join(join_type)),
    }
}

/// XX000 `nested loop does not support {type} joins`.
fn unsupported_join(join_type: JoinType) -> PgError {
    let name = match join_type {
        JoinType::Inner => "inner",
        JoinType::Left => "left",
        JoinType::Right => "right",
        JoinType::Full => "full",
        JoinType::Semi => "semi",
        JoinType::Anti => "anti",
    };
    PgError::internal(format!("nested loop does not support {name} joins"))
}

/// `outer ++ inner` as a fresh row.
fn join_rows(outer: &[Datum], inner: &[Datum]) -> Row {
    let mut row = Vec::with_capacity(outer.len() + inner.len());
    row.extend_from_slice(outer);
    row.extend_from_slice(inner);
    row
}

/// `outer ++ NULLs` (`width` NULL slots) as a fresh row.
fn pad_nulls(outer: &[Datum], width: usize) -> Row {
    let mut row = Vec::with_capacity(outer.len() + width);
    row.extend_from_slice(outer);
    row.resize(outer.len() + width, Datum::Null);
    row
}

/// Nested loop state (module docs). Owns copies of the node's expressions and its children.
struct NestLoop {
    join_type: JoinType,
    outer: Box<dyn RowSource>,
    inner: Box<dyn RowSource>,
    /// Join qualification over `outer ++ inner`.
    qual: Option<BoundExpr>,
    /// Expressions over the outer row that produce the inner side's parameters.
    inner_params: Vec<BoundExpr>,
    /// Width of the inner plan node: the NULL padding of an unmatched outer row (Left).
    inner_width: usize,
    /// The outer row being joined (`None` between outer rows).
    outer_row: Option<Row>,
    /// Parameters the inner side currently sees as `OuterRef{0, i}`.
    params: Rc<Vec<Datum>>,
    /// The current outer row has matched at least one inner row.
    matched: bool,
    /// The outer side is exhausted.
    done: bool,
    /// Counter for interrupt polling (`Executor::tick`).
    ticks: u32,
}

impl NestLoop {
    fn new(
        join_type: JoinType,
        outer: Box<dyn RowSource>,
        inner: Box<dyn RowSource>,
        qual: Option<BoundExpr>,
        inner_params: Vec<BoundExpr>,
        inner_width: usize,
    ) -> NestLoop {
        NestLoop {
            join_type,
            outer,
            inner,
            qual,
            inner_params,
            inner_width,
            outer_row: None,
            params: Rc::new(Vec::new()),
            matched: false,
            done: false,
            ticks: 0,
        }
    }

    /// Pull the next outer row, evaluate the inner parameters over it and restart the inner side with
    /// them. Sets `done` when the outer side is exhausted.
    fn start_outer(&mut self, ex: &mut Executor) -> Result<()> {
        let o = match self.outer.next(ex)? {
            Some(o) => o,
            None => {
                self.done = true;
                return Ok(());
            }
        };
        let params: Vec<Datum> = if self.inner_params.is_empty() {
            Vec::new()
        } else {
            eval_exprs(ex, &self.inner_params, &o)?
        };
        let params = Rc::new(params);
        let inner = &mut self.inner;
        let frame = Rc::clone(&params);
        ex.with_outer(Rc::clone(&params), |ex| inner.rescan(ex, frame.as_slice()))?;
        self.params = params;
        self.outer_row = Some(o);
        self.matched = false;
        Ok(())
    }

    /// Next inner row for the current outer row, pulled inside the inner parameter frame.
    fn pull_inner(&mut self, ex: &mut Executor) -> Result<Option<Row>> {
        let inner = &mut self.inner;
        let frame = Rc::clone(&self.params);
        ex.with_outer(frame, |ex| inner.next(ex))
    }

    /// The join loop (module docs). Each call returns the next output row or `None` when the outer
    /// side is exhausted.
    fn next_row(&mut self, ex: &mut Executor) -> Result<Option<Row>> {
        loop {
            if self.done {
                return Ok(None);
            }
            if self.outer_row.is_none() {
                self.start_outer(ex)?;
                continue;
            }
            ex.tick(&mut self.ticks)?;
            let inner_row = self.pull_inner(ex)?;
            let o = match &self.outer_row {
                Some(o) => o,
                None => return Err(PgError::internal("nested loop lost its outer row")),
            };
            match inner_row {
                Some(i) => {
                    let pass = match &self.qual {
                        Some(q) => eval_qual(ex, q, &join_rows(o, &i))?,
                        None => true,
                    };
                    if !pass {
                        continue;
                    }
                    self.matched = true;
                    match self.join_type {
                        JoinType::Inner | JoinType::Left => return Ok(Some(join_rows(o, &i))),
                        JoinType::Semi => {
                            let out = o.clone();
                            self.outer_row = None;
                            return Ok(Some(out));
                        }
                        JoinType::Anti => {
                            // A match disqualifies the outer row: abandon the inner scan.
                            self.outer_row = None;
                        }
                        JoinType::Right | JoinType::Full => return Err(unsupported_join(self.join_type)),
                    }
                }
                None => {
                    let emit = match self.join_type {
                        JoinType::Inner | JoinType::Semi => None,
                        JoinType::Left => {
                            if self.matched {
                                None
                            } else {
                                Some(pad_nulls(o, self.inner_width))
                            }
                        }
                        JoinType::Anti => {
                            if self.matched {
                                None
                            } else {
                                Some(o.clone())
                            }
                        }
                        JoinType::Right | JoinType::Full => return Err(unsupported_join(self.join_type)),
                    };
                    self.outer_row = None;
                    if let Some(r) = emit {
                        return Ok(Some(r));
                    }
                }
            }
        }
    }
}

impl RowSource for NestLoop {
    fn next(&mut self, ex: &mut Executor) -> Result<Option<Row>> {
        self.next_row(ex)
    }

    fn rescan(&mut self, ex: &mut Executor, params: &[Datum]) -> Result<()> {
        let frame = Rc::new(params.to_vec());
        let outer = &mut self.outer;
        let arg = Rc::clone(&frame);
        ex.with_outer(frame, |ex| outer.rescan(ex, arg.as_slice()))?;
        self.outer_row = None;
        self.matched = false;
        self.done = false;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use std::cell::Cell;

    use pgarbor_types::context::IoConfig;

    use super::*;
    use crate::context::{ExecContext, NoGuc, NoServices, SessionEnv};
    use crate::memtxn::MemTxn;
    use crate::runtime::Runtime;

    /// Replays fixed rows; rescan restarts at the first row.
    struct VecSource {
        rows: Vec<Row>,
        pos: usize,
    }

    impl VecSource {
        fn boxed(rows: Vec<Row>) -> Box<dyn RowSource> {
            Box::new(VecSource { rows, pos: 0 })
        }
    }

    impl RowSource for VecSource {
        fn next(&mut self, _ex: &mut Executor) -> Result<Option<Row>> {
            match self.rows.get(self.pos) {
                Some(r) => {
                    self.pos += 1;
                    Ok(Some(r.clone()))
                }
                None => Ok(None),
            }
        }
        fn rescan(&mut self, _ex: &mut Executor, _params: &[Datum]) -> Result<()> {
            self.pos = 0;
            Ok(())
        }
    }

    /// Returns the parameter `OuterRef{0,0}` as its only column.
    struct ParamEcho;

    impl RowSource for ParamEcho {
        fn next(&mut self, ex: &mut Executor) -> Result<Option<Row>> {
            let v = ex.outer_value(0, 0)?;
            Ok(Some(vec![v]))
        }
        fn rescan(&mut self, _ex: &mut Executor, _params: &[Datum]) -> Result<()> {
            Ok(())
        }
    }

    fn run(join: &mut NestLoop) -> Result<Vec<Row>> {
        let mut txn = MemTxn::new();
        let rng = Cell::new(1u64);
        let (_kv, env) = pgarbor_catalog::testing::bootstrapped()?;
        let se = SessionEnv::new(env, IoConfig::default(), &NoGuc, &NoServices, &rng, 0, 0);
        let mut cx = ExecContext::new(&mut txn, se);
        let mut rt = Runtime::default();
        let mut ex = Executor::new(&mut rt, &mut cx);
        let mut out = Vec::new();
        while let Some(r) = join.next(&mut ex)? {
            out.push(r);
        }
        Ok(out)
    }

    fn row(vals: &[i32]) -> Row {
        vals.iter().map(|v| Datum::Int4(*v)).collect()
    }

    fn join(jt: JoinType, outer: Vec<Row>, inner: Vec<Row>, inner_width: usize) -> NestLoop {
        NestLoop::new(jt, VecSource::boxed(outer), VecSource::boxed(inner), None, Vec::new(), inner_width)
    }

    #[test]
    fn inner_join_without_qual_is_cross_product() {
        let mut j = join(JoinType::Inner, vec![row(&[1]), row(&[2])], vec![row(&[10]), row(&[20])], 1);
        let got = run(&mut j).expect("inner join");
        assert_eq!(got, vec![row(&[1, 10]), row(&[1, 20]), row(&[2, 10]), row(&[2, 20])]);
    }

    #[test]
    fn inner_join_with_empty_inner_emits_nothing() {
        let mut j = join(JoinType::Inner, vec![row(&[1]), row(&[2])], vec![], 1);
        assert_eq!(run(&mut j).expect("inner join"), Vec::<Row>::new());
    }

    #[test]
    fn left_join_pads_unmatched_outer_rows() {
        let mut j = join(JoinType::Left, vec![row(&[1]), row(&[2])], vec![], 2);
        let got = run(&mut j).expect("left join");
        let pad = |v: i32| vec![Datum::Int4(v), Datum::Null, Datum::Null];
        assert_eq!(got, vec![pad(1), pad(2)]);
    }

    #[test]
    fn left_join_does_not_pad_matched_outer_rows() {
        let mut j = join(JoinType::Left, vec![row(&[1])], vec![row(&[10])], 1);
        assert_eq!(run(&mut j).expect("left join"), vec![row(&[1, 10])]);
    }

    #[test]
    fn semi_join_emits_each_outer_row_once() {
        let mut j = join(JoinType::Semi, vec![row(&[1]), row(&[2])], vec![row(&[10]), row(&[20])], 1);
        assert_eq!(run(&mut j).expect("semi join"), vec![row(&[1]), row(&[2])]);
    }

    #[test]
    fn semi_join_with_empty_inner_emits_nothing() {
        let mut j = join(JoinType::Semi, vec![row(&[1])], vec![], 1);
        assert_eq!(run(&mut j).expect("semi join"), Vec::<Row>::new());
    }

    #[test]
    fn anti_join_emits_only_unmatched_outer_rows() {
        let mut j = join(JoinType::Anti, vec![row(&[1]), row(&[2])], vec![row(&[10])], 1);
        assert_eq!(run(&mut j).expect("anti join"), Vec::<Row>::new());
        let mut j = join(JoinType::Anti, vec![row(&[1]), row(&[2])], vec![], 1);
        assert_eq!(run(&mut j).expect("anti join"), vec![row(&[1]), row(&[2])]);
    }

    #[test]
    fn inner_side_sees_a_parameter_frame_per_outer_row() {
        // The frame is pushed (empty, since there are no inner_params), so the slot lookup fails with
        // the "index out of range" message rather than "no enclosing parameter frame".
        let mut j = NestLoop::new(
            JoinType::Inner,
            VecSource::boxed(vec![row(&[1])]),
            Box::new(ParamEcho),
            None,
            Vec::new(),
            1,
        );
        let err = run(&mut j).expect_err("empty parameter frame");
        assert!(err.message.contains("OuterRef index 0 out of range"), "{}", err.message);
    }

    #[test]
    fn right_and_full_joins_are_rejected() {
        for jt in [JoinType::Right, JoinType::Full] {
            let err = check_join_type(jt).expect_err("unsupported join");
            assert_eq!(err.sqlstate.code(), "XX000");
        }
        let err = check_join_type(JoinType::Full).expect_err("unsupported join");
        assert_eq!(err.message, "nested loop does not support full joins");
        let err = check_join_type(JoinType::Right).expect_err("unsupported join");
        assert_eq!(err.message, "nested loop does not support right joins");
        assert!(check_join_type(JoinType::Anti).is_ok());
    }
}
