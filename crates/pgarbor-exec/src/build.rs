//! Turning a `Plan` into owned operators: [`BuildCtx`], [`build_node`] (the dispatcher) and
//! [`open_plan`]. Fully implemented, frozen (v1).
//!
//! `open_plan(plan, cx)`:
//! 1. creates a `BuildCtx` (instrumentation on iff `cx.se.options.collect_stats`);
//! 2. builds the root operator tree with [`BuildCtx::child`] (pre-order numbering of nodes for EXPLAIN
//!    ANALYZE: parent before children, children in `PlanNode::children()` order; sub-plans are
//!    numbered after the main tree in `id` order, then CTEs in `id` order; `explain_exec` maps the
//!    resulting `PlanStream::actuals()` vector onto the plan in exactly that order);
//! 3. builds a `SubPlanRt` per `Plan::subplans` entry and a `CteRt` per `Plan::ctes` entry;
//! 4. runs the data-modifying CTEs eagerly (`node_cte::run_modify_ctes`, only when the plan has any),
//!    because PG executes them to completion even when unreferenced;
//! 5. returns the resumable [`PlanStream`].
//! Per-node builders live in the `node_*` modules and have the signature
//! `fn(node: &PlanNode, spec: &<NodeStruct>, b: &mut BuildCtx) -> Result<Box<dyn RowSource>>`:
//! they COPY what they need out of the plan node (module `runtime` docs: operators own their data)
//! and build children with `b.child(&spec.input)`.
use std::cell::RefCell;
use std::rc::Rc;

use pgarbor_common::Result;
use pgarbor_plan::explain_fmt::NodeActual;
use pgarbor_plan::plan::{NodeKind, Plan, PlanNode};

use crate::context::ExecContext;
use crate::runtime::{Executor, Instrumented, PlanStream, RowSource, Runtime, SubPlanRt};
use crate::tabledesc::{table_desc, TableDesc};
use crate::{
    modify, node_agg, node_basic, node_cte, node_func, node_hashjoin, node_join, node_lock, node_mergejoin, node_scan,
    node_setops, node_sort, node_window,
};

/// Builder state (module docs).
pub struct BuildCtx<'p, 'c, 'a> {
    pub plan: &'p Plan,
    pub cx: &'c ExecContext<'a>,
    /// EXPLAIN ANALYZE actuals table shared with `Instrumented` wrappers.
    pub stats: Option<Rc<RefCell<Vec<NodeActual>>>>,
    /// Next pre-order node index.
    pub counter: usize,
}

impl<'p, 'c, 'a> BuildCtx<'p, 'c, 'a> {
    pub fn new(plan: &'p Plan, cx: &'c ExecContext<'a>) -> BuildCtx<'p, 'c, 'a> {
        let stats = if cx.se.options.collect_stats { Some(Rc::new(RefCell::new(Vec::new()))) } else { None };
        BuildCtx { plan, cx, stats, counter: 0 }
    }

    /// Cached table description (`tabledesc::table_desc`).
    pub fn table_desc(&self, oid: pgarbor_common::Oid) -> Result<Rc<TableDesc>> {
        table_desc(self.cx, oid)
    }

    /// Build the operator for `node`, numbering it and wrapping it in `Instrumented` when
    /// collecting statistics.
    pub fn child(&mut self, node: &PlanNode) -> Result<Box<dyn RowSource>> {
        let idx = self.counter;
        self.counter += 1;
        if let Some(s) = &self.stats {
            s.borrow_mut().push(NodeActual::default());
        }
        let op = build_node(self, node)?;
        Ok(match &self.stats {
            Some(s) => Box::new(Instrumented::new(op, idx, Rc::clone(s))),
            None => op,
        })
    }
}

/// Dispatch on the node kind (module docs).
pub fn build_node(b: &mut BuildCtx, node: &PlanNode) -> Result<Box<dyn RowSource>> {
    match &node.kind {
        NodeKind::Result(n) => node_scan::build_result(node, n, b),
        NodeKind::SeqScan(n) => node_scan::build_seq_scan(node, n, b),
        NodeKind::IndexScan(n) => node_scan::build_index_scan(node, n, b),
        NodeKind::VirtualScan(n) => node_scan::build_virtual_scan(node, n, b),
        NodeKind::ValuesScan(n) => node_scan::build_values_scan(node, n, b),
        NodeKind::FunctionScan(n) => node_func::build_function_scan(node, n, b),
        NodeKind::CteScan(n) => node_cte::build_cte_scan(node, n, b),
        NodeKind::WorkTableScan(n) => node_cte::build_worktable_scan(node, n, b),
        NodeKind::Filter(n) => node_basic::build_filter(node, n, b),
        NodeKind::Project(n) => node_basic::build_project(node, n, b),
        NodeKind::ProjectSet(n) => node_func::build_project_set(node, n, b),
        NodeKind::NestedLoop(n) => node_join::build_nested_loop(node, n, b),
        NodeKind::HashJoin(n) => node_hashjoin::build_hash_join(node, n, b),
        NodeKind::MergeJoin(n) => node_mergejoin::build_merge_join(node, n, b),
        NodeKind::Aggregate(n) => node_agg::build_aggregate(node, n, b),
        NodeKind::WindowAgg(n) => node_window::build_window_agg(node, n, b),
        NodeKind::Sort(n) => node_sort::build_sort(node, n, b),
        NodeKind::Limit(n) => node_sort::build_limit(node, n, b),
        NodeKind::Distinct(n) => node_setops::build_distinct(node, n, b),
        NodeKind::SetOp(n) => node_setops::build_setop(node, n, b),
        NodeKind::Append(n) => node_basic::build_append(node, n, b),
        NodeKind::Materialize(n) => node_basic::build_materialize(node, n, b),
        NodeKind::LockRows(n) => node_lock::build_lock_rows(node, n, b),
        NodeKind::ModifyTable(n) => modify::build_modify_table(node, n, b),
    }
}

/// Open `plan` for execution (module docs).
pub fn open_plan(plan: &Plan, cx: &mut ExecContext) -> Result<PlanStream> {
    let (root, mut rt) = {
        let mut b = BuildCtx::new(plan, cx);
        let root = b.child(&plan.root)?;
        let mut rt = Runtime::default();
        for def in &plan.subplans {
            let op = b.child(&def.plan)?;
            rt.subplans.push(SubPlanRt {
                id: def.id,
                kind: def.kind,
                n_params: def.n_params,
                is_initplan: def.is_initplan,
                volatility: def.volatility,
                out: def.plan.out.clone(),
                op: Some(op),
                cache: None,
            });
        }
        for def in &plan.ctes {
            rt.ctes.push(node_cte::build_cte_rt(def, &mut b)?);
        }
        rt.stats = b.stats.clone();
        (root, rt)
    };
    if plan.ctes.iter().any(|c| matches!(c.body, pgarbor_plan::plan::CteBodyPlan::Modify(_))) {
        let mut ex = Executor::new(&mut rt, cx);
        node_cte::run_modify_ctes(&mut ex)?;
    }
    Ok(PlanStream::from_parts(root, rt, plan.kind, plan.output.clone()))
}
