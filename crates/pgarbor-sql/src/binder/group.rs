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
use pgarbor_catalog::CatalogEnv;
use pgarbor_common::{PgError, Result, SqlState};
use pgarbor_types::{ops, TypeEnv, TypeId};
use sqlparser::ast::{Expr, GroupByExpr, GroupByWithModifier, Value};
use sqlparser::tokenizer::Span;

use super::context::{Binder, ExprCtx};
use super::expr::bind_expr;
use super::scope::resolve_column;
use crate::diag::{self, ExprClause};
use crate::ir::{
    BoundExpr, BoundQuery, BoundSelect, ColumnRef, CteBody, ExprKind, FrameBound, FromItem, GroupBy, QueryBody,
    RangeEntry, RangeKind, TargetEntry, WindowDef,
};

/// PG `parseCheckAggregates` / `expand_grouping_sets` limit on the number of grouping sets.
const MAX_GROUPING_SETS: usize = 4096;
/// Largest CUBE argument count whose expansion stays within [`MAX_GROUPING_SETS`] (2^12 = 4096).
const MAX_CUBE_UNITS: usize = 12;

/// Bind the GROUP BY clause (module docs); may append junk entries to `target_list`.
pub fn bind_group_by(b: &mut Binder, group_by: &GroupByExpr, target_list: &mut Vec<TargetEntry>) -> Result<GroupBy> {
    let items = match group_by {
        // `GROUP BY ALL` is DuckDB/Snowflake syntax; PostgreSQL rejects it at the ALL keyword.
        GroupByExpr::All(_) => return Err(diag::syntax_error_near("ALL", None)),
        GroupByExpr::Expressions(exprs, modifiers) => {
            // `WITH ROLLUP|CUBE|TOTALS` (MySQL/ClickHouse) and the trailing GROUPING SETS modifier are
            // not PostgreSQL grammar.
            if let Some(modifier) = modifiers.first() {
                let token = match modifier {
                    GroupByWithModifier::GroupingSets(_) => "GROUPING",
                    GroupByWithModifier::Rollup | GroupByWithModifier::Cube | GroupByWithModifier::Totals => "WITH",
                };
                return Err(diag::syntax_error_near(token, None));
            }
            exprs
        }
    };

    let mut exprs: Vec<usize> = Vec::new();
    let mut has_sets = false;
    let mut alternatives: Vec<Vec<Vec<usize>>> = Vec::with_capacity(items.len());
    for item in items {
        has_sets |= match item {
            Expr::GroupingSets(_) | Expr::Rollup(_) | Expr::Cube(_) => true,
            Expr::Tuple(members) => members.is_empty(),
            _ => false,
        };
        alternatives.push(grouping_item(b, target_list, &mut exprs, item)?);
    }

    // Plain `GROUP BY a, b` is one grouping set holding every item; any grouping-set construct (or
    // an empty `()` item) makes the clause a list of explicit sets.
    let sets = if has_sets { Some(product(alternatives)?) } else { None };
    Ok(GroupBy { exprs, sets })
}

/// Rewrite `Grouping` nodes (module docs). Operates on the target list, HAVING and ORDER BY
/// expressions of the block (the caller passes mutable references to all of them).
pub fn resolve_pending_groupings(
    b: &mut Binder,
    group: &GroupBy,
    target_list: &mut [TargetEntry],
    having: Option<&mut BoundExpr>,
) -> Result<()> {
    let pending: Vec<BoundExpr> = b.scope()?.pending_grouping.clone();
    if pending.is_empty() {
        return Ok(());
    }
    // For each GROUPING argument (index into `pending`), its position among `group.exprs`.
    let map: Vec<Option<usize>> = {
        let mut grouped: Vec<&BoundExpr> = Vec::with_capacity(group.exprs.len());
        for &ti in &group.exprs {
            let entry = target_list.get(ti).ok_or_else(|| PgError::internal("GROUP BY index out of range"))?;
            grouped.push(&entry.expr);
        }
        pending.iter().map(|p| grouped.iter().position(|g| *g == p)).collect()
    };
    for entry in target_list.iter_mut() {
        resolve_grouping_node(&mut entry.expr, &map)?;
    }
    if let Some(having) = having {
        resolve_grouping_node(having, &map)?;
    }
    Ok(())
}

/// The 42803 ungrouped-column check (module docs). `order_exprs` are the expressions of ORDER BY keys
/// that are NOT target entries (none after junk entries were added: pass `&[]`), `windows` the block's
/// window definitions.
pub fn check_grouping(
    b: &mut Binder,
    group: &GroupBy,
    target_list: &[TargetEntry],
    having: Option<&BoundExpr>,
    windows: &[WindowDef],
    has_aggs: bool,
) -> Result<()> {
    if !(has_aggs || group.sets.is_some() || !group.exprs.is_empty() || having.is_some()) {
        return Ok(());
    }
    let env: &CatalogEnv = b.env;
    let rtable: &[RangeEntry] = &b.scope()?.range_table;

    let mut group_exprs: Vec<&BoundExpr> = Vec::with_capacity(group.exprs.len());
    for &ti in &group.exprs {
        let entry = target_list.get(ti).ok_or_else(|| PgError::internal("GROUP BY index out of range"))?;
        group_exprs.push(&entry.expr);
    }

    // Functional dependency only counts columns grouped in EVERY grouping set (PG `groupClauseCommonVars`).
    let common_positions: Vec<usize> = match &group.sets {
        None => (0..group.exprs.len()).collect(),
        Some(sets) => match sets.split_first() {
            None => Vec::new(),
            Some((first, rest)) => first.iter().copied().filter(|p| rest.iter().all(|s| s.contains(p))).collect(),
        },
    };
    let mut common_cols: Vec<(u32, i16)> = Vec::new();
    for p in common_positions {
        if let Some(&ti) = group.exprs.get(p) {
            if let Some(TargetEntry { expr: BoundExpr { kind: ExprKind::Column(c), .. }, .. }) = target_list.get(ti) {
                if c.levels_up == 0 {
                    common_cols.push((c.rt_index, c.attnum));
                }
            }
        }
    }

    let mut checker = Checker { env, rtable, group_exprs, common_cols, fd_proven: Vec::new() };
    for entry in target_list {
        checker.expr(&entry.expr, 0)?;
    }
    if let Some(having) = having {
        checker.expr(having, 0)?;
    }
    checker.windows(windows, 0)
}

// ------------------------------------------------------------------------------------------------
// GROUP BY items
// ------------------------------------------------------------------------------------------------

/// One top-level GROUP BY item as its list of alternative grouping sets (positions in `exprs`).
/// A plain expression yields one set holding just that expression.
fn grouping_item(
    b: &mut Binder,
    tl: &mut Vec<TargetEntry>,
    exprs: &mut Vec<usize>,
    item: &Expr,
) -> Result<Vec<Vec<usize>>> {
    match item {
        Expr::GroupingSets(sets) => {
            let mut alternatives: Vec<Vec<usize>> = Vec::new();
            for set in sets {
                alternatives.extend(grouping_set_alternatives(b, tl, exprs, set)?);
            }
            Ok(alternatives)
        }
        Expr::Rollup(units) => {
            let units = unit_members(b, tl, exprs, units)?;
            Ok(rollup_alternatives(&units))
        }
        Expr::Cube(units) => {
            let units = unit_members(b, tl, exprs, units)?;
            cube_alternatives(&units)
        }
        Expr::Tuple(members) if members.is_empty() => Ok(vec![Vec::new()]),
        other => Ok(vec![vec![group_member(b, tl, exprs, other)?]]),
    }
}

/// One element of a GROUPING SETS list: a nested construct contributes its own alternatives, anything
/// else is a single set (`()` gives the empty set).
fn grouping_set_alternatives(
    b: &mut Binder,
    tl: &mut Vec<TargetEntry>,
    exprs: &mut Vec<usize>,
    set: &[Expr],
) -> Result<Vec<Vec<usize>>> {
    if let [single] = set {
        if matches!(single, Expr::GroupingSets(_) | Expr::Rollup(_) | Expr::Cube(_)) {
            return grouping_item(b, tl, exprs, single);
        }
    }
    let mut members: Vec<usize> = Vec::new();
    for e in set {
        let k = group_member(b, tl, exprs, e)?;
        if !members.contains(&k) {
            members.push(k);
        }
    }
    Ok(vec![members])
}

/// The units of a ROLLUP / CUBE: each unit is one expression or a parenthesized list, as positions.
fn unit_members(b: &mut Binder, tl: &mut Vec<TargetEntry>, exprs: &mut Vec<usize>, units: &[Vec<Expr>]) -> Result<Vec<Vec<usize>>> {
    let mut out: Vec<Vec<usize>> = Vec::with_capacity(units.len());
    for unit in units {
        let mut members: Vec<usize> = Vec::new();
        for e in unit {
            let k = group_member(b, tl, exprs, e)?;
            if !members.contains(&k) {
                members.push(k);
            }
        }
        out.push(members);
    }
    Ok(out)
}

/// Resolve one grouping expression to its position in `GroupBy::exprs` (module docs: ordinal, output
/// name, or expression), appending a new position when needed.
fn group_member(b: &mut Binder, tl: &mut Vec<TargetEntry>, exprs: &mut Vec<usize>, e: &Expr) -> Result<usize> {
    let pos = b.pos(e);
    let target = if let Some((n, span)) = int_literal(e) {
        let span_pos = b.pos_span(span);
        ordinal_target(tl.as_slice(), n, span_pos)?
    } else if let Some(i) = output_name_target(b, tl.as_slice(), e, pos)? {
        i
    } else {
        let bound = bind_expr(b, e, ExprCtx::group_by())?;
        match tl.iter().position(|t| t.expr == bound) {
            Some(i) => i,
            None => {
                tl.push(TargetEntry { expr: bound, name: "?column?".to_string(), junk: true, origin: None });
                tl.len() - 1
            }
        }
    };
    let ty = tl
        .get(target)
        .map(|t| t.expr.ty)
        .ok_or_else(|| PgError::internal("GROUP BY target index out of range"))?;
    add_group_index(b, exprs, target, ty, pos)
}

/// Position in `exprs` of target entry `target`, appended (after the equality check) if new.
fn add_group_index(b: &Binder, exprs: &mut Vec<usize>, target: usize, ty: TypeId, pos: Option<u32>) -> Result<usize> {
    if let Some(k) = exprs.iter().position(|&x| x == target) {
        return Ok(k);
    }
    ensure_equality(b, ty, pos)?;
    exprs.push(target);
    Ok(exprs.len() - 1)
}

/// `GROUP BY <integer literal>`: the literal, when it fits in int4 (PG `Integer` node), as an ordinal.
fn int_literal(e: &Expr) -> Option<(i32, Span)> {
    match e {
        Expr::Value(v) => match &v.value {
            Value::Number(text, _) => text.parse::<i32>().ok().map(|n| (n, v.span.clone())),
            _ => None,
        },
        _ => None,
    }
}

/// Target entry of the `n`-th select-list item (1-based, junk entries skipped).
fn ordinal_target(tl: &[TargetEntry], n: i32, pos: Option<u32>) -> Result<usize> {
    let found = if n >= 1 {
        tl.iter().enumerate().filter(|(_, t)| !t.junk).nth((n - 1) as usize).map(|(i, _)| i)
    } else {
        None
    };
    match found {
        Some(i) => {
            check_group_target(&tl[i].expr, pos)?;
            Ok(i)
        }
        None => Err(diag::position_not_in_select_list("GROUP BY", i64::from(n), pos)),
    }
}

/// SQL92 output-name rule: a bare identifier that is not an input column names a select-list item.
/// Several different items with that name -> 42702.
fn output_name_target(b: &mut Binder, tl: &[TargetEntry], e: &Expr, pos: Option<u32>) -> Result<Option<usize>> {
    let id = match e {
        Expr::Identifier(id) => id,
        _ => return Ok(None),
    };
    let name = b.ident(id);
    if resolve_column(b, &[], &name, pos, false)?.is_some() {
        return Ok(None);
    }
    let mut found: Option<usize> = None;
    for (i, t) in tl.iter().enumerate() {
        if t.junk || t.name != name {
            continue;
        }
        match found {
            None => found = Some(i),
            Some(j) => {
                if tl[j].expr != t.expr {
                    return Err(diag::output_name_ambiguous("GROUP BY", &name, pos));
                }
            }
        }
    }
    if let Some(i) = found {
        check_group_target(&tl[i].expr, pos)?;
    }
    Ok(found)
}

/// A select-list item used by ordinal / output-name grouping must not contain aggregates or windows
/// (the expression form gets the same errors from `bind_expr` with `ExprCtx::group_by()`).
fn check_group_target(e: &BoundExpr, pos: Option<u32>) -> Result<()> {
    if e.contains_local_agg() {
        return Err(diag::agg_not_allowed(ExprClause::GroupBy, pos));
    }
    if e.contains_window() {
        return Err(diag::window_not_allowed(ExprClause::GroupBy, pos));
    }
    Ok(())
}

/// PG `ColumnsCheck` for grouping: the type must have an equality operator (42883).
fn ensure_equality(b: &Binder, ty: TypeId, pos: Option<u32>) -> Result<()> {
    let candidates = ops::builtin_operator_candidates("=")?;
    if ops::resolve_operator(b.env as &dyn TypeEnv, "=", Some(ty.oid), ty.oid, &candidates).is_ok() {
        return Ok(());
    }
    let mut err = PgError::new(
        SqlState::UNDEFINED_FUNCTION,
        format!("could not identify an equality operator for type {}", b.type_name(ty)),
    );
    err.position = pos;
    Err(err)
}

// ------------------------------------------------------------------------------------------------
// Set expansion (pure)
// ------------------------------------------------------------------------------------------------

/// ROLLUP over `units`: every prefix from the full list down to the empty set (PG `GROUPING_SET_ROLLUP`).
fn rollup_alternatives(units: &[Vec<usize>]) -> Vec<Vec<usize>> {
    let mut out: Vec<Vec<usize>> = Vec::with_capacity(units.len() + 1);
    for n in (1..=units.len()).rev() {
        out.push(units[..n].iter().flatten().copied().collect());
    }
    out.push(Vec::new());
    out
}

/// CUBE over `units`: all subsets in binary counting order (unit `j` is bit `j`).
fn cube_alternatives(units: &[Vec<usize>]) -> Result<Vec<Vec<usize>>> {
    if units.len() > MAX_CUBE_UNITS {
        return Err(too_many_sets());
    }
    let mut out: Vec<Vec<usize>> = Vec::with_capacity(1usize << units.len());
    for mask in 0..(1usize << units.len()) {
        let mut set: Vec<usize> = Vec::new();
        for (j, unit) in units.iter().enumerate() {
            if mask & (1usize << j) != 0 {
                set.extend(unit.iter().copied());
            }
        }
        out.push(set);
    }
    Ok(out)
}

/// Cartesian product of the items' alternatives: later items vary fastest, and each combined set keeps
/// its first occurrence of every position (PG `expand_grouping_sets`). Sets are never merged or dropped,
/// so the number of sets is the product of the alternative counts.
fn product(items: Vec<Vec<Vec<usize>>>) -> Result<Vec<Vec<usize>>> {
    let mut result: Vec<Vec<usize>> = vec![Vec::new()];
    for alternatives in items {
        if result.len().saturating_mul(alternatives.len()) > MAX_GROUPING_SETS {
            return Err(too_many_sets());
        }
        let mut next: Vec<Vec<usize>> = Vec::new();
        for left in &result {
            for right in &alternatives {
                let mut set = left.clone();
                for &x in right {
                    if !set.contains(&x) {
                        set.push(x);
                    }
                }
                next.push(set);
            }
        }
        result = next;
    }
    Ok(result)
}

fn too_many_sets() -> PgError {
    PgError::new(SqlState::STATEMENT_TOO_COMPLEX, "too many grouping sets present (maximum 4096)")
}

// ------------------------------------------------------------------------------------------------
// GROUPING() resolution
// ------------------------------------------------------------------------------------------------

fn resolve_grouping_node(e: &mut BoundExpr, map: &[Option<usize>]) -> Result<()> {
    if let ExprKind::Grouping(grouping) = &mut e.kind {
        for arg in grouping.args.iter_mut() {
            let mapped = match map.get(*arg) {
                Some(m) => *m,
                None => return Err(PgError::internal("GROUPING argument index out of range")),
            };
            match mapped {
                Some(k) => *arg = k,
                None => return Err(grouping_argument_error()),
            }
        }
        return Ok(());
    }
    let mut failure: Option<PgError> = None;
    e.for_each_child_mut(&mut |child| {
        if failure.is_none() {
            if let Err(err) = resolve_grouping_node(child, map) {
                failure = Some(err);
            }
        }
    });
    match failure {
        Some(err) => Err(err),
        None => Ok(()),
    }
}

/// PG `finalize_grouping_exprs` raises this with ERRCODE_GROUPING_ERROR (42803), see the module docs.
fn grouping_argument_error() -> PgError {
    PgError::new(
        SqlState::GROUPING_ERROR,
        "arguments to GROUPING must be grouping expressions of the associated query level",
    )
}

// ------------------------------------------------------------------------------------------------
// The ungrouped-column walker (PG `substitute_grouped_columns_mutator`)
// ------------------------------------------------------------------------------------------------

/// State of one `check_grouping` run. `s` in the walk functions is the sub-select depth of the node
/// relative to the checked block (0 = the block's own expressions).
struct Checker<'a> {
    env: &'a CatalogEnv,
    /// Range table of the checked block.
    rtable: &'a [RangeEntry],
    /// Every grouping expression (all sets), matched whole at depth 0.
    group_exprs: Vec<&'a BoundExpr>,
    /// `(rt_index, attnum)` of plain columns grouped in every set (functional dependency).
    common_cols: Vec<(u32, i16)>,
    /// Range entries already proven functionally dependent.
    fd_proven: Vec<u32>,
}

impl<'a> Checker<'a> {
    fn expr(&mut self, e: &BoundExpr, s: u32) -> Result<()> {
        if s == 0 && self.group_exprs.iter().any(|g| *g == e) {
            return Ok(());
        }
        match &e.kind {
            ExprKind::Const(_) | ExprKind::Param(_) | ExprKind::Grouping(_) => return Ok(()),
            ExprKind::Column(c) => {
                if c.levels_up == s {
                    return self.column(*c, s);
                }
                return Ok(());
            }
            ExprKind::Aggref(agg) => {
                if agg.levels_up == s {
                    // Aggregate of the checked block: its arguments are fine, but direct arguments of an
                    // ordered-set aggregate are checked as ordinary expressions (PG).
                    for d in &agg.direct_args {
                        self.expr(d, s)?;
                    }
                    return Ok(());
                }
                if agg.levels_up > s {
                    return Ok(());
                }
            }
            _ => {}
        }
        let mut kids: Vec<&BoundExpr> = Vec::new();
        e.for_each_child(&mut |child| kids.push(child));
        for kid in kids {
            self.expr(kid, s)?;
        }
        if let ExprKind::SubLink(sub) = &e.kind {
            self.query(&sub.subquery, s + 1)?;
        }
        Ok(())
    }

    /// A column of the checked block (`levels_up == s`) that is neither grouped nor functionally dependent.
    fn column(&mut self, c: ColumnRef, s: u32) -> Result<()> {
        let grouped = self.group_exprs.iter().any(|g| {
            matches!(&g.kind, ExprKind::Column(gc) if gc.levels_up == 0 && gc.rt_index == c.rt_index && gc.attnum == c.attnum)
        });
        if grouped || self.fd_grouped(c.rt_index) {
            return Ok(());
        }
        let (rel, col) = self.label(&c)?;
        let pos = if c.pos == 0 { None } else { Some(c.pos) };
        Err(if s == 0 {
            diag::ungrouped_column(&rel, &col, pos)
        } else {
            diag::ungrouped_column_in_subquery(&rel, &col, pos)
        })
    }

    /// PG `check_functional_grouping`: every PRIMARY KEY column of the table is grouped in every set.
    fn fd_grouped(&mut self, rt: u32) -> bool {
        if self.fd_proven.contains(&rt) {
            return true;
        }
        let oid = match self.entry(rt) {
            Ok(RangeEntry { kind: RangeKind::Relation { oid, .. }, .. }) => *oid,
            _ => return false,
        };
        let env: &CatalogEnv = self.env;
        let pk = match env.snapshot().primary_key(oid) {
            Some(constraint) => constraint,
            None => return false,
        };
        let ok = !pk.columns.is_empty() && pk.columns.iter().all(|k| self.common_cols.contains(&(rt, *k)));
        if ok {
            self.fd_proven.push(rt);
        }
        ok
    }

    fn entry(&self, rt: u32) -> Result<&'a RangeEntry> {
        let rtable: &'a [RangeEntry] = self.rtable;
        rt.checked_sub(1)
            .and_then(|i| rtable.get(i as usize))
            .ok_or_else(|| PgError::internal("grouping check: range table index out of range"))
    }

    /// `(alias, column)` names used in the 42803 message (PG `get_rte_attribute_name`).
    fn label(&self, c: &ColumnRef) -> Result<(String, String)> {
        let entry = self.entry(c.rt_index)?;
        let column = match c.attnum {
            0 => "*".to_string(),
            n if n < 0 => system_attr_name(n).to_string(),
            n => entry
                .columns
                .get((n - 1) as usize)
                .map(|rc| rc.name.clone())
                .ok_or_else(|| PgError::internal("grouping check: column index out of range"))?,
        };
        Ok((entry.alias.clone(), column))
    }

    fn query(&mut self, q: &BoundQuery, d: u32) -> Result<()> {
        for cte in &q.ctes {
            if let CteBody::Query(body) = &cte.body {
                self.query(body, d + 1)?;
            }
        }
        match &q.body {
            QueryBody::Select(sel) => self.select(sel, d)?,
            QueryBody::Values(values) => {
                for row in &values.rows {
                    for cell in row {
                        self.expr(cell, d)?;
                    }
                }
            }
            QueryBody::SetOp(op) => {
                self.query(&op.left, d + 1)?;
                self.query(&op.right, d + 1)?;
            }
        }
        if let Some(limit) = &q.limit {
            self.expr(limit, d)?;
        }
        if let Some(offset) = &q.offset {
            self.expr(offset, d)?;
        }
        Ok(())
    }

    fn select(&mut self, sel: &BoundSelect, d: u32) -> Result<()> {
        for entry in &sel.range_table {
            match &entry.kind {
                RangeKind::Subquery { query } => self.query(query, d + 1)?,
                RangeKind::Function { funcs, .. } => {
                    for func in funcs {
                        self.expr(&func.expr, d)?;
                    }
                }
                RangeKind::Values { values } => {
                    for row in &values.rows {
                        for cell in row {
                            self.expr(cell, d)?;
                        }
                    }
                }
                RangeKind::Relation { .. } | RangeKind::Cte { .. } => {}
            }
        }
        for item in &sel.from {
            self.from_item(item, d)?;
        }
        if let Some(where_) = &sel.where_ {
            self.expr(where_, d)?;
        }
        for entry in &sel.target_list {
            self.expr(&entry.expr, d)?;
        }
        if let Some(having) = &sel.having {
            self.expr(having, d)?;
        }
        self.windows(&sel.windows, d)
    }

    fn from_item(&mut self, item: &FromItem, d: u32) -> Result<()> {
        if let FromItem::Join(join) = item {
            self.from_item(&join.left, d)?;
            self.from_item(&join.right, d)?;
            if let Some(quals) = &join.quals {
                self.expr(quals, d)?;
            }
        }
        Ok(())
    }

    fn windows(&mut self, windows: &[WindowDef], d: u32) -> Result<()> {
        for w in windows {
            for p in &w.partition_by {
                self.expr(p, d)?;
            }
            for o in &w.order_by {
                self.expr(&o.expr, d)?;
            }
            self.frame_bound(&w.start, d)?;
            self.frame_bound(&w.end, d)?;
        }
        Ok(())
    }

    fn frame_bound(&mut self, bound: &FrameBound, d: u32) -> Result<()> {
        match bound {
            FrameBound::Preceding(e) | FrameBound::Following(e) => self.expr(e, d),
            _ => Ok(()),
        }
    }
}

/// `pg_attribute` names of the system columns (`ColumnRef::attnum` -1..-6).
fn system_attr_name(attnum: i16) -> &'static str {
    match attnum {
        -1 => "ctid",
        -2 => "xmin",
        -3 => "cmin",
        -4 => "xmax",
        -5 => "cmax",
        -6 => "tableoid",
        _ => "?",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sets(list: &[&[usize]]) -> Vec<Vec<usize>> {
        list.iter().map(|s| s.to_vec()).collect()
    }

    #[test]
    fn rollup_is_descending_prefixes() {
        let units = vec![vec![0], vec![1]];
        assert_eq!(rollup_alternatives(&units), sets(&[&[0, 1], &[0], &[]]));
    }

    #[test]
    fn rollup_keeps_parenthesized_units_together() {
        let units = vec![vec![0, 1], vec![2]];
        assert_eq!(rollup_alternatives(&units), sets(&[&[0, 1, 2], &[0, 1], &[]]));
    }

    #[test]
    fn cube_is_binary_counting_order() -> Result<()> {
        let units = vec![vec![0], vec![1]];
        assert_eq!(cube_alternatives(&units)?, sets(&[&[], &[0], &[1], &[0, 1]]));
        Ok(())
    }

    #[test]
    fn cube_over_twelve_units_is_too_many_sets() {
        let units = vec![vec![0usize]; 13];
        assert!(cube_alternatives(&units).is_err());
    }

    #[test]
    fn product_later_items_vary_fastest() -> Result<()> {
        let items = vec![sets(&[&[0], &[]]), sets(&[&[1], &[1, 2]])];
        assert_eq!(product(items)?, sets(&[&[0, 1], &[0, 1, 2], &[1], &[1, 2]]));
        Ok(())
    }

    #[test]
    fn product_dedupes_within_a_set_but_keeps_set_count() -> Result<()> {
        let items = vec![sets(&[&[0]]), sets(&[&[0], &[0]])];
        assert_eq!(product(items)?, sets(&[&[0], &[0]]));
        Ok(())
    }

    #[test]
    fn product_over_limit_is_too_many_sets() {
        // 65 x 65 = 4225 sets, one more than the 4096 limit.
        let wide: Vec<Vec<usize>> = (0..65).map(|i| vec![i]).collect();
        assert!(product(vec![wide.clone(), wide]).is_err());
    }

    #[test]
    fn empty_group_by_item_is_one_empty_set() -> Result<()> {
        assert_eq!(product(vec![vec![Vec::new()]])?, sets(&[&[]]));
        Ok(())
    }

    #[test]
    fn system_column_names() {
        assert_eq!(system_attr_name(-1), "ctid");
        assert_eq!(system_attr_name(-6), "tableoid");
    }

    // ---- end-to-end rules: need the parser and the select binder (SQL-01, SQL-04, SQL-10, SQL-16) ----

    fn bind_query_sql(sql: &str) -> Result<BoundQuery> {
        use crate::ir::BoundStatement;
        use crate::{bind, parse, BindContext, BindSession};
        use pgarbor_types::context::IoConfig;

        let (_kv, env) = pgarbor_catalog::testing::bootstrapped()?;
        let parsed = parse(sql)?;
        let stmt = match parsed.first() {
            Some(stmt) => stmt,
            None => return Err(PgError::internal("test statement missing")),
        };
        let ctx = BindContext {
            env: &env,
            session: BindSession { database: "pgarbor".to_string() },
            io_config: IoConfig::default(),
            statement_timestamp: 0,
            declared_params: Vec::new(),
        };
        match bind(stmt, &ctx)?.statement {
            BoundStatement::Query(q) => Ok(*q),
            _ => Err(PgError::internal("test statement is not a query")),
        }
    }

    #[test]
    #[ignore = "needs SQL-01, SQL-04, SQL-10, SQL-16"]
    fn group_by_position_out_of_range_is_42p10() {
        match bind_query_sql("select 1 group by 5") {
            Err(e) => assert_eq!(e.message, "GROUP BY position 5 is not in select list"),
            Ok(_) => panic!("expected 42P10"),
        }
    }

    #[test]
    #[ignore = "needs SQL-01, SQL-04, SQL-10, SQL-16"]
    fn aggregate_in_group_by_ordinal_is_42803() {
        assert!(bind_query_sql("select count(*) group by 1").is_err());
    }

    #[test]
    #[ignore = "needs SQL-01, SQL-04, SQL-10, SQL-16"]
    fn empty_grouping_set_is_single_empty_set() -> Result<()> {
        let q = bind_query_sql("select 1 group by ()")?;
        match &q.body {
            QueryBody::Select(sel) => {
                assert!(sel.group_by.exprs.is_empty());
                assert_eq!(sel.group_by.sets, Some(vec![Vec::new()]));
            }
            _ => return Err(PgError::internal("expected a select body")),
        }
        Ok(())
    }

    #[test]
    #[ignore = "needs SQL-01, SQL-04, SQL-10, SQL-16"]
    fn rollup_ordinals_expand_to_prefix_sets() -> Result<()> {
        let q = bind_query_sql("select 1, 2 group by rollup(1, 2)")?;
        match &q.body {
            QueryBody::Select(sel) => {
                assert_eq!(sel.group_by.exprs, vec![0, 1]);
                assert_eq!(sel.group_by.sets, Some(sets(&[&[0, 1], &[0], &[]])));
            }
            _ => return Err(PgError::internal("expected a select body")),
        }
        Ok(())
    }
}
