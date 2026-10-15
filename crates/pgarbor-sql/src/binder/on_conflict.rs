//! `INSERT ... ON CONFLICT` (SQL-27). PG references: `parse_clause.c` (`transformOnConflictArbiter`,
//! `resolve_unique_index_expr`, `infer_arbiter_indexes` is planner/`plancat.c`), `analyze.c`
//! (`transformOnConflictClause`), `gram.y` (`opt_on_conflict`).
//!
//! `bind_on_conflict(b, oc, target, columns)` (sqlparser `OnConflict { conflict_target, action }`):
//! * **Arbiter** (`Arbiter`): `ConflictTarget::Columns(idents)` -> find the unique indexes of the target
//!   whose key columns EXACTLY match the set of inferred columns (order-insensitive), non-partial, non-
//!   deferrable, valid (use `env.snapshot().indexes_of(table)`); none -> 42P10 `there is no unique or
//!   exclusion constraint matching the ON CONFLICT specification`. Expression inference elements
//!   (`ON CONFLICT ((lower(a)))`) and `WHERE index_predicate` are not representable in sqlparser 0.63's
//!   `ConflictTarget::Columns(Vec<Ident>)`; document as a gap. `ConflictTarget::OnConstraint(name)` ->
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
use sqlparser::ast::{ConflictTarget, Ident, ObjectName, OnConflict, OnConflictAction};

use super::coerce::coerce_to_boolean;
use super::context::{Binder, ExprCtx};
use super::expr::bind_arg;
use super::from::relation_range_entry;
use super::scope::{add_namespace_for_range, add_range_entry};
use super::update_delete::bind_assignments;
use crate::diag;
use crate::ir::{Arbiter, BoundOnConflict, ConflictAction, PseudoRelation, RangeKind, TargetTable};

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

/// `ON CONFLICT (c1, c2, ..)`: every valid unique index over exactly that column set (order
/// insensitive, repeated columns collapse, as PG's bitmapset comparison does).
fn infer_by_columns(b: &mut Binder, idents: &[Ident], target: &TargetTable) -> Result<Arbiter> {
    let mut attnums: Vec<i16> = Vec::with_capacity(idents.len());
    for id in idents {
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
        match inference_candidate_columns(idx) {
            Some(cols) if cols == attnums => matched.push(idx),
            _ => {}
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

/// Sorted attnum set of an index usable for `ON CONFLICT (cols)` inference, or None.
fn inference_candidate_columns(idx: &Index) -> Option<Vec<i16>> {
    let n = idx.n_key_columns as usize;
    let keys: Vec<Option<i16>> = idx.keys.iter().take(n).map(|k| k.column).collect();
    candidate_key_set(idx.unique, idx.exclusion, idx.valid, idx.predicate.is_some(), &keys)
}

/// Core of [`inference_candidate_columns`]: only unique, non-exclusion, valid, non-partial indexes
/// whose key columns are all plain columns qualify (PG plancat.c `infer_arbiter_indexes`).
fn candidate_key_set(unique: bool, exclusion: bool, valid: bool, partial: bool, keys: &[Option<i16>]) -> Option<Vec<i16>> {
    if !unique || exclusion || !valid || partial || keys.is_empty() {
        return None;
    }
    let mut cols: Vec<i16> = Vec::with_capacity(keys.len());
    for key in keys {
        let attnum = (*key)?;
        if !cols.contains(&attnum) {
            cols.push(attnum);
        }
    }
    cols.sort_unstable();
    Some(cols)
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
        assert_eq!(candidate_key_set(true, false, true, false, &[Some(1), None]), None);
        assert_eq!(candidate_key_set(true, false, true, false, &[]), None);
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
