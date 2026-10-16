//! INSERT (SQL-26). PG references: `analyze.c` (`transformInsertStmt`, `transformInsertRow`,
//! `checkInsertTargets`, `transformOnConflictClause` caller), `parse_target.c`
//! (`transformInsertRow`'s `transformAssignedExpr`), `rewriteHandler.c` (`rewriteTargetListIU`:
//! DEFAULT, identity and generated column rules), `parse_clause.c` (`transformOnConflictArbiter`).
//!
//! `bind_insert(b, ins)` (sqlparser `Insert`):
//! 1. Reject non-PostgreSQL options (42601/0A000 naming them): `or` (SQLite), `ignore`,
//!    `overwrite`, `partitioned`, `after_columns`, `priority`, `insert_alias`, `replace_into`,
//!    `output`, `settings`, `format_clause`, `multi_table_*`, `OnInsert::DuplicateKeyUpdate`,
//!    `TableObject::TableFunction`, `assignments` (MySQL `INSERT .. SET`).
//! 2. Push `ScopeKind::Insert`; open the target (`target_table::open_target_table`, `DmlKind::Insert`).
//!    Column list: each name must be a live column (42703 `column "x" of relation "t" does not exist`,
//!    `diag::column_of_relation_missing`); duplicates 42701 `column "x" specified more than once`
//!    (`column_specified_twice`); `INSERT INTO t(a.b)` subscripts/fields unsupported (0A000).
//!    No list -> all non-dropped columns in attnum order (the implicit list EXCLUDES stored generated
//!    columns - oracle: `insert into g values (1)` works for `g(a int, b int generated always as (a*2)
//!    stored)`, while `values (1,2)` fails with 428C9).
//! 3. Source:
//!    * `source: None` or `DEFAULT VALUES` -> `InsertSource::DefaultValues` (`columns` empty).
//!    * `SetExpr::Values(v)` -> `values::bind_values_body(b, v, Some(targets))` (the `DEFAULT` keyword
//!      allowed per cell). Cell count mismatch: 42601 `INSERT has more expressions than target columns`
//!      (position = first surplus cell) / `INSERT has more target columns than expressions`.
//!    * any other query -> `select::bind_query` with `QueryCtx { keep_unknown_outputs: true, .. }` while the
//!      INSERT target is hidden from the sub-select (`RangeEntry::visible = false`). Then the width check
//!      (`insert_too_many_expressions` / `insert_too_many_columns`), then per-column assignment
//!      coercion of the query's output: `unknown`-typed literal outputs are converted directly to the
//!      target column type (`insert into t(int_col) select 'x'` -> 22P02 at the literal, as PG);
//!      otherwise, when an output type differs from the column type, wrap the query in a coercing
//!      projection (a `BoundQuery` whose body is a SELECT over one `RangeKind::Subquery` entry with
//!      assignment-cast target expressions), as the `InsertSource::Query` IR doc promises.
//! 4. Per-column rules (PG `rewriteTargetListIU`), applied to the bound cells for VALUES sources and
//!    enforced by the executor for query sources through `TargetColumn` metadata:
//!    * identity `GENERATED ALWAYS`: a non-DEFAULT cell is 428C9 `cannot insert a non-DEFAULT value into
//!      column "id"` DETAIL `Column "id" is an identity column defined as GENERATED ALWAYS.` HINT `Use
//!      OVERRIDING SYSTEM VALUE to override.` unless `OVERRIDING SYSTEM VALUE`; with `OVERRIDING USER
//!      VALUE` the supplied value is ignored (default used);
//!    * stored generated column: any non-DEFAULT cell is 428C9 `cannot insert a non-DEFAULT value into
//!      column "g"` DETAIL `Column "g" is a generated column.`;
//!    * `DEFAULT` cell -> `SetToDefault{attnum}` (planner substitutes the default expression).
//!    For a query source the check applies to the column list (columns named in the list that are
//!    GENERATED ALWAYS identity or generated: same 428C9 error unless overriding).
//! 5. `ON CONFLICT`: `on_conflict::bind_on_conflict` (needs the target and column list).
//! 6. `RETURNING`: `update_delete::bind_returning` (columns of the target, plus `excluded` is NOT visible).
//! 7. Pop the scope; result `BoundStatement::Insert(Box<BoundInsert { target, range_table, ctes,
//!    columns, source, overriding, on_conflict, returning }>)`. `WITH` on the INSERT (`ins.source`'s
//!    own `with`, or the statement-level WITH in sqlparser's `Query` wrapping): `cte::bind_with` with
//!    `top_level = true`; the CTEs go to `BoundInsert::ctes`.
//! Privileges: `access.insert_cols` = target columns written; `select` for ON CONFLICT/RETURNING refs.
use pgarbor_catalog::model::IdentityKind;
use pgarbor_common::{PgError, Result, SqlState};
use sqlparser::ast::{Insert, ObjectName, ObjectNamePart, OnInsert, Query, SetExpr, TableObject, Values};

use super::context::{BoundArg, Binder};
use super::coerce;
use super::on_conflict;
use super::scope::{mark_access, AccessKind, QueryScope, ScopeKind};
use super::select::{self, QueryCtx};
use super::setop;
use super::target_table::{self, DmlKind};
use super::update_delete;
use super::values::{self, InsertTargetCol};
use crate::diag;
use crate::ir::{
    AccessNeeded, BoundExpr, BoundInsert, BoundQuery, BoundSelect, BoundStatement, DistinctSpec, ExprKind, FromItem,
    GroupBy, InsertSource, OutputColumn, Overriding, QueryBody, RangeColumn, RangeEntry, RangeKind, TargetColumn,
    TargetEntry, TargetTable,
};

/// SQLSTATE 428C9 (`GENERATED_ALWAYS`): a non-DEFAULT value for an identity ALWAYS or generated column.
const GENERATED_ALWAYS: SqlState = SqlState(*b"428C9");

/// Bind an INSERT statement (module docs).
pub fn bind_insert(b: &mut Binder, ins: &Insert) -> Result<BoundStatement> {
    reject_foreign_syntax(b, ins)?;
    b.scopes.push(QueryScope::new(ScopeKind::Insert));
    let bound = bind_insert_in_scope(b, ins);
    let scope = b.scopes.pop();
    let mut bound = bound?;
    let scope = scope.ok_or_else(|| PgError::internal("binder: INSERT scope missing"))?;
    bound.range_table = scope.range_table;
    Ok(BoundStatement::Insert(Box::new(bound)))
}

/// Options that sqlparser accepts but PostgreSQL does not have (SQLite, MySQL, Hive, multi-table
/// forms). PostgreSQL reports these as a syntax error at the foreign keyword.
fn reject_foreign_syntax(b: &Binder, ins: &Insert) -> Result<()> {
    let pos = b.pos(ins);
    let bad = |word: &str| -> Result<()> { Err(diag::syntax_error_near(word, pos)) };
    if ins.or.is_some() {
        return bad("OR");
    }
    if ins.ignore {
        return bad("IGNORE");
    }
    if ins.overwrite {
        return bad("OVERWRITE");
    }
    if ins.replace_into {
        return bad("REPLACE");
    }
    if ins.partitioned.is_some() {
        return bad("PARTITION");
    }
    if let Some(first) = ins.after_columns.first() {
        return bad(first.value.as_str());
    }
    if ins.priority.is_some() {
        return bad("PRIORITY");
    }
    if ins.insert_alias.is_some() {
        return bad("AS");
    }
    if ins.output.is_some() {
        return bad("OUTPUT");
    }
    if ins.settings.is_some() {
        return bad("SETTINGS");
    }
    if ins.format_clause.is_some() {
        return bad("FORMAT");
    }
    if !ins.optimizer_hints.is_empty() {
        return bad("/*+");
    }
    if ins.has_table_keyword {
        return bad("TABLE");
    }
    if !ins.assignments.is_empty() {
        return bad("SET");
    }
    if ins.multi_table_insert_type.is_some()
        || !ins.multi_table_into_clauses.is_empty()
        || !ins.multi_table_when_clauses.is_empty()
        || ins.multi_table_else_clause.is_some()
    {
        return bad("ALL");
    }
    if !ins.into {
        let word = match &ins.table {
            TableObject::TableName(name) => name.to_string(),
            _ => String::new(),
        };
        return bad(word.as_str());
    }
    if !matches!(ins.table, TableObject::TableName(_)) {
        return bad("(");
    }
    if let Some(OnInsert::DuplicateKeyUpdate(_)) = &ins.on {
        return bad("DUPLICATE");
    }
    Ok(())
}

/// Body of INSERT, run with the INSERT scope already pushed. The caller takes the range table.
fn bind_insert_in_scope(b: &mut Binder, ins: &Insert) -> Result<BoundInsert> {
    let name: &ObjectName = match &ins.table {
        TableObject::TableName(name) => name,
        _ => return Err(diag::syntax_error_near("(", b.pos(ins))),
    };
    let alias = ins.table_alias.as_ref().map(|a| &a.alias);
    let (target, target_rt) = target_table::open_target_table(b, name, alias, DmlKind::Insert, true)?;

    // Explicit column list (attnum, position of the name), or None for the implicit list.
    let explicit: Option<Vec<(i16, Option<u32>)>> = if ins.columns.is_empty() {
        None
    } else {
        Some(resolve_column_list(b, &ins.columns, &target)?)
    };
    // sqlparser 0.63 has no OVERRIDING clause in its `Insert` node, so the binder never sees one.
    let overriding: Option<Overriding> = None;

    let (source, columns) = match ins.source.as_deref() {
        None => {
            if explicit.is_some() {
                return Err(diag::syntax_error_near("DEFAULT", b.pos(ins)));
            }
            (InsertSource::DefaultValues, Vec::new())
        }
        Some(query) => {
            let cols: Vec<(i16, Option<u32>)> = match &explicit {
                Some(list) => list.clone(),
                None => implicit_column_list(&target).into_iter().map(|a| (a, None)).collect(),
            };
            bind_source(b, query, &target, target_rt, &cols, explicit.is_some(), overriding)?
        }
    };

    // The target is visible again for ON CONFLICT and RETURNING (the source hid it).
    set_entry_visible(b, target_rt, true)?;
    mark_insert_columns(b, target_rt, &columns)?;

    let on_conflict = match &ins.on {
        Some(OnInsert::OnConflict(oc)) => Some(on_conflict::bind_on_conflict(b, oc, &target, &columns)?),
        _ => None,
    };
    if let Some(bound_oc) = &on_conflict {
        if bound_oc.excluded_rt != 0 {
            // `excluded` is visible in ON CONFLICT only, not in RETURNING (PG drops it from the
            // namespace after transformOnConflictClause): hide the entry and its namespace item.
            set_entry_visible(b, bound_oc.excluded_rt, false)?;
            let excluded_rt = bound_oc.excluded_rt;
            for item in b.scope_mut()?.namespace.iter_mut().filter(|ns| ns.rt_index == Some(excluded_rt)) {
                item.rel_visible = false;
                item.cols_visible = false;
            }
        }
    }

    let returning = update_delete::bind_returning(b, ins.returning.as_deref())?;

    Ok(BoundInsert {
        target,
        range_table: Vec::new(),
        ctes: Vec::new(),
        columns,
        source,
        overriding,
        on_conflict,
        returning,
    })
}

/// Explicit INSERT column list (PG `checkInsertTargets`): each name must be a live column (42703),
/// no duplicates (42701), and no subscript/field forms (0A000).
fn resolve_column_list(b: &mut Binder, names: &[ObjectName], target: &TargetTable) -> Result<Vec<(i16, Option<u32>)>> {
    let mut out: Vec<(i16, Option<u32>)> = Vec::with_capacity(names.len());
    for obj in names {
        let pos = b.pos(obj);
        let name: String = match obj.0.as_slice() {
            [ObjectNamePart::Identifier(id)] if id.value != crate::parser::SUBSCRIPT_TARGET_MARKER => b.ident(id),
            _ => {
                return Err(diag::unsupported_construct(
                    "assignment to a field or subscript of a column in INSERT",
                    pos,
                ))
            }
        };
        let attnum = match target.live_columns().find(|c| c.name == name) {
            Some(c) => c.attnum,
            None => return Err(diag::column_of_relation_missing(&name, &target.name, pos)),
        };
        if out.iter().any(|(a, _)| *a == attnum) {
            return Err(diag::column_specified_twice(&name, pos));
        }
        out.push((attnum, pos));
    }
    Ok(out)
}

/// Implicit column list: all live columns in attnum order, excluding generated columns (PG leaves
/// them out of the implicit list; `insert into g values (1)` works for a stored generated `b`).
fn implicit_column_list(target: &TargetTable) -> Vec<i16> {
    target.live_columns().filter(|c| c.generated.is_none()).map(|c| c.attnum).collect()
}

/// Bind the row source. Returns the source and the target attnums it fills, in source order.
/// With an implicit column list and fewer expressions than columns, only the leading columns are
/// filled and the rest take their defaults (PG `transformInsertRow` allows this without a list).
#[allow(clippy::too_many_arguments)]
fn bind_source(
    b: &mut Binder,
    query: &Query,
    target: &TargetTable,
    target_rt: u32,
    cols: &[(i16, Option<u32>)],
    explicit: bool,
    overriding: Option<Overriding>,
) -> Result<(InsertSource, Vec<i16>)> {
    let plain = query.with.is_none()
        && query.order_by.is_none()
        && query.limit_clause.is_none()
        && query.fetch.is_none()
        && query.locks.is_empty();
    if let (true, SetExpr::Values(vals)) = (plain, &*query.body) {
        return bind_values_source(b, vals, target, cols, explicit, overriding);
    }
    bind_query_source(b, query, target, target_rt, cols, explicit, overriding)
}

/// VALUES source: cells are assignment-coerced by `values::bind_values_body`; here the identity and
/// generated rules are applied per cell.
fn bind_values_source(
    b: &mut Binder,
    vals: &Values,
    target: &TargetTable,
    cols: &[(i16, Option<u32>)],
    explicit: bool,
    overriding: Option<Overriding>,
) -> Result<(InsertSource, Vec<i16>)> {
    let row_len = vals.rows.first().map(|r| r.content.len()).unwrap_or(cols.len());
    let used: &[(i16, Option<u32>)] = if !explicit && row_len < cols.len() { &cols[..row_len] } else { cols };
    let attnums: Vec<i16> = used.iter().map(|(a, _)| *a).collect();
    let mut targets: Vec<InsertTargetCol> = Vec::with_capacity(attnums.len());
    for &attnum in &attnums {
        let col = col_of(target, attnum)?;
        targets.push(InsertTargetCol { attnum, name: col.name.clone(), ty: col.ty });
    }
    let bound = values::bind_values_body(b, vals, Some(targets.as_slice()))?;
    let mut rows: Vec<Vec<BoundExpr>> = Vec::with_capacity(bound.rows.len());
    for row in bound.rows {
        let mut out: Vec<BoundExpr> = Vec::with_capacity(row.len());
        for (cell, &attnum) in row.into_iter().zip(attnums.iter()) {
            out.push(bind_value_cell(target, attnum, cell, overriding)?);
        }
        rows.push(out);
    }
    Ok((InsertSource::Values(rows), attnums))
}

/// Query source (`INSERT .. SELECT`, VALUES with extra clauses): the target is hidden from the
/// sub-select; unknown literal outputs take the target column type; other type differences are
/// bridged by a coercing projection.
fn bind_query_source(
    b: &mut Binder,
    query: &Query,
    target: &TargetTable,
    target_rt: u32,
    cols: &[(i16, Option<u32>)],
    explicit: bool,
    overriding: Option<Overriding>,
) -> Result<(InsertSource, Vec<i16>)> {
    set_entry_visible(b, target_rt, false)?;
    let qctx = QueryCtx { keep_unknown_outputs: true, ..QueryCtx::default() };
    let bound = select::bind_query(b, query, &qctx);
    set_entry_visible(b, target_rt, true)?;
    let mut q: BoundQuery = bound?;

    let width = q.width();
    if width > cols.len() {
        return Err(diag::insert_too_many_expressions(None));
    }
    if width < cols.len() && explicit {
        return Err(diag::insert_too_many_columns(cols[width].1));
    }
    let used: &[(i16, Option<u32>)] = &cols[..width];
    let attnums: Vec<i16> = used.iter().map(|(a, _)| *a).collect();
    // Source positions of the output columns, for errors raised by the per-column coercions.
    let positions = setop::column_positions(b, &*query.body, width);
    for &attnum in &attnums {
        // Query rows cannot say DEFAULT: identity ALWAYS / generated columns are errors unless
        // overridden (with OVERRIDING USER VALUE the executor ignores the supplied value).
        let col = col_of(target, attnum)?;
        supplied_fate(col, overriding)?;
    }

    {
        let BoundQuery { body, output, .. } = &mut q;
        if let QueryBody::Select(sel) = body {
            let mut k: usize = 0;
            for te in sel.target_list.iter_mut() {
                if te.junk {
                    continue;
                }
                let attnum = match attnums.get(k) {
                    Some(a) => *a,
                    None => break,
                };
                if te.expr.ty.oid == pgarbor_types::oids::UNKNOWN {
                    let col = col_of(target, attnum)?;
                    let arg = BoundArg::new(te.expr.clone(), positions.get(k).copied().flatten());
                    te.expr = coerce::coerce_assignment(b, &arg, &col.name, col.ty)?;
                    if let Some(o) = output.get_mut(k) {
                        o.ty = te.expr.ty;
                    }
                }
                k += 1;
            }
        }
    }

    let mut needs_wrap = false;
    for (i, &attnum) in attnums.iter().enumerate() {
        let col = col_of(target, attnum)?;
        if q.output.get(i).map(|o| o.ty) != Some(col.ty) {
            needs_wrap = true;
        }
    }
    let q = if needs_wrap { coercing_projection(b, q, target, &attnums, &positions)? } else { q };
    Ok((InsertSource::Query(Box::new(q)), attnums))
}

/// Wrap `inner` in `SELECT <col1 cast>, <col2 cast>, ... FROM (inner)` so that each output has its
/// target column type (assignment casts). The wrapper's range entry is the only one.
fn coercing_projection(
    b: &mut Binder,
    inner: BoundQuery,
    target: &TargetTable,
    attnums: &[i16],
    positions: &[Option<u32>],
) -> Result<BoundQuery> {
    let mut range_columns: Vec<RangeColumn> = Vec::with_capacity(inner.output.len());
    for o in &inner.output {
        range_columns.push(RangeColumn { name: o.name.clone(), ty: o.ty, dropped: false });
    }
    let mut target_list: Vec<TargetEntry> = Vec::with_capacity(attnums.len());
    let mut output: Vec<OutputColumn> = Vec::with_capacity(attnums.len());
    for (i, &attnum) in attnums.iter().enumerate() {
        let col = col_of(target, attnum)?;
        let src_out = inner.output.get(i).ok_or_else(|| PgError::internal("INSERT: projection width mismatch"))?;
        let src = BoundExpr::column(0, 1, (i + 1) as i16, src_out.ty);
        let expr = if src_out.ty == col.ty {
            src
        } else {
            let pos = positions.get(i).copied().flatten();
            coerce::coerce_assignment(b, &BoundArg::new(src, pos), &col.name, col.ty)?
        };
        output.push(OutputColumn { name: src_out.name.clone(), ty: expr.ty, origin: None });
        target_list.push(TargetEntry { expr, name: src_out.name.clone(), junk: false, origin: None });
    }
    let entry = RangeEntry {
        kind: RangeKind::Subquery { query: Box::new(inner) },
        alias: "*SELECT*".to_string(),
        columns: range_columns,
        lateral: false,
        visible: true,
        access: AccessNeeded::default(),
    };
    let sel = BoundSelect {
        range_table: vec![entry],
        from: vec![FromItem::Range(1)],
        where_: None,
        target_list,
        group_by: GroupBy::default(),
        having: None,
        distinct: DistinctSpec::None,
        windows: Vec::new(),
        has_aggs: false,
        has_windows: false,
        has_target_srfs: false,
        has_sublinks: false,
    };
    Ok(BoundQuery {
        ctes: Vec::new(),
        body: QueryBody::Select(Box::new(sel)),
        output,
        order_by: Vec::new(),
        limit: None,
        offset: None,
        limit_with_ties: false,
        locking: Vec::new(),
    })
}

/// What a supplied (non-DEFAULT) value means for a column.
enum SuppliedFate {
    /// The value is stored as given.
    Keep,
    /// The value is ignored and the column default is used (OVERRIDING USER VALUE on an identity).
    UseDefault,
}

/// PG `rewriteTargetListIU` rules for a non-DEFAULT value.
fn supplied_fate(col: &TargetColumn, overriding: Option<Overriding>) -> Result<SuppliedFate> {
    if matches!(col.identity, Some(IdentityKind::Always)) {
        return match overriding {
            Some(Overriding::System) => Ok(SuppliedFate::Keep),
            Some(Overriding::User) => Ok(SuppliedFate::UseDefault),
            None => Err(identity_always_error(&col.name)),
        };
    }
    if col.generated.is_some() {
        return Err(generated_column_error(&col.name));
    }
    Ok(SuppliedFate::Keep)
}

/// Apply [`supplied_fate`] to one bound VALUES cell.
fn bind_value_cell(target: &TargetTable, attnum: i16, cell: BoundExpr, overriding: Option<Overriding>) -> Result<BoundExpr> {
    if matches!(cell.kind, ExprKind::SetToDefault { .. }) {
        return Ok(cell);
    }
    let col = col_of(target, attnum)?;
    match supplied_fate(col, overriding)? {
        SuppliedFate::Keep => Ok(cell),
        SuppliedFate::UseDefault => Ok(BoundExpr::new(ExprKind::SetToDefault { attnum }, col.ty)),
    }
}

fn identity_always_error(name: &str) -> PgError {
    PgError::new(GENERATED_ALWAYS, format!("cannot insert a non-DEFAULT value into column \"{name}\""))
        .with_detail(format!("Column \"{name}\" is an identity column defined as GENERATED ALWAYS."))
        .with_hint("Use OVERRIDING SYSTEM VALUE to override.")
}

fn generated_column_error(name: &str) -> PgError {
    PgError::new(GENERATED_ALWAYS, format!("cannot insert a non-DEFAULT value into column \"{name}\""))
        .with_detail(format!("Column \"{name}\" is a generated column."))
}

fn col_of(target: &TargetTable, attnum: i16) -> Result<&TargetColumn> {
    target.column(attnum).ok_or_else(|| PgError::internal(format!("INSERT: no target column with attnum {attnum}")))
}

/// Range-table index (1-based) to vector index.
fn range_index(rt: u32) -> Result<usize> {
    (rt as usize).checked_sub(1).ok_or_else(|| PgError::internal("INSERT: range-table index 0"))
}

fn set_entry_visible(b: &mut Binder, rt: u32, visible: bool) -> Result<()> {
    let idx = range_index(rt)?;
    let scope = b.scope_mut()?;
    match scope.range_table.get_mut(idx) {
        Some(entry) => {
            entry.visible = visible;
            Ok(())
        }
        None => Err(PgError::internal(format!("INSERT: no range-table entry {rt}"))),
    }
}

fn mark_insert_columns(b: &mut Binder, target_rt: u32, columns: &[i16]) -> Result<()> {
    let idx = range_index(target_rt)?;
    let scope = b.scope_mut()?;
    let entry = scope.range_table.get_mut(idx).ok_or_else(|| PgError::internal("INSERT: missing target entry"))?;
    for &attnum in columns {
        mark_access(&mut entry.access, AccessKind::Insert, attnum);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use pgarbor_catalog::model::GeneratedKind;
    use pgarbor_types::TypeId;

    fn column(attnum: i16, name: &str, identity: Option<IdentityKind>, generated: bool) -> TargetColumn {
        TargetColumn {
            attnum,
            name: name.to_string(),
            ty: TypeId::INT4,
            dropped: false,
            not_null: false,
            collation: 0,
            default: None,
            identity,
            generated: if generated {
                Some((GeneratedKind::Stored, BoundExpr::bool_const(true)))
            } else {
                None
            },
        }
    }

    fn table(columns: Vec<TargetColumn>) -> TargetTable {
        TargetTable {
            oid: 16384,
            namespace: 2200,
            schema: "public".to_string(),
            name: "t".to_string(),
            kind: pgarbor_catalog::model::RelKind::Table,
            columns,
            checks: Vec::new(),
        }
    }

    #[test]
    fn identity_always_error_text() {
        let e = identity_always_error("id");
        assert_eq!(e.sqlstate.code(), "428C9");
        assert_eq!(e.message, "cannot insert a non-DEFAULT value into column \"id\"");
        assert_eq!(e.detail.as_deref(), Some("Column \"id\" is an identity column defined as GENERATED ALWAYS."));
        assert_eq!(e.hint.as_deref(), Some("Use OVERRIDING SYSTEM VALUE to override."));
    }

    #[test]
    fn generated_column_error_text() {
        let e = generated_column_error("g");
        assert_eq!(e.sqlstate.code(), "428C9");
        assert_eq!(e.message, "cannot insert a non-DEFAULT value into column \"g\"");
        assert_eq!(e.detail.as_deref(), Some("Column \"g\" is a generated column."));
        assert!(e.hint.is_none());
    }

    #[test]
    fn supplied_fate_rules() {
        let always = column(1, "id", Some(IdentityKind::Always), false);
        assert!(supplied_fate(&always, None).is_err());
        assert!(matches!(supplied_fate(&always, Some(Overriding::System)), Ok(SuppliedFate::Keep)));
        assert!(matches!(supplied_fate(&always, Some(Overriding::User)), Ok(SuppliedFate::UseDefault)));
        let generated = column(2, "g", None, true);
        assert!(supplied_fate(&generated, Some(Overriding::System)).is_err());
        let plain = column(3, "v", None, false);
        assert!(matches!(supplied_fate(&plain, None), Ok(SuppliedFate::Keep)));
    }

    #[test]
    fn implicit_list_skips_generated_columns() {
        let t = table(vec![
            column(1, "a", None, false),
            column(2, "b", None, true),
            column(3, "c", Some(IdentityKind::ByDefault), false),
        ]);
        assert_eq!(implicit_column_list(&t), vec![1, 3]);
    }

    #[test]
    fn default_cell_for_overridden_identity() {
        let t = table(vec![column(1, "id", Some(IdentityKind::Always), false)]);
        let cell = BoundExpr::constant(pgarbor_types::Datum::Null, TypeId::INT4);
        let out = bind_value_cell(&t, 1, cell, Some(Overriding::User)).ok();
        assert_eq!(out.map(|e| e.kind), Some(ExprKind::SetToDefault { attnum: 1 }));
    }
}
