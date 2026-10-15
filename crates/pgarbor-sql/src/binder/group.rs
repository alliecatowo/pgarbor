//! GROUP BY, HAVING validity, GROUPING SETS, the ungrouped-column check (SQL-20, sonnet).
//! PG references: `parse_clause.c` (`transformGroupClause`, `transformGroupClauseList`,
//! `transformGroupClauseExpr`, `transformGroupingSet`, `expand_grouping_sets`,
//! `findTargetlistEntrySQL92/99`), `parse_agg.c` (`parseCheckAggregates`, `check_ungrouped_columns`,
//! `check_ungrouped_columns_walker`, `finalize_grouping_exprs`).
//!
//! ## `bind_group_by(b, group_by, target_list)` -> [`crate::ir::GroupBy`]
//! `GroupByExpr::Expressions(exprs, modifiers)` (`ALL` is non-PG -> 42601; `WITH ROLLUP/CUBE/TOTALS`
//! modifiers MySQL -> 42601). Each grouping item is one of
//! * an integer literal `GROUP BY 2` -> ordinal into the target list (42P10 `GROUP BY position 5 is
//!   not in select list`, `diag::position_not_in_select_list`; positions of aggregates:
//!   42803 `aggregate functions are not allowed in GROUP BY`);
//! * a bare identifier that matches an OUTPUT column name AND is not an input column name -> that
//!   target entry (PG SQL92 rule: input column wins over output alias in GROUP BY, the opposite of
//!   ORDER BY); ambiguous output names -> 42702 `GROUP BY "x" is ambiguous` (`output_name_ambiguous`);
//! * any expression: bound with `ExprCtx::group_by()` (no aggs/windows/SRFs), matched by structural
//!   equality against existing target entries (reuse index) else appended as a junk target entry.
//! `GroupBy::exprs` are target-list indexes in first-appearance order, de-duplicated. A volatile
//! or SRF grouping expression is fine. Grouping on a type with no equality operator: 42883
//! `could not identify an equality operator for type X`.
//!
//! ### GROUPING SETS / ROLLUP / CUBE
//! `GROUPING SETS ((a,b),(a),())`, `ROLLUP (a,b)` = sets `(a,b),(a),()`, `CUBE (a,b)` = 4 sets;
//! nested/mixed items combine by cross product (`GROUP BY a, ROLLUP (b,c)`). Expand to explicit
//! `sets: Some(Vec<Vec<usize>>)` (indexes into `GroupBy::exprs`), de-duplicated, preserving the
//! order PG uses (`expand_grouping_sets`: ROLLUP descending prefixes, CUBE binary counting order).
//! More than 4096 sets: 54001 `too many grouping sets present (maximum 4096)`. `GROUP BY ()` alone =
//! `exprs: [], sets: Some(vec![vec![]])`.
//!
//! ## `resolve_pending_groupings` (see `expr_agg`): rewrite `Grouping` nodes' `args` from indexes into
//! `QueryScope::pending_grouping` to indexes into `GroupBy::exprs`; argument not among the grouping
//! expressions -> 42P20 `arguments to GROUPING must be grouping expressions of the associated query level`.
//!
//! ## `check_grouping(b, parts)` (PG `parseCheckAggregates`)
//! Runs only if the block `has_aggs` OR has a GROUP BY clause OR has HAVING. Walk every expression of
//! the target list (non-junk AND junk entries that are not themselves grouping entries), HAVING and
//! window ORDER/PARTITION expressions; a `Column{levels_up: 0}` that is not (a) inside an `Aggref` of
//! level 0, (b) inside a sub-expression structurally equal to a grouping expression, (c) functionally
//! dependent on a grouped primary key of the same table (PG 9.1 rule: if every column of a table's
//! PRIMARY KEY is grouped, all columns of that table are allowed; needs `env.snapshot().primary_key`)
//! is an error 42803 `column "t.b" must appear in the GROUP BY clause or be used in an aggregate
//! function` (`diag::ungrouped_column(alias, column, pos)`, pos from `ColumnRef::pos`). Outer-level
//! columns (`levels_up > 0`) are never errors at this level. Inside a sub-select of a grouped query,
//! an ungrouped OUTER column reference is `subquery uses ungrouped column "t.b" from outer query`
//! (`ungrouped_column_in_subquery`). HAVING without GROUP BY and without aggregates: the query is
//! grouped (single group) and ungrouped columns in HAVING/targets are errors. For grouping sets, an
//! expression grouped in only some sets is still "grouped" for this check.
//! Windows on grouped queries: window PARTITION/ORDER BY expressions follow the same rule.
use pgarbor_common::{PgError, Result};
use sqlparser::ast::GroupByExpr;

use super::context::Binder;
use crate::ir::{BoundExpr, GroupBy, TargetEntry, WindowDef};

/// Bind the GROUP BY clause (module docs); may append junk entries to `target_list`.
#[allow(unused_variables)]
pub fn bind_group_by(b: &mut Binder, group_by: &GroupByExpr, target_list: &mut Vec<TargetEntry>) -> Result<GroupBy> {
    Err(PgError::unimplemented("SQL-20", "GROUP BY binding (group::bind_group_by)"))
}

/// Rewrite `Grouping` nodes (module docs). Operates on the target list, HAVING and ORDER BY
/// expressions of the block (the caller passes mutable references to all of them).
#[allow(unused_variables)]
pub fn resolve_pending_groupings(
    b: &mut Binder,
    group: &GroupBy,
    target_list: &mut [TargetEntry],
    having: Option<&mut BoundExpr>,
) -> Result<()> {
    Err(PgError::unimplemented("SQL-20", "GROUPING() resolution (group::resolve_pending_groupings)"))
}

/// The 42803 ungrouped-column check (module docs). `order_exprs` are the expressions of ORDER BY keys
/// that are NOT target entries (none after junk entries were added: pass `&[]`), `windows` the block's
/// window definitions.
#[allow(unused_variables)]
pub fn check_grouping(
    b: &mut Binder,
    group: &GroupBy,
    target_list: &[TargetEntry],
    having: Option<&BoundExpr>,
    windows: &[WindowDef],
    has_aggs: bool,
) -> Result<()> {
    Err(PgError::unimplemented("SQL-20", "grouping validity check (group::check_grouping)"))
}
