//! Predicate pushdown and outer-join reduction (PLN-03). PG references:
//! `optimizer/plan/initsplan.c` (`distribute_qual_to_rels`, `check_outerjoin_delay`,
//! `deconstruct_recurse`), `optimizer/prep/prepjointree.c` (`reduce_outer_joins`,
//! `find_nonnullable_rels`, `find_forced_null_vars`), `optimizer/path/equivclass.c` (equivalence classes,
//! simplified here to constant propagation), `util/clauses.c` (`contain_nonstrict_functions`).
//!
//! ## `push_down_quals(st, plan, quals)`
//! Distributes AND-ed conjuncts (bind-time expressions) into `plan` (a join tree of scans):
//! * `expr_relids(q)` = the set of `rt_index`es of `Column{levels_up: 0}` nodes in `q` (columns of
//!   enclosing queries and params do not count). A qual with no relids (constant) stays at the top.
//! * A qual may be evaluated at a node whose `rels` is a SUPERSET of its relids, as deep as legal:
//!   - leaf scan: becomes one of `LogicalScan::quals`;
//!   - INNER join: if it references both sides it is a join qual (`LogicalJoin::quals`), else pushed into
//!     the side it references;
//!   - LEFT join: a WHERE qual referencing only the preserved (left) side is pushed to the left; a
//!     qual referencing the nullable (right) side must stay ABOVE the join as a `LogicalFilter` unless
//!     the join is first reduced (below); ON quals referencing only the nullable side are pushed into
//!     the right input; ON quals referencing only the preserved side stay in the join `quals` (they do
//!     not filter preserved rows);
//!   - RIGHT mirrors LEFT; FULL: nothing crosses (all WHERE quals stay above);
//!   - never push a qual containing volatile functions below a point where it would be evaluated a
//!     different number of times (keep volatile quals at/above the lowest join covering their relids
//!     and never below an outer join's nullable side); never push below `Aggregate`/`Limit`/
//!     `Distinct`/`Window`/set-ops here (this function only walks the join tree).
//! * **Outer join reduction**: a WHERE qual that is *strict* (returns NULL/false when all its columns
//!   from the nullable side are NULL: comparison/arithmetic operators and strict functions of such
//!   columns, `x IS NOT NULL`, but NOT `COALESCE`/`IS NULL`/`CASE`/non-strict functions) on a column of
//!   the nullable side converts `LEFT`->`INNER` (`RIGHT`->`INNER`, `FULL`->`LEFT`/`RIGHT`/`INNER`),
//!   after which more pushdown is possible. Apply to fixpoint.
//! * **Constant propagation** (lightweight equivalence classes): from a conjunct set containing
//!   `a = b` and `a = <const>` derive `b = <const>` (same operand types, `=` of a btree family) and push
//!   it to `b`'s relation; join quals `t.a = u.a` plus `t.a = 5` give `u.a = 5` on `u`. Original quals are
//!   kept.
//! * Result keeps all quals exactly once except derived ones; semantics must be identical to the
//!   original query (tests: row-count equality on small fixtures).
//!
//! The helper predicates below are used by `index`, `join` and `subplan` as well.
use pgarbor_common::{PgError, Result};
use pgarbor_sql::ir::BoundExpr;

use crate::context::PlannerState;
use crate::logical::LogicalPlan;

/// Range-table indexes (sorted, unique) of the level-0 columns in `e`. Fully implemented.
pub fn expr_relids(e: &BoundExpr) -> Vec<u32> {
    let mut cols = Vec::new();
    e.collect_columns(0, &mut cols);
    let mut v: Vec<u32> = cols.into_iter().filter(|c| c.levels_up == 0).map(|c| c.rt_index).collect();
    v.sort_unstable();
    v.dedup();
    v
}

/// Does `e` call a volatile function/operator? Fully implemented (delegates to `BoundExpr::volatility`).
pub fn is_volatile(e: &BoundExpr) -> bool {
    e.volatility() == pgarbor_types::catalog_data::Volatility::Volatile
}

/// Is `e` strict with respect to the columns of relation set `rels`: guaranteed not TRUE when every
/// column of those relations is NULL (module docs)?
#[allow(unused_variables)]
pub fn is_strict_for_rels(e: &BoundExpr, rels: &[u32]) -> Result<bool> {
    Err(PgError::unimplemented("PLN-03", "strictness analysis (pushdown::is_strict_for_rels)"))
}

/// Distribute `quals` into `plan` (module docs).
#[allow(unused_variables)]
pub fn push_down_quals(st: &mut PlannerState, plan: LogicalPlan, quals: Vec<BoundExpr>) -> Result<LogicalPlan> {
    Err(PgError::unimplemented("PLN-03", "predicate pushdown (pushdown::push_down_quals)"))
}

/// Derive implied constant quals from equalities (module docs); returns the extra quals only.
#[allow(unused_variables)]
pub fn derive_constant_quals(st: &mut PlannerState, quals: &[BoundExpr]) -> Result<Vec<BoundExpr>> {
    Err(PgError::unimplemented("PLN-03", "constant propagation (pushdown::derive_constant_quals)"))
}
