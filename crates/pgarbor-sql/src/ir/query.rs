//! Bound query IR: the output of the SELECT binder (`binder::query`) and input of
//! `pgarbor_plan::build`. Plain data, fully implemented, frozen (v1).
//!
//! ## Shape
//! ```text
//! BoundQuery { ctes, body: QueryBody, output, order_by, limit, offset, locking }
//!   QueryBody::Select(BoundSelect { range_table, from, where_, target_list, group_by, having,
//!                                   distinct, windows, ... })
//!   QueryBody::Values(BoundValues)
//!   QueryBody::SetOp(SetOpBody { op, all, left: BoundQuery, right: BoundQuery })
//! ```
//! This mirrors PG's parse-analysis output (`Query`) with these deliberate simplifications:
//! * **Grouping, DISTINCT ON and ORDER BY refer to target-list entries by index**
//!   (PG `ressortgroupref`). Expressions that appear only in GROUP BY / ORDER BY / DISTINCT ON get a
//!   *junk* target entry (`TargetEntry::junk`), dropped from the final output.
//! * **Aggregates and window functions stay inline** in `target_list` / `having` / window and
//!   ordering expressions as `Aggref` / `WindowFunc` nodes (PG does the same before the planner).
//!   The binder has already verified PG's grouping rules (42803).
//! * **Join USING/NATURAL columns are resolved at bind time**: references to a merged column
//!   became `COALESCE(left,right)` (FULL JOIN) or the surviving side's column; the join tree only
//!   carries the equality quals.
//! * **Sub-queries are nested `BoundQuery` values** referenced from `RangeKind::Subquery` and from
//!   `SubLink` expressions; correlated references inside use `ColumnRef::levels_up > 0`.
//!
//! PG references: `src/include/nodes/parsenodes.h` (`Query`, `RangeTblEntry`, `TargetEntry`,
//! `SortGroupClause`, `WindowClause`, `CommonTableExpr`), `src/include/nodes/primnodes.h` (`FromExpr`,
//! `JoinExpr`).
use super::expr::{BoundExpr, OpRef, SortExpr};
use pgarbor_common::Oid;
use pgarbor_catalog::model::RelKind;
use pgarbor_types::TypeId;

/// Where an output column came from, for the wire protocol's RowDescription
/// (`table_oid`, `column_attnum`). Resolved through views/subqueries to the base table column when
/// the output is a plain column reference, exactly as PG's `markTargetListOrigin`.
// CONTRACT
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ColumnOrigin {
    pub table: Oid,
    pub attnum: i16,
}

/// One result column of a query (names follow PG `FigureColname`: column name, function name,
/// `case`, `?column?`, a cast takes its operand's name, `ARRAY[...]` -> `array`, ...).
// CONTRACT
#[derive(Clone, Debug, PartialEq)]
pub struct OutputColumn {
    pub name: String,
    pub ty: TypeId,
    pub origin: Option<ColumnOrigin>,
}

/// One entry of a SELECT target list. PG `TargetEntry`.
// CONTRACT
#[derive(Clone, Debug, PartialEq)]
pub struct TargetEntry {
    pub expr: BoundExpr,
    /// Output column name (meaningless for junk entries but still set, `?column?`).
    pub name: String,
    /// Not part of the query result: exists only so GROUP BY / ORDER BY / DISTINCT ON can refer to it.
    pub junk: bool,
    pub origin: Option<ColumnOrigin>,
}

/// Permissions a statement needs on a range entry; filled by the binder as it sees column
/// references and consumed by the executor's ACL check (`pgarbor_catalog::acl`). Column lists hold
/// attnums; an empty list with the flag set means "table-level privilege or any column".
// CONTRACT
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct AccessNeeded {
    pub select: bool,
    pub insert: bool,
    pub update: bool,
    pub delete: bool,
    pub select_cols: Vec<i16>,
    pub insert_cols: Vec<i16>,
    pub update_cols: Vec<i16>,
}

/// A column visible through a range entry (after any column-alias list).
// CONTRACT
#[derive(Clone, Debug, PartialEq)]
pub struct RangeColumn {
    pub name: String,
    pub ty: TypeId,
    /// Dropped table columns keep their slot (attnum stability) and are never matched by name.
    pub dropped: bool,
}

/// One function in a FROM-clause function scan (`ROWS FROM(f(), g())` has several).
// CONTRACT
#[derive(Clone, Debug, PartialEq)]
pub struct RangeFunction {
    /// The call (`FuncCall` with `retset`, or any scalar/composite-valued expression; `unnest(a, b)`
    /// multi-array form is a single `FuncCall`).
    pub expr: BoundExpr,
    /// Output columns this function contributes (name, type): one for scalar SRFs (named after the
    /// function or the alias), N for composite/OUT-parameter functions or column-definition lists.
    pub columns: Vec<RangeColumn>,
}

/// What a range-table entry is. PG `RTEKind`.
// CONTRACT
#[derive(Clone, Debug, PartialEq)]
pub enum RangeKind {
    /// A base relation, or a view/matview/virtual catalog relation. Views are NOT expanded here:
    /// the binder replaces a view reference by `Subquery` (re-binding the stored definition); a
    /// `Relation` entry therefore is a table, matview (scanned like a table), sequence or a
    /// virtual `pg_catalog`/`information_schema` relation (`oid < 16384`).
    Relation {
        oid: Oid,
        schema: String,
        name: String,
        rel_kind: RelKind,
        /// Written `ONLY t` (no inheritance children; irrelevant while inheritance is unsupported).
        only: bool,
        /// Which pseudo relation this is, if any (ON CONFLICT `excluded`).
        pseudo: Option<PseudoRelation>,
    },
    /// `(SELECT ...) alias`, an expanded view, or a lateral subquery.
    Subquery { query: Box<BoundQuery> },
    /// `FROM f(...)`, `FROM unnest(...) WITH ORDINALITY`, `ROWS FROM(...)`.
    Function { funcs: Vec<RangeFunction>, with_ordinality: bool },
    /// `(VALUES (...), (...)) alias`.
    Values { values: BoundValues },
    /// Reference to a CTE of this or an enclosing query: `ctes[cte_index]` of the query
    /// `levels_up` levels out. A reference from inside the CTE's own recursive term sets
    /// `recursive_self`.
    Cte { cte_index: u32, levels_up: u32, recursive_self: bool },
}

/// Special relations that are not tables.
// CONTRACT
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PseudoRelation {
    /// The row proposed for insertion, visible in `ON CONFLICT DO UPDATE` as `excluded`.
    Excluded,
}

/// One range-table entry.
// CONTRACT
#[derive(Clone, Debug, PartialEq)]
pub struct RangeEntry {
    pub kind: RangeKind,
    /// The name used to qualify its columns (`t`, or the alias). Anonymous VALUES / subqueries
    /// without alias use PG's generated names (`*VALUES*`, `unnamed_subquery`).
    pub alias: String,
    /// Visible columns in order. For `Relation`: one per attnum slot including dropped ones
    /// (`RangeColumn::dropped`). Column `i` has attnum `i + 1`.
    pub columns: Vec<RangeColumn>,
    /// `LATERAL`: may reference earlier FROM items of the same level.
    pub lateral: bool,
    /// Visible to unqualified/qualified column lookup (false for the INSERT/UPDATE target while
    /// binding values, and for entries hidden by a join alias).
    pub visible: bool,
    pub access: AccessNeeded,
}

/// Join type of a [`JoinItem`]. Semi/anti joins are created by the planner, never the binder.
// CONTRACT
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum JoinKind {
    Inner,
    Left,
    Right,
    Full,
}

/// An explicit JOIN in the FROM tree. `CROSS JOIN` = `Inner` with `quals: None`. `USING`/`NATURAL`
/// are lowered to `quals` (an AND of `l.c = r.c`).
// CONTRACT
#[derive(Clone, Debug, PartialEq)]
pub struct JoinItem {
    pub kind: JoinKind,
    pub left: FromItem,
    pub right: FromItem,
    pub quals: Option<BoundExpr>,
}

/// FROM-clause tree. PG `FromExpr`/`JoinExpr`/`RangeTblRef`.
// CONTRACT
#[derive(Clone, Debug, PartialEq)]
pub enum FromItem {
    /// 1-based index into `BoundSelect::range_table`.
    Range(u32),
    Join(Box<JoinItem>),
}

/// How a query level is locked (`FOR UPDATE` ...). PG `LockClauseStrength`.
// CONTRACT
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LockStrength {
    Update,
    NoKeyUpdate,
    Share,
    KeyShare,
}

/// `NOWAIT` / `SKIP LOCKED`.
// CONTRACT
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LockWait {
    Block,
    NoWait,
    SkipLocked,
}

/// `FOR UPDATE [OF t] [NOWAIT|SKIP LOCKED]`.
// CONTRACT
#[derive(Clone, Debug, PartialEq)]
pub struct LockingClause {
    pub strength: LockStrength,
    pub wait: LockWait,
    /// Range-table indexes (of the select that owns the clause) to lock; empty = all.
    pub of: Vec<u32>,
}

/// Which grouping a SELECT performs. `exprs` are indexes into the target list.
// CONTRACT
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct GroupBy {
    /// Distinct grouping expressions (target-list indexes), in first-appearance order.
    pub exprs: Vec<usize>,
    /// `None` = plain `GROUP BY exprs` (or no GROUP BY). `Some(sets)` = GROUPING SETS / ROLLUP /
    /// CUBE expanded to explicit sets; each set lists indexes into `exprs`. `GROUP BY ()` is
    /// `Some(vec![vec![]])`.
    pub sets: Option<Vec<Vec<usize>>>,
}

/// SELECT DISTINCT forms.
// CONTRACT
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DistinctSpec {
    None,
    /// `SELECT DISTINCT`: all non-junk target entries.
    All,
    /// `SELECT DISTINCT ON (e1, e2)`: target-list indexes, in written order.
    On(Vec<usize>),
}

/// Window frame mode.
// CONTRACT
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FrameUnits {
    Rows,
    Range,
    Groups,
}

/// One frame boundary.
// CONTRACT
#[derive(Clone, Debug, PartialEq)]
pub enum FrameBound {
    UnboundedPreceding,
    /// `N PRECEDING`: offset expression already coerced (int8 for ROWS/GROUPS; the ordering
    /// column's "offset type" for RANGE, e.g. interval for timestamps).
    Preceding(BoundExpr),
    CurrentRow,
    Following(BoundExpr),
    UnboundedFollowing,
}

/// `EXCLUDE ...`.
// CONTRACT
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FrameExclusion {
    NoOthers,
    CurrentRow,
    Group,
    Ties,
}

/// A window definition (`WINDOW w AS (...)` or an inline `OVER (...)`). PG `WindowClause`.
/// A named reference with modifications is merged by the binder, so a `WindowDef` is always
/// complete.
// CONTRACT
#[derive(Clone, Debug, PartialEq)]
pub struct WindowDef {
    pub name: Option<String>,
    pub partition_by: Vec<BoundExpr>,
    pub order_by: Vec<SortExpr>,
    pub units: FrameUnits,
    pub start: FrameBound,
    pub end: FrameBound,
    pub exclusion: FrameExclusion,
    /// The frame was not written: default `RANGE BETWEEN UNBOUNDED PRECEDING AND CURRENT ROW`
    /// (with ORDER BY) or the whole partition (without). EXPLAIN / deparse omit default frames.
    pub frame_is_default: bool,
}

/// One ORDER BY key of a query. `target_idx` indexes the body's target list (SELECT) or output
/// columns (VALUES / set operations).
// CONTRACT
#[derive(Clone, Debug, PartialEq)]
pub struct SortKey {
    pub target_idx: usize,
    pub descending: bool,
    pub nulls_first: bool,
    /// `ORDER BY x USING op`.
    pub using_op: Option<OpRef>,
}

/// A simple SELECT block (FROM / WHERE / GROUP BY / HAVING / DISTINCT / windows / projection).
// CONTRACT
#[derive(Clone, Debug, PartialEq)]
pub struct BoundSelect {
    pub range_table: Vec<RangeEntry>,
    /// Top-level comma-separated FROM items (implicitly cross-joined). Empty = `SELECT` without FROM
    /// (one row).
    pub from: Vec<FromItem>,
    pub where_: Option<BoundExpr>,
    /// Projection list including junk entries (module docs).
    pub target_list: Vec<TargetEntry>,
    pub group_by: GroupBy,
    pub having: Option<BoundExpr>,
    pub distinct: DistinctSpec,
    pub windows: Vec<WindowDef>,
    /// Any `Aggref` with `levels_up == 0` in target list / having / order keys (=> grouped query
    /// even without GROUP BY).
    pub has_aggs: bool,
    pub has_windows: bool,
    /// A set-returning function appears in the target list (planner adds a ProjectSet).
    pub has_target_srfs: bool,
    /// Any sub-link anywhere in the block.
    pub has_sublinks: bool,
}

/// `VALUES (..), (..)` after common-type resolution: every cell of column `i` already coerced to
/// `types[i]`. May contain `SetToDefault` only when it is an INSERT source.
// CONTRACT
#[derive(Clone, Debug, PartialEq)]
pub struct BoundValues {
    pub rows: Vec<Vec<BoundExpr>>,
    pub types: Vec<TypeId>,
}

/// UNION / INTERSECT / EXCEPT.
// CONTRACT
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SetOpKind {
    Union,
    Intersect,
    Except,
}

/// A set operation. Both children's output columns have been coerced to `types` by the binder
/// (casts inserted into the children's target lists / VALUES cells; PG `coerce_setop_columns`).
// CONTRACT
#[derive(Clone, Debug, PartialEq)]
pub struct SetOpBody {
    pub op: SetOpKind,
    /// `ALL`: keep duplicates.
    pub all: bool,
    pub left: Box<BoundQuery>,
    pub right: Box<BoundQuery>,
    /// Result column types (common types) and names (the left child's names).
    pub types: Vec<TypeId>,
}

/// Query body variants.
// CONTRACT
#[derive(Clone, Debug, PartialEq)]
pub enum QueryBody {
    Select(Box<BoundSelect>),
    Values(BoundValues),
    SetOp(SetOpBody),
}

/// Body of a CTE: a query, or a data-modifying statement with RETURNING.
// CONTRACT
#[derive(Clone, Debug, PartialEq)]
pub enum CteBody {
    Query(Box<BoundQuery>),
    /// `WITH x AS (INSERT/UPDATE/DELETE ... RETURNING ...)`; always a `BoundStatement::Insert|Update|Delete`.
    Modify(Box<super::stmt::BoundStatement>),
}

/// One `WITH` item. PG `CommonTableExpr`.
// CONTRACT
#[derive(Clone, Debug, PartialEq)]
pub struct BoundCte {
    pub name: String,
    /// Output columns after any column-alias list.
    pub columns: Vec<OutputColumn>,
    pub body: CteBody,
    /// Declared `WITH RECURSIVE` AND actually self-referencing: the body is
    /// `SetOp(Union)` whose left child is the non-recursive term and right child the recursive term
    /// (which contains a `RangeKind::Cte { recursive_self: true }` entry).
    pub recursive: bool,
    /// `MATERIALIZED` (Some(true)), `NOT MATERIALIZED` (Some(false)) or unspecified.
    pub materialized: Option<bool>,
    /// Number of references (>1 forces materialization when unspecified; 0 allowed for DML CTEs).
    pub ref_count: u32,
}

/// A complete query expression (what appears in parentheses, in a view, in INSERT ... SELECT).
// CONTRACT
#[derive(Clone, Debug, PartialEq)]
pub struct BoundQuery {
    pub ctes: Vec<BoundCte>,
    pub body: QueryBody,
    /// Result columns (non-junk), the RowDescription source.
    pub output: Vec<OutputColumn>,
    pub order_by: Vec<SortKey>,
    /// `LIMIT n` / `FETCH FIRST n ROWS` (int8; NULL or absent = no limit).
    pub limit: Option<BoundExpr>,
    pub offset: Option<BoundExpr>,
    /// `FETCH FIRST n ROWS WITH TIES`.
    pub limit_with_ties: bool,
    pub locking: Vec<LockingClause>,
}

impl BoundQuery {
    /// Number of non-junk output columns.
    pub fn width(&self) -> usize {
        self.output.len()
    }
    /// Output column types.
    pub fn output_types(&self) -> Vec<TypeId> {
        self.output.iter().map(|c| c.ty).collect()
    }
    /// The simple SELECT block if the body is one.
    pub fn as_select(&self) -> Option<&BoundSelect> {
        match &self.body {
            QueryBody::Select(s) => Some(s),
            _ => None,
        }
    }
}

impl BoundSelect {
    /// An empty block (no FROM, no targets).
    pub fn empty() -> BoundSelect {
        BoundSelect {
            range_table: Vec::new(),
            from: Vec::new(),
            where_: None,
            target_list: Vec::new(),
            group_by: GroupBy::default(),
            having: None,
            distinct: DistinctSpec::None,
            windows: Vec::new(),
            has_aggs: false,
            has_windows: false,
            has_target_srfs: false,
            has_sublinks: false,
        }
    }
    /// Non-junk target entries.
    pub fn visible_targets(&self) -> impl Iterator<Item = &TargetEntry> {
        self.target_list.iter().filter(|t| !t.junk)
    }
}
