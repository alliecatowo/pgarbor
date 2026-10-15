//! EXPLAIN support, part 2: output formats (PLN-16). PG reference: `commands/explain.c`
//! (`ExplainPrintPlan`, `ExplainNode`, `ExplainProperty*`, `ExplainOpenGroup`, `ExplainBeginOutput`,
//! `ExplainEndOutput`) and `explain_format.c`.
//!
//! [`render`] turns a FINALIZED `Plan` whose nodes carry `props` (the plan was made with
//! `PlanOptions::for_explain`) into lines. The `costs`, `verbose`, `analyze`, `summary`, `timing`,
//! `buffers`, `settings`, `wal` options come from `ExplainOptions` (only `costs` and `verbose` change
//! output in v1: `ANALYZE` is accepted by the executor, which supplies actual rows/time separately
//! through [`ActualStats`]).
//!
//! ## Text format
//! * Root line has no prefix; child lines are indented by 6 spaces per level and start with `->  `;
//!   node property lines are indented 2 spaces deeper than their node label (`  Filter: (a = 1)` under
//!   the root; `        Filter:` under a `->  ` child at depth 1: the property indent is `indent + 2`
//!   where `indent` is the column of the `->` plus 4 for non-root nodes; see fixtures in `explain.rs`).
//!   Exact rule (PG): a non-root node's label line is `"{pad}->  {label}"` with `pad = " " * (6 * depth - 4)`
//!   (depth 1 -> 2 spaces); its property lines use `pad = " " * (6 * depth)`; the root's properties use 2
//!   spaces.
//! * With `COSTS` on, the label line ends with `  (cost=0.00..2.25 rows=1 width=12)` using
//!   `Estimate` (2-decimal costs, integer rows, width bytes). With `COSTS OFF` nothing is appended.
//! * Sub-plan / CTE blocks as described in `explain.rs`: `"{pad}{Name}"` then the block's plan indented
//!   with `->  `.
//! * Hash joins print the inner child under an extra `Hash` node (`->  Hash` with its own indentation).
//! * Trailing lines (only with `ANALYZE`/`SUMMARY`): `Planning Time: 0.123 ms`, `Execution Time: ...`
//!   are the executor's job; v1 prints none.
//!
//! ## JSON format (`FORMAT JSON`), key order and exact layout (2-space indent, `"key": value`):
//! ```json
//! [
//!   {
//!     "Plan": {
//!       "Node Type": "Seq Scan",
//!       "Parallel Aware": false,
//!       "Async Capable": false,
//!       "Relation Name": "t",
//!       "Alias": "t",
//!       "Disabled": false,
//!       "Filter": "(a = 5)"
//!     }
//!   }
//! ]
//! ```
//! Node object key order: `Node Type`, (join nodes: `Join Type`, `Inner Unique`?), `Parent Relationship`
//! (`Outer`, `Inner`, `Member`, `InitPlan`, `SubPlan`, for non-root), `Subplan Name` (for sub-plans),
//! `Parallel Aware`, `Async Capable`, scan nodes: `Relation Name`, `Schema` (VERBOSE), `Alias`,
//! index scans: `Scan Direction`, `Index Name`; then costs (`Startup Cost`, `Total Cost`, `Plan Rows`,
//! `Plan Width`) when COSTS on, then `Disabled`, then the properties (`Index Cond`, `Filter`, `Hash Cond`,
//! `Sort Key` as arrays of strings, `Group Key` array...), then `Plans` (array of child nodes). Booleans
//! `true`/`false`, strings quoted and JSON-escaped. The top-level is always an array with one object
//! (`"Plan"`), plus `"Planning Time"` only with SUMMARY.
//! ## YAML (`FORMAT YAML`): same structure, block style (`- Plan: ` ... as in the oracle sample below).
//! ## XML (`FORMAT XML`): `<explain xmlns="http://www.postgresql.org/2009/explain">` with
//! `<Query><Plan><Node-Type>Seq Scan</Node-Type>...`; keys with spaces become dashes. v1 may report
//! 0A000 for XML (`EXPLAIN FORMAT XML`).
//! Oracle YAML sample:
//! ```text
//! - Plan: 
//!     Node Type: "Seq Scan"
//!     Parallel Aware: false
//!     Async Capable: false
//!     Relation Name: "t"
//!     Alias: "t"
//!     Disabled: false
//!     Filter: "(a = 5)"
//! ```
//! (note the trailing space after `Plan:`).
use pgarbor_common::{PgError, Result, SqlState};
use pgarbor_sql::ir::{BoundExpr, ExplainFormat, ExplainOptions, ExprKind, FrameBound};

use crate::explain::node_label;
use crate::plan::{
    AggStrategy, CteBodyPlan, DistinctStrategy, Estimate, JoinType, KeyEq, ModifyOp, NodeKind, Plan, PlanNode,
    Prop, PropValue, ScanDirection, SetOpCmd, TableRef,
};

/// Actual execution statistics for `EXPLAIN ANALYZE`, provided by the executor, per plan node in
/// pre-order (root = 0).
// CONTRACT
#[derive(Clone, Debug, Default, PartialEq)]
pub struct ActualStats {
    pub nodes: Vec<NodeActual>,
    pub planning_ms: f64,
    pub execution_ms: f64,
}

/// Actuals of one node.
// CONTRACT
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct NodeActual {
    pub rows: f64,
    pub loops: u64,
    pub startup_ms: f64,
    pub total_ms: f64,
}

/// Render the plan (module docs). Dispatches on `options.format`.
pub fn render(plan: &Plan, options: &ExplainOptions) -> Result<Vec<String>> {
    build_and_emit(plan, options, None)
}

/// Render with ANALYZE actuals (`(actual time=0.010..0.020 rows=5 loops=1)` appended after the cost
/// annotation; `Planning Time:` / `Execution Time:` footer lines).
pub fn render_analyze(plan: &Plan, options: &ExplainOptions, actual: &ActualStats) -> Result<Vec<String>> {
    build_and_emit(plan, options, Some(actual))
}

// ---------------------------------------------------------------------------------------------
// Tree model shared by the text, JSON and YAML writers.
// ---------------------------------------------------------------------------------------------

/// One rendered node (a plan node, the synthetic `Hash` wrapper, or a sub-plan / CTE block root).
struct TNode {
    /// First line of the node without the `->` prefix (`node_label`); empty when labels are not needed.
    label: String,
    /// JSON `Node Type`.
    node_type: String,
    /// Extra JSON keys after `Async Capable` (`Join Type`, `Relation Name`, `Alias`, ...), in order.
    extra: Vec<(&'static str, Val)>,
    /// `""` for the root; `Outer`, `Inner`, `Member` for children; `InitPlan` / `SubPlan` for blocks.
    rel: &'static str,
    /// Block heading (`InitPlan 2`, `SubPlan 1`, `CTE x`) for sub-plan / CTE blocks.
    heading: Option<String>,
    cost: Option<Estimate>,
    actual: Option<NodeActual>,
    props: Vec<Prop>,
    children: Vec<TNode>,
}

/// Structured value for the JSON and YAML writers (keys keep their output order).
#[derive(Clone, Debug)]
enum Val {
    Str(String),
    /// Pre-formatted number or `true`-less literal (digits already rounded as PG does).
    Num(String),
    Bool(bool),
    Arr(Vec<Val>),
    Obj(Vec<(String, Val)>),
}

fn vstr(s: &str) -> Val {
    Val::Str(s.to_string())
}

fn vnum(x: f64, digits: usize) -> Val {
    if x.is_finite() {
        Val::Num(format!("{:.*}", digits, x))
    } else {
        Val::Num("null".to_string())
    }
}

fn build_and_emit(plan: &Plan, options: &ExplainOptions, actual: Option<&ActualStats>) -> Result<Vec<String>> {
    if options.format == ExplainFormat::Xml {
        return Err(PgError::unimplemented("PLN-16", "EXPLAIN FORMAT XML"));
    }
    let mut builder = Builder {
        plan,
        opts: options,
        actual,
        want_labels: options.format == ExplainFormat::Text,
        next_index: 0,
        printed: vec![false; plan.subplans.len()],
    };
    let root = builder.build_root()?;
    match options.format {
        ExplainFormat::Text => {
            let mut lines = Vec::new();
            text_node(&mut lines, &root, None, options);
            if let Some(a) = actual {
                if options.summary {
                    lines.push(format!("Planning Time: {:.3} ms", a.planning_ms));
                }
                lines.push(format!("Execution Time: {:.3} ms", a.execution_ms));
            }
            Ok(lines)
        }
        ExplainFormat::Json => {
            let doc = Val::Arr(vec![Val::Obj(top_entries(&root, options, actual))]);
            let mut s = String::new();
            json_write(&doc, 0, &mut s);
            Ok(s.split('\n').map(str::to_string).collect())
        }
        ExplainFormat::Yaml => {
            let mut lines = Vec::new();
            yaml_obj(&top_entries(&root, options, actual), 2, Some("- "), &mut lines);
            Ok(lines)
        }
        ExplainFormat::Xml => Err(PgError::unimplemented("PLN-16", "EXPLAIN FORMAT XML")),
    }
}

/// Entries of the single top-level object: `Plan`, then the footer keys (ANALYZE only).
fn top_entries(root: &TNode, options: &ExplainOptions, actual: Option<&ActualStats>) -> Vec<(String, Val)> {
    let mut top = vec![("Plan".to_string(), node_val(root, options))];
    if let Some(a) = actual {
        if options.summary {
            top.push(("Planning Time".to_string(), vnum(a.planning_ms, 3)));
        }
        top.push(("Execution Time".to_string(), vnum(a.execution_ms, 3)));
    }
    top
}

// ---------------------------------------------------------------------------------------------
// Building the tree
// ---------------------------------------------------------------------------------------------

struct Builder<'a> {
    plan: &'a Plan,
    opts: &'a ExplainOptions,
    actual: Option<&'a ActualStats>,
    /// Labels come from `node_label` only for the text format.
    want_labels: bool,
    /// Pre-order index of the next node of the MAIN tree (sub-plan and CTE bodies are not counted).
    next_index: usize,
    /// Sub-plans already rendered (each is printed once, under the first node that owns it).
    printed: Vec<bool>,
}

impl<'a> Builder<'a> {
    /// Root node, the CTE blocks, and every sub-plan no node of the tree references (so nothing is lost).
    fn build_root(&mut self) -> Result<TNode> {
        let plan = self.plan;
        let mut root = self.build_node(&plan.root, "", None, true)?;
        let mut root_inits = Vec::new();
        let mut arrows = Vec::new();
        let mut root_subs = Vec::new();
        for child in root.children.drain(..) {
            let rel = child.rel;
            match rel {
                "InitPlan" => root_inits.push(child),
                "SubPlan" => root_subs.push(child),
                _ => arrows.push(child),
            }
        }
        let mut cte_blocks = Vec::new();
        for cte in &plan.ctes {
            let heading = format!("CTE {}", cte.name);
            let block = match &cte.body {
                CteBodyPlan::Query(n) | CteBodyPlan::Modify(n) => {
                    self.build_node(n, "InitPlan", Some(heading), false)?
                }
                CteBodyPlan::Recursive { non_recursive, recursive, .. } => {
                    let first = self.build_node(non_recursive, "Outer", None, false)?;
                    let second = self.build_node(recursive, "Inner", None, false)?;
                    TNode {
                        label: "Recursive Union".to_string(),
                        node_type: "Recursive Union".to_string(),
                        extra: Vec::new(),
                        rel: "InitPlan",
                        heading: Some(heading),
                        cost: first.cost,
                        actual: None,
                        props: Vec::new(),
                        children: vec![first, second],
                    }
                }
            };
            cte_blocks.push(block);
        }
        let mut fb_inits = Vec::new();
        let mut fb_subs = Vec::new();
        for id in 0..plan.subplans.len() {
            if let Some(block) = self.subplan_block(id)? {
                if block.rel == "InitPlan" {
                    fb_inits.push(block);
                } else {
                    fb_subs.push(block);
                }
            }
        }
        let mut children = cte_blocks;
        children.append(&mut root_inits);
        children.append(&mut fb_inits);
        children.append(&mut arrows);
        children.append(&mut root_subs);
        children.append(&mut fb_subs);
        root.children = children;
        Ok(root)
    }

    /// Block for sub-plan `id`, or `None` when it was already printed.
    fn subplan_block(&mut self, id: usize) -> Result<Option<TNode>> {
        let plan = self.plan;
        let body = plan.subplans.get(id).ok_or_else(|| {
            PgError::new(SqlState::INTERNAL_ERROR, format!("EXPLAIN: sub-plan {} is not defined", id))
        })?;
        if self.printed[id] {
            return Ok(None);
        }
        self.printed[id] = true;
        let block = if body.is_initplan {
            self.build_node(&body.plan, "InitPlan", Some(format!("InitPlan {}", id + 1)), false)?
        } else {
            self.build_node(&body.plan, "SubPlan", Some(format!("SubPlan {}", id + 1)), false)?
        };
        Ok(Some(block))
    }

    /// Sub-plan blocks owned by `node` (referenced by its own expressions): initplans, correlated subplans.
    fn owned_blocks(&mut self, node: &PlanNode) -> Result<(Vec<TNode>, Vec<TNode>)> {
        let mut inits = Vec::new();
        let mut subs = Vec::new();
        for id in owned_subplan_ids(node) {
            if let Some(block) = self.subplan_block(id as usize)? {
                if block.rel == "InitPlan" {
                    inits.push(block);
                } else {
                    subs.push(block);
                }
            }
        }
        Ok((inits, subs))
    }

    fn build_node(&mut self, node: &PlanNode, rel: &'static str, heading: Option<String>, track: bool) -> Result<TNode> {
        let index = if track {
            let i = self.next_index;
            self.next_index += 1;
            Some(i)
        } else {
            None
        };
        let (init_blocks, sub_blocks) = self.owned_blocks(node)?;
        if let Some(input) = hidden_input(&node.kind) {
            // Project / Filter are not shown: their child is rendered in their place and their
            // sub-plan blocks move onto the child.
            let mut inner = self.build_node(input, rel, heading, track)?;
            let mut children = init_blocks;
            children.append(&mut inner.children);
            children.extend(sub_blocks);
            inner.children = children;
            return Ok(inner);
        }
        let (node_type, extra) = self.describe(node)?;
        let label = if self.want_labels { node_label(node, self.opts.verbose)? } else { String::new() };
        let actual = match (index, self.actual) {
            (Some(i), Some(stats)) => stats.nodes.get(i).copied(),
            _ => None,
        };
        let is_append = matches!(node.kind, NodeKind::Append(_));
        let is_binary = matches!(
            node.kind,
            NodeKind::NestedLoop(_) | NodeKind::HashJoin(_) | NodeKind::MergeJoin(_) | NodeKind::SetOp(_)
        );
        let is_hash = matches!(node.kind, NodeKind::HashJoin(_));
        let mut children = init_blocks;
        for (i, kid) in node.children().into_iter().enumerate() {
            if is_hash && i == 1 {
                // PG shows the inner side of a hash join under a `Hash` node.
                let inner = self.build_node(kid, "Outer", None, track)?;
                let cost = inner.cost;
                children.push(TNode {
                    label: "Hash".to_string(),
                    node_type: "Hash".to_string(),
                    extra: Vec::new(),
                    rel: "Inner",
                    heading: None,
                    cost,
                    actual: None,
                    props: Vec::new(),
                    children: vec![inner],
                });
            } else {
                let child_rel: &'static str = if is_append {
                    "Member"
                } else if is_binary && i > 0 {
                    "Inner"
                } else {
                    "Outer"
                };
                children.push(self.build_node(kid, child_rel, None, track)?);
            }
        }
        children.extend(sub_blocks);
        Ok(TNode {
            label,
            node_type,
            extra,
            rel,
            heading,
            cost: Some(node.est),
            actual,
            props: node.props.clone(),
            children,
        })
    }

    fn cte_name(&self, id: u32) -> Result<String> {
        self.plan
            .ctes
            .iter()
            .find(|c| c.id == id)
            .map(|c| c.name.clone())
            .ok_or_else(|| PgError::new(SqlState::INTERNAL_ERROR, format!("EXPLAIN: CTE {} is not defined", id)))
    }

    /// JSON `Node Type` and the extra keys of a shown node.
    fn describe(&self, node: &PlanNode) -> Result<(String, Vec<(&'static str, Val)>)> {
        let verbose = self.opts.verbose;
        let (name, extra): (&str, Vec<(&'static str, Val)>) = match &node.kind {
            NodeKind::Result(_) => ("Result", Vec::new()),
            NodeKind::SeqScan(s) => ("Seq Scan", table_extra(&s.table, verbose)),
            NodeKind::IndexScan(s) => {
                let dir = match s.direction {
                    ScanDirection::Forward => "Forward",
                    ScanDirection::Backward => "Backward",
                };
                let mut x = vec![("Scan Direction", vstr(dir)), ("Index Name", vstr(&s.index.name))];
                x.extend(table_extra(&s.table, verbose));
                ("Index Scan", x)
            }
            NodeKind::VirtualScan(s) => ("Seq Scan", table_extra(&s.table, verbose)),
            NodeKind::ValuesScan(_) => ("Values Scan", vec![("Alias", vstr("*VALUES*"))]),
            NodeKind::FunctionScan(s) => {
                let single = match s.funcs.as_slice() {
                    [only] => match &only.expr.kind {
                        ExprKind::FuncCall(f) => Some(f.name.clone()),
                        _ => None,
                    },
                    _ => None,
                };
                let x = match single {
                    Some(name) => vec![("Function Name", vstr(&name))],
                    None => Vec::new(),
                };
                ("Function Scan", x)
            }
            NodeKind::CteScan(s) => ("CTE Scan", vec![("CTE Name", vstr(&self.cte_name(s.cte_id)?))]),
            NodeKind::WorkTableScan(s) => ("WorkTable Scan", vec![("CTE Name", vstr(&self.cte_name(s.cte_id)?))]),
            NodeKind::ProjectSet(_) => ("ProjectSet", Vec::new()),
            NodeKind::NestedLoop(n) => ("Nested Loop", vec![("Join Type", vstr(join_name(n.join_type)))]),
            NodeKind::HashJoin(n) => ("Hash Join", vec![("Join Type", vstr(join_name(n.join_type)))]),
            NodeKind::MergeJoin(n) => ("Merge Join", vec![("Join Type", vstr(join_name(n.join_type)))]),
            NodeKind::Aggregate(a) => {
                let strategy = match &a.strategy {
                    AggStrategy::Plain => "Plain",
                    AggStrategy::Hashed => "Hashed",
                    AggStrategy::Sorted => "Sorted",
                };
                ("Aggregate", vec![("Strategy", vstr(strategy)), ("Partial Mode", vstr("Simple"))])
            }
            NodeKind::WindowAgg(_) => ("WindowAgg", Vec::new()),
            NodeKind::Sort(_) => ("Sort", Vec::new()),
            NodeKind::Limit(_) => ("Limit", Vec::new()),
            NodeKind::Distinct(d) => match &d.strategy {
                DistinctStrategy::Hashed => {
                    ("Aggregate", vec![("Strategy", vstr("Hashed")), ("Partial Mode", vstr("Simple"))])
                }
                DistinctStrategy::Sorted => ("Unique", Vec::new()),
            },
            NodeKind::SetOp(s) => {
                let command = match &s.cmd {
                    SetOpCmd::Intersect => "Intersect",
                    SetOpCmd::IntersectAll => "Intersect All",
                    SetOpCmd::Except => "Except",
                    SetOpCmd::ExceptAll => "Except All",
                };
                ("SetOp", vec![("Strategy", vstr("Hashed")), ("Command", vstr(command))])
            }
            NodeKind::Append(_) => ("Append", Vec::new()),
            NodeKind::Materialize(_) => ("Materialize", Vec::new()),
            NodeKind::LockRows(_) => ("LockRows", Vec::new()),
            NodeKind::ModifyTable(m) => {
                let operation = match &m.op {
                    ModifyOp::Insert => "Insert",
                    ModifyOp::Update => "Update",
                    ModifyOp::Delete => "Delete",
                };
                let mut x = vec![("Operation", vstr(operation))];
                x.extend(table_extra(&m.target.table, verbose));
                ("ModifyTable", x)
            }
            NodeKind::Filter(_) | NodeKind::Project(_) => {
                return Err(PgError::new(
                    SqlState::INTERNAL_ERROR,
                    "EXPLAIN: a hidden plan node reached the renderer",
                ))
            }
        };
        Ok((name.to_string(), extra))
    }
}

fn join_name(join: JoinType) -> &'static str {
    match join {
        JoinType::Inner => "Inner",
        JoinType::Left => "Left",
        JoinType::Right => "Right",
        JoinType::Full => "Full",
        JoinType::Semi => "Semi",
        JoinType::Anti => "Anti",
    }
}

fn table_extra(table: &TableRef, verbose: bool) -> Vec<(&'static str, Val)> {
    let mut x = vec![("Relation Name", vstr(&table.name))];
    if verbose {
        x.push(("Schema", vstr(&table.schema)));
    }
    x.push(("Alias", vstr(&table.alias)));
    x
}

/// The child that replaces a hidden (`Project` / `Filter`) node.
fn hidden_input(kind: &NodeKind) -> Option<&PlanNode> {
    match kind {
        NodeKind::Project(p) => Some(&p.input),
        NodeKind::Filter(f) => Some(&f.input),
        _ => None,
    }
}

/// Every expression stored directly in a node (not in its children, not inside sub-plan bodies).
fn node_exprs(kind: &NodeKind) -> Vec<&BoundExpr> {
    let mut v: Vec<&BoundExpr> = Vec::new();
    match kind {
        NodeKind::Result(n) => {
            v.extend(n.exprs.iter());
            v.extend(n.one_time_filter.iter());
        }
        NodeKind::SeqScan(n) => v.extend(n.filter.iter()),
        NodeKind::IndexScan(n) => {
            for k in &n.bounds.eq {
                match k {
                    KeyEq::Value(e) | KeyEq::AnyOf(e) => v.push(e),
                }
            }
            if let Some(r) = &n.bounds.lower {
                v.push(&r.value);
            }
            if let Some(r) = &n.bounds.upper {
                v.push(&r.value);
            }
            v.extend(n.index_cond.iter());
            v.extend(n.filter.iter());
        }
        NodeKind::VirtualScan(n) => {
            for (_, e) in n.eq_filters.iter() {
                v.push(e);
            }
            v.extend(n.filter.iter());
        }
        NodeKind::ValuesScan(n) => {
            for row in &n.rows {
                v.extend(row.iter());
            }
            v.extend(n.filter.iter());
        }
        NodeKind::FunctionScan(n) => {
            for f in &n.funcs {
                v.push(&f.expr);
            }
            v.extend(n.filter.iter());
        }
        NodeKind::CteScan(n) => v.extend(n.filter.iter()),
        NodeKind::WorkTableScan(n) => v.extend(n.filter.iter()),
        NodeKind::Filter(n) => v.push(&n.predicate),
        NodeKind::Project(n) => v.extend(n.exprs.iter()),
        NodeKind::ProjectSet(n) => v.extend(n.exprs.iter()),
        NodeKind::NestedLoop(n) => {
            v.extend(n.inner_params.iter());
            v.extend(n.qual.iter());
        }
        NodeKind::HashJoin(n) => {
            v.extend(n.outer_keys.iter());
            v.extend(n.inner_keys.iter());
            v.extend(n.qual.iter());
        }
        NodeKind::MergeJoin(n) => {
            for k in n.left_keys.iter().chain(n.right_keys.iter()) {
                v.push(&k.expr);
            }
            v.extend(n.qual.iter());
        }
        NodeKind::Aggregate(n) => {
            v.extend(n.group_keys.iter());
            for a in &n.aggs {
                v.extend(a.args.iter());
                v.extend(a.direct_args.iter());
                v.extend(a.filter.iter());
                for s in &a.order_by {
                    v.push(&s.expr);
                }
            }
            v.extend(n.having.iter());
        }
        NodeKind::WindowAgg(n) => {
            v.extend(n.partition_by.iter());
            for s in &n.order_by {
                v.push(&s.expr);
            }
            for b in [&n.frame.start, &n.frame.end] {
                match b {
                    FrameBound::Preceding(e) | FrameBound::Following(e) => v.push(e),
                    FrameBound::UnboundedPreceding | FrameBound::CurrentRow | FrameBound::UnboundedFollowing => {}
                }
            }
            for f in &n.funcs {
                v.extend(f.args.iter());
                v.extend(f.filter.iter());
            }
        }
        NodeKind::Sort(n) => {
            for s in &n.keys {
                v.push(&s.expr);
            }
        }
        NodeKind::Limit(n) => {
            v.extend(n.count.iter());
            v.extend(n.offset.iter());
            for s in &n.ties_keys {
                v.push(&s.expr);
            }
        }
        NodeKind::ModifyTable(n) => {
            if let Some(ins) = &n.insert {
                for (_, e) in ins.defaults.iter().chain(ins.generated.iter()) {
                    v.push(e);
                }
            }
            if let Some(upd) = &n.update {
                for (_, e) in upd.assignments.iter().chain(upd.generated.iter()) {
                    v.push(e);
                }
            }
            v.extend(n.returning.iter());
            for (_, _, e) in &n.target.checks {
                v.push(e);
            }
        }
        NodeKind::Distinct(_)
        | NodeKind::SetOp(_)
        | NodeKind::Append(_)
        | NodeKind::Materialize(_)
        | NodeKind::LockRows(_) => {}
    }
    v
}

/// Sub-plan ids referenced by the node's own expressions (sorted, unique).
fn owned_subplan_ids(node: &PlanNode) -> Vec<u32> {
    let mut ids: Vec<u32> = Vec::new();
    for e in node_exprs(&node.kind) {
        e.walk(&mut |x: &BoundExpr| {
            if let ExprKind::SubPlan(sp) = &x.kind {
                ids.push(sp.plan_id);
            }
            true
        });
    }
    ids.sort_unstable();
    ids.dedup();
    ids
}

// ---------------------------------------------------------------------------------------------
// Text format
// ---------------------------------------------------------------------------------------------

/// Print a node: `arrow` is the column of its `->` (`None` for the root). Properties sit two columns
/// right of the label text (root: column 2; child: arrow column + 6). Each heading block (`InitPlan 2`,
/// `SubPlan 1`, `CTE x`) is printed at the owner's property column, its plan one step further right.
fn text_node(out: &mut Vec<String>, t: &TNode, arrow: Option<usize>, opts: &ExplainOptions) {
    let (prefix, props_col) = match arrow {
        None => (String::new(), 2usize),
        Some(col) => (format!("{}->  ", " ".repeat(col)), col + 6),
    };
    let mut line = format!("{}{}", prefix, t.label);
    if opts.costs {
        if let Some(e) = t.cost {
            line.push_str(&format!(
                "  (cost={:.2}..{:.2} rows={:.0} width={})",
                e.startup_cost, e.total_cost, e.rows, e.width
            ));
        }
    }
    if let Some(a) = t.actual {
        if a.loops == 0 {
            line.push_str(" (never executed)");
        } else if opts.timing {
            line.push_str(&format!(
                " (actual time={:.3}..{:.3} rows={:.0} loops={})",
                a.startup_ms, a.total_ms, a.rows, a.loops
            ));
        } else {
            line.push_str(&format!(" (actual rows={:.0} loops={})", a.rows, a.loops));
        }
    }
    out.push(line);
    let pad = " ".repeat(props_col);
    for p in &t.props {
        out.push(format!("{}{}: {}", pad, p.key, prop_text(&p.value)));
    }
    for c in &t.children {
        match &c.heading {
            Some(h) => {
                out.push(format!("{}{}", pad, h));
                text_node(out, c, Some(props_col + 2), opts);
            }
            None => text_node(out, c, Some(props_col), opts),
        }
    }
}

fn prop_text(v: &PropValue) -> String {
    match v {
        PropValue::Text(s) => s.clone(),
        PropValue::List(items) => items.join(", "),
        PropValue::Int(i) => i.to_string(),
        PropValue::Bool(b) => b.to_string(),
    }
}

fn prop_val(v: &PropValue) -> Val {
    match v {
        PropValue::Text(s) => Val::Str(s.clone()),
        PropValue::List(items) => Val::Arr(items.iter().map(|s| Val::Str(s.clone())).collect()),
        PropValue::Int(i) => Val::Num(i.to_string()),
        PropValue::Bool(b) => Val::Bool(*b),
    }
}

// ---------------------------------------------------------------------------------------------
// JSON format
// ---------------------------------------------------------------------------------------------

/// The node object: `Node Type`, `Parent Relationship`, `Subplan Name`, `Parallel Aware`, `Async Capable`,
/// the extra keys, cost keys (COSTS), actual keys (ANALYZE), `Disabled`, properties, `Plans`.
fn node_val(t: &TNode, opts: &ExplainOptions) -> Val {
    let mut o: Vec<(String, Val)> = Vec::new();
    o.push(("Node Type".to_string(), vstr(&t.node_type)));
    if !t.rel.is_empty() {
        o.push(("Parent Relationship".to_string(), vstr(t.rel)));
    }
    if let Some(h) = &t.heading {
        o.push(("Subplan Name".to_string(), vstr(h)));
    }
    o.push(("Parallel Aware".to_string(), Val::Bool(false)));
    o.push(("Async Capable".to_string(), Val::Bool(false)));
    for (k, v) in &t.extra {
        o.push(((*k).to_string(), v.clone()));
    }
    if opts.costs {
        if let Some(e) = t.cost {
            o.push(("Startup Cost".to_string(), vnum(e.startup_cost, 2)));
            o.push(("Total Cost".to_string(), vnum(e.total_cost, 2)));
            o.push(("Plan Rows".to_string(), vnum(e.rows, 0)));
            o.push(("Plan Width".to_string(), Val::Num(e.width.to_string())));
        }
    }
    if let Some(a) = t.actual {
        if opts.timing {
            o.push(("Actual Startup Time".to_string(), vnum(a.startup_ms, 3)));
            o.push(("Actual Total Time".to_string(), vnum(a.total_ms, 3)));
        }
        o.push(("Actual Rows".to_string(), vnum(a.rows, 0)));
        o.push(("Actual Loops".to_string(), Val::Num(a.loops.to_string())));
    }
    o.push(("Disabled".to_string(), Val::Bool(false)));
    for p in &t.props {
        o.push((p.key.clone(), prop_val(&p.value)));
    }
    if !t.children.is_empty() {
        o.push(("Plans".to_string(), Val::Arr(t.children.iter().map(|c| node_val(c, opts)).collect())));
    }
    Val::Obj(o)
}

fn json_quote(s: &str) -> String {
    let mut o = String::with_capacity(s.len() + 2);
    o.push('"');
    for c in s.chars() {
        match c {
            '"' => o.push_str("\\\""),
            '\\' => o.push_str("\\\\"),
            '\n' => o.push_str("\\n"),
            '\r' => o.push_str("\\r"),
            '\t' => o.push_str("\\t"),
            '\u{8}' => o.push_str("\\b"),
            '\u{c}' => o.push_str("\\f"),
            c if (c as u32) < 0x20 => o.push_str(&format!("\\u{:04x}", c as u32)),
            c => o.push(c),
        }
    }
    o.push('"');
    o
}

/// Pretty-print with 2-space indentation, `"key": value`, one array item per line, no trailing commas.
fn json_write(v: &Val, indent: usize, out: &mut String) {
    match v {
        Val::Str(s) => out.push_str(&json_quote(s)),
        Val::Num(n) => out.push_str(n),
        Val::Bool(b) => out.push_str(if *b { "true" } else { "false" }),
        Val::Arr(items) => {
            if items.is_empty() {
                out.push_str("[]");
                return;
            }
            out.push_str("[\n");
            let last = items.len() - 1;
            for (i, item) in items.iter().enumerate() {
                out.push_str(&" ".repeat(indent + 2));
                json_write(item, indent + 2, out);
                if i != last {
                    out.push(',');
                }
                out.push('\n');
            }
            out.push_str(&" ".repeat(indent));
            out.push(']');
        }
        Val::Obj(entries) => {
            if entries.is_empty() {
                out.push_str("{}");
                return;
            }
            out.push_str("{\n");
            let last = entries.len() - 1;
            for (i, (k, item)) in entries.iter().enumerate() {
                out.push_str(&" ".repeat(indent + 2));
                out.push_str(&json_quote(k));
                out.push_str(": ");
                json_write(item, indent + 2, out);
                if i != last {
                    out.push(',');
                }
                out.push('\n');
            }
            out.push_str(&" ".repeat(indent));
            out.push('}');
        }
    }
}

// ---------------------------------------------------------------------------------------------
// YAML format
// ---------------------------------------------------------------------------------------------

/// Block-style YAML: a key whose value is a mapping or sequence ends with `: ` (trailing space, as PG
/// writes `- Plan: `); mapping keys sit two columns right of their parent key; sequence items start with
/// `- ` two columns right of their key, and the first key of a mapping item shares that line.
fn yaml_obj(entries: &[(String, Val)], indent: usize, first: Option<&str>, out: &mut Vec<String>) {
    for (i, (k, v)) in entries.iter().enumerate() {
        let lead: String = match (i, first) {
            (0, Some(f)) => f.to_string(),
            _ => " ".repeat(indent),
        };
        match v {
            Val::Obj(o) => {
                out.push(format!("{}{}: ", lead, k));
                yaml_obj(o, indent + 2, None, out);
            }
            Val::Arr(items) if items.is_empty() => out.push(format!("{}{}: []", lead, k)),
            Val::Arr(items) => {
                out.push(format!("{}{}: ", lead, k));
                yaml_items(items, indent + 2, out);
            }
            other => out.push(format!("{}{}: {}", lead, k, yaml_scalar(other))),
        }
    }
}

fn yaml_items(items: &[Val], col: usize, out: &mut Vec<String>) {
    for item in items {
        match item {
            Val::Obj(o) => {
                let first = format!("{}- ", " ".repeat(col));
                yaml_obj(o, col + 2, Some(first.as_str()), out);
            }
            other => out.push(format!("{}- {}", " ".repeat(col), yaml_scalar(other))),
        }
    }
}

fn yaml_scalar(v: &Val) -> String {
    match v {
        Val::Str(s) => json_quote(s),
        Val::Num(n) => n.clone(),
        Val::Bool(b) => b.to_string(),
        Val::Arr(_) | Val::Obj(_) => String::new(),
    }
}

// ---------------------------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::plan::{PlanKind, SeqScan};
    use pgarbor_catalog::invalidation::PlanDeps;
    use pgarbor_catalog::persist::CatalogVersion;

    fn opts(costs: bool) -> ExplainOptions {
        ExplainOptions { costs, ..ExplainOptions::default() }
    }

    /// Node in the tree model (no labels from PLN-15 needed).
    fn tn(label: &str, props: &[(&str, &str)], children: Vec<TNode>) -> TNode {
        TNode {
            label: label.to_string(),
            node_type: label.to_string(),
            extra: Vec::new(),
            rel: "Outer",
            heading: None,
            cost: None,
            actual: None,
            props: props
                .iter()
                .map(|(k, v)| Prop { key: k.to_string(), value: PropValue::Text(v.to_string()) })
                .collect(),
            children,
        }
    }

    fn block(mut t: TNode, rel: &'static str, heading: &str) -> TNode {
        t.rel = rel;
        t.heading = Some(heading.to_string());
        t
    }

    fn text_of(root: &TNode, o: &ExplainOptions) -> String {
        let mut out = Vec::new();
        text_node(&mut out, root, None, o);
        out.join("\n")
    }

    fn seq_scan_plan() -> Plan {
        let table = TableRef {
            oid: 0,
            schema: "public".to_string(),
            name: "t".to_string(),
            alias: "t".to_string(),
            ncols: 0,
            col_types: Vec::new(),
        };
        let mut root = PlanNode::new(
            NodeKind::SeqScan(SeqScan { table, filter: None, with_rowid: false, needed: Vec::new() }),
            Vec::new(),
        );
        root.props.push(Prop { key: "Filter".to_string(), value: PropValue::Text("(a = 5)".to_string()) });
        Plan {
            kind: PlanKind::Select,
            root,
            subplans: Vec::new(),
            ctes: Vec::new(),
            output: Vec::new(),
            param_types: Vec::new(),
            deps: PlanDeps::default(),
            catalog_version: CatalogVersion::default(),
        }
    }

    #[test]
    fn text_hash_join_fixture() {
        let root = tn(
            "Hash Join",
            &[("Hash Cond", "(u.a = t.a)")],
            vec![
                tn("Seq Scan on u", &[], vec![]),
                tn("Hash", &[], vec![tn("Seq Scan on t", &[], vec![])]),
            ],
        );
        let expected = [
            "Hash Join",
            "  Hash Cond: (u.a = t.a)",
            "  ->  Seq Scan on u",
            "  ->  Hash",
            "        ->  Seq Scan on t",
        ]
        .join("\n");
        assert_eq!(text_of(&root, &opts(false)), expected);
    }

    #[test]
    fn text_update_fixture_property_indent() {
        let root = tn(
            "Update on t",
            &[],
            vec![tn(
                "Nested Loop",
                &[],
                vec![
                    tn("Seq Scan on u", &[("Filter", "(d = 1)")], vec![]),
                    tn("Index Scan using t_pkey on t", &[("Index Cond", "(a = u.a)")], vec![]),
                ],
            )],
        );
        let expected = [
            "Update on t",
            "  ->  Nested Loop",
            "        ->  Seq Scan on u",
            "              Filter: (d = 1)",
            "        ->  Index Scan using t_pkey on t",
            "              Index Cond: (a = u.a)",
        ]
        .join("\n");
        assert_eq!(text_of(&root, &opts(false)), expected);
    }

    #[test]
    fn text_cost_annotation() {
        let mut root = tn("Seq Scan on t", &[], vec![]);
        root.cost = Some(Estimate { startup_cost: 0.0, total_cost: 2.25, rows: 1.0, width: 12 });
        assert_eq!(text_of(&root, &opts(true)), "Seq Scan on t  (cost=0.00..2.25 rows=1 width=12)");
        assert_eq!(text_of(&root, &opts(false)), "Seq Scan on t");
    }

    #[test]
    fn text_initplan_fixture() {
        let limit = tn(
            "Limit",
            &[],
            vec![tn("Index Only Scan Backward using u_a_idx on u", &[("Index Cond", "(a IS NOT NULL)")], vec![])],
        );
        let mut result = tn("Result", &[], vec![]);
        result.children.push(block(limit, "InitPlan", "InitPlan 1"));
        let mut root = tn("Seq Scan on t", &[("Filter", "(a = (InitPlan 2).col1)")], vec![]);
        root.children.push(block(result, "InitPlan", "InitPlan 2"));
        let expected = [
            "Seq Scan on t",
            "  Filter: (a = (InitPlan 2).col1)",
            "  InitPlan 2",
            "    ->  Result",
            "          InitPlan 1",
            "            ->  Limit",
            "                  ->  Index Only Scan Backward using u_a_idx on u",
            "                        Index Cond: (a IS NOT NULL)",
        ]
        .join("\n");
        assert_eq!(text_of(&root, &opts(false)), expected);
    }

    #[test]
    fn text_subplan_fixture() {
        let mut root = tn("Seq Scan on t", &[], vec![]);
        let agg = tn("Aggregate", &[], vec![tn("Bitmap Heap Scan on u", &[], vec![])]);
        root.children.push(block(agg, "SubPlan", "SubPlan 1"));
        let expected = [
            "Seq Scan on t",
            "  SubPlan 1",
            "    ->  Aggregate",
            "          ->  Bitmap Heap Scan on u",
        ]
        .join("\n");
        assert_eq!(text_of(&root, &opts(false)), expected);
    }

    #[test]
    fn text_analyze_never_executed_and_actual() {
        let mut root = tn("Seq Scan on t", &[], vec![]);
        root.actual = Some(NodeActual { rows: 5.0, loops: 1, startup_ms: 0.01, total_ms: 0.02 });
        let o = ExplainOptions { timing: true, ..opts(false) };
        assert_eq!(text_of(&root, &o), "Seq Scan on t (actual time=0.010..0.020 rows=5 loops=1)");
        root.actual = Some(NodeActual { rows: 0.0, loops: 0, startup_ms: 0.0, total_ms: 0.0 });
        assert_eq!(text_of(&root, &opts(false)), "Seq Scan on t (never executed)");
    }

    #[test]
    fn json_seq_scan_matches_doc_fixture() {
        let lines = render(&seq_scan_plan(), &opts(false)).expect("render");
        let expected = [
            "[",
            "  {",
            "    \"Plan\": {",
            "      \"Node Type\": \"Seq Scan\",",
            "      \"Parallel Aware\": false,",
            "      \"Async Capable\": false,",
            "      \"Relation Name\": \"t\",",
            "      \"Alias\": \"t\",",
            "      \"Disabled\": false,",
            "      \"Filter\": \"(a = 5)\"",
            "    }",
            "  }",
            "]",
        ]
        .join("\n");
        assert_eq!(lines.join("\n"), expected);
    }

    #[test]
    fn yaml_seq_scan_matches_doc_fixture() {
        let lines = render(&seq_scan_plan(), &ExplainOptions { format: ExplainFormat::Yaml, ..opts(false) })
            .expect("render");
        let expected = [
            "- Plan: ",
            "    Node Type: \"Seq Scan\"",
            "    Parallel Aware: false",
            "    Async Capable: false",
            "    Relation Name: \"t\"",
            "    Alias: \"t\"",
            "    Disabled: false",
            "    Filter: \"(a = 5)\"",
        ]
        .join("\n");
        assert_eq!(lines.join("\n"), expected);
    }

    #[test]
    fn xml_is_reported_as_unimplemented() {
        let o = ExplainOptions { format: ExplainFormat::Xml, ..opts(false) };
        let err = render(&seq_scan_plan(), &o).expect_err("xml");
        assert!(err.message.contains("EXPLAIN FORMAT XML"), "{}", err.message);
    }

    #[test]
    #[ignore = "needs PLN-15"]
    fn text_seq_scan_from_plan_uses_node_label() {
        let lines = render(&seq_scan_plan(), &opts(false)).expect("render");
        assert_eq!(lines, vec!["Seq Scan on t".to_string(), "  Filter: (a = 5)".to_string()]);
    }
}

