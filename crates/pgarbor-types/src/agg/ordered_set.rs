//! Ordered-set and hypothetical-set aggregates: percentile_cont, percentile_disc, mode, and
//! hypothetical rank/dense_rank/percent_rank/cume_dist (TYPE-12). Lease TYP-48.
//!
//! PG18 reference: `src/backend/utils/adt/orderedsetaggs.c`.
//! Contract with the executor: rows arrive ALREADY SORTED by the WITHIN GROUP clause (NULL sort keys
//! included per the clause's NULLS ordering); `spec.direct_args` holds the evaluated direct arguments.
//! - `percentile_disc(fraction) WITHIN GROUP (ORDER BY x)`: first value whose cumulative position >=
//!   fraction (NULL sort values ignored); `percentile_cont(fraction)` over float8 or interval: linear
//!   interpolation; array-of-fractions variants return arrays. fraction outside [0,1] → 22003
//!   `percentile value 1.5 is not between 0 and 1`. Oracle (1,2,4): cont(0.5) = 2, disc(0.5) = 2.
//! - `mode()`: most frequent value, ties → the first in sort order.
//! - hypothetical `rank(args) WITHIN GROUP (ORDER BY ...)` etc.: rank the direct args among the rows.
use super::{AggSpec, AggState};
use crate::catalog_data::BuiltinAggregate;
use crate::context::FuncCtx;
use pgarbor_common::{PgError, Result};

/// Instantiate an ordered-set / hypothetical-set aggregate.
#[allow(unused_variables)]
pub(crate) fn make(agg: &BuiltinAggregate, spec: &AggSpec, ctx: &FuncCtx) -> Result<Box<dyn AggState>> {
    Err(PgError::unimplemented("TYP-48", format!("aggregate {}", agg.name)))
}
