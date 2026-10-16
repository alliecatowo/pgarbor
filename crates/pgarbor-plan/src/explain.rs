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
use pgarbor_common::{PgError, Result, SqlState};
use pgarbor_sql::deparse::expr::deparse_expr;
use pgarbor_sql::deparse::{ColumnNamer, DeparseCtx, DeparseOptions};
use pgarbor_sql::ir::{
    BoundExpr, ColumnRef, ExplainOptions, ExprKind, FrameBound, FrameExclusion, FrameUnits, OpExpr, OuterRef,
    SubPlanRef,
};
use pgarbor_types::text::format::quote_identifier;
use pgarbor_types::TypeId;

use crate::context::PlannerState;
use crate::plan::{
    AggStrategy, AggregateNode, AppendNode, CteBodyPlan, ConflictPlanAction, DistinctStrategy, FrameSpec,
    HashJoinNode, JoinType, MergeJoinNode, ModifyOp, NodeKind, Plan, PlanNode, Prop, PropValue, ScanDirection,
    SetOpCmd, SortKeySpec, TableRef, WindowAggNode,
};

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
pub fn node_label(node: &PlanNode, verbose: bool) -> Result<String> {
    label_of(node, verbose)
}

/// `schema.name alias` for a relation (PG `ExplainTargetRel`): schema only in VERBOSE, alias only when it
/// differs from the relation name. Every identifier is quoted with `quote_identifier`.
fn rel_display(table: &TableRef, verbose: bool) -> Result<String> {
    let mut text = String::new();
    if verbose && !table.schema.is_empty() {
        text.push_str(&quote_identifier(&table.schema)?);
        text.push('.');
    }
    text.push_str(&quote_identifier(&table.name)?);
    if table.alias != table.name {
        text.push(' ');
        text.push_str(&quote_identifier(&table.alias)?);
    }
    Ok(text)
}

/// `Nested Loop` / `Hash Left Join` / ...: the join word is absent for an inner join.
fn join_word(join_type: JoinType) -> Option<&'static str> {
    match join_type {
        JoinType::Inner => None,
        JoinType::Left => Some("Left"),
        JoinType::Right => Some("Right"),
        JoinType::Full => Some("Full"),
        JoinType::Semi => Some("Semi"),
        JoinType::Anti => Some("Anti"),
    }
}

fn label_of(node: &PlanNode, verbose: bool) -> Result<String> {
    let text = match &node.kind {
        NodeKind::Result(_) => "Result".to_string(),
        NodeKind::SeqScan(s) => format!("Seq Scan on {}", rel_display(&s.table, verbose)?),
        NodeKind::IndexScan(s) => {
            let head = match s.direction {
                ScanDirection::Forward => "Index Scan",
                ScanDirection::Backward => "Index Scan Backward",
            };
            format!("{head} using {} on {}", s.index.name, rel_display(&s.table, verbose)?)
        }
        NodeKind::VirtualScan(v) => format!("Seq Scan on {}", rel_display(&v.table, verbose)?),
        NodeKind::ValuesScan(_) => "Values Scan on \"*VALUES*\"".to_string(),
        NodeKind::FunctionScan(f) => match f.funcs.first().map(|item| &item.expr.kind) {
            Some(ExprKind::FuncCall(call)) => format!("Function Scan on {}", quote_identifier(&call.name)?),
            _ => return Err(PgError::new(SqlState::INTERNAL_ERROR, "function scan without a function call")),
        },
        NodeKind::CteScan(_) | NodeKind::WorkTableScan(_) => {
            // The CTE name lives in `Plan::ctes`, which a node label cannot reach (see contract_breaks).
            return Err(PgError::unimplemented("PLN-15", "EXPLAIN of a CTE scan (CTE name not available to the label)"));
        }
        NodeKind::Filter(f) => label_of(&f.input, verbose)?,
        NodeKind::Project(p) => label_of(&p.input, verbose)?,
        NodeKind::ProjectSet(_) => "ProjectSet".to_string(),
        NodeKind::NestedLoop(n) => match join_word(n.join_type) {
            None => "Nested Loop".to_string(),
            Some(word) => format!("Nested Loop {word} Join"),
        },
        NodeKind::HashJoin(h) => match join_word(h.join_type) {
            None => "Hash Join".to_string(),
            Some(word) => format!("Hash {word} Join"),
        },
        NodeKind::MergeJoin(m) => match join_word(m.join_type) {
            None => "Merge Join".to_string(),
            Some(word) => format!("Merge {word} Join"),
        },
        NodeKind::Aggregate(a) => match a.strategy {
            AggStrategy::Plain => "Aggregate".to_string(),
            AggStrategy::Hashed => "HashAggregate".to_string(),
            AggStrategy::Sorted => "GroupAggregate".to_string(),
        },
        NodeKind::WindowAgg(_) => "WindowAgg".to_string(),
        NodeKind::Sort(_) => "Sort".to_string(),
        NodeKind::Limit(_) => "Limit".to_string(),
        NodeKind::Distinct(d) => match d.strategy {
            DistinctStrategy::Hashed => "HashAggregate".to_string(),
            DistinctStrategy::Sorted => "Unique".to_string(),
        },
        NodeKind::SetOp(s) => match s.cmd {
            SetOpCmd::Intersect => "HashSetOp Intersect".to_string(),
            SetOpCmd::IntersectAll => "HashSetOp Intersect All".to_string(),
            SetOpCmd::Except => "HashSetOp Except".to_string(),
            SetOpCmd::ExceptAll => "HashSetOp Except All".to_string(),
        },
        NodeKind::Append(_) => "Append".to_string(),
        NodeKind::Materialize(_) => "Materialize".to_string(),
        NodeKind::LockRows(_) => "LockRows".to_string(),
        NodeKind::ModifyTable(m) => {
            let head = match m.op {
                ModifyOp::Insert => "Insert on",
                ModifyOp::Update => "Update on",
                ModifyOp::Delete => "Delete on",
                ModifyOp::Merge => "Merge on",
            };
            format!("{head} {}", rel_display(&m.target.table, verbose)?)
        }
    };
    Ok(text)
}

/// Fill `props` of every node in the tree rooted at `node` (and in sub-plans/CTEs held by `st`).
/// Runs BEFORE `setrefs::finalize` (expressions are still bind-time).
pub fn annotate_node(st: &mut PlannerState<'_>, node: &mut PlanNode) -> Result<()> {
    annotate_all(st, node)
}

// ---------------------------------------------------------------------------------------------
// Rendering of expressions for properties
// ---------------------------------------------------------------------------------------------

/// Where an expression is rendered: the query level whose range table names its columns, whether
/// column names are qualified, and the stack of correlated parameters (`OuterRef`).
#[derive(Clone, Copy)]
struct Scope<'x> {
    level: u32,
    qualify: bool,
    outer: &'x [Vec<String>],
}

/// `PlanColumnNamer` plus the forms it cannot render: whole-row (`t.*`) and system columns (`t.ctid`).
struct PropNamer {
    base: PlanColumnNamer,
}

impl ColumnNamer for PropNamer {
    fn column(&self, col: &ColumnRef) -> Option<String> {
        if col.levels_up != 0 || col.attnum > 0 {
            return self.base.column(col);
        }
        let (alias, _) = self.base.ranges.get((col.rt_index as usize).checked_sub(1)?)?;
        let alias = quote_identifier(alias).ok()?;
        if col.attnum == 0 {
            return Some(format!("{alias}.*"));
        }
        let system = match col.attnum {
            -1 => "ctid",
            -2 => "xmin",
            -3 => "cmin",
            -4 => "xmax",
            -5 => "cmax",
            -6 => "tableoid",
            _ => return None,
        };
        Some(format!("{alias}.{system}"))
    }
    fn input_ref(&self, index: u32) -> Option<String> {
        self.base.input_ref(index)
    }
    fn outer_ref(&self, outer: &OuterRef) -> Option<String> {
        self.base.outer_ref(outer)
    }
    fn subplan(&self, plan_id: u32) -> Option<String> {
        self.base.subplan(plan_id)
    }
}

/// Whether `e` contains a window function call.
fn has_window_func(e: &BoundExpr) -> bool {
    let mut found = false;
    e.walk(&mut |x: &BoundExpr| -> bool {
        if matches!(x.kind, ExprKind::WindowFunc(_)) {
            found = true;
        }
        !found
    });
    found
}

/// Deparse one bind-time expression for a property.
fn render_one(st: &PlannerState<'_>, e: &BoundExpr, sc: Scope<'_>) -> Result<String> {
    let ranges = st.range_names.get(sc.level as usize).cloned().unwrap_or_default();
    let namer = PropNamer { base: PlanColumnNamer { ranges, qualify: sc.qualify, outer_params: sc.outer.to_vec() } };
    let ctx = DeparseCtx {
        env: st.ctx.env,
        catalog: st.ctx.env,
        namer: &namer,
        opts: DeparseOptions { explain: true, ..DeparseOptions::default() },
    };
    deparse_expr(&ctx, e)
}

fn render_all(st: &PlannerState<'_>, exprs: &[BoundExpr], sc: Scope<'_>) -> Result<Vec<String>> {
    let mut out = Vec::with_capacity(exprs.len());
    for e in exprs {
        out.push(render_one(st, e, sc)?);
    }
    Ok(out)
}

/// A sort or group key (PG `show_sort_group_keys`): a plain column stays bare; any other expression is
/// parenthesized, because PG reaches it through a child output that is not a `Var`.
fn key_text(st: &PlannerState<'_>, e: &BoundExpr, sc: Scope<'_>) -> Result<String> {
    let text = render_one(st, e, sc)?;
    Ok(match &e.kind {
        ExprKind::Column(_) | ExprKind::InputRef(_) | ExprKind::Const(_) => text,
        _ => format!("({text})"),
    })
}

/// `expr [DESC] [USING op] [NULLS FIRST|LAST]`, non-default parts only (PG `show_sortorder_options`).
fn sort_key_text(st: &PlannerState<'_>, k: &SortKeySpec, sc: Scope<'_>) -> Result<String> {
    let mut text = key_text(st, &k.expr, sc)?;
    let op_name: &str = match &k.using_op {
        Some(op) => op.name.as_str(),
        None => {
            if k.descending {
                ">"
            } else {
                "<"
            }
        }
    };
    let reverse = op_name == ">";
    if reverse {
        text.push_str(" DESC");
    } else if op_name != "<" {
        text.push_str(" USING ");
        text.push_str(op_name);
    }
    if k.nulls_first && !reverse {
        text.push_str(" NULLS FIRST");
    } else if !k.nulls_first && reverse {
        text.push_str(" NULLS LAST");
    }
    Ok(text)
}

fn frame_bound_text(st: &PlannerState<'_>, b: &FrameBound, sc: Scope<'_>) -> Result<String> {
    Ok(match b {
        FrameBound::UnboundedPreceding => "UNBOUNDED PRECEDING".to_string(),
        FrameBound::UnboundedFollowing => "UNBOUNDED FOLLOWING".to_string(),
        FrameBound::CurrentRow => "CURRENT ROW".to_string(),
        FrameBound::Preceding(e) => format!("{} PRECEDING", render_one(st, e, sc)?),
        FrameBound::Following(e) => format!("{} FOLLOWING", render_one(st, e, sc)?),
    })
}

/// Non-default frame clause (PG `get_window_frame_options`). The short form is used when the end is
/// CURRENT ROW; FrameSpec does not record an explicit BETWEEN, so `BETWEEN` is printed otherwise.
fn frame_text(st: &PlannerState<'_>, f: &FrameSpec, sc: Scope<'_>) -> Result<String> {
    let units = match f.units {
        FrameUnits::Rows => "ROWS",
        FrameUnits::Range => "RANGE",
        FrameUnits::Groups => "GROUPS",
    };
    let start = frame_bound_text(st, &f.start, sc)?;
    let mut text = if matches!(f.end, FrameBound::CurrentRow) {
        format!("{units} {start}")
    } else {
        format!("{units} BETWEEN {start} AND {}", frame_bound_text(st, &f.end, sc)?)
    };
    match f.exclusion {
        FrameExclusion::NoOthers => {}
        FrameExclusion::CurrentRow => text.push_str(" EXCLUDE CURRENT ROW"),
        FrameExclusion::Group => text.push_str(" EXCLUDE GROUP"),
        FrameExclusion::Ties => text.push_str(" EXCLUDE TIES"),
    }
    Ok(text)
}

/// `w1 AS (PARTITION BY b ORDER BY a ROWS UNBOUNDED PRECEDING)` (PG `show_window_def`). Window keys carry
/// no direction or NULLS information.
fn window_text(st: &PlannerState<'_>, w: &WindowAggNode, sc: Scope<'_>) -> Result<String> {
    let mut parts: Vec<String> = Vec::new();
    if !w.partition_by.is_empty() {
        let items = render_all(st, &w.partition_by, sc)?;
        parts.push(format!("PARTITION BY {}", items.join(", ")));
    }
    if !w.order_by.is_empty() {
        let mut items = Vec::with_capacity(w.order_by.len());
        for k in &w.order_by {
            items.push(render_one(st, &k.expr, sc)?);
        }
        parts.push(format!("ORDER BY {}", items.join(", ")));
    }
    let f = &w.frame;
    let default_frame = matches!(f.units, FrameUnits::Range)
        && matches!(f.start, FrameBound::UnboundedPreceding)
        && matches!(f.end, FrameBound::CurrentRow)
        && matches!(f.exclusion, FrameExclusion::NoOthers);
    if !default_frame {
        parts.push(frame_text(st, f, sc)?);
    }
    Ok(format!("{} AS ({})", quote_identifier(&w.window_name)?, parts.join(" ")))
}

/// `(l op r)` for each key pair; one pair stays bare, several are AND-ed (PG `Hash Cond`).
fn hash_cond_text(st: &PlannerState<'_>, h: &HashJoinNode, sc: Scope<'_>) -> Result<Option<String>> {
    let mut conds: Vec<BoundExpr> = Vec::new();
    for ((l, r), op) in h.outer_keys.iter().zip(h.inner_keys.iter()).zip(h.eq_ops.iter()) {
        let args = vec![l.clone(), r.clone()];
        conds.push(BoundExpr::new(ExprKind::Op(Box::new(OpExpr { op: op.clone(), args })), TypeId::BOOL));
    }
    if conds.is_empty() {
        return Ok(None);
    }
    Ok(Some(render_one(st, &BoundExpr::and_all(conds), sc)?))
}

/// `(l = r)` for each merge key pair (MergeKey has no operator reference, so the text is built here).
fn merge_cond_text(st: &PlannerState<'_>, m: &MergeJoinNode, sc: Scope<'_>) -> Result<Option<String>> {
    let mut parts: Vec<String> = Vec::new();
    for (l, r) in m.left_keys.iter().zip(m.right_keys.iter()) {
        let left = render_one(st, &l.expr, sc)?;
        let right = render_one(st, &r.expr, sc)?;
        parts.push(format!("({left} = {right})"));
    }
    Ok(match parts.len() {
        0 => None,
        1 => parts.pop(),
        _ => Some(format!("({})", parts.join(" AND "))),
    })
}

/// Group keys of an Aggregate (PG `show_agg_keys`). Grouping sets print one property per set:
/// `Hash Key` when hashed, `Group Key` otherwise; the empty set prints `()`.
fn agg_key_props(st: &PlannerState<'_>, props: &mut Vec<Prop>, a: &AggregateNode, sc: Scope<'_>) -> Result<()> {
    if let Some(sets) = &a.grouping_sets {
        let key = match a.strategy {
            AggStrategy::Hashed => "Hash Key",
            _ => "Group Key",
        };
        for set in sets {
            if set.is_empty() {
                props.push(prop_text(key, "()".to_string()));
                continue;
            }
            let mut items: Vec<String> = Vec::with_capacity(set.len());
            for idx in set {
                let e = match a.group_keys.get(*idx) {
                    Some(e) => e,
                    None => return Err(PgError::new(SqlState::INTERNAL_ERROR, "grouping set refers to a missing key")),
                };
                items.push(render_one(st, e, sc)?);
            }
            props.push(prop_list(key, items));
        }
    } else if !a.group_keys.is_empty() {
        let mut items: Vec<String> = Vec::with_capacity(a.group_keys.len());
        for e in &a.group_keys {
            items.push(key_text(st, e, sc)?);
        }
        props.push(prop_list("Group Key", items));
    }
    Ok(())
}

fn prop_text(key: &str, text: String) -> Prop {
    Prop { key: key.to_string(), value: PropValue::Text(text) }
}

fn prop_list(key: &str, items: Vec<String>) -> Prop {
    Prop { key: key.to_string(), value: PropValue::List(items) }
}

/// A predicate is constant when it references no column, aggregate, window, sub-plan or parameter of a
/// row (PG puts such quals on a Result as `One-Time Filter`).
fn is_constant(e: &BoundExpr) -> bool {
    !e.any(&mut |x: &BoundExpr| {
        matches!(
            x.kind,
            ExprKind::Column(_)
                | ExprKind::InputRef(_)
                | ExprKind::OuterRef(_)
                | ExprKind::Aggref(_)
                | ExprKind::WindowFunc(_)
                | ExprKind::SubPlan(_)
                | ExprKind::SubLink(_)
                | ExprKind::Grouping(_)
        )
    })
}

/// Appends one `key: value` property whose value is `own AND pending...` (nothing when both are empty).
fn push_filter(
    st: &PlannerState<'_>,
    props: &mut Vec<Prop>,
    key: &str,
    own: Option<&BoundExpr>,
    pending: &[BoundExpr],
    sc: Scope<'_>,
) -> Result<()> {
    let mut all: Vec<BoundExpr> = Vec::new();
    if let Some(e) = own {
        all.push(e.clone());
    }
    all.extend(pending.iter().cloned());
    if all.is_empty() {
        return Ok(());
    }
    let text = render_one(st, &BoundExpr::and_all(all), sc)?;
    props.push(prop_text(key, text));
    Ok(())
}

/// True when the query's range tables hold more than one relation entry across all levels.
fn multi_relation(st: &PlannerState<'_>) -> bool {
    st.range_names.iter().map(|names| names.len()).sum::<usize>() > 1
}

/// Every expression a node owns (not its children's), used to find the sub-plans it references.
fn owned_subplan_refs(node: &PlanNode) -> Vec<SubPlanRef> {
    let mut exprs: Vec<&BoundExpr> = Vec::new();
    match &node.kind {
        NodeKind::Result(r) => {
            exprs.extend(r.exprs.iter());
            exprs.extend(r.one_time_filter.iter());
        }
        NodeKind::SeqScan(s) => exprs.extend(s.filter.iter()),
        NodeKind::IndexScan(s) => {
            exprs.extend(s.index_cond.iter());
            exprs.extend(s.filter.iter());
        }
        NodeKind::VirtualScan(v) => {
            exprs.extend(v.eq_filters.iter().map(|(_, e)| e));
            exprs.extend(v.filter.iter());
        }
        NodeKind::ValuesScan(v) => {
            for row in &v.rows {
                exprs.extend(row.iter());
            }
            exprs.extend(v.filter.iter());
        }
        NodeKind::FunctionScan(f) => {
            exprs.extend(f.funcs.iter().map(|item| &item.expr));
            exprs.extend(f.filter.iter());
        }
        NodeKind::CteScan(c) => exprs.extend(c.filter.iter()),
        NodeKind::WorkTableScan(w) => exprs.extend(w.filter.iter()),
        NodeKind::Filter(f) => exprs.push(&f.predicate),
        NodeKind::Project(p) => exprs.extend(p.exprs.iter()),
        NodeKind::ProjectSet(p) => exprs.extend(p.exprs.iter()),
        NodeKind::NestedLoop(n) => {
            exprs.extend(n.qual.iter());
            exprs.extend(n.inner_params.iter());
        }
        NodeKind::HashJoin(h) => {
            exprs.extend(h.outer_keys.iter());
            exprs.extend(h.inner_keys.iter());
            exprs.extend(h.qual.iter());
        }
        NodeKind::MergeJoin(m) => {
            exprs.extend(m.left_keys.iter().map(|k| &k.expr));
            exprs.extend(m.right_keys.iter().map(|k| &k.expr));
            exprs.extend(m.qual.iter());
        }
        NodeKind::Aggregate(a) => {
            exprs.extend(a.group_keys.iter());
            exprs.extend(a.having.iter());
            for call in &a.aggs {
                exprs.extend(call.args.iter());
                exprs.extend(call.direct_args.iter());
                exprs.extend(call.filter.iter());
                exprs.extend(call.order_by.iter().map(|k| &k.expr));
            }
        }
        NodeKind::WindowAgg(w) => {
            exprs.extend(w.partition_by.iter());
            exprs.extend(w.order_by.iter().map(|k| &k.expr));
            for call in &w.funcs {
                exprs.extend(call.args.iter());
                exprs.extend(call.filter.iter());
            }
        }
        NodeKind::Sort(s) => exprs.extend(s.keys.iter().map(|k| &k.expr)),
        NodeKind::Limit(l) => {
            exprs.extend(l.count.iter());
            exprs.extend(l.offset.iter());
            exprs.extend(l.ties_keys.iter().map(|k| &k.expr));
        }
        NodeKind::ModifyTable(m) => {
            for (_, _, e) in &m.target.checks {
                exprs.push(e);
            }
            if let Some(ins) = &m.insert {
                for (_, e) in &ins.defaults {
                    exprs.push(e);
                }
                for (_, e) in &ins.generated {
                    exprs.push(e);
                }
                if let Some(cs) = &ins.on_conflict {
                    if let ConflictPlanAction::DoUpdate { assignments, where_ } = &cs.action {
                        for (_, e) in assignments {
                            exprs.push(e);
                        }
                        exprs.extend(where_.iter());
                    }
                }
            }
            if let Some(upd) = &m.update {
                for (_, e) in &upd.assignments {
                    exprs.push(e);
                }
                for (_, e) in &upd.generated {
                    exprs.push(e);
                }
            }
            exprs.extend(m.returning.iter());
        }
        NodeKind::Distinct(_)
        | NodeKind::SetOp(_)
        | NodeKind::Append(_)
        | NodeKind::Materialize(_)
        | NodeKind::LockRows(_) => {}
    }
    let mut refs: Vec<SubPlanRef> = Vec::new();
    for e in exprs {
        e.walk(&mut |x: &BoundExpr| {
            if let ExprKind::SubPlan(sp) = &x.kind {
                refs.push((**sp).clone());
            }
            true
        });
    }
    refs
}

// ---------------------------------------------------------------------------------------------
// Annotation walk
// ---------------------------------------------------------------------------------------------

/// What a shown node inherits from the non-shown nodes above it: predicates of `Filter` nodes (with their
/// level) and the output expressions of the nearest `Project` (with the level of the node they are
/// evaluated in).
#[derive(Default)]
struct Pending {
    filters: Vec<(BoundExpr, u32)>,
    output: Option<(Vec<BoundExpr>, u32)>,
}

/// Annotate the whole plan: the root, the sub-plans it references, every CTE body, and any sub-plan no
/// node references. Each sub-plan is annotated once, with the parameter values of its first reference.
fn annotate_all(st: &mut PlannerState<'_>, root: &mut PlanNode) -> Result<()> {
    let n = st.subplans.len();
    let mut visited = vec![false; n];
    let mut outer: Vec<Vec<String>> = Vec::new();
    ann(st, root, Pending::default(), &mut outer, &mut visited)?;
    for i in 0..st.ctes.len() {
        ann_cte_body(st, i, &mut outer, &mut visited)?;
    }
    for i in 0..n {
        if !visited.get(i).copied().unwrap_or(true) {
            ann_subplan_body(st, i, None, &mut outer, &mut visited)?;
        }
    }
    Ok(())
}

/// Annotate one sub-plan body. `args` are the rendered correlation values pushed for its `OuterRef`s.
fn ann_subplan_body(
    st: &mut PlannerState<'_>,
    idx: usize,
    args: Option<Vec<String>>,
    outer: &mut Vec<Vec<String>>,
    visited: &mut [bool],
) -> Result<()> {
    if let Some(v) = visited.get_mut(idx) {
        *v = true;
    }
    let placeholder = PlanNode::new(NodeKind::Append(AppendNode { children: Vec::new() }), Vec::new());
    // The body is swapped out while it is annotated so that nested references can reach the vector.
    let mut plan = match st.subplans.get_mut(idx) {
        Some(def) => std::mem::replace(&mut def.plan, placeholder),
        None => return Err(PgError::new(SqlState::INTERNAL_ERROR, "sub-plan reference out of range")),
    };
    let pushed = args.is_some();
    if let Some(a) = args {
        outer.push(a);
    }
    let res = ann(st, &mut plan, Pending::default(), outer, visited);
    if pushed {
        outer.pop();
    }
    if let Some(def) = st.subplans.get_mut(idx) {
        def.plan = plan;
    }
    res
}

/// Annotate a sub-plan referenced by a node's expression, rendering its correlation arguments first.
fn ann_owned_subplan(
    st: &mut PlannerState<'_>,
    sp: &SubPlanRef,
    level: u32,
    outer: &mut Vec<Vec<String>>,
    visited: &mut [bool],
) -> Result<()> {
    let idx = sp.plan_id as usize;
    let seen = match visited.get(idx) {
        Some(v) => *v,
        None => return Err(PgError::new(SqlState::INTERNAL_ERROR, "sub-plan reference out of range")),
    };
    if seen {
        return Ok(());
    }
    let args = render_all(st, &sp.args, Scope { level, qualify: true, outer: outer.as_slice() })?;
    ann_subplan_body(st, idx, Some(args), outer, visited)
}

/// Annotate CTE body `i` (its nodes only; the CTE is swapped out while it is walked).
fn ann_cte_body(st: &mut PlannerState<'_>, i: usize, outer: &mut Vec<Vec<String>>, visited: &mut [bool]) -> Result<()> {
    let placeholder = CteBodyPlan::Query(PlanNode::new(NodeKind::Append(AppendNode { children: Vec::new() }), Vec::new()));
    let mut body = match st.ctes.get_mut(i) {
        Some(cte) => std::mem::replace(&mut cte.body, placeholder),
        None => return Ok(()),
    };
    let res = match &mut body {
        CteBodyPlan::Query(p) | CteBodyPlan::Modify(p) => ann(st, p, Pending::default(), outer, visited),
        CteBodyPlan::Recursive { non_recursive, recursive, .. } => {
            match ann(st, non_recursive, Pending::default(), outer, visited) {
                Ok(()) => ann(st, recursive, Pending::default(), outer, visited),
                Err(e) => Err(e),
            }
        }
    };
    if let Some(cte) = st.ctes.get_mut(i) {
        cte.body = body;
    }
    res
}

/// Annotate `node` and everything below it. `Filter` and `Project` nodes are not shown: their predicate
/// and output are passed down to the shown node they sit on.
fn ann(
    st: &mut PlannerState<'_>,
    node: &mut PlanNode,
    pend: Pending,
    outer: &mut Vec<Vec<String>>,
    visited: &mut [bool],
) -> Result<()> {
    let level = node.level;
    for sp in owned_subplan_refs(node) {
        ann_owned_subplan(st, &sp, level, outer, visited)?;
    }
    if let NodeKind::Filter(f) = &mut node.kind {
        node.props.clear();
        let mut inner = pend;
        inner.filters.push((f.predicate.clone(), level));
        return ann(st, &mut f.input, inner, outer, visited);
    }
    if let NodeKind::Project(p) = &mut node.kind {
        node.props.clear();
        let mut inner = pend;
        if inner.output.is_none() {
            // `plan_project` turns an expression the child already computes into `InputRef(slot)`;
            // show the child's bind-time term for it, as PG shows the Var of the child target list.
            let exprs: Vec<BoundExpr> = p
                .exprs
                .iter()
                .map(|e| match &e.kind {
                    ExprKind::InputRef(i) => p.input.vars.get(*i as usize).cloned().unwrap_or_else(|| e.clone()),
                    _ => e.clone(),
                })
                .collect();
            inner.output = Some((exprs, p.input.level));
        }
        return ann(st, &mut p.input, inner, outer, visited);
    }
    ann_shown(st, node, pend, outer, visited)
}

/// Annotate a shown node: its properties, then its children (the inner side of a nested loop runs with
/// the rendered `inner_params` pushed).
fn ann_shown(
    st: &mut PlannerState<'_>,
    node: &mut PlanNode,
    pend: Pending,
    outer: &mut Vec<Vec<String>>,
    visited: &mut [bool],
) -> Result<()> {
    let props = node_props(st, node, &pend, outer.as_slice())?;
    node.props = props;
    let nl_params: Option<Vec<String>> = match &node.kind {
        NodeKind::NestedLoop(n) => {
            Some(render_all(st, &n.inner_params, Scope { level: node.level, qualify: true, outer: outer.as_slice() })?)
        }
        _ => None,
    };
    for (i, child) in node.children_mut().into_iter().enumerate() {
        match (&nl_params, i) {
            (Some(params), 1) => {
                outer.push(params.clone());
                let res = ann(st, child, Pending::default(), outer, visited);
                outer.pop();
                res?;
            }
            _ => ann(st, child, Pending::default(), outer, visited)?,
        }
    }
    Ok(())
}

/// The properties of a shown node, in PG `ExplainNode` order: `Output` (VERBOSE), then the node's own
/// properties, then the `Filter` (or `One-Time Filter`) that comes from the Filter nodes above it.
fn node_props(st: &PlannerState<'_>, node: &PlanNode, pend: &Pending, outer: &[Vec<String>]) -> Result<Vec<Prop>> {
    let verbose = st.ctx.options.verbose;
    let up = verbose || multi_relation(st);
    let level = node.level;
    let sc_scan = Scope { level, qualify: verbose, outer };
    let sc_up = Scope { level, qualify: up, outer };
    let mut props: Vec<Prop> = Vec::new();
    if verbose {
        let src: Option<(&[BoundExpr], u32)> = match &pend.output {
            Some((exprs, lvl)) => Some((exprs.as_slice(), *lvl)),
            None => match &node.kind {
                NodeKind::Result(r) => Some((r.exprs.as_slice(), level)),
                NodeKind::ProjectSet(p) => Some((p.exprs.as_slice(), level)),
                _ if node.vars.is_empty() => None,
                _ => Some((node.vars.as_slice(), level)),
            },
        };
        if let Some((exprs, lvl)) = src {
            // PG prints window calls as `f(..) OVER w` (and parenthesized above their WindowAgg);
            // the window-name table that needs is not wired into the namer yet.
            if matches!(node.kind, NodeKind::WindowAgg(_)) || exprs.iter().any(has_window_func) {
                return Err(PgError::unimplemented("PLN-15", "EXPLAIN VERBOSE output of window functions"));
            }
            // PG `show_plan_tlist`: Vars are prefixed only when the range table has several entries.
            let items = render_all(st, exprs, Scope { level: lvl, qualify: multi_relation(st), outer })?;
            if !items.is_empty() {
                props.push(prop_list("Output", items));
            }
        }
    }
    let pend_all: Vec<BoundExpr> = pend.filters.iter().map(|(e, _)| e.clone()).collect();
    let pend_const: Vec<BoundExpr> = pend_all.iter().filter(|e| is_constant(e)).cloned().collect();
    let pend_var: Vec<BoundExpr> = pend_all.iter().filter(|e| !is_constant(e)).cloned().collect();
    match &node.kind {
        NodeKind::Result(r) => {
            let mut once: Vec<BoundExpr> = Vec::new();
            if let Some(f) = &r.one_time_filter {
                once.push(f.clone());
            }
            once.extend(pend_const.iter().cloned());
            if !once.is_empty() {
                let text = render_one(st, &BoundExpr::and_all(once), sc_up)?;
                props.push(prop_text("One-Time Filter", text));
            }
            push_filter(st, &mut props, "Filter", None, &pend_var, sc_up)?;
        }
        NodeKind::SeqScan(s) => push_filter(st, &mut props, "Filter", s.filter.as_ref(), &pend_all, sc_scan)?,
        NodeKind::IndexScan(s) => {
            if !s.index_cond.is_empty() {
                let text = render_one(st, &BoundExpr::and_all(s.index_cond.clone()), sc_scan)?;
                props.push(prop_text("Index Cond", text));
            }
            push_filter(st, &mut props, "Filter", s.filter.as_ref(), &pend_all, sc_scan)?;
        }
        NodeKind::VirtualScan(v) => push_filter(st, &mut props, "Filter", v.filter.as_ref(), &pend_all, sc_scan)?,
        NodeKind::ValuesScan(v) => push_filter(st, &mut props, "Filter", v.filter.as_ref(), &pend_all, sc_scan)?,
        NodeKind::FunctionScan(f) => push_filter(st, &mut props, "Filter", f.filter.as_ref(), &pend_all, sc_scan)?,
        NodeKind::CteScan(c) => push_filter(st, &mut props, "Filter", c.filter.as_ref(), &pend_all, sc_scan)?,
        NodeKind::WorkTableScan(w) => push_filter(st, &mut props, "Filter", w.filter.as_ref(), &pend_all, sc_scan)?,
        NodeKind::NestedLoop(n) => {
            if let Some(q) = &n.qual {
                props.push(prop_text("Join Filter", render_one(st, q, sc_up)?));
            }
            push_filter(st, &mut props, "Filter", None, &pend_all, sc_up)?;
        }
        NodeKind::HashJoin(h) => {
            if let Some(cond) = hash_cond_text(st, h, sc_up)? {
                props.push(prop_text("Hash Cond", cond));
            }
            if let Some(q) = &h.qual {
                props.push(prop_text("Join Filter", render_one(st, q, sc_up)?));
            }
            push_filter(st, &mut props, "Filter", None, &pend_all, sc_up)?;
        }
        NodeKind::MergeJoin(m) => {
            if let Some(cond) = merge_cond_text(st, m, sc_up)? {
                props.push(prop_text("Merge Cond", cond));
            }
            if let Some(q) = &m.qual {
                props.push(prop_text("Join Filter", render_one(st, q, sc_up)?));
            }
            push_filter(st, &mut props, "Filter", None, &pend_all, sc_up)?;
        }
        NodeKind::Aggregate(a) => {
            agg_key_props(st, &mut props, a, sc_up)?;
            push_filter(st, &mut props, "Filter", a.having.as_ref(), &pend_all, sc_up)?;
        }
        NodeKind::WindowAgg(w) => {
            props.push(prop_text("Window", window_text(st, w, sc_up)?));
            push_filter(st, &mut props, "Filter", None, &pend_all, sc_up)?;
        }
        NodeKind::Sort(s) => {
            if !s.keys.is_empty() {
                let mut items: Vec<String> = Vec::with_capacity(s.keys.len());
                for k in &s.keys {
                    items.push(sort_key_text(st, k, sc_up)?);
                }
                props.push(prop_list("Sort Key", items));
            }
            push_filter(st, &mut props, "Filter", None, &pend_all, sc_up)?;
        }
        NodeKind::Distinct(d) => {
            // Hashed: the keys are the grouping keys. Sorted (`Unique`): the keys appear only in VERBOSE.
            if matches!(d.strategy, DistinctStrategy::Hashed) || verbose {
                let mut items: Vec<String> = Vec::with_capacity(d.key_slots.len());
                for slot in &d.key_slots {
                    let e = match d.input.vars.get(*slot as usize) {
                        Some(e) => e,
                        None => return Err(PgError::new(SqlState::INTERNAL_ERROR, "could not deparse variable")),
                    };
                    items.push(key_text(st, e, Scope { level: d.input.level, qualify: up, outer })?);
                }
                if !items.is_empty() {
                    props.push(prop_list("Group Key", items));
                }
            }
            push_filter(st, &mut props, "Filter", None, &pend_all, sc_up)?;
        }
        NodeKind::ModifyTable(m) => {
            if let Some(ins) = &m.insert {
                if let Some(cs) = &ins.on_conflict {
                    let resolution = match &cs.action {
                        ConflictPlanAction::DoNothing => "NOTHING",
                        ConflictPlanAction::DoUpdate { .. } => "UPDATE",
                    };
                    props.push(prop_text("Conflict Resolution", resolution.to_string()));
                    if !cs.arbiter_indexes.is_empty() {
                        let mut names: Vec<String> = Vec::with_capacity(cs.arbiter_indexes.len());
                        for oid in &cs.arbiter_indexes {
                            match st.ctx.env.snapshot().index(*oid) {
                                Some(index) => names.push(index.name.clone()),
                                None => {
                                    return Err(PgError::new(SqlState::INTERNAL_ERROR, "arbiter index not found"));
                                }
                            }
                        }
                        props.push(prop_list("Conflict Arbiter Indexes", names));
                    }
                    if let ConflictPlanAction::DoUpdate { where_: Some(w), .. } = &cs.action {
                        props.push(prop_text("Conflict Filter", render_one(st, w, sc_up)?));
                    }
                }
            }
            push_filter(st, &mut props, "Filter", None, &pend_all, sc_up)?;
        }
        _ => push_filter(st, &mut props, "Filter", None, &pend_all, sc_up)?,
    }
    Ok(props)
}

/// Annotate a whole finished (but not yet finalized) plan: root, every `SubPlanDef::plan`, every CTE body.
pub fn annotate_plan(st: &mut PlannerState<'_>, root: &mut PlanNode) -> Result<()> {
    annotate_all(st, root)
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::context::{PlanContext, PlanOptions};
    use crate::plan::{
        CteScan, DistinctNode, FilterNode, HashJoinNode, IndexBounds, IndexRef, IndexScan, NestedLoopNode,
        ResultNode, ScanDirection, SeqScan, SetOpCmd, SetOpNode, ValuesScan,
    };
    use pgarbor_types::TypeId;
    use std::collections::HashMap;

    fn tref(name: &str, alias: &str) -> TableRef {
        TableRef {
            oid: 16384,
            schema: "public".to_string(),
            name: name.to_string(),
            alias: alias.to_string(),
            ncols: 1,
            col_types: vec![TypeId::BOOL],
        }
    }

    fn seq(name: &str, alias: &str) -> PlanNode {
        let scan = SeqScan { table: tref(name, alias), filter: None, with_rowid: false, needed: vec![true] };
        PlanNode::new(NodeKind::SeqScan(scan), Vec::new())
    }

    fn index_scan(direction: ScanDirection) -> PlanNode {
        let index = IndexRef {
            oid: 16385,
            name: "t_pkey".to_string(),
            unique: true,
            primary: true,
            nulls_not_distinct: false,
            keys: Vec::new(),
        };
        let scan = IndexScan {
            table: tref("t", "t"),
            index,
            bounds: IndexBounds::default(),
            index_cond: Vec::new(),
            recheck_index_cond: false,
            filter: None,
            direction,
            with_rowid: false,
            needed: vec![true],
        };
        PlanNode::new(NodeKind::IndexScan(scan), Vec::new())
    }

    fn hash_join(join_type: JoinType) -> PlanNode {
        let join = HashJoinNode {
            join_type,
            outer: seq("u", "u"),
            inner: seq("t", "t"),
            outer_keys: Vec::new(),
            inner_keys: Vec::new(),
            eq_ops: Vec::new(),
            qual: None,
        };
        PlanNode::new(NodeKind::HashJoin(Box::new(join)), Vec::new())
    }

    fn nested_loop(join_type: JoinType) -> PlanNode {
        let join = NestedLoopNode {
            join_type,
            outer: seq("u", "u"),
            inner: seq("t", "t"),
            qual: None,
            inner_params: Vec::new(),
        };
        PlanNode::new(NodeKind::NestedLoop(Box::new(join)), Vec::new())
    }

    fn aggregate(strategy: AggStrategy) -> PlanNode {
        let agg = AggregateNode {
            strategy,
            input: seq("t", "t"),
            group_keys: Vec::new(),
            aggs: Vec::new(),
            grouping_sets: None,
            having: None,
        };
        PlanNode::new(NodeKind::Aggregate(Box::new(agg)), Vec::new())
    }

    fn distinct(strategy: DistinctStrategy) -> PlanNode {
        let node = DistinctNode { strategy, input: seq("t", "t"), key_slots: vec![0] };
        PlanNode::new(NodeKind::Distinct(Box::new(node)), Vec::new())
    }

    #[test]
    fn scan_labels_follow_pg_format() {
        assert_eq!(node_label(&seq("t", "t"), false).unwrap(), "Seq Scan on t");
        assert_eq!(node_label(&seq("t", "x"), false).unwrap(), "Seq Scan on t x");
        assert_eq!(node_label(&seq("t", "t"), true).unwrap(), "Seq Scan on public.t");
        assert_eq!(node_label(&seq("Weird Name", "Weird Name"), false).unwrap(), "Seq Scan on \"Weird Name\"");
        let values = PlanNode::new(NodeKind::ValuesScan(ValuesScan { rows: Vec::new(), filter: None }), Vec::new());
        assert_eq!(node_label(&values, false).unwrap(), "Values Scan on \"*VALUES*\"");
    }

    #[test]
    fn index_scan_labels_follow_direction() {
        let forward = index_scan(ScanDirection::Forward);
        assert_eq!(node_label(&forward, false).unwrap(), "Index Scan using t_pkey on t");
        let backward = index_scan(ScanDirection::Backward);
        assert_eq!(node_label(&backward, false).unwrap(), "Index Scan Backward using t_pkey on t");
    }

    #[test]
    fn join_labels_omit_inner_word() {
        assert_eq!(node_label(&hash_join(JoinType::Inner), false).unwrap(), "Hash Join");
        assert_eq!(node_label(&hash_join(JoinType::Left), false).unwrap(), "Hash Left Join");
        assert_eq!(node_label(&hash_join(JoinType::Full), false).unwrap(), "Hash Full Join");
        assert_eq!(node_label(&nested_loop(JoinType::Inner), false).unwrap(), "Nested Loop");
        assert_eq!(node_label(&nested_loop(JoinType::Left), false).unwrap(), "Nested Loop Left Join");
        assert_eq!(node_label(&nested_loop(JoinType::Anti), false).unwrap(), "Nested Loop Anti Join");
    }

    #[test]
    fn aggregate_and_distinct_labels() {
        assert_eq!(node_label(&aggregate(AggStrategy::Plain), false).unwrap(), "Aggregate");
        assert_eq!(node_label(&aggregate(AggStrategy::Hashed), false).unwrap(), "HashAggregate");
        assert_eq!(node_label(&aggregate(AggStrategy::Sorted), false).unwrap(), "GroupAggregate");
        assert_eq!(node_label(&distinct(DistinctStrategy::Hashed), false).unwrap(), "HashAggregate");
        assert_eq!(node_label(&distinct(DistinctStrategy::Sorted), false).unwrap(), "Unique");
    }

    #[test]
    fn set_op_and_result_labels() {
        let set_op = SetOpNode { cmd: SetOpCmd::IntersectAll, left: seq("t", "t"), right: seq("u", "u") };
        let node = PlanNode::new(NodeKind::SetOp(Box::new(set_op)), Vec::new());
        assert_eq!(node_label(&node, false).unwrap(), "HashSetOp Intersect All");
        let result = ResultNode { exprs: Vec::new(), one_time_filter: None };
        let node = PlanNode::new(NodeKind::Result(result), Vec::new());
        assert_eq!(node_label(&node, false).unwrap(), "Result");
    }

    #[test]
    fn filter_label_is_the_child_label() {
        let filter = FilterNode { input: seq("t", "t"), predicate: BoundExpr::bool_const(true) };
        let node = PlanNode::new(NodeKind::Filter(Box::new(filter)), Vec::new());
        assert_eq!(node_label(&node, false).unwrap(), "Seq Scan on t");
    }

    #[test]
    fn cte_scan_label_is_unimplemented_without_the_name() {
        let scan = CteScan { cte_id: 0, filter: None };
        let node = PlanNode::new(NodeKind::CteScan(scan), Vec::new());
        assert!(node_label(&node, false).is_err());
    }

    #[test]
    #[ignore = "needs SQL-43 (deparse_expr)"]
    fn annotate_constant_scan_filter() {
        let (_kv, env) = pgarbor_catalog::testing::bootstrapped().expect("bootstrap");
        let options = PlanOptions { for_explain: true, ..PlanOptions::default() };
        let ctx = PlanContext { env: &env, param_types: Vec::new(), param_values: None, options, table_rows: HashMap::new() };
        let mut st = PlannerState::new(&ctx);
        st.enter_level(vec![("t".to_string(), vec!["a".to_string()])]);
        let mut node = seq("t", "t");
        if let NodeKind::SeqScan(scan) = &mut node.kind {
            scan.filter = Some(BoundExpr::bool_const(false));
        }
        annotate_plan(&mut st, &mut node).expect("annotate");
        assert_eq!(node.props.len(), 1);
        assert_eq!(node.props[0].key, "Filter");
        assert_eq!(node.props[0].value, PropValue::Text("false".to_string()));
    }
}
