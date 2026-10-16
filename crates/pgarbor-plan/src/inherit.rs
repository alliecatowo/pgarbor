//! Inheritance / partition expansion of table scans (amendment pgarbor-plan-05). PG references:
//! `optimizer/util/inherit.c` (`expand_inherited_rtentry`, `make_inh_translation_list`),
//! `optimizer/path/allpaths.c` (`set_append_rel_pathlist`), `prep/prepunion.c`.
//!
//! A range entry for a table that has inheritance children (or partitions) and was not written
//! `ONLY` scans the whole inheritance tree (`build::build_range_scan` records it in
//! `PlannerState::inherit_scans`). [`plan_inherited_scan`] plans one scan per member that has
//! storage (the parent itself first, then descendants breadth-first; partitioned tables have no
//! rows) and an `Append` over them. Each member scan is projected to the PARENT's row layout:
//! parent columns are matched by name (dropped parent columns are NULL), `tableoid` becomes the
//! member's OID, and the rowid slot (UPDATE/DELETE targets) carries
//! `ROW(member oid, rowid)` for members other than the parent itself (`pgarbor_exec::dml_inherit`
//! decodes it). The scan's pushed-down quals are translated to each member's attnums.
use pgarbor_catalog::model::Relation;
use pgarbor_common::{Oid, PgError, Result};
use pgarbor_sql::ir::{BoundExpr, ExprKind, RowExpr};
use pgarbor_types::{Datum, TypeId};

use crate::context::PlannerState;
use crate::cost;
use crate::index;
use crate::logical::{LogicalPlan, LogicalScan, ScanSource};
use crate::plan::{AppendNode, NodeKind, OutCol, PlanNode, ProjectNode, ResultNode, TableRef};
use crate::planner::CTID_ATTNUM;
use crate::subplan;

/// Parent attnum -> member attnum (by column name), `None` for dropped parent columns.
pub fn attribute_map(parent: &Relation, member: &Relation) -> Vec<Option<i16>> {
    parent
        .columns
        .iter()
        .map(|c| if c.dropped { None } else { member.column_by_name(&c.name).map(|m| m.attnum) })
        .collect()
}

/// Rewrite `Column{levels_up: 0, rt_index: rt, attnum > 0}` through `map` (parent attnum -> member attnum).
pub fn map_columns(e: BoundExpr, rt: u32, map: &[Option<i16>]) -> Result<BoundExpr> {
    e.transform(&mut |x: BoundExpr| -> Result<BoundExpr> {
        if let ExprKind::Column(c) = &x.kind {
            if c.levels_up == 0 && c.rt_index == rt && c.attnum > 0 {
                let mapped = map.get((c.attnum - 1) as usize).copied().flatten();
                return match mapped {
                    Some(a) => {
                        let mut c2 = c.clone();
                        c2.attnum = a;
                        Ok(BoundExpr::new(ExprKind::Column(c2), x.ty))
                    }
                    None => Ok(BoundExpr::null(x.ty)),
                };
            }
        }
        Ok(x)
    })
}

/// Plan the scan of an inheritance tree (module docs).
pub fn plan_inherited_scan(st: &mut PlannerState, lp: &LogicalPlan, scan: &LogicalScan, parent: &TableRef) -> Result<PlanNode> {
    let env = st.ctx.env;
    let snap = env.snapshot().clone();
    let prel = snap
        .relation(parent.oid)
        .ok_or_else(|| PgError::internal(format!("could not open relation with OID {}", parent.oid)))?
        .clone();
    let rt = scan.rt_index;
    let identity = lp.vars.last().is_some_and(|e| matches!(&e.kind, ExprKind::Column(c) if c.attnum == -6));
    let width = lp.vars.len() - usize::from(identity);
    let quals = subplan::process_sublinks_in(st, scan.quals.clone())?;
    let mut out_vars: Vec<BoundExpr> = lp.vars.clone();
    let mut out_cols: Vec<OutCol> = lp.out.clone();
    if scan.with_rowid {
        out_vars.push(BoundExpr::column(0, rt, CTID_ATTNUM, TypeId::INT8));
        out_cols.push(OutCol { name: "ctid".to_string(), ty: TypeId::INT8 });
    }
    let mut children: Vec<PlanNode> = Vec::new();
    for member in snap.inheritance_tree(parent.oid) {
        let Some(mrel) = snap.relation(member) else { continue };
        if !mrel.kind.has_storage() {
            continue;
        }
        st.note_relation(member);
        let map = attribute_map(&prel, mrel);
        let schema = snap.namespace(mrel.namespace).map(|n| n.name.clone()).unwrap_or_default();
        let col_types: Vec<TypeId> = mrel.columns.iter().map(|c| if c.dropped { TypeId::of(0) } else { c.type_id }).collect();
        let mref = TableRef {
            oid: member,
            schema,
            name: mrel.name.clone(),
            alias: if member == parent.oid { parent.alias.clone() } else { mrel.name.clone() },
            ncols: mrel.columns.len() as u32,
            col_types: col_types.clone(),
        };
        let mut mquals = Vec::with_capacity(quals.len());
        for q in &quals {
            mquals.push(map_columns(q.clone(), rt, &map)?);
        }
        let mscan = LogicalScan {
            rt_index: rt,
            source: ScanSource::Table { table: mref, is_virtual: false },
            quals: mquals,
            with_rowid: scan.with_rowid,
        };
        let mut node = index::choose_scan(st, &mscan, None, false)?;
        node.vars = mrel
            .columns
            .iter()
            .map(|c| BoundExpr::column(0, rt, c.attnum, if c.dropped { TypeId::of(0) } else { c.type_id }))
            .collect();
        node.out = mrel
            .columns
            .iter()
            .map(|c| OutCol { name: c.name.clone(), ty: if c.dropped { TypeId::of(0) } else { c.type_id } })
            .collect();
        if scan.with_rowid {
            node.vars.push(BoundExpr::column(0, rt, CTID_ATTNUM, TypeId::INT8));
            node.out.push(OutCol { name: "ctid".to_string(), ty: TypeId::INT8 });
        }
        let mut exprs: Vec<BoundExpr> = Vec::with_capacity(out_vars.len());
        for (i, v) in lp.vars[..width].iter().enumerate() {
            match &v.kind {
                ExprKind::Column(c) if c.levels_up == 0 && c.rt_index == rt && c.attnum > 0 => {
                    exprs.push(map_columns(v.clone(), rt, &map)?);
                }
                _ => {
                    let ty = lp.out.get(i).map(|o| o.ty).unwrap_or(v.ty);
                    exprs.push(BoundExpr::null(ty));
                }
            }
        }
        if identity {
            exprs.push(BoundExpr::constant(Datum::Oid(member), TypeId::OID));
        }
        if scan.with_rowid {
            let rowid = BoundExpr::column(0, rt, CTID_ATTNUM, TypeId::INT8);
            if member == parent.oid {
                exprs.push(rowid);
            } else {
                exprs.push(member_rowid(member, rowid));
            }
        }
        let est = node.est;
        let mut projected = st.new_node(NodeKind::Project(Box::new(ProjectNode { input: node, exprs })), out_cols.clone());
        projected.vars = out_vars.clone();
        projected.est = est;
        children.push(projected);
    }
    match children.len() {
        0 => {
            let exprs: Vec<BoundExpr> = out_cols.iter().map(|c| BoundExpr::null(c.ty)).collect();
            let mut node = st.new_node(
                NodeKind::Result(ResultNode {
                    exprs,
                    one_time_filter: Some(BoundExpr::constant(Datum::Bool(false), TypeId::BOOL)),
                }),
                out_cols,
            );
            node.vars = out_vars;
            Ok(node)
        }
        1 => Ok(children.pop().unwrap_or_else(|| PlanNode::new(NodeKind::Append(AppendNode { children: Vec::new() }), Vec::new()))),
        _ => {
            let ests: Vec<_> = children.iter().map(|c| c.est).collect();
            let rows: f64 = ests.iter().map(|e| e.rows).sum();
            let width_b = (out_cols.len() as u32) * 8;
            let est = cost::cost_node("append", rows, width_b, &ests)?;
            let mut node = st.new_node(NodeKind::Append(AppendNode { children }), out_cols);
            node.vars = out_vars;
            node.est = est;
            Ok(node)
        }
    }
}

/// `ROW(member oid, rowid)`: the identity of a row of an inheritance member.
pub fn member_rowid(member: Oid, rowid: BoundExpr) -> BoundExpr {
    BoundExpr::new(
        ExprKind::Row(Box::new(RowExpr {
            args: vec![BoundExpr::constant(Datum::Oid(member), TypeId::OID), rowid],
            names: vec!["f1".to_string(), "f2".to_string()],
        })),
        TypeId::of(pgarbor_types::oids::RECORD),
    )
}
