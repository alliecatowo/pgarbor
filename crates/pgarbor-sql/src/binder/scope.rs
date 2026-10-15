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
use pgarbor_common::{PgError, Result};

use super::context::Binder;
use crate::ir::{AccessNeeded, BoundExpr, RangeEntry, WindowDef};
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
#[allow(unused_variables)]
pub fn add_range_entry(b: &mut Binder, entry: RangeEntry) -> Result<u32> {
    Err(PgError::unimplemented("SQL-05", "range table insertion (scope::add_range_entry)"))
}

/// Build the [`NsItem`] for range entry `rt_index` (columns = the entry's visible columns, each
/// `Column{rt_index, attnum}` typed from `RangeColumn::ty`) and add it to the namespace. Checks
/// alias uniqueness within the current FROM list: 42712 `table name "t" specified more than once`
/// (`diag::duplicate_alias`; items hidden by a join alias do not count).
#[allow(unused_variables)]
pub fn add_namespace_for_range(b: &mut Binder, rt_index: u32, rel_visible: bool, cols_visible: bool) -> Result<()> {
    Err(PgError::unimplemented("SQL-05", "namespace registration (scope::add_namespace_for_range)"))
}

/// Resolve `qualifier.name` (qualifier empty for a bare identifier; at most `[schema, table]`) at
/// `pos`. Implements the rules in the module docs including outer scopes, whole-row references,
/// system columns, ambiguity and all PG error texts. Returns `Ok(None)` ONLY when nothing matches
/// and `want_error` is false (used by ORDER BY / GROUP BY output-name matching and by field
/// selection `(a).b` probing); otherwise the 42703/42P01 error.
#[allow(unused_variables)]
pub fn resolve_column(
    b: &mut Binder,
    qualifier: &[String],
    name: &str,
    pos: Option<u32>,
    want_error: bool,
) -> Result<Option<ResolvedColumn>> {
    Err(PgError::unimplemented("SQL-05", "column reference resolution (scope::resolve_column)"))
}

/// Expand `*` (qualifier `None`) or `t.*` into `(output name, expression)` pairs in column order,
/// skipping dropped and `hidden_from_star` columns. `SELECT *` with an empty FROM clause is 42601
/// `SELECT * with no tables specified is not valid`; `t.*` with unknown `t` is 42P01 `missing
/// FROM-clause entry for table "t"`. Marks `select_cols` access. For `t.*` where `t` names a
/// whole-row expression of composite type the caller (`target`) uses `expr_misc::expand_composite`.
#[allow(unused_variables)]
pub fn expand_star(b: &mut Binder, qualifier: Option<&QualName>, pos: Option<u32>) -> Result<Vec<(String, BoundExpr)>> {
    Err(PgError::unimplemented("SQL-05", "star expansion (scope::expand_star)"))
}

/// Does a range entry or join alias named `name` exist in the innermost scope's namespace
/// (rel_visible)? Used for `t.*`/whole-row probing and for UPDATE/DELETE alias checks.
#[allow(unused_variables)]
pub fn find_relation_alias(b: &Binder, name: &str) -> Result<Option<u32>> {
    Err(PgError::unimplemented("SQL-05", "alias lookup (scope::find_relation_alias)"))
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
