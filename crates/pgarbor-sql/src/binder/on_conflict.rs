//! `INSERT ... ON CONFLICT` (SQL-27). PG references: `parse_clause.c` (`transformOnConflictArbiter`,
//! `resolve_unique_index_expr`, `infer_arbiter_indexes` is planner/`plancat.c`), `analyze.c`
//! (`transformOnConflictClause`), `gram.y` (`opt_on_conflict`).
//!
//! `bind_on_conflict(b, oc, target, columns)` (sqlparser `OnConflict { conflict_target, action }`):
//! * **Arbiter** (`Arbiter`): `ConflictTarget::Columns(idents)` -> find the unique indexes of the target
//!   whose plain key columns EXACTLY match the inferred columns (order-insensitive) and whose expression
//!   keys EXACTLY match the inferred expressions (PG `infer_arbiter_indexes`), non-partial, immediate,
//!   valid (use `env.snapshot().indexes_of(table)`); none -> 42P10 `there is no unique or
//!   exclusion constraint matching the ON CONFLICT specification`. An expression element
//!   (`ON CONFLICT (lower(a))`) reaches this module as a [`CONFLICT_EXPR_MARKER`] identifier carrying its
//!   source text (the parser rewrites it, sqlparser 0.63 takes identifiers only); it is parsed, bound in
//!   the target's scope and compared with the stored key expressions. A COLLATE element is 0A000 SQL-27.
//!   `WHERE index_predicate` (not representable in sqlparser 0.63's `ConflictTarget::Columns`) reaches
//!   this module as a trailing [`CONFLICT_WHERE_MARKER`] identifier; a partial index qualifies only when
//!   its bound predicate equals the written one (PG uses implication). `ConflictTarget::OnConstraint(name)` ->
//!   the named unique/PK constraint's backing index (42704 `constraint "x" for table "t" does not
//!   exist`, oracle; a CHECK constraint name: 42809 `constraint in ON CONFLICT clause has no associated
//!   index`). No target: only legal with `DO NOTHING`
//!   (`Arbiter { indexes: [], any_unique: true }`); with DO UPDATE: 42601 `ON CONFLICT DO UPDATE
//!   requires inference specification or constraint name` HINT `For example, ON CONFLICT (column_name).`
//! * **DO NOTHING** -> `ConflictAction::DoNothing`.
//! * **DO UPDATE SET ... [WHERE]**: register the pseudo relation `excluded` (columns = target
//!   columns, `PseudoRelation::Excluded`, same attnums, `access.select`) as range entry 2 of the INSERT
//!   scope and make both the target (by its alias/name) and `excluded` visible. Assignments are bound by
//!   `update_delete::bind_assignments` against the target (`excluded.c` and `t.c` references allowed in
//!   values and WHERE; the SET targets are unqualified column names of the target). `DoUpdate.selection`
//!   -> `coerce_to_boolean("WHERE")`. A column assigned twice: 42601 `multiple assignments to same
//!   column "x"`. Generated columns / identity ALWAYS as SET targets: 428C9 as for UPDATE.
//! * Result `BoundOnConflict { arbiter, action, excluded_rt }` where `excluded_rt` is the range-table
//!   index of `excluded` (0 for DO NOTHING).
//! * Runtime errors the executor raises (documented here for the bind side): 21000 `ON CONFLICT DO
//!   UPDATE command cannot affect row a second time` HINT `Ensure that no rows proposed for insertion
//!   within the same command have duplicate constrained values.`
use pgarbor_catalog::Index;
use pgarbor_common::{PgError, Result, SqlState};
use sqlparser::ast::{ConflictTarget, Expr, Ident, ObjectName, OnConflict, OnConflictAction};

use super::coerce::coerce_to_boolean;
use super::context::{Binder, ExprCtx};
use super::expr::bind_arg;
use super::from::relation_range_entry;
use super::scope::{add_namespace_for_range, add_range_entry};
use super::stored::{bind_relation_expr, bind_stored_expr};
use super::update_delete::bind_assignments;
use crate::diag;
use crate::ir::{Arbiter, BoundExpr, BoundOnConflict, ConflictAction, ExprKind, PseudoRelation, RangeKind, TargetTable};
use crate::parser::{parse_expr_text, CONFLICT_EXPR_MARKER, CONFLICT_WHERE_MARKER};

/// Range-table index of the DML target in the INSERT scope (`open_target_table` registers the
/// target first, so it is always entry 1).
const TARGET_RT: u32 = 1;

/// Bind the ON CONFLICT clause of an INSERT (module docs). `columns` = target attnums of the INSERT
/// column list. The `excluded` row always carries every target column (the executor lays out
/// `existing ++ excluded` over all attnum slots), so `columns` does not change the binding.
pub fn bind_on_conflict(b: &mut Binder, oc: &OnConflict, target: &TargetTable, columns: &[i16]) -> Result<BoundOnConflict> {
    let _ = columns;
    match &oc.action {
        OnConflictAction::DoNothing => {
            // PG transformOnConflictArbiter: arbiter first, then the (absent) action.
            let arbiter = infer_arbiter(b, oc.conflict_target.as_ref(), target, false)?;
            Ok(BoundOnConflict { arbiter, action: ConflictAction::DoNothing, excluded_rt: 0 })
        }
        OnConflictAction::DoUpdate(du) => {
            // PG transformOnConflictArbiter (parse_clause.c): DO UPDATE needs an inference
            // specification or a constraint name. The clause position is not carried by sqlparser 0.63.
            if oc.conflict_target.is_none() {
                return Err(PgError::syntax(
                    "ON CONFLICT DO UPDATE requires inference specification or constraint name",
                    None,
                )
                .with_hint("For example, ON CONFLICT (column_name)."));
            }
            let arbiter = infer_arbiter(b, oc.conflict_target.as_ref(), target, true)?;
            let excluded_rt = bind_excluded(b, target)?;
            ensure_target_visible(b)?;
            let assignments = bind_assignments(b, &du.assignments, target, TARGET_RT)?;
            let where_ = match &du.selection {
                Some(e) => {
                    let arg = bind_arg(b, e, ExprCtx::where_())?;
                    Some(coerce_to_boolean(b, &arg, "WHERE")?)
                }
                None => None,
            };
            Ok(BoundOnConflict { arbiter, action: ConflictAction::DoUpdate { assignments, where_ }, excluded_rt })
        }
    }
}

/// PG 42P10 raised by `infer_arbiter_indexes` (plancat.c) when no index matches.
fn no_arbiter_error() -> PgError {
    PgError::new(
        SqlState::INVALID_COLUMN_REFERENCE,
        "there is no unique or exclusion constraint matching the ON CONFLICT specification",
    )
}

/// Arbiter selection for `ON CONFLICT [target]`. `do_update` only changes the exclusion-constraint
/// check of the named-constraint form.
fn infer_arbiter(b: &mut Binder, spec: Option<&ConflictTarget>, target: &TargetTable, do_update: bool) -> Result<Arbiter> {
    match spec {
        None => Ok(Arbiter { indexes: Vec::new(), any_unique: true, index_predicate: None }),
        Some(ConflictTarget::Columns(idents)) => infer_by_columns(b, idents, target),
        Some(ConflictTarget::OnConstraint(name)) => infer_by_constraint(b, name, target, do_update),
    }
}

/// `ON CONFLICT (c1, (expr), ..)`: every valid unique index whose plain key columns are exactly the
/// inferred columns and whose expression keys are exactly the inferred expressions (PG
/// `infer_arbiter_indexes`). Order-insensitive; repeated elements collapse.
fn infer_by_columns(b: &mut Binder, idents: &[Ident], target: &TargetTable) -> Result<Arbiter> {
    let mut attnums: Vec<i16> = Vec::with_capacity(idents.len());
    let mut exprs: Vec<BoundExpr> = Vec::new();
    let mut predicate: Option<BoundExpr> = None;
    for id in idents {
        if let Some(text) = id.value.strip_prefix(CONFLICT_WHERE_MARKER) {
            predicate = Some(bind_inference_expr(b, text, id, target)?);
            continue;
        }
        if let Some(text) = id.value.strip_prefix(CONFLICT_EXPR_MARKER) {
            let bound = bind_inference_expr(b, text, id, target)?;
            // A parenthesized column (`((c))`) is the plain column, as in PG's parse tree.
            let plain = match &bound.kind {
                ExprKind::Column(c) if c.levels_up == 0 && c.rt_index == 1 && c.attnum >= 1 => Some(c.attnum),
                _ => None,
            };
            match plain {
                Some(attnum) => {
                    if !attnums.contains(&attnum) {
                        attnums.push(attnum);
                    }
                }
                None => {
                    if !exprs.contains(&bound) {
                        exprs.push(bound);
                    }
                }
            }
            continue;
        }
        let name = b.ident(id);
        let pos = b.pos_span(id.span);
        let attnum = match target.live_columns().find(|c| c.name == name) {
            Some(c) => c.attnum,
            None => return Err(diag::undefined_column(None, &name, pos)),
        };
        if !attnums.contains(&attnum) {
            attnums.push(attnum);
        }
    }
    attnums.sort_unstable();

    let env = b.env;
    let mut matched: Vec<&Index> = Vec::new();
    for idx in env.snapshot().indexes_of(target.oid) {
        match inference_candidate_columns(idx, predicate.is_some()) {
            Some(cols) if cols == attnums => {}
            _ => continue,
        }
        if let (Some(stored), Some(want)) = (&idx.predicate, &predicate) {
            // A partial index arbitrates only when the written predicate is the index predicate.
            let have = bind_stored_expr(b, &stored.sql, target.oid, ExprCtx::index_expr())?;
            if &have != want {
                continue;
            }
        }
        if index_expressions_match(b, idx, target, &exprs)? {
            matched.push(idx);
        }
    }
    if matched.is_empty() {
        return Err(no_arbiter_error());
    }
    check_arbiters_immediate(&matched, target)?;
    Ok(Arbiter { indexes: matched.iter().map(|i| i.oid).collect(), any_unique: false, index_predicate: None })
}

/// `ON CONSTRAINT name`: the index backing the named constraint of the target (PG
/// `get_relation_constraint_oid` + `get_constraint_index`).
fn infer_by_constraint(b: &mut Binder, name: &ObjectName, target: &TargetTable, do_update: bool) -> Result<Arbiter> {
    let pos = b.pos(name);
    let qn = b.qual(name)?;
    if qn.catalog.is_some() || qn.schema.is_some() {
        // The grammar takes a single `name` here, so a qualified name is a syntax error at the dot.
        return Err(PgError::syntax("syntax error at or near \".\"", pos));
    }

    let env = b.env;
    let snap = env.snapshot();
    let constraint = match snap.constraints_of(target.oid).into_iter().find(|c| c.name == qn.name) {
        Some(c) => c,
        None => {
            return Err(PgError::new(
                SqlState::UNDEFINED_OBJECT,
                format!("constraint \"{}\" for table \"{}\" does not exist", qn.name, target.name),
            ))
        }
    };
    if constraint.index == 0 {
        return Err(PgError::new(
            SqlState::WRONG_OBJECT_TYPE,
            "constraint in ON CONFLICT clause has no associated index",
        ));
    }
    let idx = match snap.index(constraint.index) {
        Some(i) if i.valid => i,
        _ => return Err(no_arbiter_error()),
    };
    if idx.exclusion && do_update {
        return Err(PgError::new(
            SqlState::WRONG_OBJECT_TYPE,
            "ON CONFLICT DO UPDATE not supported with exclusion constraints",
        ));
    }
    check_arbiters_immediate(&[idx], target)?;
    Ok(Arbiter { indexes: vec![idx.oid], any_unique: false, index_predicate: None })
}

/// Deferrable unique/exclusion indexes cannot arbitrate: PG raises this from the executor
/// (`execIndexing.c`, 55000) on the first arbiter probe; raised here at bind time instead.
fn check_arbiters_immediate(matched: &[&Index], target: &TargetTable) -> Result<()> {
    for idx in matched {
        if !idx.immediate {
            return Err(PgError::new(
                SqlState::OBJECT_NOT_IN_PREREQUISITE_STATE,
                "ON CONFLICT does not support deferrable unique constraints/exclusion constraints as arbiters",
            )
            .with_table(target.schema.clone(), target.name.clone())
            .with_constraint(idx.name.clone()));
        }
    }
    Ok(())
}

/// Sorted attnum set of the plain key columns of an index usable for `ON CONFLICT (..)` inference, or
/// None. Expression keys are matched by [`index_expressions_match`].
fn inference_candidate_columns(idx: &Index, allow_partial: bool) -> Option<Vec<i16>> {
    let n = idx.n_key_columns as usize;
    let keys: Vec<Option<i16>> = idx.keys.iter().take(n).map(|k| k.column).collect();
    candidate_key_set(idx.unique, idx.exclusion, idx.valid, idx.predicate.is_some() && !allow_partial, &keys)
}

/// Core of [`inference_candidate_columns`]: only unique, non-exclusion, valid, non-partial indexes
/// qualify (PG plancat.c `infer_arbiter_indexes`). `None` entries are expression keys.
fn candidate_key_set(unique: bool, exclusion: bool, valid: bool, partial: bool, keys: &[Option<i16>]) -> Option<Vec<i16>> {
    if !unique || exclusion || !valid || partial || keys.is_empty() {
        return None;
    }
    let mut cols: Vec<i16> = keys.iter().flatten().copied().collect();
    cols.sort_unstable();
    cols.dedup();
    Some(cols)
}

/// The expression part of PG `infer_arbiter_indexes`: the expression keys of `idx` and the inference
/// expressions must be the same set (every inference expression is a cataloged index expression and no
/// cataloged expression is left unmatched). Stored key text is bound in the target's scope.
fn index_expressions_match(b: &mut Binder, idx: &Index, target: &TargetTable, exprs: &[BoundExpr]) -> Result<bool> {
    let mut keys: Vec<BoundExpr> = Vec::new();
    for key in idx.key_columns() {
        if let Some(stored) = &key.expr {
            keys.push(bind_stored_expr(b, &stored.sql, target.oid, ExprCtx::index_expr())?);
        }
    }
    Ok(keys.iter().all(|k| exprs.contains(k)) && exprs.iter().all(|e| keys.contains(e)))
}

/// Bind one expression element of an inference list. `text` is its source (see
/// [`CONFLICT_EXPR_MARKER`]). A `COLLATE` clause is not matched against index collations yet (0A000
/// SQL-27). Errors are positioned in the query, see [`rebase_position`].
fn bind_inference_expr(b: &mut Binder, text: &str, id: &Ident, target: &TargetTable) -> Result<BoundExpr> {
    let start = b.pos_span(id.span);
    let ast: Expr = parse_expr_text(text)?;
    if let Expr::Collate { .. } = ast {
        let err = PgError::unimplemented("SQL-27", "ON CONFLICT inference element with a COLLATE clause");
        return Err(match start {
            Some(pos) => err.with_position(pos),
            None => err,
        });
    }
    bind_relation_expr(b, &ast, target.oid, ExprCtx::index_expr()).map_err(|e| rebase_position(e, start))
}

/// The element text was parsed on its own, so the positions inside it count from 1 at its start. Move
/// them to the query: `start` is the element's position in the query (an error without a position is
/// reported at the element).
fn rebase_position(e: PgError, start: Option<u32>) -> PgError {
    match (start, e.position) {
        (Some(s), Some(p)) => e.with_position(s.saturating_add(p.saturating_sub(1))),
        (Some(s), None) => e.with_position(s),
        _ => e,
    }
}

/// Register `excluded` (PG `addRangeTableEntry` of the target relation under the alias
/// `excluded`, flagged as the proposed row) as a new range-table entry and namespace item.
fn bind_excluded(b: &mut Binder, target: &TargetTable) -> Result<u32> {
    let env = b.env;
    let rel = match env.snapshot().relation(target.oid) {
        Some(r) => r,
        None => {
            return Err(PgError::internal(format!(
                "relation with oid {} is missing from the catalog snapshot",
                target.oid
            )))
        }
    };
    let mut entry = relation_range_entry(b, rel, Some("excluded"), false)?;
    if let RangeKind::Relation { pseudo, .. } = &mut entry.kind {
        *pseudo = Some(PseudoRelation::Excluded);
    }
    let rt = add_range_entry(b, entry)?;
    add_namespace_for_range(b, rt, true, true)?;
    Ok(rt)
}

/// Make sure the target (rt 1) is visible in the namespace for the SET and WHERE expressions.
fn ensure_target_visible(b: &mut Binder) -> Result<()> {
    let visible = b.scope()?.namespace.iter().any(|ns| ns.rt_index == Some(TARGET_RT));
    if !visible {
        add_namespace_for_range(b, TARGET_RT, true, true)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn candidate_requires_unique_valid_total_non_partial() {
        assert_eq!(candidate_key_set(false, false, true, false, &[Some(1)]), None);
        assert_eq!(candidate_key_set(true, true, true, false, &[Some(1)]), None);
        assert_eq!(candidate_key_set(true, false, false, false, &[Some(1)]), None);
        assert_eq!(candidate_key_set(true, false, true, true, &[Some(1)]), None);
        assert_eq!(candidate_key_set(true, false, true, false, &[]), None);
    }

    #[test]
    fn expression_keys_leave_the_plain_set_alone() {
        assert_eq!(candidate_key_set(true, false, true, false, &[Some(1), None]), Some(vec![1]));
        assert_eq!(candidate_key_set(true, false, true, false, &[None]), Some(Vec::new()));
    }

    #[test]
    fn inference_element_positions_move_into_the_query() {
        let e = PgError::unimplemented("SQL-27", "x").with_position(3);
        assert_eq!(rebase_position(e, Some(10)).position, Some(12));
        let e = PgError::unimplemented("SQL-27", "x");
        assert_eq!(rebase_position(e, Some(10)).position, Some(10));
        let e = PgError::unimplemented("SQL-27", "x").with_position(3);
        assert_eq!(rebase_position(e, None).position, Some(3));
    }

    #[test]
    fn candidate_set_is_sorted_and_deduplicated() {
        assert_eq!(candidate_key_set(true, false, true, false, &[Some(3), Some(1), Some(3)]), Some(vec![1, 3]));
        assert_eq!(candidate_key_set(true, false, true, false, &[Some(2)]), Some(vec![2]));
    }

    #[test]
    fn no_arbiter_error_is_42p10_with_pg_text() {
        let e = no_arbiter_error();
        assert!(e.sqlstate == SqlState::INVALID_COLUMN_REFERENCE);
        assert_eq!(
            e.message,
            "there is no unique or exclusion constraint matching the ON CONFLICT specification"
        );
        assert!(e.position.is_none());
    }

    #[test]
    fn do_update_without_target_hint_text() {
        let e = PgError::syntax(
            "ON CONFLICT DO UPDATE requires inference specification or constraint name",
            None,
        )
        .with_hint("For example, ON CONFLICT (column_name).");
        assert!(e.sqlstate == SqlState::SYNTAX_ERROR);
        assert_eq!(e.hint.as_deref(), Some("For example, ON CONFLICT (column_name)."));
    }
}
