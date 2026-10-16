//! Query scopes, the FROM-clause namespace and column-reference resolution (SQL-05, sonnet).
//! PG references: `parse_relation.c` (`scanNameSpaceForRefname`, `scanNSItemForColumn`,
//! `colNameToVar`, `refnameNamespaceItem`, `expandNSItemVars`, `errorMissingColumn`,
//! `errorMissingRTE`, `searchRangeTableForCol`), `parse_expr.c` (`transformColumnRef`),
//! `parse_target.c` (`ExpandColumnRefStar`, `ExpandAllTables`).
//!
//! ## Model
//! One [`QueryScope`] per query level being bound (SELECT block, INSERT/UPDATE/DELETE statement,
//! stored-expression context). The binder keeps them in `Binder::scopes`; a reference that is found
//! `n` scopes out gets `ColumnRef::levels_up = n` (correlated reference). The scope owns the
//! `range_table` that ends up in `BoundSelect::range_table` / `BoundInsert::range_table` / ...
//! Range entries are 1-based (`rt_index`); column `i` (0-based position in `RangeEntry::columns`) has
//! attnum `i + 1`.
//!
//! The *namespace* (PG `p_namespace`, a list of [`NsItem`]) says which range entries and columns can
//! be named at the current point of the FROM clause. It changes while FROM is processed:
//! * a plain table/subquery/function item adds one NsItem with its alias and columns;
//! * a join adds ONE extra NsItem (no alias unless `JOIN ... AS j`) whose columns are the join output
//!   columns (USING/NATURAL merged columns first), and marks its two inputs `cols_visible = false`
//!   (still `rel_visible` so `a.id` works; unqualified `id` uses the merged column);
//! * LATERAL: items already processed in the same FROM list are visible to a later LATERAL item
//!   (`lateral_ok`), invisible otherwise (`lateral_only` items are visible only to LATERAL items:
//!   error 42P01 `invalid reference to FROM-clause entry for table "t"` + HINT, see
//!   `diag::invalid_from_reference`).
//!
//! ## Column resolution rules (`resolve_column`)
//! * Unqualified `c`: search the innermost scope's visible-column items; exactly one match ->
//!   Column; two or more -> 42702 `column reference "c" is ambiguous`; none -> look at the next outer
//!   scope (levels_up + 1) and so on; none anywhere -> 42703 `column "c" does not exist` (with the
//!   `Perhaps you meant to reference the column "t.cc".` HINT per `errorMissingColumn` /
//!   `searchRangeTableForCol` / `updateFuzzyAttrMatchState` in `parse_relation.c` (read it; the
//!   fuzzy-match rule is Levenshtein-based with MAX_FUZZY_DISTANCE 3). The hint is best-effort: the
//!   SQLSTATE, message and position are what differential tests compare.
//! * Qualified `t.c`: find the NsItem named `t` (rel_visible) in the innermost scope, else outer
//!   scopes; missing -> 42P01 `missing FROM-clause entry for table "t"`; found but column missing ->
//!   42703 `column t.c does not exist`; `schema.t.c` additionally compares the schema and relation OID.
//! * `t` alone naming a range entry (no column of that name): whole-row reference
//!   (`attnum 0`, type = rowtype or `record`). `t.*` is handled by [`expand_star`].
//! * System columns `ctid`, `xmin`, `cmin`, `xmax`, `cmax`, `tableoid` resolve against plain
//!   relation entries (negative attnums; `ctid` type `tid` is unsupported as a Datum -> 0A000 when
//!   used; `tableoid` is `oid`).
//! * Marks `RangeEntry::access.select_cols` (for the ACL check) on every successful reference.
//! * JOIN USING merged columns: the NsColumn's `expr` is already the replacement expression
//!   (`COALESCE(l.id, r.id)` for FULL JOIN, left column for INNER/LEFT, right for RIGHT).
use pgarbor_common::{PgError, Result, SqlState};
use pgarbor_types::TypeId;

use super::context::Binder;
use super::stored::virtual_generation_exprs;
use crate::diag;
use crate::ir::{AccessNeeded, BoundExpr, ColumnRef, ExprKind, RangeEntry, RangeKind, WindowDef};
use crate::names::QualName;

/// What kind of statement owns the scope (affects which pseudo columns/hidden rules apply).
// CONTRACT
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ScopeKind {
    Select,
    Insert,
    Update,
    Delete,
    /// Expression-only context over one table (CHECK, DEFAULT with columns, index expression,
    /// generated column, COPY WHERE, policy): `rt_index 1` is the table.
    TableExpr,
    /// No columns at all (LIMIT, EXECUTE args, VALUES in INSERT).
    Empty,
}

/// A visible column of a namespace item.
// CONTRACT
#[derive(Clone, Debug, PartialEq)]
pub struct NsColumn {
    pub name: String,
    /// What a reference to this column evaluates to, with `levels_up = 0` (shifted by the resolver
    /// when found in an outer scope). A plain table column is `Column{rt_index, attnum}`; a merged
    /// USING column is `Coalesce(..)` or one side's column.
    pub expr: BoundExpr,
    /// Dropped table column: occupies an attnum, never matches a name, skipped by `*`.
    pub dropped: bool,
    /// Visible only through qualification (the non-merged copies of USING columns under their
    /// original table names are NOT hidden; this flag is for `NATURAL`/`USING` duplicates that must
    /// not appear in `SELECT *` but remain addressable as `t.c`).
    pub hidden_from_star: bool,
}

/// One entry of the FROM namespace. PG `ParseNamespaceItem`.
// CONTRACT
#[derive(Clone, Debug, PartialEq)]
pub struct NsItem {
    /// Name that qualifies the columns (table alias, table name, join alias); `None` for a join
    /// without alias (not addressable by name).
    pub alias: Option<String>,
    /// Schema of a plain relation item (for `schema.table.col`); `None` otherwise.
    pub schema: Option<String>,
    /// Range-table index (1-based) when the item is a range entry; `None` for a join item.
    pub rt_index: Option<u32>,
    pub columns: Vec<NsColumn>,
    /// `alias.col` references are allowed.
    pub rel_visible: bool,
    /// Unqualified `col` references are allowed.
    pub cols_visible: bool,
    /// Visible only to LATERAL items later in the same FROM list.
    pub lateral_only: bool,
    /// May be referenced by a LATERAL item (set while processing the FROM list).
    pub lateral_ok: bool,
}

/// Grouping facts the aggregate checker needs; filled by `group::bind_group_by`.
// CONTRACT
#[derive(Clone, Debug, Default, PartialEq)]
pub struct GroupingInfo {
    /// The bound grouping expressions (distinct, in `GroupBy::exprs` order).
    pub exprs: Vec<BoundExpr>,
    /// The query has a GROUP BY clause (possibly empty `GROUP BY ()`), HAVING, or aggregates.
    pub is_grouped: bool,
}

/// One query level under construction.
// CONTRACT
#[derive(Clone, Debug, PartialEq)]
pub struct QueryScope {
    pub kind: ScopeKind,
    pub range_table: Vec<RangeEntry>,
    pub namespace: Vec<NsItem>,
    pub grouping: GroupingInfo,
    /// Argument expressions of `GROUPING(..)` calls seen so far (see `expr_agg`, `group`).
    pub pending_grouping: Vec<BoundExpr>,
    /// Named windows from the `WINDOW` clause (merged definitions), by index.
    pub windows: Vec<WindowDef>,
    pub has_aggs: bool,
    pub has_windows: bool,
    pub has_target_srfs: bool,
    pub has_sublinks: bool,
    /// Binding the select list / ORDER BY: aggregates of this level may be added.
    pub aggs_allowed_here: bool,
    /// Lateral-visibility generation counter used by `from`/`join` while processing a FROM list.
    pub from_depth: u32,
    /// Expansions of VIRTUAL generated columns, with the stored column they stand for (result
    /// metadata `table_oid`/`column_attnum`, see `target::origin_of`).
    pub virtual_origins: Vec<(BoundExpr, crate::ir::ColumnOrigin)>,
}

impl QueryScope {
    /// Empty scope of the given kind.
    pub fn new(kind: ScopeKind) -> QueryScope {
        QueryScope {
            kind,
            range_table: Vec::new(),
            namespace: Vec::new(),
            grouping: GroupingInfo::default(),
            pending_grouping: Vec::new(),
            windows: Vec::new(),
            has_aggs: false,
            has_windows: false,
            has_target_srfs: false,
            has_sublinks: false,
            aggs_allowed_here: false,
            from_depth: 0,
            virtual_origins: Vec::new(),
        }
    }
}

/// Result of resolving a name to an expression.
// CONTRACT
#[derive(Clone, Debug, PartialEq)]
pub struct ResolvedColumn {
    pub expr: BoundExpr,
    /// Scopes out from the innermost (0 = current query).
    pub levels_up: u32,
    /// Display name for grouping errors: (range-entry alias, column name).
    pub display: (String, String),
}

/// Push `entry` on the innermost scope's range table, returning its 1-based index. Does not touch
/// the namespace.
pub fn add_range_entry(b: &mut Binder, entry: RangeEntry) -> Result<u32> {
    let scope = b.scope_mut()?;
    scope.range_table.push(entry);
    u32::try_from(scope.range_table.len()).map_err(|_| PgError::internal("binder: range table too large"))
}

/// Build the [`NsItem`] for range entry `rt_index` (columns = the entry's visible columns, each
/// `Column{rt_index, attnum}` typed from `RangeColumn::ty`) and add it to the namespace. Checks
/// alias uniqueness within the current FROM list: 42712 `table name "t" specified more than once`
/// (`diag::duplicate_alias`; items hidden by a join alias do not count).
pub fn add_namespace_for_range(b: &mut Binder, rt_index: u32, rel_visible: bool, cols_visible: bool) -> Result<()> {
    let mut item = range_nsitem(b.scope()?, rt_index, rel_visible, cols_visible)?;
    // PG expand_generated_columns_in_expr: a VIRTUAL generated column of a stored relation reads as its
    // generation expression (table expression scopes keep the stored slots: that is where the
    // expressions themselves are bound).
    if let Some(rel_oid) = expandable_relation(b, rt_index)? {
        for (slot, expr) in virtual_generation_exprs(b, rel_oid, rt_index)? {
            if let Some(col) = item.columns.get_mut(slot) {
                if let Ok(attnum) = attnum_of(slot) {
                    b.scope_mut()?.virtual_origins.push((expr.clone(), crate::ir::ColumnOrigin { table: rel_oid, attnum }));
                }
                col.expr = expr;
            }
        }
    }
    if rel_visible {
        if let Some(alias) = item.alias.as_deref() {
            // PG checkNameSpaceConflicts: two visible relation items with the same name in one FROM list.
            let clash = b
                .scope()?
                .namespace
                .iter()
                .any(|ns| ns.rel_visible && ns.cols_visible && ns.alias.as_deref() == Some(alias));
            if clash {
                return Err(diag::duplicate_alias(alias, None));
            }
        }
    }
    b.scope_mut()?.namespace.push(item);
    Ok(())
}

/// Resolve `qualifier.name` (qualifier empty for a bare identifier; at most `[schema, table]`) at
/// `pos`. Implements the rules in the module docs including outer scopes, whole-row references,
/// system columns, ambiguity and all PG error texts. Returns `Ok(None)` ONLY when nothing matches
/// and `want_error` is false (used by ORDER BY / GROUP BY output-name matching and by field
/// selection `(a).b` probing); otherwise the 42703/42P01 error.
pub fn resolve_column(
    b: &mut Binder,
    qualifier: &[String],
    name: &str,
    pos: Option<u32>,
    want_error: bool,
) -> Result<Option<ResolvedColumn>> {
    if b.scopes.is_empty() {
        return Err(PgError::internal("binder: column reference outside any query scope"));
    }
    match qualifier {
        [] => {
            if let Some(found) = search_unqualified(b, name, pos)? {
                return finalize(b, found, pos).map(Some);
            }
            // PG transformColumnRef: a bare name that is not a column may name a relation (whole row).
            if let Some(row) = whole_row(b, name, pos)? {
                return Ok(Some(row));
            }
        }
        [table] => return qualified_column(b, None, table.as_str(), name, pos, want_error),
        [schema, table] => return qualified_column(b, Some(schema.as_str()), table.as_str(), name, pos, want_error),
        _ => {
            let full = format!("{}.{}", qualifier.join("."), name);
            return Err(at(
                PgError::new(SqlState::SYNTAX_ERROR, format!("improper qualified name (too many dotted names): {full}")),
                pos,
            ));
        }
    }
    if want_error {
        Err(error_missing_column(b, None, name, pos))
    } else {
        Ok(None)
    }
}

/// Expand `*` (qualifier `None`) or `t.*` into `(output name, expression)` pairs in column order,
/// skipping dropped and `hidden_from_star` columns. `SELECT *` with an empty FROM clause is 42601
/// `SELECT * with no tables specified is not valid`; `t.*` with unknown `t` is 42P01 `missing
/// FROM-clause entry for table "t"`. Marks `select_cols` access. For `t.*` where `t` names a
/// whole-row expression of composite type the caller (`target`) uses `expr_misc::expand_composite`.
pub fn expand_star(b: &mut Binder, qualifier: Option<&QualName>, pos: Option<u32>) -> Result<Vec<(String, BoundExpr)>> {
    let Some(q) = qualifier else {
        return star_all(b, pos);
    };
    if let Some(catalog) = &q.catalog {
        return Err(diag::cross_database(&format!("{catalog}.{}", q.display()), pos));
    }
    let table = q.name.as_str();
    let Some((level, ii)) = find_rel_item(b, q.schema_str(), table, pos)? else {
        return Err(error_missing_rte(b, table, pos));
    };
    let depth = scope_depth(b, level);
    let out: Vec<(String, BoundExpr)> = {
        let scope = scope_at(b, level)?;
        let item = scope.namespace.get(ii).ok_or_else(|| PgError::internal("binder: namespace index out of range"))?;
        let mut out = Vec::with_capacity(item.columns.len());
        for col in &item.columns {
            if col.dropped {
                continue;
            }
            // A join item's output excludes the non-merged duplicate USING/NATURAL columns.
            if item.rt_index.is_none() && col.hidden_from_star {
                continue;
            }
            out.push((col.name.clone(), col.expr.clone()));
        }
        out
    };
    let mut refs: Vec<ColumnRef> = Vec::new();
    for (_, expr) in &out {
        refs.extend(collect_refs(expr));
    }
    let scope = b.scopes.get_mut(level).ok_or_else(|| PgError::internal("binder: scope index out of range"))?;
    mark_refs(scope, &refs);
    let mut shifted = Vec::with_capacity(out.len());
    for (name, expr) in out {
        shifted.push((name, shift_levels(expr, depth)?));
    }
    Ok(shifted)
}

/// `SELECT *` / unqualified star of the innermost level: every visible column of every visible item.
fn star_all(b: &mut Binder, pos: Option<u32>) -> Result<Vec<(String, BoundExpr)>> {
    let level = b.scopes.len().checked_sub(1).ok_or_else(|| PgError::internal("binder: no open query scope"))?;
    let (out, refs): (Vec<(String, BoundExpr)>, Vec<ColumnRef>) = {
        let scope = scope_at(b, level)?;
        if !scope.namespace.iter().any(|item| item.cols_visible) {
            return Err(at(PgError::new(SqlState::SYNTAX_ERROR, "SELECT * with no tables specified is not valid"), pos));
        }
        let mut out = Vec::new();
        let mut refs = Vec::new();
        for item in &scope.namespace {
            if !item.cols_visible || lateral_hidden(item) {
                continue;
            }
            for col in &item.columns {
                if col.dropped || col.hidden_from_star {
                    continue;
                }
                refs.extend(collect_refs(&col.expr));
                out.push((col.name.clone(), col.expr.clone()));
            }
        }
        (out, refs)
    };
    let scope = b.scopes.get_mut(level).ok_or_else(|| PgError::internal("binder: scope index out of range"))?;
    mark_refs(scope, &refs);
    Ok(out)
}

/// Does a range entry or join alias named `name` exist in the innermost scope's namespace
/// (rel_visible)? Used for `t.*`/whole-row probing and for UPDATE/DELETE alias checks.
/// Note: a join alias has no range-table index, so it is reported as `Ok(None)` (the alias exists but
/// names no range entry); use `Binder::scope()` to probe the namespace directly when that matters.
pub fn find_relation_alias(b: &Binder, name: &str) -> Result<Option<u32>> {
    let scope = b.scope()?;
    let mut hit: Option<Option<u32>> = None;
    for item in &scope.namespace {
        if !item.rel_visible || lateral_hidden(item) || item.alias.as_deref() != Some(name) {
            continue;
        }
        if hit.is_some() {
            return Err(PgError::new(AMBIGUOUS_ALIAS, format!("table reference \"{name}\" is ambiguous")));
        }
        hit = Some(item.rt_index);
    }
    Ok(hit.flatten())
}

/// Record `ACL` needs: mark column `attnum` of `rt_index` (of the innermost scope) as read, written
/// or inserted. `AccessNeeded` flags/columns are merged (dedup, ascending).
pub fn mark_access(access: &mut AccessNeeded, kind: AccessKind, attnum: i16) {
    let (flag, list): (&mut bool, &mut Vec<i16>) = match kind {
        AccessKind::Select => (&mut access.select, &mut access.select_cols),
        AccessKind::Insert => (&mut access.insert, &mut access.insert_cols),
        AccessKind::Update => (&mut access.update, &mut access.update_cols),
    };
    *flag = true;
    if attnum > 0 && !list.contains(&attnum) {
        list.push(attnum);
        list.sort_unstable();
    }
}

/// Which privilege a column reference needs.
// CONTRACT
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AccessKind {
    Select,
    Insert,
    Update,
}

// ------------------------------------------------------------------------------------------------
// Private helpers (SQL-05)

/// PG `MAX_FUZZY_DISTANCE` (parse_relation.c).
const MAX_FUZZY_DISTANCE: usize = 3;
/// 42P09 `table reference "x" is ambiguous`.
const AMBIGUOUS_ALIAS: SqlState = SqlState(*b"42P09");
/// PG `MaxTupleAttributeNumber`: 54011 `tables can have at most 1600 columns`.
const MAX_COLUMNS: usize = 1600;
/// PG `TableOidAttributeNumber`.
const TABLEOID_ATTNUM: i16 = -6;

/// Attach a position when one is known.
fn at(e: PgError, pos: Option<u32>) -> PgError {
    match pos {
        Some(p) => e.with_position(p),
        None => e,
    }
}

/// Range entry for a 1-based range-table index.
fn entry_at(scope: &QueryScope, rt_index: u32) -> Option<&RangeEntry> {
    let idx = (rt_index as usize).checked_sub(1)?;
    scope.range_table.get(idx)
}

fn scope_at<'b>(b: &'b Binder<'_>, level: usize) -> Result<&'b QueryScope> {
    b.scopes.get(level).ok_or_else(|| PgError::internal("binder: query scope index out of range"))
}

/// `levels_up` of the scope at index `level` counted from the innermost scope.
fn scope_depth(b: &Binder, level: usize) -> u32 {
    b.scopes.len().saturating_sub(1).saturating_sub(level) as u32
}

/// Attnum (1-based) of the column at position `index` of a range entry.
fn attnum_of(index: usize) -> Result<i16> {
    if index >= MAX_COLUMNS {
        return Err(PgError::new(SqlState(*b"54011"), format!("tables can have at most {MAX_COLUMNS} columns")));
    }
    Ok((index + 1) as i16)
}

/// LATERAL-only items are invisible unless the referencing item is itself LATERAL-visible.
fn lateral_hidden(item: &NsItem) -> bool {
    item.lateral_only && !item.lateral_ok
}

/// OID of the stored relation behind range entry `rt_index`, when its columns may need the VIRTUAL
/// generation expansion (`add_namespace_for_range`). `None` inside a table expression scope.
fn expandable_relation(b: &Binder, rt_index: u32) -> Result<Option<pgarbor_common::Oid>> {
    let scope = b.scope()?;
    if scope.kind == ScopeKind::TableExpr {
        return Ok(None);
    }
    Ok(match entry_at(scope, rt_index).map(|e| &e.kind) {
        Some(RangeKind::Relation { oid, .. }) => Some(*oid),
        _ => None,
    })
}

/// Namespace item for range entry `rt_index`.
fn range_nsitem(scope: &QueryScope, rt_index: u32, rel_visible: bool, cols_visible: bool) -> Result<NsItem> {
    let entry = entry_at(scope, rt_index)
        .ok_or_else(|| PgError::internal("binder: namespace registration for unknown range table index"))?;
    let schema = match &entry.kind {
        RangeKind::Relation { schema, .. } => Some(schema.clone()),
        _ => None,
    };
    let mut columns = Vec::with_capacity(entry.columns.len());
    for (i, col) in entry.columns.iter().enumerate() {
        columns.push(NsColumn {
            name: col.name.clone(),
            expr: BoundExpr::column(0, rt_index, attnum_of(i)?, col.ty),
            dropped: col.dropped,
            hidden_from_star: false,
        });
    }
    Ok(NsItem {
        alias: Some(entry.alias.clone()),
        schema,
        rt_index: Some(rt_index),
        columns,
        rel_visible,
        cols_visible,
        lateral_only: false,
        lateral_ok: false,
    })
}

/// System column attnum for the names PG resolves through `specialAttNum`.
fn system_attnum(name: &str) -> Option<i16> {
    match name {
        "ctid" => Some(-1),
        "xmin" => Some(-2),
        "cmin" => Some(-3),
        "xmax" => Some(-4),
        "cmax" => Some(-5),
        "tableoid" => Some(TABLEOID_ATTNUM),
        _ => None,
    }
}

/// Column references (level 0) inside an expression.
fn collect_refs(expr: &BoundExpr) -> Vec<ColumnRef> {
    let mut out = Vec::new();
    expr.collect_columns(0, &mut out);
    out
}

/// Record select access on the referenced columns of the innermost scope (`scope`).
fn mark_refs(scope: &mut QueryScope, refs: &[ColumnRef]) {
    for c in refs {
        if let Some(entry) = (c.rt_index as usize).checked_sub(1).and_then(|i| scope.range_table.get_mut(i)) {
            mark_access(&mut entry.access, AccessKind::Select, c.attnum);
        }
    }
}

/// Make every level-0 column reference of `expr` refer `by` scopes further out.
fn shift_levels(expr: BoundExpr, by: u32) -> Result<BoundExpr> {
    if by == 0 {
        return Ok(expr);
    }
    expr.transform(&mut |mut e: BoundExpr| -> Result<BoundExpr> {
        if let ExprKind::Column(c) = &mut e.kind {
            c.levels_up += by;
        }
        Ok(e)
    })
}

/// A successful column lookup before `levels_up` is applied.
struct Found {
    /// Index into `Binder::scopes` where the column lives.
    level: usize,
    /// Expression with `levels_up == 0` (the namespace column's expression).
    expr: BoundExpr,
    /// Columns of the innermost-relative level that the expression reads (for access marking).
    refs: Vec<ColumnRef>,
    display: (String, String),
}

/// Turn a lookup hit into the public result: shift to the right level, record access, set position.
fn finalize(b: &mut Binder, found: Found, pos: Option<u32>) -> Result<ResolvedColumn> {
    let levels_up = scope_depth(b, found.level);
    let scope = b.scopes.get_mut(found.level).ok_or_else(|| PgError::internal("binder: scope index out of range"))?;
    mark_refs(scope, &found.refs);
    let mut expr = shift_levels(found.expr, levels_up)?;
    if let ExprKind::Column(c) = &mut expr.kind {
        c.pos = pos.unwrap_or(0);
    }
    Ok(ResolvedColumn { expr, levels_up, display: found.display })
}

/// Look `name` up among the columns of one namespace item. `Ok(None)` when absent.
fn item_column(scope: &QueryScope, level: usize, item: &NsItem, name: &str, pos: Option<u32>) -> Result<Option<Found>> {
    let mut hit: Option<&NsColumn> = None;
    for col in &item.columns {
        if col.dropped || col.name != name {
            continue;
        }
        if hit.is_some() {
            return Err(diag::ambiguous_column(name, pos));
        }
        hit = Some(col);
    }
    let alias = item.alias.clone().unwrap_or_default();
    if let Some(col) = hit {
        let expr = col.expr.clone();
        let refs = collect_refs(&expr);
        return Ok(Some(Found { level, expr, refs, display: (alias, name.to_string()) }));
    }
    // System columns exist only for plain relations (PG `scanRTEForColumn`, RTE_RELATION).
    if let (Some(rt), Some(attnum)) = (item.rt_index, system_attnum(name)) {
        let is_relation = entry_at(scope, rt).map(|e| matches!(e.kind, RangeKind::Relation { .. })).unwrap_or(false);
        if is_relation {
            if attnum != TABLEOID_ATTNUM {
                return Err(at(
                    PgError::new(SqlState::FEATURE_NOT_SUPPORTED, format!("system column \"{name}\" is not supported by PGArbor yet")),
                    pos,
                ));
            }
            let expr = BoundExpr::column(0, rt, attnum, TypeId::OID);
            let refs = collect_refs(&expr);
            return Ok(Some(Found { level, expr, refs, display: (alias, name.to_string()) }));
        }
    }
    Ok(None)
}

/// Unqualified lookup: innermost level first; more than one match on a level is 42702.
fn search_unqualified(b: &Binder, name: &str, pos: Option<u32>) -> Result<Option<Found>> {
    for (level, scope) in b.scopes.iter().enumerate().rev() {
        let mut hit: Option<Found> = None;
        for item in &scope.namespace {
            if !item.cols_visible || lateral_hidden(item) {
                continue;
            }
            if let Some(found) = item_column(scope, level, item, name, pos)? {
                if hit.is_some() {
                    return Err(diag::ambiguous_column(name, pos));
                }
                hit = Some(found);
            }
        }
        if hit.is_some() {
            return Ok(hit);
        }
    }
    Ok(None)
}

/// Does the namespace item name relation `schema.name` (3-part references)?
fn item_in_schema(scope: &QueryScope, item: &NsItem, schema: &str, name: &str) -> bool {
    if item.schema.as_deref() != Some(schema) {
        return false;
    }
    match item.rt_index.and_then(|rt| entry_at(scope, rt)) {
        Some(entry) => match &entry.kind {
            RangeKind::Relation { name: rel, .. } => rel.as_str() == name,
            _ => false,
        },
        None => false,
    }
}

/// Find the relation item named `name` (optionally `schema.name`) visible by name, innermost level
/// first. Returns (scope index, namespace index). Two matches on one level are 42P09.
fn find_rel_item(b: &Binder, schema: Option<&str>, name: &str, pos: Option<u32>) -> Result<Option<(usize, usize)>> {
    for (level, scope) in b.scopes.iter().enumerate().rev() {
        let mut hit: Option<usize> = None;
        for (ii, item) in scope.namespace.iter().enumerate() {
            if !item.rel_visible || lateral_hidden(item) || item.alias.as_deref() != Some(name) {
                continue;
            }
            if let Some(s) = schema {
                if !item_in_schema(scope, item, s, name) {
                    continue;
                }
            }
            if hit.is_some() {
                return Err(at(PgError::new(AMBIGUOUS_ALIAS, format!("table reference \"{name}\" is ambiguous")), pos));
            }
            hit = Some(ii);
        }
        if let Some(ii) = hit {
            return Ok(Some((level, ii)));
        }
    }
    Ok(None)
}

/// Qualified `[schema.]table.name` lookup.
fn qualified_column(
    b: &mut Binder,
    schema: Option<&str>,
    table: &str,
    name: &str,
    pos: Option<u32>,
    want_error: bool,
) -> Result<Option<ResolvedColumn>> {
    let found = match find_rel_item(b, schema, table, pos)? {
        Some((level, ii)) => {
            let scope = scope_at(b, level)?;
            let item = scope.namespace.get(ii).ok_or_else(|| PgError::internal("binder: namespace index out of range"))?;
            item_column(scope, level, item, name, pos)?
        }
        None => {
            return if want_error { Err(error_missing_rte(b, table, pos)) } else { Ok(None) };
        }
    };
    match found {
        Some(f) => finalize(b, f, pos).map(Some),
        None => {
            if want_error {
                Err(error_missing_column(b, Some(table), name, pos))
            } else {
                Ok(None)
            }
        }
    }
}

/// Type of a whole-row reference: the relation's rowtype, or `record` for other range kinds.
fn whole_row_type(b: &Binder, entry: &RangeEntry, pos: Option<u32>) -> Result<TypeId> {
    match &entry.kind {
        // The synthetic relation of a table expression scope (CHECK / DEFAULT / generation
        // expressions, `stored::table_expr_scope`) has no rowtype in the catalog.
        RangeKind::Relation { oid: 0, .. } => {
            Err(crate::diag::unsupported_construct("a whole-row reference in a table constraint expression", pos))
        }
        RangeKind::Relation { oid, .. } => {
            let rel = b
                .env
                .snapshot()
                .relation(*oid)
                .ok_or_else(|| PgError::internal(format!("binder: relation {oid} missing from catalog snapshot")))?;
            Ok(TypeId::of(rel.rowtype))
        }
        _ => Ok(TypeId::RECORD),
    }
}

/// Whole-row reference `t` (attnum 0) to a relation item visible by name.
pub fn whole_row(b: &mut Binder, name: &str, pos: Option<u32>) -> Result<Option<ResolvedColumn>> {
    let Some((level, ii)) = find_rel_item(b, None, name, pos)? else {
        return Ok(None);
    };
    let (rt, alias, ty) = {
        let scope = scope_at(b, level)?;
        let item = scope.namespace.get(ii).ok_or_else(|| PgError::internal("binder: namespace index out of range"))?;
        let rt = match item.rt_index {
            Some(rt) => rt,
            None => {
                return Err(at(
                    PgError::new(
                        SqlState::FEATURE_NOT_SUPPORTED,
                        format!("whole-row reference to join \"{name}\" is not supported by PGArbor yet"),
                    ),
                    pos,
                ));
            }
        };
        let entry = entry_at(scope, rt).ok_or_else(|| PgError::internal("binder: range table index out of range"))?;
        let ty = whole_row_type(b, entry, pos)?;
        (rt, item.alias.clone().unwrap_or_default(), ty)
    };
    let expr = BoundExpr::column(0, rt, 0, ty);
    let refs = vec![ColumnRef { levels_up: 0, rt_index: rt, attnum: 0, pos: 0 }];
    let found = Found { level, expr, refs, display: (alias, name.to_string()) };
    finalize(b, found, pos).map(Some)
}

/// PG `errorMissingRTE`: a table name that exists but is not visible here is 42P01 "invalid
/// reference"; otherwise "missing FROM-clause entry".
fn error_missing_rte(b: &Binder, name: &str, pos: Option<u32>) -> PgError {
    // PG `errorMissingRTE`: a relation written by its real name while it is in FROM under a visible
    // alias (`FROM colref AS c ... colref.y`) gets the alias hint.
    let aliased = b
        .scopes
        .iter()
        .flat_map(|scope| scope.range_table.iter())
        .find(|entry| matches!(&entry.kind, RangeKind::Relation { name: rel, .. } if rel.as_str() == name) && entry.alias != name);
    if let Some(entry) = aliased {
        let alias_visible = b
            .scopes
            .iter()
            .any(|scope| scope.namespace.iter().any(|item| item.alias.as_deref() == Some(entry.alias.as_str())));
        if alias_visible {
            return diag::invalid_from_reference_alias(name, &entry.alias, pos);
        }
    }
    let exists = b.scopes.iter().any(|scope| scope.range_table.iter().any(|entry| entry.alias == name));
    if exists {
        diag::invalid_from_reference(name, pos)
    } else {
        diag::missing_from_entry(name, pos)
    }
}

/// Levenshtein distance over characters (PG `varstr_levenshtein` with unit costs).
fn levenshtein(a: &str, b: &str) -> usize {
    let a: Vec<char> = a.chars().collect();
    let b: Vec<char> = b.chars().collect();
    if a.is_empty() {
        return b.len();
    }
    if b.is_empty() {
        return a.len();
    }
    let mut prev: Vec<usize> = (0..=b.len()).collect();
    let mut cur: Vec<usize> = vec![0; b.len() + 1];
    for (i, ca) in a.iter().enumerate() {
        cur[0] = i + 1;
        for (j, cb) in b.iter().enumerate() {
            let cost = if ca == cb { 0 } else { 1 };
            cur[j + 1] = (prev[j] + cost).min(prev[j + 1] + 1).min(cur[j] + 1);
        }
        std::mem::swap(&mut prev, &mut cur);
    }
    prev[b.len()]
}

/// Like PG `varstr_levenshtein_less_equal`: the exact distance when it is `<= max`, else `max + 1`.
fn levenshtein_le(a: &str, b: &str, max: usize) -> usize {
    let d = levenshtein(a, b);
    if d > max {
        max + 1
    } else {
        d
    }
}

/// PG `FuzzyAttrMatchState`: the best (and second best) "did you mean" candidates, plus exact
/// matches in relations that are not visible from this part of the query.
struct FuzzyState {
    distance: usize,
    first: Option<(String, String)>,
    second: Option<(String, String)>,
    exact1: Option<(String, String)>,
    exact2: Option<(String, String)>,
}

impl FuzzyState {
    fn new() -> FuzzyState {
        FuzzyState { distance: MAX_FUZZY_DISTANCE + 1, first: None, second: None, exact1: None, exact2: None }
    }
}

/// PG `updateFuzzyAttrMatchState`.
fn update_fuzzy(st: &mut FuzzyState, penalty: usize, alias: &str, actual: &str, target: &str) {
    if penalty > st.distance {
        return;
    }
    // Dropped columns have no name and are never suggested.
    if actual.is_empty() {
        return;
    }
    let matchlen = target.len();
    let max_d = st.distance + 1 - penalty;
    let mut column_distance = levenshtein_le(actual, target, max_d);
    // More than half the characters differ: not a sensible suggestion.
    if column_distance > matchlen / 2 {
        return;
    }
    column_distance += penalty;
    if column_distance < st.distance {
        st.distance = column_distance;
        st.first = Some((alias.to_string(), actual.to_string()));
        st.second = None;
    } else if column_distance == st.distance {
        if st.second.is_some() {
            // Too many matches at this distance: drop them, keep the distance.
            st.first = None;
            st.second = None;
        } else if st.first.is_some() {
            st.second = Some((alias.to_string(), actual.to_string()));
        }
    }
}

/// PG `searchRangeTableForCol`, over every range entry of every open level.
fn fuzzy_search(b: &Binder, qualifier: Option<&str>, name: &str) -> FuzzyState {
    let mut st = FuzzyState::new();
    for scope in b.scopes.iter().rev() {
        for rte in &scope.range_table {
            let penalty = match qualifier {
                Some(q) => levenshtein_le(q, &rte.alias, MAX_FUZZY_DISTANCE + 1),
                None => 0,
            };
            let mut exact = false;
            for col in &rte.columns {
                if col.dropped {
                    continue;
                }
                if col.name == name {
                    exact = true;
                }
                update_fuzzy(&mut st, penalty, &rte.alias, &col.name, name);
            }
            if exact && penalty == 0 {
                let entry = (rte.alias.clone(), name.to_string());
                if st.exact1.is_none() {
                    st.exact1 = Some(entry);
                } else {
                    st.exact2 = Some(entry);
                }
            }
        }
    }
    st
}

/// PG `errorMissingColumn`: 42703 with the DETAIL/HINT PG would print.
fn error_missing_column(b: &Binder, qualifier: Option<&str>, name: &str, pos: Option<u32>) -> PgError {
    let st = fuzzy_search(b, qualifier, name);
    let base = diag::undefined_column(qualifier, name, pos);
    if let Some((alias, _)) = &st.exact1 {
        return if st.exact2.is_some() {
            let e = base.with_detail(format!(
                "There are columns named \"{name}\", but they are in tables that cannot be referenced from this part of the query."
            ));
            if qualifier.is_none() {
                e.with_hint("Try using a table-qualified name.")
            } else {
                e
            }
        } else {
            let e = base.with_detail(format!(
                "There is a column named \"{name}\" in table \"{alias}\", but it cannot be referenced from this part of the query."
            ));
            if qualifier.is_none() {
                e.with_hint("To reference that column, you must use a table-qualified name.")
            } else {
                e
            }
        };
    }
    match (&st.first, &st.second) {
        (None, _) => base,
        (Some((a, c)), None) => base.with_hint(format!("Perhaps you meant to reference the column \"{a}.{c}\".")),
        (Some((a1, c1)), Some((a2, c2))) => base.with_hint(format!(
            "Perhaps you meant to reference the column \"{a1}.{c1}\" or the column \"{a2}.{c2}\"."
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::binder::context::{BindContext, BindSession};
    use crate::ir::{BoundValues, RangeColumn};
    use crate::source::SourceMap;
    use pgarbor_catalog::testing;
    use pgarbor_types::context::IoConfig;

    fn values_entry(alias: &str, cols: &[(&str, bool)]) -> RangeEntry {
        RangeEntry {
            kind: RangeKind::Values { values: BoundValues { rows: Vec::new(), types: Vec::new() } },
            alias: alias.to_string(),
            columns: cols
                .iter()
                .map(|(n, dropped)| RangeColumn { name: n.to_string(), ty: TypeId::INT4, dropped: *dropped })
                .collect(),
            lateral: false,
            visible: true,
            access: AccessNeeded::default(),
        }
    }

    /// Range entry of `alias` with `cols`, visible by name and by column.
    fn add_table(b: &mut Binder, alias: &str, cols: &[(&str, bool)]) -> u32 {
        let idx = add_range_entry(b, values_entry(alias, cols)).unwrap();
        add_namespace_for_range(b, idx, true, true).unwrap();
        idx
    }

    fn q(parts: &[&str]) -> Vec<String> {
        parts.iter().map(|s| s.to_string()).collect()
    }

    fn with_binder<T>(f: impl FnOnce(&mut Binder) -> T) -> T {
        let (_kv, env) = testing::bootstrapped().unwrap();
        let src = SourceMap::new("");
        let ctx = BindContext {
            env: &env,
            session: BindSession { database: "postgres".to_string() },
            io_config: IoConfig::default(),
            statement_timestamp: 0,
            declared_params: Vec::new(),
        };
        let mut b = Binder::new(&ctx, &src);
        b.scopes.push(QueryScope::new(ScopeKind::Select));
        f(&mut b)
    }

    #[test]
    fn ambiguous_unqualified_column_is_42702() {
        with_binder(|b| {
            add_table(b, "t1", &[("a", false)]);
            add_table(b, "t2", &[("a", false)]);
            let e = resolve_column(b, &[], "a", Some(8), true).unwrap_err();
            assert_eq!(e.sqlstate, SqlState::AMBIGUOUS_COLUMN);
            assert_eq!(e.message, "column reference \"a\" is ambiguous");
            assert_eq!(e.position, Some(8));
        });
    }

    #[test]
    fn unqualified_column_resolves_to_column_ref() {
        with_binder(|b| {
            add_table(b, "t1", &[("a", false), ("b", false)]);
            let r = resolve_column(b, &[], "b", None, true).unwrap().unwrap();
            assert_eq!(r.levels_up, 0);
            let ExprKind::Column(c) = r.expr.kind else { panic!("expected a column reference") };
            assert_eq!((c.levels_up, c.rt_index, c.attnum), (0, 1, 2));
        });
    }

    #[test]
    fn missing_qualified_column_and_table() {
        with_binder(|b| {
            add_table(b, "t1", &[("a", false)]);
            let e = resolve_column(b, &q(&["t1"]), "zz", Some(4), true).unwrap_err();
            assert_eq!(e.sqlstate, SqlState::UNDEFINED_COLUMN);
            assert_eq!(e.message, "column t1.zz does not exist");
            assert_eq!(e.position, Some(4));
            let e = resolve_column(b, &q(&["nope"]), "a", None, true).unwrap_err();
            assert_eq!(e.sqlstate, SqlState::UNDEFINED_TABLE);
            assert_eq!(e.message, "missing FROM-clause entry for table \"nope\"");
            assert_eq!(resolve_column(b, &q(&["nope"]), "a", None, false).unwrap(), None);
        });
    }

    #[test]
    fn unknown_unqualified_column_and_fuzzy_hint() {
        with_binder(|b| {
            add_table(b, "t", &[("abc", false)]);
            let e = resolve_column(b, &[], "abd", None, true).unwrap_err();
            assert_eq!(e.message, "column \"abd\" does not exist");
            assert_eq!(e.hint.as_deref(), Some("Perhaps you meant to reference the column \"t.abc\"."));
            let e = resolve_column(b, &[], "zz", None, true).unwrap_err();
            assert_eq!(e.message, "column \"zz\" does not exist");
            assert!(e.hint.is_none());
        });
    }

    #[test]
    fn correlated_reference_has_levels_up_one() {
        with_binder(|b| {
            add_table(b, "t1", &[("a", false)]);
            b.scopes.push(QueryScope::new(ScopeKind::Select));
            add_table(b, "t2", &[("b", false)]);
            let r = resolve_column(b, &[], "a", None, true).unwrap().unwrap();
            assert_eq!(r.levels_up, 1);
            let ExprKind::Column(c) = r.expr.kind else { panic!("expected a column reference") };
            assert_eq!((c.levels_up, c.rt_index, c.attnum), (1, 1, 1));
            assert!(b.scopes[0].range_table[0].access.select_cols.contains(&1));
        });
    }

    #[test]
    fn star_expansion_skips_dropped_columns_in_order() {
        with_binder(|b| {
            add_table(b, "t", &[("a", false), ("b", true), ("c", false)]);
            let out = expand_star(b, None, None).unwrap();
            let names: Vec<&str> = out.iter().map(|(n, _)| n.as_str()).collect();
            assert_eq!(names, vec!["a", "c"]);
            let qn = QualName::simple("t");
            let out = expand_star(b, Some(&qn), None).unwrap();
            let names: Vec<&str> = out.iter().map(|(n, _)| n.as_str()).collect();
            assert_eq!(names, vec!["a", "c"]);
        });
    }

    #[test]
    fn star_without_from_is_42601() {
        with_binder(|b| {
            let e = expand_star(b, None, Some(8)).unwrap_err();
            assert_eq!(e.sqlstate, SqlState::SYNTAX_ERROR);
            assert_eq!(e.message, "SELECT * with no tables specified is not valid");
        });
    }

    #[test]
    fn whole_row_reference_has_attnum_zero() {
        with_binder(|b| {
            add_table(b, "t", &[("a", false)]);
            let r = resolve_column(b, &[], "t", None, true).unwrap().unwrap();
            assert_eq!(r.expr.ty.oid, pgarbor_types::oids::RECORD);
            let ExprKind::Column(c) = r.expr.kind else { panic!("expected a column reference") };
            assert_eq!((c.rt_index, c.attnum), (1, 0));
        });
    }

    #[test]
    fn duplicate_visible_alias_is_42712() {
        with_binder(|b| {
            add_table(b, "t", &[("a", false)]);
            let idx = add_range_entry(b, values_entry("t", &[("b", false)])).unwrap();
            let e = add_namespace_for_range(b, idx, true, true).unwrap_err();
            assert_eq!(e.sqlstate, SqlState::DUPLICATE_ALIAS);
            assert_eq!(e.message, "table name \"t\" specified more than once");
        });
    }

    #[test]
    fn levenshtein_matches_textbook_value() {
        assert_eq!(levenshtein("kitten", "sitting"), 3);
        assert_eq!(levenshtein("", "abc"), 3);
        assert_eq!(levenshtein("abc", "abc"), 0);
    }
}
