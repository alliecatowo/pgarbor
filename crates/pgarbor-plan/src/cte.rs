//! CTE planning: inlining, materialization, recursion (PLN-11). PG references:
//! `optimizer/plan/subselect.c` (`SS_process_ctes`), `prepjointree.c` (`inline_cte`,
//! `contain_dml`, `contain_outer_selfref`), `createplan.c` (`create_ctescan_plan`,
//! `create_recursiveunion_plan`, `create_worktablescan_plan`), `nodeCtescan.c`,
//! `nodeRecursiveunion.c`.
//!
//! * [`register_ctes`] runs once per query level before FROM is built: pushes the level's `BoundCte`s on
//!   `st.cte_scopes` (so `RangeKind::Cte { cte_index, levels_up }` can be resolved), decides for each CTE
//!   whether to INLINE or MATERIALIZE, and for materialized ones plans the body into `st.ctes`.
//! * **Inline** (PG 12+): non-recursive, `ref_count == 1`, no data-modifying body, not volatile, and
//!   `materialized != Some(true)`; `materialized == Some(false)` also forces inlining of multi-ref CTEs
//!   (each reference re-plans the body). An inlined reference becomes `ScanSource::Subquery(build_query(body))`.
//! * **Materialize** otherwise: `CteDef { id, name, body: Query(plan), columns, materialize: true }`;
//!   each reference is `ScanSource::Cte { cte_id }`. CTEs with `ref_count == 0` and a query body are dropped.
//! * **Recursive** (`BoundCte::recursive`): body is `SetOp(Union)`; plan the non-recursive term, then the
//!   recursive term with `RangeKind::Cte { recursive_self: true }` entries planned as
//!   `ScanSource::WorkTable { cte_id }`; `CteBodyPlan::Recursive { non_recursive, recursive, union_all }`.
//! * **Data-modifying** (`CteBody::Modify`): planned through `build_dml::plan_dml_statement` into
//!   `CteBodyPlan::Modify`, `materialize = true`, executed eagerly exactly once.
//! * [`plan_cte_reference`] returns the scan source for a `RangeKind::Cte` entry (inline subquery or
//!   `Cte`/`WorkTable` scan) plus the output columns (`OutCol`s).
//!
//! ## Id bookkeeping
//! `CteDef` ids are indexes into `st.ctes`. For a WITH list, the ids of its materialized CTEs are reserved
//! as one consecutive block (placeholders are pushed first, so CTEs planned while a body is being built
//! cannot interleave). The block base of each WITH nesting depth is kept in `st.scratch` under a fixed key
//! for that depth; a CTE's id is `base + (number of materialized CTEs before it in the list)`.
//! Decisions depend only on the bound CTE itself, so a reference recomputes them deterministically.
//!
//! ## Inlined bodies
//! PG plans an inlined body in the scope of its defining query level. The planner therefore truncates
//! `st.cte_scopes` to the defining level while building the body and restores it afterwards.
use pgarbor_common::{PgError, Result, SqlState};
use pgarbor_sql::ir::{
    BoundCte, BoundExpr, BoundQuery, CteBody, ExprKind, FrameBound, FromItem, OutputColumn, QueryBody,
    RangeKind, SetOpKind,
};
use pgarbor_types::catalog_data::Volatility;

use crate::build;
use crate::build_dml;
use crate::context::{CteScope, PlannerState};
use crate::logical::ScanSource;
use crate::plan::{AppendNode, CteBodyPlan, CteDef, NodeKind, OutCol, PlanNode};
use crate::planner;

/// What [`register_ctes`] does with one CTE of a WITH list (PG `SS_process_ctes`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Decision {
    /// Unreferenced SELECT: dropped (PG: `cterefcount == 0 && CMD_SELECT`).
    Skip,
    /// Every reference plans the body as a sub-select (PG `inline_cte`).
    Inline,
    /// Planned once into `st.ctes` and scanned by reference.
    Materialize,
}

/// Plan/register the CTEs of one query level (module docs).
pub fn register_ctes(st: &mut PlannerState, ctes: &[BoundCte]) -> Result<()> {
    let decs = decisions(ctes);

    // Reserve a consecutive id block for the materialized CTEs of this list.
    let base = next_cte_id(st)?;
    st.cte_scopes.push(std::sync::Arc::new(CteScope { ctes: ctes.to_vec(), base }));
    for (cte, dec) in ctes.iter().zip(decs.iter()) {
        if *dec == Decision::Materialize {
            let id = next_cte_id(st)?;
            st.ctes.push(placeholder(id, &cte.name));
        }
    }

    // Plan the materialized bodies in list order (PG plans CTEs in `cteList` order).
    for (i, (cte, dec)) in ctes.iter().zip(decs.iter()).enumerate() {
        if *dec != Decision::Materialize {
            continue;
        }
        let id = base
            .checked_add(rank_before(&decs, i)?)
            .ok_or_else(|| PgError::internal("too many CTEs in one WITH list"))?;
        let def = plan_cte_body(st, cte, id)?;
        let slot = st.ctes.get_mut(id as usize).ok_or_else(|| PgError::internal("CTE slot was not reserved"))?;
        *slot = def;
    }
    Ok(())
}

/// Leave the query level: pop the scope pushed by `register_ctes`.
pub fn unregister_ctes(st: &mut PlannerState) {
    st.cte_scopes.pop();
}

/// Resolve a `RangeKind::Cte` range entry to its scan source and output columns.
pub fn plan_cte_reference(st: &mut PlannerState, kind: &RangeKind) -> Result<(ScanSource, Vec<OutCol>)> {
    let (cte_index, levels_up, recursive_self) = match kind {
        RangeKind::Cte { cte_index, levels_up, recursive_self } => {
            (*cte_index as usize, *levels_up as usize, *recursive_self)
        }
        _ => return Err(PgError::internal("plan_cte_reference called for a range entry that is not a CTE")),
    };
    let n = st.cte_scopes.len();
    if levels_up >= n {
        return Err(PgError::internal("CTE reference points above the outermost WITH scope"));
    }
    let depth = n - 1 - levels_up;

    let (decision, rank, columns, inline_body) = {
        let scope: &[BoundCte] = &st.cte_scopes[depth].ctes;
        let cte = scope.get(cte_index).ok_or_else(|| PgError::internal("CTE reference index out of range"))?;
        let decs = decisions(scope);
        let decision = if recursive_self {
            Decision::Materialize
        } else {
            decs.get(cte_index).copied().ok_or_else(|| PgError::internal("CTE decision missing"))?
        };
        let rank = rank_before(&decs, cte_index)?;
        let columns: Vec<OutCol> = cte.columns.iter().map(out_col).collect();
        let inline_body: Option<BoundQuery> = match (&cte.body, decision) {
            (CteBody::Query(q), Decision::Inline) => Some((**q).clone()),
            _ => None,
        };
        (decision, rank, columns, inline_body)
    };

    match decision {
        Decision::Skip => Err(PgError::internal("reference to a CTE that was not planned")),
        Decision::Inline => {
            let body = inline_body.ok_or_else(|| PgError::internal("inline CTE without a query body"))?;
            // The body belongs to the defining level: hide the scopes of the levels in between.
            let saved = st.cte_scopes.split_off(depth + 1);
            let planned = build::build_query(st, &body);
            st.cte_scopes.extend(saved);
            Ok((ScanSource::Subquery(planned?), columns))
        }
        Decision::Materialize => {
            let base = st.cte_scopes[depth].base;
            let cte_id = base.checked_add(rank).ok_or_else(|| PgError::internal("CTE id overflow"))?;
            if recursive_self {
                Ok((ScanSource::WorkTable { cte_id }, columns))
            } else {
                Ok((ScanSource::Cte { cte_id }, columns))
            }
        }
    }
}

// ---------------------------------------------------------------------------------------------
// Bodies
// ---------------------------------------------------------------------------------------------

/// Plan the body of a materialized CTE into its reserved slot `id`.
fn plan_cte_body(st: &mut PlannerState, cte: &BoundCte, id: u32) -> Result<CteDef> {
    let body = match &cte.body {
        CteBody::Modify(stmt) => {
            let (node, _kind) = build_dml::plan_dml_statement(st, stmt)?;
            CteBodyPlan::Modify(node)
        }
        CteBody::Query(q) if cte.recursive => plan_recursive_body(st, q)?,
        CteBody::Query(q) => CteBodyPlan::Query(planner::plan_query_node(st, q)?),
    };
    Ok(CteDef {
        id,
        name: cte.name.clone(),
        body,
        columns: cte.columns.iter().map(out_col).collect(),
        materialize: true,
    })
}

/// `WITH RECURSIVE` body: `non_recursive UNION [ALL] recursive`.
fn plan_recursive_body(st: &mut PlannerState, q: &BoundQuery) -> Result<CteBodyPlan> {
    if !q.order_by.is_empty() {
        return Err(PgError::new(SqlState::FEATURE_NOT_SUPPORTED, "ORDER BY in a recursive query is not implemented"));
    }
    if q.offset.is_some() {
        return Err(PgError::new(SqlState::FEATURE_NOT_SUPPORTED, "OFFSET in a recursive query is not implemented"));
    }
    if q.limit.is_some() {
        return Err(PgError::new(SqlState::FEATURE_NOT_SUPPORTED, "LIMIT in a recursive query is not implemented"));
    }
    if !q.locking.is_empty() {
        return Err(PgError::new(
            SqlState::FEATURE_NOT_SUPPORTED,
            "FOR UPDATE/SHARE in a recursive query is not implemented",
        ));
    }
    let so = match &q.body {
        QueryBody::SetOp(so) if so.op == SetOpKind::Union => so,
        _ => return Err(PgError::internal("recursive CTE body must be a UNION")),
    };
    let non_recursive = planner::plan_query_node(st, &so.left)?;
    let recursive = planner::plan_query_node(st, &so.right)?;
    Ok(CteBodyPlan::Recursive { non_recursive, recursive, union_all: so.all })
}

/// Placeholder for a reserved slot until its body is planned.
fn placeholder(id: u32, name: &str) -> CteDef {
    CteDef {
        id,
        name: name.to_string(),
        body: CteBodyPlan::Query(PlanNode::new(NodeKind::Append(AppendNode { children: Vec::new() }), Vec::new())),
        columns: Vec::new(),
        materialize: true,
    }
}

// ---------------------------------------------------------------------------------------------
// Decisions (PG `SS_process_ctes`)
// ---------------------------------------------------------------------------------------------

fn decisions(ctes: &[BoundCte]) -> Vec<Decision> {
    ctes.iter().map(decide).collect()
}

fn decide(cte: &BoundCte) -> Decision {
    match &cte.body {
        CteBody::Modify(_) => Decision::Materialize,
        CteBody::Query(q) => {
            if cte.ref_count == 0 {
                return Decision::Skip;
            }
            if cte.recursive {
                return Decision::Materialize;
            }
            let wanted = match cte.materialized {
                Some(true) => false,
                Some(false) => true,
                None => cte.ref_count == 1,
            };
            let inlinable = wanted
                && !contain_dml(q)
                && (cte.ref_count <= 1 || !outer_selfref(q, 1))
                && !contain_volatile(q);
            if inlinable {
                Decision::Inline
            } else {
                Decision::Materialize
            }
        }
    }
}

/// Number of materialized CTEs before `index` (their rank gives the offset from the block base).
fn rank_before(decs: &[Decision], index: usize) -> Result<u32> {
    let n = decs.iter().take(index).filter(|d| **d == Decision::Materialize).count();
    u32::try_from(n).map_err(|_| PgError::internal("too many CTEs in one WITH list"))
}

fn next_cte_id(st: &PlannerState) -> Result<u32> {
    u32::try_from(st.ctes.len()).map_err(|_| PgError::internal("too many CTEs"))
}

fn out_col(c: &OutputColumn) -> OutCol {
    OutCol { name: c.name.clone(), ty: c.ty }
}

// ---------------------------------------------------------------------------------------------
// Tree analysis (PG `contain_dml`, `contain_outer_selfref`, `contain_volatile_functions`)
// ---------------------------------------------------------------------------------------------

/// Expression roots owned by one query block: target list, quals, window clauses, VALUES rows,
/// function arguments, join conditions and LIMIT/OFFSET. Sub-queries are reached via [`child_queries`].
fn block_exprs(q: &BoundQuery) -> Vec<&BoundExpr> {
    let mut out: Vec<&BoundExpr> = Vec::new();
    match &q.body {
        QueryBody::Values(v) => {
            for row in &v.rows {
                out.extend(row.iter());
            }
        }
        QueryBody::Select(sel) => {
            for rte in &sel.range_table {
                match &rte.kind {
                    RangeKind::Function { funcs, .. } => out.extend(funcs.iter().map(|f| &f.expr)),
                    RangeKind::Values { values } => {
                        for row in &values.rows {
                            out.extend(row.iter());
                        }
                    }
                    _ => {}
                }
            }
            for item in &sel.from {
                push_from_quals(item, &mut out);
            }
            out.extend(sel.target_list.iter().map(|t| &t.expr));
            out.extend(sel.where_.iter());
            out.extend(sel.having.iter());
            for w in &sel.windows {
                out.extend(w.partition_by.iter());
                out.extend(w.order_by.iter().map(|s| &s.expr));
                push_frame_bound(&w.start, &mut out);
                push_frame_bound(&w.end, &mut out);
            }
        }
        QueryBody::SetOp(_) => {}
    }
    out.extend(q.limit.iter());
    out.extend(q.offset.iter());
    out
}

fn push_from_quals<'a>(item: &'a FromItem, out: &mut Vec<&'a BoundExpr>) {
    if let FromItem::Join(j) = item {
        out.extend(j.quals.iter());
        push_from_quals(&j.left, out);
        push_from_quals(&j.right, out);
    }
}

fn push_frame_bound<'a>(b: &'a FrameBound, out: &mut Vec<&'a BoundExpr>) {
    match b {
        FrameBound::Preceding(e) | FrameBound::Following(e) => out.push(e),
        FrameBound::UnboundedPreceding | FrameBound::CurrentRow | FrameBound::UnboundedFollowing => {}
    }
}

/// Sub-queries directly nested in one query block: CTE bodies, FROM sub-queries, set-operation
/// children and sub-links in its expressions.
fn child_queries(q: &BoundQuery) -> Vec<&BoundQuery> {
    let mut out: Vec<&BoundQuery> = Vec::new();
    for c in &q.ctes {
        if let CteBody::Query(b) = &c.body {
            out.push(&**b);
        }
    }
    match &q.body {
        QueryBody::Select(sel) => {
            for rte in &sel.range_table {
                if let RangeKind::Subquery { query } = &rte.kind {
                    out.push(&**query);
                }
            }
        }
        QueryBody::SetOp(so) => {
            out.push(&*so.left);
            out.push(&*so.right);
        }
        QueryBody::Values(_) => {}
    }
    for e in block_exprs(q) {
        collect_sublinks(e, &mut out);
    }
    out
}

fn collect_sublinks<'a>(e: &'a BoundExpr, out: &mut Vec<&'a BoundQuery>) {
    e.walk(&mut |x| {
        if let ExprKind::SubLink(s) = &x.kind {
            out.push(&*s.subquery);
        }
        true
    });
}

/// Is there a data-modifying statement (or a locking clause) anywhere in the query tree?
fn contain_dml(q: &BoundQuery) -> bool {
    !q.locking.is_empty()
        || q.ctes.iter().any(|c| matches!(c.body, CteBody::Modify(_)))
        || child_queries(q).into_iter().any(contain_dml)
}

/// Does a single expression call a volatile function or operator? Sub-links are not volatile by
/// themselves; their sub-queries are checked through [`child_queries`].
fn expr_volatile(e: &BoundExpr) -> bool {
    e.any(&mut |x| match &x.kind {
        ExprKind::FuncCall(f) => f.volatility == Volatility::Volatile,
        ExprKind::Op(o) => o.op.volatility == Volatility::Volatile,
        ExprKind::DistinctFrom(d) => d.eq.volatility == Volatility::Volatile,
        ExprKind::NullIf(n) => n.eq.volatility == Volatility::Volatile,
        ExprKind::ScalarArrayOp(s) => s.op.volatility == Volatility::Volatile,
        _ => false,
    })
}

/// PG `contain_volatile_functions` over the whole query tree.
fn contain_volatile(q: &BoundQuery) -> bool {
    block_exprs(q).into_iter().any(|e| expr_volatile(e)) || child_queries(q).into_iter().any(contain_volatile)
}

/// PG `contain_outer_selfref`: does the query reference a recursive CTE from outside itself?
/// `depth` is the nesting of `q` (1 for the body being tested); a self-reference whose `levels_up`
/// reaches outside the body is an outer self-reference.
fn outer_selfref(q: &BoundQuery, depth: u32) -> bool {
    if let QueryBody::Select(sel) = &q.body {
        let direct = sel.range_table.iter().any(|rte| {
            matches!(rte.kind, RangeKind::Cte { levels_up, recursive_self: true, .. } if levels_up >= depth)
        });
        if direct {
            return true;
        }
    }
    child_queries(q).into_iter().any(|c| outer_selfref(c, depth + 1))
}

// ---------------------------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::context::{PlanContext, PlanOptions};
    use pgarbor_catalog::testing;
    use pgarbor_sql::ir::{BoundSelect, OutputColumn};
    use std::collections::HashMap;

    fn empty_query() -> BoundQuery {
        BoundQuery {
            ctes: Vec::new(),
            body: QueryBody::Select(Box::new(BoundSelect::empty())),
            output: Vec::new(),
            order_by: Vec::new(),
            limit: None,
            offset: None,
            limit_with_ties: false,
            locking: Vec::new(),
        }
    }

    fn query_cte(ref_count: u32, materialized: Option<bool>) -> BoundCte {
        BoundCte {
            name: "c".to_string(),
            columns: Vec::<OutputColumn>::new(),
            body: CteBody::Query(Box::new(empty_query())),
            recursive: false,
            materialized,
            ref_count,
        }
    }

    #[test]
    fn single_reference_is_inlined() {
        assert_eq!(decide(&query_cte(1, None)), Decision::Inline);
    }

    #[test]
    fn two_references_are_materialized() {
        assert_eq!(decide(&query_cte(2, None)), Decision::Materialize);
    }

    #[test]
    fn unreferenced_select_is_dropped() {
        assert_eq!(decide(&query_cte(0, None)), Decision::Skip);
        assert_eq!(decide(&query_cte(0, Some(true))), Decision::Skip);
    }

    #[test]
    fn materialized_hint_forces_cte_def() {
        assert_eq!(decide(&query_cte(1, Some(true))), Decision::Materialize);
    }

    #[test]
    fn not_materialized_inlines_every_reference() {
        assert_eq!(decide(&query_cte(2, Some(false))), Decision::Inline);
        assert_eq!(decide(&query_cte(5, Some(false))), Decision::Inline);
    }

    #[test]
    fn recursive_cte_is_materialized() {
        let mut c = query_cte(2, None);
        c.recursive = true;
        assert_eq!(decide(&c), Decision::Materialize);
    }

    #[test]
    fn plain_query_has_no_dml_volatility_or_selfref() {
        let q = empty_query();
        assert!(!contain_dml(&q));
        assert!(!contain_volatile(&q));
        assert!(!outer_selfref(&q, 1));
    }

    #[test]
    fn locking_clause_counts_as_dml() {
        let mut q = empty_query();
        q.locking.push(pgarbor_sql::ir::LockingClause {
            strength: pgarbor_sql::ir::LockStrength::Update,
            wait: pgarbor_sql::ir::LockWait::Block,
            of: Vec::new(),
        });
        assert!(contain_dml(&q));
        assert_eq!(decide(&BoundCte { body: CteBody::Query(Box::new(q)), ..query_cte(1, None) }), Decision::Materialize);
    }

    #[test]
    fn scope_keys_are_bounded() {
        assert!(scope_key(0).is_ok());
        assert!(scope_key(31).is_ok());
        assert!(scope_key(32).is_err());
    }

    #[test]
    fn rank_counts_only_materialized_predecessors() {
        let decs = vec![Decision::Inline, Decision::Materialize, Decision::Skip, Decision::Materialize];
        assert_eq!(rank_before(&decs, 0).ok(), Some(0));
        assert_eq!(rank_before(&decs, 1).ok(), Some(0));
        assert_eq!(rank_before(&decs, 3).ok(), Some(1));
    }

    /// Needs PLN-13 (`planner::plan_query_node`) for the materialized body.
    #[test]
    #[ignore = "needs PLN-13"]
    fn two_references_register_one_cte_def() -> Result<()> {
        let (_kv, env) = testing::bootstrapped()?;
        let ctx = PlanContext {
            env: &env,
            param_types: Vec::new(),
            param_values: None,
            options: PlanOptions::default(),
            table_rows: HashMap::new(),
        };
        let mut st = PlannerState::new(&ctx);
        register_ctes(&mut st, &[query_cte(2, None)])?;
        assert_eq!(st.ctes.len(), 1);
        assert_eq!(st.ctes[0].id, 0);
        assert!(matches!(st.ctes[0].body, CteBodyPlan::Query(_)));
        unregister_ctes(&mut st);
        assert!(st.cte_scopes.is_empty());
        Ok(())
    }

    /// Needs PLN-01 (`build::build_query`) for the inlined body.
    #[test]
    #[ignore = "needs PLN-01"]
    fn single_reference_is_inlined_without_cte_def() -> Result<()> {
        let (_kv, env) = testing::bootstrapped()?;
        let ctx = PlanContext {
            env: &env,
            param_types: Vec::new(),
            param_values: None,
            options: PlanOptions::default(),
            table_rows: HashMap::new(),
        };
        let mut st = PlannerState::new(&ctx);
        register_ctes(&mut st, &[query_cte(1, None)])?;
        assert!(st.ctes.is_empty());
        let (source, columns) = plan_cte_reference(&mut st, &RangeKind::Cte { cte_index: 0, levels_up: 0, recursive_self: false })?;
        assert!(matches!(source, ScanSource::Subquery(_)));
        assert!(columns.is_empty());
        unregister_ctes(&mut st);
        Ok(())
    }
}
