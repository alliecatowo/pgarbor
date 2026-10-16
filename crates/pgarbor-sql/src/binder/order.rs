//! ORDER BY, DISTINCT / DISTINCT ON, LIMIT / OFFSET / FETCH (SQL-21).
//! PG references: `parse_clause.c` (`transformSortClause`, `transformDistinctClause`,
//! `transformDistinctOnClause`, `findTargetlistEntrySQL92`, `findTargetlistEntrySQL99`,
//! `addTargetToSortList`, `transformLimitClause`, `get_sortgroupref_tle`), `parse_oper.c`
//! (`get_ordering_op_properties`), `analyze.c`.
//!
//! ## ORDER BY item resolution (`bind_order_by`, SELECT blocks)
//! For each `OrderByExpr { expr, options: { asc, nulls_first }, with_fill }`:
//! 1. integer literal `n` -> target entry n (1-based, among NON-junk entries): out of range ->
//!    42P10 `ORDER BY position 3 is not in select list` (`diag::position_not_in_select_list`);
//!    a non-integer constant (`ORDER BY 'a'`, `ORDER BY 1.5`) is 42601 `non-integer constant in ORDER BY`
//!    (same for GROUP BY: `non-integer constant in GROUP BY`; DISTINCT ON: `non-integer constant in DISTINCT ON`).
//! 2. bare identifier (no qualifier): if it matches an OUTPUT column name -> that entry (SQL92 rule,
//!    output names win over input columns in ORDER BY; two outputs with that name ->
//!    42702 `ORDER BY "x" is ambiguous`, unless they are the same expression);
//! 3. otherwise bind the expression (`ExprCtx::order_by()`) and look for a structurally equal target
//!    entry (reuse); if none, append a JUNK target entry — except for `SELECT DISTINCT`, where it is
//!    42P10 `for SELECT DISTINCT, ORDER BY expressions must appear in select list`
//!    (`diag::distinct_order_by`).
//! Direction: `ASC`/default ascending; `DESC`; `USING <op>` (not in sqlparser's PG dialect? if exposed,
//! resolve `<`/`>`-family operator and store `using_op`); default null ordering: `NULLS LAST` for ASC,
//! `NULLS FIRST` for DESC unless explicit. Ordering requires an ordering operator: type without
//! `<` -> 42883 `could not identify an ordering operator for type X`. Duplicate keys are kept
//! (PG removes redundant ones later; keep the first occurrence and drop later duplicates of the same
//! (index, direction)).
//! Positions for errors: the item expression start.
//!
//! ## DISTINCT ([`bind_distinct`])
//! `Distinct::Distinct` -> `DistinctSpec::All` (needs an equality operator on every output column;
//! 42883 `could not identify an equality operator for type X`); `Distinct::On(exprs)` -> each expression
//! resolved like an ORDER BY item (ordinal, output name, expression -> maybe junk entry); the DISTINCT ON
//! expressions must match the LEFTMOST ORDER BY expressions: 42P10 `SELECT DISTINCT ON expressions must
//! match initial ORDER BY expressions` (`diag::distinct_on_mismatch`). When ORDER BY is absent the
//! DISTINCT ON keys become the sort keys (planner). The distinct step runs BEFORE sort-key resolution
//! in PG (`transformDistinctOnClause` after `transformSortClause`); implement accordingly.
//!
//! ## Set operation / VALUES ORDER BY ([`bind_order_by_output`])
//! Only output-column references: ordinal or bare/qualified output name; any other expression ->
//! 0A000 `invalid UNION/INTERSECT/EXCEPT ORDER BY clause` DETAIL `Only result column names can be used,
//! not expressions or functions.` HINT `Add the expression/function to every SELECT, or move the UNION into
//! a FROM clause.` (`diag::setop_order_by_expression`).
//!
//! ## LIMIT / OFFSET / FETCH ([`bind_limit_clause`])
//! `LimitClause::LimitOffset { limit, offset, .. }`, `Query::fetch: Fetch { with_ties, percent, quantity }`.
//! Expressions bound with `ExprCtx::limit()` (no columns/aggregates: 42803/42P10 `argument of LIMIT must
//! not contain variables`), coerced to `int8` (`coerce_to_specific(.., "LIMIT")`; `OFFSET` -> "OFFSET";
//! `FETCH` -> "FETCH"). `LIMIT ALL` / `LIMIT NULL` = no limit. A negative value is NOT a bind-time error:
//! PG raises 2201W `LIMIT must not be negative` / 2201X `OFFSET must not be negative` (`diag::negative_limit`,
//! no position) when the executor starts the node, even for constants (oracle: `select 1 limit -1`). `FETCH FIRST n ROWS
//! ONLY|WITH TIES`: `FETCH FIRST ROW ONLY` = 1; `percent` -> 0A000; `FETCH` together with `LIMIT` -> 42601 `syntax error at or near "fetch"` (oracle). Returns
//! `(limit, offset, with_ties)`.
use pgarbor_common::{Oid, PgError, Result, SqlState};
use pgarbor_types::{oids, Datum, TypeId};
use sqlparser::ast::{
    Distinct, Expr, Fetch, LimitClause, ObjectName, ObjectNamePart, OrderByExpr, OrderBySort, UnaryOperator, Value,
};

use super::context::{Binder, BoundArg, ExprCtx};
use super::coerce::{coerce_to_specific, resolve_unknown_output};
use super::expr::bind_expr;
use super::expr_ops::resolve_named_op;
use crate::diag::{self, ExprClause};
use crate::ir::{
    BoundExpr, BoundQuery, BoundSelect, CoercionForm, DistinctSpec, ExprKind, OpRef, OutputColumn, QueryBody, RangeKind,
    SetOpBody, SortKey, TargetEntry,
};

/// Integer-constant view of a sort / distinct item (PG `A_Const` with `Integer` or non-`Integer` value).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum IntConst {
    /// An `Integer` node (int4 range, sign folded in).
    Integer(i64),
    /// A constant that is not an `Integer` node (string, NULL, boolean, decimal, out-of-int4 integer).
    NonInteger,
}

/// ORDER BY of a SELECT block (module docs). May append junk entries to `target_list`.
/// `distinct` is the block's DISTINCT spec (`DistinctSpec::All` forbids junk entries).
pub fn bind_order_by(
    b: &mut Binder,
    order_by: &[OrderByExpr],
    target_list: &mut Vec<TargetEntry>,
    distinct: &DistinctSpec,
) -> Result<Vec<SortKey>> {
    let allow_junk = !matches!(distinct, DistinctSpec::All);
    let mut keys: Vec<SortKey> = Vec::with_capacity(order_by.len());
    for item in order_by {
        let pos = b.pos(&item.expr);
        if item.with_fill.is_some() {
            return Err(diag::unsupported_construct("ORDER BY ... WITH FILL", pos));
        }
        let idx = resolve_sort_item(b, &item.expr, target_list, "ORDER BY", allow_junk)?;
        let ty = sort_type_of(b, target_list, idx, pos)?;
        let key = make_sort_key(b, idx, ty, item, pos)?;
        push_sort_key(&mut keys, key);
    }
    Ok(keys)
}

/// DISTINCT / DISTINCT ON (module docs). `sort_keys` are the already bound ORDER BY keys (for the
/// prefix check). May append junk entries for DISTINCT ON expressions.
pub fn bind_distinct(
    b: &mut Binder,
    distinct: Option<&Distinct>,
    target_list: &mut Vec<TargetEntry>,
    sort_keys: &[SortKey],
) -> Result<DistinctSpec> {
    match distinct {
        None | Some(Distinct::All) => Ok(DistinctSpec::None),
        Some(Distinct::Distinct) => {
            if !target_list.iter().any(|t| !t.junk) {
                return Err(diag::syntax("SELECT DISTINCT must have at least one column", None));
            }
            for i in 0..target_list.len() {
                if target_list[i].junk {
                    continue;
                }
                let ty = sort_type_of(b, target_list, i, None)?;
                check_equality_op(b, ty, None)?;
            }
            Ok(DistinctSpec::All)
        }
        Some(Distinct::On(exprs)) => {
            let mut on: Vec<usize> = Vec::with_capacity(exprs.len());
            let mut on_pos: Vec<Option<u32>> = Vec::with_capacity(exprs.len());
            for e in exprs {
                let pos = b.pos(e);
                let idx = resolve_sort_item(b, e, target_list, "DISTINCT ON", true)?;
                let ty = sort_type_of(b, target_list, idx, pos)?;
                check_equality_op(b, ty, pos)?;
                on.push(idx);
                on_pos.push(pos);
            }
            let sort_targets: Vec<usize> = sort_keys.iter().map(|k| k.target_idx).collect();
            if let Err(j) = check_distinct_on_prefix(&on, &sort_targets) {
                return Err(diag::distinct_on_mismatch(on_pos[j]));
            }
            let mut result: Vec<usize> = Vec::with_capacity(on.len());
            for &idx in &on {
                if !result.contains(&idx) {
                    result.push(idx);
                }
            }
            Ok(DistinctSpec::On(result))
        }
    }
}

/// ORDER BY for VALUES / set-operation bodies (module docs).
pub fn bind_order_by_output(b: &mut Binder, order_by: &[OrderByExpr], output: &[OutputColumn]) -> Result<Vec<SortKey>> {
    let mut keys: Vec<SortKey> = Vec::with_capacity(order_by.len());
    for item in order_by {
        let pos = b.pos(&item.expr);
        if item.with_fill.is_some() {
            return Err(diag::unsupported_construct("ORDER BY ... WITH FILL", pos));
        }
        let inner = strip_nested(&item.expr);
        let idx: usize = match int_constant(inner) {
            Some(IntConst::Integer(n)) => {
                if n >= 1 && (n as usize) <= output.len() {
                    (n - 1) as usize
                } else {
                    return Err(diag::position_not_in_select_list("ORDER BY", n, b.pos(inner)));
                }
            }
            Some(IntConst::NonInteger) => return Err(non_integer_constant("ORDER BY", b.pos(inner))),
            None => output_index_for_name(b, inner, output, pos)?,
        };
        let ty = output[idx].ty;
        let key = make_sort_key(b, idx, ty, item, pos)?;
        push_sort_key(&mut keys, key);
    }
    Ok(keys)
}

/// Collation state of one expression, merged across the arms of a set operation (PG
/// `merge_collation_state`, ordered Nothing < Implicit < Conflict < Explicit). `Unsure` marks a
/// shape this check does not model; it never raises an error.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum CollState {
    Nothing,
    Implicit(Oid),
    Conflict,
    Explicit(Oid),
    Unsure,
}

/// PG `merge_collation_state` for two sibling inputs.
fn merge_collation(a: CollState, b: CollState) -> CollState {
    match (a, b) {
        (CollState::Unsure, _) | (_, CollState::Unsure) => CollState::Unsure,
        (CollState::Nothing, x) | (x, CollState::Nothing) => x,
        (CollState::Explicit(x), CollState::Explicit(y)) => {
            if x == y {
                CollState::Explicit(x)
            } else {
                CollState::Unsure
            }
        }
        (CollState::Explicit(x), _) | (_, CollState::Explicit(x)) => CollState::Explicit(x),
        (CollState::Conflict, _) | (_, CollState::Conflict) => CollState::Conflict,
        (CollState::Implicit(x), CollState::Implicit(y)) => {
            if x == y {
                CollState::Implicit(x)
            } else if x == oids::DEFAULT_COLLATION {
                CollState::Implicit(y)
            } else if y == oids::DEFAULT_COLLATION {
                CollState::Implicit(x)
            } else {
                CollState::Conflict
            }
        }
    }
}

/// Collation of an expression of a SELECT block. Column references take the catalog collation of
/// the table column (or of the subquery's output column); `COLLATE` is explicit. Other shapes
/// contribute nothing, which can only hide a conflict, never invent one.
fn expr_collation(b: &Binder, sel: &BoundSelect, e: &BoundExpr) -> CollState {
    match &e.kind {
        ExprKind::Collate(c) => CollState::Explicit(c.collation),
        ExprKind::Column(cr) if cr.levels_up == 0 && cr.attnum > 0 => {
            let Some(entry) = (cr.rt_index as usize).checked_sub(1).and_then(|k| sel.range_table.get(k)) else {
                return CollState::Unsure;
            };
            let col = (cr.attnum as usize) - 1;
            match &entry.kind {
                RangeKind::Relation { oid, .. } => match b.env.snapshot().relation(*oid).and_then(|r| r.columns.get(col)) {
                    Some(c) if c.collation == 0 => CollState::Nothing,
                    Some(c) => CollState::Implicit(c.collation),
                    None => CollState::Unsure,
                },
                RangeKind::Subquery { query } => query_column_collation(b, query, col),
                _ => CollState::Nothing,
            }
        }
        _ => CollState::Nothing,
    }
}

/// Collation state of output column `i` of a bound query (visible entries only).
fn query_column_collation(b: &Binder, q: &BoundQuery, i: usize) -> CollState {
    match &q.body {
        QueryBody::Select(sel) => match sel.target_list.iter().filter(|t| !t.junk).nth(i) {
            Some(t) => expr_collation(b, sel, &t.expr),
            None => CollState::Unsure,
        },
        QueryBody::SetOp(s) => setop_column_collation(b, s, i),
        QueryBody::Values(_) => CollState::Nothing,
    }
}

/// Collation state of output column `i` of a set operation: the merge over both arms.
fn setop_column_collation(b: &Binder, s: &SetOpBody, i: usize) -> CollState {
    merge_collation(query_column_collation(b, &s.left, i), query_column_collation(b, &s.right, i))
}

/// PG `select_common_collation` for set-operation columns: a sort key on a column whose arms have
/// conflicting implicit collations (no explicit COLLATE wins) has no determinable collation, and
/// the sort fails with 42P22 `could not determine which collation to use for string comparison`.
/// PG raises it when the sort starts; this check raises it when the ORDER BY is bound.
pub fn check_setop_sort_collations(b: &Binder, body: &QueryBody, order_by: &[SortKey]) -> Result<()> {
    let QueryBody::SetOp(s) = body else {
        return Ok(());
    };
    for key in order_by {
        if setop_column_collation(b, s, key.target_idx) == CollState::Conflict {
            return Err(PgError::new(
                SqlState::INDETERMINATE_COLLATION,
                "could not determine which collation to use for string comparison",
            )
            .with_hint("Use the COLLATE clause to set the collation explicitly."));
        }
    }
    Ok(())
}

/// LIMIT / OFFSET / FETCH (module docs).
pub fn bind_limit_clause(
    b: &mut Binder,
    limit_clause: Option<&LimitClause>,
    fetch: Option<&Fetch>,
) -> Result<(Option<BoundExpr>, Option<BoundExpr>, bool)> {
    let mut limit: Option<BoundExpr> = None;
    let mut offset: Option<BoundExpr> = None;
    if let Some(clause) = limit_clause {
        match clause {
            LimitClause::LimitOffset { limit: limit_expr, offset: offset_clause, limit_by } => {
                if let Some(first) = limit_by.first() {
                    return Err(diag::unsupported_construct("LIMIT ... BY", b.pos(first)));
                }
                if limit_expr.is_some() && fetch.is_some() {
                    return Err(diag::syntax_error_near("fetch", None));
                }
                if let Some(off) = offset_clause {
                    let ctx = ExprCtx { clause: ExprClause::Offset, ..ExprCtx::limit() };
                    offset = bind_count_expr(b, &off.value, ctx, "OFFSET")?;
                }
                if let Some(e) = limit_expr {
                    limit = bind_count_expr(b, e, ExprCtx::limit(), "LIMIT")?;
                }
            }
            LimitClause::OffsetCommaLimit { .. } => return Err(diag::syntax_error_near(",", None)),
        }
    }
    if let Some(f) = fetch {
        if f.percent {
            return Err(diag::unsupported_construct("FETCH FIRST ... PERCENT", None));
        }
        let fetch_limit = match &f.quantity {
            None => Some(BoundExpr::constant(Datum::Int8(1), TypeId::INT8)),
            Some(q) => {
                if f.with_ties && is_null_literal(q) {
                    return Err(with_ties_null_error());
                }
                bind_count_expr(b, q, ExprCtx::limit(), "FETCH")?
            }
        };
        return Ok((fetch_limit, offset, f.with_ties));
    }
    Ok((limit, offset, false))
}

// ---------------------------------------------------------------------------------------------
// Shared helpers
// ---------------------------------------------------------------------------------------------

/// Strip parentheses (PG's grammar returns the inner node for `( a_expr )`).
fn strip_nested(e: &Expr) -> &Expr {
    match e {
        Expr::Nested(inner) => strip_nested(inner),
        _ => e,
    }
}

/// Strip implicit cast nodes the binder inserted (PG `strip_implicit_coercions`).
fn strip_implicit(e: &BoundExpr) -> &BoundExpr {
    match &e.kind {
        ExprKind::Cast(c) if c.form == CoercionForm::Implicit => strip_implicit(&c.arg),
        _ => e,
    }
}

/// `NULL` literal written directly (after parentheses).
fn is_null_literal(e: &Expr) -> bool {
    matches!(strip_nested(e), Expr::Value(v) if matches!(v.value, Value::Null))
}

/// PG `A_Const` view of an expression: integer constants (with a folded sign), other constants,
/// `None` for anything that is not a constant (placeholders are expressions, not constants).
fn int_constant(e: &Expr) -> Option<IntConst> {
    match e {
        Expr::Value(v) => value_int_constant(&v.value, false),
        Expr::UnaryOp { op: UnaryOperator::Minus, expr } => match &**expr {
            Expr::Value(v) => value_int_constant(&v.value, true),
            _ => None,
        },
        _ => None,
    }
}

fn value_int_constant(v: &Value, negative: bool) -> Option<IntConst> {
    match v {
        Value::Number(..) => Some(parse_int_text(&v.to_string(), negative)),
        Value::Placeholder(_) => None,
        // `-'a'` is an operator application in PG, not a constant.
        _ if negative => None,
        _ => Some(IntConst::NonInteger),
    }
}

/// Decimal text of a numeric literal -> `Integer` when it fits PG's int4 `Integer` node (magnitude
/// `2147483648` allowed only when negative), else `NonInteger` (PG makes it a `Float` node).
fn parse_int_text(text: &str, negative: bool) -> IntConst {
    let (neg, rest) = match text.strip_prefix('-') {
        Some(r) => (!negative, r),
        None => (negative, text),
    };
    let digits: String = rest.chars().filter(|c| *c != '_').collect();
    if digits.is_empty() || !digits.bytes().all(|c| c.is_ascii_digit()) {
        return IntConst::NonInteger;
    }
    let limit: u64 = if neg { 2_147_483_648 } else { 2_147_483_647 };
    match digits.parse::<u64>() {
        Ok(n) if n <= limit => IntConst::Integer(if neg { -(n as i64) } else { n as i64 }),
        _ => IntConst::NonInteger,
    }
}

/// Default NULL placement: `NULLS FIRST` for descending keys, `NULLS LAST` otherwise, unless explicit.
fn nulls_first_for(explicit: Option<bool>, descending: bool) -> bool {
    explicit.unwrap_or(descending)
}

/// Resolve one ORDER BY / DISTINCT ON item to a target-list index (module docs, PG
/// `findTargetlistEntrySQL92` then `findTargetlistEntrySQL99`). Junk entries are appended when
/// `allow_junk` is set; otherwise a new expression is 42P10 `for SELECT DISTINCT, ...`.
fn resolve_sort_item(
    b: &mut Binder,
    expr: &Expr,
    target_list: &mut Vec<TargetEntry>,
    clause: &'static str,
    allow_junk: bool,
) -> Result<usize> {
    let pos = b.pos(expr);
    let inner = strip_nested(expr);
    if let Some(ic) = int_constant(inner) {
        return match ic {
            IntConst::Integer(n) => {
                let found = if n >= 1 {
                    target_list.iter().enumerate().filter(|(_, t)| !t.junk).nth((n - 1) as usize).map(|(i, _)| i)
                } else {
                    None
                };
                match found {
                    Some(i) => Ok(i),
                    None => Err(diag::position_not_in_select_list(clause, n, b.pos(inner))),
                }
            }
            IntConst::NonInteger => Err(non_integer_constant(clause, b.pos(inner))),
        };
    }
    if let Expr::Identifier(id) = inner {
        let name = b.ident(id);
        let matches: Vec<usize> =
            target_list.iter().enumerate().filter(|(_, t)| !t.junk && t.name == name).map(|(i, _)| i).collect();
        if let Some(&first) = matches.first() {
            for &m in &matches[1..] {
                if target_list[m].expr != target_list[first].expr {
                    return Err(diag::output_name_ambiguous(clause, &name, pos));
                }
            }
            return Ok(first);
        }
    }
    let ec = ExprCtx { clause: ExprClause::Other(clause), ..ExprCtx::order_by() };
    let bound = bind_expr(b, expr, ec)?;
    if let Some(i) = target_list.iter().position(|t| t.expr == bound || strip_implicit(&t.expr) == &bound) {
        return Ok(i);
    }
    if !allow_junk {
        return Err(diag::distinct_order_by(pos));
    }
    target_list.push(TargetEntry { expr: bound, name: "?column?".to_string(), junk: true, origin: None });
    Ok(target_list.len() - 1)
}

/// Type of a target entry used for sorting; an `unknown` entry becomes `text` in place (PG
/// `addTargetToSortList` / `addTargetToGroupList`).
fn sort_type_of(b: &mut Binder, target_list: &mut Vec<TargetEntry>, idx: usize, pos: Option<u32>) -> Result<TypeId> {
    if target_list[idx].expr.ty.oid != pgarbor_types::oids::UNKNOWN {
        return Ok(target_list[idx].expr.ty);
    }
    let expr = target_list[idx].expr.clone();
    let coerced = resolve_unknown_output(b, &BoundArg::new(expr, pos))?;
    let ty = coerced.ty;
    target_list[idx].expr = coerced;
    Ok(ty)
}

/// Build the sort key for one ORDER BY item over an already resolved target / output index
/// (PG `addTargetToSortList`).
fn make_sort_key(b: &mut Binder, target_idx: usize, ty: TypeId, item: &OrderByExpr, pos: Option<u32>) -> Result<SortKey> {
    let (descending, using_op): (bool, Option<OpRef>) = match &item.options.sort {
        None | Some(OrderBySort::Asc) => {
            check_default_sort_ops(b, ty, false, pos)?;
            (false, None)
        }
        Some(OrderBySort::Desc) => {
            check_default_sort_ops(b, ty, true, pos)?;
            (true, None)
        }
        Some(OrderBySort::Using(name)) => {
            let op_name = last_name_part(name);
            let op = resolve_named_op(b, &op_name, ty, ty, pos)?;
            let descending = match op_name.as_str() {
                "<" => false,
                ">" => true,
                _ => return Err(invalid_ordering_op(&op_name, pos)),
            };
            (descending, Some(op))
        }
    };
    let nulls_first = nulls_first_for(item.options.nulls_first, descending);
    Ok(SortKey { target_idx, descending, nulls_first, using_op })
}

/// Keep the first occurrence of each (index, direction, operator) key.
fn push_sort_key(keys: &mut Vec<SortKey>, key: SortKey) {
    let dup = keys.iter().any(|k| {
        k.target_idx == key.target_idx
            && k.descending == key.descending
            && k.using_op.as_ref().map(|o| o.op) == key.using_op.as_ref().map(|o| o.op)
    });
    if !dup {
        keys.push(key);
    }
}

/// Look up an operator `name` for `ty op ty`. `Ok(None)` when none exists (42883 from resolution).
fn lookup_op(b: &mut Binder, name: &str, ty: TypeId, pos: Option<u32>) -> Result<Option<OpRef>> {
    match resolve_named_op(b, name, ty, ty, pos) {
        Ok(op) => Ok(Some(op)),
        Err(e) if e.sqlstate == SqlState::UNDEFINED_FUNCTION => Ok(None),
        Err(e) => Err(e),
    }
}

/// Default ASC/DESC sorting needs `<` (or `>`) and `=` for the type (PG `get_sort_group_operators`).
fn check_default_sort_ops(b: &mut Binder, ty: TypeId, descending: bool, pos: Option<u32>) -> Result<()> {
    let lt = if descending { ">" } else { "<" };
    if lookup_op(b, lt, ty, pos)?.is_none() {
        return Err(no_ordering_op(b.type_name(ty), pos));
    }
    check_equality_op(b, ty, pos)
}

/// Equality operator required for DISTINCT / DISTINCT ON / sorting (PG `get_sort_group_operators`).
fn check_equality_op(b: &mut Binder, ty: TypeId, pos: Option<u32>) -> Result<()> {
    if lookup_op(b, "=", ty, pos)?.is_none() {
        return Err(no_equality_op(b.type_name(ty), pos));
    }
    Ok(())
}

/// Last identifier of a (possibly qualified) operator name.
fn last_name_part(name: &ObjectName) -> String {
    match name.0.last() {
        Some(ObjectNamePart::Identifier(id)) => id.value.clone(),
        _ => name.to_string(),
    }
}

/// Index of the set-operation output column an ORDER BY name refers to (module docs).
fn output_index_for_name(b: &mut Binder, inner: &Expr, output: &[OutputColumn], pos: Option<u32>) -> Result<usize> {
    let (name, qualified) = match inner {
        Expr::Identifier(id) => (b.ident(id), false),
        Expr::CompoundIdentifier(parts) => match parts.last() {
            Some(id) => (b.ident(id), true),
            None => return Err(diag::setop_order_by_expression(pos)),
        },
        _ => return Err(diag::setop_order_by_expression(pos)),
    };
    let matches: Vec<usize> = output.iter().enumerate().filter(|(_, c)| c.name == name).map(|(i, _)| i).collect();
    match matches.as_slice() {
        [] if qualified => Err(diag::setop_order_by_expression(pos)),
        [] => Err(diag::undefined_column(None, &name, pos)),
        [one] => Ok(*one),
        _ => Err(diag::output_name_ambiguous("ORDER BY", &name, pos)),
    }
}

/// Bind a LIMIT / OFFSET / FETCH count, coerced to int8. A NULL constant means no limit.
fn bind_count_expr(b: &mut Binder, e: &Expr, ec: ExprCtx, what: &str) -> Result<Option<BoundExpr>> {
    let pos = b.pos(e);
    let bound = bind_expr(b, e, ec)?;
    let coerced = coerce_to_specific(b, &BoundArg::new(bound, pos), TypeId::INT8, what)?;
    if matches!(coerced.kind, ExprKind::Const(Datum::Null)) {
        Ok(None)
    } else {
        Ok(Some(coerced))
    }
}

/// DISTINCT ON prefix rule (PG `transformDistinctOnClause`). `on` = target indexes of the DISTINCT ON
/// expressions in written order, `sort` = target indexes of the ORDER BY keys in order. Ok when the
/// ORDER BY keys that match DISTINCT ON form a leading run and every DISTINCT ON expression is either
/// already a sort key or can follow the sort keys. `Err(j)` names the offending DISTINCT ON expression.
fn check_distinct_on_prefix(on: &[usize], sort: &[usize]) -> std::result::Result<(), usize> {
    let mut skipped = false;
    for &s in sort {
        match on.iter().position(|&o| o == s) {
            Some(j) => {
                if skipped {
                    return Err(j);
                }
            }
            None => skipped = true,
        }
    }
    for (j, &o) in on.iter().enumerate() {
        if sort.contains(&o) {
            continue;
        }
        if skipped {
            return Err(j);
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------------------------
// Error builders (PG 18.6 wording)
// ---------------------------------------------------------------------------------------------

/// 42601 `non-integer constant in ORDER BY` (`clause` = ORDER BY | DISTINCT ON | GROUP BY).
fn non_integer_constant(clause: &str, pos: Option<u32>) -> PgError {
    diag::syntax(format!("non-integer constant in {clause}"), pos)
}

/// 42883 `could not identify an ordering operator for type X` + HINT (parse_oper.c).
fn no_ordering_op(type_name: String, pos: Option<u32>) -> PgError {
    diag::with_position(
        PgError::new(SqlState::UNDEFINED_FUNCTION, format!("could not identify an ordering operator for type {type_name}"))
            .with_hint("Use an explicit ordering operator or modify the query."),
        pos,
    )
}

/// 42883 `could not identify an equality operator for type X` (parse_oper.c).
fn no_equality_op(type_name: String, pos: Option<u32>) -> PgError {
    diag::with_position(
        PgError::new(SqlState::UNDEFINED_FUNCTION, format!("could not identify an equality operator for type {type_name}")),
        pos,
    )
}

/// 42809 `operator X is not a valid ordering operator` + HINT (parse_clause.c, USING form).
fn invalid_ordering_op(op: &str, pos: Option<u32>) -> PgError {
    diag::with_position(
        PgError::new(SqlState::WRONG_OBJECT_TYPE, format!("operator {op} is not a valid ordering operator"))
            .with_hint("Ordering operators must be \"<\" or \">\" members of btree operator families."),
        pos,
    )
}

/// 2201W `row count cannot be null in FETCH FIRST ... WITH TIES clause`.
fn with_ties_null_error() -> PgError {
    PgError::new(
        SqlState::INVALID_ROW_COUNT_IN_LIMIT_CLAUSE,
        "row count cannot be null in FETCH FIRST ... WITH TIES clause",
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use sqlparser::dialect::PostgreSqlDialect;
    use sqlparser::parser::Parser;

    fn parse_expr_text(sql: &str) -> Expr {
        let dialect = PostgreSqlDialect {};
        Parser::new(&dialect).try_with_sql(sql).expect("test SQL parses").parse_expr().expect("test expr parses")
    }

    #[test]
    fn integer_constants() {
        assert_eq!(int_constant(&parse_expr_text("3")), Some(IntConst::Integer(3)));
        assert_eq!(int_constant(&parse_expr_text("-1")), Some(IntConst::Integer(-1)));
        assert_eq!(int_constant(&parse_expr_text("-2147483648")), Some(IntConst::Integer(-2147483648)));
        assert_eq!(int_constant(&parse_expr_text("2147483648")), Some(IntConst::NonInteger));
        assert_eq!(int_constant(&parse_expr_text("'a'")), Some(IntConst::NonInteger));
        assert_eq!(int_constant(&parse_expr_text("1.5")), Some(IntConst::NonInteger));
        assert_eq!(int_constant(&parse_expr_text("NULL")), Some(IntConst::NonInteger));
        assert_eq!(int_constant(&parse_expr_text("$1")), None);
        assert_eq!(int_constant(&parse_expr_text("a")), None);
        assert_eq!(int_constant(strip_nested(&parse_expr_text("(2)"))), Some(IntConst::Integer(2)));
    }

    #[test]
    fn default_null_ordering() {
        assert!(!nulls_first_for(None, false));
        assert!(nulls_first_for(None, true));
        assert!(!nulls_first_for(Some(false), true));
        assert!(nulls_first_for(Some(true), false));
    }

    #[test]
    fn distinct_on_prefix_rule() {
        assert_eq!(check_distinct_on_prefix(&[0], &[]), Ok(()));
        assert_eq!(check_distinct_on_prefix(&[0], &[0, 1]), Ok(()));
        assert_eq!(check_distinct_on_prefix(&[0], &[1, 0]), Err(0));
        assert_eq!(check_distinct_on_prefix(&[0, 1], &[0]), Ok(()));
        assert_eq!(check_distinct_on_prefix(&[0], &[1]), Err(0));
        assert_eq!(check_distinct_on_prefix(&[2, 0], &[2, 0]), Ok(()));
    }

    #[test]
    fn error_texts() {
        let e = no_ordering_op("json".to_string(), Some(9));
        assert_eq!(e.sqlstate, SqlState::UNDEFINED_FUNCTION);
        assert_eq!(e.message, "could not identify an ordering operator for type json");
        assert_eq!(e.hint.as_deref(), Some("Use an explicit ordering operator or modify the query."));
        assert_eq!(e.position, Some(9));

        let e = no_equality_op("json".to_string(), None);
        assert_eq!(e.sqlstate, SqlState::UNDEFINED_FUNCTION);
        assert_eq!(e.message, "could not identify an equality operator for type json");

        let e = invalid_ordering_op("~~", None);
        assert_eq!(e.sqlstate, SqlState::WRONG_OBJECT_TYPE);
        assert_eq!(e.message, "operator ~~ is not a valid ordering operator");

        assert_eq!(non_integer_constant("ORDER BY", Some(10)).message, "non-integer constant in ORDER BY");
        assert_eq!(non_integer_constant("DISTINCT ON", None).message, "non-integer constant in DISTINCT ON");
        assert_eq!(
            diag::position_not_in_select_list("ORDER BY", 3, None).message,
            "ORDER BY position 3 is not in select list"
        );
        assert_eq!(
            with_ties_null_error().message,
            "row count cannot be null in FETCH FIRST ... WITH TIES clause"
        );
        assert_eq!(parse_int_text("7", false), IntConst::Integer(7));
        assert_eq!(parse_int_text("1_000", false), IntConst::Integer(1000));
    }

    #[test]
    fn merge_collation_follows_pg_strength_rules() {
        const C: Oid = 950;
        const POSIX: Oid = 951;
        let def = CollState::Implicit(oids::DEFAULT_COLLATION);
        // Two different non-default implicit collations conflict (PG COLLATE_CONFLICT).
        assert_eq!(merge_collation(CollState::Implicit(C), CollState::Implicit(POSIX)), CollState::Conflict);
        // Non-default implicit beats the default collation.
        assert_eq!(merge_collation(def, CollState::Implicit(C)), CollState::Implicit(C));
        // An explicit COLLATE on one arm resolves the conflict.
        assert_eq!(
            merge_collation(CollState::Conflict, CollState::Explicit(C)),
            CollState::Explicit(C)
        );
        assert_eq!(merge_collation(CollState::Implicit(C), CollState::Nothing), CollState::Implicit(C));
    }
}
