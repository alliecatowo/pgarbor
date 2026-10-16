//! Execution runtime: the operator trait, the per-plan [`Runtime`], the transient [`Executor`]
//! handle that operators receive, and [`PlanStream`] (an open, resumable execution usable by
//! portals). Fully implemented, frozen (v1).
//!
//! ## Execution model (volcano, pull based; plan.rs module docs are the data contract)
//! * A plan is turned into a tree of **owned** operators (`Box<dyn RowSource>`) by `build::open_plan`.
//!   Operators own COPIES of everything they need from the `Plan` (cloned expressions, `TableRef`s,
//!   `IndexRef`s): no operator borrows the `Plan`, so a [`PlanStream`] can be stored in a portal and
//!   resumed by a later Execute message with a fresh `&mut ExecContext`.
//! * `RowSource::next(&mut self, ex)` returns the next row of exactly `PlanNode::width()` values or
//!   `None` at the end. After `None` further calls keep returning `None`.
//! * `RowSource::rescan(&mut self, ex, params)` restarts the operator (and recursively its children)
//!   for another pass with a new parameter vector: used for the inner side of nested loops and for
//!   correlated sub-plans. `params` are the values the subtree sees as `OuterRef{levels_up: 0, i}`.
//!   `rescan(&[])` on a parameter-free subtree is a plain restart. Operators with expensive,
//!   parameter-independent state (hash build side, materialized buffer, sorted input) keep it across
//!   a rescan only when [`Executor::epoch`] is unchanged since it was built (see epoch below).
//! * **Operators never hold a KV iterator across calls** (`KvRead::scan` borrows the transaction
//!   view, which every `next` call needs mutably elsewhere). Scans fetch a bounded batch with
//!   `txn::kv_scan_vec`, remember the last key and re-seek from `Bound::Excluded(last_key)` for the
//!   next batch (see `rowstore`).
//! * **Parameters (`OuterRef`)**: a rescanning parent calls child operators inside
//!   [`Executor::with_outer`]`(params, |ex| child.next(ex))`; `ExprKind::OuterRef{levels_up, index}`
//!   reads `rt.outer[len - 1 - levels_up][index]`. `with_outer` increments `epoch`, which tells
//!   buffering operators ("Materialize", hash build, sort) that their input may have changed.
//! * Errors abort the whole statement; operators need no cleanup (all state is owned and dropped).
//!
//! ## Writing signatures
//! Always spell the executor parameter as `ex: &mut Executor` (all three lifetimes elided). Evaluate
//! with `eval::eval(ex, &expr, &row)`; reach the environment with `ex.se()` (catalog env, IoConfig,
//! params), the transaction with `ex.txn()`, the function-handle cache with `ex.rt.funcs`. Borrowing
//! both `ex.rt` and `ex.cx` fields at once is fine (disjoint fields); borrowing `ex` as a whole via a
//! method while a field borrow is alive is not.
//!
//! ## Borrowing pattern for sub-plans and CTEs
//! `Runtime::subplans[i].op` / `Runtime::ctes[i]` hold operator trees that are driven FROM INSIDE
//! expression evaluation (which already has `&mut Executor`). Use the take/put-back pattern:
//! `let mut op = ex.rt.subplans[i].op.take().ok_or(..)?; let r = drive(&mut op, ex); ex.rt.subplans[i].op = Some(op); r?`.
use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;

use pgarbor_common::{Oid, PgError, Result};
use pgarbor_plan::explain_fmt::NodeActual;
use pgarbor_plan::plan::{ColumnDesc, OutCol, PlanKind};
use pgarbor_sql::ir::SubLinkKind;
use pgarbor_types::catalog_data::Volatility;
use pgarbor_types::func::FuncHandle;
use pgarbor_types::Datum;

use crate::context::{ExecContext, SessionEnv};
use crate::txn::ExecTxn;

/// One row of values (`PlanNode::width()` slots).
pub type Row = Vec<Datum>;

/// A physical operator (module docs).
// CONTRACT
pub trait RowSource {
    /// Next row, or `None` when exhausted.
    fn next(&mut self, ex: &mut Executor) -> Result<Option<Row>>;
    /// Restart for a new pass with parameter vector `params` (module docs).
    fn rescan(&mut self, ex: &mut Executor, params: &[Datum]) -> Result<()>;
    /// Rows affected, for `ModifyTable` operators only (command tag counts). `None` otherwise.
    fn affected_rows(&self) -> Option<u64> {
        None
    }
}

/// Cached result of an uncorrelated (`is_initplan`) sub-plan, or of a hashed ANY sub-plan.
#[derive(Clone, Debug)]
pub enum SubPlanCache {
    Exists(bool),
    /// Scalar / Array / Row result.
    Value(Datum),
    /// Rows of an ANY/ALL sub-plan (first column(s)), for NULL-aware three-valued evaluation.
    Rows(Rc<Vec<Row>>),
    /// Hash set of the encoded non-NULL test keys (`encode_key` bytes) plus "set contained a NULL".
    Hashed { keys: Rc<std::collections::HashSet<Vec<u8>>>, has_null: bool },
}

/// Runtime form of one `SubPlanDef`.
pub struct SubPlanRt {
    pub id: u32,
    pub kind: SubLinkKind,
    pub n_params: u32,
    pub is_initplan: bool,
    pub volatility: Volatility,
    /// Output column types of the sub-plan root (ANY/ALL comparisons, array element type).
    pub out: Vec<OutCol>,
    /// The operator tree; `None` only while taken out for a probe (module docs).
    pub op: Option<Box<dyn RowSource>>,
    /// Result cache of an uncorrelated sub-plan.
    pub cache: Option<SubPlanCache>,
}

/// State of a CTE at run time.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CteState {
    /// Not executed yet (lazily executed on first `CteScan`).
    Pending,
    /// Currently executing (a recursive reference sees only the work table).
    Running,
    /// A recursive CTE between iterations: `rows` holds a prefix of the result.
    Streaming,
    /// `rows` is complete.
    Done,
}

/// Operator trees of a CTE body.
pub enum CteBodyRt {
    Query(Box<dyn RowSource>),
    Recursive { non_recursive: Box<dyn RowSource>, recursive: Box<dyn RowSource>, union_all: bool },
    Modify(Box<dyn RowSource>),
    /// Taken out while executing.
    Taken,
}

/// A domain CHECK constraint bound for evaluation (see `eval_cast::eval_coerce_to_domain`).
#[derive(Clone, Debug)]
pub struct DomainCheckRt {
    pub name: String,
    /// Boolean expression over a ONE-slot row `[value]`: the domain value is `InputRef(0)`.
    pub expr: pgarbor_sql::ir::BoundExpr,
}

/// Bound constraints of one domain type, cached per execution in `Runtime::domains`.
#[derive(Clone, Debug, Default)]
pub struct DomainRt {
    pub not_null: bool,
    pub checks: Vec<DomainCheckRt>,
}

/// Runtime form of one `CteDef`.
pub struct CteRt {
    pub id: u32,
    pub name: String,
    pub columns: Vec<OutCol>,
    pub materialize: bool,
    pub body: CteBodyRt,
    pub state: CteState,
    /// The complete buffered result (shared with every `CteScan` reading it).
    pub rows: Rc<Vec<Row>>,
    /// The working table of a recursive CTE: rows produced by the previous iteration, read by
    /// `WorkTableScan { cte_id }`.
    pub worktable: Rc<RefCell<Vec<Row>>>,
    /// Progress of a recursive CTE that is evaluated one iteration at a time (so that a consumer
    /// such as `LIMIT` can stop an infinite recursion).
    pub stream: RecStream,
}

/// Iteration state of a recursive CTE (see `node_cte`).
#[derive(Default)]
pub struct RecStream {
    /// The non-recursive term has been run.
    pub started: bool,
    /// Rows produced by the previous iteration (the next work table).
    pub working: Vec<Row>,
    /// Keys of every row emitted so far (`UNION` de-duplication).
    pub seen: std::collections::HashSet<Vec<u8>>,
}

/// Mutable per-execution state that is not part of any single operator.
#[derive(Default)]
pub struct Runtime {
    /// Parameter frames for `OuterRef` (module docs), innermost last.
    pub outer: Vec<Rc<Vec<Datum>>>,
    /// Incremented by every `with_outer`; buffering operators compare it to decide whether a cached
    /// input is still valid.
    pub epoch: u64,
    /// Resolved function handles by `pg_proc` OID (filled lazily by the evaluator).
    pub funcs: HashMap<Oid, FuncHandle>,
    pub subplans: Vec<SubPlanRt>,
    pub ctes: Vec<CteRt>,
    /// EXPLAIN ANALYZE actuals by pre-order node index (see [`Instrumented`]).
    pub stats: Option<Rc<RefCell<Vec<NodeActual>>>>,
    /// Bound domain constraints by domain OID (filled lazily by `eval_cast`).
    pub domains: HashMap<Oid, Rc<DomainRt>>,
    /// Values of the enclosing simple-form `CASE x WHEN ...` operands (innermost last), read by
    /// `ExprKind::CaseTestValue`.
    pub case_stack: Vec<Datum>,
}

/// The handle operators and the evaluator receive: runtime state plus the execution context.
/// Created transiently by [`PlanStream::fetch`] / `execute::*` (it only holds borrows).
pub struct Executor<'r, 'c, 'a> {
    pub rt: &'r mut Runtime,
    pub cx: &'c mut ExecContext<'a>,
}

impl<'r, 'c, 'a> Executor<'r, 'c, 'a> {
    pub fn new(rt: &'r mut Runtime, cx: &'c mut ExecContext<'a>) -> Executor<'r, 'c, 'a> {
        Executor { rt, cx }
    }

    /// Run `f` with `params` pushed as the innermost `OuterRef` frame (module docs). The frame is
    /// popped even when `f` fails.
    pub fn with_outer<T>(
        &mut self,
        params: Rc<Vec<Datum>>,
        f: impl FnOnce(&mut Executor<'r, 'c, 'a>) -> Result<T>,
    ) -> Result<T> {
        self.rt.outer.push(params);
        self.rt.epoch += 1;
        let r = f(self);
        self.rt.outer.pop();
        r
    }

    /// Value of `OuterRef { levels_up, index }`. Missing frame/slot = XX000.
    pub fn outer_value(&self, levels_up: u32, index: u32) -> Result<Datum> {
        let n = self.rt.outer.len();
        let frame = (levels_up as usize)
            .checked_add(1)
            .and_then(|k| n.checked_sub(k))
            .and_then(|i| self.rt.outer.get(i))
            .ok_or_else(|| PgError::internal("OuterRef with no enclosing parameter frame"))?;
        frame
            .get(index as usize)
            .cloned()
            .ok_or_else(|| PgError::internal(format!("OuterRef index {index} out of range")))
    }

    /// Current epoch (module docs).
    pub fn epoch(&self) -> u64 {
        self.rt.epoch
    }

    /// Poll cancel/timeout.
    pub fn check_interrupts(&self) -> Result<()> {
        self.cx.txn.check_interrupts()
    }

    /// Loop helper: increments `*counter` and polls interrupts every 1024th call.
    pub fn tick(&self, counter: &mut u32) -> Result<()> {
        *counter = counter.wrapping_add(1);
        if *counter % 1024 == 0 {
            self.check_interrupts()?;
        }
        Ok(())
    }

    /// The transaction (mutable).
    pub fn txn(&mut self) -> &mut dyn ExecTxn {
        &mut *self.cx.txn
    }

    /// The session environment (catalog env, GUCs, params, clocks).
    pub fn se(&self) -> &SessionEnv<'a> {
        &self.cx.se
    }
}

/// Wrapper that records EXPLAIN ANALYZE actuals for one node (installed by `build::build_node` when
/// `ExecOptions::collect_stats` is on).
pub struct Instrumented {
    pub inner: Box<dyn RowSource>,
    pub index: usize,
    pub stats: Rc<RefCell<Vec<NodeActual>>>,
    started: bool,
}

impl Instrumented {
    pub fn new(inner: Box<dyn RowSource>, index: usize, stats: Rc<RefCell<Vec<NodeActual>>>) -> Instrumented {
        Instrumented { inner, index, stats, started: false }
    }
}

impl RowSource for Instrumented {
    fn next(&mut self, ex: &mut Executor) -> Result<Option<Row>> {
        let t0 = pgarbor_common::time::Instant::now();
        let r = self.inner.next(ex)?;
        let ms = t0.elapsed().as_secs_f64() * 1000.0;
        let mut s = self.stats.borrow_mut();
        if let Some(a) = s.get_mut(self.index) {
            if !self.started {
                self.started = true;
                a.loops += 1;
                a.startup_ms += ms;
            }
            a.total_ms += ms;
            if r.is_some() {
                a.rows += 1.0;
            }
        }
        Ok(r)
    }
    fn rescan(&mut self, ex: &mut Executor, params: &[Datum]) -> Result<()> {
        self.started = false;
        self.inner.rescan(ex, params)
    }
    fn affected_rows(&self) -> Option<u64> {
        self.inner.affected_rows()
    }
}

/// An open execution of one plan. Owns its operators; resumable (portals).
pub struct PlanStream {
    pub(crate) root: Box<dyn RowSource>,
    pub(crate) rt: Runtime,
    /// Statement kind (command tag).
    pub kind: PlanKind,
    /// Result columns (RowDescription); empty for DML without RETURNING.
    pub desc: Vec<ColumnDesc>,
    pub(crate) done: bool,
    pub(crate) returned: u64,
    pub(crate) affected: Option<u64>,
}

impl PlanStream {
    pub(crate) fn from_parts(root: Box<dyn RowSource>, rt: Runtime, kind: PlanKind, desc: Vec<ColumnDesc>) -> PlanStream {
        PlanStream { root, rt, kind, desc, done: false, returned: 0, affected: None }
    }

    /// Fetch up to `max_rows` rows (`None` = all). Sets `is_done()` when the operator returned
    /// `None`. A DML plan without RETURNING returns no rows but performs all its writes on the
    /// first call. Safe to call again after completion (returns nothing).
    pub fn fetch(&mut self, cx: &mut ExecContext, max_rows: Option<usize>) -> Result<Vec<Row>> {
        let mut out: Vec<Row> = Vec::new();
        if self.done {
            return Ok(out);
        }
        let PlanStream { root, rt, .. } = self;
        let mut ex = Executor::new(rt, cx);
        loop {
            if let Some(m) = max_rows {
                if out.len() >= m {
                    break;
                }
            }
            match root.next(&mut ex)? {
                Some(r) => {
                    out.push(r);
                }
                None => {
                    self.done = true;
                    self.affected = root.affected_rows();
                    break;
                }
            }
        }
        self.returned += out.len() as u64;
        Ok(out)
    }

    /// The operator returned `None`.
    pub fn is_done(&self) -> bool {
        self.done
    }

    /// Rows returned to the caller so far.
    pub fn rows_returned(&self) -> u64 {
        self.returned
    }

    /// Rows inserted/updated/deleted (DML plans), once `is_done()`.
    pub fn rows_affected(&self) -> Option<u64> {
        self.affected
    }

    /// Command tag as PG prints it, valid once `is_done()`: `SELECT n`, `INSERT 0 n`, `UPDATE n`,
    /// `DELETE n`. (`Populate` plans are tagged by `execute`.)
    pub fn command_tag(&self) -> String {
        let n = self.affected.unwrap_or(self.returned);
        match self.kind {
            PlanKind::Select | PlanKind::Populate => format!("SELECT {}", self.returned),
            PlanKind::Insert => format!("INSERT 0 {n}"),
            PlanKind::Update => format!("UPDATE {n}"),
            PlanKind::Delete => format!("DELETE {n}"),
            PlanKind::Merge => format!("MERGE {n}"),
        }
    }

    /// EXPLAIN ANALYZE actuals gathered so far (None unless `collect_stats` was on at open).
    pub fn actuals(&self) -> Option<Vec<NodeActual>> {
        self.rt.stats.as_ref().map(|s| s.borrow().clone())
    }
}
