//! Query expressions: `WITH ... SELECT/VALUES/set-operation ... ORDER BY ... LIMIT ...` (SQL-16).
//! PG references: `analyze.c` (`transformSelectStmt`, `transformSetOperationStmt`,
//! `transformValuesClause`, `transformStmt`), `parse_clause.c` (`transformFromClause`,
//! `transformWhereClause`, `transformLimitClause`, `transformLockingClause`), `parse_cte.c`.
//!
//! ## `bind_query(b, q, qctx)` algorithm (sqlparser `Query { with, body, order_by, limit_clause,
//! fetch, locks, .. }`)
//! 1. `b.enter()`. Reject non-PostgreSQL clauses: `for_clause`, `settings`, `format_clause`,
//!    `pipe_operators`, `limit_by`, MySQL `LIMIT a, b` (`OffsetCommaLimit`): 42601 at their position.
//! 2. `WITH`: `cte::bind_with(b, with, qctx.top_level)` -> `ctes` (frame popped at the end).
//! 3. Body (`SetExpr`):
//!    * `Select(sel)` -> [`bind_select_block`] with the query's ORDER BY (it needs the select scope
//!      for junk target entries), result gives `BoundSelect`, `Vec<SortKey>` and output columns.
//!    * `Values(v)` -> `values::bind_values_body` (ORDER BY by output ordinal/name only).
//!    * `SetOperation{..}` -> `setop::bind_set_operation`.
//!    * `Query(q2)` (parenthesized) -> `bind_query(q2)` and combine ORDER BY/LIMIT of the outer
//!      query (inner ORDER BY with outer LIMIT keeps both: wrap by making the inner the body of a
//!      `SetOp`-free passthrough: bind the inner query, then apply the outer ORDER BY by output
//!      column; if the inner already has order/limit and the outer has its own, nest through a
//!      subquery `RangeKind::Subquery` of a synthetic SELECT * — see PG which does the same via
//!      `transformSelectStmt` on a sub-select).
//!    * `Table(t)` (`TABLE name`) -> equivalent to `SELECT * FROM name`.
//!    * `Insert/Update/Delete/Merge` as a query body only occur inside CTEs (handled by `cte`); here
//!      -> 0A000/42601.
//! 4. ORDER BY on VALUES/set-operation bodies: `order::bind_order_by_output`.
//! 5. `LIMIT`/`OFFSET`/`FETCH`: `order::bind_limit_clause` (also decides `limit_with_ties`; `WITH TIES`
//!    requires ORDER BY: 42601 `WITH TIES cannot be specified without ORDER BY clause`).
//! 6. Locking: `FOR UPDATE|NO KEY UPDATE|SHARE|KEY SHARE [OF t,..] [NOWAIT|SKIP LOCKED]` ->
//!    `LockingClause`; errors 0A000 `FOR UPDATE is not allowed with DISTINCT clause` / `GROUP BY` /
//!    `HAVING` / `aggregate functions` / `window functions` / `set-returning functions in the target
//!    list` / `UNION/INTERSECT/EXCEPT` / `VALUES`? (`diag::locking_not_allowed`), `relation "x" in FOR
//!    UPDATE clause not found in FROM clause` (42P01, `diag::locking_relation_missing`).
//! 7. Pop the CTE frame; compute `BoundQuery.output`; `b.leave()`.
//!
//! ## Select block ([`bind_select_block`])
//! Order of work (matches `transformSelectStmt`, which fixes error precedence):
//! 1. Push `QueryScope::new(ScopeKind::Select)` onto `b.scopes`.
//! 2. FROM: `from::bind_from_clause` (builds range table, namespace, join tree).
//! 3. Follow PG's `transformSelectStmt` order exactly (it fixes which error wins when several apply):
//!    FROM (`WINDOW` definitions via `window::bind_window_clause` right after it) -> target list -> WHERE
//!    (`expr::bind_boolean(.., ExprCtx::where_(), "WHERE")`) -> HAVING
//!    (`ExprCtx::having()`, "HAVING") -> `group::bind_group_by` ->
//!    `group::resolve_pending_groupings` -> DISTINCT / DISTINCT ON + ORDER BY via
//!    `order::bind_order_by` (with `order::bind_distinct_on`) -> `group::check_grouping` (the 42803
//!    ungrouped-column check over target list, HAVING and ORDER BY expressions) -> unknown-type
//!    resolution of output entries (`coerce::resolve_unknown_output`, only for non-junk entries and
//!    only when `!qctx.keep_unknown_outputs`; `bind_select_block` therefore takes the flag).
//! 4. `SELECT ... INTO` is handled by [`bind_query_statement`], not here.
//! 5. Unsupported select-level syntax (all 42601 or 0A000 with the clause named): `TOP`,
//!    `PREWHERE`, `CONNECT BY`, `QUALIFY`, `CLUSTER BY`, `DISTRIBUTE BY`, `SORT BY`, `LATERAL VIEW`,
//!    `EXCLUDE`, `SELECT AS STRUCT`, optimizer hints, `select_modifiers`.
//! 6. Pop the scope into `BoundSelect { range_table, from, where_, target_list, group_by, having,
//!    distinct, windows, has_aggs, has_windows, has_target_srfs, has_sublinks }` and return it with
//!    the sort keys and `OutputColumn`s (name/type/origin of non-junk targets).
//!
//! ## Statement level
//! [`bind_query_statement`] wraps `bind_query` in `BoundStatement::Query`, and turns
//! `SELECT ... INTO [TEMP] t` into `BoundStatement::CreateTableAs` via
//! `ddl_table::bind_select_into` (TEMP: 0A000).
use pgarbor_catalog::model::RelKind;
use pgarbor_common::{PgError, Result, SqlState};
use sqlparser::ast::{
    Distinct, Ident, LimitClause, LockClause, LockType, NonBlock, ObjectName, ObjectNamePart, OrderByExpr, OrderByKind, Query,
    Select, SelectFlavor, SetExpr, Spanned, Table, TableFactor, TableWithJoins,
};

use super::context::{Binder, BoundArg, ExprCtx};
use super::scope::{QueryScope, ScopeKind};
use super::{coerce, cte, ddl_table, expr, from, group, order, scope, setop, target, values, window};
use crate::diag;
use crate::ir::{
    BoundExpr, BoundQuery, BoundSelect, BoundStatement, DistinctSpec, LockStrength, LockWait, LockingClause, OutputColumn,
    QueryBody, RangeEntry, RangeKind, SortKey, TargetEntry,
};

/// Options for [`bind_query`].
// CONTRACT
#[derive(Clone, Debug, Default)]
pub struct QueryCtx {
    /// Column alias list applied to the output names (`WITH x(a,b) AS (..)`, `(select ..) s(a,b)`,
    /// `CREATE VIEW v(a,b) AS`): more aliases than columns -> 42P10 (INVALID_COLUMN_REFERENCE) `WITH query "x" has N
    /// columns available but M columns specified` (`diag::too_many_column_aliases`).
    pub column_aliases: Vec<String>,
    /// The outermost statement query (data-modifying CTEs allowed).
    pub top_level: bool,
    /// Leave `unknown`-typed output columns (bare string literals / NULL / parameters) unresolved
    /// instead of coercing them to `text`; the caller coerces them to a known target type
    /// (INSERT ... SELECT, `UPDATE .. SET (a,b) = (SELECT ..)`). Normal queries: false.
    pub keep_unknown_outputs: bool,
}

/// Result of [`bind_select_block`].
// CONTRACT
#[derive(Clone, Debug, PartialEq)]
pub struct SelectBlock {
    pub select: BoundSelect,
    pub order_by: Vec<SortKey>,
    pub output: Vec<OutputColumn>,
}

/// Bind a complete query expression (module docs).
pub fn bind_query(b: &mut Binder, q: &Query, qctx: &QueryCtx) -> Result<BoundQuery> {
    if let Err(e) = b.enter() {
        b.leave();
        return Err(e);
    }
    let cte_base = b.cte_frames.len();
    let scope_base = b.scopes.len();
    let result = bind_query_levels(b, q, qctx, cte_base);
    b.cte_frames.truncate(cte_base);
    b.scopes.truncate(scope_base);
    b.leave();
    result
}

/// Bind one simple SELECT block together with the ORDER BY that belongs to it (module docs).
pub fn bind_select_block(b: &mut Binder, sel: &Select, order_by: &[OrderByExpr], keep_unknown_outputs: bool) -> Result<SelectBlock> {
    with_scope(b, ScopeKind::Select, |b| bind_select_in_scope(b, sel, order_by, keep_unknown_outputs))
}

/// `INTO` clause of the leftmost SELECT leaf of a query body (PG `transformSelectStmt` / gram.y: the
/// statement-level `SELECT ... INTO` of a set operation sits on its first SELECT). `None` when the
/// leftmost leaf has no `INTO`, including parenthesized leftmost operands.
pub(super) fn leftmost_select_into(body: &SetExpr) -> Option<&sqlparser::ast::SelectInto> {
    let mut cur = body;
    loop {
        match cur {
            SetExpr::Select(sel) => return sel.into.as_ref(),
            SetExpr::SetOperation { left, .. } => cur = &**left,
            _ => return None,
        }
    }
}

/// Top-level `Statement::Query`: query or `SELECT INTO` (also `SELECT INTO ... UNION SELECT ...`).
pub fn bind_query_statement(b: &mut Binder, q: &Query) -> Result<BoundStatement> {
    if let SetExpr::Merge(_) = &*q.body {
        // `WITH ... MERGE` (PG `transformMergeStmt`): WITH RECURSIVE is rejected; MERGE itself is not executed yet.
        if q.with.as_ref().is_some_and(|w| w.recursive) {
            return Err(PgError::new(SqlState::SYNTAX_ERROR, "WITH RECURSIVE is not supported for MERGE statement"));
        }
        return Err(diag::unsupported_construct("MERGE", None));
    }
    if let SetExpr::Insert(st) | SetExpr::Update(st) | SetExpr::Delete(st) = &*q.body {
        return bind_with_dml(b, q, st);
    }
    if let Some(into) = leftmost_select_into(&q.body) {
        return ddl_table::bind_select_into(b, q, into);
    }
    let ctx = QueryCtx { column_aliases: Vec::new(), top_level: true, keep_unknown_outputs: false };
    let bq = bind_query(b, q, &ctx)?;
    Ok(BoundStatement::Query(Box::new(bq)))
}

/// `WITH ... INSERT/UPDATE/DELETE`: the WITH list is bound at the top level (PG `transformWithClause`
/// on the DML statement), then the DML statement with the CTE frame visible; the CTEs go to the
/// statement's `ctes`.
fn bind_with_dml(b: &mut Binder, q: &Query, st: &sqlparser::ast::Statement) -> Result<BoundStatement> {
    if let Err(e) = b.enter() {
        b.leave();
        return Err(e);
    }
    let cte_base = b.cte_frames.len();
    let scope_base = b.scopes.len();
    let result = bind_with_dml_levels(b, q, st, cte_base);
    b.cte_frames.truncate(cte_base);
    b.scopes.truncate(scope_base);
    b.leave();
    result
}

fn bind_with_dml_levels(b: &mut Binder, q: &Query, st: &sqlparser::ast::Statement, cte_base: usize) -> Result<BoundStatement> {
    let with = q.with.as_ref().ok_or_else(|| PgError::internal("binder: data-modifying query body without WITH"))?;
    let mut ctes = cte::bind_with(b, with, true)?;
    let mut bound = super::dispatch::bind_inner_statement(b, st, false)?;
    if let Some(frame) = b.cte_frames.get(cte_base) {
        for entry in &frame.entries {
            if let Some(c) = ctes.get_mut(entry.index as usize) {
                c.ref_count = entry.ref_count;
            }
        }
    }
    match &mut bound {
        BoundStatement::Insert(x) => x.ctes = ctes,
        BoundStatement::Update(x) => x.ctes = ctes,
        BoundStatement::Delete(x) => x.ctes = ctes,
        _ => return Err(PgError::internal("binder: WITH on a statement that is not INSERT/UPDATE/DELETE")),
    }
    Ok(bound)
}

/// Bind a query that is used as a sub-select source (derived table, view body, CTE body) with
/// optional column aliases; thin wrapper over [`bind_query`] that sets `top_level = false`.
pub fn bind_subselect(b: &mut Binder, q: &Query, column_aliases: &[String]) -> Result<BoundQuery> {
    let ctx = QueryCtx { column_aliases: column_aliases.to_vec(), top_level: false, keep_unknown_outputs: false };
    bind_query(b, q, &ctx)
}

// ---------------------------------------------------------------------------------------------
// Query levels (WITH, body, tail, locking)
// ---------------------------------------------------------------------------------------------

/// Runs `f` with a fresh query scope pushed; the scope stack is restored on every path.
fn with_scope<T>(b: &mut Binder, kind: ScopeKind, f: impl FnOnce(&mut Binder) -> Result<T>) -> Result<T> {
    let base = b.scopes.len();
    b.scopes.push(QueryScope::new(kind));
    let out = f(b);
    b.scopes.truncate(base);
    out
}

fn bind_query_levels(b: &mut Binder, q: &Query, qctx: &QueryCtx, cte_base: usize) -> Result<BoundQuery> {
    reject_query_clauses(q)?;
    reject_ties_with_skip_locked(b, q)?;
    let mut ctes = match q.with.as_ref() {
        Some(with) => cte::bind_with(b, with, qctx.top_level)?,
        None => Vec::new(),
    };
    let order_items = order_by_items(q)?;
    let mut bq = match &*q.body {
        SetExpr::Query(inner) => bind_parenthesized(b, q, inner, qctx)?,
        _ => bind_plain_body(b, q, qctx, order_items)?,
    };
    if q.with.is_some() {
        // The frame pushed by `bind_with` is still present here (the caller truncates it).
        if let Some(frame) = b.cte_frames.get(cte_base) {
            for entry in &frame.entries {
                if let Some(bound) = ctes.get_mut(entry.index as usize) {
                    bound.ref_count = entry.ref_count;
                }
            }
        }
        bq.ctes = ctes;
    }
    apply_output_aliases(&mut bq.output, &qctx.column_aliases)?;
    Ok(bq)
}

/// A parenthesized query used as the whole body: its ORDER BY / LIMIT / OFFSET / FETCH / locking
/// clauses of the outer level are merged into it (PG `insertSelectOptions`), so no synthetic
/// wrapper level is needed.
fn bind_parenthesized(b: &mut Binder, q: &Query, inner: &Query, qctx: &QueryCtx) -> Result<BoundQuery> {
    if q.with.is_some() && inner.with.is_some() {
        return Err(diag::syntax("multiple WITH clauses not allowed", None));
    }
    let child = QueryCtx {
        column_aliases: Vec::new(),
        top_level: qctx.top_level,
        keep_unknown_outputs: qctx.keep_unknown_outputs,
    };
    let outer_tail = q.order_by.is_some() || q.limit_clause.is_some() || q.fetch.is_some() || !q.locks.is_empty();
    if !outer_tail {
        return bind_query(b, inner, &child);
    }
    let merged = merge_parenthesized(q, inner)?;
    bind_query(b, &merged, &child)
}

fn merge_parenthesized(outer: &Query, inner: &Query) -> Result<Query> {
    let mut merged = inner.clone();
    if let Some(ob) = outer.order_by.as_ref() {
        if inner.order_by.is_some() {
            return Err(diag::syntax("multiple ORDER BY clauses not allowed", None));
        }
        merged.order_by = Some(ob.clone());
    }
    merged.limit_clause = merge_limit(outer.limit_clause.as_ref(), inner.limit_clause.as_ref())?;
    if let Some(fetch) = outer.fetch.as_ref() {
        if inner.fetch.is_some() {
            return Err(diag::syntax("multiple LIMIT clauses not allowed", None));
        }
        merged.fetch = Some(fetch.clone());
    }
    if !outer.locks.is_empty() {
        merged.locks.extend(outer.locks.iter().cloned());
    }
    Ok(merged)
}

fn merge_limit(outer: Option<&LimitClause>, inner: Option<&LimitClause>) -> Result<Option<LimitClause>> {
    match (outer, inner) {
        (None, None) => Ok(None),
        (Some(o), None) => Ok(Some(o.clone())),
        (None, Some(i)) => Ok(Some(i.clone())),
        (
            Some(LimitClause::LimitOffset { limit: ol, offset: oo, limit_by: olb }),
            Some(LimitClause::LimitOffset { limit: il, offset: io, limit_by: ilb }),
        ) => {
            if !olb.is_empty() || !ilb.is_empty() {
                return Err(diag::syntax_error_near("BY", None));
            }
            if ol.is_some() && il.is_some() {
                return Err(diag::syntax("multiple LIMIT clauses not allowed", None));
            }
            if oo.is_some() && io.is_some() {
                return Err(diag::syntax("multiple OFFSET clauses not allowed", None));
            }
            Ok(Some(LimitClause::LimitOffset {
                limit: ol.clone().or_else(|| il.clone()),
                offset: oo.clone().or_else(|| io.clone()),
                limit_by: Vec::new(),
            }))
        }
        _ => Err(diag::syntax_error_near(",", None)),
    }
}

/// Body of a query that is not a parenthesized query: SELECT block, VALUES, set operation or TABLE.
fn bind_plain_body(b: &mut Binder, q: &Query, qctx: &QueryCtx, order_items: &[OrderByExpr]) -> Result<BoundQuery> {
    match &*q.body {
        SetExpr::Select(sel) => {
            let block = bind_select_block(b, sel, order_items, qctx.keep_unknown_outputs)?;
            let (limit, offset, with_ties) = bind_tail(b, q, !block.order_by.is_empty())?;
            let locking = bind_locking(b, &q.locks, LockSite::Select(&block.select))?;
            Ok(BoundQuery {
                ctes: Vec::new(),
                body: QueryBody::Select(Box::new(block.select)),
                output: block.output,
                order_by: block.order_by,
                limit,
                offset,
                limit_with_ties: with_ties,
                locking,
            })
        }
        SetExpr::Values(v) => {
            let bv = with_scope(b, ScopeKind::Empty, |b| values::bind_values_body(b, v, None))?;
            let output = values::values_output_columns(&bv, &[]);
            let order_by = order::bind_order_by_output(b, order_items, &output)?;
            let (limit, offset, with_ties) = bind_tail(b, q, !order_by.is_empty())?;
            let locking = bind_locking(b, &q.locks, LockSite::Values)?;
            Ok(BoundQuery {
                ctes: Vec::new(),
                body: QueryBody::Values(bv),
                output,
                order_by,
                limit,
                offset,
                limit_with_ties: with_ties,
                locking,
            })
        }
        SetExpr::SetOperation { op, set_quantifier, left, right } => {
            let child = QueryCtx { column_aliases: Vec::new(), top_level: false, keep_unknown_outputs: qctx.keep_unknown_outputs };
            let setop_bq = setop::bind_set_operation(b, op, set_quantifier, &**left, &**right, &child)?;
            let BoundQuery { body, output, .. } = setop_bq;
            let order_by = order::bind_order_by_output(b, order_items, &output)?;
            order::check_setop_sort_collations(b, &body, &order_by)?;
            let (limit, offset, with_ties) = bind_tail(b, q, !order_by.is_empty())?;
            let locking = bind_locking(b, &q.locks, LockSite::SetOp)?;
            Ok(BoundQuery { ctes: Vec::new(), body, output, order_by, limit, offset, limit_with_ties: with_ties, locking })
        }
        SetExpr::Table(t) => {
            let (body, output) = bind_table_body(b, t)?;
            let order_by = order::bind_order_by_output(b, order_items, &output)?;
            let (limit, offset, with_ties) = bind_tail(b, q, !order_by.is_empty())?;
            let site = match &body {
                QueryBody::Select(s) => LockSite::Select(&**s),
                _ => LockSite::SetOp,
            };
            let locking = bind_locking(b, &q.locks, site)?;
            Ok(BoundQuery { ctes: Vec::new(), body, output, order_by, limit, offset, limit_with_ties: with_ties, locking })
        }
        SetExpr::Query(_) => Err(PgError::internal("binder: parenthesized query reached the plain query body")),
        SetExpr::Insert(_) => Err(diag::syntax_error_near("INSERT", None)),
        SetExpr::Update(_) => Err(diag::syntax_error_near("UPDATE", None)),
        SetExpr::Delete(_) => Err(diag::syntax_error_near("DELETE", None)),
        SetExpr::Merge(_) => Err(diag::syntax_error_near("MERGE", None)),
    }
}

/// `LIMIT` / `OFFSET` / `FETCH`, bound without any column namespace.
fn bind_tail(b: &mut Binder, q: &Query, has_order: bool) -> Result<(Option<BoundExpr>, Option<BoundExpr>, bool)> {
    let (limit, offset, with_ties) =
        with_scope(b, ScopeKind::Empty, |b| order::bind_limit_clause(b, q.limit_clause.as_ref(), q.fetch.as_ref()))?;
    if with_ties && !has_order {
        return Err(diag::syntax("WITH TIES cannot be specified without ORDER BY clause", None));
    }
    Ok((limit, offset, with_ties))
}

/// `TABLE name` (and `TABLE schema.name`): the same as `SELECT * FROM name`.
fn bind_table_body(b: &mut Binder, t: &Table) -> Result<(QueryBody, Vec<OutputColumn>)> {
    let name = match t.table_name.as_ref() {
        Some(n) => n.clone(),
        None => return Err(diag::syntax_error_near("TABLE", None)),
    };
    let mut parts: Vec<ObjectNamePart> = Vec::with_capacity(2);
    if let Some(schema) = t.schema_name.as_ref() {
        parts.push(ObjectNamePart::Identifier(Ident::new(schema.clone())));
    }
    parts.push(ObjectNamePart::Identifier(Ident::new(name)));
    let twj = TableWithJoins {
        relation: TableFactor::Table {
            name: ObjectName(parts),
            alias: None,
            args: None,
            with_hints: Vec::new(),
            version: None,
            with_ordinality: false,
            partitions: Vec::new(),
            json_path: None,
            sample: None,
            index_hints: Vec::new(),
        },
        joins: Vec::new(),
    };
    with_scope(b, ScopeKind::Select, |b| {
        let from_items = from::bind_from_clause(b, std::slice::from_ref(&twj))?;
        let stars = scope::expand_star(b, None, None)?;
        let mut target_list: Vec<TargetEntry> = Vec::with_capacity(stars.len());
        for (col_name, expr) in stars {
            let origin = target::origin_of(b, &expr)?;
            target_list.push(TargetEntry { expr, name: col_name, junk: false, origin });
        }
        let output: Vec<OutputColumn> = target_list
            .iter()
            .map(|t| OutputColumn { name: t.name.clone(), ty: t.expr.ty, origin: t.origin })
            .collect();
        let range_table = std::mem::take(&mut b.scope_mut()?.range_table);
        let select = BoundSelect { range_table, from: from_items, target_list, ..BoundSelect::empty() };
        Ok((QueryBody::Select(Box::new(select)), output))
    })
}

/// Output column aliases (`WITH x(a, b) AS (...)`): renames the leading columns.
fn apply_output_aliases(output: &mut [OutputColumn], aliases: &[String]) -> Result<()> {
    if aliases.is_empty() {
        return Ok(());
    }
    if aliases.len() > output.len() {
        return Err(PgError::new(
            SqlState::INVALID_COLUMN_REFERENCE,
            format!("WITH query has {} columns available but {} columns specified", output.len(), aliases.len()),
        ));
    }
    for (col, name) in output.iter_mut().zip(aliases.iter()) {
        col.name = name.clone();
    }
    Ok(())
}

/// The query-level ORDER BY items (`ORDER BY ALL` is not PostgreSQL syntax).
fn order_by_items(q: &Query) -> Result<&[OrderByExpr]> {
    match q.order_by.as_ref() {
        None => {
            let empty: &[OrderByExpr] = &[];
            Ok(empty)
        }
        Some(ob) => match &ob.kind {
            OrderByKind::Expressions(items) => Ok(items.as_slice()),
            OrderByKind::All(_) => Err(diag::syntax_error_near("ALL", None)),
        },
    }
}

/// Non-PostgreSQL query clauses: syntax errors naming the keyword (42601).
fn reject_query_clauses(q: &Query) -> Result<()> {
    if q.for_clause.is_some() {
        return Err(diag::syntax_error_near("FOR", None));
    }
    if q.settings.is_some() {
        return Err(diag::syntax_error_near("SETTINGS", None));
    }
    if q.format_clause.is_some() {
        return Err(diag::syntax_error_near("FORMAT", None));
    }
    if !q.pipe_operators.is_empty() {
        return Err(diag::syntax_error_near("|>", None));
    }
    if let Some(lc) = q.limit_clause.as_ref() {
        match lc {
            LimitClause::LimitOffset { limit, limit_by, .. } => {
                if !limit_by.is_empty() {
                    return Err(diag::syntax_error_near("BY", None));
                }
                if limit.is_some() && q.fetch.is_some() {
                    return Err(diag::syntax_error_near("FETCH", None));
                }
            }
            LimitClause::OffsetCommaLimit { .. } => return Err(diag::syntax_error_near(",", None)),
        }
    }
    if let Some(ob) = q.order_by.as_ref() {
        if ob.interpolate.is_some() {
            return Err(diag::syntax_error_near("INTERPOLATE", None));
        }
        match &ob.kind {
            OrderByKind::All(_) => return Err(diag::syntax_error_near("ALL", None)),
            OrderByKind::Expressions(items) => {
                if items.iter().any(|item| item.with_fill.is_some()) {
                    return Err(diag::syntax_error_near("WITH", None));
                }
            }
        }
    }
    Ok(())
}

/// PG gram.y `insertSelectOptions`: `WITH TIES` cannot be combined with a `SKIP LOCKED` locking clause
/// (42601 at the `WITH` token, `limitClause->optionLoc`). The parser raises it before anything is bound.
/// Without ORDER BY the parser reports the missing ORDER BY first, so that case is left to `bind_tail`.
fn reject_ties_with_skip_locked(b: &Binder, q: &Query) -> Result<()> {
    let with_ties = q.fetch.as_ref().is_some_and(|f| f.with_ties);
    if !with_ties || !q.locks.iter().any(|l| matches!(l.nonblock, Some(NonBlock::SkipLocked))) {
        return Ok(());
    }
    let items = order_by_items(q)?;
    let anchor = match q.fetch.as_ref().and_then(|f| f.quantity.as_ref()).or_else(|| items.last().map(|i| &i.expr)) {
        Some(e) if !items.is_empty() => e,
        _ => return Ok(()),
    };
    let start = match b.src.byte_offset(anchor.span().end) {
        Some(s) => s,
        None => return Ok(()),
    };
    match find_with_ties_keyword(b.src.text(), start) {
        Some(at) => Err(diag::syntax("SKIP LOCKED and WITH TIES options cannot be used together", Some(b.src.pos_of_byte(at)))),
        None => Ok(()),
    }
}

/// Byte index of the first `WITH` word at or after `start` that is followed by `TIES` (a whole word,
/// ASCII case-insensitive). Used to locate the `WITH` token that follows the ORDER BY / FETCH text.
fn find_with_ties_keyword(text: &str, start: usize) -> Option<usize> {
    let bytes = text.as_bytes();
    let is_word = |c: u8| c.is_ascii_alphanumeric() || c == b'_';
    let mut i = start;
    while i + 4 <= bytes.len() {
        let whole = i == 0 || !is_word(bytes[i - 1]);
        if whole && bytes[i..i + 4].eq_ignore_ascii_case(b"WITH") && bytes.get(i + 4).is_some_and(|c| c.is_ascii_whitespace()) {
            let rest = text.get(i + 4..).unwrap_or("").trim_start();
            if rest.len() >= 4 && rest.as_bytes()[..4].eq_ignore_ascii_case(b"TIES") && rest.as_bytes().get(4).map_or(true, |&c| !is_word(c)) {
                return Some(i);
            }
        }
        i += 1;
    }
    None
}

/// SELECT-level syntax that PostgreSQL does not have (42601), or that is not supported (0A000).
fn reject_select_extensions(sel: &Select) -> Result<()> {
    if sel.into.is_some() {
        return Err(diag::syntax("SELECT ... INTO is not allowed here", None));
    }
    if sel.top.is_some() {
        return Err(diag::syntax_error_near("TOP", None));
    }
    if sel.prewhere.is_some() {
        return Err(diag::syntax_error_near("PREWHERE", None));
    }
    if !sel.connect_by.is_empty() {
        return Err(diag::syntax_error_near("CONNECT", None));
    }
    if sel.qualify.is_some() {
        return Err(diag::syntax_error_near("QUALIFY", None));
    }
    if !sel.cluster_by.is_empty() {
        return Err(diag::syntax_error_near("CLUSTER", None));
    }
    if !sel.distribute_by.is_empty() {
        return Err(diag::syntax_error_near("DISTRIBUTE", None));
    }
    if !sel.sort_by.is_empty() {
        return Err(diag::syntax_error_near("SORT", None));
    }
    if !sel.lateral_views.is_empty() {
        return Err(diag::syntax_error_near("LATERAL", None));
    }
    if sel.exclude.is_some() {
        return Err(diag::syntax_error_near("EXCLUDE", None));
    }
    if sel.value_table_mode.is_some() {
        return Err(diag::syntax_error_near("AS", None));
    }
    if !matches!(sel.flavor, SelectFlavor::Standard) {
        return Err(diag::syntax_error_near("FROM", None));
    }
    if sel.select_modifiers.is_some() {
        return Err(diag::syntax_error_near("SELECT", None));
    }
    if !sel.optimizer_hints.is_empty() {
        return Err(diag::unsupported_construct("optimizer hints", None));
    }
    Ok(())
}

// ---------------------------------------------------------------------------------------------
// SELECT block (PG transformSelectStmt order)
// ---------------------------------------------------------------------------------------------

fn bind_select_in_scope(b: &mut Binder, sel: &Select, order_items: &[OrderByExpr], keep_unknown_outputs: bool) -> Result<SelectBlock> {
    reject_select_extensions(sel)?;
    let from_items = from::bind_from_clause(b, &sel.from)?;
    // Named WINDOW definitions are bound after FROM (their PARTITION BY / ORDER BY reference its
    // columns) and before the target list, so that `OVER w` can refer to them.
    if !sel.named_window.is_empty() {
        window::bind_window_clause(b, &sel.named_window)?;
    }
    b.scope_mut()?.aggs_allowed_here = true;
    let mut target_list = target::bind_target_list(b, &sel.projection, ExprCtx::select_list(), keep_unknown_outputs)?;
    let where_ = match sel.selection.as_ref() {
        Some(e) => Some(expr::bind_boolean(b, e, ExprCtx::where_(), "WHERE")?),
        None => None,
    };
    let mut having = match sel.having.as_ref() {
        Some(e) => Some(expr::bind_boolean(b, e, ExprCtx::having(), "HAVING")?),
        None => None,
    };
    let group_by = group::bind_group_by(b, &sel.group_by, &mut target_list)?;
    let distinct_hint = if matches!(sel.distinct, Some(Distinct::Distinct)) { DistinctSpec::All } else { DistinctSpec::None };
    let sort_keys = order::bind_order_by(b, order_items, &mut target_list, &distinct_hint)?;
    let distinct = order::bind_distinct(b, sel.distinct.as_ref(), &mut target_list, &sort_keys)?;
    b.scope_mut()?.aggs_allowed_here = false;
    group::resolve_pending_groupings(b, &group_by, &mut target_list, having.as_mut())?;
    let windows = std::mem::take(&mut b.scope_mut()?.windows);
    let has_aggs = b.scope()?.has_aggs;
    group::check_grouping(b, &group_by, &target_list, having.as_ref(), &windows, has_aggs)?;
    if !keep_unknown_outputs {
        for entry in target_list.iter_mut() {
            if !entry.junk && entry.expr.ty.oid == pgarbor_types::oids::UNKNOWN {
                let arg = BoundArg::new(entry.expr.clone(), None);
                entry.expr = coerce::resolve_unknown_output(b, &arg)?;
            }
        }
    }
    let output: Vec<OutputColumn> = target_list
        .iter()
        .filter(|t| !t.junk)
        .map(|t| OutputColumn { name: t.name.clone(), ty: t.expr.ty, origin: t.origin })
        .collect();
    let sc = b.scope_mut()?;
    let range_table = std::mem::take(&mut sc.range_table);
    let select = BoundSelect {
        range_table,
        from: from_items,
        where_,
        target_list,
        group_by,
        having,
        distinct,
        windows,
        has_aggs,
        has_windows: sc.has_windows,
        has_target_srfs: sc.has_target_srfs,
        has_sublinks: sc.has_sublinks,
    };
    Ok(SelectBlock { select, order_by: sort_keys, output })
}

// ---------------------------------------------------------------------------------------------
// Locking clauses
// ---------------------------------------------------------------------------------------------

/// The query body a locking clause is attached to.
enum LockSite<'a> {
    Select(&'a BoundSelect),
    Values,
    SetOp,
}

fn bind_locking(b: &mut Binder, locks: &[LockClause], site: LockSite<'_>) -> Result<Vec<LockingClause>> {
    let mut out: Vec<LockingClause> = Vec::with_capacity(locks.len());
    for lock in locks {
        let clause = match lock.lock_type {
            LockType::Update => "FOR UPDATE",
            LockType::Share => "FOR SHARE",
        };
        let strength = match lock.lock_type {
            LockType::Update => LockStrength::Update,
            LockType::Share => LockStrength::Share,
        };
        match &site {
            LockSite::Values => {
                return Err(PgError::new(SqlState::FEATURE_NOT_SUPPORTED, format!("{clause} cannot be applied to VALUES")));
            }
            LockSite::SetOp => return Err(diag::locking_not_allowed(clause, "UNION/INTERSECT/EXCEPT", None)),
            LockSite::Select(s) => check_select_lockable(s, clause)?,
        }
        let wait = match lock.nonblock {
            None => LockWait::Block,
            Some(NonBlock::Nowait) => LockWait::NoWait,
            Some(NonBlock::SkipLocked) => LockWait::SkipLocked,
        };
        let of = match (&site, lock.of.as_ref()) {
            (LockSite::Select(s), Some(name)) => {
                let qn = b.qual(name)?;
                match s.range_table.iter().position(|entry| entry.alias == qn.name) {
                    Some(idx) => vec![idx as u32 + 1],
                    None => return Err(diag::locking_relation_missing(&qn.name, clause, qn.pos)),
                }
            }
            _ => Vec::new(),
        };
        if let LockSite::Select(s) = &site {
            check_lockable_relations(&s.range_table, &of)?;
        }
        out.push(LockingClause { strength, wait, of });
    }
    Ok(out)
}

/// PG `CheckValidRowMarkRel`: a relation that is row-locked must not be a materialized view or a
/// sequence (a materialized view may still be referenced without a lock). `of` holds the 1-based
/// range-table indexes named by `OF`; empty means every relation of the query level is locked.
fn check_lockable_relations(range_table: &[RangeEntry], of: &[u32]) -> Result<()> {
    for (i, entry) in range_table.iter().enumerate() {
        if !of.is_empty() && !of.contains(&(i as u32 + 1)) {
            continue;
        }
        if let RangeKind::Relation { name, rel_kind, .. } = &entry.kind {
            let what = match rel_kind {
                RelKind::MatView => "materialized view",
                RelKind::Sequence => "sequence",
                _ => continue,
            };
            return Err(PgError::new(
                SqlState::WRONG_OBJECT_TYPE,
                format!("cannot lock rows in {what} \"{name}\""),
            ));
        }
    }
    Ok(())
}

/// PG `CheckSelectLocking`: which SELECT features forbid `FOR UPDATE` / `FOR SHARE`.
fn check_select_lockable(s: &BoundSelect, clause: &str) -> Result<()> {
    let what = if !matches!(s.distinct, DistinctSpec::None) {
        "DISTINCT clause"
    } else if !s.group_by.exprs.is_empty() || s.group_by.sets.is_some() {
        "GROUP BY clause"
    } else if s.having.is_some() {
        "HAVING clause"
    } else if s.has_aggs {
        "aggregate functions"
    } else if s.has_windows {
        "window functions"
    } else if s.has_target_srfs {
        "set-returning functions in the target list"
    } else {
        return Ok(());
    };
    Err(diag::locking_not_allowed(clause, what, None))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::parser::parse_expr_text;

    fn lim(text: &str) -> LimitClause {
        let expr = parse_expr_text(text).expect("test expression parses");
        LimitClause::LimitOffset { limit: Some(expr), offset: None, limit_by: Vec::new() }
    }

    #[test]
    fn merge_limit_rejects_two_limits() {
        let a = lim("1");
        let b = lim("2");
        let err = merge_limit(Some(&a), Some(&b)).expect_err("two LIMIT clauses must fail");
        assert_eq!(err.message, "multiple LIMIT clauses not allowed");
    }

    #[test]
    fn merge_limit_keeps_single_limit() {
        let a = lim("1");
        let merged = merge_limit(Some(&a), None).expect("single limit merges");
        assert_eq!(merged, Some(a));
    }

    #[test]
    fn output_alias_count_is_checked() {
        let mut output: Vec<OutputColumn> = Vec::new();
        assert!(apply_output_aliases(&mut output, &[]).is_ok());
        let err = apply_output_aliases(&mut output, &["a".to_string()]).expect_err("more aliases than columns");
        assert_eq!(err.message, "WITH query has 0 columns available but 1 columns specified");
    }

    #[test]
    fn with_ties_keyword_is_found_after_fetch() {
        let text = "SELECT k FROM t ORDER BY k FETCH FIRST 1 ROW WITH TIES FOR UPDATE SKIP LOCKED";
        let at = find_with_ties_keyword(text, text.find("ORDER BY").unwrap_or(0)).expect("WITH TIES present");
        assert_eq!(&text[at..at + 4], "WITH");
        assert_eq!(at + 1, 46);
    }

    #[test]
    fn with_ties_keyword_skips_identifiers_that_contain_with() {
        let text = "SELECT withdrawn FROM t ORDER BY withdrawn FETCH FIRST 1 ROW ONLY";
        assert!(find_with_ties_keyword(text, 0).is_none());
    }

    fn rel_entry(name: &str, rel_kind: RelKind) -> RangeEntry {
        RangeEntry {
            kind: RangeKind::Relation {
                oid: 16384,
                schema: "public".to_string(),
                name: name.to_string(),
                rel_kind,
                only: false,
                pseudo: None,
            },
            alias: name.to_string(),
            columns: Vec::new(),
            lateral: false,
            visible: true,
            access: crate::ir::AccessNeeded::default(),
        }
    }

    #[test]
    fn row_lock_rejects_matview_but_allows_plain_reference() {
        let range = vec![rel_entry("t", RelKind::Table), rel_entry("mv", RelKind::MatView)];
        // Unrestricted FOR SHARE locks every relation of the level, including the matview.
        assert!(check_lockable_relations(&range, &[]).is_err());
        // OF restricted to the table leaves the matview as a plain reference.
        assert!(check_lockable_relations(&range, &[1]).is_ok());
        assert!(check_lockable_relations(&range, &[2]).is_err());
    }
}
