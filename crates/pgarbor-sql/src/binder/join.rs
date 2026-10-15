//! JOINs: ON / USING / NATURAL, join columns, namespace maintenance (SQL-18, sonnet).
//! PG references: `parse_clause.c` (`transformFromClauseItem` JoinExpr branch,
//! `buildMergedJoinVar`, `buildJoinNSItem`/`transformJoinUsingClause`, `transformJoinOnClause`,
//! `extractRemainingColumns`, `setNamespaceLateralState`), `parse_relation.c`
//! (`addRangeTableEntryForJoin`).
//!
//! `bind_join(b, left, join)` is called by `from::bind_from_clause` once per `Join` of a
//! `TableWithJoins`, with `left` = the item built so far.
//! 1. Bind the right factor with `from::bind_table_factor(b, &join.relation, lateral_ok)`.
//!    LATERAL/`CROSS APPLY` on the right may see the left side; a plain right side must not
//!    (namespace lateral flags; RIGHT/FULL JOIN with a LATERAL reference to the left side: 0A000?
//!    PG: 42601 `invalid reference to FROM-clause entry for table "x"` DETAIL `The combining JOIN type
//!    must be INNER or LEFT for a LATERAL reference.`).
//! 2. Map `JoinOperator`: `Join`/`Inner` -> Inner; `Left`/`LeftOuter` -> Left; `Right`/`RightOuter` ->
//!    Right; `FullOuter` -> Full; `CrossJoin` -> Inner without quals (constraint must be `None`);
//!    `Semi/LeftSemi/RightSemi/Anti/LeftAnti/RightAnti/CrossApply/OuterApply/AsOf/StraightJoin` -> 42601
//!    (not PostgreSQL syntax).
//! 3. Constraint (`JoinConstraint`):
//!    * `On(expr)`: bind with `ExprCtx::join_on()` in the namespace of left+right (both
//!      `cols_visible`, nothing else of this FROM list), `coerce_to_boolean("JOIN/ON")`.
//!    * `Using(cols)`: each name must exist exactly once on both sides (42703 `column "c" specified in
//!      USING clause does not exist in left table` / `right table` via `diag::using_column_missing`;
//!      42702 `common column name "c" appears more than once in left table` via
//!      `diag::using_column_ambiguous`); quals = AND of `l.c = r.c` (operator resolved on the two
//!      column types; types coerced to the common type, the merged column has the common type);
//!      duplicate names in the USING list: 42701 `column name "c" appears more than once in USING
//!      clause`.
//!    * `Natural`: USING with all common column names (none common: becomes a cross join).
//!    * `None` (plain `JOIN` without ON): 42601 `syntax error at or near ...` (PG grammar requires ON/USING);
//!      `CROSS JOIN` has no constraint.
//! 4. Merged columns for USING/NATURAL: output column list = merged columns (in USING order) then
//!    remaining left columns, then remaining right columns. The merged column's replacement
//!    expression: INNER/LEFT -> the left column; RIGHT -> the right column; FULL ->
//!    `COALESCE(left, right)` (coerced to the common type). Names in the result namespace come from the
//!    left side. `SELECT *` over the join lists the merged column once; `t.*` still lists the table's
//!    own columns (`hidden_from_star` handles the duplicates).
//! 5. Namespace: both input items get `cols_visible = false` (still `rel_visible`), a NEW join
//!    `NsItem` (alias `None` or the `JOIN ... AS j` alias if `TableAlias` present on a NestedJoin) is
//!    added with `cols_visible = true` and the merged column list; `ON` clauses are bound BEFORE this
//!    change (they see both sides' columns).
//! 6. Result: `FromItem::Join(Box<JoinItem { kind, left, right, quals }>)`.
//! Chains `a JOIN b ON .. JOIN c ON ..` are left-deep: the left item of the second join is the first
//! join (the ON of the second sees a, b, c).
use pgarbor_common::{PgError, Result, SqlState};
use sqlparser::ast::{Expr, Join, JoinConstraint, JoinOperator, ObjectName, ObjectNamePart, TableFactor};

use super::coerce;
use super::context::{Binder, BoundArg, ExprCtx};
use super::expr;
use super::expr_ops;
use super::from;
use super::scope::{self, AccessKind, NsColumn, NsItem};
use crate::diag;
use crate::ir::{BoundExpr, ExprKind, FromItem, JoinItem, JoinKind};

/// A USING column name (already case-folded) with the position of its identifier.
type UsingName = (String, Option<u32>);

/// Combine `left` with the right side of `join` (module docs).
pub fn bind_join(b: &mut Binder, left: FromItem, join: &Join) -> Result<FromItem> {
    let (kind, constraint) = join_kind(&join.join_operator)?;
    // USING names are simple identifiers; a qualified name is a syntax error before any binding.
    let using_names: Vec<UsingName> = match constraint {
        JoinConstraint::Using(names) => using_name_list(b, names)?,
        _ => Vec::new(),
    };

    // The left item is the last namespace entry: the previous table factor or join pushed it last.
    let left_idx = last_ns_index(b)?;
    check_item_matches(b, left_idx, &left)?;
    let mut left_leaves = Vec::new();
    collect_leaves(&left, &mut left_leaves);
    let left_set = side_indices(&b.scope()?.namespace, left_idx, &left_leaves);

    // PG transformFromClauseItem: left-side names are exposed to a LATERAL right side, but a
    // reference is an error unless the join is INNER or LEFT (lateral_only + lateral_ok).
    let lateral_ok_left = matches!(kind, JoinKind::Inner | JoinKind::Left);
    set_lateral_state(b, &left_set, true, lateral_ok_left)?;

    let right = from::bind_table_factor(b, &join.relation, relation_is_lateral(&join.relation))?;
    let right_idx = last_ns_index(b)?;
    if right_idx == left_idx {
        return Err(PgError::internal("binder: JOIN right side added no namespace item"));
    }
    check_item_matches(b, right_idx, &right)?;
    let mut right_leaves = Vec::new();
    collect_leaves(&right, &mut right_leaves);
    let right_set = side_indices(&b.scope()?.namespace, right_idx, &right_leaves);
    set_lateral_state(b, &left_set, false, true)?;

    let left_cols = item_columns(b, left_idx)?;
    let right_cols = item_columns(b, right_idx)?;

    let (quals, output) = match constraint {
        JoinConstraint::On(cond) => {
            let mut inputs = left_set.clone();
            inputs.extend(right_set.iter().copied());
            let qual = bind_join_on(b, cond, &inputs)?;
            (Some(qual), plain_output(&left_cols, &right_cols))
        }
        JoinConstraint::Using(_) => finish_using(b, kind, &using_names, &left_cols, &right_cols)?,
        JoinConstraint::Natural => {
            let names = natural_names(&left_cols, &right_cols);
            finish_using(b, kind, &names, &left_cols, &right_cols)?
        }
        JoinConstraint::None => (None, plain_output(&left_cols, &right_cols)),
    };

    // Both inputs keep their names (`t1.id`, `t2.id`) but stop supplying unqualified columns.
    set_cols_visible(b, left_idx, false)?;
    set_cols_visible(b, right_idx, false)?;
    b.scope_mut()?.namespace.push(NsItem {
        alias: None,
        schema: None,
        rt_index: None,
        columns: output,
        rel_visible: false,
        cols_visible: true,
        lateral_only: false,
        lateral_ok: true,
    });
    Ok(FromItem::Join(Box::new(JoinItem { kind, left, right, quals })))
}

/// Map the sqlparser operator to a join kind and its constraint (module docs, step 2). Syntax
/// errors that PostgreSQL's grammar reports before any binding are raised here.
fn join_kind(op: &JoinOperator) -> Result<(JoinKind, &JoinConstraint)> {
    let (kind, is_cross, constraint) = match op {
        JoinOperator::Join(c) | JoinOperator::Inner(c) => (JoinKind::Inner, false, c),
        JoinOperator::Left(c) | JoinOperator::LeftOuter(c) => (JoinKind::Left, false, c),
        JoinOperator::Right(c) | JoinOperator::RightOuter(c) => (JoinKind::Right, false, c),
        JoinOperator::FullOuter(c) => (JoinKind::Full, false, c),
        JoinOperator::CrossJoin(c) => (JoinKind::Inner, true, c),
        _ => return Err(PgError::new(SqlState::SYNTAX_ERROR, "syntax error")),
    };
    match (is_cross, constraint) {
        (true, JoinConstraint::On(_)) => Err(syntax_error_at("ON")),
        (true, JoinConstraint::Using(_)) => Err(syntax_error_at("USING")),
        (true, JoinConstraint::Natural) => Err(syntax_error_at("NATURAL")),
        (false, JoinConstraint::None) => Err(PgError::new(SqlState::SYNTAX_ERROR, "syntax error at end of input")),
        _ => Ok((kind, constraint)),
    }
}

fn syntax_error_at(token: &str) -> PgError {
    PgError::new(SqlState::SYNTAX_ERROR, format!("syntax error at or near \"{token}\""))
}

/// The names of a `USING (a, b)` list. Each must be a single identifier.
fn using_name_list(b: &mut Binder, names: &[ObjectName]) -> Result<Vec<UsingName>> {
    let mut out = Vec::with_capacity(names.len());
    for name in names {
        let pos = b.pos(name);
        match name.0.as_slice() {
            [ObjectNamePart::Identifier(id)] => {
                let folded = b.ident(id);
                out.push((folded, pos));
            }
            _ => return Err(syntax_error_at(".")),
        }
    }
    Ok(out)
}

/// Index of the last namespace item (the output of the most recently bound FROM item).
fn last_ns_index(b: &Binder) -> Result<usize> {
    b.scope()?
        .namespace
        .len()
        .checked_sub(1)
        .ok_or_else(|| PgError::internal("binder: JOIN without a namespace item"))
}

/// Sanity check that the namespace item at `idx` is the one for a plain range-table `item`.
fn check_item_matches(b: &Binder, idx: usize, item: &FromItem) -> Result<()> {
    if let FromItem::Range(rt) = item {
        let ns = &b.scope()?.namespace;
        let same = ns.get(idx).map_or(false, |ns_item| ns_item.rt_index == Some(*rt));
        if !same {
            return Err(PgError::internal("binder: JOIN namespace out of step with the FROM tree"));
        }
    }
    Ok(())
}

/// Range-table indices of every plain relation under `item`.
fn collect_leaves(item: &FromItem, out: &mut Vec<u32>) {
    match item {
        FromItem::Range(rt) => out.push(*rt),
        FromItem::Join(j) => {
            collect_leaves(&j.left, out);
            collect_leaves(&j.right, out);
        }
    }
}

/// Namespace indices that belong to one join input: its own output item (`own`) plus every
/// namespace item of a range-table entry under the input (`leaves`).
fn side_indices(ns: &[NsItem], own: usize, leaves: &[u32]) -> Vec<usize> {
    ns.iter()
        .enumerate()
        .filter(|(i, item)| *i == own || item.rt_index.is_some_and(|rt| leaves.contains(&rt)))
        .map(|(i, _)| i)
        .collect()
}

fn set_lateral_state(b: &mut Binder, idxs: &[usize], lateral_only: bool, lateral_ok: bool) -> Result<()> {
    let ns = &mut b.scope_mut()?.namespace;
    for i in idxs {
        if let Some(item) = ns.get_mut(*i) {
            item.lateral_only = lateral_only;
            item.lateral_ok = lateral_ok;
        }
    }
    Ok(())
}

fn set_cols_visible(b: &mut Binder, idx: usize, visible: bool) -> Result<()> {
    let ns = &mut b.scope_mut()?.namespace;
    match ns.get_mut(idx) {
        Some(item) => {
            item.cols_visible = visible;
            Ok(())
        }
        None => Err(PgError::internal("binder: JOIN namespace item missing")),
    }
}

/// Output columns of a namespace item (clones; the caller decides what goes into the join item).
fn item_columns(b: &Binder, idx: usize) -> Result<Vec<NsColumn>> {
    let ns = &b.scope()?.namespace;
    ns.get(idx)
        .map(|item| item.columns.clone())
        .ok_or_else(|| PgError::internal("binder: JOIN namespace item missing"))
}

/// Output columns of a plain `ON` / `CROSS` join: every non-dropped column of both inputs.
fn plain_output(left: &[NsColumn], right: &[NsColumn]) -> Vec<NsColumn> {
    left.iter().chain(right.iter()).filter(|c| !c.dropped).cloned().collect()
}

/// Whether the right side of a join may reference the left side (LATERAL or implicitly lateral).
/// PG: functions in FROM are implicitly LATERAL.
fn relation_is_lateral(tf: &TableFactor) -> bool {
    match tf {
        TableFactor::Derived { lateral, .. } => *lateral,
        TableFactor::Function { .. } | TableFactor::UNNEST { .. } => true,
        TableFactor::Table { args: Some(_), .. } => true,
        _ => false,
    }
}

/// Bind `ON cond` with only the two join inputs visible (PG `transformJoinOnClause`). Every other
/// namespace item of the FROM list is made unreachable while the condition is bound.
fn bind_join_on(b: &mut Binder, cond: &Expr, inputs: &[usize]) -> Result<BoundExpr> {
    let saved = hide_outside_inputs(b, inputs)?;
    let bound = expr::bind_boolean(b, cond, ExprCtx::join_on(), "JOIN/ON");
    restore_lateral_flags(b, &saved);
    bound
}

/// Mark the namespace items outside `inputs` as `lateral_only` and not `lateral_ok`, so a
/// reference reports `invalid reference to FROM-clause entry`. Returns the old flags for restore.
/// Hidden join items (`!cols_visible` and no range entry) are not touched.
fn hide_outside_inputs(b: &mut Binder, inputs: &[usize]) -> Result<Vec<(usize, bool, bool)>> {
    let ns = &mut b.scope_mut()?.namespace;
    let mut saved = Vec::new();
    for (i, item) in ns.iter_mut().enumerate() {
        if inputs.contains(&i) {
            continue;
        }
        if item.rt_index.is_none() && !item.cols_visible {
            continue;
        }
        saved.push((i, item.lateral_only, item.lateral_ok));
        item.lateral_only = true;
        item.lateral_ok = false;
    }
    Ok(saved)
}

fn restore_lateral_flags(b: &mut Binder, saved: &[(usize, bool, bool)]) {
    if let Ok(scope) = b.scope_mut() {
        for (i, only, ok) in saved {
            if let Some(item) = scope.namespace.get_mut(*i) {
                item.lateral_only = *only;
                item.lateral_ok = *ok;
            }
        }
    }
}

/// `NATURAL`: the common column names, in left-side order (PG builds the same USING list).
/// Empty when the sides share no column, which makes the join a cross join.
fn natural_names(left: &[NsColumn], right: &[NsColumn]) -> Vec<UsingName> {
    let mut names: Vec<UsingName> = Vec::new();
    for c in left.iter().filter(|c| !c.dropped) {
        if names.iter().any(|(n, _)| *n == c.name) {
            continue;
        }
        if right.iter().any(|r| !r.dropped && r.name == c.name) {
            names.push((c.name.clone(), None));
        }
    }
    names
}

/// Result of the USING/NATURAL column matching.
struct UsingOutcome {
    /// `l.c = r.c` per USING column, in USING order.
    quals: Vec<BoundExpr>,
    /// Merged join columns, in USING order.
    merged: Vec<NsColumn>,
    /// Which left / right input columns were merged (and so are not repeated in the output).
    left_used: Vec<bool>,
    right_used: Vec<bool>,
}

/// USING / NATURAL: build the quals and the merged columns, then the output column list.
fn finish_using(
    b: &mut Binder,
    kind: JoinKind,
    names: &[UsingName],
    left: &[NsColumn],
    right: &[NsColumn],
) -> Result<(Option<BoundExpr>, Vec<NsColumn>)> {
    let UsingOutcome { quals, merged, left_used, right_used } = bind_using(b, kind, names, left, right)?;
    let qual = if quals.is_empty() { None } else { Some(BoundExpr::and_all(quals)) };
    let output = assemble_using_output(merged, left, right, &left_used, &right_used);
    Ok((qual, output))
}

fn bind_using(
    b: &mut Binder,
    kind: JoinKind,
    names: &[UsingName],
    left: &[NsColumn],
    right: &[NsColumn],
) -> Result<UsingOutcome> {
    let mut outcome = UsingOutcome {
        quals: Vec::new(),
        merged: Vec::new(),
        left_used: vec![false; left.len()],
        right_used: vec![false; right.len()],
    };
    let mut seen: Vec<&str> = Vec::new();
    for (name, pos) in names {
        let pos = *pos;
        if seen.contains(&name.as_str()) {
            return Err(with_pos(duplicate_using_error(name), pos));
        }
        seen.push(name.as_str());

        let li = find_common(left, name, "left", pos)?;
        let ri = find_common(right, name, "right", pos)?;
        let lexpr = column_expr(left, li)?;
        let rexpr = column_expr(right, ri)?;

        // PG transformJoinUsingClause: the equality is resolved on the original column types.
        let qual = expr_ops::make_op(
            b,
            "=",
            Some(BoundArg::new(lexpr.clone(), pos)),
            BoundArg::new(rexpr.clone(), pos),
            pos,
        )?;
        mark_select_access(b, &lexpr)?;
        mark_select_access(b, &rexpr)?;

        // PG buildMergedJoinVar: the merged column has the common type of the two inputs.
        let args = vec![BoundArg::new(lexpr, pos), BoundArg::new(rexpr, pos)];
        let (coerced, common) = coerce::coerce_to_common(b, args, "JOIN/USING")?;
        let mut coerced = coerced;
        let rc = coerced
            .pop()
            .ok_or_else(|| PgError::internal("binder: JOIN/USING lost a coerced argument"))?;
        let lc = coerced
            .pop()
            .ok_or_else(|| PgError::internal("binder: JOIN/USING lost a coerced argument"))?;
        let value = match kind {
            JoinKind::Inner | JoinKind::Left => lc,
            JoinKind::Right => rc,
            JoinKind::Full => BoundExpr::new(ExprKind::Coalesce(vec![lc, rc]), common),
        };

        mark_used(&mut outcome.left_used, li);
        mark_used(&mut outcome.right_used, ri);
        outcome.quals.push(qual);
        outcome.merged.push(NsColumn {
            name: name.clone(),
            expr: value,
            dropped: false,
            hidden_from_star: false,
        });
    }
    Ok(outcome)
}

/// Find the single non-dropped column called `name` on one side of a USING.
fn find_common(cols: &[NsColumn], name: &str, side: &str, pos: Option<u32>) -> Result<usize> {
    let mut found: Option<usize> = None;
    let mut count = 0usize;
    for (i, c) in cols.iter().enumerate() {
        if !c.dropped && c.name == name {
            count += 1;
            if found.is_none() {
                found = Some(i);
            }
        }
    }
    match (count, found) {
        (1, Some(i)) => Ok(i),
        (0, _) => Err(diag::using_column_missing(name, side, pos)),
        _ => Err(diag::using_column_ambiguous(name, side, pos)),
    }
}

fn column_expr(cols: &[NsColumn], idx: usize) -> Result<BoundExpr> {
    cols.get(idx)
        .map(|c| c.expr.clone())
        .ok_or_else(|| PgError::internal("binder: JOIN USING column index out of range"))
}

fn mark_used(used: &mut [bool], idx: usize) {
    if let Some(flag) = used.get_mut(idx) {
        *flag = true;
    }
}

/// Output list of a USING/NATURAL join: merged columns first, then the unmerged left columns,
/// then the unmerged right columns (module docs, step 4).
fn assemble_using_output(
    merged: Vec<NsColumn>,
    left: &[NsColumn],
    right: &[NsColumn],
    left_used: &[bool],
    right_used: &[bool],
) -> Vec<NsColumn> {
    let mut out = merged;
    push_unused(&mut out, left, left_used);
    push_unused(&mut out, right, right_used);
    out
}

fn push_unused(out: &mut Vec<NsColumn>, cols: &[NsColumn], used: &[bool]) {
    for (i, c) in cols.iter().enumerate() {
        let is_used = used.get(i).copied().unwrap_or(false);
        if !c.dropped && !is_used {
            out.push(c.clone());
        }
    }
}

/// Record a select-privilege need for every column variable (of the innermost query) in `e`.
fn mark_select_access(b: &mut Binder, e: &BoundExpr) -> Result<()> {
    let mut refs: Vec<(u32, i16)> = Vec::new();
    e.walk(&mut |n: &BoundExpr| {
        if let ExprKind::Column(c) = &n.kind {
            if c.levels_up == 0 {
                refs.push((c.rt_index, c.attnum));
            }
        }
        true
    });
    let scope = b.scope_mut()?;
    for (rt, attnum) in refs {
        if let Some(entry) = scope.range_table.get_mut((rt as usize).wrapping_sub(1)) {
            scope::mark_access(&mut entry.access, AccessKind::Select, attnum);
        }
    }
    Ok(())
}

fn duplicate_using_error(name: &str) -> PgError {
    PgError::new(
        SqlState::DUPLICATE_COLUMN,
        format!("column name \"{name}\" appears more than once in USING clause"),
    )
}

fn with_pos(e: PgError, pos: Option<u32>) -> PgError {
    match pos {
        Some(p) => e.with_position(p),
        None => e,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ir::BoundExpr;
    use pgarbor_types::TypeId;
    use sqlparser::ast::{JoinConstraint, JoinOperator};

    fn col(name: &str, dropped: bool) -> NsColumn {
        NsColumn {
            name: name.to_string(),
            expr: BoundExpr::column(0, 1, 1, TypeId::INT4),
            dropped,
            hidden_from_star: false,
        }
    }

    #[test]
    fn join_operator_mapping() {
        let kind = |op: &JoinOperator| join_kind(op).ok().map(|(k, _)| k);
        assert_eq!(kind(&JoinOperator::Inner(JoinConstraint::Natural)), Some(JoinKind::Inner));
        assert_eq!(kind(&JoinOperator::Join(JoinConstraint::Natural)), Some(JoinKind::Inner));
        assert_eq!(kind(&JoinOperator::LeftOuter(JoinConstraint::Natural)), Some(JoinKind::Left));
        assert_eq!(kind(&JoinOperator::RightOuter(JoinConstraint::Natural)), Some(JoinKind::Right));
        assert_eq!(kind(&JoinOperator::FullOuter(JoinConstraint::Natural)), Some(JoinKind::Full));
        assert_eq!(kind(&JoinOperator::CrossJoin(JoinConstraint::None)), Some(JoinKind::Inner));
        assert_eq!(kind(&JoinOperator::Semi(JoinConstraint::Natural)), None);
    }

    #[test]
    fn join_syntax_errors() {
        let code = |op: &JoinOperator| join_kind(op).err().map(|e| e.sqlstate.code().to_string());
        assert_eq!(code(&JoinOperator::Join(JoinConstraint::None)).as_deref(), Some("42601"));
        assert_eq!(code(&JoinOperator::CrossJoin(JoinConstraint::Natural)).as_deref(), Some("42601"));
        assert_eq!(code(&JoinOperator::Semi(JoinConstraint::Natural)).as_deref(), Some("42601"));
    }

    #[test]
    fn common_column_lookup() {
        let cols = vec![col("id", false), col("gone", true), col("id", true)];
        assert_eq!(find_common(&cols, "id", "left", None).ok(), Some(0));

        let missing = find_common(&cols, "nope", "left", Some(3)).err().expect("missing column");
        assert_eq!(missing.sqlstate.code(), "42703");
        assert_eq!(missing.message, "column \"nope\" specified in USING clause does not exist in left table");
        assert_eq!(missing.position, Some(3));

        let dup = vec![col("x", false), col("x", false)];
        let ambiguous = find_common(&dup, "x", "right", None).err().expect("ambiguous column");
        assert_eq!(ambiguous.sqlstate.code(), "42702");
        assert_eq!(ambiguous.message, "common column name \"x\" appears more than once in right table");
    }

    #[test]
    fn duplicate_using_name() {
        let e = duplicate_using_error("x");
        assert_eq!(e.sqlstate.code(), "42701");
        assert_eq!(e.message, "column name \"x\" appears more than once in USING clause");
    }

    #[test]
    fn natural_names_are_common_columns_in_left_order() {
        let left = vec![col("a", false), col("b", false), col("c", false)];
        let right = vec![col("b", false), col("c", false), col("d", false)];
        let names: Vec<String> = natural_names(&left, &right).into_iter().map(|(n, _)| n).collect();
        assert_eq!(names, vec!["b".to_string(), "c".to_string()]);
        assert!(natural_names(&left, &[col("z", false)]).is_empty());
    }

    #[test]
    fn using_output_lists_merged_then_remaining_columns() {
        let left = vec![col("id", false), col("a", false)];
        let right = vec![col("id", false), col("b", false), col("gone", true)];
        let merged = vec![col("id", false)];
        let out = assemble_using_output(merged, &left, &right, &[true, false], &[true, false, false]);
        let names: Vec<&str> = out.iter().map(|c| c.name.as_str()).collect();
        assert_eq!(names, vec!["id", "a", "b"]);
    }

    #[test]
    fn join_side_indices_follow_the_join_tree() {
        let item = |rt: Option<u32>| NsItem {
            alias: None,
            schema: None,
            rt_index: rt,
            columns: Vec::new(),
            rel_visible: true,
            cols_visible: true,
            lateral_only: false,
            lateral_ok: true,
        };
        // a (rt 1), b (rt 2), join(a, b) output (index 2), c (rt 3)
        let ns = vec![item(Some(1)), item(Some(2)), item(None), item(Some(3))];
        assert_eq!(side_indices(&ns, 2, &[1, 2]), vec![0, 1, 2]);
        assert_eq!(side_indices(&ns, 3, &[3]), vec![3]);
    }
}
