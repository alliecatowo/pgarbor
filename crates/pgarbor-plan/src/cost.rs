//! Cost model and cardinality estimates (PLN-12). PGArbor keeps NO table statistics in v1
//! (`relpages`/`reltuples` are static), so estimates are rule based; they only steer join order,
//! hash build side and strategy choices and fill the `(cost=.. rows=.. width=..)` EXPLAIN annotations.
//! EXPLAIN costs are NOT expected to equal PostgreSQL's (differential tests run with `COSTS OFF`).
//! PG references for the formulas: `optimizer/path/costsize.c` (`cost_seqscan`, `cost_index`,
//! `cost_sort`, `cost_agg`, `final_cost_hashjoin`, ...), `utils/adt/selfuncs.c` default selectivities
//! (`DEFAULT_EQ_SEL` 0.005, `DEFAULT_INEQ_SEL` 0.3333333333333333, `DEFAULT_RANGE_INEQ_SEL` 0.005,
//! `DEFAULT_MATCH_SEL` 0.005, `DEFAULT_NUM_DISTINCT` 200, `DEFAULT_UNK_SEL` 0.005).
//!
//! * [`table_rows`]: `PlanContext::table_rows[oid]` when present, else `options.default_table_rows` (1000); virtual catalog relations 100;
//!   `VALUES` the row count; function scans `1000` for SRFs (`BuiltinProc`/`prorows` if known), 1 for
//!   scalar functions; sub-queries their plan's estimate; CTE scans the CTE estimate.
//! * [`selectivity`]: `=` against a constant 0.005 (full-key match of a unique index: `1/rows`), `<>`
//!   0.995, `<`,`<=`,`>`,`>=` 1/3, a pair of opposing inequalities on one column 0.005, `LIKE`/`~~`/regex
//!   match operators 0.005, `IS NULL` 0.005, `IS NOT NULL` 0.995, `x = ANY(array of n)` `min(1, 0.005 n)`,
//!   `AND` multiplies, `OR` is `s1 + s2 - s1*s2`, `NOT` is `1 - s`, constant `true` 1 and `false`/NULL 0,
//!   anything else 0.005. Clamp to [0, 1].
//! * Costs (arbitrary but monotone): `cpu_tuple_cost 0.01`, `cpu_operator_cost 0.0025`, `seq_page_cost 1.0`
//!   (pages = rows * width / 8192), `random_page_cost 4.0`, `cpu_index_tuple_cost 0.005`; sort
//!   `2 * cpu_operator_cost * n * log2(n)`; hash join build/probe linear; nested loop `outer_rows *
//!   inner_cost`.
//! All functions are pure over their arguments.
use pgarbor_common::{PgError, Result};
use pgarbor_sql::ir::{BoolOp, BoundExpr, ColumnRef, ExprKind, OpExpr};
use pgarbor_types::Datum;

use crate::context::PlannerState;
use crate::logical::{LogicalNode, LogicalPlan, LogicalScan, LogicalSetKind, ScanSource};
use crate::plan::{CteBodyPlan, Estimate, JoinType, SetOpCmd};

// PG `utils/adt/selfuncs.c` default selectivities and `optimizer/path/costsize.c` constants.
const DEFAULT_EQ_SEL: f64 = 0.005;
const DEFAULT_INEQ_SEL: f64 = 1.0 / 3.0;
const DEFAULT_RANGE_INEQ_SEL: f64 = 0.005;
const DEFAULT_MATCH_SEL: f64 = 0.005;
const DEFAULT_UNK_SEL: f64 = 0.005;
const NOT_EQ_SEL: f64 = 0.995;
const DEFAULT_NUM_DISTINCT: f64 = 200.0;
/// Row estimate of a virtual catalog relation without a row-count hint.
const VIRTUAL_REL_ROWS: f64 = 100.0;
/// Row estimate of a set-returning function (PG `prorows` default for SRFs).
const SRF_ROWS: f64 = 1000.0;
const CPU_TUPLE_COST: f64 = 0.01;
const CPU_OPERATOR_COST: f64 = 0.0025;
const SEQ_PAGE_COST: f64 = 1.0;
const RANDOM_PAGE_COST: f64 = 4.0;
const CPU_INDEX_TUPLE_COST: f64 = 0.005;
const BLCKSZ: f64 = 8192.0;

/// Replace a non-finite or non-positive row estimate by 1 (PG clamps row estimates to at least one).
fn sane_rows(x: f64) -> f64 {
    if x.is_finite() && x > 0.0 {
        x
    } else {
        1.0
    }
}

/// Estimated row count of a scan before its quals.
pub fn table_rows(st: &PlannerState, scan: &LogicalScan) -> Result<f64> {
    let opts = &st.ctx.options;
    match &scan.source {
        ScanSource::Table { table, is_virtual } => {
            if let Some(hint) = st.ctx.table_rows.get(&table.oid) {
                if hint.is_finite() && *hint >= 0.0 {
                    return Ok(*hint);
                }
            }
            if *is_virtual {
                Ok(VIRTUAL_REL_ROWS)
            } else {
                Ok(opts.default_table_rows)
            }
        }
        ScanSource::Subquery(plan) => logical_rows(st, plan),
        ScanSource::Function { funcs, .. } => {
            // Zipped functions: the longest one decides the row count.
            let mut rows = 1.0_f64;
            for item in funcs {
                let r = match &item.expr.kind {
                    ExprKind::FuncCall(fc) if fc.retset => SRF_ROWS,
                    _ => 1.0,
                };
                if r > rows {
                    rows = r;
                }
            }
            Ok(rows)
        }
        ScanSource::Values { rows } => Ok(rows.len() as f64),
        ScanSource::Cte { cte_id } => match st.ctes.get(*cte_id as usize) {
            Some(cte) => Ok(match &cte.body {
                CteBodyPlan::Query(n) | CteBodyPlan::Modify(n) => n.est.rows,
                CteBodyPlan::Recursive { non_recursive, .. } => non_recursive.est.rows,
            }),
            None => Ok(opts.default_table_rows),
        },
        // The working table of a recursive CTE has no estimate of its own: use the default.
        ScanSource::WorkTable { .. } => Ok(opts.default_table_rows),
        ScanSource::OneRow => Ok(1.0),
    }
}

/// Estimated rows of a scan after its own quals (`table_rows * selectivity(quals)`, at least 1).
pub fn estimate_scan_rows(st: &PlannerState, scan: &LogicalScan) -> Result<f64> {
    let base = table_rows(st, scan)?;
    let sel = conjunction_selectivity(&scan.quals);
    Ok((base * sel).max(1.0))
}

/// Selectivity of one boolean expression in [0, 1] (module docs).
pub fn selectivity(e: &BoundExpr) -> Result<f64> {
    Ok(sel_of(e).clamp(0.0, 1.0))
}

/// Infallible selectivity used by the recursive walk (module docs).
fn sel_of(e: &BoundExpr) -> f64 {
    match &e.kind {
        ExprKind::Const(Datum::Bool(true)) => 1.0,
        ExprKind::Const(Datum::Bool(false)) | ExprKind::Const(Datum::Null) => 0.0,
        ExprKind::Bool(b) => match b.op {
            BoolOp::And => conjunction_selectivity(&b.args),
            BoolOp::Or => {
                let mut s = 0.0_f64;
                for a in &b.args {
                    let sa = sel_of(a);
                    s = s + sa - s * sa;
                }
                s
            }
            BoolOp::Not => match b.args.as_slice() {
                [a] => 1.0 - sel_of(a),
                _ => DEFAULT_UNK_SEL,
            },
        },
        ExprKind::Op(o) => op_selectivity(o),
        ExprKind::NullTest(nt) => {
            if nt.is_null {
                DEFAULT_EQ_SEL
            } else {
                NOT_EQ_SEL
            }
        }
        ExprKind::ScalarArrayOp(s) => {
            if s.use_or && s.op.name == "=" {
                match &s.right.kind {
                    ExprKind::Array(arr) => (DEFAULT_EQ_SEL * arr.elements.len() as f64).min(1.0),
                    _ => DEFAULT_EQ_SEL,
                }
            } else {
                DEFAULT_UNK_SEL
            }
        }
        _ => DEFAULT_UNK_SEL,
    }
}

/// Selectivity of a binary operator application by its operator name.
fn op_selectivity(o: &OpExpr) -> f64 {
    if o.args.len() != 2 {
        return DEFAULT_UNK_SEL;
    }
    match o.op.name.as_str() {
        "=" => DEFAULT_EQ_SEL,
        "<>" => NOT_EQ_SEL,
        "<" | "<=" | ">" | ">=" => DEFAULT_INEQ_SEL,
        "~~" | "!~~" | "~~*" | "!~~*" | "~" | "!~" | "~*" | "!~*" => DEFAULT_MATCH_SEL,
        _ => DEFAULT_UNK_SEL,
    }
}

/// Which side of a column a range comparison bounds.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Bound {
    Lower,
    Upper,
}

fn flip(b: Bound) -> Bound {
    match b {
        Bound::Lower => Bound::Upper,
        Bound::Upper => Bound::Lower,
    }
}

/// True for operands that are constants or parameters (the "other side" of a `var op const` qual).
fn is_pseudo_const(e: &BoundExpr) -> bool {
    matches!(e.kind, ExprKind::Const(_) | ExprKind::Param(_))
}

/// `col < const`, `const < col` and the other inequality forms: the column and the bound it sets.
fn range_bound(e: &BoundExpr) -> Option<(ColumnRef, Bound)> {
    let o = match &e.kind {
        ExprKind::Op(o) => o,
        _ => return None,
    };
    let dir = match o.op.name.as_str() {
        ">" | ">=" => Bound::Lower,
        "<" | "<=" => Bound::Upper,
        _ => return None,
    };
    match o.args.as_slice() {
        [l, r] => {
            if let ExprKind::Column(c) = &l.kind {
                if is_pseudo_const(r) {
                    return Some((*c, dir));
                }
            }
            if let ExprKind::Column(c) = &r.kind {
                if is_pseudo_const(l) {
                    return Some((*c, flip(dir)));
                }
            }
            None
        }
        _ => None,
    }
}

/// Selectivity of an AND-ed list. A lower and an upper bound on the same column together count as one
/// range (`DEFAULT_RANGE_INEQ_SEL`) instead of two inequalities (PG `clauselist_selectivity`).
fn conjunction_selectivity(args: &[BoundExpr]) -> f64 {
    let n = args.len();
    let ranges: Vec<Option<(ColumnRef, Bound)>> = args.iter().map(range_bound).collect();
    let mut used = vec![false; n];
    let mut sel = 1.0_f64;
    for i in 0..n {
        if used.get(i).copied().unwrap_or(true) {
            continue;
        }
        let (ci, di) = match ranges.get(i).copied().flatten() {
            Some(r) => r,
            None => continue,
        };
        let partner = (i + 1..n).find(|&j| {
            !used.get(j).copied().unwrap_or(true)
                && matches!(ranges.get(j).copied().flatten(), Some((cj, dj)) if cj == ci && dj != di)
        });
        if let Some(j) = partner {
            if let Some(slot) = used.get_mut(i) {
                *slot = true;
            }
            if let Some(slot) = used.get_mut(j) {
                *slot = true;
            }
            sel *= DEFAULT_RANGE_INEQ_SEL;
        }
    }
    for (k, a) in args.iter().enumerate() {
        if used.get(k).copied().unwrap_or(false) {
            continue;
        }
        sel *= sel_of(a);
    }
    sel.clamp(0.0, 1.0)
}

/// Estimated number of groups for `n_keys` grouping keys over `input_rows` rows.
pub fn estimate_groups(input_rows: f64, n_keys: usize) -> Result<f64> {
    if n_keys == 0 {
        // Aggregate without GROUP BY: exactly one output row.
        return Ok(1.0);
    }
    let input = sane_rows(input_rows);
    let mut groups = 1.0_f64;
    for _ in 0..n_keys {
        groups *= DEFAULT_NUM_DISTINCT;
        if groups >= input {
            break;
        }
    }
    Ok(groups.min(input).max(1.0))
}

/// Estimated join output rows (`inner join: l*r*sel`, outer joins at least the preserved side).
pub fn join_rows(left_rows: f64, right_rows: f64, join_type: JoinType, equi_keys: usize, other_quals: usize) -> Result<f64> {
    let l = sane_rows(left_rows);
    let r = sane_rows(right_rows);
    // Every join clause contributes its default selectivity: `=` 0.005, other quals 0.005.
    let sel = DEFAULT_EQ_SEL.powf(equi_keys as f64) * DEFAULT_UNK_SEL.powf(other_quals as f64);
    let inner = (l * r * sel).max(1.0);
    let rows = match join_type {
        JoinType::Inner => inner,
        JoinType::Left => inner.max(l),
        JoinType::Right => inner.max(r),
        JoinType::Full => inner.max(l).max(r),
        JoinType::Semi | JoinType::Anti => {
            // Probability that an outer row finds at least one inner match.
            let p = (r * sel).min(1.0);
            let kept = match join_type {
                JoinType::Semi => l * p,
                _ => l * (1.0 - p),
            };
            kept.max(1.0)
        }
    };
    Ok(rows)
}

/// Count `(equi, other)` join clauses: `=` between two operands is an equi key.
fn count_equi(quals: &[BoundExpr]) -> (usize, usize) {
    let mut eq: usize = 0;
    let mut other: usize = 0;
    for q in quals {
        match &q.kind {
            ExprKind::Op(o) if o.op.name == "=" && o.args.len() == 2 => eq += 1,
            _ => other += 1,
        }
    }
    (eq, other)
}

/// Estimated output rows of a logical plan (used for sub-queries and CTE-free inputs).
fn logical_rows(st: &PlannerState, plan: &LogicalPlan) -> Result<f64> {
    match &plan.node {
        LogicalNode::Scan(s) => estimate_scan_rows(st, s),
        LogicalNode::Join(j) => {
            let l = logical_rows(st, &j.left)?;
            let r = logical_rows(st, &j.right)?;
            let (eq, other) = count_equi(&j.quals);
            join_rows(l, r, j.join_type, eq, other)
        }
        LogicalNode::Filter(f) => {
            let r = logical_rows(st, &f.input)?;
            Ok((r * conjunction_selectivity(&f.quals)).max(1.0))
        }
        LogicalNode::Aggregate(a) => {
            let input = logical_rows(st, &a.input)?;
            let mut groups = estimate_groups(input, a.group_exprs.len())?;
            if let Some(sets) = &a.grouping_sets {
                groups *= sets.len().max(1) as f64;
            }
            if !a.having.is_empty() {
                groups = (groups * conjunction_selectivity(&a.having)).max(1.0);
            }
            Ok(groups)
        }
        LogicalNode::Window(w) => logical_rows(st, &w.input),
        LogicalNode::Project(p) => logical_rows(st, &p.input),
        LogicalNode::Sort(s) => logical_rows(st, &s.input),
        LogicalNode::LockRows(lr) => logical_rows(st, &lr.input),
        LogicalNode::Limit(lim) => {
            let input = logical_rows(st, &lim.input)?;
            if let Some(count) = &lim.count {
                if let ExprKind::Const(Datum::Int8(n)) = &count.kind {
                    if *n >= 0 {
                        return Ok(input.min(*n as f64).max(1.0));
                    }
                }
            }
            Ok(input)
        }
        LogicalNode::Distinct(d) => {
            let input = logical_rows(st, &d.input)?;
            estimate_groups(input, d.key_slots.len())
        }
        LogicalNode::SetOp(s) => {
            let l = logical_rows(st, &s.left)?;
            let r = logical_rows(st, &s.right)?;
            match s.kind {
                LogicalSetKind::UnionAll => Ok(l + r),
                LogicalSetKind::Setop(SetOpCmd::Intersect) | LogicalSetKind::Setop(SetOpCmd::IntersectAll) => {
                    Ok(l.min(r))
                }
                LogicalSetKind::Setop(_) => Ok(l),
            }
        }
    }
}

/// Cost estimate of common node shapes: returns `Estimate` with startup/total cost filled from the
/// child estimates. `shape` names the node: "seqscan", "indexscan", "filter", "project", "sort",
/// "hashagg", "groupagg", "hashjoin", "mergejoin", "nestloop", "limit", "append", "unique", "setop",
/// "window", "materialize", "values", "function", "result".
///
/// Child order: joins take `[outer, inner]` (the hash build side is the inner child); missing children
/// count as zero-cost, zero-startup inputs with one row.
pub fn cost_node(shape: &str, rows: f64, width: u32, children: &[Estimate]) -> Result<Estimate> {
    let rows = sane_rows(rows);
    let c0 = children.first().copied().unwrap_or_default();
    let c1 = children.get(1).copied().unwrap_or_default();
    let (startup, total) = match shape {
        "seqscan" => {
            // Pages from the output width; one page-read per page plus one tuple cost per row.
            let pages = rows * width as f64 / BLCKSZ;
            (0.0, pages * SEQ_PAGE_COST + rows * CPU_TUPLE_COST)
        }
        "indexscan" => {
            // Descend the tree once (startup), then per row an index tuple, a heap tuple and a heap page.
            let s = RANDOM_PAGE_COST;
            (s, s + rows * (CPU_INDEX_TUPLE_COST + CPU_TUPLE_COST + RANDOM_PAGE_COST))
        }
        "filter" => (c0.startup_cost, c0.total_cost + c0.rows * CPU_OPERATOR_COST),
        "project" => (c0.startup_cost, c0.total_cost + c0.rows * CPU_OPERATOR_COST),
        "sort" => {
            // Blocking: all input is consumed before the first row; cost 2 * cpu_operator * n * log2(n).
            let n = rows.max(1.0);
            let sort_cost = 2.0 * CPU_OPERATOR_COST * n * n.log2();
            let startup = c0.total_cost + sort_cost;
            (startup, startup + rows * CPU_TUPLE_COST)
        }
        "hashagg" => {
            let startup = c0.total_cost + c0.rows * CPU_OPERATOR_COST;
            (startup, startup + rows * CPU_TUPLE_COST)
        }
        "groupagg" => (c0.startup_cost, c0.total_cost + c0.rows * CPU_OPERATOR_COST + rows * CPU_TUPLE_COST),
        "hashjoin" => {
            // Build the hash table from the inner (build) child, then probe with the outer child.
            let startup = c1.total_cost + c1.rows * CPU_OPERATOR_COST;
            (startup, startup + c0.total_cost + c0.rows * CPU_OPERATOR_COST + rows * CPU_TUPLE_COST)
        }
        "mergejoin" => (
            c0.startup_cost + c1.startup_cost,
            c0.total_cost + c1.total_cost + (c0.rows + c1.rows) * CPU_OPERATOR_COST + rows * CPU_TUPLE_COST,
        ),
        "nestloop" => {
            // The inner child is re-scanned once per outer row.
            (c0.startup_cost + c1.startup_cost, c0.total_cost + c0.rows * c1.total_cost + rows * CPU_TUPLE_COST)
        }
        "limit" => {
            // Only the fraction of the input actually consumed is paid for after startup.
            let frac = if c0.rows > 0.0 { (rows / c0.rows).min(1.0) } else { 1.0 };
            let span = (c0.total_cost - c0.startup_cost).max(0.0);
            (c0.startup_cost, c0.startup_cost + span * frac)
        }
        "append" => {
            let startup = children.first().map(|c| c.startup_cost).unwrap_or(0.0);
            let total: f64 = children.iter().map(|c| c.total_cost).sum();
            (startup, total + rows * CPU_TUPLE_COST)
        }
        "unique" => (c0.startup_cost, c0.total_cost + c0.rows * CPU_OPERATOR_COST),
        "setop" => {
            let input_rows: f64 = children.iter().map(|c| c.rows).sum();
            let input_cost: f64 = children.iter().map(|c| c.total_cost).sum();
            let startup = input_cost + input_rows * CPU_OPERATOR_COST;
            (startup, startup + rows * CPU_TUPLE_COST)
        }
        "window" => {
            let startup = c0.total_cost + c0.rows * CPU_OPERATOR_COST;
            (startup, startup + rows * CPU_TUPLE_COST)
        }
        "materialize" => (c0.startup_cost, c0.total_cost + c0.rows * CPU_OPERATOR_COST),
        "values" => (0.0, rows * CPU_OPERATOR_COST),
        "function" => (0.0, rows * CPU_TUPLE_COST),
        "result" => (0.0, rows * CPU_OPERATOR_COST),
        _ => return Err(PgError::internal(format!("cost_node: unknown node shape '{}'", shape))),
    };
    Ok(Estimate { startup_cost: startup, total_cost: total, rows, width })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::context::PlanContext;
    use crate::context::PlanOptions;
    use crate::plan::TableRef;
    use pgarbor_catalog::testing;
    use pgarbor_sql::ir::{ArrayExpr, BoolExpr, NullTest, OpRef, ScalarArrayOp};
    use pgarbor_types::catalog_data::Volatility;
    use pgarbor_types::TypeId;

    fn close(a: f64, b: f64) -> bool {
        (a - b).abs() < 1e-9
    }

    fn col(attnum: i16) -> BoundExpr {
        BoundExpr::new(ExprKind::Column(ColumnRef { levels_up: 0, rt_index: 1, attnum, pos: 0 }), TypeId::INT4)
    }

    fn int(v: i32) -> BoundExpr {
        BoundExpr::constant(Datum::Int4(v), TypeId::INT4)
    }

    fn opx(name: &str, l: BoundExpr, r: BoundExpr) -> BoundExpr {
        let op = OpRef { op: 0, proc: 0, name: name.to_string(), volatility: Volatility::Immutable, strict: true };
        BoundExpr::new(ExprKind::Op(Box::new(OpExpr { op, args: vec![l, r] })), TypeId::BOOL)
    }

    fn and(args: Vec<BoundExpr>) -> BoundExpr {
        BoundExpr::new(ExprKind::Bool(Box::new(BoolExpr { op: BoolOp::And, args })), TypeId::BOOL)
    }

    fn or(args: Vec<BoundExpr>) -> BoundExpr {
        BoundExpr::new(ExprKind::Bool(Box::new(BoolExpr { op: BoolOp::Or, args })), TypeId::BOOL)
    }

    fn scan_of(oid: u32, is_virtual: bool, quals: Vec<BoundExpr>) -> LogicalScan {
        let table = TableRef {
            oid,
            schema: "public".to_string(),
            name: "t".to_string(),
            alias: "t".to_string(),
            ncols: 1,
            col_types: vec![TypeId::INT4],
        };
        LogicalScan { rt_index: 1, source: ScanSource::Table { table, is_virtual }, quals, with_rowid: false }
    }

    #[test]
    fn selectivity_documented_values() -> Result<()> {
        assert!(close(selectivity(&opx("=", col(1), int(5)))?, 0.005));
        assert!(close(selectivity(&opx(">", col(1), int(1)))?, 1.0 / 3.0));
        assert!(close(selectivity(&opx("<>", col(1), int(1)))?, 0.995));
        assert!(close(selectivity(&opx("~~", col(1), int(1)))?, 0.005));
        assert!(close(selectivity(&opx("+", col(1), int(1)))?, 0.005));
        assert!(close(selectivity(&and(vec![opx(">", col(1), int(1)), opx("<", col(1), int(9))]))?, 0.005));
        // Reversed operands: 1 < a is a lower bound on a, so it pairs with a < 9.
        assert!(close(selectivity(&and(vec![opx("<", int(1), col(1)), opx("<", col(1), int(9))]))?, 0.005));
        // Different columns do not pair.
        assert!(close(selectivity(&and(vec![opx(">", col(1), int(1)), opx("<", col(2), int(9))]))?, 1.0 / 9.0));
        assert!(close(selectivity(&BoundExpr::bool_const(true))?, 1.0));
        assert!(close(selectivity(&BoundExpr::bool_const(false))?, 0.0));
        assert!(close(selectivity(&BoundExpr::null(TypeId::BOOL))?, 0.0));
        Ok(())
    }

    #[test]
    fn selectivity_boolean_connectives_and_null_tests() -> Result<()> {
        let eq = || opx("=", col(1), int(5));
        assert!(close(selectivity(&and(vec![eq(), opx(">", col(2), int(1))]))?, 0.005 / 3.0));
        assert!(close(selectivity(&or(vec![eq(), eq()]))?, 0.005 + 0.005 - 0.005 * 0.005));
        let not = BoundExpr::new(ExprKind::Bool(Box::new(BoolExpr { op: BoolOp::Not, args: vec![eq()] })), TypeId::BOOL);
        assert!(close(selectivity(&not)?, 0.995));
        let is_null = BoundExpr::new(
            ExprKind::NullTest(Box::new(NullTest { arg: col(1), is_null: true, row_semantics: false })),
            TypeId::BOOL,
        );
        assert!(close(selectivity(&is_null)?, 0.005));
        let is_not_null = BoundExpr::new(
            ExprKind::NullTest(Box::new(NullTest { arg: col(1), is_null: false, row_semantics: false })),
            TypeId::BOOL,
        );
        assert!(close(selectivity(&is_not_null)?, 0.995));
        Ok(())
    }

    #[test]
    fn selectivity_scalar_array_op() -> Result<()> {
        let elems = vec![int(1), int(2), int(3)];
        let arr = BoundExpr::new(
            ExprKind::Array(Box::new(ArrayExpr { elements: elems, elem_type: 23, multidims: false })),
            TypeId::INT4_ARRAY,
        );
        let op = OpRef { op: 0, proc: 0, name: "=".to_string(), volatility: Volatility::Immutable, strict: true };
        let any = BoundExpr::new(
            ExprKind::ScalarArrayOp(Box::new(ScalarArrayOp { op, use_or: true, left: col(1), right: arr })),
            TypeId::BOOL,
        );
        assert!(close(selectivity(&any)?, 0.015));
        Ok(())
    }

    #[test]
    fn table_rows_hint_virtual_and_default() -> Result<()> {
        let (_kv, env) = testing::bootstrapped()?;
        let mut ctx = PlanContext {
            env: &env,
            param_types: Vec::new(),
            param_values: None,
            options: PlanOptions::default(),
            table_rows: std::collections::HashMap::new(),
        };
        let user = scan_of(16384, false, Vec::new());
        let virt = scan_of(100, true, Vec::new());
        {
            let st = PlannerState::new(&ctx);
            assert!(close(table_rows(&st, &user)?, 1000.0));
            assert!(close(table_rows(&st, &virt)?, 100.0));
        }
        ctx.table_rows.insert(16384, 42.0);
        let st = PlannerState::new(&ctx);
        assert!(close(table_rows(&st, &user)?, 42.0));
        Ok(())
    }

    #[test]
    fn scan_rows_apply_quals_with_minimum_one() -> Result<()> {
        let (_kv, env) = testing::bootstrapped()?;
        let ctx = PlanContext {
            env: &env,
            param_types: Vec::new(),
            param_values: None,
            options: PlanOptions::default(),
            table_rows: std::collections::HashMap::new(),
        };
        let st = PlannerState::new(&ctx);
        let range = scan_of(16384, false, vec![opx(">", col(1), int(1)), opx("<", col(1), int(9))]);
        assert!(close(estimate_scan_rows(&st, &range)?, 5.0));
        let eq = scan_of(16384, false, vec![opx("=", col(1), int(5))]);
        assert!(close(estimate_scan_rows(&st, &eq)?, 5.0));
        let virt_eq = scan_of(100, true, vec![opx("=", col(1), int(5))]);
        assert!(close(estimate_scan_rows(&st, &virt_eq)?, 1.0));
        Ok(())
    }

    #[test]
    fn join_rows_documented_values() -> Result<()> {
        assert!(close(join_rows(1000.0, 1000.0, JoinType::Inner, 1, 0)?, 5000.0));
        assert!(close(join_rows(1.0, 1.0, JoinType::Inner, 1, 0)?, 1.0));
        // Outer join keeps the preserved side.
        assert!(close(join_rows(10.0, 10.0, JoinType::Left, 1, 0)?, 10.0));
        assert!(close(join_rows(10.0, 1000.0, JoinType::Right, 1, 0)?, 1000.0));
        assert!(close(join_rows(10.0, 1000.0, JoinType::Full, 1, 0)?, 1000.0));
        // Semi join never exceeds the outer side.
        assert!(join_rows(1000.0, 1000.0, JoinType::Semi, 1, 0)? <= 1000.0);
        Ok(())
    }

    #[test]
    fn groups_default_distinct() -> Result<()> {
        assert!(close(estimate_groups(1000.0, 0)?, 1.0));
        assert!(close(estimate_groups(1000.0, 1)?, 200.0));
        assert!(close(estimate_groups(1000.0, 2)?, 1000.0));
        assert!(close(estimate_groups(0.0, 1)?, 1.0));
        Ok(())
    }

    #[test]
    fn sort_cost_grows_n_log_n() -> Result<()> {
        let s1000 = cost_node("sort", 1000.0, 8, &[])?;
        let s100 = cost_node("sort", 100.0, 8, &[])?;
        assert!(s1000.startup_cost > s100.startup_cost);
        assert!(close(s1000.startup_cost, 2.0 * 0.0025 * 1000.0 * 1000f64.log2()));
        assert!(s1000.total_cost > s100.total_cost);
        assert_eq!(s1000.width, 8);
        Ok(())
    }

    #[test]
    fn seqscan_and_unknown_shape() -> Result<()> {
        let e = cost_node("seqscan", 1000.0, 100, &[])?;
        assert!(close(e.total_cost, 1000.0 * 100.0 / 8192.0 + 1000.0 * 0.01));
        assert!(close(e.rows, 1000.0));
        assert!(cost_node("no-such-shape", 1.0, 0, &[]).is_err());
        Ok(())
    }
}
