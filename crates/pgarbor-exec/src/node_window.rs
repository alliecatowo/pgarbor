//! Window aggregation operator (lease EXE-20, sonnet). PG: `nodeWindowAgg.c`. Layout: plan.rs
//! `WindowAggNode`: input sorted by `partition_by` then `order_by`; output row = input row ++ one
//! value per `funcs`.
//!
//! ## Algorithm
//! 1. Read input rows into a partition buffer until the `partition_by` expressions (evaluated per
//!    row, compared with `compare::not_distinct`) change or the input ends. (A partition is held in
//!    memory entirely: no spilling in v1.)
//! 2. Build `window_funcs::WinPartition`: evaluate the `order_by` key values for every row (via
//!    `sortutil::eval_keys`) and compute peer groups (consecutive rows whose keys are equal under
//!    `compare::not_distinct`; all rows are peers when there is no ORDER BY).
//! 3. For each row `i`, in order, compute its frame [`WinFrame`](crate::window_funcs::WinFrame) from `frame`
//!    ([`FrameSpec`](pgarbor_plan::plan::FrameSpec); offset expressions are evaluated once per
//!    partition-row: a NULL offset -> 22013 `frame starting offset must not be null`/`frame ending offset must
//!    not be null`, negative -> 22013 `frame starting offset must not be negative` (ending likewise)):
//!    * `ROWS`: start/end are row counts relative to `i` (`UNBOUNDED PRECEDING` = 0, `UNBOUNDED
//!      FOLLOWING` = len, `CURRENT ROW` = i itself, `N PRECEDING` = i - N, `N FOLLOWING` = i + N), clamped to
//!      `[0, len]`; end is exclusive (+1).
//!    * `GROUPS`: offsets count peer GROUPS: `CURRENT ROW` start = start of i's peer group, end = end of
//!      i's peer group; `N PRECEDING/FOLLOWING` move by whole groups.
//!    * `RANGE`: `CURRENT ROW` = peer group boundaries; `UNBOUNDED` as above; `N PRECEDING/FOLLOWING`
//!      compares the single ORDER BY key value with `key_i -/+ N` using the type's `+`/`-` operators
//!      (resolve via `pgarbor_types::resolve::oper` for the key type and offset type) honoring
//!      ASC/DESC (for DESC the arithmetic direction flips); NULL keys frame only their peers.
//!      Without ORDER BY only `UNBOUNDED`/`CURRENT ROW` are legal (the binder enforces).
//!    * `EXCLUDE CURRENT ROW` removes row i; `GROUP` removes i's peer group; `TIES` removes the peers
//!      except i. An empty or inverted range gives an empty frame.
//!    * The default frame (no frame clause, `frame_is_default`) arrives as `RANGE UNBOUNDED
//!      PRECEDING .. CURRENT ROW` with ORDER BY, or the whole partition without.
//! 4. For each `WindowCall` of `funcs`:
//!    * pure window function (`window_funcs::is_pure_window_function(name)`): `window_funcs::
//!      eval_window_func(ex, call, &part, i, &frame)`;
//!    * aggregate (`is_agg`): result over the frame rows with `pgarbor_types::agg::make_aggregate`:
//!      evaluate `call.filter` and `call.args` per frame row, skip filtered rows, `transition`, then
//!      `finalize`. Optimize the common growing frame (start fixed, end moving forward) by feeding only
//!      new rows; for sliding frames use `AggState::retract` when it returns `Ok(true)`, otherwise
//!      recompute the frame from scratch. `star` aggregates (`count(*) OVER`) take no args.
//! 5. Emit `input_row ++ results`; rows are emitted in input order, one per `next` call.
//! `rescan(params)` clears buffers and rescans the input.
use pgarbor_common::{PgError, Result};
use pgarbor_plan::plan::{PlanNode, WindowAggNode};

use crate::build::BuildCtx;
use crate::runtime::RowSource;

#[allow(unused_variables)]
pub fn build_window_agg(node: &PlanNode, n: &WindowAggNode, b: &mut BuildCtx) -> Result<Box<dyn RowSource>> {
    Err(PgError::unimplemented("EXE-20", "WindowAgg node (node_window::build_window_agg)"))
}
