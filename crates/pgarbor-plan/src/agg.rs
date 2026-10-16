//! Aggregation planning (PLN-07). PG references: `optimizer/plan/planner.c`
//! (`create_grouping_paths`, `consider_groupingsets_paths`, `make_partial_grouping_target`),
//! `plan/createplan.c` (`create_agg_plan`, `create_groupingsets_plan`), `util/clauses.c`
//! (`contain_agg_clause`), `plan/setrefs.c` (`set_upper_references` for aggregates),
//! `parse_agg.c`.
//!
//! ## Logical build (`build_aggregate`)
//! Called by `build::build_select` when `sel.has_aggs` or GROUP BY / HAVING is present:
//! * `group_exprs` = the grouping expressions: `sel.target_list[i].expr` for `i in group_by.exprs`
//!   (in that order); `grouping_sets` = `group_by.sets`.
//! * `aggrefs` = every distinct (structurally equal) `Aggref` with `levels_up == 0` found in the target
//!   list, HAVING and window definitions' expressions ([`collect_aggrefs`]).
//! * `having` = HAVING conjuncts; conjuncts of HAVING that reference NO aggregate and only grouping
//!   expressions are moved BELOW the aggregate as filters on the input (PG does this; result-equivalent
//!   for plain GROUP BY, NOT for grouping sets or `Plain` aggregation without GROUP BY... only when
//!   there is a GROUP BY and no grouping sets).
//! * Output `vars = group_exprs ++ aggrefs` (+ `Const` mask placeholder `Column`-free var for the mask
//!   when grouping sets: a `Grouping`-less `Const(Int8(0))` marker is NOT used: setrefs finds the mask
//!   slot by position `group_exprs.len() + aggrefs.len()` from `AggregateNode`).
//! * The query's target list is evaluated above (`Project`), where `Aggref` nodes and group-expression
//!   subtrees are matched to aggregate output slots by `setrefs`.
//!
//! ## Physical choice (`plan_aggregate`)
//! * No `group_exprs` and no sets -> `AggStrategy::Plain`.
//! * Otherwise `Hashed` if `enable_hashagg`, every group key type is hashable (`compare::hash_datum`
//!   supports it) and there are no sets requiring sorted input; `Sorted` if the input is already ordered
//!   by the group keys (`PlanNode::ordering` prefix match), or hashing is disabled/impossible (insert a
//!   `Sort` on the keys; EXPLAIN shows `GroupAggregate`). Grouping sets: `Hashed`.
//! * `AggCall` construction from each `Aggref`: copy args/filter/order/distinct/star, `result_type = ty`,
//!   `arg_types` from the args.
//! * `Plain` aggregation over an empty input still yields one row; planner must not add a one-time
//!   false filter below it. A `Plain` aggregate with `LIMIT 0`... normal.
//! * Output `out`: names `group key` / aggregate names; estimates: groups = min(input rows,
//!   `cost::estimate_groups`).
//! * `ordering`: `Sorted` -> the group keys; `Hashed` -> none.
use pgarbor_common::{PgError, Result, SqlState};
use pgarbor_sql::ir::{BoundExpr, BoundSelect, ColumnRef, ExprKind, SortExpr};
use pgarbor_types::{compare, TypeEnv, TypeId};

use crate::context::PlannerState;
use crate::cost;
use crate::logical::{LogicalAggregate, LogicalFilter, LogicalNode, LogicalPlan};
use crate::plan::{AggCall, AggStrategy, AggregateNode, NodeKind, OutCol, PlanNode, SortKeySpec, SortNode};
use crate::planner::plan_logical;
use crate::pushdown;
use crate::simplify::{make_and, split_conjuncts};

/// Distinct `Aggref` (level 0) expressions found in `exprs`, in first-appearance order.
pub fn collect_aggrefs(exprs: &[&BoundExpr]) -> Result<Vec<BoundExpr>> {
    let mut found: Vec<BoundExpr> = Vec::new();
    for e in exprs {
        let root: &BoundExpr = e;
        root.walk(&mut |n: &BoundExpr| match &n.kind {
            ExprKind::Aggref(a) => {
                // Only aggregates evaluated at this level; an aggregate's own arguments are never
                // scanned for nested aggregates (the binder rejects those).
                if a.levels_up == 0 && !found.contains(n) {
                    found.push(n.clone());
                }
                false
            }
            _ => true,
        });
    }
    Ok(found)
}

/// Wrap `input` in a `LogicalAggregate` for `sel` (module docs "Logical build").
pub fn build_aggregate(st: &mut PlannerState, input: LogicalPlan, sel: &BoundSelect) -> Result<LogicalPlan> {
    let mut group_exprs: Vec<BoundExpr> = Vec::with_capacity(sel.group_by.exprs.len());
    for &idx in &sel.group_by.exprs {
        let entry = sel
            .target_list
            .get(idx)
            .ok_or_else(|| PgError::internal("GROUP BY refers to a missing target list entry"))?;
        group_exprs.push(entry.expr.clone());
    }

    let mut exprs: Vec<&BoundExpr> = Vec::new();
    for t in &sel.target_list {
        exprs.push(&t.expr);
    }
    if let Some(h) = &sel.having {
        exprs.push(h);
    }
    for w in &sel.windows {
        exprs.extend(w.partition_by.iter());
        exprs.extend(w.order_by.iter().map(|k| &k.expr));
    }
    let aggrefs = collect_aggrefs(&exprs)?;

    // Columns referenced outside aggregates that are not grouping expressions were accepted by the
    // binder because they are functionally dependent on the grouping columns (primary key). PG
    // carries them through the Agg node; here they become extra grouping keys, which yields the
    // same value (one distinct value per group). Binder validation requires the determining
    // primary key in every grouping set, so dependent columns belong to every set as well.
    let mut grouping_sets = sel.group_by.sets.clone();
    {
        let mut extra: Vec<BoundExpr> = Vec::new();
        for e in &exprs {
            let root: &BoundExpr = e;
            root.walk(&mut |n: &BoundExpr| {
                if matches!(n.kind, ExprKind::Aggref(_)) || group_exprs.contains(n) {
                    return false;
                }
                if let ExprKind::Column(c) = &n.kind {
                    if c.levels_up == 0 && !extra.contains(n) {
                        extra.push(n.clone());
                    }
                }
                true
            });
        }
        if let Some(sets) = &mut grouping_sets {
            for set in sets {
                set.extend(group_exprs.len()..group_exprs.len() + extra.len());
            }
        }
        group_exprs.extend(extra);
    }

    // HAVING: split into conjuncts and move the aggregate-free ones below the aggregate.
    let plain_group = sel.group_by.sets.is_none();
    let having_conjuncts: Vec<BoundExpr> = match &sel.having {
        Some(h) => split_conjuncts(h.clone()),
        None => Vec::new(),
    };
    let mut having: Vec<BoundExpr> = Vec::new();
    let mut below: Vec<BoundExpr> = Vec::new();
    for c in having_conjuncts {
        if can_push_below(&c, &group_exprs, plain_group) {
            below.push(c);
        } else {
            having.push(c);
        }
    }

    let input = if below.is_empty() {
        input
    } else {
        let vars = input.vars.clone();
        let out = input.out.clone();
        let rels = input.rels.clone();
        let level = input.level;
        LogicalPlan {
            node: LogicalNode::Filter(Box::new(LogicalFilter { input, quals: below })),
            vars,
            out,
            rels,
            level,
        }
    };

    let level = input.level;
    let mut vars: Vec<BoundExpr> = group_exprs.clone();
    vars.extend(aggrefs.iter().cloned());
    let out = output_cols(st, level, &group_exprs, &aggrefs, false);
    // Plain column group keys are exposed directly by the aggregate's output.
    let mut rels: Vec<u32> = Vec::new();
    for g in &group_exprs {
        if let ExprKind::Column(c) = &g.kind {
            if c.levels_up == 0 {
                rels.push(c.rt_index);
            }
        }
    }
    rels.sort_unstable();
    rels.dedup();

    let agg = LogicalAggregate {
        input,
        group_exprs,
        grouping_sets,
        aggrefs,
        having,
    };
    Ok(LogicalPlan {
        node: LogicalNode::Aggregate(Box::new(agg)),
        vars,
        out,
        rels,
        level,
    })
}

/// Physical plan of a `LogicalAggregate` (module docs "Physical choice").
pub fn plan_aggregate(st: &mut PlannerState, a: &LogicalAggregate) -> Result<PlanNode> {
    let level = st.cur_level;
    let n = a.group_exprs.len();
    let aggs: Vec<AggCall> = a.aggrefs.iter().map(agg_call).collect::<Result<Vec<AggCall>>>()?;
    let child = plan_logical(st, &a.input)?;

    let mut vars: Vec<BoundExpr> = a.group_exprs.clone();
    vars.extend(a.aggrefs.iter().cloned());
    let out = output_cols(st, level, &a.group_exprs, &a.aggrefs, a.grouping_sets.is_some());

    let (strategy, input, ordering): (AggStrategy, PlanNode, Vec<SortKeySpec>) = if n == 0 {
        (AggStrategy::Plain, child, Vec::new())
    } else {
        let env: &dyn TypeEnv = st.ctx.env;
        for g in &a.group_exprs {
            if !compare::is_orderable(env, g.ty)? {
                return Err(no_equality_error(env, g.ty));
            }
        }
        if a.grouping_sets.is_some() {
            // Grouping sets are always hashed (one pass per set over the hashed input).
            (AggStrategy::Hashed, child, Vec::new())
        } else if ordering_covers(&child.ordering, &a.group_exprs) {
            // Input already grouped: consecutive groups, no Sort node.
            let ord: Vec<SortKeySpec> = child.ordering.iter().take(n).cloned().collect();
            (AggStrategy::Sorted, child, ord)
        } else if st.ctx.options.enable_hashagg {
            (AggStrategy::Hashed, child, Vec::new())
        } else {
            let keys = group_sort_keys(&a.group_exprs);
            let sorted = insert_sort(st, child, keys.clone())?;
            (AggStrategy::Sorted, sorted, keys)
        }
    };

    let input_rows = input.est.rows;
    let sets_count = a.grouping_sets.as_ref().map_or(1, |s| s.len().max(1)) as f64;
    let rows = if n == 0 {
        sets_count
    } else {
        let groups = cost::estimate_groups(input_rows, n)?.min(input_rows).max(0.0);
        groups * sets_count
    };
    let shape = match strategy {
        AggStrategy::Hashed => "hashagg",
        AggStrategy::Plain | AggStrategy::Sorted => "groupagg",
    };
    let est = cost::cost_node(shape, rows, input.est.width, &[input.est])?;

    let kind = NodeKind::Aggregate(Box::new(AggregateNode {
        strategy,
        input,
        group_keys: a.group_exprs.clone(),
        aggs,
        grouping_sets: a.grouping_sets.clone(),
        having: make_and(a.having.clone()),
    }));
    let mut node = st.new_node(kind, out);
    node.vars = vars;
    node.est = est;
    node.ordering = ordering;
    Ok(node)
}

// ---------------------------------------------------------------------------------------------
// Private helpers
// ---------------------------------------------------------------------------------------------

/// Can this HAVING conjunct be evaluated on the input rows? It must contain no aggregate of this
/// level, no window function, grouping call, sub-link or volatile call, and every local column must be
/// a plain grouping column. Only plain GROUP BY (no grouping sets, at least one key) qualifies.
fn can_push_below(c: &BoundExpr, group_exprs: &[BoundExpr], plain_group: bool) -> bool {
    if group_exprs.is_empty() || !plain_group {
        return false;
    }
    let barrier = c.any(&mut |e: &BoundExpr| match &e.kind {
        ExprKind::Aggref(a) => a.levels_up == 0,
        ExprKind::WindowFunc(_) | ExprKind::Grouping(_) | ExprKind::SubLink(_) | ExprKind::SubPlan(_) => true,
        _ => false,
    });
    if barrier || pushdown::is_volatile(c) {
        return false;
    }
    let mut cols: Vec<ColumnRef> = Vec::new();
    c.collect_columns(0, &mut cols);
    cols.iter().all(|col| group_exprs.iter().any(|g| is_same_column(g, col)))
}

fn is_same_column(g: &BoundExpr, col: &ColumnRef) -> bool {
    matches!(&g.kind, ExprKind::Column(gc) if gc.levels_up == 0 && gc.rt_index == col.rt_index && gc.attnum == col.attnum)
}

/// Output slot names and types of the aggregate: group keys, aggregates, and (grouping sets only)
/// the grouping mask. Group keys that are plain columns take the column name, other expressions
/// `?column?` (PG naming); aggregates take the aggregate function name.
fn output_cols(st: &PlannerState, level: u32, group_exprs: &[BoundExpr], aggrefs: &[BoundExpr], sets: bool) -> Vec<OutCol> {
    let mut out: Vec<OutCol> = Vec::with_capacity(group_exprs.len() + aggrefs.len() + 1);
    for g in group_exprs {
        out.push(OutCol { name: column_name(st, level, g), ty: g.ty });
    }
    for e in aggrefs {
        let name = match &e.kind {
            ExprKind::Aggref(a) => a.name.clone(),
            _ => "?column?".to_string(),
        };
        out.push(OutCol { name, ty: e.ty });
    }
    if sets {
        out.push(OutCol { name: "grouping".to_string(), ty: TypeId::INT8 });
    }
    out
}

/// Name of a plain local column from the range table of `level`; `?column?` otherwise.
fn column_name(st: &PlannerState, level: u32, e: &BoundExpr) -> String {
    let c: ColumnRef = match &e.kind {
        ExprKind::Column(c) if c.levels_up == 0 => *c,
        _ => return "?column?".to_string(),
    };
    let entry = c
        .rt_index
        .checked_sub(1)
        .and_then(|i| st.range_names.get(level as usize).and_then(|names| names.get(i as usize)));
    let Some((alias, cols)) = entry else {
        return "?column?".to_string();
    };
    if c.attnum == 0 {
        return alias.clone();
    }
    if c.attnum > 0 {
        if let Some(name) = cols.get((c.attnum as usize) - 1) {
            return name.clone();
        }
    }
    "?column?".to_string()
}

/// Structural `AggCall` of one distinct aggregate expression.
fn agg_call(e: &BoundExpr) -> Result<AggCall> {
    match &e.kind {
        ExprKind::Aggref(a) => Ok(AggCall {
            agg: a.agg,
            name: a.name.clone(),
            args: a.args.clone(),
            direct_args: a.direct_args.clone(),
            distinct: a.distinct,
            filter: a.filter.as_deref().cloned(),
            order_by: a.order_by.iter().map(sort_expr_spec).collect(),
            star: a.star,
            result_type: e.ty,
            arg_types: a.args.iter().map(|x| x.ty).collect(),
        }),
        _ => Err(PgError::internal("aggregate list contains a non-aggregate expression")),
    }
}

fn sort_expr_spec(s: &SortExpr) -> SortKeySpec {
    SortKeySpec {
        expr: s.expr.clone(),
        descending: s.descending,
        nulls_first: s.nulls_first,
        collation: 0,
        using_op: s.using_op.clone(),
    }
}

/// Ascending sort keys over the grouping expressions (default NULL placement).
fn group_sort_keys(groups: &[BoundExpr]) -> Vec<SortKeySpec> {
    groups
        .iter()
        .map(|g| SortKeySpec { expr: g.clone(), descending: false, nulls_first: false, collation: 0, using_op: None })
        .collect()
}

/// True when the first `groups.len()` ordering keys are exactly the grouping expressions (in any
/// order, each once): the input then delivers every group as one consecutive run.
fn ordering_covers(ordering: &[SortKeySpec], groups: &[BoundExpr]) -> bool {
    if groups.is_empty() || ordering.len() < groups.len() {
        return false;
    }
    let mut used = vec![false; groups.len()];
    for key in ordering.iter().take(groups.len()) {
        let found = groups
            .iter()
            .enumerate()
            .position(|(j, g)| !used[j] && *g == key.expr);
        match found {
            Some(j) => used[j] = true,
            None => return false,
        }
    }
    true
}

/// Put a `Sort` node on top of `child` (its vars and output are unchanged).
fn insert_sort(st: &PlannerState, child: PlanNode, keys: Vec<SortKeySpec>) -> Result<PlanNode> {
    let out = child.out.clone();
    let vars = child.vars.clone();
    let est = cost::cost_node("sort", child.est.rows, child.est.width, &[child.est])?;
    let kind = NodeKind::Sort(Box::new(SortNode { input: child, keys: keys.clone(), limit: None }));
    let mut node = st.new_node(kind, out);
    node.vars = vars;
    node.est = est;
    node.ordering = keys;
    Ok(node)
}

/// PG error for a grouping key whose type has no equality/ordering support.
fn no_equality_error(env: &dyn TypeEnv, ty: TypeId) -> PgError {
    let name = match env.type_info(ty.oid) {
        Some(info) => info.name.to_string(),
        None => ty.oid.to_string(),
    };
    PgError::new(SqlState::UNDEFINED_FUNCTION, format!("could not identify an equality operator for type {name}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    use pgarbor_catalog::testing;
    use pgarbor_sql::ir::{Aggref, BoundSelect, DistinctSpec, GroupBy, TargetEntry};
    use pgarbor_types::catalog_data::AggKind;

    use crate::context::{PlanContext, PlanOptions};
    use crate::logical::{LogicalNode, LogicalScan, ScanSource};
    use crate::plan::OutCol;

    fn col(attnum: i16) -> BoundExpr {
        BoundExpr::column(0, 1, attnum, TypeId::INT4)
    }

    fn agg(name: &str, args: Vec<BoundExpr>, star: bool) -> BoundExpr {
        BoundExpr::new(
            ExprKind::Aggref(Box::new(Aggref {
                agg: 2147,
                name: name.to_string(),
                args,
                direct_args: Vec::new(),
                kind: AggKind::Normal,
                distinct: false,
                filter: None,
                order_by: Vec::new(),
                star,
                variadic: false,
                levels_up: 0,
            })),
            TypeId::INT8,
        )
    }

    fn names() -> Vec<(String, Vec<String>)> {
        vec![(
            "t".to_string(),
            vec!["a".to_string(), "b".to_string(), "c".to_string()],
        )]
    }

    fn input_plan() -> LogicalPlan {
        let out = vec![
            OutCol { name: "a".to_string(), ty: TypeId::INT4 },
            OutCol { name: "b".to_string(), ty: TypeId::INT4 },
            OutCol { name: "c".to_string(), ty: TypeId::INT4 },
        ];
        LogicalPlan {
            node: LogicalNode::Scan(Box::new(LogicalScan {
                rt_index: 1,
                source: ScanSource::Values { rows: Vec::new() },
                quals: Vec::new(),
                with_rowid: false,
            })),
            vars: vec![col(1), col(2), col(3)],
            out,
            rels: vec![1],
            level: 0,
        }
    }

    fn select(targets: Vec<BoundExpr>, group: Vec<usize>, having: Option<BoundExpr>) -> BoundSelect {
        BoundSelect {
            range_table: Vec::new(),
            from: Vec::new(),
            where_: None,
            target_list: targets
                .into_iter()
                .enumerate()
                .map(|(i, e)| TargetEntry { expr: e, name: format!("c{i}"), junk: false, origin: None })
                .collect(),
            group_by: GroupBy { exprs: group, sets: None },
            having,
            distinct: DistinctSpec::None,
            windows: Vec::new(),
            has_aggs: true,
            has_windows: false,
            has_target_srfs: false,
            has_sublinks: false,
        }
    }

    #[test]
    fn identical_aggregates_collapse() -> Result<()> {
        let c1 = agg("count", Vec::new(), true);
        let c2 = agg("count", Vec::new(), true);
        let s = agg("sum", vec![col(3)], false);
        let got = collect_aggrefs(&[&c1, &c2, &s])?;
        assert_eq!(got.len(), 2);
        assert_eq!(got[0], c1);
        assert_eq!(got[1], s);
        Ok(())
    }

    #[test]
    fn having_on_group_column_moves_below() -> Result<()> {
        let (_kv, env) = testing::bootstrapped()?;
        let ctx = PlanContext {
            env: &env,
            param_types: Vec::new(),
            param_values: None,
            options: PlanOptions::default(),
            table_rows: HashMap::new(),
        };
        let mut st = PlannerState::new(&ctx);
        st.enter_level(names());

        // SELECT b, sum(c) FROM t GROUP BY b HAVING NOT sum(c) AND NOT b
        let having = BoundExpr::and_all(vec![
            BoundExpr::not(agg("sum", vec![col(3)], false)),
            BoundExpr::not(col(2)),
        ]);
        let sel = select(vec![col(2), agg("sum", vec![col(3)], false)], vec![0], Some(having));
        let plan = build_aggregate(&mut st, input_plan(), &sel)?;

        assert_eq!(plan.vars.len(), 2);
        assert_eq!(plan.out[0].name, "b");
        assert_eq!(plan.out[1].name, "sum");
        match &plan.node {
            LogicalNode::Aggregate(a) => {
                assert_eq!(a.group_exprs.len(), 1);
                assert_eq!(a.aggrefs.len(), 1);
                assert_eq!(a.having.len(), 1);
                assert!(matches!(a.input.node, LogicalNode::Filter(_)));
            }
            other => panic!("expected aggregate node, got {other:?}"),
        }
        Ok(())
    }

    #[test]
    fn duplicate_count_star_is_one_aggregate() -> Result<()> {
        let (_kv, env) = testing::bootstrapped()?;
        let ctx = PlanContext {
            env: &env,
            param_types: Vec::new(),
            param_values: None,
            options: PlanOptions::default(),
            table_rows: HashMap::new(),
        };
        let mut st = PlannerState::new(&ctx);
        st.enter_level(names());

        let sel = select(vec![agg("count", Vec::new(), true), agg("count", Vec::new(), true)], Vec::new(), None);
        let plan = build_aggregate(&mut st, input_plan(), &sel)?;
        match &plan.node {
            LogicalNode::Aggregate(a) => {
                assert!(a.group_exprs.is_empty());
                assert_eq!(a.aggrefs.len(), 1);
                assert!(a.having.is_empty());
            }
            other => panic!("expected aggregate node, got {other:?}"),
        }
        assert_eq!(plan.vars.len(), 1);
        Ok(())
    }

    #[test]
    fn having_aggregate_stays_above() -> Result<()> {
        let (_kv, env) = testing::bootstrapped()?;
        let ctx = PlanContext {
            env: &env,
            param_types: Vec::new(),
            param_values: None,
            options: PlanOptions::default(),
            table_rows: HashMap::new(),
        };
        let mut st = PlannerState::new(&ctx);
        st.enter_level(names());

        // SELECT b FROM t GROUP BY b HAVING NOT sum(c): the aggregate conjunct is not pushed down.
        let sel = select(vec![col(2)], vec![0], Some(BoundExpr::not(agg("sum", vec![col(3)], false))));
        let plan = build_aggregate(&mut st, input_plan(), &sel)?;
        match &plan.node {
            LogicalNode::Aggregate(a) => {
                assert!(matches!(a.input.node, LogicalNode::Scan(_)));
                assert_eq!(a.having.len(), 1);
                assert_eq!(a.aggrefs.len(), 1);
            }
            other => panic!("expected aggregate node, got {other:?}"),
        }
        Ok(())
    }

    #[test]
    #[ignore = "needs PLN-13"]
    fn plain_count_star_physical() -> Result<()> {
        let (_kv, env) = testing::bootstrapped()?;
        let ctx = PlanContext {
            env: &env,
            param_types: Vec::new(),
            param_values: None,
            options: PlanOptions::default(),
            table_rows: HashMap::new(),
        };
        let mut st = PlannerState::new(&ctx);
        st.enter_level(names());
        let sel = select(vec![agg("count", Vec::new(), true)], Vec::new(), None);
        let logical = build_aggregate(&mut st, input_plan(), &sel)?;
        let agg_logical = match &logical.node {
            LogicalNode::Aggregate(a) => a.as_ref().clone(),
            other => panic!("expected aggregate node, got {other:?}"),
        };
        let node = plan_aggregate(&mut st, &agg_logical)?;
        match &node.kind {
            NodeKind::Aggregate(a) => {
                assert_eq!(a.strategy, AggStrategy::Plain);
                assert_eq!(a.aggs.len(), 1);
                assert!(a.group_keys.is_empty());
            }
            other => panic!("expected Aggregate node, got {other:?}"),
        }
        Ok(())
    }

    #[test]
    #[ignore = "needs PLN-12"]
    fn grouped_input_hashes_without_order() -> Result<()> {
        let (_kv, env) = testing::bootstrapped()?;
        let ctx = PlanContext {
            env: &env,
            param_types: Vec::new(),
            param_values: None,
            options: PlanOptions::default(),
            table_rows: HashMap::new(),
        };
        let mut st = PlannerState::new(&ctx);
        st.enter_level(names());
        let sel = select(vec![col(2), agg("sum", vec![col(3)], false)], vec![0], None);
        let logical = build_aggregate(&mut st, input_plan(), &sel)?;
        let agg_logical = match &logical.node {
            LogicalNode::Aggregate(a) => a.as_ref().clone(),
            other => panic!("expected aggregate node, got {other:?}"),
        };
        let node = plan_aggregate(&mut st, &agg_logical)?;
        match &node.kind {
            NodeKind::Aggregate(a) => assert_eq!(a.strategy, AggStrategy::Hashed),
            other => panic!("expected Aggregate node, got {other:?}"),
        }
        Ok(())
    }
}
