//! Sub-links -> sub-plans (PLN-04). PG references: `optimizer/plan/subselect.c`
//! (`SS_process_sublinks`, `make_subplan`, `build_subplan`, `convert_ANY_sublink_to_join`,
//! `convert_EXISTS_sublink_to_join`, `SS_replace_correlation_vars`, `replace_correlation_vars_mutator`,
//! `SS_make_initplan_output_param`, `hash_ok_operator`), `prepjointree.c` (`pull_up_sublinks`).
//!
//! ## `process_sublinks(st, e, parent_ranges)`
//! Walks `e` (bind-time expression of the current query level) and replaces every `SubLink`
//! with an `ExprKind::SubPlan(SubPlanRef)`:
//! 1. Plan the sub-query: `build::build_query(st, &sublink.subquery)` -> `planner::plan_logical` ->
//!    `PlanNode` (nested planning; the nested level pushes its range names on `st.range_names`).
//! 2. **Correlation**: inside the sub-query, `Column{levels_up >= 1}` refer to enclosing levels. For
//!    references with `levels_up == 1` (the level that owns the SubPlan expression): allocate a parameter
//!    `i` per distinct column, rewrite the reference inside the sub-plan to `OuterRef{levels_up: 0,
//!    index: i}` and add the column (as `Column{levels_up: 0, ..}` of the owning level) to
//!    `SubPlanRef::args`. References with `levels_up >= 2` become `Column{levels_up - 1}` in the
//!    sub-plan, to be resolved when the enclosing sub-plan is itself processed (nested sub-plans pass
//!    values down through successive `args`). A sub-plan with no `OuterRef` is uncorrelated
//!    (`is_initplan = true`, executed once).
//! 3. Kind specifics (`SubLinkKind`): `Exists` (`SubPlanDef.plan` may be wrapped in `Limit 1`), `Scalar`
//!    (`Limit 2`? no: executor raises 21000; leave unwrapped), `Array`, `Row`, `Any`/`All`: the `test`
//!    expression (also processed recursively) goes into `SubPlanRef::test`, `ops` copied. `Any` over an
//!    uncorrelated sub-query with hashable `=` operator(s) (all ops are `=` and every tested type is hashable: orderable types per
//!    `compare::is_orderable`; `json`, `xml`-like and other unorderable types are not) sets `use_hash = true`.
//! 4. Each created `SubPlanDef` gets `id = st.subplans.len()`; the numbering is the order of creation
//!    (EXPLAIN prints `SubPlan {id+1}` / `InitPlan {id+1}`; PG creates inner plans first).
//!
//! ## Pull-up of EXISTS / IN into semi-joins (`pull_up_sublinks`) - optional optimization
//! For AND-ed WHERE conjuncts at the top level of a query block: `EXISTS (correlated simple SELECT ...)` ->
//! `Semi` join, `NOT EXISTS` -> `Anti` join, `x IN (SELECT y ...)` (uncorrelated or correlated, no
//! aggregates/limit) -> `Semi` join with qual `x = y`. NOT IN is never converted (NULL semantics).
//! The function may return the plan unchanged; correctness never depends on it.
//!
//! ### Implementation notes (deviations from the sketch above, all semantics-preserving)
//! * Step 1 uses `planner::plan_query_node` alone: it already runs `build::build_query` and
//!   `planner::plan_logical` (the documented contract of that function), so calling `build_query` first
//!   would build the level twice.
//! * Step 2 is applied uniformly to every `Column{levels_up = k >= 1}` found in the sub-plan's node tree
//!   (including the `args` of nested sub-plans that already sit in that tree): the reference becomes
//!   `OuterRef{0, i}` and the argument is `Column{levels_up = k - 1}` in the owner's terms, deduplicated on
//!   `(k, rt_index, attnum)`. A nested sub-plan's body is never walked again after it was created, so a
//!   reference to two levels out must become a parameter of the nested sub-plan (its `args`), which the
//!   enclosing walk then resolves in turn.
//! * `pull_up_sublinks` does not convert anything (the contract allows it); see the function docs.
use pgarbor_common::Result;
use pgarbor_sql::ir::{BoundExpr, ExprKind, FrameBound, OpRef, OuterRef, SubLink, SubLinkKind, SubPlanRef};
use pgarbor_types::catalog_data::Volatility;
use pgarbor_types::compare::is_orderable;
use pgarbor_types::TypeId;

use crate::context::PlannerState;
use crate::logical::LogicalPlan;
use crate::plan::{ConflictPlanAction, KeyEq, NodeKind, PlanNode, SubPlanDef};
use crate::planner;

/// Replace every `SubLink` inside `e` by a `SubPlan` (module docs, steps 1-4).
pub fn process_sublinks(st: &mut PlannerState, e: BoundExpr) -> Result<BoundExpr> {
    // `transform` is bottom-up: the `test` expression of a sub-link is already processed when the
    // sub-link itself is turned into a sub-plan (`map_children` visits `SubLink::test`).
    e.transform(&mut |node: BoundExpr| -> Result<BoundExpr> {
        let BoundExpr { kind, ty } = node;
        match kind {
            ExprKind::SubLink(sl) => make_subplan(&mut *st, *sl, ty),
            other => Ok(BoundExpr { kind: other, ty }),
        }
    })
}

/// Process all sub-links of a list of expressions (target list, quals, join conditions...).
pub fn process_sublinks_in(st: &mut PlannerState, exprs: Vec<BoundExpr>) -> Result<Vec<BoundExpr>> {
    let mut out = Vec::with_capacity(exprs.len());
    for e in exprs {
        out.push(process_sublinks(&mut *st, e)?);
    }
    Ok(out)
}

/// Pull up top-level EXISTS/IN sub-links of `quals` into semi/anti joins on `plan` (module docs).
/// Returns the new plan and the quals that remain (including converted-away ones removed).
///
/// Not implemented as a rewrite: the contract marks the pull-up as optional and says the plan may be
/// returned unchanged. The conversion would need the sub-query's range table to be merged into the
/// outer level (rt indexes are per level and must not be mixed in one `LogicalPlan`), which is not
/// done in this version. The quals are returned as given, so the sub-links are planned as sub-plans.
pub fn pull_up_sublinks(st: &mut PlannerState, plan: LogicalPlan, quals: Vec<BoundExpr>) -> Result<(LogicalPlan, Vec<BoundExpr>)> {
    let _ = st;
    Ok((plan, quals))
}

// ---------------------------------------------------------------------------------------------
// Sub-plan construction
// ---------------------------------------------------------------------------------------------

/// Plan one sub-link (`sl`, already processed for its `test`) into a `SubPlan` expression of type `ty`.
fn make_subplan(st: &mut PlannerState, sl: SubLink, ty: TypeId) -> Result<BoundExpr> {
    let SubLink { kind, test, ops, subquery } = sl;
    // Nested planning: registers the sub-query's own level and plans its body (no setrefs).
    let mut root = planner::plan_query_node(&mut *st, &subquery)?;
    let corr = correlate_body(&mut root)?;
    let n_params = corr.args.len() as u32;
    let is_initplan = n_params == 0;
    let use_hash = kind == SubLinkKind::Any && is_initplan && hash_ok(st, test.as_deref(), &ops, &root)?;
    // Reserve the id after the body is planned: sub-plans created while planning the body come first.
    let id = st.subplans.len() as u32;
    st.subplans.push(SubPlanDef {
        id,
        kind,
        plan: root,
        n_params,
        is_initplan,
        volatility: corr.volatility,
    });
    let sub = SubPlanRef { plan_id: id, kind, test, ops, args: corr.args, use_hash };
    Ok(BoundExpr::new(ExprKind::SubPlan(Box::new(sub)), ty))
}

/// `ANY`/`IN` may use a hash set when the sub-plan is uncorrelated, every operator is `=`, and the
/// tested values have the same type as the sub-plan output and an orderable (hashable) type.
fn hash_ok(st: &PlannerState, test: Option<&BoundExpr>, ops: &[OpRef], root: &PlanNode) -> Result<bool> {
    let test = match test {
        Some(t) => t,
        None => return Ok(false),
    };
    if ops.is_empty() || ops.iter().any(|o| o.name != "=") {
        return Ok(false);
    }
    let lefts: Vec<&BoundExpr> = match &test.kind {
        ExprKind::Row(r) => r.args.iter().collect(),
        _ => vec![test],
    };
    if lefts.len() != ops.len() || lefts.len() != root.out.len() {
        return Ok(false);
    }
    for (left, col) in lefts.iter().zip(root.out.iter()) {
        if left.ty.oid != col.ty.oid {
            return Ok(false);
        }
        if !is_orderable(st.ctx.env, left.ty)? {
            return Ok(false);
        }
    }
    Ok(true)
}

// ---------------------------------------------------------------------------------------------
// Correlation
// ---------------------------------------------------------------------------------------------

/// Result of rewriting the references of one sub-plan body.
struct Correlation {
    /// `(levels_up, rt_index, attnum)` of each parameter, in parameter order.
    keys: Vec<(u32, u32, i16)>,
    /// Parameter values, in the owning level's terms (`Column{levels_up: k - 1}` for `Column{levels_up: k}`).
    args: Vec<BoundExpr>,
    /// Worst volatility of the body's expressions.
    volatility: Volatility,
}

/// Rewrite every outer `Column` of the sub-plan tree `root` into a parameter (module docs, step 2).
fn correlate_body(root: &mut PlanNode) -> Result<Correlation> {
    let mut keys: Vec<(u32, u32, i16)> = Vec::new();
    let mut args: Vec<BoundExpr> = Vec::new();
    let mut volatility = Volatility::Immutable;
    visit_node_exprs_mut(root, &mut |x: &mut BoundExpr| -> Result<()> {
        let taken = std::mem::replace(x, BoundExpr::bool_const(false));
        let rewritten = taken.transform(&mut |n: BoundExpr| -> Result<BoundExpr> { correlate_column(n, &mut keys, &mut args) })?;
        let v = rewritten.volatility();
        if v > volatility {
            volatility = v;
        }
        *x = rewritten;
        Ok(())
    })?;
    Ok(Correlation { keys, args, volatility })
}

/// Replace one outer column reference by a parameter reference (other expressions are returned as is).
fn correlate_column(n: BoundExpr, keys: &mut Vec<(u32, u32, i16)>, args: &mut Vec<BoundExpr>) -> Result<BoundExpr> {
    let col = match &n.kind {
        ExprKind::Column(c) if c.levels_up >= 1 => Some(*c),
        _ => None,
    };
    let c = match col {
        Some(c) => c,
        None => return Ok(n),
    };
    let key = (c.levels_up, c.rt_index, c.attnum);
    let index = match keys.iter().position(|k| *k == key) {
        Some(i) => i,
        None => {
            keys.push(key);
            args.push(BoundExpr::column(c.levels_up - 1, c.rt_index, c.attnum, n.ty));
            keys.len() - 1
        }
    };
    let ty = n.ty;
    Ok(BoundExpr::new(ExprKind::OuterRef(OuterRef { levels_up: 0, index: index as u32 }), ty))
}

// ---------------------------------------------------------------------------------------------
// Generic walk over every expression of a plan tree (node-owned expressions only; sub-plan bodies
// stored in `Plan::subplans` are separate trees and are not visited).
// ---------------------------------------------------------------------------------------------

/// Apply `f` to every expression stored in `node` (and its descendants): node fields, planner scratch
/// `vars` and `ordering`.
fn visit_node_exprs_mut(node: &mut PlanNode, f: &mut dyn FnMut(&mut BoundExpr) -> Result<()>) -> Result<()> {
    visit_kind_exprs_mut(&mut node.kind, f)?;
    visit_vec(&mut node.vars, f)?;
    for k in node.ordering.iter_mut() {
        f(&mut k.expr)?;
    }
    for child in node.children_mut() {
        visit_node_exprs_mut(child, f)?;
    }
    Ok(())
}

fn visit_vec(v: &mut [BoundExpr], f: &mut dyn FnMut(&mut BoundExpr) -> Result<()>) -> Result<()> {
    for e in v.iter_mut() {
        f(e)?;
    }
    Ok(())
}

fn visit_opt(e: &mut Option<BoundExpr>, f: &mut dyn FnMut(&mut BoundExpr) -> Result<()>) -> Result<()> {
    if let Some(x) = e {
        f(x)?;
    }
    Ok(())
}

fn visit_sort_keys(keys: &mut [crate::plan::SortKeySpec], f: &mut dyn FnMut(&mut BoundExpr) -> Result<()>) -> Result<()> {
    for k in keys.iter_mut() {
        f(&mut k.expr)?;
    }
    Ok(())
}

fn visit_frame_bound(b: &mut FrameBound, f: &mut dyn FnMut(&mut BoundExpr) -> Result<()>) -> Result<()> {
    match b {
        FrameBound::Preceding(e) | FrameBound::Following(e) => f(e),
        FrameBound::UnboundedPreceding | FrameBound::CurrentRow | FrameBound::UnboundedFollowing => Ok(()),
    }
}

/// The expressions of one node kind (exhaustive over `NodeKind`; children are visited by the caller).
fn visit_kind_exprs_mut(kind: &mut NodeKind, f: &mut dyn FnMut(&mut BoundExpr) -> Result<()>) -> Result<()> {
    match kind {
        NodeKind::Result(n) => {
            visit_vec(&mut n.exprs, f)?;
            visit_opt(&mut n.one_time_filter, f)?;
        }
        NodeKind::SeqScan(n) => {
            visit_opt(&mut n.filter, f)?;
        }
        NodeKind::IndexScan(n) => {
            for k in n.bounds.eq.iter_mut() {
                match k {
                    KeyEq::Value(e) | KeyEq::AnyOf(e) => f(e)?,
                }
            }
            if let Some(r) = &mut n.bounds.lower {
                f(&mut r.value)?;
            }
            if let Some(r) = &mut n.bounds.upper {
                f(&mut r.value)?;
            }
            visit_vec(&mut n.index_cond, f)?;
            visit_opt(&mut n.filter, f)?;
        }
        NodeKind::VirtualScan(n) => {
            for (_, e) in n.eq_filters.iter_mut() {
                f(e)?;
            }
            visit_opt(&mut n.filter, f)?;
        }
        NodeKind::ValuesScan(n) => {
            for row in n.rows.iter_mut() {
                visit_vec(row, f)?;
            }
            visit_opt(&mut n.filter, f)?;
        }
        NodeKind::FunctionScan(n) => {
            for item in n.funcs.iter_mut() {
                f(&mut item.expr)?;
            }
            visit_opt(&mut n.filter, f)?;
        }
        NodeKind::CteScan(n) => {
            visit_opt(&mut n.filter, f)?;
        }
        NodeKind::WorkTableScan(n) => {
            visit_opt(&mut n.filter, f)?;
        }
        NodeKind::Filter(n) => {
            f(&mut n.predicate)?;
        }
        NodeKind::Project(n) => {
            visit_vec(&mut n.exprs, f)?;
        }
        NodeKind::ProjectSet(n) => {
            visit_vec(&mut n.exprs, f)?;
        }
        NodeKind::NestedLoop(n) => {
            visit_opt(&mut n.qual, f)?;
            visit_vec(&mut n.inner_params, f)?;
        }
        NodeKind::HashJoin(n) => {
            visit_vec(&mut n.outer_keys, f)?;
            visit_vec(&mut n.inner_keys, f)?;
            visit_opt(&mut n.qual, f)?;
        }
        NodeKind::MergeJoin(n) => {
            for k in n.left_keys.iter_mut() {
                f(&mut k.expr)?;
            }
            for k in n.right_keys.iter_mut() {
                f(&mut k.expr)?;
            }
            visit_opt(&mut n.qual, f)?;
        }
        NodeKind::Aggregate(n) => {
            visit_vec(&mut n.group_keys, f)?;
            for agg in n.aggs.iter_mut() {
                visit_vec(&mut agg.args, f)?;
                visit_vec(&mut agg.direct_args, f)?;
                visit_opt(&mut agg.filter, f)?;
                visit_sort_keys(&mut agg.order_by, f)?;
            }
            visit_opt(&mut n.having, f)?;
        }
        NodeKind::WindowAgg(n) => {
            visit_vec(&mut n.partition_by, f)?;
            visit_sort_keys(&mut n.order_by, f)?;
            visit_frame_bound(&mut n.frame.start, f)?;
            visit_frame_bound(&mut n.frame.end, f)?;
            for call in n.funcs.iter_mut() {
                visit_vec(&mut call.args, f)?;
                visit_opt(&mut call.filter, f)?;
            }
        }
        NodeKind::Sort(n) => {
            visit_sort_keys(&mut n.keys, f)?;
        }
        NodeKind::Limit(n) => {
            visit_opt(&mut n.count, f)?;
            visit_opt(&mut n.offset, f)?;
            visit_sort_keys(&mut n.ties_keys, f)?;
        }
        NodeKind::ModifyTable(n) => {
            for (_, _, e) in n.target.checks.iter_mut() {
                f(e)?;
            }
            for w in n.target.wco.iter_mut() {
                f(&mut w.expr)?;
            }
            if let Some(ins) = &mut n.insert {
                for (_, e) in ins.defaults.iter_mut() {
                    f(e)?;
                }
                for (_, e) in ins.generated.iter_mut() {
                    f(e)?;
                }
                if let Some(conflict) = &mut ins.on_conflict {
                    if let ConflictPlanAction::DoUpdate { assignments, where_ } = &mut conflict.action {
                        for (_, e) in assignments.iter_mut() {
                            f(e)?;
                        }
                        visit_opt(where_, f)?;
                    }
                }
            }
            if let Some(upd) = &mut n.update {
                for (_, e) in upd.assignments.iter_mut() {
                    f(e)?;
                }
                for (_, e) in upd.generated.iter_mut() {
                    f(e)?;
                }
            }
            if let Some(mg) = &mut n.merge {
                for a in mg.actions.iter_mut() {
                    visit_opt(&mut a.condition, f)?;
                    match &mut a.kind {
                        crate::plan::MergePlanActionKind::Update(u) => {
                            for (_, e) in u.assignments.iter_mut().chain(u.generated.iter_mut()) {
                                f(e)?;
                            }
                        }
                        crate::plan::MergePlanActionKind::Insert { values, spec } => {
                            visit_vec(values, f)?;
                            for (_, e) in spec.defaults.iter_mut().chain(spec.generated.iter_mut()) {
                                f(e)?;
                            }
                        }
                        _ => {}
                    }
                }
            }
            visit_vec(&mut n.returning, f)?;
        }
        // Nodes whose only children are plan nodes (no expressions of their own).
        NodeKind::Distinct(_) | NodeKind::SetOp(_) | NodeKind::Append(_) | NodeKind::Materialize(_) | NodeKind::LockRows(_) => {}
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::context::{PlanContext, PlanOptions};
    use crate::plan::{OutCol, ResultNode};
    use pgarbor_catalog::ddl::spec::{ColumnSpec, CreateTableSpec};
    use pgarbor_catalog::model::relation::{Persistence, RelKind};
    use pgarbor_catalog::{testing, CatalogEnv};
    use pgarbor_common::{PgError, Result as PgResult};
    use pgarbor_sql::ir::{BoundQuery, BoundStatement};
    use pgarbor_types::context::IoConfig;
    use std::collections::HashMap;

    const INT4: TypeId = TypeId::of(23);

    fn col(levels_up: u32, rt_index: u32, attnum: i16) -> BoundExpr {
        BoundExpr::column(levels_up, rt_index, attnum, INT4)
    }

    fn result_node(exprs: Vec<BoundExpr>) -> PlanNode {
        let out: Vec<OutCol> = exprs.iter().map(|_| OutCol { name: "c".to_string(), ty: INT4 }).collect();
        PlanNode::new(NodeKind::Result(ResultNode { exprs, one_time_filter: None }), out)
    }

    fn result_exprs(node: &PlanNode) -> Vec<BoundExpr> {
        match &node.kind {
            NodeKind::Result(r) => r.exprs.clone(),
            _ => Vec::new(),
        }
    }

    #[test]
    fn outer_columns_become_parameters() -> PgResult<()> {
        let mut root = result_node(vec![col(1, 1, 2), col(2, 3, 1), col(1, 1, 2), col(0, 1, 1)]);
        let corr = correlate_body(&mut root)?;
        assert_eq!(corr.keys, vec![(1, 1, 2), (2, 3, 1)]);
        assert_eq!(corr.args, vec![col(0, 1, 2), col(1, 3, 1)]);
        assert_eq!(corr.volatility, Volatility::Immutable);
        let exprs = result_exprs(&root);
        assert_eq!(exprs[0].kind, ExprKind::OuterRef(OuterRef { levels_up: 0, index: 0 }));
        assert_eq!(exprs[1].kind, ExprKind::OuterRef(OuterRef { levels_up: 0, index: 1 }));
        assert_eq!(exprs[2].kind, ExprKind::OuterRef(OuterRef { levels_up: 0, index: 0 }));
        assert!(matches!(&exprs[3].kind, ExprKind::Column(c) if c.levels_up == 0));
        Ok(())
    }

    #[test]
    fn nested_sub_plan_args_are_resolved() -> PgResult<()> {
        // A nested sub-plan inside the body whose argument refers to the owner's owner (levels_up 2
        // from the body): after this walk it refers to the owner (levels_up 1 -> parameter).
        let nested = BoundExpr::new(
            ExprKind::SubPlan(Box::new(SubPlanRef {
                plan_id: 0,
                kind: SubLinkKind::Exists,
                test: None,
                ops: Vec::new(),
                args: vec![col(2, 5, 3)],
                use_hash: false,
            })),
            TypeId::BOOL,
        );
        let mut root = result_node(vec![nested]);
        let corr = correlate_body(&mut root)?;
        assert_eq!(corr.keys, vec![(2, 5, 3)]);
        assert_eq!(corr.args, vec![col(1, 5, 3)]);
        let exprs = result_exprs(&root);
        match &exprs[0].kind {
            ExprKind::SubPlan(sp) => {
                assert_eq!(sp.args, vec![BoundExpr::new(ExprKind::OuterRef(OuterRef { levels_up: 0, index: 0 }), INT4)]);
            }
            other => return Err(PgError::internal(format!("unexpected kind {other:?}"))),
        }
        Ok(())
    }

    #[test]
    fn plain_expressions_are_untouched() -> PgResult<()> {
        let mut root = result_node(vec![BoundExpr::constant(pgarbor_types::Datum::Null, INT4)]);
        let corr = correlate_body(&mut root)?;
        assert!(corr.args.is_empty());
        assert!(corr.keys.is_empty());
        assert_eq!(result_exprs(&root)[0].kind, ExprKind::Const(pgarbor_types::Datum::Null));
        Ok(())
    }

    fn table_spec(name: &str) -> CreateTableSpec {
        CreateTableSpec {
            namespace: 2200,
            name: name.to_string(),
            owner: 10,
            kind: RelKind::Table,
            persistence: Persistence::Permanent,
            columns: vec![ColumnSpec::new("a", INT4), ColumnSpec::new("d", INT4)],
            constraints: Vec::new(),
            options: Vec::new(),
            if_not_exists: false,
            inherits: Vec::new(),
            extra: Default::default(),
        }
    }

    /// Catalog with tables `t(a, d)` and `u(a, d)`.
    fn two_tables() -> PgResult<CatalogEnv> {
        let (mut kv, env) = testing::bootstrapped()?;
        let (_, env) = testing::run(&mut kv, &env, |t| t.create_table(&table_spec("t")).map(|_| ()))?;
        let (_, env) = testing::run(&mut kv, &env, |t| t.create_table(&table_spec("u")).map(|_| ()))?;
        Ok(env)
    }

    fn bind_query(env: &CatalogEnv, sql: &str) -> PgResult<BoundQuery> {
        let raw = pgarbor_sql::parse_one(sql)?;
        let ctx = pgarbor_sql::BindContext {
            env,
            session: pgarbor_sql::BindSession { database: "postgres".to_string() },
            io_config: IoConfig::default(),
            statement_timestamp: 0,
            declared_params: Vec::new(),
        };
        let out = pgarbor_sql::bind(&raw, &ctx)?;
        match out.statement {
            BoundStatement::Query(q) => Ok(*q),
            _ => Err(PgError::internal("test statement is not a query")),
        }
    }

    fn find_subplan(e: &BoundExpr) -> Option<&SubPlanRef> {
        let mut found: Option<&SubPlanRef> = None;
        e.walk(&mut |x| {
            if let ExprKind::SubPlan(sp) = &x.kind {
                if found.is_none() {
                    found = Some(&**sp);
                }
            }
            true
        });
        found
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
    #[ignore = "needs PLN-01 (build::build_query) and PLN-13 (planner::plan_query_node)"]
    fn scalar_uncorrelated_is_initplan() -> PgResult<()> {
        let env = two_tables()?;
        let q = bind_query(&env, "select a from t where a = (select max(d) from u)")?;
        let e = match q.as_select().and_then(|s| s.where_.clone()) {
            Some(e) => e,
            None => return Err(PgError::internal("test query has no WHERE")),
        };
        let ctx = plan_ctx(&env);
        let mut st = PlannerState::new(&ctx);
        let out = process_sublinks(&mut st, e)?;
        let sp = match find_subplan(&out) {
            Some(sp) => sp,
            None => return Err(PgError::internal("no SubPlan produced")),
        };
        assert_eq!(st.subplans.len(), 1);
        assert!(st.subplans[0].is_initplan);
        assert!(sp.args.is_empty());
        Ok(())
    }

    #[test]
    #[ignore = "needs PLN-01 (build::build_query) and PLN-13 (planner::plan_query_node)"]
    fn correlated_scalar_has_one_arg() -> PgResult<()> {
        let env = two_tables()?;
        let q = bind_query(&env, "select (select max(d) from u where u.a = t.a) from t")?;
        let sel = match q.as_select() {
            Some(s) => s,
            None => return Err(PgError::internal("test query is not a select")),
        };
        let e = sel.target_list[0].expr.clone();
        let ctx = plan_ctx(&env);
        let mut st = PlannerState::new(&ctx);
        let out = process_sublinks(&mut st, e)?;
        let sp = match find_subplan(&out) {
            Some(sp) => sp,
            None => return Err(PgError::internal("no SubPlan produced")),
        };
        assert_eq!(sp.args.len(), 1);
        assert!(matches!(&sp.args[0].kind, ExprKind::Column(c) if c.levels_up == 0));
        assert_eq!(st.subplans[0].n_params, 1);
        assert!(!st.subplans[0].is_initplan);
        Ok(())
    }

    #[test]
    #[ignore = "needs PLN-01 (build::build_query) and PLN-13 (planner::plan_query_node)"]
    fn uncorrelated_in_is_hashed() -> PgResult<()> {
        let env = two_tables()?;
        let q = bind_query(&env, "select a from t where a in (select d from u)")?;
        let e = match q.as_select().and_then(|s| s.where_.clone()) {
            Some(e) => e,
            None => return Err(PgError::internal("test query has no WHERE")),
        };
        let ctx = plan_ctx(&env);
        let mut st = PlannerState::new(&ctx);
        let out = process_sublinks(&mut st, e)?;
        let sp = match find_subplan(&out) {
            Some(sp) => sp,
            None => return Err(PgError::internal("no SubPlan produced")),
        };
        assert_eq!(sp.kind, SubLinkKind::Any);
        assert!(sp.use_hash);
        Ok(())
    }

    #[test]
    #[ignore = "needs PLN-01 (build::build_query) and PLN-13 (planner::plan_query_node)"]
    fn not_in_stays_a_sub_plan() -> PgResult<()> {
        let env = two_tables()?;
        let q = bind_query(&env, "select a from t where a not in (select d from u)")?;
        let e = match q.as_select().and_then(|s| s.where_.clone()) {
            Some(e) => e,
            None => return Err(PgError::internal("test query has no WHERE")),
        };
        let ctx = plan_ctx(&env);
        let mut st = PlannerState::new(&ctx);
        let out = process_sublinks(&mut st, e)?;
        assert!(find_subplan(&out).is_some());
        assert_eq!(st.subplans.len(), 1);
        Ok(())
    }

    #[test]
    #[ignore = "needs PLN-01 (build::build_query) and PLN-13 (planner::plan_query_node)"]
    fn nested_correlation_passes_args_down() -> PgResult<()> {
        let env = two_tables()?;
        let q = bind_query(&env, "select a from t where exists (select 1 from u where exists (select 1 from u as u2 where u2.a = t.a))")?;
        let e = match q.as_select().and_then(|s| s.where_.clone()) {
            Some(e) => e,
            None => return Err(PgError::internal("test query has no WHERE")),
        };
        let ctx = plan_ctx(&env);
        let mut st = PlannerState::new(&ctx);
        let out = process_sublinks(&mut st, e)?;
        // Inner sub-plan is created first (id 0), the outer one second (id 1).
        assert_eq!(st.subplans.len(), 2);
        assert_eq!(st.subplans[0].n_params, 1);
        assert_eq!(st.subplans[1].n_params, 1);
        let sp = match find_subplan(&out) {
            Some(sp) => sp,
            None => return Err(PgError::internal("no SubPlan produced")),
        };
        assert_eq!(sp.plan_id, 1);
        assert!(matches!(&sp.args[0].kind, ExprKind::Column(c) if c.levels_up == 0));
        Ok(())
    }
}
