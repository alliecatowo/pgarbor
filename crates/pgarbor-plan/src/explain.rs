//! EXPLAIN support, part 1: node labels, properties, annotation pass (PLN-15). PG references:
//! `commands/explain.c` (`ExplainNode`, `show_plan_tlist`, `show_scan_qual`, `show_upper_qual`,
//! `show_sort_keys`, `show_agg_keys`, `show_expression`, `ExplainSubPlans`, `ExplainPreScanNode`,
//! `explain_get_index_name`), `utils/adt/ruleutils.c` (`deparse_expression`, `set_deparse_context_plan`).
//!
//! `COSTS OFF` output is what the differential tests compare. Fixtures below are PG 18.6 output for the
//! shapes PGArbor produces (PGArbor has no table statistics, so plan SHAPES may legitimately differ from
//! PG's choice; the FORMAT of every line must match).
//!
//! ## Node labels ([`node_label`], first line of a node, text format)
//! | NodeKind | label |
//! |---|---|
//! | `SeqScan` | `Seq Scan on t` / `Seq Scan on t x` when alias != name; schema-qualified as `s.t` only in VERBOSE (`Seq Scan on scr_tmp.u`) |
//! | `IndexScan` | `Index Scan using t_pkey on t`; `Index Scan Backward using t_pkey on t` |
//! | `VirtualScan` | `Seq Scan on pg_class` (PG shows catalogs as seq scans) |
//! | `ValuesScan` | `Values Scan on "*VALUES*"` (alias as given: `v`) |
//! | `FunctionScan` | `Function Scan on generate_series g` (function name then alias; alias omitted when equal) |
//! | `CteScan` | `CTE Scan on r` / `CTE Scan on x y` ; `WorkTableScan` `WorkTable Scan on r` |
//! | `Result` | `Result` |
//! | `Filter` | (no node: filters are shown as the child's `Filter:` property; a standalone Filter node prints as `Result` with `One-Time Filter` only if constant, else `Filter` is merged into its child's props by `annotate`) |
//! | `Project` | no node in PG output unless it computes: PGArbor `Project` nodes are NOT shown (their expressions are shown as `Output:` in VERBOSE); skip the node, show its child |
//! | `ProjectSet` | `ProjectSet` |
//! | `NestedLoop` | `Nested Loop`, `Nested Loop Left Join`, `Nested Loop Semi Join`, `Nested Loop Anti Join` |
//! | `HashJoin` | `Hash Join`, `Hash Left Join`, `Hash Right Join`, `Hash Full Join`, `Hash Semi Join`, `Hash Anti Join`; the INNER child is rendered under an extra `->  Hash` node (no props; verbose: `Output:`) |
//! | `MergeJoin` | `Merge Join` / `Merge Left Join` ...; props `Merge Cond: (a = b)` |
//! | `Aggregate` | `Aggregate` (Plain), `HashAggregate` (+`Group Key: b, c`), `GroupAggregate` (Sorted, +`Group Key`), grouping sets: `MixedAggregate`/`HashAggregate` with `Hash Key`/`Group Key: (a, b)` lines per set (`Group Key: ()` for the empty set) |
//! | `WindowAgg` | `WindowAgg` + `Window: w1 AS (PARTITION BY b ORDER BY a ROWS UNBOUNDED PRECEDING)` (frame text only when non-default) |
//! | `Sort` | `Sort` + `Sort Key: b, a DESC NULLS FIRST` (direction/nulls only when non-default); top-N: `Sort Method` only with ANALYZE |
//! | `Limit` | `Limit` |
//! | `Distinct` | Hashed: `HashAggregate` + `Group Key: b`; Sorted: `Unique` (no props; keys shown only VERBOSE) |
//! | `SetOp` | `HashSetOp Intersect`, `HashSetOp Intersect All`, `HashSetOp Except`, `HashSetOp Except All` |
//! | `Append` | `Append` |
//! | `Materialize` | `Materialize` |
//! | `LockRows` | `LockRows` |
//! | `ModifyTable` | `Insert on t` / `Update on t` / `Delete on t` (alias after the name when it differs: `Update on t x`); INSERT ... VALUES root shows `->  Result` / `->  Values Scan on "*VALUES*"`; `ON CONFLICT`: props `Conflict Resolution: NOTHING` / `UPDATE`, `Conflict Arbiter Indexes: t_pkey`, `Conflict Filter: ...` |
//!
//! ## Properties ([`annotate_plan`] fills `PlanNode::props`, in this order, only when non-empty)
//! scans: `Index Cond: (a = 5)` (IndexScan: from `index_cond`, AND-ed conjuncts joined `AND`, each
//! parenthesized as `deparse_expr` gives; single condition printed bare `(a = 5)`), `Filter: (...)`;
//! joins: `Join Filter:` (NestedLoop qual), `Hash Cond: (u.a = t.a)` (outer key = inner key per key
//! pair), `Merge Cond:`, plus `Filter:` for HashJoin/MergeJoin residual `qual` printed as `Join Filter`;
//! `Sort Key`, `Group Key`, `Filter` (HAVING on Aggregate), `One-Time Filter: false`, `Recheck`
//! none. VERBOSE adds `Output: a, b` (the node's `vars` deparsed) and schema-qualified names. Expressions
//! are rendered by `deparse::expr::deparse_expr` with a [`PlanColumnNamer`] built from
//! `PlannerState::range_names[node.level]` (each node remembers its query level). **Qualification** (PG
//! `show_scan_qual` / `show_upper_qual` / `get_parameter`, oracle-verified): scan nodes (`Filter`, `Index
//! Cond`, `Recheck`) print bare column names unless VERBOSE; "upper" nodes (join conditions, `Join
//! Filter`, `Sort Key`, `Group Key`, `Merge Cond`, `Hash Cond`, one-time filters, projections) print
//! `alias.col` when the plan's flattened range table has more than one entry (count the relation entries of
//! ALL levels in `range_names`) or VERBOSE; a column of ANOTHER plan node reached through a parameter
//! (`OuterRef`, e.g. `Index Cond: (a = u.a)` under a nested loop) is always qualified. Constants print like
//! `get_const_expr` (`(c > '2'::numeric)`).
//! Sub-plan references print `(SubPlan 1)` / `(InitPlan 2).col1`; `Param` `$1`.
//!
//! ## Sub-plans and CTEs in the text tree
//! A node that owns sub-plans prints, AFTER its own properties and BEFORE its `->` children, one block per
//! initplan, and AFTER its children one block per correlated sub-plan:
//! ```text
//! Seq Scan on t
//!   Filter: (a = (InitPlan 2).col1)
//!   InitPlan 2
//!     ->  Result
//!           InitPlan 1
//!             ->  Limit
//!                   ->  Index Only Scan Backward using u_a_idx on u
//!                         Index Cond: (a IS NOT NULL)
//! ```
//! ```text
//! Seq Scan on t
//!   SubPlan 1
//!     ->  Aggregate
//!           ->  Bitmap Heap Scan on u
//! ```
//! CTE blocks `CTE x` + `->` plan are printed on the node where the CTE belongs (the root node of the
//! query level that defines it), after props, before children (see fixtures in `explain_fmt`).
//! Numbering: `plan_id + 1` for SubPlan/InitPlan; CTEs by name.
//!
//! Fixtures (PG 18.6, `COSTS OFF`, shape depends on statistics; format is what matters):
//! ```text
//! Limit
//!   ->  Index Scan Backward using t_pkey on t
//! Hash Join
//!   Hash Cond: (u.a = t.a)
//!   ->  Seq Scan on u
//!   ->  Hash
//!         ->  Seq Scan on t
//! Nested Loop
//!   Join Filter: ((t.a + 1) < u.d)
//!   ->  Seq Scan on u
//!   ->  Materialize
//!         ->  Seq Scan on t
//! Sort
//!   Sort Key: (count(*)) DESC
//!   ->  HashAggregate
//!         Group Key: b
//!         ->  Seq Scan on t
//! Update on t
//!   ->  Nested Loop
//!         ->  Seq Scan on u
//!               Filter: (d = 1)
//!         ->  Index Scan using t_pkey on t
//!               Index Cond: (a = u.a)
//! WindowAgg
//!   Window: w1 AS (PARTITION BY b ORDER BY a ROWS UNBOUNDED PRECEDING)
//!   ->  Sort
//!         Sort Key: b, a
//!         ->  Seq Scan on t
//! ```
//! (Aggregate output references inside Sort Key print as `(count(*))` with parentheses: a non-Var
//! expression referring to a child output prints parenthesized.)
use pgarbor_common::{PgError, Result};
use pgarbor_sql::deparse::ColumnNamer;
use pgarbor_sql::ir::{ColumnRef, ExplainOptions, OuterRef};

use crate::context::PlannerState;
use crate::plan::{NodeKind, Plan, PlanNode};

/// Namer used while annotating: bind-time `Column` refs through the range names of the node's query
/// level (`PlannerState::range_names[node.level]`); `qualify` is decided PER PROPERTY by `annotate_node`
/// (scan quals: VERBOSE only; upper-node expressions: multi-relation plan or VERBOSE, see module docs).
/// `outer_params` is the stack of rendered parameter vectors for `OuterRef`: annotate pushes the rendered
/// (always qualified) `NestedLoop::inner_params` before descending into the inner subtree and the rendered
/// `SubPlanRef::args` before annotating a sub-plan body; `OuterRef{levels_up: n, index}` reads
/// `outer_params[len-1-n][index]`.
// CONTRACT
#[derive(Clone, Debug, Default)]
pub struct PlanColumnNamer {
    /// Range table names of the query level being annotated: (alias, column names by attnum-1).
    pub ranges: Vec<(String, Vec<String>)>,
    pub qualify: bool,
    pub outer_params: Vec<Vec<String>>,
}

impl ColumnNamer for PlanColumnNamer {
    fn column(&self, col: &ColumnRef) -> Option<String> {
        if col.levels_up != 0 || col.attnum < 1 {
            return None;
        }
        let (alias, cols) = self.ranges.get((col.rt_index as usize).checked_sub(1)?)?;
        let name = pgarbor_types::text::format::quote_identifier(cols.get(col.attnum as usize - 1)?).ok()?;
        if self.qualify {
            Some(format!("{}.{}", pgarbor_types::text::format::quote_identifier(alias).ok()?, name))
        } else {
            Some(name)
        }
    }
    fn input_ref(&self, _index: u32) -> Option<String> {
        None
    }
    fn outer_ref(&self, outer: &OuterRef) -> Option<String> {
        let level = self.outer_params.len().checked_sub(1 + outer.levels_up as usize)?;
        self.outer_params.get(level)?.get(outer.index as usize).cloned()
    }
    fn subplan(&self, plan_id: u32) -> Option<String> {
        Some(format!("(SubPlan {})", plan_id + 1))
    }
}

/// First line of a node in text format (table above), without indentation or `->`.
#[allow(unused_variables)]
pub fn node_label(node: &PlanNode, verbose: bool) -> Result<String> {
    Err(PgError::unimplemented("PLN-15", "EXPLAIN node label (explain::node_label)"))
}

/// Fill `props` of every node in the tree rooted at `node` (and in sub-plans/CTEs held by `st`).
/// Runs BEFORE `setrefs::finalize` (expressions are still bind-time).
#[allow(unused_variables)]
pub fn annotate_node(st: &mut PlannerState, node: &mut PlanNode) -> Result<()> {
    Err(PgError::unimplemented("PLN-15", "EXPLAIN property annotation (explain::annotate_node)"))
}

/// Annotate a whole finished (but not yet finalized) plan: root, every `SubPlanDef::plan`, every CTE body.
#[allow(unused_variables)]
pub fn annotate_plan(st: &mut PlannerState, root: &mut PlanNode) -> Result<()> {
    Err(PgError::unimplemented("PLN-15", "EXPLAIN plan annotation (explain::annotate_plan)"))
}

/// Render a finalized plan as EXPLAIN output lines in the format of `options` (delegates to
/// `explain_fmt`): `Text` -> lines; `Json`/`Yaml`/`Xml` -> the document split into lines by `\n`
/// (the executor wraps them as the single-column result `QUERY PLAN`; JSON is ONE row containing the
/// whole document).
pub fn explain_plan(plan: &Plan, options: &ExplainOptions) -> Result<Vec<String>> {
    crate::explain_fmt::render(plan, options)
}

/// Is `kind` rendered as a node in the EXPLAIN tree? (`Project` and `Filter` are folded into their
/// child, see the label table.) Fully implemented.
pub fn is_shown(kind: &NodeKind) -> bool {
    !matches!(kind, NodeKind::Project(_) | NodeKind::Filter(_))
}
