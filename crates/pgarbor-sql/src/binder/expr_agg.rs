//! Aggregate and window function calls, GROUPING() (SQL-13, sonnet).
//! PG references: `parse_agg.c` (`transformAggregateCall`, `transformWindowFuncCall`,
//! `check_agglevels_and_constraints`, `check_agg_arguments`, `transformGroupingFunc`),
//! `parse_func.c` (`ParseFuncOrColumn` aggregate branch), `parse_clause.c` (`transformWindowDefinitions`
//! lives in `window.rs`).
//!
//! ## Aggregates
//! Called by `expr_func::bind_function_call` after resolution says `ProcKind::Aggregate` (so the
//! argument types are final). Build [`crate::ir::Aggref`]:
//! * `agg` = proc OID, `name`, `args` coerced to the declared types (`coerce_to`), `star` for
//!   `count(*)` (zero args), `distinct` (`DISTINCT`), `filter` (bound with `ExprCtx::where_()` and
//!   `coerce_to_boolean("FILTER")`), `order_by` (`agg(x ORDER BY y)`; sort operators resolved as in
//!   `order::sort_expr`), `variadic`, `kind` from `catalog_data::builtin_aggregate(oid).kind`.
//! * Ordered-set (`percentile_cont(0.5) WITHIN GROUP (ORDER BY x)`): direct arguments are the call
//!   args, the WITHIN GROUP sort keys are `order_by` and ALSO the aggregated `args`; hypothetical-set
//!   (`rank(1) WITHIN GROUP (ORDER BY x)`) has matching counts (42883 `function rank(integer) does not
//!   exist`...). A normal aggregate with WITHIN GROUP: 42809 `X is not an ordered-set aggregate, so
//!   it cannot have WITHIN GROUP`; an ordered-set aggregate without it: 42809 `WITHIN GROUP is
//!   required for ordered-set aggregate X`. `DISTINCT` + ordered-set: 42809? (PG: `cannot use
//!   DISTINCT with WITHIN GROUP`); `ORDER BY` in a call to an ordered-set aggregate: error.
//! * `count(DISTINCT a, b)`: multi-arg DISTINCT allowed. `ORDER BY` with `DISTINCT`: each ORDER BY
//!   expression must appear in the argument list (42P10 `in an aggregate with DISTINCT, ORDER BY
//!   expressions must appear in argument list`).
//! * **Level** (`agglevelsup`): `min(levels_up of every Column in args and filter)` (outer-only args
//!   make the aggregate belong to the outer query; no columns at all -> level 0). The node is
//!   returned with `levels_up` set; the owning scope (`scopes[len-1-levels_up]`) gets `has_aggs =
//!   true`. Placement checks (`check_agglevels_and_constraints`) use the clause of the OWNING level:
//!   `aggregate functions are not allowed in WHERE` (42803, also GROUP BY, JOIN conditions,
//!   FROM function arguments, LIMIT, DEFAULT, CHECK, index expressions, ...) via
//!   `diag::agg_not_allowed`; `aggregate function calls cannot be nested` (42803) when the
//!   arguments contain an aggregate of the same level (track `Binder::agg_depth`); `aggregate
//!   function calls cannot contain set-returning function calls`; `aggregate function calls cannot
//!   contain window function calls` (42P20).
//! * Aggregates whose argument types need resolution use the normal function machinery so
//!   `sum(int4)` -> `bigint`, `sum(numeric)` -> `numeric`, `count(*)` -> `bigint`, `avg(int)` ->
//!   `numeric`, `string_agg(text,text)`, `array_agg(x)` -> array, `min(text)`, `max(date)`.
//!
//! ## Window functions
//! `f(args) OVER (spec)` / `OVER name`: resolve `f` as a window function OR an aggregate (becomes
//! `WindowFunc { is_agg: true }`). Build [`crate::ir::WindowFunc`]: coerced `args`, `filter`
//! (aggregates only; FILTER on a pure window function: 42809? `FILTER is not implemented for
//! non-aggregate window functions` 0A000), `distinct` (aggregate windows: 0A000 `DISTINCT is not
//! implemented for window functions`), `ignore_nulls` / `RESPECT NULLS`: 0A000 for functions other
//! than lead/lag/first_value/last_value/nth_value (PG 18 supports IGNORE NULLS for exactly those),
//! `window` = index returned by `window::bind_window_spec`. Placement: `window functions are not
//! allowed in WHERE` / GROUP BY / HAVING / JOIN conditions (42P20, `diag::window_not_allowed`);
//! nested: `window function calls cannot be nested` (42P20); inside an aggregate's argument: same
//! rule. Marks `scope.has_windows`. `OVER` is only legal in the SELECT list and ORDER BY of the
//! owning query.
//!
//! ## GROUPING(a, b, ...)
//! Arguments must be grouping expressions of the owning query (42P20 `arguments to GROUPING must
//! be grouping expressions of the associated query level`); at most 31 (54023 `GROUPING must have
//! fewer than 32 arguments`); result type `int4`. GROUP BY is bound AFTER the select list, so the
//! grouping expressions are not known yet. Protocol: this function binds each argument expression,
//! appends it to the owning scope's `QueryScope::pending_grouping` (dedup by equality) and returns
//! `ExprKind::Grouping(GroupingExpr { args })` where `args[i]` is the index of the i-th argument
//! INTO `pending_grouping`. `group::resolve_pending_groupings` (after GROUP BY is bound) rewrites
//! every `Grouping` node of the query block so `args` index `GroupBy::exprs`, raising the 42P20
//! error for an argument that is not a grouping expression.
use pgarbor_common::{Oid, PgError, Result, SqlState};
use pgarbor_types::catalog_data::{builtin_aggregate, AggKind, CastContext, ProcKind};
use pgarbor_types::compare::is_orderable;
use pgarbor_types::resolve::func::ResolvedFunc;
use pgarbor_types::TypeId;
use sqlparser::ast::{
    DuplicateTreatment, Expr, Function, FunctionArg, FunctionArgExpr, FunctionArgumentClause, FunctionArgumentList,
    FunctionArguments, Ident, NullTreatment, ObjectName, ObjectNamePart, OrderByExpr, OrderBySort, WindowType,
};

use super::context::{Binder, BoundArg, ExprCtx};
use super::coerce::{coerce_strict, coerce_to_boolean};
use super::expr::bind_expr;
use super::expr_ops::resolve_named_op;
use super::window::bind_window_spec;
use crate::diag::{self, ExprClause};
use crate::ir::{
    Aggref, BoundExpr, BoundQuery, BoundSelect, BoundValues, CoercionForm, CteBody, ExprKind, FrameBound, FromItem,
    GroupingExpr, QueryBody, RangeKind, SortExpr, WindowFunc,
};

/// Build an `Aggref` for an already resolved aggregate call (module docs). `f` is the whole call
/// (filter, within group, order by, distinct are read from it); `args` are the bound arguments in call
/// order (after named-argument handling).
///
/// For an ordered-set aggregate `args` holds the direct arguments, optionally followed by the bound
/// WITHIN GROUP keys (one per `f.within_group` item); when the keys are absent they are bound here.
/// `resolved.coerce_to` covers direct arguments then aggregated keys, in that order.
pub fn bind_aggregate(b: &mut Binder, f: &Function, resolved: &ResolvedFunc, args: Vec<BoundArg>, ec: ExprCtx) -> Result<BoundExpr> {
    let pos = b.pos(f);
    let call_name = qualified_name(&f.name);
    let info = match builtin_aggregate(resolved.oid) {
        Some(info) => info,
        None => return Err(PgError::unimplemented("SQL-13", format!("user-defined aggregate {call_name}"))),
    };
    let kind = info.kind;
    let ordered_set = !matches!(kind, AggKind::Normal);
    let clauses = call_clauses(f, pos)?;
    let distinct = distinct_flag(f);
    let star = is_star_call(f);

    // Grammar-level checks (gram.y `func_application ... WITHIN GROUP`).
    if let Some(first) = f.within_group.first() {
        let wpos = b.pos(&first.expr);
        if !clauses.order_by.is_empty() {
            return Err(PgError::syntax("cannot use multiple ORDER BY clauses with WITHIN GROUP", wpos));
        }
        if distinct {
            return Err(PgError::syntax("cannot use DISTINCT with WITHIN GROUP", wpos));
        }
    }
    // Analysis-level checks (parse_func.c `ParseFuncOrColumn`).
    if ordered_set {
        if f.within_group.is_empty() {
            return Err(at(
                PgError::new(SqlState::WRONG_OBJECT_TYPE, format!("WITHIN GROUP is required for ordered-set aggregate {call_name}")),
                pos,
            ));
        }
    } else if !f.within_group.is_empty() {
        return Err(at(
            PgError::new(
                SqlState::WRONG_OBJECT_TYPE,
                format!("{call_name} is not an ordered-set aggregate, so it cannot have WITHIN GROUP"),
            ),
            pos,
        ));
    }
    if clauses.ignore_nulls.is_some() {
        return Err(diag::unsupported_construct("IGNORE NULLS / RESPECT NULLS in an aggregate call", pos));
    }
    if resolved.nvariadic > 0 {
        return Err(PgError::unimplemented("SQL-13", "VARIADIC arguments of an aggregate call"));
    }

    // Sort keys: WITHIN GROUP items for ordered-set aggregates, ORDER BY items otherwise.
    let sort_items: Vec<&OrderByExpr> = if ordered_set { f.within_group.iter().collect() } else { clauses.order_by };

    // Split the bound arguments into direct arguments and (for ordered-set) pre-bound keys.
    let (direct, agg_args, prebound_keys): (Vec<BoundArg>, Vec<BoundArg>, Option<Vec<BoundArg>>) = if ordered_set {
        let nd = positional_count(f);
        let k = sort_items.len();
        let mut direct = args;
        let prebound = if direct.len() == nd + k {
            Some(direct.split_off(nd))
        } else if direct.len() == nd {
            None
        } else {
            return Err(PgError::internal(format!("aggregate {call_name}: argument count does not match the call")));
        };
        (direct, Vec::new(), prebound)
    } else {
        (Vec::new(), args, None)
    };

    let key_ctx = ExprCtx { clause: ExprClause::Other("ORDER BY"), ..ec.inside_aggregate() };
    let key_args: Vec<BoundArg> = match prebound_keys {
        Some(keys) => keys,
        None => {
            let mut keys = Vec::with_capacity(sort_items.len());
            for item in sort_items.iter().copied() {
                keys.push(bind_sort_key(b, item, key_ctx)?);
            }
            keys
        }
    };
    let filter = match clauses.filter {
        Some(e) => Some(bind_filter(b, e)?),
        None => None,
    };

    // Coercions (PG `transformAggregateCall`: args to the declared types).
    let direct_out = {
        let mut v = Vec::with_capacity(direct.len());
        for (i, a) in direct.iter().enumerate() {
            v.push(coerce_arg(b, resolved, i, a)?);
        }
        v
    };
    let (agg_out, key_out): (Vec<BoundExpr>, Vec<BoundExpr>) = if ordered_set {
        let mut keys = Vec::with_capacity(key_args.len());
        for (j, key) in key_args.iter().enumerate() {
            keys.push(coerce_arg(b, resolved, direct.len() + j, key)?);
        }
        (keys.clone(), keys)
    } else {
        let mut aggs = Vec::with_capacity(agg_args.len());
        for (i, a) in agg_args.iter().enumerate() {
            aggs.push(coerce_arg(b, resolved, i, a)?);
        }
        (aggs, key_args.iter().map(|k| k.expr.clone()).collect())
    };

    // DISTINCT: every ORDER BY key must be one of the (uncoerced) arguments (PG 42P10).
    if distinct {
        let raw: Vec<&BoundExpr> = agg_args.iter().map(|a| &a.expr).collect();
        for key in &key_args {
            if !raw.iter().any(|a| **a == key.expr) {
                return Err(at(
                    PgError::new(
                        SqlState::INVALID_COLUMN_REFERENCE,
                        "in an aggregate with DISTINCT, ORDER BY expressions must appear in argument list",
                    ),
                    key.pos,
                ));
            }
        }
    }

    let key_positions: Vec<Option<u32>> = key_args.iter().map(|k| k.pos).collect();
    let mut order_by: Vec<SortExpr> = Vec::with_capacity(sort_items.len());
    for ((item, kpos), expr) in sort_items.iter().copied().zip(key_positions).zip(key_out) {
        order_by.push(make_sort(b, item, expr, kpos)?);
    }

    // Level and nesting (PG `check_agg_arguments`).
    let mut scan = LevelScan::default();
    for e in agg_out.iter().chain(order_by.iter().map(|s| &s.expr)).chain(filter.iter()) {
        scan_expr(e, 0, &mut scan);
    }
    let level = scan.min_level.unwrap_or(0);
    if scan.agg_levels.contains(&level) {
        return Err(diag::nested_aggregate(pos));
    }
    if agg_out.iter().chain(order_by.iter().map(|s| &s.expr)).any(|e| e.contains_window()) {
        return Err(at(
            PgError::new(SqlState::GROUPING_ERROR, "aggregate function calls cannot contain window function calls"),
            pos,
        ));
    }
    if agg_out.iter().chain(order_by.iter().map(|s| &s.expr)).any(|e| e.contains_srf()) {
        return Err(at(
            PgError::new(
                SqlState::FEATURE_NOT_SUPPORTED,
                "aggregate function calls cannot contain set-returning function calls",
            )
            .with_hint("You might be able to move the set-returning function into a LATERAL FROM item."),
            pos,
        ));
    }

    // Placement (PG `check_agglevels_and_constraints`): the call's own clause must allow aggregates.
    if !ec.aggs_ok {
        return Err(if b.agg_depth > 0 {
            diag::nested_aggregate(pos)
        } else {
            diag::agg_not_allowed(ec.clause, pos)
        });
    }

    if level > 0 {
        return Err(PgError::unimplemented("SQL-13", "aggregate evaluated by an enclosing query level"));
    }
    let owner = b
        .scopes
        .len()
        .checked_sub(1 + level as usize)
        .ok_or_else(|| PgError::internal("aggregate level exceeds the open query scopes"))?;
    let owner_scope = b.scopes.get_mut(owner).ok_or_else(|| PgError::internal("aggregate owner scope missing"))?;
    owner_scope.has_aggs = true;

    let aggref = Aggref {
        agg: resolved.oid,
        name: info.name.to_string(),
        args: agg_out,
        direct_args: direct_out,
        kind,
        distinct,
        filter: filter.map(Box::new),
        order_by,
        star,
        variadic: false,
        levels_up: level,
    };
    Ok(BoundExpr::new(ExprKind::Aggref(Box::new(aggref)), resolved.ret))
}

/// Build a `WindowFunc` for `f(...) OVER window` (module docs).
pub fn bind_window_call(b: &mut Binder, f: &Function, resolved: &ResolvedFunc, args: Vec<BoundArg>, over: &WindowType, ec: ExprCtx) -> Result<BoundExpr> {
    let pos = b.pos(f);
    let call_name = qualified_name(&f.name);
    let is_agg = matches!(resolved.kind, ProcKind::Aggregate);
    let agg_info = if is_agg {
        match builtin_aggregate(resolved.oid) {
            Some(info) => Some(info),
            None => return Err(PgError::unimplemented("SQL-13", format!("user-defined aggregate {call_name}"))),
        }
    } else {
        None
    };
    let ordered_set = agg_info.map_or(false, |info| !matches!(info.kind, AggKind::Normal));
    let clauses = call_clauses(f, pos)?;

    if distinct_flag(f) {
        return Err(at(PgError::new(SqlState::FEATURE_NOT_SUPPORTED, "DISTINCT is not implemented for window functions"), pos));
    }
    if is_agg && ordered_set {
        if f.within_group.is_empty() {
            return Err(at(
                PgError::new(SqlState::WRONG_OBJECT_TYPE, format!("WITHIN GROUP is required for ordered-set aggregate {call_name}")),
                pos,
            ));
        }
        return Err(at(
            PgError::new(SqlState::FEATURE_NOT_SUPPORTED, format!("OVER is not supported for ordered-set aggregate {call_name}")),
            pos,
        ));
    }
    if !f.within_group.is_empty() {
        let msg = if is_agg {
            format!("{call_name} is not an ordered-set aggregate, so it cannot have WITHIN GROUP")
        } else {
            format!("window function {call_name} cannot have WITHIN GROUP")
        };
        return Err(at(PgError::new(SqlState::WRONG_OBJECT_TYPE, msg), pos));
    }
    if !clauses.order_by.is_empty() {
        if is_agg {
            return Err(at(
                PgError::new(SqlState::FEATURE_NOT_SUPPORTED, "aggregate ORDER BY is not implemented for window functions"),
                pos,
            ));
        }
        return Err(at(
            PgError::new(SqlState::WRONG_OBJECT_TYPE, format!("ORDER BY specified, but {call_name} is not an aggregate function")),
            pos,
        ));
    }
    if !is_agg && clauses.filter.is_some() {
        return Err(at(
            PgError::new(SqlState::FEATURE_NOT_SUPPORTED, "FILTER is not implemented for non-aggregate window functions"),
            pos,
        ));
    }
    let ignores = matches!(clauses.ignore_nulls, Some(true));
    if ignores && !matches!(bare_name(&f.name).as_str(), "lead" | "lag" | "first_value" | "last_value" | "nth_value") {
        return Err(diag::unsupported_construct(&format!("IGNORE NULLS for window function {call_name}"), pos));
    }

    // Placement (PG `transformWindowFuncCall` / `check_agglevels_and_constraints`).
    if !ec.windows_ok {
        return Err(if b.agg_depth > 0 {
            at(
                PgError::new(SqlState::GROUPING_ERROR, "aggregate function calls cannot contain window function calls"),
                pos,
            )
        } else {
            diag::window_not_allowed(ec.clause, pos)
        });
    }
    if args.iter().any(|a| a.expr.contains_window()) {
        return Err(at(PgError::new(SqlState::WINDOWING_ERROR, "window function calls cannot be nested"), pos));
    }

    let mut arg_exprs = Vec::with_capacity(args.len());
    for (i, a) in args.iter().enumerate() {
        arg_exprs.push(coerce_arg(b, resolved, i, a)?);
    }
    if is_agg {
        let mut scan = LevelScan::default();
        for e in &arg_exprs {
            scan_expr(e, 0, &mut scan);
        }
        if !scan.agg_levels.is_empty() {
            return Err(diag::nested_aggregate(pos));
        }
    }
    let filter = if is_agg {
        match clauses.filter {
            Some(e) => Some(bind_filter(b, e)?),
            None => None,
        }
    } else {
        None
    };

    let window = bind_window_spec(b, over, pos)?;
    b.scope_mut()?.has_windows = true;

    let wf = WindowFunc {
        func: resolved.oid,
        name: match agg_info {
            Some(info) => info.name.to_string(),
            None => bare_name(&f.name),
        },
        args: arg_exprs,
        is_agg,
        star: is_agg && is_star_call(f),
        distinct: false,
        filter: filter.map(Box::new),
        ignore_nulls: ignores,
        window,
    };
    Ok(BoundExpr::new(ExprKind::WindowFunc(Box::new(wf)), resolved.ret))
}

/// `GROUPING(args)` (module docs).
pub fn bind_grouping(b: &mut Binder, f: &Function, ec: ExprCtx) -> Result<BoundExpr> {
    let pos = b.pos(f);
    if f.over.is_some() || f.filter.is_some() || !f.within_group.is_empty() || distinct_flag(f) {
        return Err(diag::unsupported_construct("GROUPING with FILTER, OVER, DISTINCT or WITHIN GROUP", pos));
    }
    if !ec.aggs_ok {
        return Err(grouping_not_allowed(ec.clause, pos));
    }
    let mut exprs: Vec<&Expr> = Vec::new();
    if let Some(list) = arg_list(f) {
        for a in &list.args {
            match a {
                FunctionArg::Unnamed(FunctionArgExpr::Expr(e)) => exprs.push(e),
                _ => return Err(diag::unsupported_construct("named or wildcard arguments of GROUPING", pos)),
            }
        }
    }
    if exprs.is_empty() {
        return Err(PgError::syntax("syntax error at or near \")\"", pos));
    }
    if exprs.len() >= 32 {
        return Err(at(PgError::new(SqlState(*b"54023"), "GROUPING must have fewer than 32 arguments"), pos));
    }

    let ctx = ExprCtx { aggs_ok: false, windows_ok: false, srf_ok: false, ..ec };
    let mut idxs: Vec<usize> = Vec::with_capacity(exprs.len());
    for e in exprs {
        let bound = bind_expr(b, e, ctx)?;
        let scope = b.scope_mut()?;
        let found = scope.pending_grouping.iter().position(|x| *x == bound);
        let idx = match found {
            Some(i) => i,
            None => {
                scope.pending_grouping.push(bound);
                scope.pending_grouping.len() - 1
            }
        };
        idxs.push(idx);
    }
    Ok(BoundExpr::new(ExprKind::Grouping(Box::new(GroupingExpr { args: idxs, mask_slot: None })), TypeId::INT4))
}

/// The query level (0 = current) an aggregate with these bound arguments/filter belongs to:
/// the minimum `levels_up` over all `Column` nodes at the aggregate's own nesting (columns inside
/// sub-selects count with their level adjusted by the sub-select depth). No columns -> 0.
/// Pure helper over bound expressions (`BoundExpr::collect_columns`), fully testable.
///
/// Nested aggregate calls contribute their own level like PG's `check_agg_arguments_walker`, and
/// sub-selects are entered (a sub-select at depth `k` referencing `levels_up = L` contributes `L - k`
/// when `L >= k`).
pub fn aggregate_level(args: &[BoundExpr], filter: Option<&BoundExpr>) -> Result<u32> {
    let mut scan = LevelScan::default();
    for e in args.iter().chain(filter) {
        scan_expr(e, 0, &mut scan);
    }
    Ok(scan.min_level.unwrap_or(0))
}

// ---------------------------------------------------------------------------------------------
// Private helpers.
// ---------------------------------------------------------------------------------------------

/// Lowest level found and the levels of nested aggregates found.
#[derive(Default)]
struct LevelScan {
    /// Minimum over variables and aggregates (PG `min_varlevel` / `min_agglevel`).
    min_level: Option<u32>,
    /// Levels of every aggregate found in the arguments (for the nested-aggregate check).
    agg_levels: Vec<u32>,
}

impl LevelScan {
    /// A variable reference `levels_up` levels out, seen at sub-select depth `depth`.
    fn note_var(&mut self, levels_up: u32, depth: u32) {
        if let Some(level) = levels_up.checked_sub(depth) {
            self.min_level = Some(self.min_level.map_or(level, |m| m.min(level)));
        }
    }

    /// A nested aggregate `levels_up` levels out, seen at sub-select depth `depth`.
    fn note_agg(&mut self, levels_up: u32, depth: u32) {
        if let Some(level) = levels_up.checked_sub(depth) {
            self.min_level = Some(self.min_level.map_or(level, |m| m.min(level)));
            self.agg_levels.push(level);
        }
    }
}

fn scan_expr(e: &BoundExpr, depth: u32, sc: &mut LevelScan) {
    e.walk(&mut |n: &BoundExpr| {
        match &n.kind {
            ExprKind::Column(c) => sc.note_var(c.levels_up, depth),
            ExprKind::Aggref(a) => sc.note_agg(a.levels_up, depth),
            ExprKind::SubLink(s) => scan_query(&s.subquery, depth + 1, &mut *sc),
            _ => {}
        }
        true
    });
}

fn scan_query(q: &BoundQuery, depth: u32, sc: &mut LevelScan) {
    for cte in &q.ctes {
        if let CteBody::Query(inner) = &cte.body {
            scan_query(inner, depth + 1, &mut *sc);
        }
    }
    match &q.body {
        QueryBody::Select(s) => scan_select(s, depth, &mut *sc),
        QueryBody::Values(v) => scan_values(v, depth, &mut *sc),
        QueryBody::SetOp(op) => {
            scan_query(&op.left, depth + 1, &mut *sc);
            scan_query(&op.right, depth + 1, &mut *sc);
        }
    }
    if let Some(e) = &q.limit {
        scan_expr(e, depth, &mut *sc);
    }
    if let Some(e) = &q.offset {
        scan_expr(e, depth, &mut *sc);
    }
}

fn scan_select(s: &BoundSelect, depth: u32, sc: &mut LevelScan) {
    for entry in &s.range_table {
        match &entry.kind {
            RangeKind::Subquery { query } => scan_query(query, depth + 1, &mut *sc),
            RangeKind::Function { funcs, .. } => {
                for f in funcs {
                    scan_expr(&f.expr, depth, &mut *sc);
                }
            }
            RangeKind::Values { values } => scan_values(values, depth, &mut *sc),
            RangeKind::Relation { .. } | RangeKind::Cte { .. } => {}
        }
    }
    for item in &s.from {
        scan_from(item, depth, &mut *sc);
    }
    for e in s.where_.iter().chain(s.having.iter()) {
        scan_expr(e, depth, &mut *sc);
    }
    for t in &s.target_list {
        scan_expr(&t.expr, depth, &mut *sc);
    }
    for w in &s.windows {
        for e in &w.partition_by {
            scan_expr(e, depth, &mut *sc);
        }
        for k in &w.order_by {
            scan_expr(&k.expr, depth, &mut *sc);
        }
        for bound in [&w.start, &w.end] {
            if let FrameBound::Preceding(e) | FrameBound::Following(e) = bound {
                scan_expr(e, depth, &mut *sc);
            }
        }
    }
}

fn scan_from(item: &FromItem, depth: u32, sc: &mut LevelScan) {
    match item {
        FromItem::Range(_) => {}
        FromItem::Join(j) => {
            scan_from(&j.left, depth, &mut *sc);
            scan_from(&j.right, depth, &mut *sc);
            if let Some(q) = &j.quals {
                scan_expr(q, depth, &mut *sc);
            }
        }
    }
}

fn scan_values(v: &BoundValues, depth: u32, sc: &mut LevelScan) {
    for row in &v.rows {
        for e in row {
            scan_expr(e, depth, &mut *sc);
        }
    }
}

/// Clauses of an aggregate or window call that this module handles.
struct CallClauses<'a> {
    /// `ORDER BY` inside the call (plain aggregates only; `WITHIN GROUP` is `f.within_group`).
    order_by: Vec<&'a OrderByExpr>,
    /// `FILTER (WHERE ..)` (or the GoogleSQL inline `WHERE` in the argument list).
    filter: Option<&'a Expr>,
    /// `Some(true)` = IGNORE NULLS, `Some(false)` = RESPECT NULLS, `None` = not written.
    ignore_nulls: Option<bool>,
}

fn call_clauses(f: &Function, pos: Option<u32>) -> Result<CallClauses<'_>> {
    let mut out = CallClauses { order_by: Vec::new(), filter: f.filter.as_deref(), ignore_nulls: None };
    if let Some(n) = &f.null_treatment {
        out.ignore_nulls = Some(matches!(n, NullTreatment::IgnoreNulls));
    }
    if let Some(list) = arg_list(f) {
        for clause in &list.clauses {
            match clause {
                FunctionArgumentClause::OrderBy(items) => out.order_by.extend(items.iter()),
                FunctionArgumentClause::Where(e) => {
                    if out.filter.is_some() {
                        return Err(diag::unsupported_construct("a second FILTER clause", pos));
                    }
                    out.filter = Some(e);
                }
                FunctionArgumentClause::IgnoreOrRespectNulls(n) => {
                    out.ignore_nulls = Some(matches!(n, NullTreatment::IgnoreNulls));
                }
                _ => return Err(diag::unsupported_construct("this argument clause of an aggregate or window call", pos)),
            }
        }
    }
    Ok(out)
}

fn arg_list(f: &Function) -> Option<&FunctionArgumentList> {
    match &f.args {
        FunctionArguments::List(l) => Some(l),
        _ => None,
    }
}

fn is_wildcard_arg(a: &FunctionArg) -> bool {
    matches!(a, FunctionArg::Unnamed(FunctionArgExpr::Wildcard))
}

/// `count(*)`: the only argument is the wildcard.
fn is_star_call(f: &Function) -> bool {
    match arg_list(f) {
        Some(l) => matches!(l.args.as_slice(), [a] if is_wildcard_arg(a)),
        None => false,
    }
}

/// Number of non-wildcard arguments written in the call (direct arguments for ordered-set calls).
fn positional_count(f: &Function) -> usize {
    arg_list(f).map_or(0, |l| l.args.iter().filter(|a| !is_wildcard_arg(a)).count())
}

fn distinct_flag(f: &Function) -> bool {
    arg_list(f)
        .and_then(|l| l.duplicate_treatment.as_ref())
        .map_or(false, |d| matches!(d, DuplicateTreatment::Distinct))
}

/// Identifier text as PG folds it (unquoted names lower-cased).
fn ident_text(id: &Ident) -> String {
    if id.quote_style.is_some() {
        id.value.clone()
    } else {
        id.value.to_lowercase()
    }
}

/// `NameListToString` of a function name (`pg_catalog.sum`).
fn qualified_name(name: &ObjectName) -> String {
    let parts: Vec<String> = name
        .0
        .iter()
        .filter_map(|p| match p {
            ObjectNamePart::Identifier(id) => Some(ident_text(id)),
            _ => None,
        })
        .collect();
    parts.join(".")
}

/// The unqualified (last) part of a function name.
fn bare_name(name: &ObjectName) -> String {
    name.0
        .iter()
        .rev()
        .find_map(|p| match p {
            ObjectNamePart::Identifier(id) => Some(ident_text(id)),
            _ => None,
        })
        .unwrap_or_default()
}

fn at(mut e: PgError, pos: Option<u32>) -> PgError {
    e.position = pos;
    e
}

fn grouping_not_allowed(clause: ExprClause, pos: Option<u32>) -> PgError {
    at(PgError::new(SqlState::GROUPING_ERROR, format!("grouping operations are not allowed in {}", clause.name())), pos)
}

/// Bind one ORDER BY item expression (not yet coerced).
fn bind_sort_key(b: &mut Binder, item: &OrderByExpr, ctx: ExprCtx) -> Result<BoundArg> {
    let pos = b.pos(&item.expr);
    let expr = bind_expr(b, &item.expr, ctx)?;
    Ok(BoundArg::new(expr, pos))
}

/// Bind the FILTER expression: boolean, aggregates/windows/SRFs not allowed.
fn bind_filter(b: &mut Binder, e: &Expr) -> Result<BoundExpr> {
    let ctx = ExprCtx {
        clause: ExprClause::Other("FILTER"),
        aggs_ok: false,
        windows_ok: false,
        srf_ok: false,
        sublinks_ok: true,
        columns_ok: true,
    };
    let fpos = b.pos(e);
    let bound = bind_expr(b, e, ctx)?;
    coerce_to_boolean(b, &BoundArg::new(bound, fpos), "FILTER")
}

/// Coerce argument `idx` to the declared parameter type from the resolution.
fn coerce_arg(b: &mut Binder, resolved: &ResolvedFunc, idx: usize, arg: &BoundArg) -> Result<BoundExpr> {
    let target: Oid = match resolved.coerce_to.get(idx).copied() {
        Some(oid) => oid,
        None => return Err(PgError::internal("aggregate call: missing argument coercion target")),
    };
    coerce_strict(b, arg, TypeId::of(target), CastContext::Implicit, CoercionForm::Implicit)
}

/// Sort key of an aggregate ORDER BY / WITHIN GROUP item: direction, null placement and the
/// `USING` operator (PG `addTargetToSortList`, `get_sort_group_operators`). Without `USING` the
/// type's default ordering operator is used at run time (`using_op: None`).
fn make_sort(b: &mut Binder, item: &OrderByExpr, expr: BoundExpr, pos: Option<u32>) -> Result<SortExpr> {
    if item.with_fill.is_some() {
        return Err(diag::unsupported_construct("WITH FILL", pos));
    }
    let ty = expr.ty;
    let (descending, using_op) = match &item.options.sort {
        None | Some(OrderBySort::Asc) => (false, None),
        Some(OrderBySort::Desc) => (true, None),
        Some(OrderBySort::Using(name)) => {
            let op_name = bare_name(name);
            let op = resolve_named_op(b, &op_name, ty, ty, pos)?;
            (op_name == ">", Some(op))
        }
    };
    if using_op.is_none() && !is_orderable(b.env, ty)? {
        let msg = format!("could not identify an ordering operator for type {}", b.type_name(ty));
        return Err(at(PgError::new(SqlState::UNDEFINED_FUNCTION, msg), pos));
    }
    let nulls_first = item.options.nulls_first.unwrap_or(descending);
    Ok(SortExpr { expr, descending, nulls_first, using_op })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ir::{BoundQuery, BoundSelect, ColumnRef, QueryBody, SubLink, SubLinkKind};
    use pgarbor_types::{Datum, TypeId};

    fn col(levels_up: u32) -> BoundExpr {
        BoundExpr::column(levels_up, 1, 1, TypeId::INT4)
    }

    fn int(v: i32) -> BoundExpr {
        BoundExpr::constant(Datum::Int4(v), TypeId::INT4)
    }

    fn query_with_where(where_: BoundExpr) -> BoundQuery {
        let mut sel = BoundSelect::empty();
        sel.where_ = Some(where_);
        BoundQuery {
            ctes: Vec::new(),
            body: QueryBody::Select(Box::new(sel)),
            output: Vec::new(),
            order_by: Vec::new(),
            limit: None,
            offset: None,
            limit_with_ties: false,
            locking: Vec::new(),
        }
    }

    #[test]
    fn level_without_columns_is_zero() {
        assert_eq!(aggregate_level(&[int(1)], None).unwrap(), 0);
        assert_eq!(aggregate_level(&[], None).unwrap(), 0);
    }

    #[test]
    fn level_is_minimum_over_args_and_filter() {
        assert_eq!(aggregate_level(&[col(0), col(2)], None).unwrap(), 0);
        assert_eq!(aggregate_level(&[col(2), col(1)], None).unwrap(), 1);
        assert_eq!(aggregate_level(&[col(1)], Some(&col(0))).unwrap(), 0);
        assert_eq!(aggregate_level(&[int(1)], Some(&col(1))).unwrap(), 1);
    }

    #[test]
    fn sublink_columns_are_shifted_by_depth() {
        // Sub-select at depth 1 referencing levels_up 1 -> the aggregate's own query (level 0).
        let local = BoundExpr::new(
            ExprKind::SubLink(Box::new(SubLink {
                kind: SubLinkKind::Scalar,
                test: None,
                ops: Vec::new(),
                subquery: Box::new(query_with_where(col(1))),
            })),
            TypeId::INT4,
        );
        assert_eq!(aggregate_level(&[local], None).unwrap(), 0);

        // Referencing levels_up 2 from depth 1 -> one level above the aggregate's query.
        let outer = BoundExpr::new(
            ExprKind::SubLink(Box::new(SubLink {
                kind: SubLinkKind::Scalar,
                test: None,
                ops: Vec::new(),
                subquery: Box::new(query_with_where(col(2))),
            })),
            TypeId::INT4,
        );
        assert_eq!(aggregate_level(&[outer], None).unwrap(), 1);
    }

    #[test]
    fn sublink_local_columns_do_not_count() {
        // levels_up 0 inside the sub-select refers to the sub-select itself.
        let inner = BoundExpr::new(
            ExprKind::SubLink(Box::new(SubLink {
                kind: SubLinkKind::Scalar,
                test: None,
                ops: Vec::new(),
                subquery: Box::new(query_with_where(col(0))),
            })),
            TypeId::INT4,
        );
        assert_eq!(aggregate_level(&[inner], None).unwrap(), 0);
        assert_eq!(aggregate_level(&[col(0)], None).unwrap(), 0);
    }

    #[test]
    fn nested_aggregate_levels_are_collected() {
        let nested = BoundExpr::new(
            ExprKind::Aggref(Box::new(Aggref {
                agg: 2147,
                name: "count".to_string(),
                args: vec![col(1)],
                direct_args: Vec::new(),
                kind: AggKind::Normal,
                distinct: false,
                filter: None,
                order_by: Vec::new(),
                star: false,
                variadic: false,
                levels_up: 1,
            })),
            TypeId::INT8,
        );
        assert_eq!(aggregate_level(&[nested.clone()], None).unwrap(), 1);
        let mut sc = LevelScan::default();
        scan_expr(&nested, 0, &mut sc);
        assert_eq!(sc.agg_levels, vec![1]);
    }

    #[test]
    fn grouping_message_matches_pg() {
        let e = grouping_not_allowed(ExprClause::Where, Some(7));
        assert_eq!(e.message, "grouping operations are not allowed in WHERE");
        assert_eq!(e.sqlstate, SqlState::GROUPING_ERROR);
        assert_eq!(e.position, Some(7));
    }

    #[test]
    fn column_ref_equality_ignores_position() {
        let a = ColumnRef { levels_up: 0, rt_index: 1, attnum: 2, pos: 3 };
        let b = ColumnRef { levels_up: 0, rt_index: 1, attnum: 2, pos: 9 };
        assert_eq!(a, b);
    }
}
