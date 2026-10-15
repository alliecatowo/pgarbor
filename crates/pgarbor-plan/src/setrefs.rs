//! Final expression translation: bind-time -> executable (PLN-14, sonnet). PG reference:
//! `optimizer/plan/setrefs.c` (`set_plan_references`, `set_plan_refs`, `set_upper_references`,
//! `set_join_references`, `fix_join_expr`, `fix_upper_expr`, `search_indexed_tlist_for_var`,
//! `search_indexed_tlist_for_non_var`, `build_tlist_index`).
//!
//! `finalize(plan)` walks the tree top-down (the root first, then children with their own `vars`) and
//! rewrites every expression stored in a node so it only contains executable forms (see `plan.rs` module
//! docs). The contract between planner and `setrefs` is the `PlanNode::vars` list: for each node, the
//! bind-time expression producing each output slot.
//!
//! ## Rules per node
//! * **Scans** (`SeqScan`, `IndexScan`, `VirtualScan`, `FunctionScan`, `ValuesScan`, `CteScan`,
//!   `WorkTableScan`): the scan's quals and other expressions are translated against the scan's OWN `vars`
//!   (the bind-time expressions it produces, `Column{0, rt_index, attnum}` in its own query level's terms),
//!   exactly like any other node translates against its child's vars: `Column` equal to `vars[i]` ->
//!   `InputRef(i)`. Scan nodes carry no rt_index: rt indexes restart in every sub-query, CTE and sub-plan
//!   and self-joins reuse one table OID, so NEVER key anything by rt_index across nodes. Whole-row
//!   `attnum 0` -> `Row` of the scan's column `InputRef`s; `tableoid` -> constant table OID; other system
//!   columns 0A000. Scan `needed` masks are NOT computed in v1: leave every entry `true`.
//! * **Project/ProjectSet/Filter/Sort/Limit/Distinct/Aggregate(input exprs)/WindowAgg/Materialize/LockRows/
//!   ModifyTable input exprs**: each `Column` or non-trivial subtree equal (`PartialEq`, ignoring
//!   `ColumnRef::pos`) to `child.vars[i]` becomes `InputRef(i)` (search the WHOLE subtree first, then
//!   recurse into children if no match - `search_indexed_tlist_for_non_var`), so shared subexpressions
//!   already computed by a lower node (grouping expressions, aggregates, window functions, SRF outputs) are
//!   referenced, not recomputed. A `Column` with no match in the child's vars is an internal error
//!   (XX000 `variable not found in subplan target list`).
//! * **Joins**: expressions over `outer ++ inner`: search the outer child's vars (slot `i`) then the inner
//!   child's vars (slot `outer.width() + i`). Join keys (`outer_keys`/`left_keys`) are translated against
//!   their own child only. `NestedLoop::inner_params` are translated against the OUTER child; inside the
//!   inner subtree `OuterRef{levels_up: 0, index}` is already in place (created by the join planner).
//! * **Aggregate**: `group_keys`, `aggs[*].args/filter/order_by/direct_args` against the input;
//!   `having` against the OUTPUT (`group_keys ++ aggs`): `Aggref` -> `InputRef(n_keys + k)` by structural
//!   match with the corresponding `AggCall`'s source `Aggref` (kept in `vars`); a group-key subexpression ->
//!   `InputRef(i)`. `Grouping{args}` -> `Grouping{args, mask_slot: Some(n_keys + n_aggs)}`.
//! * **WindowAgg**: partition/order/args/frame offsets against the input; the parent Project's
//!   `WindowFunc` -> `InputRef(input.width() + k)`.
//! * **Sub-plans**: `SubPlanRef::test` against the evaluating node's input; `SubPlanRef::args` are
//!   translated like any expression of that node; the `SubPlanDef.plan` trees are finalized
//!   independently with their own `vars` (their `OuterRef`s were created by `subplan`).
//! * **ModifyTable**: translate `insert.defaults/generated`, `update.assignments/generated`, `checks`
//!   (over the NEW row layout documented in `plan.rs`), `returning` (RETURNING input row layout),
//!   `ConflictPlanAction` expressions (existing ++ excluded layout).
//! * `Param`, `Const`, `OuterRef`, `SqlValue`, `CaseTestValue`, `SetToDefault` (error if left) unchanged.
//! * **Pass-through projections** (planner.rs sub-query boundaries): a `Project` whose translated `exprs`
//!   are exactly `InputRef(0)..InputRef(n-1)` with `n == child.width()` is removed (replaced by its child,
//!   keeping the Project's `out` names and `est`); do this bottom-up AFTER translating the node and its
//!   children so that parents' `InputRef` slots (which refer to the Project's slots) stay valid.
//! After translation clear every `PlanNode::vars`/`ordering` (set to empty, `level` to 0). Also re-derive each node's `out` types from the translated expressions when
//! the planner left them empty (`Project`, `Aggregate`, `WindowAgg`).
//!
//! Postconditions asserted in debug builds: no `Column`/`SubLink`/`Aggref`/`WindowFunc` anywhere in
//! `Plan` (use `BoundExpr::any`); `Grouping` only with `mask_slot: Some`.
use pgarbor_common::{Oid, PgError, Result, SqlState};
use pgarbor_sql::ir::{BoundExpr, ColumnRef, ExprKind, FrameBound, GroupingExpr, RowExpr};
use pgarbor_types::{Datum, TypeId};

use crate::plan::{
    AggregateNode, AppendNode, ConflictPlanAction, CteBodyPlan, KeyEq, NodeKind, OutCol, Plan, PlanNode,
    ProjectNode, TableRef,
};

/// Convert all expressions of `plan` (root, sub-plans and CTE bodies) to executable form.
pub fn finalize(plan: &mut Plan) -> Result<()> {
    fix_root(&mut plan.root)?;
    for sp in plan.subplans.iter_mut() {
        fix_root(&mut sp.plan)?;
    }
    for cte in plan.ctes.iter_mut() {
        match &mut cte.body {
            CteBodyPlan::Query(n) | CteBodyPlan::Modify(n) => fix_root(n)?,
            CteBodyPlan::Recursive { non_recursive, recursive, .. } => {
                fix_root(non_recursive)?;
                fix_root(recursive)?;
            }
        }
    }
    verify_executable(plan)
}

/// Check the executable-form postconditions (module docs); returns XX000 describing the first offending
/// node. Used by tests and `debug_assert`s.
pub fn verify_executable(plan: &Plan) -> Result<()> {
    verify_tree(&plan.root)?;
    for sp in &plan.subplans {
        verify_tree(&sp.plan)?;
    }
    for cte in &plan.ctes {
        match &cte.body {
            CteBodyPlan::Query(n) | CteBodyPlan::Modify(n) => verify_tree(n)?,
            CteBodyPlan::Recursive { non_recursive, recursive, .. } => {
                verify_tree(non_recursive)?;
                verify_tree(recursive)?;
            }
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------------------------
// Translation context
// ---------------------------------------------------------------------------------------------

/// How `GROUPING(...)` in an expression evaluated over a node's input is executed (PG
/// `set_upper_references`: the grouping mask is an extra Agg output column).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum GroupingMode {
    /// No aggregate below: GROUPING is not allowed here.
    Absent,
    /// Aggregate without grouping sets: no mask column exists; every argument is grouped, so the
    /// result is always 0 (PG `GROUPING` semantics for a single grouping set).
    Zero,
    /// Aggregate with grouping sets: the mask is input slot `n`.
    Slot(u32),
}

/// One row source a set of bind-time expressions is matched against.
#[derive(Clone, Copy)]
struct Side<'a> {
    /// Bind-time expression producing each slot of this source (`PlanNode::vars`).
    vars: &'a [BoundExpr],
    /// Slot offset of this source inside the row the expressions are evaluated over.
    offset: u32,
}

/// Owned description of a scan's table, used for whole-row and `tableoid` references.
struct ScanCol {
    ty: TypeId,
    name: String,
}

struct ScanInfo {
    oid: Oid,
    cols: Vec<ScanCol>,
}

impl ScanInfo {
    /// Whole-row reference `t` = `ROW(col1, ..)` over the live (non-dropped) slots of the scan row.
    fn whole_row(&self, ty: TypeId) -> BoundExpr {
        let mut args = Vec::with_capacity(self.cols.len());
        let mut names = Vec::with_capacity(self.cols.len());
        for (i, col) in self.cols.iter().enumerate() {
            if col.ty.oid == 0 {
                // dropped column: TypeId::of(0) by the TableRef convention
                continue;
            }
            args.push(BoundExpr::input_ref(i as u32, col.ty));
            names.push(col.name.clone());
        }
        BoundExpr::new(ExprKind::Row(Box::new(RowExpr { args, names })), ty)
    }
}

fn scan_info(table: &TableRef, out: &[OutCol]) -> ScanInfo {
    let cols = table
        .col_types
        .iter()
        .enumerate()
        .map(|(i, ty)| ScanCol {
            ty: *ty,
            name: out.get(i).map(|o| o.name.clone()).unwrap_or_else(|| format!("f{}", i + 1)),
        })
        .collect();
    ScanInfo { oid: table.oid, cols }
}

/// Matching context for one group of expressions.
struct Ctx<'a> {
    /// Searched in order; offsets make the concatenation `outer ++ inner` of joins.
    sides: Vec<Side<'a>>,
    grouping: GroupingMode,
    /// Set only for a scan's own quals: enables whole-row and `tableoid` resolution.
    scan: Option<&'a ScanInfo>,
}

impl<'a> Ctx<'a> {
    fn empty() -> Ctx<'a> {
        Ctx { sides: Vec::new(), grouping: GroupingMode::Absent, scan: None }
    }

    /// Slot of the first source whose vars structurally equal `e` (`PartialEq`, ignoring positions).
    fn lookup(&self, e: &BoundExpr) -> Option<u32> {
        for side in &self.sides {
            if let Some(i) = side.vars.iter().position(|v| v == e) {
                return Some(side.offset + i as u32);
            }
        }
        None
    }
}

/// Which row an expression slot of a node is evaluated over.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Scope {
    /// The node's own output row (scan quals, HAVING).
    Own,
    /// The child row (or `outer ++ inner` for joins).
    Input,
    /// The outer child alone (join keys, NestedLoop parameters).
    Outer,
    /// The inner child alone (join keys).
    Inner,
    /// No input row (Result, VALUES cells, function arguments, LIMIT, frame offsets, DML pre-rewritten).
    NoRow,
}

struct Scopes<'a> {
    own: Ctx<'a>,
    input: Ctx<'a>,
    outer: Ctx<'a>,
    inner: Ctx<'a>,
    no_row: Ctx<'a>,
}

impl<'a> Scopes<'a> {
    fn get(&self, s: Scope) -> &Ctx<'a> {
        match s {
            Scope::Own => &self.own,
            Scope::Input => &self.input,
            Scope::Outer => &self.outer,
            Scope::Inner => &self.inner,
            Scope::NoRow => &self.no_row,
        }
    }
}

fn aggregate_mode(a: &AggregateNode) -> GroupingMode {
    match &a.grouping_sets {
        Some(_) => GroupingMode::Slot((a.group_keys.len() + a.aggs.len()) as u32),
        None => GroupingMode::Zero,
    }
}

/// Grouping mode of the expressions evaluated over `node`'s output: found through slot-preserving
/// nodes down to an Aggregate.
fn grouping_mode(node: &PlanNode) -> GroupingMode {
    match &node.kind {
        NodeKind::Aggregate(a) => aggregate_mode(a),
        NodeKind::Filter(n) => grouping_mode(&n.input),
        NodeKind::Sort(n) => grouping_mode(&n.input),
        NodeKind::Limit(n) => grouping_mode(&n.input),
        NodeKind::Distinct(n) => grouping_mode(&n.input),
        NodeKind::Materialize(n) => grouping_mode(&n.input),
        NodeKind::LockRows(n) => grouping_mode(&n.input),
        NodeKind::WindowAgg(n) => grouping_mode(&n.input),
        _ => GroupingMode::Absent,
    }
}

// ---------------------------------------------------------------------------------------------
// Expression translation
// ---------------------------------------------------------------------------------------------

/// Translate one expression: every subtree equal to a source var becomes `InputRef`, top-down
/// (PG `search_indexed_tlist_for_non_var` / `fix_join_expr`).
fn translate(e: BoundExpr, ctx: &Ctx<'_>) -> Result<BoundExpr> {
    e.transform_down(&mut |n| translate_step(n, ctx))
}

fn translate_step(node: BoundExpr, ctx: &Ctx<'_>) -> Result<(BoundExpr, bool)> {
    if let Some(slot) = ctx.lookup(&node) {
        let ty = node.ty;
        return Ok((BoundExpr::new(ExprKind::InputRef(slot), ty), false));
    }
    let ty = node.ty;
    let descend = match &node.kind {
        ExprKind::Column(c) => {
            let c = *c;
            return Ok((resolve_column(c, ty, ctx)?, false));
        }
        ExprKind::Grouping(g) => {
            let args = g.args.clone();
            return Ok((grouping_expr(args, ctx)?, false));
        }
        ExprKind::SetToDefault { .. } => {
            return Err(PgError::new(SqlState::SYNTAX_ERROR, "DEFAULT is not allowed in this context"));
        }
        ExprKind::SubLink(_) => {
            return Err(PgError::internal("sub-link was not converted to a sub-plan"));
        }
        ExprKind::Aggref(_) => {
            return Err(PgError::internal("aggregate function not found in subplan target list"));
        }
        ExprKind::WindowFunc(_) => {
            return Err(PgError::internal("window function not found in subplan target list"));
        }
        ExprKind::Const(_)
        | ExprKind::Param(_)
        | ExprKind::OuterRef(_)
        | ExprKind::InputRef(_)
        | ExprKind::CaseTestValue
        | ExprKind::SqlValue(_) => false,
        _ => true,
    };
    Ok((node, descend))
}

/// A column that is not a var of the context: whole-row and `tableoid` of a scan's own quals, else
/// the internal error of PG `set_plan_refs` (`variable not found in subplan target list`).
fn resolve_column(c: ColumnRef, ty: TypeId, ctx: &Ctx<'_>) -> Result<BoundExpr> {
    if c.attnum < 0 && c.attnum != -6 {
        return Err(PgError::new(
            SqlState::FEATURE_NOT_SUPPORTED,
            format!("system column \"{}\" is not supported", system_column_name(c.attnum)),
        ));
    }
    if c.levels_up == 0 {
        if let Some(info) = ctx.scan {
            if c.attnum == 0 {
                return Ok(info.whole_row(ty));
            }
            if c.attnum == -6 {
                return Ok(BoundExpr::constant(Datum::Oid(info.oid), ty));
            }
        }
    }
    Err(PgError::internal("variable not found in subplan target list"))
}

fn system_column_name(attnum: i16) -> &'static str {
    match attnum {
        -1 => "ctid",
        -2 => "xmin",
        -3 => "cmin",
        -4 => "xmax",
        -5 => "cmax",
        -6 => "tableoid",
        _ => "unknown",
    }
}

/// `GROUPING(args)` evaluated over an aggregate's output (PG `set_upper_references`, `Grouping`).
fn grouping_expr(args: Vec<usize>, ctx: &Ctx<'_>) -> Result<BoundExpr> {
    match ctx.grouping {
        GroupingMode::Absent => Err(PgError::internal("GROUPING is not allowed in this context")),
        GroupingMode::Zero => Ok(BoundExpr::constant(Datum::Int4(0), TypeId::INT4)),
        GroupingMode::Slot(slot) => Ok(BoundExpr::new(
            ExprKind::Grouping(Box::new(GroupingExpr { args, mask_slot: Some(slot) })),
            TypeId::INT4,
        )),
    }
}

// ---------------------------------------------------------------------------------------------
// Expression enumeration (every expression field of every node kind)
// ---------------------------------------------------------------------------------------------

type ExprFn<'f> = dyn FnMut(&mut BoundExpr, Scope) -> Result<()> + 'f;

fn each_opt(e: &mut Option<BoundExpr>, scope: Scope, f: &mut ExprFn<'_>) -> Result<()> {
    match e {
        Some(x) => f(x, scope),
        None => Ok(()),
    }
}

fn each_vec(v: &mut [BoundExpr], scope: Scope, f: &mut ExprFn<'_>) -> Result<()> {
    for x in v.iter_mut() {
        f(x, scope)?;
    }
    Ok(())
}

fn frame_bound_mut(b: &mut FrameBound, f: &mut ExprFn<'_>) -> Result<()> {
    match b {
        FrameBound::Preceding(e) | FrameBound::Following(e) => f(e, Scope::NoRow),
        FrameBound::UnboundedPreceding | FrameBound::CurrentRow | FrameBound::UnboundedFollowing => Ok(()),
    }
}

/// Visit every expression field of `kind` (exhaustive over `NodeKind`), tagged with the row it is
/// evaluated over. Children nodes are not entered.
fn for_each_expr_mut(kind: &mut NodeKind, f: &mut ExprFn<'_>) -> Result<()> {
    match kind {
        NodeKind::Result(n) => {
            each_vec(&mut n.exprs, Scope::NoRow, f)?;
            each_opt(&mut n.one_time_filter, Scope::NoRow, f)?;
        }
        NodeKind::SeqScan(n) => each_opt(&mut n.filter, Scope::Own, f)?,
        NodeKind::IndexScan(n) => {
            for k in n.bounds.eq.iter_mut() {
                match k {
                    KeyEq::Value(e) | KeyEq::AnyOf(e) => f(e, Scope::NoRow)?,
                }
            }
            if let Some(r) = n.bounds.lower.as_mut() {
                f(&mut r.value, Scope::NoRow)?;
            }
            if let Some(r) = n.bounds.upper.as_mut() {
                f(&mut r.value, Scope::NoRow)?;
            }
            each_vec(&mut n.index_cond, Scope::Own, f)?;
            each_opt(&mut n.filter, Scope::Own, f)?;
        }
        NodeKind::VirtualScan(n) => {
            for (_, e) in n.eq_filters.iter_mut() {
                f(e, Scope::NoRow)?;
            }
            each_opt(&mut n.filter, Scope::Own, f)?;
        }
        NodeKind::ValuesScan(n) => {
            for row in n.rows.iter_mut() {
                each_vec(row, Scope::NoRow, f)?;
            }
            each_opt(&mut n.filter, Scope::Own, f)?;
        }
        NodeKind::FunctionScan(n) => {
            for item in n.funcs.iter_mut() {
                f(&mut item.expr, Scope::NoRow)?;
            }
            each_opt(&mut n.filter, Scope::Own, f)?;
        }
        NodeKind::CteScan(n) => each_opt(&mut n.filter, Scope::Own, f)?,
        NodeKind::WorkTableScan(n) => each_opt(&mut n.filter, Scope::Own, f)?,
        NodeKind::Filter(n) => f(&mut n.predicate, Scope::Input)?,
        NodeKind::Project(n) => each_vec(&mut n.exprs, Scope::Input, f)?,
        NodeKind::ProjectSet(n) => each_vec(&mut n.exprs, Scope::Input, f)?,
        NodeKind::NestedLoop(n) => {
            each_opt(&mut n.qual, Scope::Input, f)?;
            each_vec(&mut n.inner_params, Scope::Outer, f)?;
        }
        NodeKind::HashJoin(n) => {
            each_vec(&mut n.outer_keys, Scope::Outer, f)?;
            each_vec(&mut n.inner_keys, Scope::Inner, f)?;
            each_opt(&mut n.qual, Scope::Input, f)?;
        }
        NodeKind::MergeJoin(n) => {
            for k in n.left_keys.iter_mut() {
                f(&mut k.expr, Scope::Outer)?;
            }
            for k in n.right_keys.iter_mut() {
                f(&mut k.expr, Scope::Inner)?;
            }
            each_opt(&mut n.qual, Scope::Input, f)?;
        }
        NodeKind::Aggregate(n) => {
            each_vec(&mut n.group_keys, Scope::Input, f)?;
            for a in n.aggs.iter_mut() {
                each_vec(&mut a.args, Scope::Input, f)?;
                each_vec(&mut a.direct_args, Scope::Input, f)?;
                each_opt(&mut a.filter, Scope::Input, f)?;
                for k in a.order_by.iter_mut() {
                    f(&mut k.expr, Scope::Input)?;
                }
            }
            each_opt(&mut n.having, Scope::Own, f)?;
        }
        NodeKind::WindowAgg(n) => {
            each_vec(&mut n.partition_by, Scope::Input, f)?;
            for k in n.order_by.iter_mut() {
                f(&mut k.expr, Scope::Input)?;
            }
            frame_bound_mut(&mut n.frame.start, f)?;
            frame_bound_mut(&mut n.frame.end, f)?;
            for w in n.funcs.iter_mut() {
                each_vec(&mut w.args, Scope::Input, f)?;
                each_opt(&mut w.filter, Scope::Input, f)?;
            }
        }
        NodeKind::Sort(n) => {
            for k in n.keys.iter_mut() {
                f(&mut k.expr, Scope::Input)?;
            }
        }
        NodeKind::Limit(n) => {
            each_opt(&mut n.count, Scope::NoRow, f)?;
            each_opt(&mut n.offset, Scope::NoRow, f)?;
            for k in n.ties_keys.iter_mut() {
                f(&mut k.expr, Scope::Input)?;
            }
        }
        NodeKind::Distinct(_)
        | NodeKind::SetOp(_)
        | NodeKind::Append(_)
        | NodeKind::Materialize(_)
        | NodeKind::LockRows(_) => {}
        NodeKind::ModifyTable(n) => {
            // INSERT defaults/generated/ON CONFLICT and CHECK/RETURNING are pre-rewritten by the planner
            // to `InputRef` over the new row (build_dml.rs), so they are translated with no input row.
            if let Some(ins) = n.insert.as_mut() {
                for (_, e) in ins.defaults.iter_mut() {
                    f(e, Scope::NoRow)?;
                }
                for (_, e) in ins.generated.iter_mut() {
                    f(e, Scope::NoRow)?;
                }
                if let Some(conflict) = ins.on_conflict.as_mut() {
                    if let ConflictPlanAction::DoUpdate { assignments, where_ } = &mut conflict.action {
                        for (_, e) in assignments.iter_mut() {
                            f(e, Scope::NoRow)?;
                        }
                        each_opt(where_, Scope::NoRow, f)?;
                    }
                }
            }
            if let Some(upd) = n.update.as_mut() {
                for (_, e) in upd.assignments.iter_mut() {
                    f(e, Scope::Input)?;
                }
                for (_, e) in upd.generated.iter_mut() {
                    f(e, Scope::NoRow)?;
                }
            }
            for (_, _, e) in n.target.checks.iter_mut() {
                f(e, Scope::NoRow)?;
            }
            each_vec(&mut n.returning, Scope::NoRow, f)?;
        }
    }
    Ok(())
}

/// Read-only mirror of [`for_each_expr_mut`] used by `verify_executable`.
fn collect_exprs<'a>(kind: &'a NodeKind, out: &mut Vec<&'a BoundExpr>) {
    match kind {
        NodeKind::Result(n) => {
            out.extend(n.exprs.iter());
            out.extend(n.one_time_filter.iter());
        }
        NodeKind::SeqScan(n) => out.extend(n.filter.iter()),
        NodeKind::IndexScan(n) => {
            for k in &n.bounds.eq {
                match k {
                    KeyEq::Value(e) | KeyEq::AnyOf(e) => out.push(e),
                }
            }
            if let Some(r) = &n.bounds.lower {
                out.push(&r.value);
            }
            if let Some(r) = &n.bounds.upper {
                out.push(&r.value);
            }
            out.extend(n.index_cond.iter());
            out.extend(n.filter.iter());
        }
        NodeKind::VirtualScan(n) => {
            for (_, e) in &n.eq_filters {
                out.push(e);
            }
            out.extend(n.filter.iter());
        }
        NodeKind::ValuesScan(n) => {
            for row in &n.rows {
                out.extend(row.iter());
            }
            out.extend(n.filter.iter());
        }
        NodeKind::FunctionScan(n) => {
            for item in &n.funcs {
                out.push(&item.expr);
            }
            out.extend(n.filter.iter());
        }
        NodeKind::CteScan(n) => out.extend(n.filter.iter()),
        NodeKind::WorkTableScan(n) => out.extend(n.filter.iter()),
        NodeKind::Filter(n) => out.push(&n.predicate),
        NodeKind::Project(n) => out.extend(n.exprs.iter()),
        NodeKind::ProjectSet(n) => out.extend(n.exprs.iter()),
        NodeKind::NestedLoop(n) => {
            out.extend(n.qual.iter());
            out.extend(n.inner_params.iter());
        }
        NodeKind::HashJoin(n) => {
            out.extend(n.outer_keys.iter());
            out.extend(n.inner_keys.iter());
            out.extend(n.qual.iter());
        }
        NodeKind::MergeJoin(n) => {
            for k in &n.left_keys {
                out.push(&k.expr);
            }
            for k in &n.right_keys {
                out.push(&k.expr);
            }
            out.extend(n.qual.iter());
        }
        NodeKind::Aggregate(n) => {
            out.extend(n.group_keys.iter());
            for a in &n.aggs {
                out.extend(a.args.iter());
                out.extend(a.direct_args.iter());
                out.extend(a.filter.iter());
                for k in &a.order_by {
                    out.push(&k.expr);
                }
            }
            out.extend(n.having.iter());
        }
        NodeKind::WindowAgg(n) => {
            out.extend(n.partition_by.iter());
            for k in &n.order_by {
                out.push(&k.expr);
            }
            frame_bound_ref(&n.frame.start, out);
            frame_bound_ref(&n.frame.end, out);
            for w in &n.funcs {
                out.extend(w.args.iter());
                out.extend(w.filter.iter());
            }
        }
        NodeKind::Sort(n) => {
            for k in &n.keys {
                out.push(&k.expr);
            }
        }
        NodeKind::Limit(n) => {
            out.extend(n.count.iter());
            out.extend(n.offset.iter());
            for k in &n.ties_keys {
                out.push(&k.expr);
            }
        }
        NodeKind::Distinct(_)
        | NodeKind::SetOp(_)
        | NodeKind::Append(_)
        | NodeKind::Materialize(_)
        | NodeKind::LockRows(_) => {}
        NodeKind::ModifyTable(n) => {
            if let Some(ins) = &n.insert {
                for (_, e) in &ins.defaults {
                    out.push(e);
                }
                for (_, e) in &ins.generated {
                    out.push(e);
                }
                if let Some(conflict) = &ins.on_conflict {
                    if let ConflictPlanAction::DoUpdate { assignments, where_ } = &conflict.action {
                        for (_, e) in assignments {
                            out.push(e);
                        }
                        out.extend(where_.iter());
                    }
                }
            }
            if let Some(upd) = &n.update {
                for (_, e) in &upd.assignments {
                    out.push(e);
                }
                for (_, e) in &upd.generated {
                    out.push(e);
                }
            }
            for (_, _, e) in &n.target.checks {
                out.push(e);
            }
            out.extend(n.returning.iter());
        }
    }
}

fn frame_bound_ref<'a>(b: &'a FrameBound, out: &mut Vec<&'a BoundExpr>) {
    match b {
        FrameBound::Preceding(e) | FrameBound::Following(e) => out.push(e),
        FrameBound::UnboundedPreceding | FrameBound::CurrentRow | FrameBound::UnboundedFollowing => {}
    }
}

// ---------------------------------------------------------------------------------------------
// Node traversal
// ---------------------------------------------------------------------------------------------

/// Derive the output slots of a node the planner left without them (PG `set_upper_references` builds
/// the target list types from the expressions). Idempotent; recurses into the input only where the
/// output of a WindowAgg is the input row followed by the window results.
fn ensure_out(node: &mut PlanNode) -> Result<()> {
    if !node.out.is_empty() {
        return Ok(());
    }
    match &mut node.kind {
        NodeKind::Project(p) => {
            if !p.exprs.is_empty() {
                node.out = p.exprs.iter().map(|e| OutCol { name: "?column?".to_string(), ty: e.ty }).collect();
            }
        }
        NodeKind::Aggregate(a) => {
            let mut out: Vec<OutCol> =
                a.group_keys.iter().map(|e| OutCol { name: "?column?".to_string(), ty: e.ty }).collect();
            out.extend(a.aggs.iter().map(|g| OutCol { name: g.name.clone(), ty: g.result_type }));
            if a.grouping_sets.is_some() {
                out.push(OutCol { name: "grouping".to_string(), ty: TypeId::INT8 });
            }
            node.out = out;
        }
        NodeKind::WindowAgg(w) => {
            ensure_out(&mut w.input)?;
            let mut out = w.input.out.clone();
            out.extend(w.funcs.iter().map(|f| OutCol { name: f.name.clone(), ty: f.result_type }));
            node.out = out;
        }
        _ => {}
    }
    Ok(())
}

/// Translate every expression of `node` (against its children's `vars`), then recurse into the
/// children, drop identity projections among them and clear the planner scratch fields.
fn fix_node(node: &mut PlanNode) -> Result<()> {
    ensure_out(node)?;
    let own_vars: Vec<BoundExpr> = std::mem::take(&mut node.vars);
    let own_mode = match &node.kind {
        NodeKind::Aggregate(a) => aggregate_mode(a),
        _ => GroupingMode::Absent,
    };
    let scan = match &node.kind {
        NodeKind::SeqScan(s) => Some(scan_info(&s.table, &node.out)),
        NodeKind::IndexScan(s) => Some(scan_info(&s.table, &node.out)),
        NodeKind::VirtualScan(s) => Some(scan_info(&s.table, &node.out)),
        _ => None,
    };

    // Children's vars are moved out so the expressions of this node can be rewritten while the
    // contexts borrow them; they are put back before the children are processed.
    let mut child_vars: Vec<Vec<BoundExpr>> = Vec::new();
    let mut child_widths: Vec<u32> = Vec::new();
    let mut child_modes: Vec<GroupingMode> = Vec::new();
    for child in node.children_mut() {
        ensure_out(child)?;
        child_modes.push(grouping_mode(child));
        child_widths.push(child.width() as u32);
        child_vars.push(std::mem::take(&mut child.vars));
    }
    let mut offsets: Vec<u32> = Vec::with_capacity(child_widths.len());
    let mut acc: u32 = 0;
    for w in &child_widths {
        offsets.push(acc);
        acc = acc.saturating_add(*w);
    }
    let single_mode = if child_modes.len() == 1 {
        child_modes.first().copied().unwrap_or(GroupingMode::Absent)
    } else {
        GroupingMode::Absent
    };

    {
        let input_sides: Vec<Side<'_>> = child_vars
            .iter()
            .zip(offsets.iter())
            .map(|(vars, off)| Side { vars: vars.as_slice(), offset: *off })
            .collect();
        let outer_sides: Vec<Side<'_>> =
            child_vars.first().map(|vars| vec![Side { vars: vars.as_slice(), offset: 0 }]).unwrap_or_default();
        let inner_sides: Vec<Side<'_>> =
            child_vars.get(1).map(|vars| vec![Side { vars: vars.as_slice(), offset: 0 }]).unwrap_or_default();
        let scopes = Scopes {
            own: Ctx { sides: vec![Side { vars: own_vars.as_slice(), offset: 0 }], grouping: own_mode, scan: scan.as_ref() },
            input: Ctx { sides: input_sides, grouping: single_mode, scan: None },
            outer: Ctx { sides: outer_sides, grouping: GroupingMode::Absent, scan: None },
            inner: Ctx { sides: inner_sides, grouping: GroupingMode::Absent, scan: None },
            no_row: Ctx::empty(),
        };
        let mut visit = |e: &mut BoundExpr, scope: Scope| -> Result<()> {
            let taken = std::mem::replace(e, BoundExpr::bool_const(false));
            *e = translate(taken, scopes.get(scope))?;
            Ok(())
        };
        for_each_expr_mut(&mut node.kind, &mut visit)?;
    }

    for (child, vars) in node.children_mut().into_iter().zip(child_vars.into_iter()) {
        child.vars = vars;
    }
    for child in node.children_mut() {
        fix_node(child)?;
    }
    for child in node.children_mut() {
        elide_identity_project(child);
    }
    node.vars.clear();
    node.ordering.clear();
    node.level = 0;
    Ok(())
}

fn fix_root(node: &mut PlanNode) -> Result<()> {
    fix_node(node)?;
    elide_identity_project(node);
    Ok(())
}

/// A `Project` whose expressions are exactly `InputRef(0..n)` over an input of width `n` (the
/// pass-through projections of sub-query boundaries).
fn is_identity_project(node: &PlanNode) -> bool {
    match &node.kind {
        NodeKind::Project(p) => {
            p.exprs.len() == p.input.width()
                && p.exprs.iter().enumerate().all(|(i, e)| matches!(&e.kind, ExprKind::InputRef(j) if *j as usize == i))
        }
        _ => false,
    }
}

/// Replace an identity projection by its input, keeping the projection's output names and estimate.
fn elide_identity_project(node: &mut PlanNode) {
    if !is_identity_project(node) {
        return;
    }
    let placeholder = PlanNode::new(NodeKind::Append(AppendNode { children: Vec::new() }), Vec::new());
    let old = std::mem::replace(node, placeholder);
    let PlanNode { kind, out, est, props, vars, ordering, level } = old;
    match kind {
        NodeKind::Project(p) => {
            let ProjectNode { mut input, .. } = *p;
            input.out = out;
            input.est = est;
            *node = input;
        }
        other => {
            *node = PlanNode { kind: other, out, est, props, vars, ordering, level };
        }
    }
}

// ---------------------------------------------------------------------------------------------
// Verification
// ---------------------------------------------------------------------------------------------

fn verify_tree(node: &PlanNode) -> Result<()> {
    if !node.vars.is_empty() || !node.ordering.is_empty() {
        return Err(PgError::internal("planner scratch fields left in executable plan node"));
    }
    let mut exprs: Vec<&BoundExpr> = Vec::new();
    collect_exprs(&node.kind, &mut exprs);
    for e in exprs {
        verify_expr(e)?;
    }
    for child in node.children() {
        verify_tree(child)?;
    }
    Ok(())
}

fn verify_expr(e: &BoundExpr) -> Result<()> {
    let bad = e.any(&mut |x| {
        matches!(
            x.kind,
            ExprKind::Column(_)
                | ExprKind::SubLink(_)
                | ExprKind::Aggref(_)
                | ExprKind::WindowFunc(_)
                | ExprKind::SetToDefault { .. }
        )
    });
    if bad {
        return Err(PgError::internal(
            "executable plan still contains a Column, SubLink, Aggref, WindowFunc or DEFAULT expression",
        ));
    }
    let bad_grouping = e.any(&mut |x| matches!(&x.kind, ExprKind::Grouping(g) if g.mask_slot.is_none()));
    if bad_grouping {
        return Err(PgError::internal("GROUPING without a grouping mask slot in executable plan"));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::plan::{AggCall, AggStrategy, FilterNode, FrameSpec, JoinType, NestedLoopNode, PlanKind, SeqScan, SubPlanDef, WindowAggNode, WindowCall};
    use pgarbor_catalog::invalidation::PlanDeps;
    use pgarbor_catalog::persist::CatalogVersion;
    use pgarbor_sql::ir::{Aggref, FrameExclusion, FrameUnits, OuterRef, SubLinkKind, SubPlanRef, WindowFunc};
    use pgarbor_types::catalog_data::{AggKind, Volatility};

    fn col(rt: u32, attnum: i16) -> BoundExpr {
        BoundExpr::column(0, rt, attnum, TypeId::INT4)
    }

    fn outs(n: usize) -> Vec<OutCol> {
        (0..n).map(|i| OutCol { name: format!("c{}", i + 1), ty: TypeId::INT4 }).collect()
    }

    /// Scan of range-table entry `rt` with `ncols` int4 columns (a self join reuses the same OID).
    fn scan(rt: u32, ncols: u32) -> PlanNode {
        let table = TableRef {
            oid: 16384,
            schema: "public".to_string(),
            name: "t".to_string(),
            alias: format!("t{rt}"),
            ncols,
            col_types: vec![TypeId::INT4; ncols as usize],
        };
        let mut node = PlanNode::new(
            NodeKind::SeqScan(SeqScan { table, filter: None, with_rowid: false, needed: vec![true; ncols as usize] }),
            outs(ncols as usize),
        );
        node.vars = (1..=ncols).map(|i| col(rt, i as i16)).collect();
        node
    }

    fn project(input: PlanNode, exprs: Vec<BoundExpr>, out: Vec<OutCol>) -> PlanNode {
        PlanNode::new(NodeKind::Project(Box::new(ProjectNode { input, exprs })), out)
    }

    fn plan_of(root: PlanNode) -> Plan {
        Plan {
            kind: PlanKind::Select,
            root,
            subplans: Vec::new(),
            ctes: Vec::new(),
            output: Vec::new(),
            param_types: Vec::new(),
            deps: PlanDeps::default(),
            catalog_version: CatalogVersion::default(),
        }
    }

    fn count_expr(arg: BoundExpr) -> BoundExpr {
        BoundExpr::new(
            ExprKind::Aggref(Box::new(Aggref {
                agg: 2147,
                name: "count".to_string(),
                args: vec![arg],
                direct_args: Vec::new(),
                kind: AggKind::Normal,
                distinct: false,
                filter: None,
                order_by: Vec::new(),
                star: false,
                variadic: false,
                levels_up: 0,
            })),
            TypeId::INT8,
        )
    }

    #[test]
    fn scan_filter_uses_own_vars() {
        let mut s = scan(1, 2);
        if let NodeKind::SeqScan(ss) = &mut s.kind {
            ss.filter = Some(BoundExpr::not(col(1, 2)));
        }
        let mut plan = plan_of(s);
        finalize(&mut plan).expect("finalize");
        match &plan.root.kind {
            NodeKind::SeqScan(ss) => {
                let f = ss.filter.as_ref().expect("filter kept");
                match &f.kind {
                    ExprKind::Bool(b) => assert_eq!(b.args[0].kind, ExprKind::InputRef(1)),
                    other => panic!("unexpected filter {other:?}"),
                }
            }
            other => panic!("root is not a scan: {other:?}"),
        }
        assert!(verify_executable(&plan).is_ok());
    }

    #[test]
    fn identity_project_over_scan_is_removed() {
        let p = project(scan(1, 2), vec![col(1, 1), col(1, 2)], outs(2));
        let mut plan = plan_of(p);
        finalize(&mut plan).expect("finalize");
        assert!(matches!(plan.root.kind, NodeKind::SeqScan(_)));
        assert_eq!(plan.root.out.len(), 2);
    }

    #[test]
    fn self_join_qual_and_project_slots() {
        let outer = scan(1, 2);
        let inner = scan(2, 2);
        let mut join_vars = outer.vars.clone();
        join_vars.extend(inner.vars.clone());
        let qual = BoundExpr::and_all(vec![col(1, 1), col(2, 1)]);
        let mut j = PlanNode::new(
            NodeKind::NestedLoop(Box::new(NestedLoopNode {
                join_type: JoinType::Inner,
                outer,
                inner,
                qual: Some(qual),
                inner_params: Vec::new(),
            })),
            outs(4),
        );
        j.vars = join_vars;
        let root = project(j, vec![col(2, 2), col(1, 2)], outs(2));
        let mut plan = plan_of(root);
        finalize(&mut plan).expect("finalize");
        let NodeKind::Project(p) = &plan.root.kind else {
            panic!("root is not a project");
        };
        assert_eq!(p.exprs[0].kind, ExprKind::InputRef(3));
        assert_eq!(p.exprs[1].kind, ExprKind::InputRef(1));
        let NodeKind::NestedLoop(nl) = &p.input.kind else {
            panic!("input is not a nested loop");
        };
        let q = nl.qual.as_ref().expect("qual kept");
        match &q.kind {
            ExprKind::Bool(b) => {
                assert_eq!(b.args[0].kind, ExprKind::InputRef(0));
                assert_eq!(b.args[1].kind, ExprKind::InputRef(2));
            }
            other => panic!("unexpected qual {other:?}"),
        }
    }

    #[test]
    fn aggregate_having_aggref_and_grouping_mask() {
        let agg_expr = count_expr(col(1, 2));
        let mut agg = PlanNode::new(
            NodeKind::Aggregate(Box::new(AggregateNode {
                strategy: AggStrategy::Hashed,
                input: scan(1, 2),
                group_keys: vec![col(1, 1)],
                aggs: vec![AggCall {
                    agg: 2147,
                    name: "count".to_string(),
                    args: vec![col(1, 2)],
                    direct_args: Vec::new(),
                    distinct: false,
                    filter: None,
                    order_by: Vec::new(),
                    star: false,
                    result_type: TypeId::INT8,
                    arg_types: vec![TypeId::INT4],
                }],
                grouping_sets: Some(vec![vec![0]]),
                having: Some(agg_expr.clone()),
            })),
            outs(3),
        );
        agg.vars = vec![col(1, 1), agg_expr.clone()];
        let grouping = BoundExpr::new(
            ExprKind::Grouping(Box::new(GroupingExpr { args: vec![0], mask_slot: None })),
            TypeId::INT4,
        );
        let root = project(agg, vec![agg_expr.clone(), grouping], outs(2));
        let mut plan = plan_of(root);
        finalize(&mut plan).expect("finalize");
        let NodeKind::Project(p) = &plan.root.kind else {
            panic!("root is not a project");
        };
        assert_eq!(p.exprs[0].kind, ExprKind::InputRef(1));
        assert_eq!(
            p.exprs[1].kind,
            ExprKind::Grouping(Box::new(GroupingExpr { args: vec![0], mask_slot: Some(2) }))
        );
        let NodeKind::Aggregate(a) = &p.input.kind else {
            panic!("input is not an aggregate");
        };
        assert_eq!(a.group_keys[0].kind, ExprKind::InputRef(0));
        assert_eq!(a.aggs[0].args[0].kind, ExprKind::InputRef(1));
        assert_eq!(a.having.as_ref().map(|h| h.kind.clone()), Some(ExprKind::InputRef(1)));
    }

    #[test]
    fn pass_through_project_over_filtered_scan_disappears() {
        let mut s = scan(1, 2);
        if let NodeKind::SeqScan(ss) = &mut s.kind {
            ss.filter = Some(BoundExpr::not(col(1, 2)));
        }
        let mut inner = project(s, vec![col(1, 1), col(1, 2)], outs(2));
        inner.vars = vec![BoundExpr::column(0, 2, 1, TypeId::INT4), BoundExpr::column(0, 2, 2, TypeId::INT4)];
        let root = project(inner, vec![BoundExpr::column(0, 2, 2, TypeId::INT4)], outs(1));
        let mut plan = plan_of(root);
        finalize(&mut plan).expect("finalize");
        let NodeKind::Project(p) = &plan.root.kind else {
            panic!("root is not a project");
        };
        assert_eq!(p.exprs[0].kind, ExprKind::InputRef(1));
        assert!(matches!(p.input.kind, NodeKind::SeqScan(_)));
    }

    #[test]
    fn window_function_in_parent_project_maps_past_input_width() {
        let wf = BoundExpr::new(
            ExprKind::WindowFunc(Box::new(WindowFunc {
                func: 3100,
                name: "row_number".to_string(),
                args: Vec::new(),
                is_agg: false,
                star: false,
                distinct: false,
                filter: None,
                ignore_nulls: false,
                window: 0,
            })),
            TypeId::INT8,
        );
        let mut w = PlanNode::new(
            NodeKind::WindowAgg(Box::new(WindowAggNode {
                input: scan(1, 2),
                partition_by: Vec::new(),
                order_by: Vec::new(),
                frame: FrameSpec {
                    units: FrameUnits::Rows,
                    start: FrameBound::UnboundedPreceding,
                    end: FrameBound::CurrentRow,
                    exclusion: FrameExclusion::NoOthers,
                },
                window_name: "w".to_string(),
                funcs: vec![WindowCall {
                    func: 3100,
                    name: "row_number".to_string(),
                    args: Vec::new(),
                    is_agg: false,
                    star: false,
                    filter: None,
                    ignore_nulls: false,
                    result_type: TypeId::INT8,
                    arg_types: Vec::new(),
                }],
            })),
            outs(3),
        );
        w.vars = vec![col(1, 1), col(1, 2), wf.clone()];
        let root = project(w, vec![wf], outs(1));
        let mut plan = plan_of(root);
        finalize(&mut plan).expect("finalize");
        let NodeKind::Project(p) = &plan.root.kind else {
            panic!("root is not a project");
        };
        assert_eq!(p.exprs[0].kind, ExprKind::InputRef(2));
    }

    #[test]
    fn correlated_subplan_args_are_translated() {
        let mut sub_scan = scan(5, 1);
        if let NodeKind::SeqScan(ss) = &mut sub_scan.kind {
            ss.filter = Some(BoundExpr::not(BoundExpr::new(
                ExprKind::OuterRef(OuterRef { levels_up: 0, index: 0 }),
                TypeId::INT4,
            )));
        }
        let predicate = BoundExpr::new(
            ExprKind::SubPlan(Box::new(SubPlanRef {
                plan_id: 0,
                kind: SubLinkKind::Exists,
                test: None,
                ops: Vec::new(),
                args: vec![col(1, 1)],
                use_hash: false,
            })),
            TypeId::BOOL,
        );
        let filter = PlanNode::new(
            NodeKind::Filter(Box::new(FilterNode { input: scan(1, 2), predicate })),
            outs(2),
        );
        let mut plan = plan_of(filter);
        plan.subplans.push(SubPlanDef {
            id: 0,
            kind: SubLinkKind::Exists,
            plan: sub_scan,
            n_params: 1,
            is_initplan: false,
            volatility: Volatility::Volatile,
        });
        finalize(&mut plan).expect("finalize");
        let NodeKind::Filter(f) = &plan.root.kind else {
            panic!("root is not a filter");
        };
        match &f.predicate.kind {
            ExprKind::SubPlan(sp) => assert_eq!(sp.args[0].kind, ExprKind::InputRef(0)),
            other => panic!("unexpected predicate {other:?}"),
        }
        let NodeKind::SeqScan(ss) = &plan.subplans[0].plan.kind else {
            panic!("sub-plan root is not a scan");
        };
        match ss.filter.as_ref().map(|e| &e.kind) {
            Some(ExprKind::Bool(b)) => {
                assert_eq!(b.args[0].kind, ExprKind::OuterRef(OuterRef { levels_up: 0, index: 0 }))
            }
            other => panic!("unexpected sub-plan filter {other:?}"),
        }
        assert!(verify_executable(&plan).is_ok());
    }

    #[test]
    fn unmatched_column_is_internal_error() {
        let p = project(scan(1, 2), vec![BoundExpr::column(0, 9, 1, TypeId::INT4)], outs(1));
        let mut plan = plan_of(p);
        let err = finalize(&mut plan).expect_err("unmatched column must fail");
        assert_eq!(err.sqlstate.code(), "XX000");
        assert_eq!(err.message, "variable not found in subplan target list");
    }

    #[test]
    fn verify_rejects_leftover_column() {
        let plan = plan_of(project(scan(1, 2), vec![col(1, 1)], outs(1)));
        assert!(verify_executable(&plan).is_err());
    }
}
