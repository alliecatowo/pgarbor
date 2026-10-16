//! Join ordering and join algorithm selection (PLN-06). PG references:
//! `optimizer/path/joinpath.c` (`add_paths_to_joinrel`, `match_unsorted_outer`, `hash_inner_and_outer`,
//! `sort_inner_and_outer`), `joinrels.c` (`make_join_rel`), `allpaths.c` (`standard_join_search`
//! simplified), `util/restrictinfo.c`/`equivclass.c` (`select_equality_operators`),
//! `plan/createplan.c` (`create_nestloop_plan`, `create_hashjoin_plan`, `create_mergejoin_plan`,
//! `get_switched_clauses`).
//!
//! ## Join ordering (`order_join_tree`, called while building the logical plan)
//! Input: a list of FROM items (each a logical plan) plus the AND-ed WHERE conjuncts. Inner joins
//! commute: build a left-deep tree greedily: start from the item with the smallest estimated rows
//! (scan quals counted, `cost::estimate_scan_rows`); repeatedly add the remaining item that is
//! connected to the current set by an equality conjunct (prefer the one with a unique-index lookup
//! possibility on the new item), falling back to the smallest unconnected item (cross join, last).
//! LATERAL items keep their dependency order. Explicit outer joins are NOT reordered
//! (their syntactic nesting is preserved); only maximal inner-join groups are. Item count above 8: keep
//! syntactic order (PG `join_collapse_limit`). Join quals are attached by `pushdown`.
//!
//! ## `plan_join(st, j)` physical choice
//! 1. Plan both inputs (`planner::plan_logical`; for a nested-loop with an index-parameterized inner
//!    plan the inner side is planned by `plan_inner_with_params`).
//! 2. Split `j.quals` into **equi-join keys** and residual: a conjunct `L = R` (operator `=` resolved
//!    for the operands, hashable/mergeable type) where `expr_relids(L)` is within the left input's rels and
//!    `expr_relids(R)` within the right's (or swapped: swap them) and neither side is volatile; the rest
//!    is the residual `qual`.
//! 3. Strategy by join type (RIGHT is rewritten to LEFT by swapping inputs - keep the output column
//!    order via a final `Project` that restores `left ++ right` order):
//!    * keys non-empty, `enable_hashjoin`: **HashJoin** with the SMALLER estimated input as the build
//!      (inner) side when the join type allows (INNER/FULL/SEMI-with-swap rules; LEFT/SEMI/ANTI build on
//!      the nullable/inner side only: for LEFT the right input builds; swap an INNER join if the left is
//!      estimated smaller). FULL JOIN requires hashable or mergeable keys, else 0A000 `FULL JOIN is
//!      only supported with merge-joinable or hash-joinable join conditions` (PG message, SQLSTATE
//!      0A000).
//!    * both inputs already ordered by the keys (`PlanNode::ordering`, e.g. index scans) or
//!      `enable_hashjoin` off: **MergeJoin** (insert `Sort` below inputs that are not ordered).
//!    * no equi keys: **NestedLoop** (INNER/LEFT/SEMI/ANTI; the right side wrapped in `Materialize` when
//!      it is not a cheap rescannable scan and `enable_material`); FULL/RIGHT without keys: 0A000 as above.
//!    * **Index nested loop**: for INNER/LEFT/SEMI/ANTI, when the inner (right) input is a base table
//!      scan and `index::find_param_index` finds an index on its equi columns, and the outer is not
//!      estimated larger than ~1000 rows... (rule: always when the lookup is a unique point lookup; for
//!      non-unique indexes when the outer estimate < 1/10 of the inner estimate): build NestedLoop with
//!      `inner_params` = outer key expressions and the inner plan an `IndexScan` whose `bounds.eq`
//!      values are `OuterRef{0,i}`; remaining equi conjuncts and residual go to `qual`/filter.
//!    * LATERAL right side: NestedLoop with `inner_params` = the outer columns the right side
//!      references (rewritten to `OuterRef`).
//! 4. Output `vars = left.vars ++ right.vars` (Semi/Anti: left only), `out` accordingly, `ordering`:
//!    NestedLoop/HashJoin(probe order) keep the outer ordering; MergeJoin: its key order.
//! 5. `Plan` estimates via `cost::join_rows` etc.
use pgarbor_common::{PgError, Result, SqlState};
use pgarbor_sql::ir::{BoundExpr, ColumnRef, ExprKind, OpExpr, OpRef, OuterRef};
use pgarbor_types::compare::is_orderable;
use pgarbor_types::TypeId;

use crate::context::PlannerState;
use crate::cost;
use crate::index;
use crate::logical::{LogicalJoin, LogicalNode, LogicalPlan, LogicalScan, ScanSource};
use crate::plan::{
    Estimate, HashJoinNode, JoinType, KeyEq, MaterializeNode, MergeJoinNode, MergeKey, NestedLoopNode,
    NodeKind, OutCol, PlanNode, ProjectNode, RangeEnd, SortKeySpec, SortNode,
};
use crate::planner;
use crate::pushdown::{expr_relids, is_volatile};
use crate::simplify;

/// Equi-join key candidate extracted from a join qual.
// CONTRACT
#[derive(Clone, Debug, PartialEq)]
pub struct EquiKey {
    /// Expression over the left input only.
    pub left: BoundExpr,
    /// Expression over the right input only.
    pub right: BoundExpr,
    pub op: pgarbor_sql::ir::OpRef,
}

/// Largest FROM item count that is reordered (PG `join_collapse_limit` analogue).
const MAX_REORDER_ITEMS: usize = 8;

/// Marker level used for outer-row columns while an index lookup is planned: `choose_scan` sees them
/// as outer references (levels_up >= 1) and they are rewritten back to `OuterRef` afterwards. Chosen
/// far above any real query nesting depth so it never collides with a correlated reference.
const MARK_LEVEL: u32 = 1 << 30;

/// Combine independent FROM items and AND-ed quals into a join tree (module docs "Join ordering").
/// Returns the plan and the quals that were not attached (the caller hands them to pushdown).
pub fn order_join_tree(st: &mut PlannerState, items: Vec<LogicalPlan>, quals: Vec<BoundExpr>) -> Result<(LogicalPlan, Vec<BoundExpr>)> {
    let n = items.len();
    let mut it = items.into_iter();
    let first = match it.next() {
        Some(p) => p,
        None => return Err(PgError::internal("join tree without FROM items")),
    };
    let rest: Vec<LogicalPlan> = it.collect();
    if rest.is_empty() {
        return Ok((first, quals));
    }
    let mut all = Vec::with_capacity(n);
    all.push(first);
    all.extend(rest);

    // Dependent items (LATERAL, table functions, VALUES and derived tables may reference earlier FROM
    // items) and large FROM lists keep the syntactic left-deep order.
    if n > MAX_REORDER_ITEMS || all.iter().any(item_is_dependent) {
        let mut parts = all.into_iter();
        let mut cur = match parts.next() {
            Some(p) => p,
            None => return Err(PgError::internal("join tree without FROM items")),
        };
        for p in parts {
            let lateral = item_is_dependent(&p);
            cur = make_join(cur, p, Vec::new(), lateral);
        }
        return Ok((cur, quals));
    }

    // Greedy left-deep ordering.
    let stref: &PlannerState = st;
    let rows: Vec<f64> = all.iter().map(|p| item_rows(stref, p)).collect::<Result<Vec<f64>>>()?;
    let item_rels: Vec<Vec<u32>> = all.iter().map(|p| p.rels.clone()).collect();
    // Relation sets of equality conjuncts (empty for non-equality conjuncts).
    let qual_rels: Vec<Vec<u32>> = quals
        .iter()
        .map(|q| if is_equality(q) { expr_relids(q) } else { Vec::new() })
        .collect();
    let mut slots: Vec<Option<LogicalPlan>> = all.into_iter().map(Some).collect();
    let mut used = vec![false; n];

    let start = (0..n)
        .min_by(|&a, &b| rows[a].partial_cmp(&rows[b]).unwrap_or(std::cmp::Ordering::Equal))
        .unwrap_or(0);
    used[start] = true;
    let mut set_rels: Vec<u32> = item_rels[start].clone();
    let mut cur = take_item(&mut slots, start)?;

    for _ in 1..n {
        // Best candidate: connected beats unconnected, then fewer estimated rows, then lower index.
        let mut best: Option<(usize, bool)> = None;
        for i in 0..n {
            if used[i] {
                continue;
            }
            let conn = connected(&qual_rels, &set_rels, &item_rels[i]);
            let better = match best {
                None => true,
                Some((bi, bconn)) => (conn && !bconn) || (conn == bconn && rows[i] < rows[bi]),
            };
            if better {
                best = Some((i, conn));
            }
        }
        let (i, _) = match best {
            Some(b) => b,
            None => return Err(PgError::internal("join ordering ran out of items")),
        };
        used[i] = true;
        set_rels.extend(item_rels[i].iter().copied());
        let next = take_item(&mut slots, i)?;
        cur = make_join(cur, next, Vec::new(), false);
    }
    Ok((cur, quals))
}

/// Split join conjuncts into equi keys and the residual (module docs step 2).
pub fn extract_equi_keys(st: &PlannerState, quals: &[BoundExpr], left_rels: &[u32], right_rels: &[u32]) -> Result<(Vec<EquiKey>, Vec<BoundExpr>)> {
    let mut keys = Vec::new();
    let mut residual = Vec::new();
    for q in quals {
        match equi_candidate(st, q, left_rels, right_rels)? {
            Some(k) => keys.push(k),
            None => residual.push(q.clone()),
        }
    }
    Ok((keys, residual))
}

/// Physical plan of a logical join (module docs step 1-5).
pub fn plan_join(st: &mut PlannerState, j: &LogicalJoin) -> Result<PlanNode> {
    let mut lowered = j.clone();
    lowered.quals = crate::subplan::process_sublinks_in(st, lowered.quals)?;
    let j = &lowered;
    if j.lateral {
        return plan_lateral_join(st, j);
    }
    let (keys, residual) = extract_equi_keys(st, &j.quals, &j.left.rels, &j.right.rels)?;
    if keys.is_empty() {
        if j.join_type == JoinType::Full {
            return Err(full_join_error());
        }
        let left_node = planner::plan_logical(st, &j.left)?;
        let right_node = planner::plan_logical(st, &j.right)?;
        return plan_nested_loop_no_keys(st, j, left_node, right_node, residual);
    }

    let left_node = planner::plan_logical(st, &j.left)?;
    let left_node = match try_index_nested_loop(st, j, &keys, &residual, left_node)? {
        IndexTry::Used(node) => return Ok(node),
        IndexTry::Skipped(left) => left,
    };
    let right_node = planner::plan_logical(st, &j.right)?;

    let ctx = st.ctx;
    let hash_on = ctx.options.enable_hashjoin;
    let merge_on = ctx.options.enable_mergejoin;
    let lkeys: Vec<BoundExpr> = keys.iter().map(|k| k.left.clone()).collect();
    let rkeys: Vec<BoundExpr> = keys.iter().map(|k| k.right.clone()).collect();
    let both_ordered = ordering_matches(&left_node.ordering, &lkeys) && ordering_matches(&right_node.ordering, &rkeys);
    if merge_on && (both_ordered || !hash_on) {
        return plan_merge_join(st, j, left_node, right_node, &keys, residual);
    }
    plan_hash_join(st, j, left_node, right_node, &keys, residual)
}

// ---------------------------------------------------------------------------------------------
// Join ordering helpers
// ---------------------------------------------------------------------------------------------

/// Scan items that may reference earlier FROM items, and joins already marked LATERAL.
fn item_is_dependent(p: &LogicalPlan) -> bool {
    match &p.node {
        LogicalNode::Scan(s) => matches!(
            s.source,
            ScanSource::Function { .. } | ScanSource::Values { .. } | ScanSource::Subquery(_)
        ),
        LogicalNode::Join(j) => j.lateral,
        _ => false,
    }
}

/// Estimated rows of a FROM item (scan quals counted; other shapes use the default table size).
fn item_rows(st: &PlannerState, p: &LogicalPlan) -> Result<f64> {
    match &p.node {
        LogicalNode::Scan(s) => cost::estimate_scan_rows(st, s),
        _ => Ok(st.ctx.options.default_table_rows),
    }
}

/// `=` conjunct (by operator name).
fn is_equality(q: &BoundExpr) -> bool {
    matches!(&q.kind, ExprKind::Op(o) if o.op.name == "=" && o.args.len() == 2)
}

/// Does some equality conjunct connect the current set with the candidate item?
fn connected(qual_rels: &[Vec<u32>], set: &[u32], item: &[u32]) -> bool {
    qual_rels.iter().any(|r| {
        !r.is_empty()
            && r.iter().all(|x| set.contains(x) || item.contains(x))
            && r.iter().any(|x| set.contains(x))
            && r.iter().any(|x| item.contains(x))
    })
}

fn take_item(slots: &mut [Option<LogicalPlan>], i: usize) -> Result<LogicalPlan> {
    match slots.get_mut(i).and_then(|s| s.take()) {
        Some(p) => Ok(p),
        None => Err(PgError::internal("join item consumed twice")),
    }
}

/// Inner join node of two logical inputs: `vars`/`out` concatenated, `rels` merged.
fn make_join(left: LogicalPlan, right: LogicalPlan, quals: Vec<BoundExpr>, lateral: bool) -> LogicalPlan {
    let mut vars = left.vars.clone();
    vars.extend(right.vars.iter().cloned());
    let mut out = left.out.clone();
    out.extend(right.out.iter().cloned());
    let mut rels = left.rels.clone();
    rels.extend(right.rels.iter().copied());
    rels.sort_unstable();
    rels.dedup();
    let level = left.level;
    LogicalPlan {
        node: LogicalNode::Join(Box::new(LogicalJoin { join_type: JoinType::Inner, left, right, quals, lateral })),
        vars,
        out,
        rels,
        level,
    }
}

// ---------------------------------------------------------------------------------------------
// Equi-key extraction
// ---------------------------------------------------------------------------------------------

fn within(rels: &[u32], set: &[u32]) -> bool {
    !rels.is_empty() && rels.iter().all(|r| set.contains(r))
}

fn equi_candidate(st: &PlannerState, q: &BoundExpr, left_rels: &[u32], right_rels: &[u32]) -> Result<Option<EquiKey>> {
    let op = match &q.kind {
        ExprKind::Op(o) if o.op.name == "=" && o.args.len() == 2 => o,
        _ => return Ok(None),
    };
    let a = &op.args[0];
    let b = &op.args[1];
    if a.ty != b.ty || !is_orderable(st.ctx.env, a.ty)? || !is_orderable(st.ctx.env, b.ty)? {
        return Ok(None);
    }
    if is_volatile(a) || is_volatile(b) {
        return Ok(None);
    }
    let ra = expr_relids(a);
    let rb = expr_relids(b);
    let (left, right) = if within(&ra, left_rels) && within(&rb, right_rels) {
        (a, b)
    } else if within(&ra, right_rels) && within(&rb, left_rels) {
        (b, a)
    } else {
        return Ok(None);
    };
    Ok(Some(EquiKey { left: left.clone(), right: right.clone(), op: op.op.clone() }))
}

fn full_join_error() -> PgError {
    PgError::new(
        SqlState::FEATURE_NOT_SUPPORTED,
        "FULL JOIN is only supported with merge-joinable or hash-joinable join conditions",
    )
}

// ---------------------------------------------------------------------------------------------
// Physical helpers
// ---------------------------------------------------------------------------------------------

/// `op(left, right)` as a boolean expression.
fn op_conjunct(op: OpRef, left: BoundExpr, right: BoundExpr) -> BoundExpr {
    BoundExpr { kind: ExprKind::Op(Box::new(OpExpr { op, args: vec![left, right] })), ty: TypeId::BOOL }
}

/// The original form of an equi key conjunct.
fn key_conjunct(k: &EquiKey) -> BoundExpr {
    op_conjunct(k.op.clone(), k.left.clone(), k.right.clone())
}

/// Does `ordering` begin with `keys` (ascending, NULLS LAST)?
fn ordering_matches(ordering: &[SortKeySpec], keys: &[BoundExpr]) -> bool {
    ordering.len() >= keys.len()
        && keys
            .iter()
            .zip(ordering.iter())
            .all(|(k, s)| s.expr == *k && !s.descending && !s.nulls_first)
}

fn sort_spec(expr: BoundExpr) -> SortKeySpec {
    SortKeySpec { expr, descending: false, nulls_first: false, collation: 0, using_op: None }
}

/// Estimate of a join node from its two physical inputs.
fn join_estimate(shape: &str, outer: &PlanNode, inner: &PlanNode, jt: JoinType, nkeys: usize, nother: usize) -> Result<Estimate> {
    let rows = cost::join_rows(outer.est.rows, inner.est.rows, jt, nkeys, nother)?;
    let width = outer.est.width + inner.est.width;
    cost::cost_node(shape, rows, width, &[outer.est, inner.est])
}

/// Output layout of a physical join: `outer ++ inner` (Semi/Anti: outer only).
fn join_layout(outer: &PlanNode, inner: &PlanNode, semi: bool) -> (Vec<BoundExpr>, Vec<OutCol>) {
    let mut vars = outer.vars.clone();
    let mut out = outer.out.clone();
    if !semi {
        vars.extend(inner.vars.iter().cloned());
        out.extend(inner.out.iter().cloned());
    }
    (vars, out)
}

fn is_semi(jt: JoinType) -> bool {
    matches!(jt, JoinType::Semi | JoinType::Anti)
}

/// Put a `Sort` under `child` unless it is already ordered by `keys`.
fn ensure_sorted(st: &PlannerState, child: PlanNode, keys: &[BoundExpr]) -> Result<PlanNode> {
    if ordering_matches(&child.ordering, keys) {
        return Ok(child);
    }
    let specs: Vec<SortKeySpec> = keys.iter().map(|e| sort_spec(e.clone())).collect();
    let est = cost::cost_node("sort", child.est.rows, child.est.width, &[child.est])?;
    let out = child.out.clone();
    let vars = child.vars.clone();
    let mut node = st.new_node(NodeKind::Sort(Box::new(SortNode { input: child, keys: specs.clone(), limit: None })), out);
    node.vars = vars;
    node.ordering = specs;
    node.est = est;
    Ok(node)
}

/// Wrap the inner side of a nested loop in `Materialize` (rescans read the buffer).
fn maybe_materialize(st: &PlannerState, inner: PlanNode) -> Result<PlanNode> {
    if !st.ctx.options.enable_material {
        return Ok(inner);
    }
    let est = cost::cost_node("materialize", inner.est.rows, inner.est.width, &[inner.est])?;
    let out = inner.out.clone();
    let vars = inner.vars.clone();
    let ordering = inner.ordering.clone();
    let mut node = st.new_node(NodeKind::Materialize(Box::new(MaterializeNode { input: inner })), out);
    node.vars = vars;
    node.ordering = ordering;
    node.est = est;
    Ok(node)
}

/// Restore the logical `left ++ right` column order after a RIGHT->LEFT or swapped inner join.
fn finish_join(st: &PlannerState, j: &LogicalJoin, node: PlanNode, swapped: bool) -> Result<PlanNode> {
    if !swapped {
        return Ok(node);
    }
    let mut out = j.left.out.clone();
    out.extend(j.right.out.iter().cloned());
    let mut exprs = j.left.vars.clone();
    exprs.extend(j.right.vars.iter().cloned());
    // Physical children may carry slots the logical vars do not list (a DML target's rowid): use
    // the children's own layouts, left child first.
    if node.vars.len() > exprs.len() && node.vars.len() == node.out.len() {
        // Partition the physical layout into the logical left side (its vars, or the rowid slot of one of
        // its range entries) and the rest, keeping each side's physical order.
        let in_left = |v: &BoundExpr| {
            j.left.vars.contains(v)
                || matches!(&v.kind, ExprKind::Column(c) if c.levels_up == 0 && c.attnum == -1 && j.left.rels.contains(&c.rt_index))
        };
        let (mut lv, mut lo, mut rv, mut ro) = (Vec::new(), Vec::new(), Vec::new(), Vec::new());
        for (v, o) in node.vars.iter().zip(node.out.iter()) {
            if in_left(v) {
                lv.push(v.clone());
                lo.push(o.clone());
            } else {
                rv.push(v.clone());
                ro.push(o.clone());
            }
        }
        lv.extend(rv);
        lo.extend(ro);
        exprs = lv;
        out = lo;
    }
    let est = cost::cost_node("project", node.est.rows, node.est.width, &[node.est])?;
    let ordering = node.ordering.clone();
    let mut p = st.new_node(NodeKind::Project(Box::new(ProjectNode { input: node, exprs: exprs.clone() })), out);
    p.vars = exprs;
    p.ordering = ordering;
    p.est = est;
    Ok(p)
}

fn plan_nested_loop_no_keys(st: &mut PlannerState, j: &LogicalJoin, left: PlanNode, right: PlanNode, residual: Vec<BoundExpr>) -> Result<PlanNode> {
    let nother = residual.len();
    let qual = simplify::make_and(residual);
    let swapped = j.join_type == JoinType::Right;
    let (outer, inner, jt) = if swapped {
        (right, left, JoinType::Left)
    } else {
        (left, right, j.join_type)
    };
    let inner = maybe_materialize(st, inner)?;
    let node = build_nested_loop(st, jt, outer, inner, qual, Vec::new(), 0, nother)?;
    finish_join(st, j, node, swapped)
}

/// Build a NestedLoop node over physical inputs (layout and estimate filled in).
#[allow(clippy::too_many_arguments)]
fn build_nested_loop(
    st: &PlannerState,
    jt: JoinType,
    outer: PlanNode,
    inner: PlanNode,
    qual: Option<BoundExpr>,
    inner_params: Vec<BoundExpr>,
    nkeys: usize,
    nother: usize,
) -> Result<PlanNode> {
    let est = join_estimate("nestloop", &outer, &inner, jt, nkeys, nother)?;
    let (vars, out) = join_layout(&outer, &inner, is_semi(jt));
    let ordering = outer.ordering.clone();
    let kind = NodeKind::NestedLoop(Box::new(NestedLoopNode { join_type: jt, outer, inner, qual, inner_params }));
    let mut node = st.new_node(kind, out);
    node.vars = vars;
    node.ordering = ordering;
    node.est = est;
    Ok(node)
}

fn plan_merge_join(
    st: &mut PlannerState,
    j: &LogicalJoin,
    left: PlanNode,
    right: PlanNode,
    keys: &[EquiKey],
    residual: Vec<BoundExpr>,
) -> Result<PlanNode> {
    let lkeys: Vec<BoundExpr> = keys.iter().map(|k| k.left.clone()).collect();
    let rkeys: Vec<BoundExpr> = keys.iter().map(|k| k.right.clone()).collect();
    let left = ensure_sorted(st, left, &lkeys)?;
    let right = ensure_sorted(st, right, &rkeys)?;
    let nother = residual.len();
    let qual = simplify::make_and(residual);
    let left_keys: Vec<MergeKey> = lkeys
        .iter()
        .map(|e| MergeKey { expr: e.clone(), descending: false, nulls_first: false })
        .collect();
    let right_keys: Vec<MergeKey> = rkeys
        .iter()
        .map(|e| MergeKey { expr: e.clone(), descending: false, nulls_first: false })
        .collect();
    let est = join_estimate("mergejoin", &left, &right, j.join_type, keys.len(), nother)?;
    let semi = is_semi(j.join_type);
    let (vars, out) = join_layout(&left, &right, semi);
    let ordering: Vec<SortKeySpec> = if matches!(j.join_type, JoinType::Inner | JoinType::Left | JoinType::Semi | JoinType::Anti) {
        lkeys.iter().map(|e| sort_spec(e.clone())).collect()
    } else {
        Vec::new()
    };
    let kind = NodeKind::MergeJoin(Box::new(MergeJoinNode {
        join_type: j.join_type,
        left,
        right,
        left_keys,
        right_keys,
        qual,
    }));
    let mut node = st.new_node(kind, out);
    node.vars = vars;
    node.ordering = ordering;
    node.est = est;
    Ok(node)
}

fn plan_hash_join(
    st: &mut PlannerState,
    j: &LogicalJoin,
    left: PlanNode,
    right: PlanNode,
    keys: &[EquiKey],
    residual: Vec<BoundExpr>,
) -> Result<PlanNode> {
    let jt = j.join_type;
    // The inner (build) side is the right input, except: RIGHT is rewritten to LEFT with the inputs
    // swapped (the preserved side probes), and INNER/FULL build on the smaller input.
    let swap = match jt {
        JoinType::Right => true,
        JoinType::Inner | JoinType::Full => left.est.rows < right.est.rows,
        _ => false,
    };
    let probe_jt = if jt == JoinType::Right { JoinType::Left } else { jt };
    let lkeys: Vec<BoundExpr> = keys.iter().map(|k| k.left.clone()).collect();
    let rkeys: Vec<BoundExpr> = keys.iter().map(|k| k.right.clone()).collect();
    let eq_ops: Vec<OpRef> = keys.iter().map(|k| k.op.clone()).collect();
    let nother = residual.len();
    let qual = simplify::make_and(residual);
    let (outer, inner, outer_keys, inner_keys) = if swap {
        (right, left, rkeys, lkeys)
    } else {
        (left, right, lkeys, rkeys)
    };
    let est = join_estimate("hashjoin", &outer, &inner, probe_jt, keys.len(), nother)?;
    let (vars, out) = join_layout(&outer, &inner, is_semi(probe_jt));
    let ordering = outer.ordering.clone();
    let kind = NodeKind::HashJoin(Box::new(HashJoinNode {
        join_type: probe_jt,
        outer,
        inner,
        outer_keys,
        inner_keys,
        eq_ops,
        qual,
    }));
    let mut node = st.new_node(kind, out);
    node.vars = vars;
    node.ordering = ordering;
    node.est = est;
    finish_join(st, j, node, swap)
}

// ---------------------------------------------------------------------------------------------
// Index nested loop and LATERAL
// ---------------------------------------------------------------------------------------------

enum IndexTry {
    Used(PlanNode),
    Skipped(PlanNode),
}

/// Index nested loop over a base table on the right (module docs step 3). Returns the left plan back
/// when the strategy does not apply.
fn try_index_nested_loop(
    st: &mut PlannerState,
    j: &LogicalJoin,
    keys: &[EquiKey],
    residual: &[BoundExpr],
    left: PlanNode,
) -> Result<IndexTry> {
    let ctx = st.ctx;
    if !matches!(j.join_type, JoinType::Inner | JoinType::Left | JoinType::Semi | JoinType::Anti)
        || !ctx.options.enable_nestloop
        || !ctx.options.enable_indexscan
    {
        return Ok(IndexTry::Skipped(left));
    }
    let scan: &LogicalScan = match &j.right.node {
        LogicalNode::Scan(s) => s.as_ref(),
        _ => return Ok(IndexTry::Skipped(left)),
    };
    let table_oid = match &scan.source {
        ScanSource::Table { table, is_virtual: false } => table.oid,
        _ => return Ok(IndexTry::Skipped(left)),
    };
    if st.is_inherit_scan(j.right.level, scan.rt_index) {
        return Ok(IndexTry::Skipped(left));
    }

    // Decision: is the lookup selective enough compared with a hash join over the whole table?
    let probe_quals: Vec<BoundExpr> = keys.iter().map(|k| op_conjunct(k.op.clone(), k.right.clone(), k.left.clone())).collect();
    let m = match index::find_param_index(st, table_oid, scan.rt_index, &probe_quals)? {
        Some(m) => m,
        None => return Ok(IndexTry::Skipped(left)),
    };
    let table_rows = cost::table_rows(st, scan)?;
    let outer_rows = left.est.rows;
    let use_index = if m.unique_point {
        outer_rows * 2.0 < table_rows
    } else {
        outer_rows * 10.0 < table_rows
    };
    if !use_index {
        return Ok(IndexTry::Skipped(left));
    }

    // Parameters: one per distinct outer key expression. Inside the inner scan they appear as
    // marked columns and are rewritten to `OuterRef{0, i}`.
    let mut params: Vec<BoundExpr> = Vec::new();
    for k in keys {
        if !params.contains(&k.left) {
            params.push(k.left.clone());
        }
    }
    let mut convs: Vec<BoundExpr> = Vec::with_capacity(params.len());
    for p in &params {
        convs.push(mark_outer(p.clone())?);
    }
    let mut scan2: LogicalScan = scan.clone();
    for k in keys {
        let marked = mark_outer(k.left.clone())?;
        scan2.quals.push(op_conjunct(k.op.clone(), k.right.clone(), marked));
    }
    let mut inner = index::choose_scan(st, &scan2, None, false)?;
    if !matches!(inner.kind, NodeKind::IndexScan(_)) {
        return Ok(IndexTry::Skipped(left));
    }

    let mut replaced = false;
    {
        let mut rewrite = |x: BoundExpr| -> Result<BoundExpr> {
            match convs.iter().position(|c| *c == x) {
                Some(i) => {
                    replaced = true;
                    let ty = x.ty;
                    Ok(BoundExpr::new(ExprKind::OuterRef(OuterRef { levels_up: 0, index: i as u32 }), ty))
                }
                None => Ok(x),
            }
        };
        map_exprs_in_node(&mut inner, &mut rewrite)?;
    }
    if !replaced {
        return Ok(IndexTry::Skipped(left));
    }

    // The equi conjuncts stay in the join qual as well (cheap, and it keeps the join correct when the
    // index consumed only some of them); residual conjuncts are evaluated over outer ++ inner.
    let mut qual_parts: Vec<BoundExpr> = keys.iter().map(key_conjunct).collect();
    qual_parts.extend(residual.iter().cloned());
    let nother = residual.len();
    let qual = simplify::make_and(qual_parts);
    let node = build_nested_loop(st, j.join_type, left, inner, qual, params, keys.len(), nother)?;
    Ok(IndexTry::Used(node))
}

/// Replace every column of level 0 by the marker level (outer-row columns for a parameterized scan).
fn mark_outer(e: BoundExpr) -> Result<BoundExpr> {
    e.transform(&mut |x: BoundExpr| -> Result<BoundExpr> { Ok(mark_column(x)) })
}

fn mark_column(x: BoundExpr) -> BoundExpr {
    let ty = x.ty;
    match x.kind {
        ExprKind::Column(c) if c.levels_up == 0 => BoundExpr {
            kind: ExprKind::Column(ColumnRef { levels_up: MARK_LEVEL, ..c }),
            ty,
        },
        kind => BoundExpr { kind, ty },
    }
}

/// LATERAL join: nested loop whose inner side is parameterized by the outer columns it references.
fn plan_lateral_join(st: &mut PlannerState, j: &LogicalJoin) -> Result<PlanNode> {
    if matches!(j.join_type, JoinType::Right | JoinType::Full) {
        return Err(PgError::new(
            SqlState::FEATURE_NOT_SUPPORTED,
            "LATERAL is not supported with RIGHT or FULL JOIN",
        ));
    }
    let left = planner::plan_logical(st, &j.left)?;
    let mut right = planner::plan_logical(st, &j.right)?;
    let mut params: Vec<(ColumnRef, TypeId)> = Vec::new();
    rewrite_lateral_tree(&mut right, j.right.level, &st.level_parents, &j.left.rels, &mut params)?;
    let param_exprs: Vec<BoundExpr> = params
        .iter()
        .map(|(c, ty)| BoundExpr::new(ExprKind::Column(*c), *ty))
        .collect();
    let nother = j.quals.len();
    let qual = simplify::make_and(j.quals.clone());
    let inner = if param_exprs.is_empty() { maybe_materialize(st, right)? } else { right };
    let node = build_nested_loop(st, j.join_type, left, inner, qual, param_exprs, 0, nother)?;
    Ok(node)
}

/// A derived table starts a query level: its local Column(0, ...) variables must remain local.
/// Only references to the owning FROM level are parameters of this nested loop.
fn rewrite_lateral_tree(node: &mut PlanNode, owner: u32, parents: &[Option<u32>], rels: &[u32], params: &mut Vec<(ColumnRef, TypeId)>) -> Result<()> {
    let level = node.level;
    {
        let mut rewrite = |x: BoundExpr| -> Result<BoundExpr> {
            match &x.kind {
                ExprKind::Column(c) if column_owner(level, c.levels_up, parents) == Some(owner) && rels.contains(&c.rt_index) => {
                    let local = BoundExpr::new(ExprKind::Column(ColumnRef { levels_up: 0, ..*c }), x.ty);
                    Ok(lateral_rewrite(local, rels, params))
                }
                _ => Ok(x),
            }
        };
        for v in &mut node.vars { map_in_place(v, &mut rewrite)?; }
        map_kind_exprs(&mut node.kind, &mut rewrite)?;
    }
    for c in node.children_mut() { rewrite_lateral_tree(c, owner, parents, rels, params)?; }
    Ok(())
}

fn column_owner(mut level: u32, levels_up: u32, parents: &[Option<u32>]) -> Option<u32> {
    for _ in 0..levels_up { level = parents.get(level as usize).copied().flatten()?; }
    Some(level)
}

/// Rewrite one outer column reference of a LATERAL right side into `OuterRef`, registering it as a
/// parameter (deduplicated by column).
fn lateral_rewrite(x: BoundExpr, left_rels: &[u32], params: &mut Vec<(ColumnRef, TypeId)>) -> BoundExpr {
    let ty = x.ty;
    let found: Option<ColumnRef> = if let ExprKind::Column(c) = &x.kind {
        if c.levels_up == 0 && left_rels.contains(&c.rt_index) {
            Some(*c)
        } else {
            None
        }
    } else {
        None
    };
    let c = match found {
        Some(c) => c,
        None => return x,
    };
    let index = match params.iter().position(|(p, _)| *p == c) {
        Some(i) => i,
        None => {
            params.push((c, ty));
            params.len() - 1
        }
    };
    BoundExpr::new(ExprKind::OuterRef(OuterRef { levels_up: 0, index: index as u32 }), ty)
}

// ---------------------------------------------------------------------------------------------
// Expression rewriting over a physical subtree
// ---------------------------------------------------------------------------------------------

/// Apply `f` (bottom-up) to every expression of `node` and its children, including planner scratch
/// `vars`.
fn map_exprs_in_node(node: &mut PlanNode, f: &mut dyn FnMut(BoundExpr) -> Result<BoundExpr>) -> Result<()> {
    for v in node.vars.iter_mut() {
        map_in_place(v, f)?;
    }
    map_kind_exprs(&mut node.kind, f)?;
    for c in node.children_mut() {
        map_exprs_in_node(c, f)?;
    }
    Ok(())
}

fn map_in_place(e: &mut BoundExpr, f: &mut dyn FnMut(BoundExpr) -> Result<BoundExpr>) -> Result<()> {
    let old = std::mem::replace(e, BoundExpr::bool_const(false));
    *e = old.transform(&mut *f)?;
    Ok(())
}

fn map_opt(e: &mut Option<BoundExpr>, f: &mut dyn FnMut(BoundExpr) -> Result<BoundExpr>) -> Result<()> {
    if let Some(x) = e.as_mut() {
        map_in_place(x, f)?;
    }
    Ok(())
}

fn map_list(v: &mut [BoundExpr], f: &mut dyn FnMut(BoundExpr) -> Result<BoundExpr>) -> Result<()> {
    for x in v.iter_mut() {
        map_in_place(x, f)?;
    }
    Ok(())
}

fn map_sort_keys(v: &mut [SortKeySpec], f: &mut dyn FnMut(BoundExpr) -> Result<BoundExpr>) -> Result<()> {
    for k in v.iter_mut() {
        map_in_place(&mut k.expr, f)?;
    }
    Ok(())
}

fn map_range(r: &mut Option<RangeEnd>, f: &mut dyn FnMut(BoundExpr) -> Result<BoundExpr>) -> Result<()> {
    if let Some(r) = r.as_mut() {
        map_in_place(&mut r.value, f)?;
    }
    Ok(())
}

fn map_kind_exprs(kind: &mut NodeKind, f: &mut dyn FnMut(BoundExpr) -> Result<BoundExpr>) -> Result<()> {
    match kind {
        NodeKind::Result(n) => {
            map_list(&mut n.exprs, f)?;
            map_opt(&mut n.one_time_filter, f)?;
        }
        NodeKind::SeqScan(n) => map_opt(&mut n.filter, f)?,
        NodeKind::IndexScan(n) => {
            for k in n.bounds.eq.iter_mut() {
                match k {
                    KeyEq::Value(e) | KeyEq::AnyOf(e) => map_in_place(e, f)?,
                }
            }
            map_range(&mut n.bounds.lower, f)?;
            map_range(&mut n.bounds.upper, f)?;
            map_list(&mut n.index_cond, f)?;
            map_opt(&mut n.filter, f)?;
        }
        NodeKind::VirtualScan(n) => {
            for (_, e) in n.eq_filters.iter_mut() {
                map_in_place(e, f)?;
            }
            map_opt(&mut n.filter, f)?;
        }
        NodeKind::ValuesScan(n) => {
            for row in n.rows.iter_mut() {
                map_list(row, f)?;
            }
            map_opt(&mut n.filter, f)?;
        }
        NodeKind::FunctionScan(n) => {
            for fi in n.funcs.iter_mut() {
                map_in_place(&mut fi.expr, f)?;
            }
            map_opt(&mut n.filter, f)?;
        }
        NodeKind::CteScan(n) => map_opt(&mut n.filter, f)?,
        NodeKind::WorkTableScan(n) => map_opt(&mut n.filter, f)?,
        NodeKind::Filter(n) => map_in_place(&mut n.predicate, f)?,
        NodeKind::Project(n) => map_list(&mut n.exprs, f)?,
        NodeKind::ProjectSet(n) => map_list(&mut n.exprs, f)?,
        NodeKind::NestedLoop(n) => {
            map_opt(&mut n.qual, f)?;
            map_list(&mut n.inner_params, f)?;
        }
        NodeKind::HashJoin(n) => {
            map_list(&mut n.outer_keys, f)?;
            map_list(&mut n.inner_keys, f)?;
            map_opt(&mut n.qual, f)?;
        }
        NodeKind::MergeJoin(n) => {
            for k in n.left_keys.iter_mut() {
                map_in_place(&mut k.expr, f)?;
            }
            for k in n.right_keys.iter_mut() {
                map_in_place(&mut k.expr, f)?;
            }
            map_opt(&mut n.qual, f)?;
        }
        NodeKind::Aggregate(n) => {
            map_list(&mut n.group_keys, f)?;
            for a in n.aggs.iter_mut() {
                map_list(&mut a.args, f)?;
                map_list(&mut a.direct_args, f)?;
                map_opt(&mut a.filter, f)?;
                map_sort_keys(&mut a.order_by, f)?;
            }
            map_opt(&mut n.having, f)?;
        }
        NodeKind::WindowAgg(n) => {
            map_list(&mut n.partition_by, f)?;
            map_sort_keys(&mut n.order_by, f)?;
            for w in n.funcs.iter_mut() {
                map_list(&mut w.args, f)?;
                map_opt(&mut w.filter, f)?;
            }
        }
        NodeKind::Sort(n) => map_sort_keys(&mut n.keys, f)?,
        NodeKind::Limit(n) => {
            map_opt(&mut n.count, f)?;
            map_opt(&mut n.offset, f)?;
            map_sort_keys(&mut n.ties_keys, f)?;
        }
        NodeKind::Distinct(_)
        | NodeKind::SetOp(_)
        | NodeKind::Append(_)
        | NodeKind::Materialize(_)
        | NodeKind::LockRows(_)
        | NodeKind::ModifyTable(_) => {}
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::context::{PlanContext, PlanOptions};
    use crate::logical::{LogicalScan, ScanSource};
    use crate::plan::TableRef;
    use pgarbor_catalog::testing;
    use pgarbor_common::Oid;
    use pgarbor_sql::ir::OpRef;
    use pgarbor_types::catalog_data::Volatility;
    use std::collections::HashMap;

    const INT4_EQ: Oid = 96;
    const INT4_LT: Oid = 97;

    fn col(rt: u32, attnum: i16) -> BoundExpr {
        BoundExpr::column(0, rt, attnum, TypeId::INT4)
    }

    fn binop(name: &str, oid: Oid, l: BoundExpr, r: BoundExpr) -> BoundExpr {
        let op = OpRef { op: oid, proc: 0, name: name.to_string(), volatility: Volatility::Immutable, strict: true };
        BoundExpr { kind: ExprKind::Op(Box::new(OpExpr { op, args: vec![l, r] })), ty: TypeId::BOOL }
    }

    fn one_row_plan(rt: u32) -> LogicalPlan {
        LogicalPlan {
            node: LogicalNode::Scan(Box::new(LogicalScan {
                rt_index: rt,
                source: ScanSource::OneRow,
                quals: Vec::new(),
                with_rowid: false,
            })),
            vars: Vec::new(),
            out: Vec::new(),
            rels: vec![rt],
            level: 0,
        }
    }

    fn table_plan(rt: u32, oid: Oid, ncols: u32) -> LogicalPlan {
        let table = TableRef {
            oid,
            schema: "public".to_string(),
            name: format!("t{rt}"),
            alias: format!("t{rt}"),
            ncols,
            col_types: vec![TypeId::INT4; ncols as usize],
        };
        LogicalPlan {
            node: LogicalNode::Scan(Box::new(LogicalScan {
                rt_index: rt,
                source: ScanSource::Table { table, is_virtual: false },
                quals: Vec::new(),
                with_rowid: false,
            })),
            vars: (1..=ncols as i16).map(|a| col(rt, a)).collect(),
            out: Vec::new(),
            rels: vec![rt],
            level: 0,
        }
    }

    #[test]
    fn equi_key_is_extracted_in_either_order() -> Result<()> {
        let (_kv, env) = testing::bootstrapped()?;
        let ctx = PlanContext {
            env: &env,
            param_types: Vec::new(),
            param_values: None,
            options: PlanOptions::default(),
            table_rows: HashMap::new(),
        };
        let st = PlannerState::new(&ctx);
        let left = vec![1u32];
        let right = vec![2u32];
        let quals = vec![
            binop("=", INT4_EQ, col(1, 1), col(2, 1)),
            binop("=", INT4_EQ, col(2, 2), col(1, 2)),
            binop("<", INT4_LT, col(1, 1), col(2, 3)),
        ];
        let (keys, residual) = extract_equi_keys(&st, &quals, &left, &right)?;
        assert_eq!(keys.len(), 2);
        assert_eq!(keys[0].left, col(1, 1));
        assert_eq!(keys[0].right, col(2, 1));
        // Swapped operands: the left-input expression is put first.
        assert_eq!(keys[1].left, col(1, 2));
        assert_eq!(keys[1].right, col(2, 2));
        assert_eq!(residual.len(), 1);
        Ok(())
    }

    #[test]
    fn constant_side_is_not_an_equi_key() -> Result<()> {
        let (_kv, env) = testing::bootstrapped()?;
        let ctx = PlanContext {
            env: &env,
            param_types: Vec::new(),
            param_values: None,
            options: PlanOptions::default(),
            table_rows: HashMap::new(),
        };
        let st = PlannerState::new(&ctx);
        let quals = vec![binop("=", INT4_EQ, col(1, 1), BoundExpr::param(1, TypeId::INT4))];
        let (keys, residual) = extract_equi_keys(&st, &quals, &[1], &[2])?;
        assert!(keys.is_empty());
        assert_eq!(residual.len(), 1);
        Ok(())
    }

    #[test]
    fn full_join_without_keys_is_0a000() -> Result<()> {
        let (_kv, env) = testing::bootstrapped()?;
        let ctx = PlanContext {
            env: &env,
            param_types: Vec::new(),
            param_values: None,
            options: PlanOptions::default(),
            table_rows: HashMap::new(),
        };
        let mut st = PlannerState::new(&ctx);
        let j = LogicalJoin {
            join_type: JoinType::Full,
            left: one_row_plan(1),
            right: one_row_plan(2),
            quals: vec![binop("<", INT4_LT, col(1, 1), col(2, 1))],
            lateral: false,
        };
        let err = match plan_join(&mut st, &j) {
            Ok(_) => return Err(PgError::internal("FULL JOIN without keys must fail")),
            Err(e) => e,
        };
        assert_eq!(err.sqlstate, SqlState::FEATURE_NOT_SUPPORTED);
        assert_eq!(
            err.message,
            "FULL JOIN is only supported with merge-joinable or hash-joinable join conditions"
        );
        Ok(())
    }

    #[test]
    fn lateral_rewrite_deduplicates_outer_columns() {
        let left_rels = [1u32];
        let mut params: Vec<(ColumnRef, TypeId)> = Vec::new();
        let a = lateral_rewrite(col(1, 1), &left_rels, &mut params);
        let b = lateral_rewrite(col(1, 1), &left_rels, &mut params);
        let c = lateral_rewrite(col(2, 1), &left_rels, &mut params);
        assert_eq!(params.len(), 1);
        assert_eq!(a.kind, ExprKind::OuterRef(OuterRef { levels_up: 0, index: 0 }));
        assert_eq!(b.kind, ExprKind::OuterRef(OuterRef { levels_up: 0, index: 0 }));
        // Columns of the right side are left alone.
        assert_eq!(c, col(2, 1));
    }

    #[test]
    #[ignore = "needs PLN-13 (planner::plan_logical) and PLN-12 (cost)"]
    fn equi_join_builds_hash_on_smaller_side() -> Result<()> {
        let (_kv, env) = testing::bootstrapped()?;
        let mut table_rows = HashMap::new();
        table_rows.insert(16400, 1000.0);
        table_rows.insert(16401, 10.0);
        let ctx = PlanContext {
            env: &env,
            param_types: Vec::new(),
            param_values: None,
            options: PlanOptions::default(),
            table_rows,
        };
        let mut st = PlannerState::new(&ctx);
        let j = LogicalJoin {
            join_type: JoinType::Inner,
            left: table_plan(1, 16400, 2),
            right: table_plan(2, 16401, 2),
            quals: vec![binop("=", INT4_EQ, col(1, 1), col(2, 1))],
            lateral: false,
        };
        let node = plan_join(&mut st, &j)?;
        match &node.kind {
            NodeKind::HashJoin(h) => {
                // The smaller input (u, 10 rows) is the build side and therefore the inner child.
                assert_eq!(h.outer_keys, vec![col(1, 1)]);
                assert_eq!(h.inner_keys, vec![col(2, 1)]);
            }
            other => return Err(PgError::internal(format!("expected HashJoin, got {other:?}"))),
        }
        Ok(())
    }

    #[test]
    #[ignore = "needs PLN-13 (planner::plan_logical) and PLN-12 (cost)"]
    fn non_equi_join_uses_materialized_nested_loop() -> Result<()> {
        let (_kv, env) = testing::bootstrapped()?;
        let ctx = PlanContext {
            env: &env,
            param_types: Vec::new(),
            param_values: None,
            options: PlanOptions::default(),
            table_rows: HashMap::new(),
        };
        let mut st = PlannerState::new(&ctx);
        let j = LogicalJoin {
            join_type: JoinType::Inner,
            left: table_plan(1, 16400, 2),
            right: table_plan(2, 16401, 4),
            quals: vec![binop("<", INT4_LT, col(1, 1), col(2, 4))],
            lateral: false,
        };
        let node = plan_join(&mut st, &j)?;
        match &node.kind {
            NodeKind::NestedLoop(n) => {
                assert!(n.qual.is_some());
                assert!(matches!(n.inner.kind, NodeKind::Materialize(_)));
            }
            other => return Err(PgError::internal(format!("expected NestedLoop, got {other:?}"))),
        }
        Ok(())
    }
}
