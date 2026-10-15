//! Predicate pushdown and outer-join reduction (PLN-03). PG references:
//! `optimizer/plan/initsplan.c` (`distribute_qual_to_rels`, `check_outerjoin_delay`,
//! `deconstruct_recurse`), `optimizer/prep/prepjointree.c` (`reduce_outer_joins`,
//! `find_nonnullable_rels`, `find_forced_null_vars`), `optimizer/path/equivclass.c` (equivalence classes,
//! simplified here to constant propagation), `util/clauses.c` (`contain_nonstrict_functions`).
//!
//! ## `push_down_quals(st, plan, quals)`
//! Distributes AND-ed conjuncts (bind-time expressions) into `plan` (a join tree of scans):
//! * `expr_relids(q)` = the set of `rt_index`es of `Column{levels_up: 0}` nodes in `q` (columns of
//!   enclosing queries and params do not count). A qual with no relids (constant) stays at the top.
//! * A qual may be evaluated at a node whose `rels` is a SUPERSET of its relids, as deep as legal:
//!   - leaf scan: becomes one of `LogicalScan::quals`;
//!   - INNER join: if it references both sides it is a join qual (`LogicalJoin::quals`), else pushed into
//!     the side it references;
//!   - LEFT join: a WHERE qual referencing only the preserved (left) side is pushed to the left; a
//!     qual referencing the nullable (right) side must stay ABOVE the join as a `LogicalFilter` unless
//!     the join is first reduced (below); ON quals referencing only the nullable side are pushed into the
//!     right input; ON quals referencing only the preserved side stay in the join `quals` (they do
//!     not filter preserved rows);
//!   - RIGHT mirrors LEFT; FULL: nothing crosses (all WHERE quals stay above);
//!   - never push a qual containing volatile functions below a point where it would be evaluated a
//!     different number of times (keep volatile quals at/above the lowest join covering their relids
//!     and never below an outer join's nullable side); never push below `Aggregate`/`Limit`/
//!     `Distinct`/`Window`/set-ops here (this function only walks the join tree).
//! * **Outer join reduction**: a WHERE qual that is *strict* (returns NULL/false when all its columns
//!   from the nullable side are NULL: comparison/arithmetic operators and strict functions of such
//!   columns, `x IS NOT NULL`, but NOT `COALESCE`/`IS NULL`/`CASE`/non-strict functions) on a column of
//!   the nullable side converts `LEFT`->`INNER` (`RIGHT`->`INNER`, `FULL`->`LEFT`/`RIGHT`/`INNER`),
//!   after which more pushdown is possible. Apply to fixpoint.
//! * **Constant propagation** (lightweight equivalence classes): from a conjunct set containing
//!   `a = b` and `a = <const>` derive `b = <const>` (same operand types, `=` of a btree family) and push
//!   it to `b`'s relation; join quals `t.a = u.a` plus `t.a = 5` give `u.a = 5` on `u`. Original quals are
//!   kept.
//! * Result keeps all quals exactly once except derived ones; semantics must be identical to the
//!   original query (tests: row-count equality on small fixtures).
//!
//! The helper predicates below are used by `index`, `join` and `subplan` as well.
use pgarbor_common::Result;
use pgarbor_sql::ir::{BoolOp, BoolTestKind, BoundExpr, ColumnRef, ExprKind, OpExpr, OpRef};
use pgarbor_types::{Datum, TypeId};

use crate::context::PlannerState;
use crate::logical::{LogicalFilter, LogicalJoin, LogicalNode, LogicalPlan};
use crate::plan::JoinType;
use crate::simplify::split_conjuncts;

/// Range-table indexes (sorted, unique) of the level-0 columns in `e`. Fully implemented.
pub fn expr_relids(e: &BoundExpr) -> Vec<u32> {
    let mut cols = Vec::new();
    e.collect_columns(0, &mut cols);
    let mut v: Vec<u32> = cols
        .into_iter()
        .filter(|c| c.levels_up == 0)
        .map(|c| c.rt_index)
        .collect();
    v.sort_unstable();
    v.dedup();
    v
}

/// Does `e` call a volatile function/operator? Fully implemented (delegates to `BoundExpr::volatility`).
pub fn is_volatile(e: &BoundExpr) -> bool {
    e.volatility() == pgarbor_types::catalog_data::Volatility::Volatile
}

/// Is `e` strict with respect to the columns of relation set `rels`: guaranteed not TRUE when every
/// column of those relations is NULL (module docs)?
// The predicate is "not TRUE": a NULL or FALSE result both filter the row out, which is what a WHERE
// qual needs. Strictness of a value expression (`null_forced`) is the building block.
pub fn is_strict_for_rels(e: &BoundExpr, rels: &[u32]) -> Result<bool> {
    Ok(not_true(e, rels))
}

/// Distribute `quals` into `plan` (module docs).
pub fn push_down_quals(
    st: &mut PlannerState,
    plan: LogicalPlan,
    quals: Vec<BoundExpr>,
) -> Result<LogicalPlan> {
    // Split the WHERE conjuncts; these are the quals that hold for every output row of `plan`.
    let mut conj: Vec<BoundExpr> = Vec::new();
    for q in quals {
        conj.extend(split_conjuncts(q));
    }
    // Constant propagation: equalities that hold for every output row (WHERE conjuncts and the ON
    // conditions of inner joins that are not under a null-extended side) imply new WHERE conjuncts.
    let mut sources = conj.clone();
    collect_implied_sources(&plan, false, &mut sources);
    let derived = derive_constant_quals(st, &sources)?;
    conj.extend(derived);

    // Constant quals (and quals referencing relations outside this tree) stay at the top.
    let mut top: Vec<BoundExpr> = Vec::new();
    let mut work: Vec<BoundExpr> = Vec::new();
    for q in conj {
        let ids = expr_relids(&q);
        if ids.is_empty() || !subset(&ids, &plan.rels) {
            top.push(q);
        } else {
            work.push(q);
        }
    }

    // Outer-join reduction, then distribution of the quals through the join tree.
    let mut plan = plan;
    let mut ctx: Vec<BoundExpr> = work.clone();
    reduce_outer_joins(&mut plan, &mut ctx);
    let plan = distribute(plan, work)?;
    Ok(wrap_residual(plan, top))
}

/// Derive implied constant quals from equalities (module docs); returns the extra quals only.
pub fn derive_constant_quals(st: &mut PlannerState, quals: &[BoundExpr]) -> Result<Vec<BoundExpr>> {
    let _ = &*st;
    let mut flat: Vec<&BoundExpr> = Vec::new();
    for q in quals {
        flatten_and(q, &mut flat);
    }

    // Equivalence classes over plain columns (union-find), and `column = constant` bindings.
    let mut keys: Vec<ColKey> = Vec::new();
    let mut parent: Vec<usize> = Vec::new();
    let mut binds: Vec<(usize, BoundExpr, OpRef)> = Vec::new();
    for &q in flat.iter() {
        let o = match &q.kind {
            ExprKind::Op(o) => o,
            _ => continue,
        };
        if o.op.name != "=" || !o.op.strict || o.args.len() != 2 || is_volatile(q) {
            continue;
        }
        let a = &o.args[0];
        let b = &o.args[1];
        if a.ty.oid != b.ty.oid {
            continue;
        }
        match (plain_column(a), plain_column(b)) {
            (Some(ca), Some(cb)) => {
                let ka = key_index(&mut keys, &mut parent, ca, a.ty);
                let kb = key_index(&mut keys, &mut parent, cb, b.ty);
                union(&mut parent, ka, kb);
            }
            (Some(ca), None) if is_nonnull_const(b) => {
                let ka = key_index(&mut keys, &mut parent, ca, a.ty);
                binds.push((ka, b.clone(), o.op.clone()));
            }
            (None, Some(cb)) if is_nonnull_const(a) => {
                let kb = key_index(&mut keys, &mut parent, cb, b.ty);
                binds.push((kb, a.clone(), o.op.clone()));
            }
            _ => {}
        }
    }
    if binds.is_empty() {
        return Ok(Vec::new());
    }

    let mut roots: Vec<usize> = Vec::with_capacity(keys.len());
    for i in 0..keys.len() {
        roots.push(find(&mut parent, i));
    }

    // Every member of a class that holds a constant gets `member = constant`.
    let mut out: Vec<BoundExpr> = Vec::new();
    for (bk, cexpr, op) in binds.iter() {
        let root = roots[*bk];
        for (mk, key) in keys.iter().enumerate() {
            if roots[mk] != root {
                continue;
            }
            let col = BoundExpr::column(0, key.col.rt_index, key.col.attnum, key.ty);
            let cand = make_eq(op.clone(), col, cexpr.clone());
            if flat.iter().any(|&q| is_same_eq(q, &cand))
                || out.iter().any(|q| is_same_eq(q, &cand))
            {
                continue;
            }
            out.push(cand);
        }
    }
    Ok(out)
}

// ---------------------------------------------------------------------------------------------
// Strictness
// ---------------------------------------------------------------------------------------------

/// `e` evaluates to NULL whenever every level-0 column of `rels` is NULL (a strict value expression
/// over such a column). Conservative: `false` when unsure.
fn null_forced(e: &BoundExpr, rels: &[u32]) -> bool {
    match &e.kind {
        ExprKind::Const(d) => matches!(d, Datum::Null),
        // Whole-row (attnum 0) and system columns are not NULL when their table's columns are NULL.
        ExprKind::Column(c) => column_in_rels(c, rels),
        ExprKind::Op(o) => o.op.strict && o.args.iter().any(|a| null_forced(a, rels)),
        ExprKind::FuncCall(f) => {
            f.strict && !f.retset && f.args.iter().any(|a| null_forced(a, rels))
        }
        ExprKind::Cast(c) => null_forced(&c.arg, rels),
        ExprKind::Collate(c) => null_forced(&c.arg, rels),
        ExprKind::NullIf(n) => null_forced(&n.left, rels),
        ExprKind::Bool(b) if b.op == BoolOp::Not => b.args.iter().any(|a| null_forced(a, rels)),
        _ => false,
    }
}

/// The boolean `e` is never TRUE when every level-0 column of `rels` is NULL.
fn not_true(e: &BoundExpr, rels: &[u32]) -> bool {
    if null_forced(e, rels) {
        return true;
    }
    match &e.kind {
        ExprKind::Const(d) => matches!(d, Datum::Bool(false)),
        ExprKind::Bool(b) => match b.op {
            BoolOp::And => b.args.iter().any(|a| not_true(a, rels)),
            BoolOp::Or => !b.args.is_empty() && b.args.iter().all(|a| not_true(a, rels)),
            BoolOp::Not => false,
        },
        // `x IS NOT NULL` is FALSE for a NULL x; `x IS NULL` is TRUE and is not strict.
        ExprKind::NullTest(n) => !n.is_null && null_forced(&n.arg, rels),
        // IS TRUE / IS FALSE / IS NOT UNKNOWN are FALSE for NULL input; the others are TRUE for NULL.
        ExprKind::BooleanTest(t) => {
            matches!(
                t.test,
                BoolTestKind::IsTrue | BoolTestKind::IsFalse | BoolTestKind::IsNotUnknown
            ) && null_forced(&t.arg, rels)
        }
        // `x op ANY (array)` with a strict operator is NULL (or FALSE for an empty array) for NULL x.
        // `ALL` over an empty array is TRUE, so only the ANY form qualifies.
        ExprKind::ScalarArrayOp(s) => s.use_or && s.op.strict && null_forced(&s.left, rels),
        _ => false,
    }
}

fn column_in_rels(c: &ColumnRef, rels: &[u32]) -> bool {
    c.levels_up == 0 && c.attnum > 0 && rels.contains(&c.rt_index)
}

// ---------------------------------------------------------------------------------------------
// Outer-join reduction
// ---------------------------------------------------------------------------------------------

/// Does some non-volatile qual in `ctx` reject rows whose `rels` columns are all NULL?
fn null_rejected(ctx: &[BoundExpr], rels: &[u32]) -> bool {
    if rels.is_empty() {
        return false;
    }
    ctx.iter()
        .any(|q| !is_volatile(q) && touches(&expr_relids(q), rels) && not_true(q, rels))
}

/// Reduce outer joins that are null-rejected by a qual applied above them (module docs). `ctx` holds
/// the quals that filter every row of `plan` (WHERE conjuncts, quals of enclosing inner joins and of
/// enclosing filters). Top-down: a join reduced to INNER contributes its own quals to its children.
fn reduce_outer_joins(plan: &mut LogicalPlan, ctx: &mut Vec<BoundExpr>) {
    match &mut plan.node {
        LogicalNode::Filter(f) => {
            let mark = ctx.len();
            ctx.extend(f.quals.iter().cloned());
            reduce_outer_joins(&mut f.input, ctx);
            ctx.truncate(mark);
        }
        LogicalNode::Join(j) => {
            let kill_left = null_rejected(ctx.as_slice(), &j.left.rels);
            let kill_right = null_rejected(ctx.as_slice(), &j.right.rels);
            j.join_type = match j.join_type {
                JoinType::Left if kill_right => JoinType::Inner,
                JoinType::Right if kill_left => JoinType::Inner,
                JoinType::Full => match (kill_left, kill_right) {
                    (true, true) => JoinType::Inner,
                    (true, false) => JoinType::Left,
                    (false, true) => JoinType::Right,
                    (false, false) => JoinType::Full,
                },
                other => other,
            };
            let mark = ctx.len();
            if j.join_type == JoinType::Inner {
                ctx.extend(j.quals.iter().cloned());
            }
            reduce_outer_joins(&mut j.left, ctx);
            reduce_outer_joins(&mut j.right, ctx);
            ctx.truncate(mark);
        }
        // Leaves and every other node are boundaries: sub-plans are separate query levels.
        _ => {}
    }
}

// ---------------------------------------------------------------------------------------------
// Distribution
// ---------------------------------------------------------------------------------------------

/// Push `quals` (WHERE-level: each holds for every output row of `plan`) as deep as legal. Quals that
/// cannot move are returned as a `Filter` above the node.
fn distribute(plan: LogicalPlan, quals: Vec<BoundExpr>) -> Result<LogicalPlan> {
    let LogicalPlan {
        node,
        vars,
        out,
        rels,
        level,
    } = plan;
    let (node, residual) = match node {
        LogicalNode::Scan(mut s) => {
            let mut keep: Vec<BoundExpr> = Vec::new();
            for q in quals {
                let ids = expr_relids(&q);
                if ids.is_empty() || !subset(&ids, &rels) {
                    keep.push(q);
                } else {
                    s.quals.push(q);
                }
            }
            (LogicalNode::Scan(s), keep)
        }
        LogicalNode::Filter(f) => {
            let LogicalFilter { input, quals: fq } = *f;
            let joinish = matches!(
                input.node,
                LogicalNode::Scan(_) | LogicalNode::Join(_) | LogicalNode::Filter(_)
            );
            let mut down: Vec<BoundExpr> = Vec::new();
            let mut keep: Vec<BoundExpr> = Vec::new();
            if joinish {
                for q in quals {
                    if pushable_ids(&q).is_some() {
                        down.push(q);
                    } else {
                        keep.push(q);
                    }
                }
            } else {
                keep = quals;
            }
            let input = distribute(input, down)?;
            (
                LogicalNode::Filter(Box::new(LogicalFilter { input, quals: fq })),
                keep,
            )
        }
        LogicalNode::Join(j) => {
            let (j, keep) = distribute_join(*j, quals)?;
            (LogicalNode::Join(Box::new(j)), keep)
        }
        other => (other, quals),
    };
    let plan = LogicalPlan {
        node,
        vars,
        out,
        rels,
        level,
    };
    Ok(wrap_residual(plan, residual))
}

/// Distribute quals through a join. Returns the rebuilt join and the quals that must stay above it.
fn distribute_join(j: LogicalJoin, quals: Vec<BoundExpr>) -> Result<(LogicalJoin, Vec<BoundExpr>)> {
    let LogicalJoin {
        join_type,
        left,
        right,
        quals: own,
        lateral,
    } = j;
    let lrels = left.rels.clone();
    let rrels = right.rels.clone();
    let mut to_left: Vec<BoundExpr> = Vec::new();
    let mut to_right: Vec<BoundExpr> = Vec::new();
    let mut keep: Vec<BoundExpr> = Vec::new();
    let mut residual: Vec<BoundExpr> = Vec::new();

    match join_type {
        JoinType::Inner => {
            // Both the WHERE-level quals and the ON quals are conditions on the output rows.
            for q in quals.into_iter().chain(own) {
                match pushable_ids(&q) {
                    Some(ids) if subset(&ids, &lrels) => to_left.push(q),
                    Some(ids) if subset(&ids, &rrels) => to_right.push(q),
                    _ => keep.push(q),
                }
            }
        }
        JoinType::Left | JoinType::Semi | JoinType::Anti => {
            // The left side is preserved: WHERE quals on it move left; ON quals on the right move right.
            for q in quals {
                match pushable_ids(&q) {
                    Some(ids) if subset(&ids, &lrels) => to_left.push(q),
                    _ => residual.push(q),
                }
            }
            for q in own {
                match pushable_ids(&q) {
                    Some(ids) if subset(&ids, &rrels) => to_right.push(q),
                    _ => keep.push(q),
                }
            }
        }
        JoinType::Right => {
            for q in quals {
                match pushable_ids(&q) {
                    Some(ids) if subset(&ids, &rrels) => to_right.push(q),
                    _ => residual.push(q),
                }
            }
            for q in own {
                match pushable_ids(&q) {
                    Some(ids) if subset(&ids, &lrels) => to_left.push(q),
                    _ => keep.push(q),
                }
            }
        }
        JoinType::Full => {
            // Nothing crosses a FULL join.
            residual.extend(quals);
            keep.extend(own);
        }
    }

    let left = distribute(left, to_left)?;
    let right = distribute(right, to_right)?;
    Ok((
        LogicalJoin {
            join_type,
            left,
            right,
            quals: keep,
            lateral,
        },
        residual,
    ))
}

/// Wrap `plan` in a `Filter` holding `residual` (no-op when empty). The filter exposes the same slots.
fn wrap_residual(plan: LogicalPlan, residual: Vec<BoundExpr>) -> LogicalPlan {
    if residual.is_empty() {
        return plan;
    }
    let vars = plan.vars.clone();
    let out = plan.out.clone();
    let rels = plan.rels.clone();
    let level = plan.level;
    LogicalPlan {
        node: LogicalNode::Filter(Box::new(LogicalFilter {
            input: plan,
            quals: residual,
        })),
        vars,
        out,
        rels,
        level,
    }
}

/// `Some(relids)` when `q` is deterministic and references at least one relation.
fn pushable_ids(q: &BoundExpr) -> Option<Vec<u32>> {
    if is_volatile(q) {
        return None;
    }
    let ids = expr_relids(q);
    if ids.is_empty() {
        None
    } else {
        Some(ids)
    }
}

/// Collect the quals that hold for every output row of `p` (not under a null-extended side) so their
/// equalities can feed constant propagation.
fn collect_implied_sources(p: &LogicalPlan, nullable: bool, out: &mut Vec<BoundExpr>) {
    match &p.node {
        LogicalNode::Filter(f) => {
            if !nullable {
                out.extend(f.quals.iter().cloned());
            }
            collect_implied_sources(&f.input, nullable, out);
        }
        LogicalNode::Join(j) => {
            if !nullable && j.join_type == JoinType::Inner {
                out.extend(j.quals.iter().cloned());
            }
            let (ln, rn) = match j.join_type {
                JoinType::Inner => (nullable, nullable),
                JoinType::Left | JoinType::Semi | JoinType::Anti => (nullable, true),
                JoinType::Right => (true, nullable),
                JoinType::Full => (true, true),
            };
            collect_implied_sources(&j.left, ln, out);
            collect_implied_sources(&j.right, rn, out);
        }
        _ => {}
    }
}

// ---------------------------------------------------------------------------------------------
// Constant propagation helpers
// ---------------------------------------------------------------------------------------------

/// A plain (non-volatile-sensitive) column key of the equivalence structure.
struct ColKey {
    col: ColumnRef,
    ty: TypeId,
}

fn key_index(keys: &mut Vec<ColKey>, parent: &mut Vec<usize>, col: ColumnRef, ty: TypeId) -> usize {
    for (i, k) in keys.iter().enumerate() {
        if k.col == col {
            return i;
        }
    }
    keys.push(ColKey { col, ty });
    parent.push(parent.len());
    keys.len() - 1
}

fn find(parent: &mut [usize], mut x: usize) -> usize {
    loop {
        let p = parent[x];
        if p == x {
            return x;
        }
        let gp = parent[p];
        parent[x] = gp;
        x = gp;
    }
}

fn union(parent: &mut [usize], a: usize, b: usize) {
    let ra = find(parent, a);
    let rb = find(parent, b);
    if ra != rb {
        parent[rb] = ra;
    }
}

fn plain_column(e: &BoundExpr) -> Option<ColumnRef> {
    match &e.kind {
        ExprKind::Column(c) if c.levels_up == 0 && c.attnum > 0 => Some(*c),
        _ => None,
    }
}

fn is_nonnull_const(e: &BoundExpr) -> bool {
    e.is_const() && !e.is_null_const()
}

fn make_eq(op: OpRef, left: BoundExpr, right: BoundExpr) -> BoundExpr {
    BoundExpr {
        kind: ExprKind::Op(Box::new(OpExpr {
            op,
            args: vec![left, right],
        })),
        ty: TypeId::BOOL,
    }
}

/// Structural equality, also accepting the operands in swapped order for a binary operator.
fn is_same_eq(x: &BoundExpr, cand: &BoundExpr) -> bool {
    if x == cand {
        return true;
    }
    match (&x.kind, &cand.kind) {
        (ExprKind::Op(xo), ExprKind::Op(co)) => {
            xo.op == co.op
                && xo.args.len() == 2
                && co.args.len() == 2
                && xo.args[0] == co.args[1]
                && xo.args[1] == co.args[0]
        }
        _ => false,
    }
}

fn flatten_and<'a>(e: &'a BoundExpr, out: &mut Vec<&'a BoundExpr>) {
    match &e.kind {
        ExprKind::Bool(b) if b.op == BoolOp::And => {
            for a in &b.args {
                flatten_and(a, out);
            }
        }
        _ => out.push(e),
    }
}

fn subset(ids: &[u32], rels: &[u32]) -> bool {
    ids.iter().all(|i| rels.contains(i))
}

fn touches(ids: &[u32], rels: &[u32]) -> bool {
    ids.iter().any(|i| rels.contains(i))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::context::{PlanContext, PlanOptions};
    use crate::logical::{LogicalScan, ScanSource};
    use crate::plan::OutCol;
    use pgarbor_catalog::testing;
    use pgarbor_sql::ir::NullTest;
    use pgarbor_types::catalog_data::Volatility;
    use std::collections::HashMap;

    fn col(rt: u32, attnum: i16) -> BoundExpr {
        BoundExpr::column(0, rt, attnum, TypeId::INT4)
    }

    fn int(v: i32) -> BoundExpr {
        BoundExpr::constant(Datum::Int4(v), TypeId::INT4)
    }

    fn opref(name: &str, strict: bool, volatility: Volatility) -> OpRef {
        OpRef {
            op: 96,
            proc: 65,
            name: name.to_string(),
            volatility,
            strict,
        }
    }

    fn bin_with(op: OpRef, l: BoundExpr, r: BoundExpr) -> BoundExpr {
        BoundExpr {
            kind: ExprKind::Op(Box::new(OpExpr {
                op,
                args: vec![l, r],
            })),
            ty: TypeId::BOOL,
        }
    }

    fn bin(name: &str, l: BoundExpr, r: BoundExpr) -> BoundExpr {
        bin_with(opref(name, true, Volatility::Immutable), l, r)
    }

    fn scan(rt: u32) -> LogicalPlan {
        let out = (1..=3)
            .map(|i| OutCol {
                name: format!("c{i}"),
                ty: TypeId::INT4,
            })
            .collect::<Vec<_>>();
        LogicalPlan {
            node: LogicalNode::Scan(Box::new(LogicalScan {
                rt_index: rt,
                source: ScanSource::Values { rows: Vec::new() },
                quals: Vec::new(),
                with_rowid: false,
            })),
            vars: vec![col(rt, 1), col(rt, 2), col(rt, 3)],
            out,
            rels: vec![rt],
            level: 0,
        }
    }

    fn join(jt: JoinType, l: LogicalPlan, r: LogicalPlan, quals: Vec<BoundExpr>) -> LogicalPlan {
        let mut vars = l.vars.clone();
        vars.extend(r.vars.iter().cloned());
        let mut out = l.out.clone();
        out.extend(r.out.iter().cloned());
        let mut rels = l.rels.clone();
        rels.extend(r.rels.iter().copied());
        rels.sort_unstable();
        LogicalPlan {
            node: LogicalNode::Join(Box::new(LogicalJoin {
                join_type: jt,
                left: l,
                right: r,
                quals,
                lateral: false,
            })),
            vars,
            out,
            rels,
            level: 0,
        }
    }

    /// Standard fixture: t (rt 1: a, b, c) and u (rt 2: a, b, c) joined on t.a = u.a.
    fn t_join_u(jt: JoinType) -> LogicalPlan {
        join(jt, scan(1), scan(2), vec![bin("=", col(1, 1), col(2, 1))])
    }

    fn run_push(plan: LogicalPlan, quals: Vec<BoundExpr>) -> Result<LogicalPlan> {
        let (_kv, env) = testing::bootstrapped()?;
        let ctx = PlanContext {
            env: &env,
            param_types: Vec::new(),
            param_values: None,
            options: PlanOptions::default(),
            table_rows: HashMap::new(),
        };
        let mut st = PlannerState::new(&ctx);
        push_down_quals(&mut st, plan, quals)
    }

    fn run_derive(quals: &[BoundExpr]) -> Result<Vec<BoundExpr>> {
        let (_kv, env) = testing::bootstrapped()?;
        let ctx = PlanContext {
            env: &env,
            param_types: Vec::new(),
            param_values: None,
            options: PlanOptions::default(),
            table_rows: HashMap::new(),
        };
        let mut st = PlannerState::new(&ctx);
        derive_constant_quals(&mut st, quals)
    }

    fn join_of(p: &LogicalPlan) -> &LogicalJoin {
        match &p.node {
            LogicalNode::Join(j) => &**j,
            _ => unreachable!("test expects a join at this node"),
        }
    }

    fn scan_quals(p: &LogicalPlan) -> Vec<BoundExpr> {
        match &p.node {
            LogicalNode::Scan(s) => s.quals.clone(),
            _ => unreachable!("test expects a scan at this node"),
        }
    }

    #[test]
    fn where_on_one_side_of_inner_join_pushes_to_scan() -> Result<()> {
        let q = bin("=", col(1, 2), int(1));
        let out = run_push(t_join_u(JoinType::Inner), vec![q.clone()])?;
        let j = join_of(&out);
        assert_eq!(j.join_type, JoinType::Inner);
        assert_eq!(j.quals, vec![bin("=", col(1, 1), col(2, 1))]);
        assert_eq!(scan_quals(&j.left), vec![q]);
        assert!(scan_quals(&j.right).is_empty());
        Ok(())
    }

    #[test]
    fn where_across_inner_join_becomes_join_qual() -> Result<()> {
        let q = bin(">", col(1, 2), col(2, 2));
        let out = run_push(t_join_u(JoinType::Inner), vec![q.clone()])?;
        let j = join_of(&out);
        assert_eq!(j.quals.len(), 2);
        assert!(j.quals.contains(&q));
        Ok(())
    }

    #[test]
    fn left_join_where_on_nullable_side_reduces_to_inner_and_pushes() -> Result<()> {
        let q = bin(">", col(2, 3), int(5));
        let out = run_push(t_join_u(JoinType::Left), vec![q.clone()])?;
        let j = join_of(&out);
        assert_eq!(j.join_type, JoinType::Inner);
        assert_eq!(scan_quals(&j.right), vec![q]);
        Ok(())
    }

    #[test]
    fn left_join_where_is_null_stays_above_the_join() -> Result<()> {
        let q = BoundExpr {
            kind: ExprKind::NullTest(Box::new(NullTest {
                arg: col(2, 3),
                is_null: true,
                row_semantics: false,
            })),
            ty: TypeId::BOOL,
        };
        let out = run_push(t_join_u(JoinType::Left), vec![q.clone()])?;
        let filter = match &out.node {
            LogicalNode::Filter(f) => f,
            _ => unreachable!("IS NULL qual must stay above the LEFT join"),
        };
        assert_eq!(filter.quals, vec![q]);
        let j = join_of(&filter.input);
        assert_eq!(j.join_type, JoinType::Left);
        assert!(scan_quals(&j.right).is_empty());
        Ok(())
    }

    #[test]
    fn left_join_on_qual_on_nullable_side_pushes_into_right() -> Result<()> {
        let on_nullable = bin(">", col(2, 3), int(5));
        let plan = join(
            JoinType::Left,
            scan(1),
            scan(2),
            vec![bin("=", col(1, 1), col(2, 1)), on_nullable.clone()],
        );
        let out = run_push(plan, Vec::new())?;
        let j = join_of(&out);
        assert_eq!(j.join_type, JoinType::Left);
        assert_eq!(j.quals, vec![bin("=", col(1, 1), col(2, 1))]);
        assert_eq!(scan_quals(&j.right), vec![on_nullable]);
        Ok(())
    }

    #[test]
    fn left_join_on_qual_on_preserved_side_stays_in_join() -> Result<()> {
        let on_preserved = bin("=", col(1, 1), int(5));
        let plan = join(
            JoinType::Left,
            scan(1),
            scan(2),
            vec![on_preserved.clone(), bin("=", col(1, 1), col(2, 1))],
        );
        let out = run_push(plan, Vec::new())?;
        let j = join_of(&out);
        assert_eq!(j.join_type, JoinType::Left);
        assert!(j.quals.contains(&on_preserved));
        assert_eq!(j.quals.len(), 2);
        assert!(scan_quals(&j.left).is_empty());
        assert!(scan_quals(&j.right).is_empty());
        Ok(())
    }

    #[test]
    fn full_join_keeps_non_strict_where_quals_above() -> Result<()> {
        // IS NULL is not null-rejecting, so the FULL join is not reduced and nothing crosses it.
        let q = BoundExpr {
            kind: ExprKind::NullTest(Box::new(NullTest {
                arg: col(2, 3),
                is_null: true,
                row_semantics: false,
            })),
            ty: TypeId::BOOL,
        };
        let out = run_push(t_join_u(JoinType::Full), vec![q.clone()])?;
        let filter = match &out.node {
            LogicalNode::Filter(f) => f,
            _ => unreachable!("WHERE qual must stay above the FULL join"),
        };
        assert_eq!(filter.quals, vec![q]);
        assert_eq!(join_of(&filter.input).join_type, JoinType::Full);
        Ok(())
    }

    #[test]
    fn full_join_strict_where_on_left_reduces_to_left() -> Result<()> {
        // A strict qual on the left side rejects the left-NULL rows, leaving a LEFT join.
        let q = bin("=", col(1, 2), int(1));
        let out = run_push(t_join_u(JoinType::Full), vec![q.clone()])?;
        let j = join_of(&out);
        assert_eq!(j.join_type, JoinType::Left);
        assert_eq!(scan_quals(&j.left), vec![q]);
        Ok(())
    }

    #[test]
    fn constant_qual_stays_at_top() -> Result<()> {
        let q = BoundExpr::bool_const(true);
        let out = run_push(t_join_u(JoinType::Inner), vec![q.clone()])?;
        let filter = match &out.node {
            LogicalNode::Filter(f) => f,
            _ => unreachable!("constant qual stays at the top"),
        };
        assert_eq!(filter.quals, vec![q]);
        Ok(())
    }

    #[test]
    fn constant_is_propagated_across_join_equality() -> Result<()> {
        let q = bin("=", col(1, 1), int(5));
        let out = run_push(t_join_u(JoinType::Inner), vec![q.clone()])?;
        let j = join_of(&out);
        assert_eq!(scan_quals(&j.left), vec![q]);
        assert_eq!(scan_quals(&j.right), vec![bin("=", col(2, 1), int(5))]);
        Ok(())
    }

    #[test]
    fn derive_constant_quals_transitively() -> Result<()> {
        // a = b, b = c, c = 7 derive a = 7 and b = 7 (original quals are not returned).
        let quals = vec![
            bin("=", col(1, 1), col(2, 1)),
            bin("=", col(2, 1), col(3, 1)),
            bin("=", col(3, 1), int(7)),
        ];
        let derived = run_derive(&quals)?;
        assert!(derived.contains(&bin("=", col(1, 1), int(7))));
        assert!(derived.contains(&bin("=", col(2, 1), int(7))));
        assert!(!derived.contains(&bin("=", col(3, 1), int(7))));
        Ok(())
    }

    #[test]
    fn volatile_qual_is_not_pushed_below_join() -> Result<()> {
        let vol = bin_with(opref(">", true, Volatility::Volatile), col(1, 2), int(0));
        let out = run_push(t_join_u(JoinType::Inner), vec![vol.clone()])?;
        let j = join_of(&out);
        assert!(scan_quals(&j.left).is_empty());
        assert!(scan_quals(&j.right).is_empty());
        assert!(j.quals.contains(&vol));
        Ok(())
    }

    #[test]
    fn strictness_rules() {
        let rels = [2u32];
        assert!(is_strict_for_rels(&bin("=", col(2, 3), int(1)), &rels).unwrap_or(false));
        assert!(!is_strict_for_rels(
            &bin_with(opref("=", false, Volatility::Immutable), col(2, 3), int(1)),
            &rels
        )
        .unwrap_or(true));
        assert!(!is_strict_for_rels(&bin("=", col(1, 3), int(1)), &rels).unwrap_or(true));

        let is_null = BoundExpr {
            kind: ExprKind::NullTest(Box::new(NullTest {
                arg: col(2, 3),
                is_null: true,
                row_semantics: false,
            })),
            ty: TypeId::BOOL,
        };
        assert!(!is_strict_for_rels(&is_null, &rels).unwrap_or(true));
        let is_not_null = BoundExpr {
            kind: ExprKind::NullTest(Box::new(NullTest {
                arg: col(2, 3),
                is_null: false,
                row_semantics: false,
            })),
            ty: TypeId::BOOL,
        };
        assert!(is_strict_for_rels(&is_not_null, &rels).unwrap_or(false));

        let coalesce = BoundExpr {
            kind: ExprKind::Coalesce(vec![col(2, 3), int(0)]),
            ty: TypeId::INT4,
        };
        let coalesce_pred = bin("=", coalesce, int(0));
        assert!(!is_strict_for_rels(&coalesce_pred, &rels).unwrap_or(true));

        let or = BoundExpr::or_all(vec![
            bin("=", col(2, 3), int(1)),
            bin("=", col(1, 1), int(1)),
        ]);
        assert!(!is_strict_for_rels(&or, &rels).unwrap_or(true));
        let and = BoundExpr::and_all(vec![
            bin("=", col(2, 3), int(1)),
            bin("=", col(1, 1), int(1)),
        ]);
        assert!(is_strict_for_rels(&and, &rels).unwrap_or(false));

        let not = BoundExpr::not(bin("=", col(2, 3), int(1)));
        assert!(is_strict_for_rels(&not, &rels).unwrap_or(false));
    }
}
