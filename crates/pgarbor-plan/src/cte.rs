//! CTE planning: inlining, materialization, recursion (PLN-11). PG references:
//! `optimizer/plan/subselect.c` (`SS_process_ctes`), `prepjointree.c` (`inline_cte`,
//! `contain_dml`, `contain_outer_selfref`), `createplan.c` (`create_ctescan_plan`,
//! `create_recursiveunion_plan`, `create_worktablescan_plan`), `nodeCtescan.c`,
//! `nodeRecursiveunion.c`.
//!
//! * [`register_ctes`] runs once per query level before FROM is built: pushes the level's `BoundCte`s on
//!   `st.cte_scopes` (so `RangeKind::Cte { cte_index, levels_up }` can be resolved), decides for each CTE
//!   whether to INLINE or MATERIALIZE, and for materialized ones plans the body into `st.ctes`.
//! * **Inline** (PG 12+): non-recursive, `ref_count == 1`, no data-modifying body, not volatile, and
//!   `materialized != Some(true)`; `materialized == Some(false)` also forces inlining of multi-ref CTEs
//!   (each reference re-plans the body). An inlined reference becomes `ScanSource::Subquery(build_query(body))`.
//! * **Materialize** otherwise: `CteDef { id, name, body: Query(plan), columns, materialize: true }`;
//!   each reference is `ScanSource::Cte { cte_id }`. CTEs with `ref_count == 0` and a query body are dropped.
//! * **Recursive** (`BoundCte::recursive`): body is `SetOp(Union)`; plan the non-recursive term, then the
//!   recursive term with `RangeKind::Cte { recursive_self: true }` entries planned as
//!   `ScanSource::WorkTable { cte_id }`; `CteBodyPlan::Recursive { non_recursive, recursive, union_all }`.
//! * **Data-modifying** (`CteBody::Modify`): planned through `build_dml::plan_dml_statement` into
//!   `CteBodyPlan::Modify`, `materialize = true`, executed eagerly exactly once.
//! * [`plan_cte_reference`] returns the scan source for a `RangeKind::Cte` entry (inline subquery or
//!   `Cte`/`WorkTable` scan) plus the output columns (`OutCol`s).
use pgarbor_common::{PgError, Result};
use pgarbor_sql::ir::{BoundCte, RangeKind};

use crate::context::PlannerState;
use crate::logical::ScanSource;
use crate::plan::OutCol;

/// Plan/register the CTEs of one query level (module docs).
#[allow(unused_variables)]
pub fn register_ctes(st: &mut PlannerState, ctes: &[BoundCte]) -> Result<()> {
    Err(PgError::unimplemented("PLN-11", "CTE registration (cte::register_ctes)"))
}

/// Leave the query level: pop the scope pushed by `register_ctes`.
pub fn unregister_ctes(st: &mut PlannerState) {
    st.cte_scopes.pop();
}

/// Resolve a `RangeKind::Cte` range entry to its scan source and output columns.
#[allow(unused_variables)]
pub fn plan_cte_reference(st: &mut PlannerState, kind: &RangeKind) -> Result<(ScanSource, Vec<OutCol>)> {
    Err(PgError::unimplemented("PLN-11", "CTE reference planning (cte::plan_cte_reference)"))
}
