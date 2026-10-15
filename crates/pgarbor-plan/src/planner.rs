//! Planner orchestration: `BoundStatement` -> `Plan` (PLN-13). PG reference: `planner.c`
//! (`standard_planner`, `subquery_planner`, `grouping_planner`), `createplan.c` (`create_plan`),
//! `setrefs.c` (`set_plan_references`), `plancat.c`.
//!
//! ## `plan_statement(stmt, ctx)`
//! 1. `PlannerState::new(ctx)`.
//! 2. By statement: `Query` -> `plan_query_plan`; `Insert`/`Update`/`Delete`/`CreateTableAs` ->
//!    `build_dml::plan_dml_statement` (PlanKind Insert/Update/Delete/Populate); anything else ->
//!    `PgError::internal("statement is not plannable")` (XX000).
//! 3. Query: `build::build_query` -> [`plan_logical`] -> root `PlanNode`.
//! 4. If `ctx.options.for_explain`: `explain::annotate_plan` (properties) BEFORE finalization.
//! 5. `setrefs::finalize` converts expressions to executable form (consumes `vars`/`ordering`).
//! 6. Assemble [`Plan`]: `subplans`/`ctes` from the state, `output` from [`output_columns`], `param_types`
//!    from the context, `deps` (`PlanDeps { relations, types, functions, uses_search_path: true }`),
//!    `catalog_version = ctx.env.snapshot().version()`.
//!
//! ## `plan_logical(st, lp)`: physical conversion of a logical node (recursive)
//! * `Scan`: `Table` -> `index::choose_scan` (virtual -> `VirtualScan` with `eq_filters`); `Subquery` ->
//!   `plan_logical` of the child, then a PASS-THROUGH `Project` on top: `exprs` = the child's `vars`
//!   (inner-level terms, unchanged), `vars` = `Column{0, rt_index, i+1}` for the parent's range entry,
//!   `out` from the range entry. NEVER mutate the child's own `vars` (its filters are still expressed in
//!   its own level's terms; `setrefs` removes identity projections later); keep the child's `ordering`
//!   translated through the projection when it is a plain column mapping. `Function` -> `FunctionScan`; `Values` -> `ValuesScan`
//!   (`subplan::process_sublinks_in` over cells); `Cte` -> `CteScan`; `WorkTable` -> `WorkTableScan`;
//!   `OneRow` -> `Result` with no exprs. Scan quals: sub-links processed first
//!   (`subplan::process_sublinks_in`), then given to the path chooser.
//! * `Join` -> `join::plan_join`; `Filter` -> `Filter` node (sub-links processed); `Aggregate` ->
//!   `agg::plan_aggregate`; `Window` -> `window::plan_window`; `Project` -> [`plan_project`]; `Sort` ->
//!   `sort_limit::plan_sort` (the parent `Limit` passes top-N); `Limit` -> `sort_limit::plan_limit`;
//!   `Distinct` -> `sort_limit::plan_distinct`; `SetOp` -> `setop::plan_setop`; `LockRows` ->
//!   `LockRows` node.
//! * Every node gets `out` (names/types), `vars`, `est` (via `cost::cost_node`), `ordering`, and
//!   `level`: `plan_logical` sets `st.cur_level = lp.level` on entry (restoring the previous value on exit)
//!   and every module creates nodes with `PlannerState::new_node`, so EXPLAIN can find the range table names
//!   of each node (`PlanNode::level`).
//!
//! ## `plan_project`
//! Evaluates the target list. Simplify every expression (`simplify::simplify_expr`), process sub-links,
//! build `Project` (or `ProjectSet` when `has_srf`). Do NOT elide identity projections here: `setrefs`
//! does it after translation (an identity can only be recognized in slot terms). `LIMIT` above a `Project` containing
//! expensive/volatile expressions is still applied after projection (semantics). `ordering` passes
//! through for expressions that merely rename columns.
//!
//! ## `output_columns(q)`
//! `ColumnDesc { name, type_id, table_oid, column_attnum }` from `BoundQuery::output` (`origin`).
use pgarbor_catalog::invalidation::PlanDeps;
use pgarbor_common::{Oid, PgError, Result};
use pgarbor_sql::ir::{BoundExpr, BoundQuery, BoundStatement, ExprKind, TargetEntry};
use pgarbor_types::TypeId;

use crate::agg;
use crate::build;
use crate::build_dml;
use crate::context::{PlanContext, PlannerState};
use crate::cost;
use crate::explain;
use crate::index;
use crate::join;
use crate::logical::{LogicalFilter, LogicalLockRows, LogicalNode, LogicalPlan, LogicalProject, LogicalScan, ScanSource};
use crate::plan::{
    ColumnDesc, CteScan, FilterNode, FunctionItem, FunctionScan, JoinType, LockRowsNode, LockTarget, NodeKind, OutCol,
    Plan, PlanKind, PlanNode, ProjectNode, ProjectSetNode, ResultNode, SortKeySpec, ValuesScan, WorkTableScan,
};
use crate::{setop, setrefs, simplify, sort_limit, subplan, window};

/// `ColumnRef::attnum` of the system column `ctid`; the planner uses it as the bind-time name of a
/// scan's trailing rowid slot (the storage row id), which no user expression can otherwise reference.
const CTID_ATTNUM: i16 = -1;

/// Plan one statement (module docs).
pub fn plan_statement(stmt: &BoundStatement, ctx: &PlanContext) -> Result<Plan> {
    let mut st = PlannerState::new(ctx);
    let (mut root, kind, output) = match stmt {
        BoundStatement::Query(q) => {
            let root = plan_query_node(&mut st, q)?;
            (root, PlanKind::Select, output_columns(q))
        }
        BoundStatement::Insert(_)
        | BoundStatement::Update(_)
        | BoundStatement::Delete(_)
        | BoundStatement::CreateTableAs(_) => {
            let (root, kind) = build_dml::plan_dml_statement(&mut st, stmt)?;
            let output = dml_output(stmt);
            (root, kind, output)
        }
        _ => return Err(PgError::internal("statement is not plannable")),
    };
    if ctx.options.for_explain {
        explain::annotate_plan(&mut st, &mut root)?;
    }
    let deps = PlanDeps {
        relations: std::mem::take(&mut st.dep_relations),
        types: std::mem::take(&mut st.dep_types),
        functions: std::mem::take(&mut st.dep_functions),
        uses_search_path: true,
    };
    let mut plan = Plan {
        kind,
        root,
        subplans: std::mem::take(&mut st.subplans),
        ctes: std::mem::take(&mut st.ctes),
        output,
        param_types: ctx.param_types.clone(),
        deps,
        catalog_version: ctx.env.snapshot().version(),
    };
    setrefs::finalize(&mut plan)?;
    Ok(plan)
}

/// Plan a query (SELECT) into its physical root node (steps 3 of the module docs, without
/// finalization). Also used for sub-plans, CTAS sources and INSERT ... SELECT sources.
pub fn plan_query_node(st: &mut PlannerState, q: &BoundQuery) -> Result<PlanNode> {
    let lp = build::build_query(st, q)?;
    plan_logical(st, &lp)
}

/// Physical conversion of a logical plan (module docs).
pub fn plan_logical(st: &mut PlannerState, lp: &LogicalPlan) -> Result<PlanNode> {
    let saved_level = st.cur_level;
    st.cur_level = lp.level;
    let result = plan_logical_node(st, lp);
    st.cur_level = saved_level;
    result
}

/// Target-list projection (module docs).
pub fn plan_project(st: &mut PlannerState, p: &LogicalProject) -> Result<PlanNode> {
    let child = plan_logical(st, &p.input)?;
    let mut simplified = Vec::with_capacity(p.exprs.len());
    for e in &p.exprs {
        simplified.push(simplify::simplify_expr(st, e.clone())?);
    }
    let exprs = subplan::process_sublinks_in(st, simplified)?;
    // Output names are fixed by the caller (`set_names`); the types are the expression types.
    let outs: Vec<OutCol> = exprs.iter().map(|e| OutCol { name: "?column?".to_string(), ty: e.ty }).collect();
    let width = (exprs.len() as u32).saturating_mul(8);
    // Ordering of the input that survives the projection (keys that are output expressions).
    let ordering = map_ordering(&child.ordering, &p.exprs, &p.exprs);
    let child_est = child.est;
    // A projection over the one-row `Result` (no input columns) is the `Result` node itself
    // (`SELECT 1`): PG folds the target list into the Result as well.
    let fold = match &child.kind {
        NodeKind::Result(r) if child.out.is_empty() && !p.has_srf => Some(r.one_time_filter.clone()),
        _ => None,
    };
    let mut node = if let Some(one_time_filter) = fold {
        let est = cost::cost_node("result", 1.0, width, &[])?;
        let mut n = st.new_node(NodeKind::Result(ResultNode { exprs, one_time_filter }), outs);
        n.est = est;
        n
    } else if p.has_srf {
        let est = cost::cost_node("project", child_est.rows, width, &[child_est])?;
        let mut n = st.new_node(NodeKind::ProjectSet(Box::new(ProjectSetNode { input: child, exprs })), outs);
        n.est = est;
        n
    } else {
        let est = cost::cost_node("project", child_est.rows, width, &[child_est])?;
        let mut n = st.new_node(NodeKind::Project(Box::new(ProjectNode { input: child, exprs })), outs);
        n.est = est;
        n
    };
    node.vars = p.exprs.clone();
    node.ordering = ordering;
    Ok(node)
}

/// RowDescription columns of a query (module docs).
pub fn output_columns(q: &BoundQuery) -> Vec<ColumnDesc> {
    q.output
        .iter()
        .map(|c| ColumnDesc {
            name: c.name.clone(),
            type_id: c.ty,
            table_oid: c.origin.map(|o| o.table).unwrap_or(0),
            column_attnum: c.origin.map(|o| o.attnum).unwrap_or(0),
        })
        .collect()
}

// ---------------------------------------------------------------------------------------------
// Statement output
// ---------------------------------------------------------------------------------------------

/// Result columns of a DML / CTAS statement (`Plan::output`).
fn dml_output(stmt: &BoundStatement) -> Vec<ColumnDesc> {
    match stmt {
        BoundStatement::Insert(ins) => returning_columns(&ins.returning, ins.target.oid, 1),
        BoundStatement::Update(upd) => returning_columns(&upd.returning, upd.target.oid, upd.target_rt),
        BoundStatement::Delete(del) => returning_columns(&del.returning, del.target.oid, del.target_rt),
        BoundStatement::CreateTableAs(ctas) => ctas
            .table
            .columns
            .iter()
            .map(|c| ColumnDesc { name: c.name.clone(), type_id: c.type_id, table_oid: 0, column_attnum: 0 })
            .collect(),
        _ => Vec::new(),
    }
}

/// RETURNING columns: plain references to the target table report its OID and attnum.
fn returning_columns(entries: &[TargetEntry], table: Oid, target_rt: u32) -> Vec<ColumnDesc> {
    entries
        .iter()
        .filter(|e| !e.junk)
        .map(|e| {
            let (table_oid, column_attnum) = match &e.expr.kind {
                ExprKind::Column(c) if c.levels_up == 0 && c.rt_index == target_rt && c.attnum > 0 => (table, c.attnum),
                _ => (0, 0),
            };
            ColumnDesc { name: e.name.clone(), type_id: e.expr.ty, table_oid, column_attnum }
        })
        .collect()
}

// ---------------------------------------------------------------------------------------------
// Logical -> physical
// ---------------------------------------------------------------------------------------------

fn plan_logical_node(st: &mut PlannerState, lp: &LogicalPlan) -> Result<PlanNode> {
    match &lp.node {
        LogicalNode::Scan(scan) => plan_scan(st, lp, scan),
        LogicalNode::Join(j) => {
            let mut node = join::plan_join(st, j)?;
            fill_layout(&mut node, lp);
            Ok(node)
        }
        LogicalNode::Filter(f) => plan_filter(st, f),
        LogicalNode::Aggregate(a) => {
            let mut node = agg::plan_aggregate(st, a)?;
            fill_layout(&mut node, lp);
            Ok(node)
        }
        LogicalNode::Window(w) => {
            let mut node = window::plan_window(st, w)?;
            fill_layout(&mut node, lp);
            Ok(node)
        }
        LogicalNode::Project(p) => {
            let mut node = plan_project(st, p)?;
            set_names(&mut node, lp)?;
            Ok(node)
        }
        LogicalNode::Sort(s) => {
            let mut node = sort_limit::plan_sort(st, s, None)?;
            fill_layout(&mut node, lp);
            Ok(node)
        }
        LogicalNode::Limit(l) => {
            let mut node = sort_limit::plan_limit(st, l)?;
            fill_layout(&mut node, lp);
            Ok(node)
        }
        LogicalNode::Distinct(d) => {
            let mut node = sort_limit::plan_distinct(st, d)?;
            fill_layout(&mut node, lp);
            Ok(node)
        }
        LogicalNode::SetOp(s) => {
            let mut node = setop::plan_setop(st, s)?;
            fill_layout(&mut node, lp);
            Ok(node)
        }
        LogicalNode::LockRows(l) => plan_lock_rows(st, l),
    }
}

/// Leaf scans (module docs, `Scan`).
fn plan_scan(st: &mut PlannerState, lp: &LogicalPlan, scan: &LogicalScan) -> Result<PlanNode> {
    match &scan.source {
        ScanSource::Table { table, is_virtual } => {
            if !*is_virtual {
                st.note_relation(table.oid);
            }
            let quals = subplan::process_sublinks_in(st, scan.quals.clone())?;
            let scan_with_quals = LogicalScan {
                rt_index: scan.rt_index,
                source: scan.source.clone(),
                quals,
                with_rowid: scan.with_rowid,
            };
            let mut node = index::choose_scan(st, &scan_with_quals, None, false)?;
            // The scan's layout is the table row (bind-time `Column`s of the range entry) plus the
            // trailing rowid slot when requested. The planner owns this layout (setrefs matches it).
            node.vars = lp.vars.clone();
            node.out = lp.out.clone();
            if scan.with_rowid {
                node.vars.push(BoundExpr::column(0, scan.rt_index, CTID_ATTNUM, TypeId::INT8));
                node.out.push(OutCol { name: "ctid".to_string(), ty: TypeId::INT8 });
            }
            Ok(node)
        }
        ScanSource::Subquery(child_lp) => plan_subquery_scan(st, lp, scan, child_lp),
        ScanSource::Function { funcs, with_ordinality } => {
            let mut items = Vec::with_capacity(funcs.len());
            for f in funcs {
                let expr = subplan::process_sublinks(st, f.expr.clone())?;
                items.push(FunctionItem { expr, columns: f.columns.clone() });
            }
            let quals = subplan::process_sublinks_in(st, scan.quals.clone())?;
            let rows = scan_rows(st, scan, &quals)?;
            let kind = NodeKind::FunctionScan(FunctionScan {
                funcs: items,
                with_ordinality: *with_ordinality,
                filter: simplify::make_and(quals),
            });
            scan_node(st, lp, kind, "function", rows)
        }
        ScanSource::Values { rows } => {
            let mut cells = Vec::with_capacity(rows.len());
            for row in rows {
                cells.push(subplan::process_sublinks_in(st, row.clone())?);
            }
            let quals = subplan::process_sublinks_in(st, scan.quals.clone())?;
            let est_rows = scan_rows(st, scan, &quals)?;
            let kind = NodeKind::ValuesScan(ValuesScan { rows: cells, filter: simplify::make_and(quals) });
            scan_node(st, lp, kind, "values", est_rows)
        }
        ScanSource::Cte { cte_id } => {
            let quals = subplan::process_sublinks_in(st, scan.quals.clone())?;
            let rows = scan_rows(st, scan, &quals)?;
            let kind = NodeKind::CteScan(CteScan { cte_id: *cte_id, filter: simplify::make_and(quals) });
            scan_node(st, lp, kind, "seqscan", rows)
        }
        ScanSource::WorkTable { cte_id } => {
            let quals = subplan::process_sublinks_in(st, scan.quals.clone())?;
            let rows = scan_rows(st, scan, &quals)?;
            let kind = NodeKind::WorkTableScan(WorkTableScan { cte_id: *cte_id, filter: simplify::make_and(quals) });
            scan_node(st, lp, kind, "seqscan", rows)
        }
        ScanSource::OneRow => {
            let quals = subplan::process_sublinks_in(st, scan.quals.clone())?;
            let kind = NodeKind::Result(ResultNode { exprs: Vec::new(), one_time_filter: simplify::make_and(quals) });
            scan_node(st, lp, kind, "result", 1.0)
        }
    }
}

/// Sub-query in FROM: the child plan under a pass-through projection (module docs).
fn plan_subquery_scan(st: &mut PlannerState, lp: &LogicalPlan, scan: &LogicalScan, child_lp: &LogicalPlan) -> Result<PlanNode> {
    let child = plan_logical(st, child_lp)?;
    if child.vars.len() != child.width() || child.vars.len() != lp.out.len() {
        return Err(PgError::internal("sub-query plan output does not match its range entry"));
    }
    let child_exprs = child.vars.clone();
    let child_est = child.est;
    let child_ordering = child.ordering.clone();
    let outs: Vec<BoundExpr> = lp
        .out
        .iter()
        .enumerate()
        .map(|(i, c)| BoundExpr::column(0, scan.rt_index, (i + 1) as i16, c.ty))
        .collect();
    let ordering = map_ordering(&child_ordering, &child_exprs, &outs);
    let est = cost::cost_node("project", child_est.rows, width_of(lp), &[child_est])?;
    let kind = NodeKind::Project(Box::new(ProjectNode { input: child, exprs: child_exprs }));
    let mut node = st.new_node(kind, lp.out.clone());
    // The projection's expressions are the child's (its own level's) terms.
    node.level = child_lp.level;
    node.vars = outs;
    node.ordering = ordering;
    node.est = est;
    Ok(node)
}

/// Residual filter (module docs, `Filter`).
fn plan_filter(st: &mut PlannerState, f: &LogicalFilter) -> Result<PlanNode> {
    let child = plan_logical(st, &f.input)?;
    let quals = subplan::process_sublinks_in(st, f.quals.clone())?;
    let predicate = match simplify::make_and(quals) {
        Some(p) => p,
        None => return Ok(child),
    };
    let rows = child.est.rows * cost::selectivity(&predicate)?;
    let est = cost::cost_node("filter", rows, child.est.width, &[child.est])?;
    let vars = child.vars.clone();
    let ordering = child.ordering.clone();
    let out = child.out.clone();
    let mut node = st.new_node(NodeKind::Filter(Box::new(FilterNode { input: child, predicate })), out);
    node.vars = vars;
    node.ordering = ordering;
    node.est = est;
    Ok(node)
}

/// `FOR UPDATE` / `FOR SHARE`: the input is passed through; each locked table's rowid slot is located
/// in the input layout.
fn plan_lock_rows(st: &mut PlannerState, l: &LogicalLockRows) -> Result<PlanNode> {
    let child = plan_logical(st, &l.input)?;
    let mut targets = Vec::with_capacity(l.tables.len());
    for (table_oid, rt_index) in &l.tables {
        match find_rowid_slot(&child, *rt_index) {
            Some(rowid_slot) => targets.push(LockTarget { table_oid: *table_oid, rowid_slot }),
            None => {
                return Err(PgError::internal(format!(
                    "row identity of range entry {rt_index} is not available for row locking"
                )))
            }
        }
    }
    let est = child.est;
    let vars = child.vars.clone();
    let ordering = child.ordering.clone();
    let out = child.out.clone();
    let kind = NodeKind::LockRows(Box::new(LockRowsNode {
        input: child,
        strength: l.strength.clone(),
        wait: l.wait.clone(),
        targets,
    }));
    let mut node = st.new_node(kind, out);
    node.vars = vars;
    node.ordering = ordering;
    node.est = est;
    Ok(node)
}

// ---------------------------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------------------------

/// Output width estimate of a node with `lp`'s slots (8 bytes per slot).
fn width_of(lp: &LogicalPlan) -> u32 {
    (lp.out.len() as u32).saturating_mul(8)
}

/// Estimated rows of a leaf scan after its (processed) quals.
fn scan_rows(st: &PlannerState, scan: &LogicalScan, quals: &[BoundExpr]) -> Result<f64> {
    let mut rows = cost::estimate_scan_rows(st, scan)?;
    for q in quals {
        rows *= cost::selectivity(q)?;
    }
    Ok(rows)
}

/// A leaf scan node that reads its own rows (layout and bind-time vars from `lp`), with the quals
/// already stored in `kind`.
fn scan_node(st: &PlannerState, lp: &LogicalPlan, kind: NodeKind, shape: &str, rows: f64) -> Result<PlanNode> {
    let est = cost::cost_node(shape, rows, width_of(lp), &[])?;
    let mut node = st.new_node(kind, lp.out.clone());
    node.vars = lp.vars.clone();
    node.est = est;
    Ok(node)
}

/// Set the names of a projection's output slots from the logical node.
fn set_names(node: &mut PlanNode, lp: &LogicalPlan) -> Result<()> {
    if node.out.len() != lp.out.len() {
        return Err(PgError::internal("projection width does not match its output columns"));
    }
    node.out = lp.out.clone();
    Ok(())
}

/// Fill the layout of a node built by another module when it left it empty.
fn fill_layout(node: &mut PlanNode, lp: &LogicalPlan) {
    if node.vars.is_empty() {
        node.vars = lp.vars.clone();
    }
    if node.out.is_empty() {
        node.out = lp.out.clone();
    }
}

/// Keys of `keys` (over the input's terms) that are some `exprs[i]`, rewritten to `outs[i]`. The result
/// is the longest prefix whose keys all survive the projection.
fn map_ordering(keys: &[SortKeySpec], exprs: &[BoundExpr], outs: &[BoundExpr]) -> Vec<SortKeySpec> {
    let mut result = Vec::new();
    for key in keys {
        let mapped = exprs.iter().position(|e| *e == key.expr).and_then(|i| outs.get(i));
        match mapped {
            Some(out) => {
                let mut k = key.clone();
                k.expr = out.clone();
                result.push(k);
            }
            None => break,
        }
    }
    result
}

/// The rowid slot of range entry `rt` in the output row of `node`, if `node` exposes it.
/// Scans with rowids put it last; joins concatenate outer ++ inner (only the outer side survives
/// Semi/Anti joins); row-preserving nodes pass it through.
fn find_rowid_slot(node: &PlanNode, rt: u32) -> Option<u32> {
    let is_rowid_scan = match &node.kind {
        NodeKind::SeqScan(s) => s.with_rowid,
        NodeKind::IndexScan(s) => s.with_rowid,
        _ => false,
    };
    if is_rowid_scan {
        let of_rt = matches!(node.vars.first().map(|v| &v.kind), Some(ExprKind::Column(c)) if c.levels_up == 0 && c.rt_index == rt);
        if !of_rt {
            return None;
        }
        return node.width().checked_sub(1).map(|w| w as u32);
    }
    match &node.kind {
        NodeKind::Filter(_)
        | NodeKind::Sort(_)
        | NodeKind::Limit(_)
        | NodeKind::Distinct(_)
        | NodeKind::LockRows(_)
        | NodeKind::Materialize(_) => node.children().first().and_then(|c| find_rowid_slot(*c, rt)),
        NodeKind::NestedLoop(_) | NodeKind::HashJoin(_) | NodeKind::MergeJoin(_) => {
            let join_type = match &node.kind {
                NodeKind::NestedLoop(j) => j.join_type,
                NodeKind::HashJoin(j) => j.join_type,
                NodeKind::MergeJoin(j) => j.join_type,
                _ => return None,
            };
            let children = node.children();
            if children.len() != 2 {
                return None;
            }
            let outer = children[0];
            if let Some(slot) = find_rowid_slot(outer, rt) {
                return Some(slot);
            }
            if matches!(join_type, JoinType::Semi | JoinType::Anti) {
                return None;
            }
            find_rowid_slot(children[1], rt).map(|slot| slot + outer.width() as u32)
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use pgarbor_catalog::testing;
    use pgarbor_catalog::CatalogEnv;
    use pgarbor_common::{PgError, Result, SqlState};
    use pgarbor_sql::ir::BoundStatement;
    use pgarbor_sql::{bind, parse_one, BindContext, BindOutput, BindSession};
    use pgarbor_types::{IoConfig, TypeId};

    use crate::context::{PlanContext, PlanOptions};
    use crate::plan::{NodeKind, PlanKind};

    fn bind_sql(env: &CatalogEnv, sql: &str) -> Result<BindOutput> {
        let raw = parse_one(sql)?;
        let bind_ctx = BindContext {
            env,
            session: BindSession { database: "postgres".to_string() },
            io_config: IoConfig::default(),
            statement_timestamp: 0,
            declared_params: Vec::new(),
        };
        bind(&raw, &bind_ctx)
    }

    fn plan_ctx(env: &CatalogEnv) -> PlanContext<'_> {
        PlanContext {
            env,
            param_types: Vec::new(),
            param_values: None,
            options: PlanOptions::default(),
            table_rows: HashMap::new(),
        }
    }

    #[test]
    fn select_one_plans_result_node() -> Result<()> {
        let (_kv, env) = testing::bootstrapped()?;
        let bound = bind_sql(&env, "select 1")?;
        let plan = crate::plan(&bound.statement, &plan_ctx(&env))?;
        assert_eq!(plan.kind, PlanKind::Select);
        assert!(matches!(plan.root.kind, NodeKind::Result(_)));
        assert_eq!(plan.output.len(), 1);
        assert_eq!(plan.output[0].name, "?column?");
        assert_eq!(plan.output[0].type_id, TypeId::INT4);
        assert_eq!(plan.output[0].column_attnum, 0);
        assert_eq!(plan.catalog_version, env.snapshot().version());
        Ok(())
    }

    #[test]
    fn utility_statement_is_not_plannable() -> Result<()> {
        let (_kv, env) = testing::bootstrapped()?;
        let err = crate::plan(&BoundStatement::Empty, &plan_ctx(&env))
            .err()
            .ok_or_else(|| PgError::internal("expected a planning error"))?;
        assert_eq!(err.sqlstate, SqlState::INTERNAL_ERROR);
        assert_eq!(err.message, "statement is not plannable");
        Ok(())
    }

    #[test]
    #[ignore = "needs PLN-01 (build), PLN-05 (index), PLN-09 (sort/limit) and PLN-14 (setrefs)"]
    fn filter_order_limit_plans_over_a_table() -> Result<()> {
        let (mut kv, env) = testing::bootstrapped()?;
        let ddl = bind_sql(&env, "create table t (a int4)")?;
        let spec = match ddl.statement {
            BoundStatement::CreateTable(spec) => spec,
            _ => return Err(PgError::internal("expected CREATE TABLE")),
        };
        let (_res, env) = testing::run(&mut kv, &env, |t| t.create_table(&spec).map(|_| ()))?;
        let bound = bind_sql(&env, "select a from t where a > 1 order by a limit 3")?;
        let plan = crate::plan(&bound.statement, &plan_ctx(&env))?;
        assert_eq!(plan.kind, PlanKind::Select);
        assert_eq!(plan.output.len(), 1);
        assert_eq!(plan.output[0].name, "a");
        assert_eq!(plan.output[0].column_attnum, 1);
        assert!(!plan.deps.relations.is_empty());
        Ok(())
    }
}
