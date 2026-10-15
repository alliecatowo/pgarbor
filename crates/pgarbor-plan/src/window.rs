//! Window function planning (PLN-08). PG references: `optimizer/plan/planner.c`
//! (`create_window_paths`, `select_active_windows`, `make_pathkeys_for_window`),
//! `plan/createplan.c` (`create_windowagg_plan`), `nodeWindowAgg.c` for semantics.
//!
//! ## Logical build (`build_windows`)
//! For each `WindowDef` in `sel.windows` (in index order) that has at least one `WindowFunc`
//! referencing it (`WindowFunc::window == index`) in the target list / ORDER BY expressions, stack a
//! [`LogicalWindow`] over the input: `partition_by`, `order_by` (from `WindowDef`), frame, name
//! (`WindowDef::name` or `w{index+1}`), `funcs` = the distinct `WindowFunc` expressions for that window.
//! Stacking order: windows with the same sort requirement adjacent; windows whose
//! (partition, order) is a prefix of another's come first (PG `select_active_windows` ordering) so a
//! single Sort can serve several windows.
//! Output `vars = input.vars ++ funcs`.
//!
//! ## Physical (`plan_window`)
//! * Required input order = `partition_by` (asc, nulls last) then `order_by`. If the child's `ordering`
//!   already satisfies it (prefix match incl. direction), no Sort; otherwise add `Sort`.
//! * Produce `WindowAggNode { partition_by, order_by, frame, window_name, funcs: WindowCall.. }`
//!   (`WindowFunc` -> `WindowCall`: copy name/args/filter/ignore_nulls/is_agg/star, `result_type`).
//! * `frame_is_default` frames are passed as the PG default (RANGE UNBOUNDED PRECEDING..CURRENT ROW with
//!   ORDER BY, ROWS whole partition otherwise); the executor derives behavior from `FrameSpec`.
//! * `ordering` of the node = its input order.
use pgarbor_common::{PgError, Result, SqlState};
use pgarbor_sql::ir::{BoundExpr, BoundSelect, ExprKind, FrameBound, FrameExclusion, FrameUnits, SortExpr, WindowDef, WindowFunc};

use crate::context::PlannerState;
use crate::logical::{LogicalNode, LogicalPlan, LogicalWindow};
use crate::plan::{FrameSpec, NodeKind, OutCol, PlanNode, SortKeySpec, SortNode, WindowAggNode, WindowCall};
use crate::{planner, sort_limit};

/// Sort key of a PARTITION BY expression: ascending, NULLS LAST (PG's partition clause defaults).
fn partition_key(expr: &BoundExpr) -> SortKeySpec {
    SortKeySpec { expr: expr.clone(), descending: false, nulls_first: false, collation: 0, using_op: None }
}

/// Executable sort key of a window ORDER BY item.
fn sort_expr_key(s: &SortExpr) -> SortKeySpec {
    SortKeySpec {
        expr: s.expr.clone(),
        descending: s.descending,
        nulls_first: s.nulls_first,
        collation: 0,
        using_op: s.using_op.clone(),
    }
}

/// ORDER BY keys of a window definition as sort keys.
fn order_keys(def: &WindowDef) -> Vec<SortKeySpec> {
    def.order_by.iter().map(sort_expr_key).collect()
}

/// Required input ordering of a window: the partition keys (ascending, NULLS LAST) followed by the
/// ORDER BY keys. An ORDER BY key that repeats a partition expression in the plain ascending form is
/// dropped (PG `list_concat_unique` in `select_active_windows`).
fn required_keys(partition_by: &[BoundExpr], order_by: &[SortKeySpec]) -> Vec<SortKeySpec> {
    let mut keys: Vec<SortKeySpec> = partition_by.iter().map(partition_key).collect();
    for k in order_by {
        let plain_asc = !k.descending && !k.nulls_first && k.using_op.is_none();
        if plain_asc && partition_by.iter().any(|p| *p == k.expr) {
            continue;
        }
        keys.push(k.clone());
    }
    keys
}

/// Two window definitions denote the same window clause (PG `findWindowClause`): equal partitioning,
/// ordering and frame; names must agree when both are given.
fn same_window(a: &WindowDef, b: &WindowDef) -> bool {
    let names_ok = match (&a.name, &b.name) {
        (Some(x), Some(y)) => x == y,
        _ => true,
    };
    names_ok
        && a.partition_by == b.partition_by
        && a.order_by == b.order_by
        && a.units == b.units
        && a.start == b.start
        && a.end == b.end
        && a.exclusion == b.exclusion
        && a.frame_is_default == b.frame_is_default
}

/// Frame of a window definition. A frame that was not written gets the PG default explicitly:
/// `RANGE UNBOUNDED PRECEDING AND CURRENT ROW` with ORDER BY, the whole partition otherwise.
fn frame_of(def: &WindowDef) -> (FrameUnits, FrameBound, FrameBound, FrameExclusion) {
    if !def.frame_is_default {
        return (def.units, def.start.clone(), def.end.clone(), def.exclusion);
    }
    if def.order_by.is_empty() {
        (FrameUnits::Rows, FrameBound::UnboundedPreceding, FrameBound::UnboundedFollowing, FrameExclusion::NoOthers)
    } else {
        (FrameUnits::Range, FrameBound::UnboundedPreceding, FrameBound::CurrentRow, FrameExclusion::NoOthers)
    }
}

/// The `WindowFunc` inside a window-function expression.
fn as_window_func(e: &BoundExpr) -> Result<&WindowFunc> {
    match &e.kind {
        ExprKind::WindowFunc(wf) => Ok(&**wf),
        _ => Err(PgError::internal("window planning: expression is not a window function call")),
    }
}

/// Error for `DISTINCT` inside a window function (PG parse_func.c).
fn distinct_error() -> PgError {
    PgError::new(SqlState::FEATURE_NOT_SUPPORTED, "DISTINCT is not implemented for window functions")
}

/// WindowFunc -> executable call descriptor.
fn window_call(f: &BoundExpr) -> Result<WindowCall> {
    let wf = as_window_func(f)?;
    if wf.distinct {
        return Err(distinct_error());
    }
    Ok(WindowCall {
        func: wf.func,
        name: wf.name.clone(),
        args: wf.args.clone(),
        is_agg: wf.is_agg,
        star: wf.star,
        filter: wf.filter.as_deref().cloned(),
        ignore_nulls: wf.ignore_nulls,
        result_type: f.ty,
        arg_types: wf.args.iter().map(|a| a.ty).collect(),
    })
}

/// Collect every `WindowFunc` node of an expression tree (sub-queries are opaque).
fn collect_window_calls<'a>(e: &'a BoundExpr, out: &mut Vec<&'a BoundExpr>) {
    e.walk(&mut |x| {
        if matches!(x.kind, ExprKind::WindowFunc(_)) {
            out.push(x);
        }
        true
    });
}

/// A window clause that is referenced by at least one window function.
struct Active<'a> {
    def: &'a WindowDef,
    /// Explicit name (WINDOW clause name), if any.
    name: Option<String>,
    /// Distinct window function expressions using this clause.
    funcs: Vec<BoundExpr>,
    /// Required input ordering (`required_keys`).
    req: Vec<SortKeySpec>,
}

/// Stack windows over `input` (module docs).
pub fn build_windows(st: &mut PlannerState, input: LogicalPlan, sel: &BoundSelect) -> Result<LogicalPlan> {
    // Naming and ordering depend only on the select block, not on planner state.
    let _ = st;

    // 1. Window function calls of the target list, grouped by the window definition they use.
    let mut calls: Vec<&BoundExpr> = Vec::new();
    for te in &sel.target_list {
        collect_window_calls(&te.expr, &mut calls);
    }
    let mut per_window: Vec<Vec<BoundExpr>> = vec![Vec::new(); sel.windows.len()];
    for call in calls {
        let wf = as_window_func(call)?;
        if wf.distinct {
            return Err(distinct_error());
        }
        let Some(slot) = per_window.get_mut(wf.window) else {
            return Err(PgError::internal("window function refers to an unknown window definition"));
        };
        if !slot.contains(call) {
            slot.push(call.clone());
        }
    }

    // 2. Active windows; identical window clauses share one entry (one WindowAgg).
    let mut active = Vec::new();
    for (def, funcs) in sel.windows.iter().zip(per_window) {
        if funcs.is_empty() {
            continue;
        }
        let pos = active.iter().position(|a: &Active| same_window(a.def, def));
        match pos {
            Some(i) => {
                let a = &mut active[i];
                if a.name.is_none() {
                    a.name = def.name.clone();
                }
                for f in funcs {
                    if !a.funcs.contains(&f) {
                        a.funcs.push(f);
                    }
                }
            }
            None => {
                let req = required_keys(&def.partition_by, &order_keys(def));
                active.push(Active { def, name: def.name.clone(), funcs, req });
            }
        }
    }

    // 3. Order (PG common_prefix_cmp): the window with more sort keys goes first, so that the
    // following windows whose requirement is a prefix of it need no further Sort. Windows with the
    // same requirement are kept adjacent; the sort is stable so the original order is kept otherwise.
    active.sort_by(|a, b| b.req.len().cmp(&a.req.len()));
    let mut groups: Vec<Vec<Active>> = Vec::new();
    for a in active {
        let pos = groups.iter().position(|g| g.first().is_some_and(|f| f.req == a.req));
        match pos {
            Some(i) => groups[i].push(a),
            None => groups.push(vec![a]),
        }
    }
    let ordered: Vec<Active> = groups.into_iter().flatten().collect();

    // 4. Names (PG name_active_windows): explicit names kept; the others get `w<n>`, skipping names
    // already in use among the active windows.
    let mut used: Vec<String> = ordered.iter().filter_map(|a| a.name.clone()).collect();
    let mut names: Vec<String> = Vec::with_capacity(ordered.len());
    let mut next_n: usize = 1;
    for a in &ordered {
        if let Some(n) = &a.name {
            names.push(n.clone());
            continue;
        }
        let name = loop {
            let cand = format!("w{next_n}");
            next_n += 1;
            if !used.contains(&cand) {
                break cand;
            }
        };
        used.push(name.clone());
        names.push(name);
    }

    // 5. Stack the LogicalWindow nodes over the input, innermost first.
    let mut plan = input;
    for (a, name) in ordered.into_iter().zip(names) {
        let mut out = plan.out.clone();
        let mut vars = plan.vars.clone();
        for f in &a.funcs {
            let wf = as_window_func(f)?;
            out.push(OutCol { name: wf.name.clone(), ty: f.ty });
            vars.push(f.clone());
        }
        let (units, start, end, exclusion) = frame_of(a.def);
        let rels = plan.rels.clone();
        let level = plan.level;
        let lw = LogicalWindow {
            input: plan,
            name,
            partition_by: a.def.partition_by.clone(),
            order_by: order_keys(a.def),
            units,
            start,
            end,
            exclusion,
            funcs: a.funcs,
        };
        plan = LogicalPlan { node: LogicalNode::Window(Box::new(lw)), vars, out, rels, level };
    }
    Ok(plan)
}

/// Physical plan of a `LogicalWindow`.
pub fn plan_window(st: &mut PlannerState, w: &LogicalWindow) -> Result<PlanNode> {
    let child = planner::plan_logical(st, &w.input)?;

    // Required input order: partition keys then ORDER BY keys. Sort only when the child does not
    // already provide it.
    let required = required_keys(&w.partition_by, &w.order_by);
    let input = if sort_limit::ordering_satisfies(&child.ordering, &required)? {
        child
    } else {
        let out = child.out.clone();
        let vars = child.vars.clone();
        let mut sort = st.new_node(
            NodeKind::Sort(Box::new(SortNode { input: child, keys: required.clone(), limit: None })),
            out,
        );
        sort.vars = vars;
        sort.ordering = required;
        sort
    };

    let mut calls = Vec::with_capacity(w.funcs.len());
    for f in &w.funcs {
        calls.push(window_call(f)?);
    }
    let mut out = input.out.clone();
    for f in &w.funcs {
        let wf = as_window_func(f)?;
        out.push(OutCol { name: wf.name.clone(), ty: f.ty });
    }
    let mut vars = input.vars.clone();
    vars.extend(w.funcs.iter().cloned());
    let ordering = input.ordering.clone();

    let node = WindowAggNode {
        input,
        partition_by: w.partition_by.clone(),
        order_by: w.order_by.clone(),
        frame: FrameSpec { units: w.units, start: w.start.clone(), end: w.end.clone(), exclusion: w.exclusion },
        window_name: w.name.clone(),
        funcs: calls,
    };
    let mut result = st.new_node(NodeKind::WindowAgg(Box::new(node)), out);
    result.vars = vars;
    result.ordering = ordering;
    Ok(result)
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use pgarbor_catalog::testing;
    use pgarbor_sql::ir::{DistinctSpec, GroupBy, TargetEntry};
    use pgarbor_types::TypeId;

    use super::*;
    use crate::context::{PlanContext, PlanOptions};
    use crate::logical::{LogicalScan, ScanSource};
    use crate::plan::TableRef;

    fn col(attnum: i16) -> BoundExpr {
        BoundExpr::column(0, 1, attnum, TypeId::INT4)
    }

    fn sort_key(expr: BoundExpr) -> SortExpr {
        SortExpr { expr, descending: false, nulls_first: false, using_op: None }
    }

    fn key(expr: BoundExpr) -> SortKeySpec {
        sort_expr_key(&sort_key(expr))
    }

    /// `row_number() OVER <window>` with the given window index.
    fn row_number(window: usize) -> BoundExpr {
        let wf = WindowFunc {
            func: 3100,
            name: "row_number".to_string(),
            args: Vec::new(),
            is_agg: false,
            star: false,
            distinct: false,
            filter: None,
            ignore_nulls: false,
            window,
        };
        BoundExpr::new(ExprKind::WindowFunc(Box::new(wf)), TypeId::INT8)
    }

    fn window_def(partition: Vec<BoundExpr>, order: Vec<BoundExpr>) -> WindowDef {
        WindowDef {
            name: None,
            partition_by: partition,
            order_by: order.into_iter().map(sort_key).collect(),
            units: FrameUnits::Range,
            start: FrameBound::UnboundedPreceding,
            end: FrameBound::CurrentRow,
            exclusion: FrameExclusion::NoOthers,
            frame_is_default: true,
        }
    }

    /// Select block with two plain columns followed by one target entry per window call.
    fn select(windows: Vec<WindowDef>, calls: Vec<BoundExpr>) -> BoundSelect {
        let mut target_list: Vec<TargetEntry> = [1i16, 2]
            .iter()
            .map(|&i| TargetEntry { expr: col(i), name: format!("c{i}"), junk: false, origin: None })
            .collect();
        for (i, e) in calls.into_iter().enumerate() {
            target_list.push(TargetEntry { expr: e, name: format!("rn{i}"), junk: false, origin: None });
        }
        BoundSelect {
            range_table: Vec::new(),
            from: Vec::new(),
            where_: None,
            target_list,
            group_by: GroupBy::default(),
            having: None,
            distinct: DistinctSpec::None,
            windows,
            has_aggs: false,
            has_windows: true,
            has_target_srfs: false,
            has_sublinks: false,
        }
    }

    /// Plain table scan with `ncols` int4 columns (range table entry 1).
    fn scan(ncols: usize) -> LogicalPlan {
        let table = TableRef {
            oid: 16384,
            schema: "public".to_string(),
            name: "t".to_string(),
            alias: "t".to_string(),
            ncols: ncols as u32,
            col_types: vec![TypeId::INT4; ncols],
        };
        let vars: Vec<BoundExpr> = (1..=ncols).map(|i| col(i as i16)).collect();
        let out: Vec<OutCol> = (1..=ncols).map(|i| OutCol { name: format!("c{i}"), ty: TypeId::INT4 }).collect();
        let scan = LogicalScan {
            rt_index: 1,
            source: ScanSource::Table { table, is_virtual: false },
            quals: Vec::new(),
            with_rowid: false,
        };
        LogicalPlan {
            node: LogicalNode::Scan(Box::new(scan)),
            vars,
            out,
            rels: vec![1],
            level: 0,
        }
    }

    #[test]
    fn required_keys_partition_then_order() {
        let keys = required_keys(&[col(2)], &[key(col(1))]);
        assert_eq!(keys.len(), 2);
        assert_eq!(keys[0].expr, col(2));
        assert!(!keys[0].descending && !keys[0].nulls_first);
        assert_eq!(keys[1].expr, col(1));
    }

    #[test]
    fn required_keys_drops_plain_order_key_repeating_partition() {
        let keys = required_keys(&[col(1)], &[key(col(1))]);
        assert_eq!(keys.len(), 1);
        assert_eq!(keys[0].expr, col(1));
    }

    #[test]
    fn required_keys_keeps_descending_repeat() {
        let desc = SortExpr { expr: col(1), descending: true, nulls_first: false, using_op: None };
        let keys = required_keys(&[col(1)], &[sort_expr_key(&desc)]);
        assert_eq!(keys.len(), 2);
        assert!(keys[1].descending);
    }

    #[test]
    fn build_single_window_over_partition_and_order() -> Result<()> {
        let (_kv, env) = testing::bootstrapped()?;
        let ctx = PlanContext {
            env: &env,
            param_types: Vec::new(),
            param_values: None,
            options: PlanOptions::default(),
            table_rows: HashMap::new(),
        };
        let mut st = PlannerState::new(&ctx);
        let sel = select(vec![window_def(vec![col(2)], vec![col(1)])], vec![row_number(0)]);

        let lp = build_windows(&mut st, scan(2), &sel)?;
        let LogicalNode::Window(w) = &lp.node else {
            return Err(PgError::internal("expected a window node"));
        };
        assert_eq!(w.funcs.len(), 1);
        assert_eq!(w.funcs[0], row_number(0));
        assert_eq!(w.name, "w1");
        assert_eq!(w.partition_by, vec![col(2)]);
        assert_eq!(w.order_by.len(), 1);
        assert_eq!(w.units, FrameUnits::Range);
        assert_eq!(lp.width(), 3);
        assert_eq!(lp.out[2].name, "row_number");
        assert_eq!(lp.vars[2], row_number(0));
        assert!(matches!(w.input.node, LogicalNode::Scan(_)));
        Ok(())
    }

    #[test]
    fn identical_window_definitions_share_one_window() -> Result<()> {
        let (_kv, env) = testing::bootstrapped()?;
        let ctx = PlanContext {
            env: &env,
            param_types: Vec::new(),
            param_values: None,
            options: PlanOptions::default(),
            table_rows: HashMap::new(),
        };
        let mut st = PlannerState::new(&ctx);
        let sel = select(
            vec![window_def(vec![col(2)], vec![col(1)]), window_def(vec![col(2)], vec![col(1)])],
            vec![row_number(0), row_number(1)],
        );

        let lp = build_windows(&mut st, scan(2), &sel)?;
        let LogicalNode::Window(w) = &lp.node else {
            return Err(PgError::internal("expected a window node"));
        };
        assert_eq!(w.funcs.len(), 2);
        assert!(matches!(w.input.node, LogicalNode::Scan(_)));
        assert_eq!(lp.width(), 4);
        Ok(())
    }

    #[test]
    fn longer_requirement_is_stacked_first() -> Result<()> {
        let (_kv, env) = testing::bootstrapped()?;
        let ctx = PlanContext {
            env: &env,
            param_types: Vec::new(),
            param_values: None,
            options: PlanOptions::default(),
            table_rows: HashMap::new(),
        };
        let mut st = PlannerState::new(&ctx);
        // Window 0: PARTITION BY b. Window 1: PARTITION BY b ORDER BY a (more keys, goes first).
        let sel = select(
            vec![window_def(vec![col(2)], Vec::new()), window_def(vec![col(2)], vec![col(1)])],
            vec![row_number(0), row_number(1)],
        );

        let lp = build_windows(&mut st, scan(2), &sel)?;
        let LogicalNode::Window(top) = &lp.node else {
            return Err(PgError::internal("expected a window node"));
        };
        assert!(top.order_by.is_empty());
        assert_eq!(top.name, "w2");
        assert_eq!(top.funcs, vec![row_number(0)]);
        let LogicalNode::Window(inner) = &top.input.node else {
            return Err(PgError::internal("expected an inner window node"));
        };
        assert_eq!(inner.name, "w1");
        assert_eq!(inner.funcs, vec![row_number(1)]);
        Ok(())
    }

    #[test]
    fn no_window_functions_leaves_input_unchanged() -> Result<()> {
        let (_kv, env) = testing::bootstrapped()?;
        let ctx = PlanContext {
            env: &env,
            param_types: Vec::new(),
            param_values: None,
            options: PlanOptions::default(),
            table_rows: HashMap::new(),
        };
        let mut st = PlannerState::new(&ctx);
        let sel = select(vec![window_def(vec![col(2)], vec![col(1)])], Vec::new());

        let lp = build_windows(&mut st, scan(2), &sel)?;
        assert_eq!(lp, scan(2));
        Ok(())
    }

    #[test]
    fn distinct_window_function_is_rejected() -> Result<()> {
        let (_kv, env) = testing::bootstrapped()?;
        let ctx = PlanContext {
            env: &env,
            param_types: Vec::new(),
            param_values: None,
            options: PlanOptions::default(),
            table_rows: HashMap::new(),
        };
        let mut st = PlannerState::new(&ctx);
        let mut bad = row_number(0);
        if let ExprKind::WindowFunc(wf) = &mut bad.kind {
            wf.distinct = true;
        }
        let sel = select(vec![window_def(vec![col(2)], Vec::new())], vec![bad]);

        match build_windows(&mut st, scan(2), &sel) {
            Ok(_) => Err(PgError::internal("DISTINCT window function was accepted")),
            Err(e) => {
                assert_eq!(e.sqlstate.code(), "0A000");
                assert_eq!(e.message, "DISTINCT is not implemented for window functions");
                Ok(())
            }
        }
    }

    #[test]
    #[ignore = "needs PLN-09 (ordering_satisfies) and PLN-13 (plan_logical)"]
    fn plan_window_adds_sort_for_unordered_child() -> Result<()> {
        let (_kv, env) = testing::bootstrapped()?;
        let ctx = PlanContext {
            env: &env,
            param_types: Vec::new(),
            param_values: None,
            options: PlanOptions::default(),
            table_rows: HashMap::new(),
        };
        let mut st = PlannerState::new(&ctx);
        let sel = select(vec![window_def(vec![col(2)], vec![col(1)])], vec![row_number(0)]);

        let lp = build_windows(&mut st, scan(2), &sel)?;
        let LogicalNode::Window(w) = &lp.node else {
            return Err(PgError::internal("expected a window node"));
        };
        let node = plan_window(&mut st, w)?;
        assert!(matches!(node.kind, NodeKind::WindowAgg(_)));
        assert_eq!(node.width(), 3);
        assert_eq!(node.vars.len(), 3);
        let kids = node.children();
        assert_eq!(kids.len(), 1);
        assert!(matches!(kids[0].kind, NodeKind::Sort(_)));
        Ok(())
    }
}
