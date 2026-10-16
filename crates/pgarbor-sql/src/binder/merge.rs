//! MERGE (amendment pgarbor-sql-rewrite-rls-merge). PG references: `parse_merge.c` (`transformMergeStmt`,
//! `setNamespaceForMergeWhen`), `gram.y` (`MergeStmt`, `merge_when_clause`), `parse_expr.c`
//! (`transformMergeSupportFunc`).
//!
//! Binding order follows PG: WHEN clause sanity (unreachable clauses), the target (range entry 1), the
//! source FROM item, the join condition (target visible again), RETURNING (with the `old`/`new` pseudo
//! relations and `merge_action()`), then every WHEN clause with the PG namespace rules:
//! `MATCHED` sees target and source, `NOT MATCHED BY SOURCE` only the target, `NOT MATCHED [BY TARGET]`
//! only the source (references to the hidden one are 42P01 "invalid reference to FROM-clause entry").
use pgarbor_catalog::model::RelKind;
use pgarbor_common::{PgError, Result, SqlState};
use pgarbor_types::TypeId;
use sqlparser::ast::{
    Expr, Function, FunctionArguments, Merge, MergeAction, MergeClause, MergeClauseKind, MergeInsertKind,
    MergeUpdateKind, OutputClause, TableFactor, TableWithJoins,
};

use super::context::{Binder, ExprCtx};
use super::expr::bind_boolean;
use super::from::bind_from_clause;
use super::insert;
use super::scope::{QueryScope, ScopeKind};
use super::target_table::{open_target_table, DmlKind};
use super::update_delete::{bind_assignments, bind_returning, plain_table_target};
use super::values::{self, InsertTargetCol};
use crate::diag::{self, ExprClause};
use crate::ir::{
    AccessNeeded, BoundExpr, BoundMerge, BoundMergeWhen, BoundStatement, FromItem, MergeActionKind, MergeMatchKind,
    RangeColumn, RangeEntry, RangeKind, TargetTable,
};

/// `Binder::scratch` key holding `"<scope index>:<action rt>"` while a MERGE RETURNING list is bound.
const MERGE_ACTION_KEY: &str = "merge action";

/// Bind a MERGE statement (module docs).
pub fn bind_merge(b: &mut Binder, m: &Merge) -> Result<BoundStatement> {
    if !m.optimizer_hints.is_empty() {
        return Err(diag::syntax_error_near("/*+", b.pos_span(m.merge_token.0.span)));
    }
    if matches!(m.output, Some(OutputClause::Output { .. })) {
        return Err(diag::syntax_error_near("OUTPUT", None));
    }
    check_clauses(b, &m.clauses)?;
    b.scopes.push(QueryScope::new(ScopeKind::Update));
    let result = bind_merge_in_scope(b, m);
    let saved = b.scopes.pop();
    b.scratch.remove(MERGE_ACTION_KEY);
    let mut merge = result?;
    let scope = saved.ok_or_else(|| PgError::internal("binder: MERGE scope missing"))?;
    merge.range_table = scope.range_table;
    if merge.target.kind == RelKind::View {
        super::view_dml::rewrite_merge(b, &mut merge)?;
    }
    Ok(BoundStatement::Merge(Box::new(merge)))
}

/// PG grammar and `transformMergeStmt` sanity checks on the WHEN list.
fn check_clauses(b: &Binder, clauses: &[MergeClause]) -> Result<()> {
    let mut terminal = [false; 3];
    for c in clauses {
        let k = match_kind(c.clause_kind);
        let pos = b.pos_span(c.when_token.0.span);
        // gram.y: INSERT only in NOT MATCHED [BY TARGET], UPDATE/DELETE only in the other two kinds.
        match (&c.action, k) {
            (MergeAction::Insert(i), MergeMatchKind::Matched | MergeMatchKind::NotMatchedBySource) => {
                return Err(diag::syntax_error_near("INSERT", b.pos_span(i.insert_token.0.span)));
            }
            (MergeAction::Update(u), MergeMatchKind::NotMatchedByTarget) => {
                return Err(diag::syntax_error_near("UPDATE", b.pos_span(u.update_token.0.span)));
            }
            (MergeAction::Delete { delete_token }, MergeMatchKind::NotMatchedByTarget) => {
                return Err(diag::syntax_error_near("DELETE", b.pos_span(delete_token.0.span)));
            }
            _ => {}
        }
        let slot = k as usize;
        if terminal[slot] {
            let _ = pos;
            return Err(PgError::new(
                SqlState::SYNTAX_ERROR,
                "unreachable WHEN clause specified after unconditional WHEN clause",
            ));
        }
        if c.predicate.is_none() {
            terminal[slot] = true;
        }
    }
    Ok(())
}

fn match_kind(k: MergeClauseKind) -> MergeMatchKind {
    match k {
        MergeClauseKind::Matched => MergeMatchKind::Matched,
        MergeClauseKind::NotMatchedBySource => MergeMatchKind::NotMatchedBySource,
        MergeClauseKind::NotMatched | MergeClauseKind::NotMatchedByTarget => MergeMatchKind::NotMatchedByTarget,
    }
}

/// PG `errdetail_relkind_not_supported` wording for a MERGE target of the wrong kind.
fn relkind_detail(kind: RelKind) -> &'static str {
    match kind {
        RelKind::MatView => "This operation is not supported for materialized views.",
        RelKind::Sequence => "This operation is not supported for sequences.",
        RelKind::Index | RelKind::PartitionedIndex => "This operation is not supported for indexes.",
        RelKind::CompositeType => "This operation is not supported for composite types.",
        RelKind::ForeignTable => "This operation is not supported for foreign tables.",
        RelKind::Toast => "This operation is not supported for TOAST tables.",
        _ => "This operation is not supported for this relation kind.",
    }
}

fn bind_merge_in_scope(b: &mut Binder, m: &Merge) -> Result<BoundMerge> {
    let (name, alias) = match &m.table {
        TableFactor::Table { name, alias, args: None, .. } => (name, alias.as_ref().map(|a| &a.name)),
        other => plain_table_target(other)?,
    };
    // Kind check (PG: the target must be a table or a view).
    let qn = b.qual(name)?;
    let env = b.env;
    if let Ok(rel) = env.resolve_relation(qn.schema.as_deref(), &qn.name) {
        if !rel.is_virtual() && !matches!(rel.kind, RelKind::Table | RelKind::PartitionedTable | RelKind::View) {
            return Err(PgError::new(
                SqlState::FEATURE_NOT_SUPPORTED,
                format!("cannot execute MERGE on relation \"{}\"", rel.name),
            )
            .with_detail(relkind_detail(rel.kind)));
        }
    }

    let has = |f: fn(&MergeAction) -> bool| m.clauses.iter().any(|c| f(&c.action));
    let wants_insert = has(|a| matches!(a, MergeAction::Insert(_)));
    let wants_update = has(|a| matches!(a, MergeAction::Update(_)));
    let wants_delete = has(|a| matches!(a, MergeAction::Delete { .. }));
    let wants_nothing = has(|a| matches!(a, MergeAction::DoNothing { .. }));

    let (target, target_rt) = open_target_table(b, name, alias, DmlKind::Update, true)?;
    {
        let entry = entry_mut(b, target_rt)?;
        entry.access.update = wants_update;
        entry.access.insert = wants_insert;
        entry.access.delete = wants_delete;
        entry.access.select = wants_nothing;
    }
    let target_alias = entry_mut(b, target_rt)?.alias.clone();
    // The source cannot see the target; the PG duplicate-name check below replaces checkNameSpaceConflicts.
    set_visibility(b, target_rt, false, false)?;
    let source_items = bind_from_clause(b, &[TableWithJoins { relation: m.source.clone(), joins: Vec::new() }])?;
    let source = match source_items.into_iter().next() {
        Some(item) => item,
        None => return Err(PgError::internal("binder: MERGE source produced no FROM item")),
    };
    if let FromItem::Range(src_rt) = &source {
        let src_alias = entry_mut(b, *src_rt)?.alias.clone();
        if src_alias == target_alias {
            return Err(PgError::new(
                SqlState::DUPLICATE_ALIAS,
                format!("name \"{target_alias}\" specified more than once"),
            )
            .with_detail("The name is used both as MERGE target table and data source."));
        }
    }
    set_visibility(b, target_rt, true, true)?;
    let source_rts = source_rt_indexes(&source);

    let join_condition = bind_boolean(b, &m.on, ExprCtx::join_on(), "JOIN/ON")?;

    // RETURNING: pseudo relations `old` / `new` and `merge_action()`.
    let action_rt = add_pseudo_entry(b, "*MERGE ACTION*", vec![RangeColumn { name: "merge_action".into(), ty: TypeId::TEXT, dropped: false }])?;
    let target_cols: Vec<RangeColumn> = target
        .columns
        .iter()
        .map(|c| RangeColumn { name: c.name.clone(), ty: c.ty, dropped: c.dropped })
        .collect();
    let old_rt = add_pseudo_entry(b, "old", target_cols.clone())?;
    let new_rt = add_pseudo_entry(b, "new", target_cols)?;
    let returning = match &m.output {
        Some(OutputClause::Returning { select_items, .. }) => {
            add_pseudo_namespace(b, old_rt)?;
            add_pseudo_namespace(b, new_rt)?;
            let level = b.scopes.len().saturating_sub(1);
            b.scratch.insert(MERGE_ACTION_KEY, format!("{level}:{action_rt}"));
            let r = bind_returning(b, Some(select_items.as_slice()));
            b.scratch.remove(MERGE_ACTION_KEY);
            remove_namespace(b, old_rt)?;
            remove_namespace(b, new_rt)?;
            r?
        }
        _ => Vec::new(),
    };

    let mut whens: Vec<BoundMergeWhen> = Vec::with_capacity(m.clauses.len());
    for c in &m.clauses {
        let kind = match_kind(c.clause_kind);
        let (target_vis, source_vis) = match kind {
            MergeMatchKind::Matched => (true, true),
            MergeMatchKind::NotMatchedBySource => (true, false),
            MergeMatchKind::NotMatchedByTarget => (false, true),
        };
        set_visibility(b, target_rt, target_vis, target_vis)?;
        for rt in &source_rts {
            set_visibility(b, *rt, source_vis, source_vis)?;
        }
        let condition = match &c.predicate {
            Some(p) => {
                let ec = ExprCtx { clause: ExprClause::Other("MERGE WHEN conditions"), ..ExprCtx::where_() };
                Some(bind_boolean(b, p, ec, "WHEN")?)
            }
            None => None,
        };
        let action = bind_action(b, &c.action, &target, target_rt)?;
        whens.push(BoundMergeWhen { match_kind: kind, condition, action });
    }
    set_visibility(b, target_rt, true, true)?;
    for rt in &source_rts {
        set_visibility(b, *rt, true, true)?;
    }

    Ok(BoundMerge {
        target,
        target_rt,
        range_table: Vec::new(),
        ctes: Vec::new(),
        source,
        join_condition,
        target_quals: Vec::new(),
        whens,
        returning,
        action_rt,
        old_rt,
        new_rt,
    })
}

fn bind_action(b: &mut Binder, action: &MergeAction, target: &TargetTable, target_rt: u32) -> Result<MergeActionKind> {
    match action {
        MergeAction::DoNothing { .. } => Ok(MergeActionKind::DoNothing),
        MergeAction::Delete { .. } => Ok(MergeActionKind::Delete),
        MergeAction::Update(u) => {
            if u.update_predicate.is_some() {
                return Err(diag::syntax_error_near("WHERE", None));
            }
            if u.delete_predicate.is_some() {
                return Err(diag::syntax_error_near("DELETE", None));
            }
            let assignments = match &u.kind {
                MergeUpdateKind::Set(a) => a,
                MergeUpdateKind::Wildcard => return Err(diag::syntax_error_near("*", None)),
            };
            let assignments = bind_assignments(b, assignments, target, target_rt)?;
            Ok(MergeActionKind::Update { assignments })
        }
        MergeAction::Insert(ins) => {
            if ins.insert_predicate.is_some() {
                return Err(diag::syntax_error_near("WHERE", None));
            }
            let vals = match &ins.kind {
                MergeInsertKind::Values(v) => v,
                MergeInsertKind::Row => return Err(diag::syntax_error_near("ROW", b.pos_span(ins.kind_token.0.span))),
                MergeInsertKind::Wildcard => return Err(diag::syntax_error_near("*", b.pos_span(ins.kind_token.0.span))),
            };
            if vals.rows.len() > 1 {
                return Err(diag::syntax_error_near(",", None));
            }
            let overriding = b.overriding;
            let explicit = !ins.columns.is_empty();
            let cols: Vec<(i16, Option<u32>)> = if explicit {
                insert::resolve_column_list(b, &ins.columns, target)?
            } else {
                insert::implicit_column_list(target).into_iter().map(|a| (a, None)).collect()
            };
            let row_len = vals.rows.first().map(|r| r.content.len()).unwrap_or(0);
            let used: &[(i16, Option<u32>)] = if !explicit && row_len < cols.len() { &cols[..row_len] } else { &cols };
            let attnums: Vec<i16> = used.iter().map(|(a, _)| *a).collect();
            let mut targets: Vec<InsertTargetCol> = Vec::with_capacity(attnums.len());
            for &attnum in &attnums {
                let col = insert::col_of(target, attnum)?;
                targets.push(InsertTargetCol { attnum, name: col.name.clone(), ty: col.ty });
            }
            let bound = values::bind_values_body(b, vals, Some(targets.as_slice()))?;
            let row = bound.rows.into_iter().next().unwrap_or_default();
            let mut out: Vec<BoundExpr> = Vec::with_capacity(row.len());
            for (cell, &attnum) in row.into_iter().zip(attnums.iter()) {
                out.push(insert::bind_value_cell(target, attnum, cell, overriding)?);
            }
            insert::mark_insert_columns(b, target_rt, &attnums)?;
            Ok(MergeActionKind::Insert { columns: attnums, values: out, overriding })
        }
    }
}

/// `merge_action()` (PG `transformMergeSupportFunc`): only inside the RETURNING list of a MERGE (any
/// sub-query depth); it reads the pseudo column `Column{action_rt, 1}` of the MERGE level.
pub fn bind_merge_action(b: &mut Binder, f: &Function, pos: Option<u32>) -> Result<BoundExpr> {
    if let FunctionArguments::List(list) = &f.args {
        if let Some(first) = list.args.first() {
            let arg_pos = match first {
                sqlparser::ast::FunctionArg::Unnamed(sqlparser::ast::FunctionArgExpr::Expr(e)) => b.pos(e),
                _ => pos,
            };
            let word = match first {
                sqlparser::ast::FunctionArg::Unnamed(sqlparser::ast::FunctionArgExpr::Expr(e)) => first_word(e),
                _ => None,
            };
            return Err(diag::syntax_error_near(word.as_deref().unwrap_or(")"), arg_pos));
        }
    }
    let info = b.scratch.get(MERGE_ACTION_KEY).cloned();
    let Some(info) = info else {
        return Err(diag::with_position(
            PgError::new(
                SqlState::SYNTAX_ERROR,
                "MERGE_ACTION() can only be used in the RETURNING list of a MERGE command",
            ),
            pos,
        ));
    };
    let mut parts = info.split(':');
    let level: usize = parts.next().and_then(|s| s.parse().ok()).ok_or_else(|| PgError::internal("merge action scope"))?;
    let rt: u32 = parts.next().and_then(|s| s.parse().ok()).ok_or_else(|| PgError::internal("merge action rt"))?;
    let depth = b.scopes.len().saturating_sub(1);
    if level > depth {
        return Err(PgError::internal("merge_action(): MERGE scope is gone"));
    }
    let levels_up = (depth - level) as u32;
    Ok(BoundExpr::column(levels_up, rt, 1, TypeId::TEXT))
}

/// The leading word of an argument expression (for the syntax error of `merge_action(x)`).
fn first_word(e: &Expr) -> Option<String> {
    match e {
        Expr::Function(f) => f.name.0.first().map(|p| p.to_string()),
        Expr::Identifier(id) => Some(id.value.clone()),
        Expr::CompoundIdentifier(ids) => ids.first().map(|id| id.value.clone()),
        _ => None,
    }
}

fn entry_mut<'b>(b: &'b mut Binder, rt: u32) -> Result<&'b mut RangeEntry> {
    let idx = (rt as usize).checked_sub(1).ok_or_else(|| PgError::internal("MERGE: range-table index 0"))?;
    b.scope_mut()?
        .range_table
        .get_mut(idx)
        .ok_or_else(|| PgError::internal(format!("MERGE: no range-table entry {rt}")))
}

fn set_visibility(b: &mut Binder, rt: u32, rel_visible: bool, cols_visible: bool) -> Result<()> {
    for item in b.scope_mut()?.namespace.iter_mut().filter(|ns| ns.rt_index == Some(rt)) {
        item.rel_visible = rel_visible;
        item.cols_visible = cols_visible;
    }
    Ok(())
}

/// Range-table indexes of every leaf under a FROM item (the source of a MERGE may be a join).
fn source_rt_indexes(item: &FromItem) -> Vec<u32> {
    match item {
        FromItem::Range(rt) => vec![*rt],
        FromItem::Join(j) => {
            let mut v = source_rt_indexes(&j.left);
            v.extend(source_rt_indexes(&j.right));
            v
        }
    }
}

/// A never-scanned range entry backing RETURNING-only references (module docs).
fn add_pseudo_entry(b: &mut Binder, alias: &str, columns: Vec<RangeColumn>) -> Result<u32> {
    let types = columns.iter().map(|c| c.ty).collect();
    let entry = RangeEntry {
        kind: RangeKind::Values { values: crate::ir::BoundValues { rows: Vec::new(), types } },
        alias: alias.to_string(),
        columns,
        lateral: false,
        visible: false,
        access: AccessNeeded::default(),
    };
    super::scope::add_range_entry(b, entry)
}

/// `old.c` / `new.c` are addressable only qualified (PG 18 `returningOldAlias`/`returningNewAlias`).
fn add_pseudo_namespace(b: &mut Binder, rt: u32) -> Result<()> {
    super::scope::add_namespace_for_range(b, rt, true, false)
}

fn remove_namespace(b: &mut Binder, rt: u32) -> Result<()> {
    b.scope_mut()?.namespace.retain(|ns| ns.rt_index != Some(rt));
    Ok(())
}
