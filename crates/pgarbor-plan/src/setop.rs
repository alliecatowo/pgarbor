//! UNION / INTERSECT / EXCEPT planning (PLN-10). PG references: `optimizer/prep/prepunion.c`
//! (`plan_set_operations`, `generate_union_paths`, `generate_nonunion_paths`, `recurse_set_operations`),
//! `plan/createplan.c` (`create_append_plan`, `create_setop_plan`), `nodeSetOp.c`.
//!
//! ## Logical build (`build_setop`)
//! For `QueryBody::SetOp(SetOpBody { op, all, left, right, types })`:
//! * build both children with `build::build_query` (each child already produces the common types: the
//!   binder inserted the casts);
//! * `Union ALL` -> `LogicalSetOp { kind: UnionAll }`; nested UNION ALL of the same op are flattened
//!   by the physical planner into one `Append`;
//! * `Union` (distinct) -> `LogicalDistinct { key_slots: all, on: false }` over a `UnionAll`;
//! * `Intersect`/`Except` [ALL] -> `LogicalSetOp { kind: Setop(SetOpCmd::..) }`;
//! * `vars` of the result: `Column`-free placeholders - use the LEFT child's `vars` for `vars`
//!   (positions are what matter) with `out` = the set-op output (names of the left child, common types);
//!   `rels` empty.
//! Operator precedence/nesting is already in the bound tree.
//!
//! ## Physical
//! * `UnionAll` -> `Append(children)`; children are planned independently; type/width equality asserted.
//! * Distinct over Append -> `Distinct { Hashed }` unless the caller needs sorted output (ORDER BY on all
//!   columns) -> `Sort`+`Distinct{Sorted}` (`sort_limit::plan_distinct` decides).
//! * `Intersect`/`Except`: `SetOpNode { cmd, left, right }` (executor hashes both sides).
//! * Estimates: Append sums; INTERSECT min; EXCEPT left.
//! * EXPLAIN naming: `Append`, `HashAggregate` (union distinct), `HashSetOp Intersect`, `HashSetOp
//!   Except All`.
use pgarbor_common::{PgError, Result};
use pgarbor_sql::ir::SetOpBody;

use crate::context::PlannerState;
use crate::logical::{LogicalPlan, LogicalSetOp};
use crate::plan::PlanNode;

/// Logical plan of a set-operation body.
#[allow(unused_variables)]
pub fn build_setop(st: &mut PlannerState, body: &SetOpBody) -> Result<LogicalPlan> {
    Err(PgError::unimplemented("PLN-10", "set operation logical plan (setop::build_setop)"))
}

/// Physical plan of a `LogicalSetOp`.
#[allow(unused_variables)]
pub fn plan_setop(st: &mut PlannerState, s: &LogicalSetOp) -> Result<PlanNode> {
    Err(PgError::unimplemented("PLN-10", "set operation planning (setop::plan_setop)"))
}
