//! `BoundQuery` -> `LogicalPlan` for queries (PLN-01). PG references: `planner.c`
//! (`subquery_planner`, `grouping_planner`), `prepjointree.c` (`pull_up_subqueries`,
//! `deconstruct_jointree` concepts), `initsplan.c` (`distribute_qual_to_rels`), `planmain.c`
//! (`query_planner`).
//!
//! `build_query(st, q)` converts a bound query into a logical plan. Statement-level entry points
//! (`planner::plan_statement`) call it for SELECT, for the source of INSERT, for CTAS and for
//! sub-queries.
//!
//! ## Steps for a SELECT block (`QueryBody::Select`)
//! 1. **Range table**: for every range entry create the leaf [`LogicalScan`] (`build_range_scan`):
//!    `RangeKind::Relation` -> `ScanSource::Table` (TableRef from the catalog `Relation`, `is_virtual`
//!    if oid < 16384; note the dependency with `st.note_relation`); `Subquery` ->
//!    `ScanSource::Subquery(build_query(sub))` (a trivial sub-select - no aggregates/limit/DISTINCT, one
//!    FROM item - MAY be pulled up by substituting its target expressions for the outer `Column`s; skip in
//!    v1: keep the sub-plan); `Function` -> `ScanSource::Function`; `Values` -> `ScanSource::Values`
//!    (rows after `simplify`); `Cte` -> `cte::plan_cte_reference` (inline or `ScanSource::Cte`).
//!    `vars` = `Column{levels_up: 0, rt_index, attnum}` for every column (dropped columns too), `out` =
//!    names/types from the `RangeEntry`.
//! 2. **FROM tree**: `build_from_item` converts `FromItem::Join` recursively into [`LogicalJoin`]
//!    (JoinKind Inner/Left/Right/Full -> `JoinType`), keeping ON quals in `quals`; top-level comma items
//!    are combined with `join::order_join_tree` (inner joins may be reordered freely by it; outer
//!    joins keep their syntactic structure). LATERAL: a scan/subquery whose expressions reference
//!    earlier items gets `lateral = true` on the join that combines it.
//!    `FROM`-less select: `ScanSource::OneRow`.
//! 3. **WHERE**: `simplify::simplify_quals(split_conjuncts(where))`, then
//!    `pushdown::push_down_quals(plan, quals)` distributes them (including outer-join reductions).
//! 4. **Aggregation**: if `sel.has_aggs` or GROUP BY / HAVING exist: `agg::build_aggregate`.
//! 5. **Windows**: `window::build_windows` (one `LogicalWindow` per distinct `WindowDef`).
//! 6. **Target list**: `LogicalProject` over all target entries including junk ones (sublinks in
//!    expressions are handled by `subplan::process_sublinks` when the physical plan is made, not here);
//!    `has_srf` from `sel.has_target_srfs`.
//! 7. **DISTINCT / ORDER BY / LIMIT / locking**: `sort_limit::build_sort_limit_distinct` appends
//!    `Distinct`/`Sort`/`Limit` and a final projection dropping junk entries (`n_visible`), `LockRows`
//!    for `q.locking` (applied above step 3, see `wrap_lock_rows`; LIMIT without ORDER BY then locks
//!    only the rows pulled, but ORDER BY + LIMIT locks every filtered row before sorting).
//! `QueryBody::Values` -> a `Values` scan wrapped by sort/limit; `QueryBody::SetOp` ->
//! `setop::build_setop` then sort/limit. CTEs of the query are registered first with
//! `cte::register_ctes` (they must be available to every reference, including from sub-queries).
//!
//! ## Output
//! The returned plan's `vars` are the query's visible output expressions; `out` carries the final
//! names/types (`BoundQuery::output`).
//!
//! ## Levels
//! `build_query` enters one query level per block (`PlannerState::enter_level`) and leaves it when the
//! block is built; every node created for the block carries that level id in `LogicalPlan::level`.
//! `build_select`, `build_range_scan`, `build_from_item` and `scan_to_plan` do not enter levels: they
//! run inside the level opened by `build_query`.
use pgarbor_common::{Oid, PgError, Result, SqlState};
use pgarbor_sql::ir::{
    BoundExpr, BoundQuery, BoundSelect, BoundValues, ExprKind, FromItem, JoinKind, QueryBody, RangeEntry, RangeKind,
    SetOpBody,
};
use pgarbor_types::TypeId;

use crate::agg;
use crate::context::PlannerState;
use crate::cte;
use crate::join;
use crate::logical::{LogicalJoin, LogicalLockRows, LogicalNode, LogicalPlan, LogicalProject, LogicalScan, ScanSource};
use crate::plan::{FunctionItem, JoinType, OutCol, TableRef};
use crate::pushdown;
use crate::setop;
use crate::simplify;
use crate::sort_limit;
use crate::window;

/// Relations with an OID below this bound are virtual catalog relations (PG `FirstNormalObjectId`).
const FIRST_NORMAL_OID: u32 = 16384;

/// Internal planner error (a state that the binder should have prevented).
fn internal(message: impl Into<String>) -> PgError {
    PgError::new(SqlState::INTERNAL_ERROR, message)
}

/// Build the logical plan of a complete query (module docs).
pub fn build_query(st: &mut PlannerState, q: &BoundQuery) -> Result<LogicalPlan> {
    // The binder opens a CTE frame only for query levels that have a WITH clause, so
    // `levels_up` of a CTE reference counts WITH levels only: mirror that here.
    let has_with = !q.ctes.is_empty();
    if has_with {
        cte::register_ctes(st, &q.ctes)?;
    }
    let result = match &q.body {
        QueryBody::Select(sel) => build_select_block(st, sel, q),
        QueryBody::Values(bv) => build_values_block(st, bv, q),
        QueryBody::SetOp(body) => build_setop_block(st, body, q),
    };
    if has_with {
        cte::unregister_ctes(st);
    }
    result
}

/// Build the part of a SELECT block below ORDER BY/LIMIT (steps 1-6). Runs inside the query level
/// opened by [`build_query`] (it does not enter a level itself).
pub fn build_select(st: &mut PlannerState, sel: &BoundSelect, q: &BoundQuery) -> Result<LogicalPlan> {
    let with_rowid = rowid_range_indexes(sel, q);
    let mut plan = build_from_clause(st, sel, &with_rowid)?;
    add_table_identity(&mut plan, sel)?;
    if let Some(where_expr) = &sel.where_ {
        let quals = simplify::simplify_quals(st, simplify::split_conjuncts(where_expr.clone()))?;
        if !quals.is_empty() {
            plan = pushdown::push_down_quals(st, plan, quals)?;
        }
    }
    // Locking clauses sit above the FROM/WHERE tree. The binder rejects them together with
    // aggregation, windows, DISTINCT and set operations, so the target list is the only thing above.
    plan = wrap_lock_rows(plan, sel, q)?;
    let grouped = sel.has_aggs || sel.having.is_some() || !sel.group_by.exprs.is_empty() || sel.group_by.sets.is_some();
    if grouped || !sel.windows.is_empty() {
        // Aggregate and window calls are collected from the target list and matched structurally
        // against the final projection, so both must see the same (simplified) expressions.
        let simplified = simplify_select_exprs(st, sel)?;
        let sel = &simplified;
        if grouped {
            plan = agg::build_aggregate(st, plan, sel)?;
        }
        if !sel.windows.is_empty() {
            plan = window::build_windows(st, plan, sel)?;
        }
        return project_targets(st, plan, sel);
    }
    project_targets(st, plan, sel)
}

/// Every expression slot of `sel` that `resolve_tableoid` rewrites.
fn select_exprs(sel: &BoundSelect) -> Vec<&BoundExpr> {
    fn from_quals<'a>(item: &'a FromItem, out: &mut Vec<&'a BoundExpr>) {
        if let FromItem::Join(j) = item {
            if let Some(on) = &j.quals {
                out.push(on);
            }
            from_quals(&j.left, out);
            from_quals(&j.right, out);
        }
    }
    let mut out: Vec<&BoundExpr> = sel.target_list.iter().map(|t| &t.expr).collect();
    out.extend(sel.where_.iter());
    out.extend(sel.having.iter());
    for w in &sel.windows {
        out.extend(w.partition_by.iter());
        out.extend(w.order_by.iter().map(|k| &k.expr));
    }
    for item in &sel.from {
        from_quals(item, &mut out);
    }
    out
}

/// Carry table identity through joins as a real slot, so outer joins null-extend it too.
/// Whole-row references use the same slot to distinguish an absent tuple from an all-NULL tuple.
fn add_table_identity(plan: &mut LogicalPlan, sel: &BoundSelect) -> Result<()> {
    match &mut plan.node {
        LogicalNode::Scan(scan) => {
            if !matches!(scan.source, ScanSource::Table { is_virtual: false, .. }) {
                return Ok(());
            }
            let rt = scan.rt_index;
            let needed = select_exprs(sel).into_iter().any(|e| e.any(&mut |n| {
                matches!(&n.kind, ExprKind::Column(c) if c.levels_up == 0 && c.rt_index == rt && matches!(c.attnum, 0 | -6))
            }));
            if needed {
                if scan.with_rowid {
                    return Err(PgError::unimplemented("PLN-01", "whole-row/tableoid references with row locking"));
                }
                plan.vars.push(BoundExpr::column(0, rt, -6, TypeId::OID));
                plan.out.push(OutCol { name: "tableoid".into(), ty: TypeId::OID });
            }
        }
        LogicalNode::Join(j) => {
            add_table_identity(&mut j.left, sel)?;
            add_table_identity(&mut j.right, sel)?;
            plan.vars = j.left.vars.iter().chain(&j.right.vars).cloned().collect();
            plan.out = j.left.out.iter().chain(&j.right.out).cloned().collect();
        }
        _ => {}
    }
    Ok(())
}

/// Copy of `sel` whose target list, HAVING and window expressions are constant-folded.
fn simplify_select_exprs(st: &mut PlannerState, sel: &BoundSelect) -> Result<BoundSelect> {
    let mut out = sel.clone();
    for t in out.target_list.iter_mut() {
        t.expr = simplify::simplify_expr(st, t.expr.clone())?;
    }
    if let Some(h) = out.having.take() {
        out.having = Some(simplify::simplify_expr(st, h)?);
    }
    for w in out.windows.iter_mut() {
        for e in w.partition_by.iter_mut() {
            *e = simplify::simplify_expr(st, e.clone())?;
        }
        for k in w.order_by.iter_mut() {
            k.expr = simplify::simplify_expr(st, k.expr.clone())?;
        }
    }
    Ok(out)
}

/// Leaf scan for range entry `rt_index` (1-based) of `sel.range_table`.
pub fn build_range_scan(st: &mut PlannerState, entries: &[RangeEntry], rt_index: u32, with_rowid: bool) -> Result<LogicalScan> {
    let entry = entry_at(entries, rt_index)?;
    let source = match &entry.kind {
        RangeKind::Relation { oid, schema, name, .. } => {
            let env = st.ctx.env;
            let rel = env
                .snapshot()
                .relation(*oid)
                .ok_or_else(|| internal(format!("could not open relation with OID {}", *oid)))?;
            let is_virtual = *oid < FIRST_NORMAL_OID;
            if !is_virtual {
                st.note_relation(*oid);
            }
            let col_types: Vec<TypeId> = rel
                .columns
                .iter()
                .map(|c| if c.dropped { TypeId::of(0) } else { c.type_id })
                .collect();
            ScanSource::Table {
                table: TableRef {
                    oid: *oid,
                    schema: schema.clone(),
                    name: name.clone(),
                    alias: entry.alias.clone(),
                    ncols: rel.columns.len() as u32,
                    col_types,
                },
                is_virtual,
            }
        }
        RangeKind::Subquery { query } => ScanSource::Subquery(build_query(st, query)?),
        RangeKind::Function { funcs, with_ordinality } => {
            let mut items: Vec<FunctionItem> = Vec::with_capacity(funcs.len());
            for f in funcs {
                let mut called: Vec<Oid> = Vec::new();
                f.expr.walk(&mut |e: &BoundExpr| {
                    if let ExprKind::FuncCall(call) = &e.kind {
                        called.push(call.func);
                    }
                    true
                });
                for oid in called {
                    if !st.dep_functions.contains(&oid) {
                        st.dep_functions.push(oid);
                    }
                }
                items.push(FunctionItem {
                    expr: f.expr.clone(),
                    columns: f.columns.iter().map(|c| c.ty).collect(),
                });
            }
            ScanSource::Function {
                funcs: items,
                with_ordinality: *with_ordinality,
            }
        }
        RangeKind::Values { values } => ScanSource::Values {
            rows: simplify_rows(st, &values.rows)?,
        },
        RangeKind::Cte { .. } => {
            let (source, _columns) = cte::plan_cte_reference(st, &entry.kind)?;
            source
        }
    };
    // Row-id slots are only meaningful for base tables (UPDATE/DELETE targets, FOR UPDATE).
    let is_table = matches!(entry.kind, RangeKind::Relation { .. });
    Ok(LogicalScan {
        rt_index,
        source,
        quals: Vec::new(),
        with_rowid: with_rowid && is_table,
    })
}

/// Convert one FROM tree item (leaf or explicit join) to a logical plan.
pub fn build_from_item(st: &mut PlannerState, entries: &[RangeEntry], item: &FromItem, with_rowid: &[u32]) -> Result<LogicalPlan> {
    match item {
        FromItem::Range(rt_index) => {
            let scan = build_range_scan(st, entries, *rt_index, with_rowid.contains(rt_index))?;
            scan_to_plan(st, entries, scan)
        }
        FromItem::Join(join_item) => {
            let left = build_from_item(st, entries, &join_item.left, with_rowid)?;
            let right = build_from_item(st, entries, &join_item.right, with_rowid)?;
            let lateral = item_is_lateral(entries, &join_item.right);
            let quals = match &join_item.quals {
                Some(on) => simplify::simplify_quals(st, simplify::split_conjuncts(on.clone()))?,
                None => Vec::new(),
            };
            let join_type = match join_item.kind {
                JoinKind::Inner => JoinType::Inner,
                JoinKind::Left => JoinType::Left,
                JoinKind::Right => JoinType::Right,
                JoinKind::Full => JoinType::Full,
            };
            Ok(make_join(st, left, right, join_type, quals, lateral))
        }
    }
}

/// Wrap a scan into a [`LogicalPlan`] with `vars`/`out`/`rels` filled in.
///
/// `rt_index == 0` is only valid for `OneRow` (no columns) and for a `VALUES` body (columns from the
/// first row's cells, named `column1..`); every other scan names a range entry of `entries`.
pub fn scan_to_plan(st: &mut PlannerState, entries: &[RangeEntry], scan: LogicalScan) -> Result<LogicalPlan> {
    let (vars, out, rels): (Vec<BoundExpr>, Vec<OutCol>, Vec<u32>) = if scan.rt_index == 0 {
        match &scan.source {
            ScanSource::OneRow => (Vec::new(), Vec::new(), Vec::new()),
            ScanSource::Values { rows } => {
                let first = rows.first().ok_or_else(|| internal("VALUES scan without rows"))?;
                let vars: Vec<BoundExpr> = first
                    .iter()
                    .enumerate()
                    .map(|(i, cell)| BoundExpr::column(0, 0, (i + 1) as i16, cell.type_id()))
                    .collect();
                let out: Vec<OutCol> = first
                    .iter()
                    .enumerate()
                    .map(|(i, cell)| OutCol {
                        name: format!("column{}", i + 1),
                        ty: cell.type_id(),
                    })
                    .collect();
                (vars, out, Vec::new())
            }
            _ => return Err(internal("range table index 0 is only valid for OneRow and VALUES scans")),
        }
    } else {
        let entry = entry_at(entries, scan.rt_index)?;
        let (vars, out) = column_vars(scan.rt_index, entry);
        (vars, out, vec![scan.rt_index])
    };
    let level = st.current_level();
    Ok(LogicalPlan {
        node: LogicalNode::Scan(Box::new(scan)),
        vars,
        out,
        rels,
        level,
    })
}

// ---------------------------------------------------------------------------------------------
// Query blocks
// ---------------------------------------------------------------------------------------------

/// Run `f` inside a fresh query level registered with `names`; the level is always left again.
fn in_level<'a, T, F>(st: &mut PlannerState<'a>, names: Vec<(String, Vec<String>)>, f: F) -> Result<T>
where
    F: FnOnce(&mut PlannerState<'a>) -> Result<T>,
{
    st.enter_level(names);
    let result = f(st);
    st.leave_level();
    result
}

fn build_select_block(st: &mut PlannerState, sel: &BoundSelect, q: &BoundQuery) -> Result<LogicalPlan> {
    let names = range_names(&sel.range_table);
    in_level(st, names, |st| {
        let projected = build_select(st, sel, q)?;
        let plan = sort_limit::build_sort_limit_distinct(st, projected, Some(sel), q)?;
        finish_output(st, plan, q)
    })
}

fn build_values_block(st: &mut PlannerState, bv: &BoundValues, q: &BoundQuery) -> Result<LogicalPlan> {
    in_level(st, Vec::new(), |st| {
        let rows = simplify_rows(st, &bv.rows)?;
        let vars: Vec<BoundExpr> = bv
            .types
            .iter()
            .enumerate()
            .map(|(i, ty)| BoundExpr::column(0, 0, (i + 1) as i16, *ty))
            .collect();
        let out: Vec<OutCol> = bv
            .types
            .iter()
            .enumerate()
            .map(|(i, ty)| OutCol {
                name: format!("column{}", i + 1),
                ty: *ty,
            })
            .collect();
        let level = st.current_level();
        let scan = LogicalScan {
            rt_index: 0,
            source: ScanSource::Values { rows },
            quals: Vec::new(),
            with_rowid: false,
        };
        let plan = LogicalPlan {
            node: LogicalNode::Scan(Box::new(scan)),
            vars,
            out,
            rels: Vec::new(),
            level,
        };
        let plan = sort_limit::build_sort_limit_distinct(st, plan, None, q)?;
        finish_output(st, plan, q)
    })
}

fn build_setop_block(st: &mut PlannerState, body: &SetOpBody, q: &BoundQuery) -> Result<LogicalPlan> {
    in_level(st, Vec::new(), |st| {
        let mut plan = setop::build_setop(st, body)?;
        // The set-operation node itself belongs to this block (its children carry their own levels).
        plan.level = st.current_level();
        let plan = sort_limit::build_sort_limit_distinct(st, plan, None, q)?;
        finish_output(st, plan, q)
    })
}

/// Make the top plan of a query block expose exactly the query's visible output and carry its final
/// names/types. Sort/limit/distinct already dropped junk entries; a wider plan gets a defensive
/// projection over its first `output.len()` slots.
fn finish_output(st: &PlannerState, mut plan: LogicalPlan, q: &BoundQuery) -> Result<LogicalPlan> {
    let width = q.output.len();
    if plan.vars.len() < width {
        return Err(internal("query plan produces fewer columns than its output"));
    }
    if plan.vars.len() > width {
        let exprs: Vec<BoundExpr> = plan.vars[..width].to_vec();
        let rels = plan.rels.clone();
        let level = st.current_level();
        let project = LogicalProject {
            input: plan,
            exprs: exprs.clone(),
            n_visible: width,
            has_srf: false,
        };
        plan = LogicalPlan {
            node: LogicalNode::Project(Box::new(project)),
            vars: exprs,
            out: Vec::new(),
            rels,
            level,
        };
    }
    plan.out = output_cols(q);
    Ok(plan)
}

/// FROM, then the target list (steps 2 and 6 of the module docs, plus WHERE/aggregation/windows).
fn build_from_clause(st: &mut PlannerState, sel: &BoundSelect, with_rowid: &[u32]) -> Result<LogicalPlan> {
    match sel.from.len() {
        0 => {
            let scan = LogicalScan {
                rt_index: 0,
                source: ScanSource::OneRow,
                quals: Vec::new(),
                with_rowid: false,
            };
            scan_to_plan(st, &sel.range_table, scan)
        }
        1 => build_from_item(st, &sel.range_table, &sel.from[0], with_rowid),
        _ => {
            let mut items: Vec<LogicalPlan> = Vec::with_capacity(sel.from.len());
            for item in &sel.from {
                items.push(build_from_item(st, &sel.range_table, item, with_rowid)?);
            }
            let (plan, leftover) = join::order_join_tree(st, items, Vec::new())?;
            if leftover.is_empty() {
                Ok(plan)
            } else {
                pushdown::push_down_quals(st, plan, leftover)
            }
        }
    }
}

/// Step 6: `LogicalProject` over every target entry (junk entries last).
fn project_targets(st: &mut PlannerState, input: LogicalPlan, sel: &BoundSelect) -> Result<LogicalPlan> {
    let mut exprs: Vec<BoundExpr> = Vec::with_capacity(sel.target_list.len());
    let mut out: Vec<OutCol> = Vec::with_capacity(sel.target_list.len());
    for target in &sel.target_list {
        let expr = simplify::simplify_expr(st, target.expr.clone())?;
        out.push(OutCol {
            name: target.name.clone(),
            ty: expr.type_id(),
        });
        exprs.push(expr);
    }
    let n_visible = sel.visible_targets().count();
    let rels = input.rels.clone();
    let level = st.current_level();
    Ok(LogicalPlan {
        node: LogicalNode::Project(Box::new(LogicalProject {
            input,
            exprs: exprs.clone(),
            n_visible,
            has_srf: sel.has_target_srfs,
        })),
        vars: exprs,
        out,
        rels,
        level,
    })
}

// ---------------------------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------------------------

/// Range-table indexes that must carry a row-id slot: the scans named by the locking clauses
/// (`of` empty = every base table of the block).
fn rowid_range_indexes(sel: &BoundSelect, q: &BoundQuery) -> Vec<u32> {
    let mut out: Vec<u32> = Vec::new();
    for clause in &q.locking {
        if clause.of.is_empty() {
            for (i, entry) in sel.range_table.iter().enumerate() {
                if matches!(entry.kind, RangeKind::Relation { .. }) {
                    out.push((i + 1) as u32);
                }
            }
        } else {
            out.extend(clause.of.iter().copied());
        }
    }
    out.sort_unstable();
    out.dedup();
    out
}

/// One `LogicalLockRows` per locking clause (PG `create_lockrows_path`), each locking the tables the
/// clause names (`of` empty = every base table of the block). Only base tables carry a row identity;
/// a subquery or function in a locking clause is not supported yet.
fn wrap_lock_rows(mut plan: LogicalPlan, sel: &BoundSelect, q: &BoundQuery) -> Result<LogicalPlan> {
    for clause in &q.locking {
        let mut tables: Vec<(Oid, u32)> = Vec::new();
        let indexes: Vec<u32> = if clause.of.is_empty() {
            (1..=sel.range_table.len() as u32).collect()
        } else {
            clause.of.clone()
        };
        for rt_index in indexes {
            let entry = entry_at(&sel.range_table, rt_index)?;
            match &entry.kind {
                RangeKind::Relation { oid, .. } => tables.push((*oid, rt_index)),
                _ if clause.of.is_empty() => {}
                _ => return Err(PgError::unimplemented("PLN-01", "FOR UPDATE/SHARE on a subquery or function in FROM")),
            }
        }
        if tables.is_empty() {
            continue;
        }
        let vars = plan.vars.clone();
        let out = plan.out.clone();
        let rels = plan.rels.clone();
        let level = plan.level;
        plan = LogicalPlan {
            node: LogicalNode::LockRows(Box::new(LogicalLockRows {
                input: plan,
                strength: clause.strength,
                wait: clause.wait,
                tables,
            })),
            vars,
            out,
            rels,
            level,
        };
    }
    Ok(plan)
}

/// The range entry for a 1-based range-table index.
fn entry_at(entries: &[RangeEntry], rt_index: u32) -> Result<&RangeEntry> {
    rt_index
        .checked_sub(1)
        .and_then(|i| entries.get(i as usize))
        .ok_or_else(|| internal(format!("range table entry {} does not exist", rt_index)))
}

/// `(alias, column names by attnum-1)` of every entry, for `PlannerState::enter_level`.
fn range_names(entries: &[RangeEntry]) -> Vec<(String, Vec<String>)> {
    entries
        .iter()
        .map(|e| {
            let columns: Vec<String> = e.columns.iter().map(|c| c.name.clone()).collect();
            (e.alias.clone(), columns)
        })
        .collect()
}

/// Output slots of a scan over a range entry: `Column{0, rt_index, i + 1}` for every column (the
/// binder caps tables at 1600 columns, so `i + 1` fits in an attnum).
fn column_vars(rt_index: u32, entry: &RangeEntry) -> (Vec<BoundExpr>, Vec<OutCol>) {
    let mut vars: Vec<BoundExpr> = Vec::with_capacity(entry.columns.len());
    let mut out: Vec<OutCol> = Vec::with_capacity(entry.columns.len());
    for (i, column) in entry.columns.iter().enumerate() {
        vars.push(BoundExpr::column(0, rt_index, (i + 1) as i16, column.ty));
        out.push(OutCol {
            name: column.name.clone(),
            ty: column.ty,
        });
    }
    (vars, out)
}

/// Output names and types of the query (`BoundQuery::output`).
fn output_cols(q: &BoundQuery) -> Vec<OutCol> {
    q.output
        .iter()
        .map(|c| OutCol {
            name: c.name.clone(),
            ty: c.ty,
        })
        .collect()
}

/// Simplify every cell of a VALUES list (rows after `simplify`).
fn simplify_rows(st: &mut PlannerState, rows: &[Vec<BoundExpr>]) -> Result<Vec<Vec<BoundExpr>>> {
    let mut out: Vec<Vec<BoundExpr>> = Vec::with_capacity(rows.len());
    for row in rows {
        let mut cells: Vec<BoundExpr> = Vec::with_capacity(row.len());
        for cell in row {
            cells.push(simplify::simplify_expr(st, cell.clone())?);
        }
        out.push(cells);
    }
    Ok(out)
}

/// Sorted union of two range-table index lists.
fn merge_rels(a: &[u32], b: &[u32]) -> Vec<u32> {
    let mut v: Vec<u32> = a.iter().chain(b.iter()).copied().collect();
    v.sort_unstable();
    v.dedup();
    v
}

/// True when some range entry under `item` is LATERAL (it may reference earlier FROM items).
fn item_is_lateral(entries: &[RangeEntry], item: &FromItem) -> bool {
    match item {
        FromItem::Range(rt_index) => match entry_at(entries, *rt_index) {
            Ok(entry) => entry.lateral,
            Err(_) => false,
        },
        FromItem::Join(join_item) => item_is_lateral(entries, &join_item.left) || item_is_lateral(entries, &join_item.right),
    }
}

/// Join node over two planned inputs: `vars` and `out` are `left ++ right`, `rels` the union.
fn make_join(
    st: &PlannerState,
    left: LogicalPlan,
    right: LogicalPlan,
    join_type: JoinType,
    quals: Vec<BoundExpr>,
    lateral: bool,
) -> LogicalPlan {
    let rels = merge_rels(&left.rels, &right.rels);
    let mut vars: Vec<BoundExpr> = left.vars.clone();
    vars.extend(right.vars.iter().cloned());
    let mut out: Vec<OutCol> = left.out.clone();
    out.extend(right.out.iter().cloned());
    let level = st.current_level();
    LogicalPlan {
        node: LogicalNode::Join(Box::new(LogicalJoin {
            join_type,
            left,
            right,
            quals,
            lateral,
        })),
        vars,
        out,
        rels,
        level,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::context::{PlanContext, PlanOptions};
    use pgarbor_catalog::model::RelKind;
    use pgarbor_catalog::testing;
    use pgarbor_sql::ir::{AccessNeeded, JoinItem, OutputColumn, RangeColumn, RangeFunction, TargetEntry};
    use std::collections::HashMap;

    fn int4_column(name: &str) -> RangeColumn {
        RangeColumn {
            name: name.to_string(),
            ty: TypeId::INT4,
            dropped: false,
        }
    }

    /// `alias` = a function scan `f() AS alias(col int4)`.
    fn function_entry(alias: &str, col: &str) -> RangeEntry {
        RangeEntry {
            kind: RangeKind::Function {
                funcs: vec![RangeFunction {
                    expr: BoundExpr::null(TypeId::INT4),
                    columns: vec![int4_column(col)],
                }],
                with_ordinality: false,
            },
            alias: alias.to_string(),
            columns: vec![int4_column(col)],
            lateral: false,
            visible: true,
            access: AccessNeeded::default(),
        }
    }

    #[test]
    fn one_row_scan_has_no_columns() -> Result<()> {
        let (_kv, env) = testing::bootstrapped()?;
        let ctx = PlanContext {
            env: &env,
            param_types: Vec::new(),
            param_values: None,
            options: PlanOptions::default(),
            table_rows: HashMap::new(),
        };
        let mut st = PlannerState::new(&ctx);
        let scan = LogicalScan {
            rt_index: 0,
            source: ScanSource::OneRow,
            quals: Vec::new(),
            with_rowid: false,
        };
        let plan = scan_to_plan(&mut st, &[], scan)?;
        assert!(plan.vars.is_empty());
        assert!(plan.out.is_empty());
        assert!(plan.rels.is_empty());
        assert!(matches!(plan.node, LogicalNode::Scan(_)));
        Ok(())
    }

    #[test]
    fn function_scan_exposes_entry_columns() -> Result<()> {
        let (_kv, env) = testing::bootstrapped()?;
        let ctx = PlanContext {
            env: &env,
            param_types: Vec::new(),
            param_values: None,
            options: PlanOptions::default(),
            table_rows: HashMap::new(),
        };
        let mut st = PlannerState::new(&ctx);
        let entries = vec![function_entry("g", "n")];
        let scan = build_range_scan(&mut st, &entries, 1, false)?;
        match &scan.source {
            ScanSource::Function { funcs, with_ordinality } => {
                assert_eq!(funcs.len(), 1);
                assert!(funcs[0].columns == vec![TypeId::INT4]);
                assert!(!*with_ordinality);
            }
            other => panic!("unexpected scan source: {other:?}"),
        }
        let plan = scan_to_plan(&mut st, &entries, scan)?;
        assert!(plan.vars == vec![BoundExpr::column(0, 1, 1, TypeId::INT4)]);
        assert_eq!(plan.rels, vec![1]);
        assert_eq!(plan.out.len(), 1);
        assert_eq!(plan.out[0].name, "n");
        Ok(())
    }

    #[test]
    fn explicit_inner_join_without_quals_keeps_both_sides() -> Result<()> {
        let (_kv, env) = testing::bootstrapped()?;
        let ctx = PlanContext {
            env: &env,
            param_types: Vec::new(),
            param_values: None,
            options: PlanOptions::default(),
            table_rows: HashMap::new(),
        };
        let mut st = PlannerState::new(&ctx);
        let entries = vec![function_entry("a", "x"), function_entry("b", "y")];
        let item = FromItem::Join(Box::new(JoinItem {
            kind: JoinKind::Inner,
            left: FromItem::Range(1),
            right: FromItem::Range(2),
            quals: None,
        }));
        let plan = build_from_item(&mut st, &entries, &item, &[])?;
        assert_eq!(plan.rels, vec![1, 2]);
        assert_eq!(plan.vars.len(), 2);
        assert_eq!(plan.level, st.current_level());
        match &plan.node {
            LogicalNode::Join(join) => {
                assert_eq!(join.join_type, JoinType::Inner);
                assert!(join.quals.is_empty());
                assert!(!join.lateral);
            }
            other => panic!("unexpected node: {other:?}"),
        }
        Ok(())
    }

    #[test]
    fn virtual_relation_is_not_a_dependency() -> Result<()> {
        let (_kv, env) = testing::bootstrapped()?;
        let ctx = PlanContext {
            env: &env,
            param_types: Vec::new(),
            param_values: None,
            options: PlanOptions::default(),
            table_rows: HashMap::new(),
        };
        let mut st = PlannerState::new(&ctx);
        let entries = vec![RangeEntry {
            kind: RangeKind::Relation {
                oid: 1259,
                schema: "pg_catalog".to_string(),
                name: "pg_class".to_string(),
                rel_kind: RelKind::Table,
                only: false,
                pseudo: None,
            },
            alias: "c".to_string(),
            columns: Vec::new(),
            lateral: false,
            visible: true,
            access: AccessNeeded::default(),
        }];
        let scan = build_range_scan(&mut st, &entries, 1, false)?;
        match &scan.source {
            ScanSource::Table { table, is_virtual } => {
                assert!(*is_virtual);
                assert_eq!(table.oid, 1259);
                assert_eq!(table.alias, "c");
            }
            other => panic!("unexpected scan source: {other:?}"),
        }
        assert!(st.dep_relations.is_empty());
        Ok(())
    }

    #[test]
    fn unknown_relation_is_an_error() -> Result<()> {
        let (_kv, env) = testing::bootstrapped()?;
        let ctx = PlanContext {
            env: &env,
            param_types: Vec::new(),
            param_values: None,
            options: PlanOptions::default(),
            table_rows: HashMap::new(),
        };
        let mut st = PlannerState::new(&ctx);
        let entries = vec![RangeEntry {
            kind: RangeKind::Relation {
                oid: 99_999,
                schema: "public".to_string(),
                name: "missing".to_string(),
                rel_kind: RelKind::Table,
                only: false,
                pseudo: None,
            },
            alias: "missing".to_string(),
            columns: Vec::new(),
            lateral: false,
            visible: true,
            access: AccessNeeded::default(),
        }];
        assert!(build_range_scan(&mut st, &entries, 1, false).is_err());
        Ok(())
    }

    #[test]
    #[ignore = "needs PLN-02 (simplify_expr) and PLN-09 (build_sort_limit_distinct)"]
    fn select_without_from_is_project_over_one_row() -> Result<()> {
        let (_kv, env) = testing::bootstrapped()?;
        let ctx = PlanContext {
            env: &env,
            param_types: Vec::new(),
            param_values: None,
            options: PlanOptions::default(),
            table_rows: HashMap::new(),
        };
        let mut st = PlannerState::new(&ctx);
        let sel = BoundSelect {
            target_list: vec![TargetEntry {
                expr: BoundExpr::bool_const(true),
                name: "bool".to_string(),
                junk: false,
                origin: None,
            }],
            ..BoundSelect::empty()
        };
        let q = BoundQuery {
            ctes: Vec::new(),
            body: QueryBody::Select(Box::new(sel)),
            output: vec![OutputColumn {
                name: "bool".to_string(),
                ty: TypeId::BOOL,
                origin: None,
            }],
            order_by: Vec::new(),
            limit: None,
            offset: None,
            limit_with_ties: false,
            locking: Vec::new(),
        };
        let plan = build_query(&mut st, &q)?;
        assert_eq!(plan.vars.len(), 1);
        match &plan.node {
            LogicalNode::Project(project) => match &project.input.node {
                LogicalNode::Scan(scan) => assert!(matches!(scan.source, ScanSource::OneRow)),
                other => panic!("unexpected input: {other:?}"),
            },
            other => panic!("unexpected node: {other:?}"),
        }
        Ok(())
    }
}
