//! The logical plan: relational algebra with bind-time expressions, the input of the rewrite
//! passes (`simplify`, `pushdown`, `subplan`) and of the physical planner. Plain data, fully
//! implemented, frozen (v1).
//!
//! ## Conventions
//! * Expressions are BIND-TIME (`ExprKind::Column{levels_up: 0, rt_index, attnum}`, `Aggref`,
//!   `WindowFunc`, `SubLink`). Columns of enclosing queries (`levels_up >= 1`) stay as they are until
//!   `setrefs`/`subplan` turn them into `OuterRef`s.
//! * Every node lists the bind-time expression that produces each of its output slots in `vars` (PG's
//!   target list) and the slot names/types in `out`. A parent's expression is translated to input slots
//!   by structural matching against the child's `vars` (`setrefs`). Scans list every column of their
//!   range entry; `Project` lists its expressions; `Aggregate` lists `group_exprs ++ aggrefs`; `Window`
//!   lists `child.vars ++ window funcs`; joins list `left.vars ++ right.vars`.
//! * `rels` = the set of range-table indexes (of the query level being planned) whose columns the
//!   node's `vars` still expose directly (used by pushdown to decide where a qual can be evaluated).
//! * Sub-queries in FROM (`RangeKind::Subquery`) become `ScanSource::Subquery(Box<LogicalPlan>)`: the
//!   child plan is planned recursively; the scan exposes the child's output slots as the columns of
//!   its range entry (`Column{rt_index: k, attnum: i+1}` <-> child slot `i`).
//! * View references were already expanded to sub-queries by the binder.
use pgarbor_common::Oid;
use pgarbor_sql::ir::{BoundExpr, FrameBound, FrameExclusion, FrameUnits, GroupBy, LockStrength, LockWait};
use pgarbor_types::TypeId;

use crate::plan::{FunctionItem, JoinType, OutCol, SetOpCmd, SortKeySpec, TableRef};

/// A logical plan node plus the bookkeeping common to all nodes.
// CONTRACT
#[derive(Clone, Debug, PartialEq)]
pub struct LogicalPlan {
    pub node: LogicalNode,
    /// Bind-time expression producing each output slot (module docs).
    pub vars: Vec<BoundExpr>,
    /// Name and type of each output slot (parallel to `vars`).
    pub out: Vec<OutCol>,
    /// Range-table indexes whose columns this node exposes (sorted, unique).
    pub rels: Vec<u32>,
    /// Query level id (index into `PlannerState::range_names`) in whose range table the node's
    /// bind-time `Column`s are expressed. Assigned by `build::build_query` (one fresh level per query
    /// block, including sub-queries, CTE bodies and sub-plans).
    pub level: u32,
}

impl LogicalPlan {
    /// Number of output slots.
    pub fn width(&self) -> usize {
        self.vars.len()
    }
}

/// Logical node variants.
// CONTRACT
#[derive(Clone, Debug, PartialEq)]
pub enum LogicalNode {
    Scan(Box<LogicalScan>),
    Join(Box<LogicalJoin>),
    /// Residual quals that cannot be pushed lower (above an outer join, HAVING-free post filters).
    Filter(Box<LogicalFilter>),
    Aggregate(Box<LogicalAggregate>),
    Window(Box<LogicalWindow>),
    Project(Box<LogicalProject>),
    Sort(Box<LogicalSort>),
    Limit(Box<LogicalLimit>),
    Distinct(Box<LogicalDistinct>),
    SetOp(Box<LogicalSetOp>),
    LockRows(Box<LogicalLockRows>),
}

/// What a leaf scan reads.
// CONTRACT
#[derive(Clone, Debug, PartialEq)]
pub enum ScanSource {
    /// Base table or virtual catalog relation (`is_virtual` = `oid < 16384`).
    Table { table: TableRef, is_virtual: bool },
    Subquery(LogicalPlan),
    Function { funcs: Vec<FunctionItem>, with_ordinality: bool },
    Values { rows: Vec<Vec<BoundExpr>> },
    /// CTE reference (`cte_id` indexes `PlannerState::ctes` / `Plan::ctes`).
    Cte { cte_id: u32 },
    /// The working table of the enclosing recursive CTE.
    WorkTable { cte_id: u32 },
    /// `SELECT` without FROM: exactly one empty row.
    OneRow,
}

/// A leaf scan with its pushed-down quals.
// CONTRACT
#[derive(Clone, Debug, PartialEq)]
pub struct LogicalScan {
    /// Range-table index (1-based) of the entry this scan produces (0 for `OneRow`).
    pub rt_index: u32,
    pub source: ScanSource,
    /// AND-ed conjuncts evaluated by the scan (bind-time terms, only this rt_index's columns plus outer
    /// references).
    pub quals: Vec<BoundExpr>,
    /// The scan needs the rowid slot (UPDATE/DELETE target, FOR UPDATE).
    pub with_rowid: bool,
}

/// A join of two logical inputs.
// CONTRACT
#[derive(Clone, Debug, PartialEq)]
pub struct LogicalJoin {
    pub join_type: JoinType,
    pub left: LogicalPlan,
    pub right: LogicalPlan,
    /// Join conditions (`ON` quals and WHERE conjuncts that pushdown moved to this join), AND-ed.
    pub quals: Vec<BoundExpr>,
    /// The right side references columns of the left side (LATERAL): must be a nested loop with
    /// parameters.
    pub lateral: bool,
}

/// Residual filter.
// CONTRACT
#[derive(Clone, Debug, PartialEq)]
pub struct LogicalFilter {
    pub input: LogicalPlan,
    pub quals: Vec<BoundExpr>,
}

/// Grouping/aggregation. Output vars: `group_exprs ++ aggrefs` (and the grouping mask when `sets`).
// CONTRACT
#[derive(Clone, Debug, PartialEq)]
pub struct LogicalAggregate {
    pub input: LogicalPlan,
    pub group_exprs: Vec<BoundExpr>,
    /// `Some(sets)`: indexes into `group_exprs` per grouping set.
    pub grouping_sets: Option<Vec<Vec<usize>>>,
    /// Distinct `Aggref` expressions (structural equality) used by target list / HAVING / ORDER BY.
    pub aggrefs: Vec<BoundExpr>,
    /// HAVING conjuncts in terms of the aggregate output.
    pub having: Vec<BoundExpr>,
}

/// One window definition with the window functions that use it. Output vars: `input.vars ++ funcs`.
// CONTRACT
#[derive(Clone, Debug, PartialEq)]
pub struct LogicalWindow {
    pub input: LogicalPlan,
    pub name: String,
    pub partition_by: Vec<BoundExpr>,
    pub order_by: Vec<SortKeySpec>,
    pub units: FrameUnits,
    pub start: FrameBound,
    pub end: FrameBound,
    pub exclusion: FrameExclusion,
    /// Distinct `WindowFunc` expressions using this window.
    pub funcs: Vec<BoundExpr>,
}

/// Projection (target list evaluation). Output vars = `exprs`.
// CONTRACT
#[derive(Clone, Debug, PartialEq)]
pub struct LogicalProject {
    pub input: LogicalPlan,
    pub exprs: Vec<BoundExpr>,
    /// Number of leading entries that are real output columns; the remaining ones are junk (ORDER BY /
    /// DISTINCT ON helpers) removed by a final projection after sorting/distinct.
    pub n_visible: usize,
    /// Some `exprs` contain set-returning functions (=> `ProjectSet`).
    pub has_srf: bool,
}

/// Sort.
// CONTRACT
#[derive(Clone, Debug, PartialEq)]
pub struct LogicalSort {
    pub input: LogicalPlan,
    pub keys: Vec<SortKeySpec>,
}

/// LIMIT / OFFSET.
// CONTRACT
#[derive(Clone, Debug, PartialEq)]
pub struct LogicalLimit {
    pub input: LogicalPlan,
    pub count: Option<BoundExpr>,
    pub offset: Option<BoundExpr>,
    pub with_ties: bool,
    /// Sort keys of the input used for WITH TIES comparison.
    pub ties_keys: Vec<SortKeySpec>,
}

/// DISTINCT / DISTINCT ON over the input's output slots.
// CONTRACT
#[derive(Clone, Debug, PartialEq)]
pub struct LogicalDistinct {
    pub input: LogicalPlan,
    /// Slots forming the key (all visible slots for plain DISTINCT).
    pub key_slots: Vec<u32>,
    /// `DISTINCT ON`: the input must be ordered so the first row per key is the wanted one.
    pub on: bool,
}

/// Set operation. `Union` (non-ALL) and `UnionAll` are expressed as `UnionAll` + `LogicalDistinct`.
// CONTRACT
#[derive(Clone, Debug, PartialEq)]
pub struct LogicalSetOp {
    pub kind: LogicalSetKind,
    pub left: LogicalPlan,
    pub right: LogicalPlan,
}

/// Set operation kinds.
// CONTRACT
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LogicalSetKind {
    UnionAll,
    Setop(SetOpCmd),
}

/// Row locking above a join tree (`FOR UPDATE`).
// CONTRACT
#[derive(Clone, Debug, PartialEq)]
pub struct LogicalLockRows {
    pub input: LogicalPlan,
    pub strength: LockStrength,
    pub wait: LockWait,
    /// (table oid, rt_index) pairs to lock; each rt_index's scan has `with_rowid`.
    pub tables: Vec<(Oid, u32)>,
}

/// Convenience: the type list of a plan's output.
pub fn out_types(p: &LogicalPlan) -> Vec<TypeId> {
    p.out.iter().map(|c| c.ty).collect()
}

/// `GroupBy` helper: positions of the grouping expressions in the target list (re-exported for the
/// builders).
pub fn group_target_indexes(g: &GroupBy) -> &[usize] {
    &g.exprs
}
