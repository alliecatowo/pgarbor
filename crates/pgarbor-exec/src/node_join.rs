//! Nested loop join (lease EXE-16, north-star). PG: `nodeNestloop.c`. Layout: plan.rs `NestedLoopNode`.
//!
//! For each outer row `o`:
//! 1. evaluate `inner_params` over `o` -> a parameter vector (empty for uncorrelated inner sides);
//! 2. restart the inner side: `Executor::with_outer(Rc::new(params), |ex| inner.rescan(ex, &params))`
//!    (when `inner_params` is empty and the inner side is not yet started, a plain first scan; an
//!    unparameterized inner is rescanned with `rescan(&[])` per outer row - it is normally wrapped in a
//!    `Materialize` by the planner, which replays its buffer);
//! 3. every inner `next` call is wrapped in `with_outer(params.clone(), ...)` as well, so `OuterRef`
//!    inside the inner subtree resolves while it iterates lazily (runtime module docs);
//! 4. join semantics with `qual` evaluated over `o ++ inner_row` (`eval_opt_qual`):
//!    * `Inner`: emit `o ++ inner` for every inner row satisfying `qual`.
//!    * `Left`: like Inner, but if NO inner row satisfied `qual` emit `o ++ NULLs` (NULL per inner slot,
//!      width = `inner.width()`).
//!    * `Semi`: emit `o` (outer slots only) at the FIRST satisfying inner row, then stop the inner scan.
//!    * `Anti`: emit `o` if NO inner row satisfied `qual`.
//!    * `Right`/`Full`: not supported by nested loops (the planner swaps or uses hash/merge): XX000
//!      `nested loop does not support {type} joins`.
//! The outer side is pulled lazily one row at a time. NULL padding uses `Datum::Null`.
use pgarbor_common::{PgError, Result};
use pgarbor_plan::plan::{NestedLoopNode, PlanNode};

use crate::build::BuildCtx;
use crate::runtime::RowSource;

#[allow(unused_variables)]
pub fn build_nested_loop(node: &PlanNode, n: &NestedLoopNode, b: &mut BuildCtx) -> Result<Box<dyn RowSource>> {
    Err(PgError::unimplemented("EXE-16", "NestedLoop node (node_join::build_nested_loop)"))
}
