//! Statistical aggregates: stddev/variance families, covariance, correlation, regression (TYPE-12).
//! Lease TYP-47.
//!
//! PG18 reference: `utils/adt/float.c` (`float8_accum` uses the Youngs-Cramer algorithm,
//! `float8_var_samp`, `float8_regr_accum`, `float8_corr`, ...), `numeric.c` (`numeric_accum`,
//! `numeric_var_samp`, `numeric_stddev_*`, `int8_accum`, `numeric_poly_*`).
//! Aggregates (module AggStats): `variance`/`var_samp`, `var_pop`, `stddev`/`stddev_samp`, `stddev_pop`
//! over int2/int4/int8/numeric (→ numeric, computed EXACTLY then rounded per PG's scale rules in
//! `numeric_stddev_internal`: rscale = max(dscale of sumX² ..., ...)) and float4/float8 (→ float8,
//! Youngs-Cramer); `covar_pop covar_samp corr regr_count regr_sxx regr_syy regr_sxy regr_avgx regr_avgy
//! regr_r2 regr_slope regr_intercept` (float8 args, rows with any NULL skipped).
//! Oracle (values 1,2,4): `stddev(x)` = `1.5275252316519467`, `variance(x)` = `2.3333333333333333`,
//! `stddev_pop(x::float8)` = `1.247219128924647`, `corr(x, x*2)` = `1`, `regr_slope(x*2, x)` = `2`.
//! Fewer than 2 rows for samp variants → NULL; zero rows → NULL.
use super::{AggSpec, AggState};
use crate::catalog_data::BuiltinAggregate;
use crate::context::FuncCtx;
use pgarbor_common::{PgError, Result};

/// Instantiate a statistical aggregate.
#[allow(unused_variables)]
pub(crate) fn make(agg: &BuiltinAggregate, spec: &AggSpec, ctx: &FuncCtx) -> Result<Box<dyn AggState>> {
    Err(PgError::unimplemented("TYP-47", format!("aggregate {}", agg.name)))
}
