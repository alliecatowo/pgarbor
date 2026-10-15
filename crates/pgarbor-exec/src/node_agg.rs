//! Aggregation (lease EXE-19, north-star). PG: `nodeAgg.c`, `parse_agg.c`. Layout: plan.rs
//! `AggregateNode` / `AggCall`. Aggregate state machines: `pgarbor_types::agg` (`make_aggregate`,
//! `AggState::{transition, finalize}`; its module docs define NULL handling: the executor does NOT
//! skip NULL arguments, the aggregate does).
//!
//! ## Output
//! One row per group: `group_keys` values (in `group_keys` order) ++ one finalized value per `aggs`
//! entry ++ (only if `grouping_sets` is `Some`) the grouping mask `Int8` (bit k set = group key k is
//! NOT part of the set that produced the row; keys not in the set output NULL). `having` (executable
//! over the OUTPUT row) filters groups after finalization.
//!
//! ## Strategies
//! * `Plain`: no keys; consumes all input; ALWAYS emits exactly one row (aggregates over an empty
//!   input finalize their empty state: `count` 0, `sum` NULL, ...), subject to `having`.
//! * `Hashed`: `HashMap` from the group key bytes (`keycodec::encode_key_vec` with `KeyColumn::asc(ty)`
//!   for every group key; NULLs form one group) to group state; output in first-appearance order
//!   (deterministic). Consume all input first, then emit lazily.
//! * `Sorted`: input is sorted by the keys; consecutive rows with `compare::not_distinct` keys form a
//!   group; emit a group as soon as the next key differs (streaming). Empty input -> no rows (unless
//!   keyless).
//! * Grouping sets (`grouping_sets: Some(sets)`, each set = indexes into `group_keys`): every input
//!   row is aggregated once per set into that set's own hash table; output sets in the listed order,
//!   each set's groups in first-appearance order. The EMPTY set `()` outputs one row even when the
//!   input is empty (all keys NULL, mask all ones); a non-empty set outputs nothing for empty input.
//!
//! ## Per-group state and per-row work
//! For every `AggCall` of a group keep an `AggInstance`:
//! * created when the group is created with `agg::make_aggregate(&AggSpec{agg_oid: call.agg,
//!   arg_types: call.arg_types, result_type: call.result_type, direct_args}, &FuncCtx)`, where
//!   `direct_args` are `call.direct_args` evaluated over the group's FIRST input row;
//! * per input row: skip if `call.filter` evaluates to anything but TRUE; evaluate `call.args` over the
//!   row; `star` aggregates (`count(*)`) pass no args;
//! * `distinct`: keep a set of `keycodec` bytes of the argument tuple per group (NULL equals NULL for
//!   this purpose) and skip repeats BEFORE feeding the state;
//! * `order_by` non-empty (ordered-set `WITHIN GROUP` or `agg(x ORDER BY ...)`): do not transition
//!   yet; buffer `(sort key values, arg values)` per group and at finalization sort the buffer with
//!   `sortutil::compare_keys` and feed the sorted rows to `transition`;
//! * otherwise `state.transition(&arg_values, &FuncCtx)` immediately.
//! * Finalize with `state.finalize(&FuncCtx)`.
//! The `FuncCtx` is built like the evaluator's (`eval_call::with_func_ctx`) with `arg_types =
//! call.arg_types`, `result_type = call.result_type`.
//! Memory: all groups are held in memory (no spilling in v1). Call `ex.check_interrupts()` at least
//! every 1024 input rows.
use pgarbor_common::{PgError, Result};
use pgarbor_plan::plan::{AggregateNode, PlanNode};

use crate::build::BuildCtx;
use crate::runtime::RowSource;

#[allow(unused_variables)]
pub fn build_aggregate(node: &PlanNode, n: &AggregateNode, b: &mut BuildCtx) -> Result<Box<dyn RowSource>> {
    Err(PgError::unimplemented("EXE-19", "Aggregate node (node_agg::build_aggregate)"))
}
