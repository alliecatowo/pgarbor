//! INSERT / UPDATE / DELETE / MERGE through automatically updatable views (amendment pgarbor-sql-rewrite-rls-merge).
//! PG references: `rewriteHandler.c` (`view_query_is_auto_updatable`, `view_col_is_auto_updatable`,
//! `view_cols_are_auto_updatable`, `rewriteTargetView`, `error_view_not_updatable`).
//!
//! PGArbor does the rewrite at bind time:
//! * [`open_view_target`] (called by `target_table::open_target_table` for a view) analyzes the view
//!   chain down to its base table ([`analyze_view`]), registers the BASE table as range entry of the
//!   statement under the view's alias, and a namespace item whose columns are the VIEW's columns, each
//!   evaluating to its expression over the base table. Everything the statement says about view
//!   columns (WHERE, SET values, RETURNING, correlated sub-queries) therefore binds directly against the
//!   base table. The returned `TargetTable` describes the view (names, types, view defaults) so column
//!   lists and SET targets resolve to VIEW attnums.
//! * [`rewrite_insert`] / [`rewrite_update`] / [`rewrite_delete`] then run PG's rewriter checks (55000
//!   "cannot insert into view", 0A000 "cannot insert into column"), map view attnums to base attnums,
//!   add the view quals to UPDATE/DELETE, build the WITH CHECK OPTIONs and replace the target by the base
//!   table (view column defaults override the base defaults).
use pgarbor_catalog::model::RelKind;
use pgarbor_catalog::Relation;
use pgarbor_common::{Oid, PgError, Result, SqlState};
use sqlparser::ast::{Ident, Statement};

use super::context::Binder;
use super::from::relation_range_entry;
use super::scope::{add_range_entry, NsColumn, NsItem};
use super::select;
use super::target_table::{build_target_table, DmlKind};
use crate::ir::{
    Assignment, BoolExpr, BoolOp, BoundDelete, BoundExpr, BoundInsert, BoundQuery, BoundUpdate, DistinctSpec,
    ExprKind, FromItem, InsertSource, QueryBody, RangeKind, TargetTable, TargetWco, WcoKind,
};
use crate::parser;

/// SQLSTATE 55000 `object_not_in_prerequisite_state`.
const NOT_IN_PREREQUISITE_STATE: SqlState = SqlState(*b"55000");

/// One view of the chain from the DML target down to the base table.
#[derive(Clone, Debug)]
pub struct ViewLevel {
    pub view_oid: Oid,
    pub view_name: String,
    pub check_option: Option<String>,
    /// Statement-level reason the view is not auto-updatable (check_cols = false).
    pub stmt_detail: Option<&'static str>,
    pub has_updatable_col: bool,
    /// Per view attnum (index = attnum - 1): name, reason the column is not updatable, and the attnum
    /// of the next level (the inner view's column or the base column).
    pub col_names: Vec<String>,
    pub col_details: Vec<Option<&'static str>>,
    pub col_map: Vec<Option<i16>>,
    /// The view's WHERE, over the BASE table (`Column{0, 1, attnum}`).
    pub qual: Option<BoundExpr>,
}

/// Result of [`analyze_view`].
#[derive(Clone, Debug)]
pub struct ViewInfo {
    /// Outermost (the DML target) first. Analysis stops at the first non-updatable level.
    pub levels: Vec<ViewLevel>,
    /// The base table when every level is updatable.
    pub base: Option<Oid>,
    /// Per OUTER view attnum: the column expression over the base table (`None` when unavailable).
    pub col_exprs: Vec<Option<BoundExpr>>,
}

impl ViewInfo {
    /// Base attnum of an outer view column (through every level), if it is updatable everywhere.
    pub fn base_attnum(&self, attnum: i16) -> Option<i16> {
        let mut a = attnum;
        for level in &self.levels {
            let idx = usize::try_from(a).ok()?.checked_sub(1)?;
            if level.col_details.get(idx).copied().flatten().is_some() {
                return None;
            }
            a = level.col_map.get(idx).copied().flatten()?;
        }
        self.base.map(|_| a)
    }
}

/// Bind the stored definition of view `rel` (PG `ApplyRetrieveRule`).
fn bind_view_query(b: &mut Binder, rel: &Relation) -> Result<BoundQuery> {
    let view = rel
        .view
        .as_ref()
        .ok_or_else(|| PgError::internal(format!("view \"{}\" has no definition", rel.name)))?;
    if b.expanding_views.contains(&rel.oid) {
        return Err(PgError::new(
            SqlState::INVALID_OBJECT_DEFINITION,
            format!("infinite recursion detected in rules for relation \"{}\"", rel.name),
        ));
    }
    let raw = parser::parse_one(&view.query.sql).map_err(|mut e| {
        e.position = None;
        e
    })?;
    let query = match raw.as_ast() {
        Some(Statement::Query(q)) => (**q).clone(),
        _ => return Err(PgError::internal(format!("stored definition of view \"{}\" is not a query", rel.name))),
    };
    let names: Vec<String> = rel.columns.iter().filter(|c| !c.dropped).map(|c| c.name.clone()).collect();
    b.expanding_views.push(rel.oid);
    let bound = select::bind_subselect(b, &query, &names);
    b.expanding_views.pop();
    bound.map_err(|mut e| {
        e.position = None;
        e
    })
}

/// PG `view_query_is_auto_updatable` (check_cols = false).
fn query_detail(q: &BoundQuery) -> Option<&'static str> {
    let sel = match &q.body {
        QueryBody::Select(sel) => sel,
        QueryBody::SetOp(_) => return Some("Views containing UNION, INTERSECT, or EXCEPT are not automatically updatable."),
        QueryBody::Values(_) => {
            return Some("Views that do not select from a single table or view are not automatically updatable.")
        }
    };
    if !matches!(sel.distinct, DistinctSpec::None) {
        return Some("Views containing DISTINCT are not automatically updatable.");
    }
    if !sel.group_by.exprs.is_empty() || sel.group_by.sets.is_some() {
        return Some("Views containing GROUP BY are not automatically updatable.");
    }
    if sel.having.is_some() {
        return Some("Views containing HAVING are not automatically updatable.");
    }
    if !q.ctes.is_empty() {
        return Some("Views containing WITH are not automatically updatable.");
    }
    if q.limit.is_some() || q.offset.is_some() {
        return Some("Views containing LIMIT or OFFSET are not automatically updatable.");
    }
    if sel.has_aggs {
        return Some("Views that return aggregate functions are not automatically updatable.");
    }
    if sel.has_windows {
        return Some("Views that return window functions are not automatically updatable.");
    }
    if sel.has_target_srfs {
        return Some("Views that return set-returning functions are not automatically updatable.");
    }
    if sel.from.len() != 1 || !matches!(sel.from[0], FromItem::Range(_)) {
        return Some("Views that do not select from a single table or view are not automatically updatable.");
    }
    None
}

const NOT_SINGLE: &str = "Views that do not select from a single table or view are not automatically updatable.";

/// What the single FROM item of an updatable view is.
enum Inner {
    Base(Oid),
    View(Oid),
}

/// Analyze the chain of views starting at `rel` (module docs).
pub fn analyze_view(b: &mut Binder, rel: &Relation) -> Result<ViewInfo> {
    let mut levels: Vec<ViewLevel> = Vec::new();
    // Column expressions of each level over ITS from entry (rt k), plus that rt.
    let mut raw_exprs: Vec<(u32, Vec<Option<BoundExpr>>)> = Vec::new();
    let env = b.env;
    let mut cur: Relation = rel.clone();
    let mut base: Option<Oid> = None;
    for _depth in 0..64 {
        let q = bind_view_query(b, &cur)?;
        let check_option = cur.view.as_ref().and_then(|v| v.check_option.clone());
        let col_names: Vec<String> = cur.columns.iter().map(|c| c.name.clone()).collect();
        let ncols = cur.columns.len();
        let mut level = ViewLevel {
            view_oid: cur.oid,
            view_name: cur.name.clone(),
            check_option,
            stmt_detail: query_detail(&q),
            has_updatable_col: false,
            col_names,
            col_details: vec![Some("View columns that are not columns of their base relation are not updatable."); ncols],
            col_map: vec![None; ncols],
            qual: None,
        };
        if level.stmt_detail.is_some() {
            levels.push(level);
            raw_exprs.push((1, vec![None; ncols]));
            break;
        }
        let sel = match &q.body {
            QueryBody::Select(sel) => sel,
            _ => return Err(PgError::internal("view analysis: not a SELECT")),
        };
        let from_rt = match sel.from.first() {
            Some(FromItem::Range(rt)) => *rt,
            _ => return Err(PgError::internal("view analysis: no single FROM item")),
        };
        let entry = sel
            .range_table
            .get((from_rt as usize).saturating_sub(1))
            .ok_or_else(|| PgError::internal("view analysis: FROM entry missing"))?;
        let inner = match &entry.kind {
            RangeKind::Relation { oid, rel_kind: RelKind::Table | RelKind::PartitionedTable, .. } => Some(Inner::Base(*oid)),
            RangeKind::Subquery { query } => {
                // An expanded view: `from::expand_view` stamps the view as the origin of its outputs. A
                // stored definition may also carry the inner view inlined as a sub-select aliased with
                // the view's name; the view is then one of the outer view's dependencies.
                let origin = query.output.first().and_then(|c| c.origin).map(|o| o.table);
                match origin.and_then(|o| env.snapshot().relation(o)) {
                    Some(r) if r.kind == RelKind::View => Some(Inner::View(r.oid)),
                    _ => cur
                        .view
                        .as_ref()
                        .into_iter()
                        .flat_map(|v| v.query.depends_on.iter())
                        .filter(|d| d.class == pgarbor_catalog::ObjClass::Relation && d.sub_id == 0)
                        .filter_map(|d| env.snapshot().relation(d.oid))
                        .find(|r| r.kind == RelKind::View && r.name == entry.alias)
                        .map(|r| Inner::View(r.oid)),
                }
            }
            _ => None,
        };
        let Some(inner) = inner else {
            level.stmt_detail = Some(NOT_SINGLE);
            levels.push(level);
            raw_exprs.push((1, vec![None; ncols]));
            break;
        };
        // Columns: the non-junk target entries map to the non-dropped view columns in order.
        let mut exprs: Vec<Option<BoundExpr>> = vec![None; ncols];
        let mut live = cur.columns.iter().enumerate().filter(|(_, c)| !c.dropped).map(|(i, _)| i);
        for te in sel.target_list.iter().filter(|t| !t.junk) {
            let Some(i) = live.next() else { break };
            let detail = match &te.expr.kind {
                ExprKind::Column(c) if c.levels_up == 0 && c.rt_index == from_rt => {
                    if c.attnum > 0 {
                        level.col_map[i] = Some(c.attnum);
                        None
                    } else if c.attnum == 0 {
                        Some("View columns that return whole-row references are not updatable.")
                    } else {
                        Some("View columns that refer to system columns are not updatable.")
                    }
                }
                _ => Some("View columns that are not columns of their base relation are not updatable."),
            };
            level.col_details[i] = detail;
            if detail.is_none() {
                level.has_updatable_col = true;
            }
            exprs[i] = Some(te.expr.clone());
        }
        level.qual = sel.where_.clone();
        levels.push(level);
        raw_exprs.push((from_rt, exprs));
        match inner {
            Inner::Base(oid) => {
                base = Some(oid);
                break;
            }
            Inner::View(oid) => {
                cur = env
                    .snapshot()
                    .relation(oid)
                    .cloned()
                    .ok_or_else(|| PgError::internal("view analysis: inner view vanished"))?;
            }
        }
    }

    // Compose expressions from the innermost level outwards: substitute each level's references to
    // its FROM entry by the inner level's column expressions (over the base, rt 1).
    let mut inner_cols: Option<Vec<Option<BoundExpr>>> = None;
    for idx in (0..levels.len()).rev() {
        let (from_rt, exprs) = &raw_exprs[idx];
        let from_rt = *from_rt;
        let mut composed: Vec<Option<BoundExpr>> = Vec::with_capacity(exprs.len());
        for e in exprs {
            composed.push(match e {
                Some(e) => subst(e.clone(), from_rt, inner_cols.as_deref()).ok(),
                None => None,
            });
        }
        if let Some(q) = levels[idx].qual.take() {
            levels[idx].qual = Some(subst(q, from_rt, inner_cols.as_deref())?);
        }
        inner_cols = Some(composed);
    }
    let col_exprs = if base.is_some() { inner_cols.unwrap_or_default() } else { vec![None; rel.columns.len()] };
    Ok(ViewInfo { levels, base, col_exprs })
}

/// Rewrite references to range entry `from_rt` (level 0): to `inner[attnum-1]` when the entry is an
/// inner view, else to the base table at rt 1. Sub-queries are opaque to the expression walker, so an
/// expression that needs substitution and contains one is not supported.
fn subst(e: BoundExpr, from_rt: u32, inner: Option<&[Option<BoundExpr>]>) -> Result<BoundExpr> {
    if inner.is_none() && from_rt == 1 {
        return Ok(e);
    }
    if e.contains_sublink() {
        return Err(PgError::unimplemented("SQL-25", "sub-queries in the definition of a nested updatable view"));
    }
    e.transform(&mut |x: BoundExpr| -> Result<BoundExpr> {
        if let ExprKind::Column(c) = &x.kind {
            if c.levels_up == 0 && c.rt_index == from_rt {
                return match inner {
                    None => Ok(BoundExpr::column(0, 1, c.attnum, x.ty)),
                    Some(cols) => {
                        let idx = usize::try_from(c.attnum).ok().and_then(|a| a.checked_sub(1));
                        match idx.and_then(|i| cols.get(i)).cloned().flatten() {
                            Some(r) => Ok(r),
                            None => Err(PgError::unimplemented("SQL-25", "this column of a nested view in a DML statement")),
                        }
                    }
                };
            }
        }
        Ok(x)
    })
}

/// Open view `rel` as the target of a DML statement (module docs). Returns the VIEW-shaped target
/// description and the range-table index of the target entry.
pub fn open_view_target(
    b: &mut Binder,
    rel: &Relation,
    alias: Option<&Ident>,
    kind: DmlKind,
    bind_defaults: bool,
) -> Result<(TargetTable, u32)> {
    let info = analyze_view(b, rel)?;
    let alias_name: String = match alias {
        Some(id) => b.ident(id),
        None => rel.name.clone(),
    };
    let env = b.env;
    let base_rel = info.base.and_then(|oid| env.snapshot().relation(oid));
    let mut entry = match base_rel {
        Some(base) => relation_range_entry(b, base, Some(&alias_name), false)?,
        None => relation_range_entry(b, rel, Some(&alias_name), false)?,
    };
    entry.visible = true;
    entry.access.select = false;
    entry.access.insert = matches!(kind, DmlKind::Insert | DmlKind::Copy);
    entry.access.update = kind == DmlKind::Update;
    entry.access.delete = kind == DmlKind::Delete;
    let rt = add_range_entry(b, entry)?;
    let schema = env.snapshot().namespace(rel.namespace).map(|n| n.name.clone());
    let mut columns: Vec<NsColumn> = Vec::with_capacity(rel.columns.len());
    for (i, c) in rel.columns.iter().enumerate() {
        let expr = match info.col_exprs.get(i).cloned().flatten() {
            Some(e) => shift_rt(e, rt)?,
            None => BoundExpr::null(c.type_id),
        };
        columns.push(NsColumn { name: c.name.clone(), expr, dropped: c.dropped, hidden_from_star: false });
    }
    b.scope_mut()?.namespace.push(NsItem {
        alias: Some(alias_name),
        schema,
        rt_index: Some(rt),
        columns,
        rel_visible: true,
        cols_visible: true,
        lateral_only: false,
        lateral_ok: false,
    });
    let target = build_target_table(b, rel, bind_defaults)?;
    Ok((target, rt))
}

/// Base-table references are bound against rt 1; move them to the statement's target rt.
fn shift_rt(e: BoundExpr, rt: u32) -> Result<BoundExpr> {
    if rt == 1 {
        return Ok(e);
    }
    e.transform(&mut |x: BoundExpr| -> Result<BoundExpr> {
        if let ExprKind::Column(c) = &x.kind {
            if c.levels_up == 0 && c.rt_index == 1 {
                return Ok(BoundExpr::column(0, rt, c.attnum, x.ty));
            }
        }
        Ok(x)
    })
}

fn view_relation<'e>(b: &Binder<'e>, oid: Oid) -> Result<&'e Relation> {
    b.env.snapshot().relation(oid).ok_or_else(|| PgError::internal(format!("view {oid} vanished")))
}

/// PG `error_view_not_updatable`.
fn view_error(kind: DmlKind, merge: bool, view: &str, detail: &str) -> PgError {
    let (msg, hint) = match (kind, merge) {
        (DmlKind::Insert | DmlKind::Copy, false) => (
            format!("cannot insert into view \"{view}\""),
            "To enable inserting into the view, provide an INSTEAD OF INSERT trigger or an unconditional ON INSERT DO INSTEAD rule.",
        ),
        (DmlKind::Update, false) => (
            format!("cannot update view \"{view}\""),
            "To enable updating the view, provide an INSTEAD OF UPDATE trigger or an unconditional ON UPDATE DO INSTEAD rule.",
        ),
        (DmlKind::Delete, false) => (
            format!("cannot delete from view \"{view}\""),
            "To enable deleting from the view, provide an INSTEAD OF DELETE trigger or an unconditional ON DELETE DO INSTEAD rule.",
        ),
        (DmlKind::Insert | DmlKind::Copy, true) => (
            format!("cannot insert into view \"{view}\""),
            "To enable inserting into the view using MERGE, provide an INSTEAD OF INSERT trigger.",
        ),
        (DmlKind::Update, true) => (
            format!("cannot update view \"{view}\""),
            "To enable updating the view using MERGE, provide an INSTEAD OF UPDATE trigger.",
        ),
        (DmlKind::Delete, true) => (
            format!("cannot delete from view \"{view}\""),
            "To enable deleting from the view using MERGE, provide an INSTEAD OF DELETE trigger.",
        ),
    };
    PgError::new(NOT_IN_PREREQUISITE_STATE, msg).with_detail(detail).with_hint(hint)
}

fn column_error(kind: DmlKind, col: &str, view: &str, detail: &str) -> PgError {
    let msg = match kind {
        DmlKind::Insert | DmlKind::Copy => format!("cannot insert into column \"{col}\" of view \"{view}\""),
        DmlKind::Update => format!("cannot update column \"{col}\" of view \"{view}\""),
        DmlKind::Delete => format!("cannot delete from column \"{col}\" of view \"{view}\""),
    };
    PgError::new(SqlState::FEATURE_NOT_SUPPORTED, msg).with_detail(detail)
}

/// The rewriter's checks for a statement of `kind` modifying the OUTER view attnums `modified`.
/// `merge`: the statement is a MERGE (hint wording); then `kinds` lists every action kind.
pub fn check_updatable(info: &ViewInfo, kinds: &[DmlKind], merge: bool, modified: &[i16]) -> Result<()> {
    let check_cols = kinds.iter().any(|k| matches!(k, DmlKind::Insert | DmlKind::Update | DmlKind::Copy));
    let mut cols: Vec<i16> = modified.to_vec();
    for level in &info.levels {
        if let Some(d) = level.stmt_detail {
            let k = kinds.first().copied().unwrap_or(DmlKind::Update);
            return Err(view_error(k, merge, &level.view_name, d));
        }
        if check_cols && !level.has_updatable_col {
            let k = kinds.iter().copied().find(|k| *k != DmlKind::Delete).unwrap_or(DmlKind::Update);
            return Err(view_error(k, merge, &level.view_name, "Views that have no updatable columns are not automatically updatable."));
        }
        let mut next: Vec<i16> = Vec::with_capacity(cols.len());
        for a in &cols {
            let idx = usize::try_from(*a).ok().and_then(|x| x.checked_sub(1)).unwrap_or(usize::MAX);
            if let Some(Some(d)) = level.col_details.get(idx) {
                let name = level.col_names.get(idx).cloned().unwrap_or_default();
                let k = kinds.iter().copied().find(|k| *k != DmlKind::Delete).unwrap_or(DmlKind::Update);
                if merge {
                    return Err(PgError::new(
                        SqlState::FEATURE_NOT_SUPPORTED,
                        format!("cannot merge into column \"{name}\" of view \"{}\"", level.view_name),
                    )
                    .with_detail(*d));
                }
                return Err(column_error(k, &name, &level.view_name, d));
            }
            if let Some(Some(m)) = level.col_map.get(idx) {
                next.push(*m);
            }
        }
        cols = next;
    }
    Ok(())
}

/// The WITH CHECK OPTIONs of the view chain (PG `rewriteTargetView`: innermost first), over rt 1.
pub fn view_wcos(info: &ViewInfo) -> Vec<TargetWco> {
    let mut out: Vec<TargetWco> = Vec::new();
    let mut cascaded = false;
    for level in &info.levels {
        let has = level.check_option.is_some() || cascaded;
        if level.check_option.as_deref() == Some("cascaded") {
            cascaded = true;
        }
        if has && (cascaded || level.qual.is_some()) {
            let expr = level.qual.clone().unwrap_or_else(|| BoundExpr::bool_const(true));
            out.insert(0, TargetWco { kind: WcoKind::ViewCheck, relname: level.view_name.clone(), polname: None, expr });
        }
    }
    out
}

/// AND of the view quals (outermost first), over rt `rt`.
fn view_quals(info: &ViewInfo, rt: u32) -> Result<Vec<BoundExpr>> {
    let mut out = Vec::new();
    for level in &info.levels {
        if let Some(q) = &level.qual {
            out.push(shift_rt(q.clone(), rt)?);
        }
    }
    Ok(out)
}

fn and_all(mut parts: Vec<BoundExpr>) -> Option<BoundExpr> {
    match parts.len() {
        0 => None,
        1 => parts.pop(),
        _ => Some(BoundExpr::new(
            ExprKind::Bool(Box::new(BoolExpr { op: BoolOp::And, args: parts })),
            pgarbor_types::TypeId::BOOL,
        )),
    }
}

/// Base `TargetTable` with the view's column defaults moved onto the base columns.
fn base_target(b: &mut Binder, info: &ViewInfo, view_tt: &TargetTable, bind_defaults: bool) -> Result<TargetTable> {
    let base_oid = info.base.ok_or_else(|| PgError::internal("updatable view without a base table"))?;
    let env = b.env;
    let base = env.snapshot().relation(base_oid).ok_or_else(|| PgError::internal("view base table vanished"))?;
    let mut tt = build_target_table(b, base, bind_defaults)?;
    for vc in &view_tt.columns {
        if vc.dropped {
            continue;
        }
        if let (Some(d), Some(ba)) = (&vc.default, info.base_attnum(vc.attnum)) {
            if let Some(bc) = tt.columns.get_mut((ba as usize).saturating_sub(1)) {
                bc.default = Some(d.clone());
            }
        }
    }
    Ok(tt)
}

fn map_attnum(info: &ViewInfo, a: i16) -> Result<i16> {
    info.base_attnum(a).ok_or_else(|| PgError::internal("view column is not updatable"))
}

/// `SetToDefault{view attnum}` -> `SetToDefault{base attnum}` inside `e`.
fn map_defaults(info: &ViewInfo, e: BoundExpr) -> Result<BoundExpr> {
    e.transform(&mut |x: BoundExpr| -> Result<BoundExpr> {
        if let ExprKind::SetToDefault { attnum } = &x.kind {
            let a = map_attnum(info, *attnum)?;
            return Ok(BoundExpr::new(ExprKind::SetToDefault { attnum: a }, x.ty));
        }
        Ok(x)
    })
}

/// INSERT into a view (module docs).
pub fn rewrite_insert(b: &mut Binder, ins: &mut BoundInsert) -> Result<()> {
    let view = view_relation(b, ins.target.oid)?.clone();
    let info = analyze_view(b, &view)?;
    // Columns the statement writes: supplied ones and omitted ones that have a view default.
    let mut modified: Vec<i16> = ins.columns.clone();
    for c in ins.target.live_columns() {
        if !modified.contains(&c.attnum) && c.default.is_some() {
            modified.push(c.attnum);
        }
    }
    check_updatable(&info, &[DmlKind::Insert], false, &modified)?;
    if ins.on_conflict.is_some() {
        return Err(PgError::unimplemented("SQL-27", "INSERT ... ON CONFLICT on a view"));
    }
    let mut tt = base_target(b, &info, &ins.target, true)?;
    tt.wco = view_wcos(&info);
    let mut cols = Vec::with_capacity(ins.columns.len());
    for a in &ins.columns {
        cols.push(map_attnum(&info, *a)?);
    }
    ins.columns = cols;
    if let InsertSource::Values(rows) = &mut ins.source {
        for row in rows.iter_mut() {
            for cell in row.iter_mut() {
                let e = std::mem::replace(cell, BoundExpr::bool_const(false));
                *cell = map_defaults(&info, e)?;
            }
            // The base table's identity / generated column rules apply to what the view passes down.
            if row.len() == ins.columns.len() {
                for (cell, attnum) in row.iter_mut().zip(ins.columns.iter()) {
                    let e = std::mem::replace(cell, BoundExpr::bool_const(false));
                    *cell = super::insert::bind_value_cell(&tt, *attnum, e, ins.overriding)?;
                }
            }
        }
    }
    ins.target = tt;
    Ok(())
}

fn map_assignments(info: &ViewInfo, assignments: &mut [Assignment]) -> Result<()> {
    for a in assignments.iter_mut() {
        for t in a.targets.iter_mut() {
            t.attnum = map_attnum(info, t.attnum)?;
        }
        let v = std::mem::replace(&mut a.value, BoundExpr::bool_const(false));
        a.value = map_defaults(info, v)?;
    }
    Ok(())
}

/// UPDATE of a view (module docs).
pub fn rewrite_update(b: &mut Binder, upd: &mut BoundUpdate) -> Result<()> {
    let view = view_relation(b, upd.target.oid)?.clone();
    let info = analyze_view(b, &view)?;
    let modified: Vec<i16> = upd.assignments.iter().flat_map(|a| a.targets.iter().map(|t| t.attnum)).collect();
    check_updatable(&info, &[DmlKind::Update], false, &modified)?;
    let mut tt = base_target(b, &info, &upd.target, true)?;
    tt.wco = view_wcos(&info);
    map_assignments(&info, &mut upd.assignments)?;
    let mut parts: Vec<BoundExpr> = upd.where_.take().into_iter().collect();
    parts.extend(view_quals(&info, upd.target_rt)?);
    upd.where_ = and_all(parts);
    upd.target = tt;
    Ok(())
}

/// DELETE from a view (module docs).
pub fn rewrite_delete(b: &mut Binder, del: &mut BoundDelete) -> Result<()> {
    let view = view_relation(b, del.target.oid)?.clone();
    let info = analyze_view(b, &view)?;
    check_updatable(&info, &[DmlKind::Delete], false, &[])?;
    let tt = base_target(b, &info, &del.target, false)?;
    let mut parts: Vec<BoundExpr> = del.where_.take().into_iter().collect();
    parts.extend(view_quals(&info, del.target_rt)?);
    del.where_ = and_all(parts);
    del.target = tt;
    Ok(())
}

/// MERGE into a view (module docs): the view quals restrict the target rows.
pub fn rewrite_merge(b: &mut Binder, m: &mut crate::ir::BoundMerge) -> Result<()> {
    use crate::ir::MergeActionKind;
    let view = view_relation(b, m.target.oid)?.clone();
    let info = analyze_view(b, &view)?;
    let mut kinds: Vec<DmlKind> = Vec::new();
    let mut modified: Vec<i16> = Vec::new();
    for w in &m.whens {
        match &w.action {
            MergeActionKind::Update { assignments } => {
                kinds.push(DmlKind::Update);
                modified.extend(assignments.iter().flat_map(|a| a.targets.iter().map(|t| t.attnum)));
            }
            MergeActionKind::Insert { columns, .. } => {
                kinds.push(DmlKind::Insert);
                modified.extend(columns.iter().copied());
            }
            MergeActionKind::Delete => kinds.push(DmlKind::Delete),
            MergeActionKind::DoNothing => {}
        }
    }
    // PG reports the first action kind whose view support is missing; DELETE needs no columns.
    for level in &info.levels {
        if let Some(d) = level.stmt_detail {
            let k = kinds.first().copied().unwrap_or(DmlKind::Update);
            return Err(view_error(k, true, &level.view_name, d));
        }
    }
    check_updatable(&info, &kinds, true, &modified)?;
    let mut tt = base_target(b, &info, &m.target, true)?;
    tt.wco = view_wcos(&info);
    for w in m.whens.iter_mut() {
        match &mut w.action {
            MergeActionKind::Update { assignments } => map_assignments(&info, assignments)?,
            MergeActionKind::Insert { columns, values, .. } => {
                for a in columns.iter_mut() {
                    *a = map_attnum(&info, *a)?;
                }
                for v in values.iter_mut() {
                    let e = std::mem::replace(v, BoundExpr::bool_const(false));
                    *v = map_defaults(&info, e)?;
                }
            }
            _ => {}
        }
    }
    m.target_quals = view_quals(&info, m.target_rt)?;
    // The pseudo `old`/`new` entries keep the VIEW's column numbering; RETURNING through them is not
    // remapped yet.
    m.target = tt;
    Ok(())
}
