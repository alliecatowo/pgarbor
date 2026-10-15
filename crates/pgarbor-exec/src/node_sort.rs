//! `Sort` and `Limit` operators (lease EXE-23, north-star). PG: `nodeSort.c`, `nodeLimit.c`.
//!
//! ## Sort
//! Blocking: on the first `next` drain the input, then emit in key order. Keys = `SortKeyRt::from_spec`
//! of `keys` (evaluated ONCE per row), compared with `sortutil::compare_keys`; `sortutil::sort_rows` for
//! the full sort. With `limit: Some(n)` (top-N: offset already added) keep only the best `n` rows with
//! `sortutil::TopN`. Output row = input row. `rescan(params)`: if `ex.epoch()` is unchanged since the
//! sort was done and the input is parameter-independent re-emit from the start without re-reading;
//! otherwise rescan the input and sort again.
//!
//! ## Limit
//! `count`/`offset` expressions are evaluated ONCE per scan over the empty row (`Param`/`Const`/stable):
//! `count` NULL = no limit; `offset` NULL = 0; negative `count` -> `error::limit_negative()` (2201W),
//! negative `offset` -> `error::offset_negative()` (2201X) (checked when the scan starts, before the
//! first row). Skip `offset` rows, then emit up to `count` rows. `count == 0` must NOT pull any input
//! row. `with_ties`: after `count` rows continue emitting while the next input row's `ties_keys` values
//! are equal (`compare_keys` == Equal) to the LAST emitted row's (input is sorted by them); `count == 0`
//! with ties emits nothing. Output row = input row. Int8 limit values (`Datum::Int8`).
use pgarbor_common::{PgError, Result};
use pgarbor_plan::plan::{LimitNode, PlanNode, SortNode};

use crate::build::BuildCtx;
use crate::runtime::RowSource;

#[allow(unused_variables)]
pub fn build_sort(node: &PlanNode, n: &SortNode, b: &mut BuildCtx) -> Result<Box<dyn RowSource>> {
    Err(PgError::unimplemented("EXE-23", "Sort node (node_sort::build_sort)"))
}

#[allow(unused_variables)]
pub fn build_limit(node: &PlanNode, n: &LimitNode, b: &mut BuildCtx) -> Result<Box<dyn RowSource>> {
    Err(PgError::unimplemented("EXE-23", "Limit node (node_sort::build_limit)"))
}
