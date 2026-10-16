//! MERGE planning (amendment pgarbor-plan-merge-wco). PG references: `prep/prepjointree.c`
//! (`transform_MERGE_to_join`), `plan/createplan.c` (`create_modifytable_plan`), `nodeModifyTable.c`
//! (`ExecMerge*`).
//!
//! The input of the `ModifyTable` is the join of the target scan (with its rowid) and the source:
//! INNER when only `WHEN MATCHED` clauses exist, source LEFT JOIN target with `NOT MATCHED [BY TARGET]`,
//! target LEFT JOIN source with `NOT MATCHED BY SOURCE`, FULL with both. The source is wrapped in a
//! projection that adds a constant TRUE marker column, so a NULL marker means "no source row" and a NULL
//! rowid means "no target row". Every action expression is rewritten to `InputRef`s over the physical
//! join output (slots found structurally in the planned node's `vars`).
use pgarbor_common::{PgError, Result};
use pgarbor_sql::ir::{
    BoundExpr, BoundMerge, ColumnRef, ExprKind, MergeActionKind, MergeMatchKind, Overriding,
};
use pgarbor_catalog::model::GeneratedKind;
use pgarbor_types::TypeId;

use crate::build;
use crate::build_dml::{
    check_update_targets, column_of, default_expr, generated_exprs, in_level, lower_assignment, lower_returning,
    lower_value, remap_columns, table_ref, target_alias, target_checks, with_ctes,
};
use crate::context::PlannerState;
use crate::logical::{LogicalJoin, LogicalNode, LogicalPlan, LogicalProject};
use crate::plan::{
    InsertSpec, JoinType, MergePlanAction, MergePlanActionKind, MergeSpec, ModifyOp, ModifyTableNode, ModifyTarget,
    NodeKind, OutCol, PlanNode, UpdateSpec,
};
use crate::{planner, simplify, subplan};

/// MERGE.
pub fn plan_merge(st: &mut PlannerState, m: &BoundMerge) -> Result<PlanNode> {
    with_ctes(st, &m.ctes, |st| in_level(st, &m.range_table, |st| plan_merge_inner(st, m)))
}

fn same_col(v: &BoundExpr, c: &ColumnRef) -> bool {
    matches!(&v.kind, ExprKind::Column(vc) if vc.levels_up == 0 && vc.rt_index == c.rt_index && vc.attnum == c.attnum)
}

fn plan_merge_inner(st: &mut PlannerState, m: &BoundMerge) -> Result<PlanNode> {
    let target = &m.target;
    let ncols = target.columns.len() as u32;
    st.note_relation(target.oid);

    let by_source = m.whens.iter().any(|w| w.match_kind == MergeMatchKind::NotMatchedBySource);
    let by_target = m.whens.iter().any(|w| w.match_kind == MergeMatchKind::NotMatchedByTarget);

    // Target scan (with rowid) and the source with its marker column.
    let mut scan = build::build_range_scan(st, &m.range_table, m.target_rt, true)?;
    if !m.target_quals.is_empty() {
        scan.quals = simplify::simplify_quals(st, m.target_quals.clone())?;
    }
    let target_lp = build::scan_to_plan(st, &m.range_table, scan)?;
    let source_lp = build::build_from_item(st, &m.range_table, &m.source, &[])?;
    let marker = BoundExpr::bool_const(true);
    let mut src_vars = source_lp.vars.clone();
    src_vars.push(marker.clone());
    let mut src_out = source_lp.out.clone();
    src_out.push(OutCol { name: "?merge_source?".to_string(), ty: TypeId::BOOL });
    let src_rels = source_lp.rels.clone();
    let level = st.current_level();
    let n_visible = src_vars.len();
    let source_lp = LogicalPlan {
        node: LogicalNode::Project(Box::new(LogicalProject {
            input: source_lp,
            exprs: src_vars.clone(),
            n_visible,
            has_srf: false,
        })),
        vars: src_vars,
        out: src_out,
        rels: src_rels,
        level,
    };

    let quals = simplify::simplify_quals(st, simplify::split_conjuncts(m.join_condition.clone()))?;
    let (left, right, join_type) = match (by_source, by_target) {
        (false, false) => (target_lp, source_lp, JoinType::Inner),
        (true, false) => (target_lp, source_lp, JoinType::Left),
        (false, true) => (source_lp, target_lp, JoinType::Left),
        (true, true) => (target_lp, source_lp, JoinType::Full),
    };
    let mut vars = left.vars.clone();
    vars.extend(right.vars.iter().cloned());
    let mut out = left.out.clone();
    out.extend(right.out.iter().cloned());
    let mut rels = left.rels.clone();
    rels.extend(right.rels.iter().copied());
    rels.sort_unstable();
    rels.dedup();
    let joined = LogicalPlan {
        node: LogicalNode::Join(Box::new(LogicalJoin { join_type, left, right, quals, lateral: false })),
        vars,
        out,
        rels,
        level,
    };
    let input = planner::plan_logical(st, &joined)?;

    // Slots of the physical join output.
    let in_vars: Vec<BoundExpr> = input.vars.clone();
    let width = in_vars.len() as u32;
    if width == 0 {
        return Err(PgError::internal("MERGE input has no layout"));
    }
    let rowid_slot = in_vars
        .iter()
        .position(|v| matches!(&v.kind, ExprKind::Column(c) if c.levels_up == 0 && c.rt_index == m.target_rt && c.attnum == -1))
        .ok_or_else(|| PgError::unimplemented("PLN-17", "MERGE with this join shape (target on the nullable side of a swapped join)"))? as u32;
    let marker_slot = in_vars
        .iter()
        .position(|v| *v == marker)
        .ok_or_else(|| PgError::unimplemented("PLN-17", "MERGE with this join shape (source marker not exposed)"))? as u32;
    let input_map = |c: &ColumnRef| -> Result<u32> {
        if c.levels_up != 0 {
            return Err(PgError::internal("unexpected outer column reference in MERGE"));
        }
        match in_vars.iter().position(|v| same_col(v, c)) {
            Some(p) => Ok(p as u32),
            None => Err(PgError::unimplemented(
                "PLN-17",
                "this column reference in a MERGE expression (whole-row or system column)",
            )),
        }
    };

    let target_slots: Vec<Option<u32>> = target
        .columns
        .iter()
        .map(|c| {
            let probe = ColumnRef { levels_up: 0, rt_index: m.target_rt, attnum: c.attnum, pos: 0 };
            if c.dropped { None } else { in_vars.iter().position(|v| same_col(v, &probe)).map(|p| p as u32) }
        })
        .collect();
    let mut actions: Vec<MergePlanAction> = Vec::with_capacity(m.whens.len());
    for w in &m.whens {
        let condition = match &w.condition {
            Some(c) => {
                let c = simplify::simplify_expr(st, c.clone())?;
                let c = subplan::process_sublinks(st, c)?;
                Some(remap_columns(c, &input_map)?)
            }
            None => None,
        };
        let kind = match &w.action {
            MergeActionKind::DoNothing => MergePlanActionKind::DoNothing,
            MergeActionKind::Delete => MergePlanActionKind::Delete,
            MergeActionKind::Update { assignments } => {
                check_update_targets(target, assignments)?;
                let mut pairs = Vec::new();
                for a in assignments {
                    pairs.extend(lower_assignment(st, target, a)?);
                }
                let mut mapped = Vec::with_capacity(pairs.len());
                for (attnum, e) in pairs {
                    mapped.push((attnum, remap_columns(e, &input_map)?));
                }
                let generated = generated_exprs(st, target)?;
                MergePlanActionKind::Update(UpdateSpec { assignments: mapped, generated })
            }
            MergeActionKind::Insert { columns, values, overriding } => {
                let mut kept_cols: Vec<i16> = Vec::with_capacity(columns.len());
                let mut kept_vals: Vec<BoundExpr> = Vec::with_capacity(columns.len());
                for (attnum, v) in columns.iter().zip(values.iter()) {
                    let col = column_of(target, *attnum)?;
                    if *overriding == Some(Overriding::User) && col.identity.is_some() {
                        continue;
                    }
                    let e = lower_value(st, v.clone(), target, *attnum)?;
                    kept_vals.push(remap_columns(e, &input_map)?);
                    kept_cols.push(*attnum);
                }
                let mut defaults = Vec::new();
                for col in target.columns.iter() {
                    if col.dropped || kept_cols.contains(&col.attnum) {
                        continue;
                    }
                    if matches!(col.generated, Some((GeneratedKind::Stored, _))) {
                        continue;
                    }
                    let e = default_expr(st, target, col.attnum)?;
                    let e = simplify::simplify_expr(st, e)?;
                    defaults.push((col.attnum, e));
                }
                let generated = generated_exprs(st, target)?;
                MergePlanActionKind::Insert {
                    values: kept_vals,
                    spec: InsertSpec { columns: kept_cols, defaults, generated, on_conflict: None },
                }
            }
        };
        actions.push(MergePlanAction { match_kind: w.match_kind, condition, kind });
    }

    // RETURNING input: image ++ old ++ new ++ input row ++ [action].
    let (target_rt, old_rt, new_rt, action_rt) = (m.target_rt, m.old_rt, m.new_rt, m.action_rt);
    let ret_map = |c: &ColumnRef| -> Result<u32> {
        if c.levels_up != 0 {
            return Err(PgError::internal("unexpected outer column reference in MERGE RETURNING"));
        }
        let attslot = |c: &ColumnRef| -> Result<u32> {
            if c.attnum < 1 || c.attnum as u32 > ncols {
                return Err(PgError::unimplemented("PLN-17", "whole-row or system column reference in MERGE RETURNING"));
            }
            Ok(c.attnum as u32 - 1)
        };
        if c.rt_index == target_rt {
            attslot(c)
        } else if c.rt_index == old_rt {
            Ok(ncols + attslot(c)?)
        } else if c.rt_index == new_rt {
            Ok(2 * ncols + attslot(c)?)
        } else if c.rt_index == action_rt {
            Ok(3 * ncols + width)
        } else {
            Ok(3 * ncols + input_map(c)?)
        }
    };
    let (returning, out) = lower_returning(st, &m.returning, &ret_map)?;

    let table = table_ref(target, &target_alias(&m.range_table, m.target_rt, &target.name));
    let node = ModifyTableNode {
        op: ModifyOp::Merge,
        target: ModifyTarget { table, meta: target.clone(), checks: target_checks(target)?, wco: crate::build_dml::target_wco(st, target)? },
        input,
        rowid_slot: Some(rowid_slot),
        insert: None,
        update: None,
        returning,
        returning_extra_from: None,
        merge: Some(MergeSpec { source_marker_slot: marker_slot, target_slots, actions }),
    };
    Ok(st.new_node(NodeKind::ModifyTable(Box::new(node)), out))
}
