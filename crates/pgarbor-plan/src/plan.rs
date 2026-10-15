//! The physical plan: what `pgarbor_plan::plan` returns and `pgarbor_exec` runs. Plain data, fully
//! implemented, frozen (v1). THIS FILE IS THE PLAN/EXEC CONTRACT: the executor codes against it.
//!
//! ## Shape
//! ```text
//! Plan { kind, root: PlanNode, subplans: Vec<SubPlanDef>, ctes: Vec<CteDef>, output: Vec<ColumnDesc>,
//!        param_types, deps }
//! PlanNode { kind: NodeKind, out: Vec<OutCol>, est: Estimate, props: Vec<Prop> }
//! ```
//! Execution model: pull-based (volcano) iterators over **rows = `Vec<Datum>`**. Every node produces
//! rows of exactly `PlanNode::width()` values, described by `out` (name + type per slot).
//!
//! ## Expressions inside a plan
//! After `setrefs::finalize` (the last planner pass) every `BoundExpr` stored in a node is
//! *executable*: it contains `Const`, `Param`, `InputRef(slot)` (slot of the node's INPUT row, see each
//! node's "input row" rule), `OuterRef` (parameters of an enclosing rescan, see below), `FuncCall`,
//! `Op`, `Bool`, `Case`, ..., `SubPlan` and `Grouping{mask_slot: Some}`. It never contains
//! `Column`, `SubLink`, `Aggref`, `WindowFunc` (`pgarbor_sql::ir::expr` module docs, rule 3).
//! While the planner is still building the tree, the same fields hold bind-time expressions
//! (`Column{rt_index, attnum}`); `setrefs` converts them by matching against the child's output
//! expressions (`PlanNode::vars`, kept only until finalization).
//!
//! **Input row of a node**: scans: the node's own produced row; Filter/Sort/Limit/...: the child's row;
//! joins: `outer_row ++ inner_row` (NestedLoop/HashJoin: probe/outer first; MergeJoin: left ++ right;
//! Semi/Anti joins evaluate `qual` over `outer ++ inner` but OUTPUT only the outer row);
//! Project/ProjectSet/Aggregate/WindowAgg: the child's row.
//!
//! ## Parameters of rescans (`OuterRef`)
//! `ExprKind::OuterRef { levels_up: 0, index: i }` inside a node's subtree refers to value `i` of the
//! parameter vector supplied by the nearest enclosing *rescanning parent*:
//! * `NestedLoop::inner_params[i]` (expressions over the OUTER row, evaluated once per outer row,
//!   before (re)starting the inner subtree) — index nested-loop joins;
//! * `SubPlanRef::args[i]` for a correlated sub-plan;
//! * `CteScan`/`RecursiveUnion` have no parameters.
//! `levels_up: n` skips `n` further such boundaries (a sub-plan inside a sub-plan referring two
//! levels out). An *uncorrelated* sub-plan is executed once and its result cached.
//!
//! ## Sub-query boundaries
//! A sub-query in FROM (derived table, expanded view, inlined CTE) is planned recursively and appears in
//! the parent's tree as its own plan nodes. The planner puts a pass-through `Project` on top of it whose
//! expressions are the child's own output expressions and whose `vars` are the parent's `Column{rt_index,
//! attnum}` references; `setrefs` removes projections that turned out to be the identity on their child's
//! row (same width, `InputRef(0..n)`). The executor therefore never sees "subquery scan" nodes.
//!
//! ## Row identity
//! Scans with `with_rowid = true` append one trailing slot: the row id (`Datum::Int8`, the u64 rowid
//! of the storage key `[relid][rowid BE]` stored bit-for-bit). UPDATE/DELETE/SELECT FOR UPDATE rely on it.
//! Dropped columns appear as `Null` in the table-row part (slot = attnum - 1). `needed[i] == false`
//! tells the scan it may leave slot `i` as `Null` without decoding.
use pgarbor_catalog::invalidation::PlanDeps;
use pgarbor_catalog::persist::CatalogVersion;
use pgarbor_common::Oid;
use pgarbor_sql::ir::{BoundExpr, OpRef, SubLinkKind, TargetTable};
use pgarbor_types::catalog_data::Volatility;
use pgarbor_types::TypeId;

// ---------------------------------------------------------------------------------------------
// Plan, output, properties
// ---------------------------------------------------------------------------------------------

/// What kind of statement the plan implements (decides the command tag and result handling).
// CONTRACT
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PlanKind {
    /// `SELECT`/`VALUES`/`TABLE`: rows returned to the client, tag `SELECT n`.
    Select,
    /// `INSERT`: tag `INSERT 0 n`; rows returned only with RETURNING.
    Insert,
    /// `UPDATE`: tag `UPDATE n`.
    Update,
    /// `DELETE`: tag `DELETE n`.
    Delete,
    /// `CREATE TABLE AS` / `SELECT INTO` / `CREATE MATERIALIZED VIEW ... AS` / `REFRESH MATERIALIZED
    /// VIEW`: the root yields the rows to store; the executor creates/fills the table.
    Populate,
}

/// One result column as reported in RowDescription (ARCHITECTURE §10).
// CONTRACT
#[derive(Clone, Debug, PartialEq)]
pub struct ColumnDesc {
    pub name: String,
    pub type_id: TypeId,
    /// OID of the table the column comes from, 0 if not a plain table column.
    pub table_oid: Oid,
    /// Attribute number inside `table_oid`, 0 if not a plain table column.
    pub column_attnum: i16,
}

/// One produced slot of a node.
// CONTRACT
#[derive(Clone, Debug, PartialEq)]
pub struct OutCol {
    pub name: String,
    pub ty: TypeId,
}

/// Planner estimates (not used for correctness; EXPLAIN prints them when `COSTS` is on).
/// PGArbor has no table statistics: row counts come from defaults documented in `cost.rs`.
// CONTRACT
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Estimate {
    pub startup_cost: f64,
    pub total_cost: f64,
    pub rows: f64,
    /// Average row width in bytes.
    pub width: u32,
}

impl Default for Estimate {
    fn default() -> Estimate {
        Estimate { startup_cost: 0.0, total_cost: 0.0, rows: 1.0, width: 0 }
    }
}

/// Value of an EXPLAIN property.
// CONTRACT
#[derive(Clone, Debug, PartialEq)]
pub enum PropValue {
    Text(String),
    List(Vec<String>),
    Int(i64),
    Bool(bool),
}

/// One EXPLAIN property line of a node (`Filter: (a = 1)`, `Index Cond: (a = 1)`, `Sort Key: b, a DESC`,
/// `Group Key: b`, `Hash Cond: (t.a = u.a)`). Filled by `explain::annotate_plan` BEFORE `setrefs`
/// (while expressions can still be rendered with bind-time names).
// CONTRACT
#[derive(Clone, Debug, PartialEq)]
pub struct Prop {
    pub key: String,
    pub value: PropValue,
}

/// A complete plan.
// CONTRACT
#[derive(Clone, Debug, PartialEq)]
pub struct Plan {
    pub kind: PlanKind,
    pub root: PlanNode,
    /// Sub-plans referenced by `ExprKind::SubPlan(SubPlanRef { plan_id, .. })` (index = `plan_id`).
    pub subplans: Vec<SubPlanDef>,
    /// Common table expressions referenced by `CteScan { cte_id }` (index = `cte_id`) or executed
    /// eagerly (data-modifying CTEs).
    pub ctes: Vec<CteDef>,
    /// Result columns (empty for DML without RETURNING). For `Populate` plans: the columns of the
    /// table to create.
    pub output: Vec<ColumnDesc>,
    /// Parameter types `$1..$n` (final), same as `BindOutput::param_types`.
    pub param_types: Vec<TypeId>,
    /// Objects the plan depends on (for `invalidation::plan_is_stale`).
    pub deps: PlanDeps,
    /// Catalog snapshot version the plan was made against.
    pub catalog_version: CatalogVersion,
}

// ---------------------------------------------------------------------------------------------
// Nodes
// ---------------------------------------------------------------------------------------------

/// A plan tree node.
// CONTRACT
#[derive(Clone, Debug, PartialEq)]
pub struct PlanNode {
    pub kind: NodeKind,
    /// Produced slots; `out.len()` is the row width.
    pub out: Vec<OutCol>,
    pub est: Estimate,
    /// EXPLAIN properties (empty unless the plan was made for EXPLAIN).
    pub props: Vec<Prop>,
    /// Planner scratch: bind-time expression that produces each output slot (parallel to `out`).
    /// Used by `setrefs` to translate the parent's expressions; empty after finalization.
    pub vars: Vec<BoundExpr>,
    /// Planner scratch: ordering the node's output is known to have (bind-time key expressions); lets
    /// the planner skip redundant sorts. Empty after finalization.
    pub ordering: Vec<SortKeySpec>,
    /// Planner scratch: id of the query level whose range table names the bind-time columns in this
    /// node's expressions (`PlannerState::range_names[level]`). Needed by `explain::annotate_plan`, which
    /// runs after the whole tree is built. Reset to 0 by `setrefs`.
    pub level: u32,
}

impl PlanNode {
    /// Node with no properties/vars and default estimate.
    pub fn new(kind: NodeKind, out: Vec<OutCol>) -> PlanNode {
        PlanNode { kind, out, est: Estimate::default(), props: Vec::new(), vars: Vec::new(), ordering: Vec::new(), level: 0 }
    }
    /// Number of produced slots.
    pub fn width(&self) -> usize {
        self.out.len()
    }
    /// Output slot types.
    pub fn out_types(&self) -> Vec<TypeId> {
        self.out.iter().map(|c| c.ty).collect()
    }
    /// Direct children in execution order (outer/probe/left first). Sub-plans and CTEs are NOT
    /// children (they are reached through `Plan::subplans` / `Plan::ctes`).
    pub fn children(&self) -> Vec<&PlanNode> {
        match &self.kind {
            NodeKind::Result(_)
            | NodeKind::SeqScan(_)
            | NodeKind::IndexScan(_)
            | NodeKind::VirtualScan(_)
            | NodeKind::ValuesScan(_)
            | NodeKind::FunctionScan(_)
            | NodeKind::CteScan(_)
            | NodeKind::WorkTableScan(_) => Vec::new(),
            NodeKind::Filter(n) => vec![&n.input],
            NodeKind::Project(n) => vec![&n.input],
            NodeKind::ProjectSet(n) => vec![&n.input],
            NodeKind::NestedLoop(n) => vec![&n.outer, &n.inner],
            NodeKind::HashJoin(n) => vec![&n.outer, &n.inner],
            NodeKind::MergeJoin(n) => vec![&n.left, &n.right],
            NodeKind::Aggregate(n) => vec![&n.input],
            NodeKind::WindowAgg(n) => vec![&n.input],
            NodeKind::Sort(n) => vec![&n.input],
            NodeKind::Limit(n) => vec![&n.input],
            NodeKind::Distinct(n) => vec![&n.input],
            NodeKind::SetOp(n) => vec![&n.left, &n.right],
            NodeKind::Append(n) => n.children.iter().collect(),
            NodeKind::Materialize(n) => vec![&n.input],
            NodeKind::LockRows(n) => vec![&n.input],
            NodeKind::ModifyTable(n) => vec![&n.input],
        }
    }
    /// Mutable children (same order as [`PlanNode::children`]).
    pub fn children_mut(&mut self) -> Vec<&mut PlanNode> {
        match &mut self.kind {
            NodeKind::Result(_)
            | NodeKind::SeqScan(_)
            | NodeKind::IndexScan(_)
            | NodeKind::VirtualScan(_)
            | NodeKind::ValuesScan(_)
            | NodeKind::FunctionScan(_)
            | NodeKind::CteScan(_)
            | NodeKind::WorkTableScan(_) => Vec::new(),
            NodeKind::Filter(n) => vec![&mut n.input],
            NodeKind::Project(n) => vec![&mut n.input],
            NodeKind::ProjectSet(n) => vec![&mut n.input],
            NodeKind::NestedLoop(n) => vec![&mut n.outer, &mut n.inner],
            NodeKind::HashJoin(n) => vec![&mut n.outer, &mut n.inner],
            NodeKind::MergeJoin(n) => vec![&mut n.left, &mut n.right],
            NodeKind::Aggregate(n) => vec![&mut n.input],
            NodeKind::WindowAgg(n) => vec![&mut n.input],
            NodeKind::Sort(n) => vec![&mut n.input],
            NodeKind::Limit(n) => vec![&mut n.input],
            NodeKind::Distinct(n) => vec![&mut n.input],
            NodeKind::SetOp(n) => vec![&mut n.left, &mut n.right],
            NodeKind::Append(n) => n.children.iter_mut().collect(),
            NodeKind::Materialize(n) => vec![&mut n.input],
            NodeKind::LockRows(n) => vec![&mut n.input],
            NodeKind::ModifyTable(n) => vec![&mut n.input],
        }
    }
}

/// Node variants (module docs for the row layout of each).
// CONTRACT
#[derive(Clone, Debug, PartialEq)]
pub enum NodeKind {
    /// One row of constant-ish expressions, no input (`SELECT 1`, `INSERT ... VALUES` single row
    /// source, one-time false filter).
    Result(ResultNode),
    SeqScan(SeqScan),
    IndexScan(IndexScan),
    /// Virtual `pg_catalog`/`information_schema` relation (`pgarbor_catalog::vrel::scan`).
    VirtualScan(VirtualScan),
    ValuesScan(ValuesScan),
    FunctionScan(FunctionScan),
    /// Reads a CTE's materialized result (`Plan::ctes[cte_id]`).
    CteScan(CteScan),
    /// Reads the working table of the enclosing `CteDef::Recursive` (previous iteration's rows).
    WorkTableScan(WorkTableScan),
    Filter(Box<FilterNode>),
    Project(Box<ProjectNode>),
    /// Projection with set-returning functions (`SELECT generate_series(1,3), a`).
    ProjectSet(Box<ProjectSetNode>),
    NestedLoop(Box<NestedLoopNode>),
    HashJoin(Box<HashJoinNode>),
    MergeJoin(Box<MergeJoinNode>),
    Aggregate(Box<AggregateNode>),
    WindowAgg(Box<WindowAggNode>),
    Sort(Box<SortNode>),
    Limit(Box<LimitNode>),
    /// DISTINCT / DISTINCT ON (PG `Unique`/HashAggregate-as-distinct).
    Distinct(Box<DistinctNode>),
    /// INTERSECT / EXCEPT (UNION is `Append` + `Distinct`).
    SetOp(Box<SetOpNode>),
    Append(AppendNode),
    /// Buffers its input so it can be rescanned (inner side of a nested loop).
    Materialize(Box<MaterializeNode>),
    /// `SELECT ... FOR UPDATE/SHARE`.
    LockRows(Box<LockRowsNode>),
    ModifyTable(Box<ModifyTableNode>),
}

/// Reference to a base table used by scan and DML nodes.
// CONTRACT
#[derive(Clone, Debug, PartialEq)]
pub struct TableRef {
    pub oid: Oid,
    pub schema: String,
    pub name: String,
    /// Alias in the query (`FROM t x` -> `x`), equal to `name` when none.
    pub alias: String,
    /// Number of attribute slots INCLUDING dropped columns.
    pub ncols: u32,
    /// Column types by attnum-1 (dropped: `TypeId::of(0)`).
    pub col_types: Vec<TypeId>,
}

/// `Result`: evaluates `exprs` once (over no input row; `OuterRef`/`Param` allowed). If
/// `one_time_filter` is `Some` and evaluates to anything but TRUE, produces no rows.
// CONTRACT
#[derive(Clone, Debug, PartialEq)]
pub struct ResultNode {
    pub exprs: Vec<BoundExpr>,
    pub one_time_filter: Option<BoundExpr>,
}

/// Sequential scan. Output row: table row (`ncols` slots) [+ rowid]. `filter` is evaluated over the
/// produced row; rows where it is not TRUE are dropped.
// CONTRACT
#[derive(Clone, Debug, PartialEq)]
pub struct SeqScan {
    pub table: TableRef,
    pub filter: Option<BoundExpr>,
    pub with_rowid: bool,
    /// Per attnum-1: the column is read by some ancestor expression (others may stay Null).
    /// v1: the planner always sets every entry to `true` (column pruning is a later optimization; the
    /// executor must still honor `false` entries by leaving the slot Null if it wants to).
    pub needed: Vec<bool>,
}

/// Scan direction of an index scan.
// CONTRACT
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ScanDirection {
    Forward,
    Backward,
}

/// Static description of the index used (copied from the catalog so the executor needs no lookup).
// CONTRACT
#[derive(Clone, Debug, PartialEq)]
pub struct IndexRef {
    pub oid: Oid,
    pub name: String,
    pub unique: bool,
    pub primary: bool,
    pub nulls_not_distinct: bool,
    /// Key columns in index order (INCLUDE columns excluded).
    pub keys: Vec<IndexKeyRef>,
}

/// One key column of an index as needed for key encoding (`keycodec::KeyColumn`).
// CONTRACT
#[derive(Clone, Debug, PartialEq)]
pub struct IndexKeyRef {
    /// Table attnum for column keys; `None` for expression keys (not usable by `IndexBounds` in v1).
    pub column: Option<i16>,
    pub ty: TypeId,
    pub descending: bool,
    pub nulls_first: bool,
}

/// Equality (or IN-list) condition on one leading key column.
// CONTRACT
#[derive(Clone, Debug, PartialEq)]
pub enum KeyEq {
    /// `key = value`. `value` is evaluated once at scan start (`Const`, `Param`, `OuterRef`, stable
    /// expressions). NULL value => the scan returns no rows (strict `=`).
    Value(BoundExpr),
    /// `key = ANY (array)`: the array-valued expression yields values; the scan visits the sorted,
    /// de-duplicated non-NULL elements in key order (reverse for backward scans).
    AnyOf(BoundExpr),
}

/// One end of a range condition on a key column: `(value, inclusive)`.
// CONTRACT
#[derive(Clone, Debug, PartialEq)]
pub struct RangeEnd {
    pub value: BoundExpr,
    pub inclusive: bool,
}

/// Bounds of an index scan over `index.keys`. Interpretation (all key encodings via
/// `keycodec::encode_key` with the index's `KeyColumn`s):
/// * `eq[i]` constrains key column `i` for `i < eq.len()` (leading prefix, no gaps);
/// * `lower`/`upper` constrain key column `eq.len()` (None = unbounded on that side) - NULLs are never
///   returned by a range on a non-NULL bound;
/// * everything else is unconstrained. An index scan with empty `eq` and no range reads the whole
///   index in index order (used to satisfy ORDER BY).
// CONTRACT
#[derive(Clone, Debug, Default, PartialEq)]
pub struct IndexBounds {
    pub eq: Vec<KeyEq>,
    pub lower: Option<RangeEnd>,
    pub upper: Option<RangeEnd>,
}

/// Index scan (btree). Output row: table row [+ rowid], exactly like `SeqScan`. The executor walks the
/// index KV range described by `bounds`, fetches each row by rowid, evaluates `recheck`
/// (if any) and `filter`, and emits matching rows in index order (reversed for `Backward`).
/// Index-only scans do not exist in v1: index keys are memcomparable and not decodable.
// CONTRACT
#[derive(Clone, Debug, PartialEq)]
pub struct IndexScan {
    pub table: TableRef,
    pub index: IndexRef,
    pub bounds: IndexBounds,
    /// The conditions the index handles (for EXPLAIN `Index Cond`), in executable form. Evaluated as a
    /// recheck over the produced row only if `recheck_index_cond`.
    pub index_cond: Vec<BoundExpr>,
    pub recheck_index_cond: bool,
    /// Remaining quals (`Filter`), AND semantics.
    pub filter: Option<BoundExpr>,
    pub direction: ScanDirection,
    pub with_rowid: bool,
    pub needed: Vec<bool>,
}

/// Scan of a virtual catalog relation. Output row: relation columns (`ncols`). `eq_filters` are
/// `(attnum, value)` equality hints passed to `ScanRequest`; `filter` is the complete qual (always
/// re-applied by the executor).
// CONTRACT
#[derive(Clone, Debug, PartialEq)]
pub struct VirtualScan {
    pub table: TableRef,
    pub eq_filters: Vec<(i16, BoundExpr)>,
    pub filter: Option<BoundExpr>,
}

/// `VALUES` list. Output row: one value per column. Cell expressions may use `OuterRef`/`Param`.
// CONTRACT
#[derive(Clone, Debug, PartialEq)]
pub struct ValuesScan {
    pub rows: Vec<Vec<BoundExpr>>,
    pub filter: Option<BoundExpr>,
}

/// One function of a function scan.
// CONTRACT
#[derive(Clone, Debug, PartialEq)]
pub struct FunctionItem {
    /// `FuncCall` (set-returning or scalar). A set-returning call yields 0..n rows; a scalar call one.
    pub expr: BoundExpr,
    /// Types of the columns this function contributes (1 for scalar SRFs; N for composite/OUT).
    pub columns: Vec<TypeId>,
}

/// `FROM f(...)` / `ROWS FROM(f(), g())` / `unnest(...)` / `WITH ORDINALITY`. Several functions are
/// zipped: row i takes row i of each function, shorter ones padded with NULLs. Output row: columns of
/// all functions in order, then `ordinality bigint` (1-based) if `with_ordinality`.
// CONTRACT
#[derive(Clone, Debug, PartialEq)]
pub struct FunctionScan {
    pub funcs: Vec<FunctionItem>,
    pub with_ordinality: bool,
    pub filter: Option<BoundExpr>,
}

/// CTE reference. Output row: the CTE's output columns. `filter` over that row.
// CONTRACT
#[derive(Clone, Debug, PartialEq)]
pub struct CteScan {
    pub cte_id: u32,
    pub filter: Option<BoundExpr>,
}

/// Working-table scan (only inside the recursive term of `CteDef::Recursive`).
// CONTRACT
#[derive(Clone, Debug, PartialEq)]
pub struct WorkTableScan {
    pub cte_id: u32,
    pub filter: Option<BoundExpr>,
}

/// Row filter: passes rows whose `predicate` is TRUE.
// CONTRACT
#[derive(Clone, Debug, PartialEq)]
pub struct FilterNode {
    pub input: PlanNode,
    pub predicate: BoundExpr,
}

/// Projection: output row = `exprs` evaluated over the input row.
// CONTRACT
#[derive(Clone, Debug, PartialEq)]
pub struct ProjectNode {
    pub input: PlanNode,
    pub exprs: Vec<BoundExpr>,
}

/// Projection with set-returning functions. For each input row evaluate all `exprs`; an expression
/// that is a top-level `FuncCall` with `retset` (or contains one nested inside non-SRF expressions) is
/// a SRF: all SRFs advance in lock step, producing `max(len)` rows, shorter ones padded with NULL; an
/// input row where every SRF yields zero rows produces no output. Non-SRF expressions repeat.
// CONTRACT
#[derive(Clone, Debug, PartialEq)]
pub struct ProjectSetNode {
    pub input: PlanNode,
    pub exprs: Vec<BoundExpr>,
}

/// Join type of join nodes. Semi/Anti come from EXISTS/IN rewriting and are output-only-outer.
// CONTRACT
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum JoinType {
    Inner,
    Left,
    Right,
    Full,
    /// Emit each outer row once if some inner row satisfies `qual`.
    Semi,
    /// Emit each outer row for which NO inner row satisfies `qual`.
    Anti,
}

/// Nested loop join. For each outer row: evaluate `inner_params` over it (-> `OuterRef{0,i}` values),
/// (re)start `inner`, join. Supports `Inner`, `Left`, `Semi`, `Anti` (the planner swaps sides for
/// RIGHT and uses hash/merge for FULL). `qual` is evaluated over `outer ++ inner`; for outer joins a
/// failing/absent match emits `outer ++ NULLs`. Output: `outer ++ inner` (`Semi`/`Anti`: outer only).
// CONTRACT
#[derive(Clone, Debug, PartialEq)]
pub struct NestedLoopNode {
    pub join_type: JoinType,
    pub outer: PlanNode,
    pub inner: PlanNode,
    pub qual: Option<BoundExpr>,
    pub inner_params: Vec<BoundExpr>,
}

/// Hash join. The INNER child is the build side. `outer_keys[i]` is evaluated over the outer row,
/// `inner_keys[i]` over the inner row; keys are compared with `compare::not_distinct`-style equality
/// after hashing with `compare::hash_datum` (`=` operator semantics: NULL keys never match).
/// `qual` (residual join filter, may be None) is evaluated over `outer ++ inner`. Supports all
/// join types; `Right`/`Full` emit unmatched build rows as `NULLs ++ inner`. Output: `outer ++ inner`
/// (`Semi`/`Anti`: outer only).
// CONTRACT
#[derive(Clone, Debug, PartialEq)]
pub struct HashJoinNode {
    pub join_type: JoinType,
    pub outer: PlanNode,
    pub inner: PlanNode,
    pub outer_keys: Vec<BoundExpr>,
    pub inner_keys: Vec<BoundExpr>,
    pub eq_ops: Vec<OpRef>,
    pub qual: Option<BoundExpr>,
}

/// One merge key.
// CONTRACT
#[derive(Clone, Debug, PartialEq)]
pub struct MergeKey {
    pub expr: BoundExpr,
    pub descending: bool,
    pub nulls_first: bool,
}

/// Merge join; both inputs are sorted by their keys (the planner inserts `Sort`). `left_keys`/
/// `right_keys` are evaluated over the respective child rows and compared pairwise with the type's
/// ordering (`compare::cmp_sort`, NULLs per `nulls_first`; NULL keys never match). Supports all join
/// types. Output `left ++ right` (`Semi`/`Anti`: left only).
// CONTRACT
#[derive(Clone, Debug, PartialEq)]
pub struct MergeJoinNode {
    pub join_type: JoinType,
    pub left: PlanNode,
    pub right: PlanNode,
    pub left_keys: Vec<MergeKey>,
    pub right_keys: Vec<MergeKey>,
    pub qual: Option<BoundExpr>,
}

/// Aggregation strategy.
// CONTRACT
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AggStrategy {
    /// No grouping: one output row for the whole input (even for empty input).
    Plain,
    /// Hash table keyed by `group_keys`; output order unspecified.
    Hashed,
    /// Input is sorted by `group_keys`; groups are consecutive.
    Sorted,
}

/// An aggregate call to evaluate (`agg::make_aggregate(AggSpec{..})`).
// CONTRACT
#[derive(Clone, Debug, PartialEq)]
pub struct AggCall {
    pub agg: Oid,
    pub name: String,
    /// Aggregated arguments over the input row.
    pub args: Vec<BoundExpr>,
    /// Direct arguments (ordered-set aggregates: `percentile_cont(0.5)`); evaluated once per group over
    /// the group's FIRST input row (they are constants, parameters or group keys).
    pub direct_args: Vec<BoundExpr>,
    pub distinct: bool,
    pub filter: Option<BoundExpr>,
    /// `agg(x ORDER BY ...)` / `WITHIN GROUP (ORDER BY ...)` sort keys over the input row.
    pub order_by: Vec<SortKeySpec>,
    pub star: bool,
    pub result_type: TypeId,
    pub arg_types: Vec<TypeId>,
}

/// Aggregation. Output row: `group_keys` values (in order) ++ one value per `AggCall` ++ (only when
/// `grouping_sets` is `Some`) the grouping mask `int8` (bit k set = key k is NOT in the current set).
/// `Plain` has no keys: always exactly one row. With `grouping_sets` each set is aggregated
/// separately; keys not in the set output NULL. `having` is evaluated over the OUTPUT row.
// CONTRACT
#[derive(Clone, Debug, PartialEq)]
pub struct AggregateNode {
    pub strategy: AggStrategy,
    pub input: PlanNode,
    pub group_keys: Vec<BoundExpr>,
    pub aggs: Vec<AggCall>,
    pub grouping_sets: Option<Vec<Vec<usize>>>,
    pub having: Option<BoundExpr>,
}

/// A window function call.
// CONTRACT
#[derive(Clone, Debug, PartialEq)]
pub struct WindowCall {
    pub func: Oid,
    pub name: String,
    pub args: Vec<BoundExpr>,
    /// Aggregate used as window function (`sum(x) OVER`) rather than `row_number()` & co.
    pub is_agg: bool,
    pub star: bool,
    pub filter: Option<BoundExpr>,
    pub ignore_nulls: bool,
    pub result_type: TypeId,
    pub arg_types: Vec<TypeId>,
}

/// Window frame (copied from the bound `WindowDef`; offsets are executable expressions).
// CONTRACT
#[derive(Clone, Debug, PartialEq)]
pub struct FrameSpec {
    pub units: pgarbor_sql::ir::FrameUnits,
    pub start: pgarbor_sql::ir::FrameBound,
    pub end: pgarbor_sql::ir::FrameBound,
    pub exclusion: pgarbor_sql::ir::FrameExclusion,
}

/// Window aggregation for ONE window definition. The input is sorted by `partition_by` then
/// `order_by`. Output row: input row ++ one value per `funcs`.
// CONTRACT
#[derive(Clone, Debug, PartialEq)]
pub struct WindowAggNode {
    pub input: PlanNode,
    pub partition_by: Vec<BoundExpr>,
    pub order_by: Vec<SortKeySpec>,
    pub frame: FrameSpec,
    /// Name for EXPLAIN (`w1`, or the WINDOW clause name).
    pub window_name: String,
    pub funcs: Vec<WindowCall>,
}

/// One sort key.
// CONTRACT
#[derive(Clone, Debug, PartialEq)]
pub struct SortKeySpec {
    pub expr: BoundExpr,
    pub descending: bool,
    pub nulls_first: bool,
    /// Collation OID (0 = default; v1 sorts by code point regardless).
    pub collation: Oid,
    /// `USING <op>`; None = the type's default `<` (asc) / `>` (desc).
    pub using_op: Option<OpRef>,
}

/// Sort. Output = input rows ordered by `keys` (stable not required). `limit`: if Some(n) only the
/// first n rows after sorting (offset already added) are needed (top-N heap sort allowed).
// CONTRACT
#[derive(Clone, Debug, PartialEq)]
pub struct SortNode {
    pub input: PlanNode,
    pub keys: Vec<SortKeySpec>,
    pub limit: Option<u64>,
}

/// LIMIT/OFFSET/FETCH. `count`/`offset` are evaluated once at start (`Param`/`Const`/stable): NULL =
/// no limit / 0 offset; negative -> 2201W `LIMIT must not be negative` / 2201X `OFFSET must not be
/// negative`. `with_ties`: after `count` rows continue while the next row's `ties_keys` equal the last
/// emitted row's (input must be sorted by them).
// CONTRACT
#[derive(Clone, Debug, PartialEq)]
pub struct LimitNode {
    pub input: PlanNode,
    pub count: Option<BoundExpr>,
    pub offset: Option<BoundExpr>,
    pub with_ties: bool,
    pub ties_keys: Vec<SortKeySpec>,
}

/// How a `Distinct` node de-duplicates.
// CONTRACT
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DistinctStrategy {
    /// Hash table of seen keys; output order unspecified.
    Hashed,
    /// Input sorted so equal keys are adjacent: emit the first row of each run.
    Sorted,
}

/// DISTINCT / DISTINCT ON. `key_slots` are input-row slots forming the dedup key (all non-junk output
/// slots for plain DISTINCT; the DISTINCT ON expressions' slots otherwise). Rows are compared with
/// `compare::not_distinct` (NULLs equal). Output = the FIRST input row of each key. Output row = input
/// row (junk columns, if any, are stripped by a Project above).
// CONTRACT
#[derive(Clone, Debug, PartialEq)]
pub struct DistinctNode {
    pub strategy: DistinctStrategy,
    pub input: PlanNode,
    pub key_slots: Vec<u32>,
}

/// INTERSECT / EXCEPT variant.
// CONTRACT
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SetOpCmd {
    Intersect,
    IntersectAll,
    Except,
    ExceptAll,
}

/// INTERSECT/EXCEPT [ALL]. Rows are compared on ALL slots with `not_distinct` equality. Output row =
/// left row layout. Semantics: `Intersect` distinct rows present on both sides; `IntersectAll` each
/// row min(l, r) times; `Except` distinct rows of left absent from right; `ExceptAll` max(l - r, 0)
/// copies.
// CONTRACT
#[derive(Clone, Debug, PartialEq)]
pub struct SetOpNode {
    pub cmd: SetOpCmd,
    pub left: PlanNode,
    pub right: PlanNode,
}

/// Append (UNION ALL): all children produce the same slot types; rows are emitted child by child.
// CONTRACT
#[derive(Clone, Debug, PartialEq)]
pub struct AppendNode {
    pub children: Vec<PlanNode>,
}

/// Materialize.
// CONTRACT
#[derive(Clone, Debug, PartialEq)]
pub struct MaterializeNode {
    pub input: PlanNode,
}

/// Row lock strengths (same as `pgarbor_sql::ir::LockStrength`).
pub use pgarbor_sql::ir::{LockStrength, LockWait};

/// One locked table of a `LockRows` node.
// CONTRACT
#[derive(Clone, Debug, PartialEq)]
pub struct LockTarget {
    pub table_oid: Oid,
    /// Input slot holding that table's rowid.
    pub rowid_slot: u32,
}

/// `FOR UPDATE ...`: for each input row take a key lock (`KvWrite` row lock via the txn's lock hook) on
/// every `targets` row; `SkipLocked` drops rows that cannot be locked, `NoWait` raises 55P03
/// `could not obtain lock on row in relation "t"`. Output = input row.
// CONTRACT
#[derive(Clone, Debug, PartialEq)]
pub struct LockRowsNode {
    pub input: PlanNode,
    pub strength: LockStrength,
    pub wait: LockWait,
    pub targets: Vec<LockTarget>,
}

// ---------------------------------------------------------------------------------------------
// DML
// ---------------------------------------------------------------------------------------------

/// Operation of a `ModifyTable`.
// CONTRACT
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ModifyOp {
    Insert,
    Update,
    Delete,
}

/// INSERT specifics. The input child produces rows with ONE value per entry of `columns`, in that order
/// (values already assignment-coerced; no `SetToDefault` left). For every other live column the executor
/// computes the value from `defaults` (planner-prepared: column DEFAULT, identity `nextval(..)`, or
/// `Const(NULL)`), then generated columns from `generated`, assembling a full row of `table.ncols`
/// slots (dropped = NULL). `Const(NULL)` is also used for explicit nothing.
// CONTRACT
#[derive(Clone, Debug, PartialEq)]
pub struct InsertSpec {
    /// attnums supplied by the input row, in input order.
    pub columns: Vec<i16>,
    /// For attnums NOT in `columns` (and for supplied columns replaced by `OVERRIDING USER VALUE`):
    /// the expression evaluated per row over no input (volatile functions such as `nextval` run once per
    /// row). Includes an entry for EVERY live attnum not in `columns` that is not a stored generated column.
    pub defaults: Vec<(i16, BoundExpr)>,
    /// Stored generated columns: expression over the FULL NEW ROW (`InputRef(attnum-1)`), evaluated
    /// after defaults, in dependency (attnum) order.
    pub generated: Vec<(i16, BoundExpr)>,
    pub on_conflict: Option<ConflictSpec>,
}

/// `ON CONFLICT` handling.
// CONTRACT
#[derive(Clone, Debug, PartialEq)]
pub struct ConflictSpec {
    /// Unique/PK index OIDs whose violation is "the conflict" (empty + `any_unique` = all unique
    /// indexes of the table).
    pub arbiter_indexes: Vec<Oid>,
    pub any_unique: bool,
    pub action: ConflictPlanAction,
}

/// What to do on conflict.
// CONTRACT
#[derive(Clone, Debug, PartialEq)]
pub enum ConflictPlanAction {
    DoNothing,
    /// `SET` assignments `(attnum, value)` and optional `WHERE`, evaluated over
    /// `existing_row ++ excluded_row` (each `table.ncols` slots: existing = slots `0..n`, excluded =
    /// `n..2n`; the planner rewrote `excluded.c` / `t.c` references accordingly); then generated columns
    /// are recomputed and CHECKs re-run. Raises 21000 if a row is affected twice.
    DoUpdate { assignments: Vec<(i16, BoundExpr)>, where_: Option<BoundExpr> },
}

/// UPDATE specifics. The input child produces: `table row (ncols) ++ rowid ++ extra FROM columns`
/// (the target scan has `with_rowid`). `assignments` are `(attnum, value)` with `value` over the input row;
/// the new row = old row with assignments applied (`SetToDefault` replaced by the column default
/// expression), then `generated` recomputed (over the new row), NOT NULL / CHECK re-validated.
/// A target row matched by several input rows is updated once (first match; PG semantics).
// CONTRACT
#[derive(Clone, Debug, PartialEq)]
pub struct UpdateSpec {
    pub assignments: Vec<(i16, BoundExpr)>,
    pub generated: Vec<(i16, BoundExpr)>,
}

/// Table-level facts needed by DML execution (from `pgarbor_sql::ir::TargetTable` plus planner output).
// CONTRACT
#[derive(Clone, Debug, PartialEq)]
pub struct ModifyTarget {
    pub table: TableRef,
    /// Full target description (names, not-null flags, identity) for error messages and constraint
    /// reporting.
    pub meta: TargetTable,
    /// CHECK constraints rewritten over the NEW row (`InputRef(attnum-1)`), `(constraint oid, name, expr)`.
    pub checks: Vec<(Oid, String, BoundExpr)>,
}

/// INSERT / UPDATE / DELETE.
///
/// Input row layouts: INSERT see [`InsertSpec`]; UPDATE see [`UpdateSpec`]; DELETE: `table row ++ rowid ++
/// extra USING columns`. `rowid_slot` = slot of the target rowid (UPDATE/DELETE).
/// Executor duties for each affected row: enforce NOT NULL (23502 `null value in column "a" of
/// relation "t" violates not-null constraint`), CHECKs (23514 `new row for relation "t" violates check
/// constraint "c"`), unique indexes (23505 `duplicate key value violates unique constraint "i"`),
/// foreign keys, index maintenance, domain constraints (inside expressions), then RETURNING.
/// **Output**: if `returning` is empty the node emits no rows (only counts: `rows_affected`); else it
/// emits one row per affected row: `returning` expressions evaluated over the RETURNING INPUT ROW =
/// `[new table row (INSERT/UPDATE) | deleted row (DELETE)] (ncols slots) ++ [input row slots from
/// `returning_extra_from` onward: the extra FROM/USING columns]`.
// CONTRACT
#[derive(Clone, Debug, PartialEq)]
pub struct ModifyTableNode {
    pub op: ModifyOp,
    pub target: ModifyTarget,
    pub input: PlanNode,
    pub rowid_slot: Option<u32>,
    pub insert: Option<InsertSpec>,
    pub update: Option<UpdateSpec>,
    pub returning: Vec<BoundExpr>,
    pub returning_extra_from: Option<u32>,
}

// ---------------------------------------------------------------------------------------------
// Sub-plans and CTEs
// ---------------------------------------------------------------------------------------------

/// A sub-plan referenced by `ExprKind::SubPlan`.
///
/// Evaluation by `kind` (PG semantics): `Exists`: true iff the plan yields a row (stop at first);
/// `Scalar`: first column of the single row, NULL for none, 21000 `more than one row returned by a
/// subquery used as an expression` for more; `Array`: array of the single column; `Row`: composite
/// of the row; `Any`/`All`: three-valued `test op ANY/ALL (plan column(s))` using `SubPlanRef::ops` over the
/// plan's output columns (`use_hash`: build a hash set of the plan output once; NULL-aware: if no match
/// and the set contained a NULL (or the test is NULL) the result is NULL).
/// `n_params` = length of `SubPlanRef::args`; the plan reads them as `OuterRef{levels_up: 0, index}`.
// CONTRACT
#[derive(Clone, Debug, PartialEq)]
pub struct SubPlanDef {
    pub id: u32,
    pub kind: SubLinkKind,
    pub plan: PlanNode,
    pub n_params: u32,
    /// Uncorrelated (`n_params == 0` and no `OuterRef`): execute once, cache the result (PG InitPlan).
    pub is_initplan: bool,
    pub volatility: Volatility,
}

/// CTE body.
// CONTRACT
#[derive(Clone, Debug, PartialEq)]
pub enum CteBodyPlan {
    /// Plain query: executed lazily on first reference, result buffered; referenced by `CteScan`.
    Query(PlanNode),
    /// `WITH RECURSIVE`: iterate `recursive` (which reads `WorkTableScan{cte_id}`) until it yields no
    /// rows; `union_all == false` de-duplicates (against everything produced so far). Result = all rows
    /// produced (non-recursive term first).
    Recursive { non_recursive: PlanNode, recursive: PlanNode, union_all: bool },
    /// Data-modifying CTE (`INSERT/UPDATE/DELETE ... RETURNING`): executed exactly once, EAGERLY at
    /// statement start (even if unreferenced); its RETURNING rows are buffered for `CteScan`s.
    Modify(PlanNode),
}

/// One CTE of the plan.
// CONTRACT
#[derive(Clone, Debug, PartialEq)]
pub struct CteDef {
    pub id: u32,
    pub name: String,
    pub body: CteBodyPlan,
    pub columns: Vec<OutCol>,
    /// Always buffer the result (referenced more than once, `MATERIALIZED`, recursive or DML);
    /// `false` = single-reference non-recursive CTE inlined by the planner (then it does not appear here).
    pub materialize: bool,
}
