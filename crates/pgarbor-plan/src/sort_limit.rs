//! ORDER BY / LIMIT / DISTINCT planning and ordering reasoning (PLN-09). PG references:
//! `optimizer/path/pathkeys.c` (`pathkeys_contained_in`, `make_pathkeys_for_sortclauses`),
//! `plan/planner.c` (`create_ordered_paths`, `create_distinct_paths`, `create_limit_path`),
//! `plan/createplan.c` (`make_sort`, `create_limit_plan`, `make_unique_from_sortclauses`),
//! `nodeSort.c` (bounded sort), `nodeLimit.c` (WITH TIES).
//!
//! ## Logical build (`build_sort_limit_distinct`)
//! Applied to the SELECT block's projected plan (`LogicalProject` over all target entries incl. junk):
//! 1. DISTINCT / DISTINCT ON (`sel.distinct`): `LogicalDistinct`. Plain `DISTINCT`: key = all visible
//!    slots; the ORDER BY keys (which `bind` guaranteed are among the visible entries) sort afterwards.
//!    `DISTINCT ON (..)`: key = the ON expressions' slots; the input is sorted by the ORDER BY keys
//!    first (the binder guaranteed the ON keys are a prefix of them) and `on = true`.
//! 2. ORDER BY (`q.order_by: Vec<SortKey>` with `target_idx` into the target list): `LogicalSort` with
//!    [`sort_key_specs`] (expr = target entry expression, direction, NULLS, `using_op`).
//! 3. LIMIT/OFFSET (`q.limit`, `q.offset`, `q.limit_with_ties`) -> `LogicalLimit` (ties keys = the sort
//!    keys; `WITH TIES` without sort keys is a binder error already).
//! 4. A final projection removes junk entries (`n_visible`) when there are any.
//! Order of nodes bottom-up: Project -> Distinct -> Sort -> Limit -> (final Project).
//!
//! Implementation note (deviation, required by the lease acceptance "`select distinct b` with ORDER BY b ->
//! Sort + Distinct Sorted"): for plain `DISTINCT` WITH an ORDER BY the bottom-up order is
//! Project -> Sort -> Distinct -> Limit -> (final Project). The sort keys are the ORDER BY keys followed by
//! the remaining visible slots, so that rows equal on all visible slots are adjacent for `Sorted`
//! de-duplication and the output stays ordered by the ORDER BY keys (PG `create_distinct_paths` does the
//! same). Plain DISTINCT without ORDER BY and all DISTINCT ON forms keep the order above.
//!
//! ## Physical
//! * `plan_sort`: skip the `Sort` if the child's `ordering` satisfies the requested keys
//!   ([`ordering_satisfies`]: requested keys are a PREFIX of the child's ordering, comparing structurally
//!   equal expressions, same direction and NULLS placement; keys constrained to a single constant by
//!   equality quals may be ignored - optional). Otherwise `Sort`; if the parent is a `Limit` with a
//!   constant count (and no ties) set `SortNode::limit = count + offset` (top-N).
//! * `plan_limit`: `LimitNode` with expressions simplified; `Limit 0` constants may be replaced by a `Result`
//!   with `one_time_filter = false`; a LIMIT over an `IndexScan` that provides the order lets the
//!   planner have asked `index::choose_scan` with `want_ordered_scan_for_limit`.
//! * `plan_distinct`: `Hashed` (HashAggregate-like, `enable_hashagg`, hashable types) for plain DISTINCT
//!   without an ORDER BY requirement; `Sorted` when the child is ordered by the key slots (sort
//!   added otherwise) or for DISTINCT ON; EXPLAIN labels: Hashed -> `HashAggregate` with `Group Key:`,
//!   Sorted -> `Unique`.
use pgarbor_common::{PgError, Result};
use pgarbor_sql::ir::{BoundExpr, BoundQuery, BoundSelect, DistinctSpec, SortKey};
use pgarbor_types::Datum;

use crate::context::PlannerState;
use crate::logical::{LogicalDistinct, LogicalLimit, LogicalNode, LogicalPlan, LogicalProject, LogicalSort};
use crate::plan::{DistinctNode, DistinctStrategy, LimitNode, NodeKind, PlanNode, SortKeySpec, SortNode};
use crate::planner::plan_logical;

/// Append Distinct/Sort/Limit (and the junk-stripping projection) above `projected` (module docs).
pub fn build_sort_limit_distinct(st: &mut PlannerState, projected: LogicalPlan, sel: Option<&BoundSelect>, q: &BoundQuery) -> Result<LogicalPlan> {
    let _ = st;
    let n_visible = match &projected.node {
        LogicalNode::Project(p) => p.n_visible.min(projected.width()),
        _ => projected.width(),
    };
    let mut cur = projected;
    match sel.map(|s| &s.distinct) {
        Some(DistinctSpec::All) => {
            // ORDER BY keys first, then every remaining visible slot, so that Sorted de-duplication sees
            // equal rows adjacent and the output stays ordered by the ORDER BY keys.
            let mut keys = sort_key_specs(&q.order_by, &cur.vars)?;
            if !keys.is_empty() {
                for i in 0..n_visible {
                    let e = cur.vars[i].clone();
                    if !keys.iter().any(|k| k.expr == e) {
                        keys.push(asc_key(e));
                    }
                }
                cur = wrap_sort(cur, keys);
            }
            let key_slots: Vec<u32> = (0..n_visible).map(slot_u32).collect::<Result<Vec<u32>>>()?;
            cur = wrap_distinct(cur, key_slots, false);
        }
        Some(DistinctSpec::On(idxs)) => {
            let keys = sort_key_specs(&q.order_by, &cur.vars)?;
            if !keys.is_empty() {
                cur = wrap_sort(cur, keys);
            }
            let width = cur.width();
            let mut key_slots: Vec<u32> = Vec::with_capacity(idxs.len());
            for &i in idxs {
                check_slot(i, width, "DISTINCT ON")?;
                key_slots.push(slot_u32(i)?);
            }
            cur = wrap_distinct(cur, key_slots, true);
        }
        _ => {
            let keys = sort_key_specs(&q.order_by, &cur.vars)?;
            if !keys.is_empty() {
                cur = wrap_sort(cur, keys);
            }
        }
    }
    if q.limit.is_some() || q.offset.is_some() || q.limit_with_ties {
        let ties_keys = if q.limit_with_ties { sort_key_specs(&q.order_by, &cur.vars)? } else { Vec::new() };
        cur = wrap_limit(cur, q.limit.clone(), q.offset.clone(), q.limit_with_ties, ties_keys);
    }
    if n_visible < cur.width() {
        let exprs: Vec<BoundExpr> = cur.vars[..n_visible].to_vec();
        let out = cur.out[..n_visible].to_vec();
        let rels = cur.rels.clone();
        let level = cur.level;
        cur = LogicalPlan {
            node: LogicalNode::Project(Box::new(LogicalProject { input: cur, exprs: exprs.clone(), n_visible, has_srf: false })),
            vars: exprs,
            out,
            rels,
            level,
        };
    }
    Ok(cur)
}

/// Convert bound `SortKey`s (target-list indexes) to executable key specs over `vars` (the projected
/// plan's output expressions): `SortKeySpec { expr: vars[key.target_idx].clone(), descending,
/// nulls_first, collation: 0 (or the expression's collation), using_op }`.
pub fn sort_key_specs(keys: &[SortKey], vars: &[BoundExpr]) -> Result<Vec<SortKeySpec>> {
    let mut out: Vec<SortKeySpec> = Vec::with_capacity(keys.len());
    for k in keys {
        let expr = match vars.get(k.target_idx) {
            Some(e) => e.clone(),
            None => return Err(PgError::internal(format!("ORDER BY refers to missing target entry {}", k.target_idx))),
        };
        out.push(SortKeySpec {
            expr,
            descending: k.descending,
            nulls_first: k.nulls_first,
            collation: 0,
            using_op: k.using_op.clone(),
        });
    }
    Ok(out)
}

/// Does an input with ordering `have` satisfy the requested ordering `want` (prefix semantics)?
pub fn ordering_satisfies(have: &[SortKeySpec], want: &[SortKeySpec]) -> Result<bool> {
    if want.len() > have.len() {
        return Ok(false);
    }
    Ok(want.iter().zip(have.iter()).all(|(w, h)| {
        h.expr == w.expr && h.descending == w.descending && h.nulls_first == w.nulls_first && h.using_op == w.using_op
    }))
}

/// Physical `Sort` (or the child itself when already ordered).
pub fn plan_sort(st: &mut PlannerState, s: &LogicalSort, top_n: Option<u64>) -> Result<PlanNode> {
    let child = plan_logical(st, &s.input)?;
    if ordering_satisfies(&child.ordering, &s.keys)? {
        return Ok(child);
    }
    Ok(sort_node(st, child, s.keys.clone(), top_n, s.input.level))
}

/// Physical `Limit`.
pub fn plan_limit(st: &mut PlannerState, l: &LogicalLimit) -> Result<PlanNode> {
    let top_n = top_n_for(l.with_ties, l.count.as_ref(), l.offset.as_ref());
    let child = match (&l.input.node, top_n) {
        (LogicalNode::Sort(s), Some(_)) => plan_sort(st, s, top_n)?,
        _ => plan_logical(st, &l.input)?,
    };
    let out = child.out.clone();
    let vars = child.vars.clone();
    let ordering = child.ordering.clone();
    let est = child.est;
    let kind = NodeKind::Limit(Box::new(LimitNode {
        input: child,
        count: l.count.clone(),
        offset: l.offset.clone(),
        with_ties: l.with_ties,
        ties_keys: l.ties_keys.clone(),
    }));
    let mut node = st.new_node(kind, out);
    node.vars = vars;
    node.est = est;
    node.ordering = ordering;
    node.level = l.input.level;
    Ok(node)
}

/// Physical `Distinct`.
pub fn plan_distinct(st: &mut PlannerState, d: &LogicalDistinct) -> Result<PlanNode> {
    let child = plan_logical(st, &d.input)?;
    let mut key_exprs: Vec<BoundExpr> = Vec::with_capacity(d.key_slots.len());
    for &slot in &d.key_slots {
        match child.vars.get(slot as usize) {
            Some(e) => key_exprs.push(e.clone()),
            None => return Err(PgError::internal(format!("DISTINCT key slot {slot} does not exist"))),
        }
    }
    let level = d.input.level;
    let (input, strategy) = if ordering_covers_set(&child.ordering, &key_exprs) {
        (child, DistinctStrategy::Sorted)
    } else if !d.on && st.ctx.options.enable_hashagg {
        (child, DistinctStrategy::Hashed)
    } else {
        let keys: Vec<SortKeySpec> = key_exprs.into_iter().map(asc_key).collect();
        (sort_node(st, child, keys, None, level), DistinctStrategy::Sorted)
    };
    let ordering = if strategy == DistinctStrategy::Sorted { input.ordering.clone() } else { Vec::new() };
    let out = input.out.clone();
    let vars = input.vars.clone();
    let est = input.est;
    let kind = NodeKind::Distinct(Box::new(DistinctNode { strategy, input, key_slots: d.key_slots.clone() }));
    let mut node = st.new_node(kind, out);
    node.vars = vars;
    node.est = est;
    node.ordering = ordering;
    node.level = level;
    Ok(node)
}

// ---------------------------------------------------------------------------------------------
// Private helpers
// ---------------------------------------------------------------------------------------------

/// Ascending, NULLS LAST key on `expr` (PG default for sort/distinct keys without explicit clauses).
fn asc_key(expr: BoundExpr) -> SortKeySpec {
    SortKeySpec { expr, descending: false, nulls_first: false, collation: 0, using_op: None }
}

fn slot_u32(i: usize) -> Result<u32> {
    u32::try_from(i).map_err(|_| PgError::internal("sort/distinct slot index out of range"))
}

fn check_slot(i: usize, width: usize, what: &str) -> Result<()> {
    if i < width {
        Ok(())
    } else {
        Err(PgError::internal(format!("{what} refers to missing target entry {i}")))
    }
}

/// True when the first `m` entries of `have` (m = number of distinct key expressions) are exactly the key
/// expressions as a set. Rows equal on the keys are then contiguous in an input sorted by `have`.
fn ordering_covers_set(have: &[SortKeySpec], keys: &[BoundExpr]) -> bool {
    let mut distinct: Vec<&BoundExpr> = Vec::with_capacity(keys.len());
    for k in keys {
        if !distinct.iter().any(|d| *d == k) {
            distinct.push(k);
        }
    }
    let m = distinct.len();
    if have.len() < m {
        return false;
    }
    let prefix = &have[..m];
    prefix.iter().all(|h| distinct.iter().any(|d| **d == h.expr))
        && distinct.iter().all(|d| prefix.iter().any(|h| h.expr == **d))
}

/// Top-N bound for a `Sort` under a `Limit` (PG `count + offset`): only for constant non-negative counts
/// and offsets without WITH TIES. NULL count = no limit; NULL offset = 0. Anything else returns `None`
/// (the executor reports 2201W / 2201X at run time).
fn top_n_for(with_ties: bool, count: Option<&BoundExpr>, offset: Option<&BoundExpr>) -> Option<u64> {
    if with_ties {
        return None;
    }
    let count: u64 = match count?.as_const()? {
        Datum::Int8(n) if *n >= 0 => *n as u64,
        _ => return None,
    };
    let offset: u64 = match offset {
        None => 0,
        Some(e) => match e.as_const() {
            Some(Datum::Null) => 0,
            Some(Datum::Int8(o)) if *o >= 0 => *o as u64,
            _ => return None,
        },
    };
    count.checked_add(offset)
}

/// Build a `Sort` physical node over `child` (keeps the child's output slots and bind-time vars).
fn sort_node(st: &PlannerState, child: PlanNode, keys: Vec<SortKeySpec>, limit: Option<u64>, level: u32) -> PlanNode {
    let out = child.out.clone();
    let vars = child.vars.clone();
    let est = child.est;
    let mut node = st.new_node(NodeKind::Sort(Box::new(SortNode { input: child, keys: keys.clone(), limit })), out);
    node.vars = vars;
    node.est = est;
    node.ordering = keys;
    node.level = level;
    node
}

/// Wrap `input` in a logical node that passes its output slots (vars, out, rels, level) through unchanged.
fn wrap_plan(input: LogicalPlan, node: impl FnOnce(LogicalPlan) -> LogicalNode) -> LogicalPlan {
    let vars = input.vars.clone();
    let out = input.out.clone();
    let rels = input.rels.clone();
    let level = input.level;
    LogicalPlan { node: node(input), vars, out, rels, level }
}

fn wrap_sort(input: LogicalPlan, keys: Vec<SortKeySpec>) -> LogicalPlan {
    wrap_plan(input, |input| LogicalNode::Sort(Box::new(LogicalSort { input, keys })))
}

fn wrap_distinct(input: LogicalPlan, key_slots: Vec<u32>, on: bool) -> LogicalPlan {
    wrap_plan(input, |input| LogicalNode::Distinct(Box::new(LogicalDistinct { input, key_slots, on })))
}

fn wrap_limit(input: LogicalPlan, count: Option<BoundExpr>, offset: Option<BoundExpr>, with_ties: bool, ties_keys: Vec<SortKeySpec>) -> LogicalPlan {
    wrap_plan(input, |input| LogicalNode::Limit(Box::new(LogicalLimit { input, count, offset, with_ties, ties_keys })))
}

#[cfg(test)]
mod tests {
    use super::*;
    use pgarbor_sql::ir::OpRef;
    use pgarbor_types::TypeId;
    use pgarbor_types::catalog_data::Volatility;

    fn col(attnum: i16) -> BoundExpr {
        BoundExpr::column(0, 1, attnum, TypeId::of(23))
    }

    fn key(e: BoundExpr, descending: bool) -> SortKeySpec {
        SortKeySpec { expr: e, descending, nulls_first: descending, collation: 0, using_op: None }
    }

    #[test]
    fn ordering_prefix_satisfied() {
        let have = vec![key(col(1), false), key(col(2), false)];
        assert!(ordering_satisfies(&have, &[key(col(1), false)]).unwrap());
        assert!(ordering_satisfies(&have, &have).unwrap());
        assert!(ordering_satisfies(&have, &[]).unwrap());
    }

    #[test]
    fn ordering_mismatch_not_satisfied() {
        let have = vec![key(col(1), false)];
        // wrong column
        assert!(!ordering_satisfies(&have, &[key(col(2), false)]).unwrap());
        // wrong direction
        assert!(!ordering_satisfies(&have, &[key(col(1), true)]).unwrap());
        // longer than the child ordering
        assert!(!ordering_satisfies(&have, &[key(col(1), false), key(col(2), false)]).unwrap());
        // not a prefix
        let have2 = vec![key(col(1), false), key(col(2), false)];
        assert!(!ordering_satisfies(&have2, &[key(col(2), false)]).unwrap());
    }

    #[test]
    fn ordering_using_op_must_match() {
        let op = OpRef {
            op: 97,
            proc: 65,
            name: "<".to_string(),
            volatility: Volatility::Immutable,
            strict: true,
        };
        let have = vec![key(col(1), false)];
        let mut want = key(col(1), false);
        want.using_op = Some(op);
        assert!(!ordering_satisfies(&have, &[want]).unwrap());
    }

    #[test]
    fn covers_set_requires_exact_prefix_set() {
        let have = vec![key(col(1), false), key(col(2), false)];
        assert!(ordering_covers_set(&have, &[col(1)]));
        assert!(ordering_covers_set(&have, &[col(2), col(1)]));
        // the prefix of length 1 is [a], so {b} is not covered
        assert!(!ordering_covers_set(&have, &[col(2)]));
        // more keys than ordering entries
        assert!(!ordering_covers_set(&have, &[col(1), col(2), col(3)]));
        // duplicate key expressions count once
        assert!(ordering_covers_set(&have, &[col(1), col(1)]));
    }

    #[test]
    fn sort_key_specs_maps_target_entries() {
        let vars = vec![col(1), col(2)];
        let keys = vec![SortKey { target_idx: 1, descending: true, nulls_first: true, using_op: None }];
        let specs = sort_key_specs(&keys, &vars).unwrap();
        assert_eq!(specs.len(), 1);
        assert_eq!(specs[0].expr, col(2));
        assert!(specs[0].descending && specs[0].nulls_first);
        assert_eq!(specs[0].collation, 0);
        // missing target entry is an internal error, never a panic
        let bad = vec![SortKey { target_idx: 5, descending: false, nulls_first: false, using_op: None }];
        assert!(sort_key_specs(&bad, &vars).is_err());
    }

    #[test]
    fn top_n_is_count_plus_offset() {
        let ten = BoundExpr::constant(Datum::Int8(10), TypeId::of(20));
        let five = BoundExpr::constant(Datum::Int8(5), TypeId::of(20));
        let null = BoundExpr::null(TypeId::of(20));
        let neg = BoundExpr::constant(Datum::Int8(-1), TypeId::of(20));
        assert_eq!(top_n_for(false, Some(&ten), Some(&five)), Some(15));
        assert_eq!(top_n_for(false, Some(&ten), None), Some(10));
        assert_eq!(top_n_for(false, Some(&ten), Some(&null)), Some(10));
        assert_eq!(top_n_for(false, None, None), None);
        assert_eq!(top_n_for(false, Some(&null), None), None);
        assert_eq!(top_n_for(false, Some(&neg), None), None);
        assert_eq!(top_n_for(false, Some(&ten), Some(&neg)), None);
        assert_eq!(top_n_for(true, Some(&ten), Some(&five)), None);
    }
}
