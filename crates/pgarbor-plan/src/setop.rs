//! UNION / INTERSECT / EXCEPT planning (PLN-10). PG references: `optimizer/prep/prepunion.c`
//! (`plan_set_operations`, `generate_union_paths`, `generate_nonunion_paths`, `recurse_set_operations`),
//! `plan/createplan.c` (`create_append_plan`, `create_setop_plan`), `nodeSetOp.c`.
//!
//! ## Logical build (`build_setop`)
//! For `QueryBody::SetOp(SetOpBody { op, all, left, right, types })`:
//! * build both children with `build::build_query` (each child already produces the common types: the
//!   binder inserted the casts);
//! * `Union ALL` -> `LogicalSetOp { kind: UnionAll }`; nested UNION ALL of the same op are flattened
//!   by the physical planner into one `Append`;
//! * `Union` (distinct) -> `LogicalDistinct { key_slots: all, on: false }` over a `UnionAll`;
//! * `Intersect`/`Except` [ALL] -> `LogicalSetOp { kind: Setop(SetOpCmd::..) }`;
//! * `vars` of the result: `Column`-free placeholders - use the LEFT child's `vars` for `vars`
//!   (positions are what matter) with `out` = the set-op output (names of the left child, common types);
//!   `rels` empty.
//! Operator precedence/nesting is already in the bound tree.
//!
//! ## Physical
//! * `UnionAll` -> `Append(children)`; children are planned independently; type/width equality asserted.
//! * Distinct over Append -> `Distinct { Hashed }` unless the caller needs sorted output (ORDER BY on all
//!   columns) -> `Sort`+`Distinct{Sorted}` (`sort_limit::plan_distinct` decides).
//! * `Intersect`/`Except`: `SetOpNode { cmd, left, right }` (executor hashes both sides).
//! * Estimates: Append sums; INTERSECT min; EXCEPT left.
//! * EXPLAIN naming: `Append`, `HashAggregate` (union distinct), `HashSetOp Intersect`, `HashSetOp
//!   Except All`.
use pgarbor_common::{PgError, Result};
use pgarbor_sql::ir::{SetOpBody, SetOpKind};
use pgarbor_types::TypeId;

use crate::build;
use crate::context::PlannerState;
use crate::logical::{LogicalDistinct, LogicalNode, LogicalPlan, LogicalSetKind, LogicalSetOp};
use crate::plan::{AppendNode, Estimate, NodeKind, OutCol, PlanNode, SetOpCmd, SetOpNode};
use crate::planner::plan_logical;

/// Per-row CPU cost used for the set-operation estimates (same unit as `cost.rs`, `cpu_tuple_cost`).
const CPU_TUPLE_COST: f64 = 0.01;

/// Logical plan of a set-operation body.
pub fn build_setop(st: &mut PlannerState, body: &SetOpBody) -> Result<LogicalPlan> {
    let left = build::build_query(st, &body.left)?;
    let right = build::build_query(st, &body.right)?;
    wrap_setop(body.op, body.all, &body.types, left, right)
}

/// Physical plan of a `LogicalSetOp`.
pub fn plan_setop(st: &mut PlannerState, s: &LogicalSetOp) -> Result<PlanNode> {
    match s.kind {
        LogicalSetKind::UnionAll => plan_append(st, s),
        LogicalSetKind::Setop(cmd) => plan_hashed_setop(st, cmd, s),
    }
}

// ---------------------------------------------------------------------------------------------
// Logical construction
// ---------------------------------------------------------------------------------------------

/// Wrap two already-built children into the logical plan of the set operation (module docs).
/// `types` are the common result types; names come from the left child.
fn wrap_setop(op: SetOpKind, all: bool, types: &[TypeId], left: LogicalPlan, right: LogicalPlan) -> Result<LogicalPlan> {
    let width = types.len();
    if left.width() != width || right.width() != width || left.out.len() != width {
        return Err(PgError::internal(format!(
            "set operation children have {} and {} columns, expected {} (planner)",
            left.width(),
            right.width(),
            width
        )));
    }
    let out: Vec<OutCol> = left
        .out
        .iter()
        .zip(types.iter())
        .map(|(c, ty)| OutCol { name: c.name.clone(), ty: *ty })
        .collect();
    let vars = left.vars.clone();
    let level = left.level;

    let node = match op {
        SetOpKind::Union => {
            let union_all = LogicalPlan {
                node: LogicalNode::SetOp(Box::new(LogicalSetOp { kind: LogicalSetKind::UnionAll, left, right })),
                vars: vars.clone(),
                out: out.clone(),
                rels: Vec::new(),
                level,
            };
            if all {
                return Ok(union_all);
            }
            let key_slots: Vec<u32> = (0..width as u32).collect();
            LogicalNode::Distinct(Box::new(LogicalDistinct { input: union_all, key_slots, on: false }))
        }
        SetOpKind::Intersect => {
            let cmd = if all { SetOpCmd::IntersectAll } else { SetOpCmd::Intersect };
            LogicalNode::SetOp(Box::new(LogicalSetOp { kind: LogicalSetKind::Setop(cmd), left, right }))
        }
        SetOpKind::Except => {
            let cmd = if all { SetOpCmd::ExceptAll } else { SetOpCmd::Except };
            LogicalNode::SetOp(Box::new(LogicalSetOp { kind: LogicalSetKind::Setop(cmd), left, right }))
        }
    };
    Ok(LogicalPlan { node, vars, out, rels: Vec::new(), level })
}

// ---------------------------------------------------------------------------------------------
// Physical construction
// ---------------------------------------------------------------------------------------------

/// `UNION ALL`: one `Append` over the (flattened) children. PG `generate_union_paths` /
/// `create_append_plan`: a nested UNION ALL child contributes its own children directly.
fn plan_append(st: &mut PlannerState, s: &LogicalSetOp) -> Result<PlanNode> {
    let mut children: Vec<PlanNode> = Vec::new();
    let left = plan_logical(st, &s.left)?;
    push_flat(&mut children, left);
    let right = plan_logical(st, &s.right)?;
    push_flat(&mut children, right);

    let first = children.first().ok_or_else(|| PgError::internal("UNION ALL without children (planner)"))?;
    for child in children.iter().skip(1) {
        check_same_row_shape(first, child)?;
    }
    let out = first.out.clone();
    let vars = first.vars.clone();
    let est = append_estimate(&children)?;

    let mut node = st.new_node(NodeKind::Append(AppendNode { children }), out);
    node.vars = vars;
    node.est = est;
    Ok(node)
}

/// Append a planned child to `children`, splicing in the children of an `Append` (the only node kind
/// that a UNION ALL produces, so the flattening is exact).
fn push_flat(children: &mut Vec<PlanNode>, node: PlanNode) {
    if matches!(node.kind, NodeKind::Append(_)) {
        if let NodeKind::Append(a) = node.kind {
            children.extend(a.children);
        }
    } else {
        children.push(node);
    }
}

/// INTERSECT / EXCEPT [ALL]: a `SetOpNode` over the planned left and right inputs. The output row has
/// the layout of the left input (PG `generate_nonunion_paths`, `create_setop_plan`).
fn plan_hashed_setop(st: &mut PlannerState, cmd: SetOpCmd, s: &LogicalSetOp) -> Result<PlanNode> {
    let left = plan_logical(st, &s.left)?;
    let right = plan_logical(st, &s.right)?;
    check_same_row_shape(&left, &right)?;

    let est = setop_estimate(cmd, &left.est, &right.est, left.est.width);
    let out = left.out.clone();
    let vars = left.vars.clone();

    let mut node = st.new_node(NodeKind::SetOp(Box::new(SetOpNode { cmd, left, right })), out);
    node.vars = vars;
    node.est = est;
    Ok(node)
}

// ---------------------------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------------------------

/// Both inputs of a set operation must produce the same number of columns and the same types (the
/// binder inserted the casts; a mismatch here is a planner bug, reported as XX000).
fn check_same_row_shape(a: &PlanNode, b: &PlanNode) -> Result<()> {
    if a.width() != b.width() {
        return Err(PgError::internal(format!(
            "set operation inputs have {} and {} columns (planner)",
            a.width(),
            b.width()
        )));
    }
    if a.out_types() != b.out_types() {
        return Err(PgError::internal("set operation inputs have different column types (planner)"));
    }
    Ok(())
}

/// Append: rows and total cost sum over the children, startup is the first child's startup, width is
/// the widest child.
fn append_estimate(children: &[PlanNode]) -> Result<Estimate> {
    let first = children.first().ok_or_else(|| PgError::internal("Append without children (planner)"))?;
    let mut total_cost = 0.0;
    let mut rows = 0.0;
    let mut width = 0u32;
    for c in children {
        total_cost += c.est.total_cost;
        rows += c.est.rows;
        width = width.max(c.est.width);
    }
    Ok(Estimate { startup_cost: first.est.startup_cost, total_cost, rows, width })
}

/// INTERSECT: rows bounded by the smaller input; EXCEPT: rows bounded by the left input. Both inputs
/// are hashed before the first row is produced, so the startup cost includes both totals.
fn setop_estimate(cmd: SetOpCmd, left: &Estimate, right: &Estimate, width: u32) -> Estimate {
    let startup_cost = left.total_cost + right.total_cost;
    let rows = match cmd {
        SetOpCmd::Intersect | SetOpCmd::IntersectAll => left.rows.min(right.rows),
        SetOpCmd::Except | SetOpCmd::ExceptAll => left.rows,
    };
    let total_cost = startup_cost + (left.rows + right.rows) * CPU_TUPLE_COST;
    Estimate { startup_cost, total_cost, rows, width }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    use pgarbor_catalog::testing;
    use pgarbor_sql::ir::{BoundExpr, ExprKind};
    use pgarbor_types::Datum;

    use crate::context::{PlanContext, PlanOptions};
    use crate::logical::{LogicalScan, ScanSource};
    use crate::plan::ResultNode;

    fn konst(ty: TypeId) -> BoundExpr {
        BoundExpr { kind: ExprKind::Const(Datum::Null), ty }
    }

    fn leaf_plan(names: &[&str], types: &[TypeId]) -> LogicalPlan {
        LogicalPlan {
            node: LogicalNode::Scan(Box::new(LogicalScan {
                rt_index: 0,
                source: ScanSource::OneRow,
                quals: Vec::new(),
                with_rowid: false,
            })),
            vars: types.iter().map(|t| konst(*t)).collect(),
            out: names
                .iter()
                .zip(types.iter())
                .map(|(n, t)| OutCol { name: n.to_string(), ty: *t })
                .collect(),
            rels: Vec::new(),
            level: 0,
        }
    }

    fn phys_leaf(names: &[&str], types: &[TypeId], rows: f64) -> PlanNode {
        let out: Vec<OutCol> = names
            .iter()
            .zip(types.iter())
            .map(|(n, t)| OutCol { name: n.to_string(), ty: *t })
            .collect();
        let mut n = PlanNode::new(NodeKind::Result(ResultNode { exprs: Vec::new(), one_time_filter: None }), out);
        n.vars = types.iter().map(|t| konst(*t)).collect();
        n.est = Estimate { startup_cost: 0.0, total_cost: rows, rows, width: 8 };
        n
    }

    fn append_of(children: Vec<PlanNode>) -> PlanNode {
        let out = children.first().map(|c| c.out.clone()).unwrap_or_default();
        PlanNode::new(NodeKind::Append(AppendNode { children }), out)
    }

    fn setop_kind(p: &LogicalPlan) -> Option<LogicalSetKind> {
        match &p.node {
            LogicalNode::SetOp(s) => Some(s.kind),
            _ => None,
        }
    }

    #[test]
    fn union_all_wraps_setop_with_left_names() -> Result<()> {
        let l = leaf_plan(&["a", "b"], &[TypeId::INT4, TypeId::TEXT]);
        let r = leaf_plan(&["x", "y"], &[TypeId::INT4, TypeId::TEXT]);
        let p = wrap_setop(SetOpKind::Union, true, &[TypeId::INT4, TypeId::TEXT], l, r)?;
        assert_eq!(setop_kind(&p), Some(LogicalSetKind::UnionAll));
        let names: Vec<&str> = p.out.iter().map(|c| c.name.as_str()).collect();
        assert_eq!(names, vec!["a", "b"]);
        assert_eq!(p.out[1].ty, TypeId::TEXT);
        assert!(p.rels.is_empty());
        Ok(())
    }

    #[test]
    fn union_distinct_wraps_distinct_over_union_all() -> Result<()> {
        let l = leaf_plan(&["a", "b"], &[TypeId::INT4, TypeId::TEXT]);
        let r = leaf_plan(&["x", "y"], &[TypeId::INT4, TypeId::TEXT]);
        let p = wrap_setop(SetOpKind::Union, false, &[TypeId::INT4, TypeId::TEXT], l, r)?;
        match &p.node {
            LogicalNode::Distinct(d) => {
                assert_eq!(d.key_slots, vec![0u32, 1u32]);
                assert!(!d.on);
                assert_eq!(setop_kind(&d.input), Some(LogicalSetKind::UnionAll));
            }
            _ => panic!("UNION must be a Distinct over a UnionAll"),
        }
        Ok(())
    }

    #[test]
    fn intersect_and_except_map_to_setop_cmds() -> Result<()> {
        let types = [TypeId::INT4];
        let cases = [
            (SetOpKind::Intersect, false, SetOpCmd::Intersect),
            (SetOpKind::Intersect, true, SetOpCmd::IntersectAll),
            (SetOpKind::Except, false, SetOpCmd::Except),
            (SetOpKind::Except, true, SetOpCmd::ExceptAll),
        ];
        for (op, all, cmd) in cases {
            let l = leaf_plan(&["a"], &types);
            let r = leaf_plan(&["a"], &types);
            let p = wrap_setop(op, all, &types, l, r)?;
            assert_eq!(setop_kind(&p), Some(LogicalSetKind::Setop(cmd)));
        }
        Ok(())
    }

    #[test]
    fn mismatched_child_width_is_internal_error() {
        let l = leaf_plan(&["a", "b"], &[TypeId::INT4, TypeId::INT4]);
        let r = leaf_plan(&["a"], &[TypeId::INT4]);
        let res = wrap_setop(SetOpKind::Union, true, &[TypeId::INT4, TypeId::INT4], l, r);
        assert!(res.is_err());
    }

    #[test]
    fn nested_union_all_children_are_flattened() -> Result<()> {
        let a = phys_leaf(&["a"], &[TypeId::INT4], 10.0);
        let b = phys_leaf(&["a"], &[TypeId::INT4], 20.0);
        let c = phys_leaf(&["a"], &[TypeId::INT4], 30.0);
        let inner = append_of(vec![a, b]);
        let mut children = Vec::new();
        push_flat(&mut children, inner);
        push_flat(&mut children, c);
        assert_eq!(children.len(), 3);
        assert!(children.iter().all(|c| matches!(c.kind, NodeKind::Result(_))));
        Ok(())
    }

    #[test]
    fn append_estimate_sums_rows_and_takes_widest() -> Result<()> {
        let a = phys_leaf(&["a"], &[TypeId::INT4], 10.0);
        let b = phys_leaf(&["a"], &[TypeId::INT4], 5.0);
        let e = append_estimate(&[a, b])?;
        assert_eq!(e.rows, 15.0);
        assert_eq!(e.total_cost, 15.0);
        assert_eq!(e.width, 8);
        Ok(())
    }

    #[test]
    fn setop_estimates_follow_intersect_min_and_except_left() {
        let l = Estimate { startup_cost: 0.0, total_cost: 10.0, rows: 10.0, width: 8 };
        let r = Estimate { startup_cost: 0.0, total_cost: 4.0, rows: 4.0, width: 8 };
        assert_eq!(setop_estimate(SetOpCmd::Intersect, &l, &r, 8).rows, 4.0);
        assert_eq!(setop_estimate(SetOpCmd::IntersectAll, &l, &r, 8).rows, 4.0);
        assert_eq!(setop_estimate(SetOpCmd::Except, &l, &r, 8).rows, 10.0);
        assert_eq!(setop_estimate(SetOpCmd::ExceptAll, &l, &r, 8).rows, 10.0);
    }

    #[test]
    fn differing_column_types_are_internal_error() {
        let a = phys_leaf(&["a"], &[TypeId::INT4], 1.0);
        let b = phys_leaf(&["a"], &[TypeId::TEXT], 1.0);
        assert!(check_same_row_shape(&a, &b).is_err());
        assert!(check_same_row_shape(&a, &a).is_ok());
    }

    #[test]
    #[ignore = "needs PLN-13"]
    fn plan_union_all_of_three_selects_is_one_append() -> Result<()> {
        let (_kv, env) = testing::bootstrapped()?;
        let ctx = PlanContext {
            env: &env,
            param_types: Vec::new(),
            param_values: None,
            options: PlanOptions::default(),
            table_rows: HashMap::new(),
        };
        let mut st = PlannerState::new(&ctx);
        let a = leaf_plan(&["a"], &[TypeId::INT4]);
        let b = leaf_plan(&["b"], &[TypeId::INT4]);
        let c = leaf_plan(&["c"], &[TypeId::INT4]);
        let inner = wrap_setop(SetOpKind::Union, true, &[TypeId::INT4], a, b)?;
        let outer = wrap_setop(SetOpKind::Union, true, &[TypeId::INT4], inner, c)?;
        match &outer.node {
            LogicalNode::SetOp(s) => {
                let node = plan_setop(&mut st, s)?;
                match &node.kind {
                    NodeKind::Append(app) => assert_eq!(app.children.len(), 3),
                    _ => panic!("UNION ALL must plan to Append"),
                }
                assert_eq!(node.out[0].name, "a");
            }
            _ => panic!("expected a SetOp logical node"),
        }
        Ok(())
    }
}
