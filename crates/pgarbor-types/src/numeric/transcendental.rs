//! numeric sqrt / exp / ln / log / power (TYPE-05). Lease TYP-11 (sonnet).
//!
//! PG18 reference: `src/backend/utils/adt/numeric.c` — `numeric_sqrt` + `sqrt_var`, `numeric_exp`
//! + `exp_var`, `numeric_ln` + `ln_var` + `estimate_ln_dweight`, `numeric_log` + `log_var`,
//! `numeric_power` + `power_var` + `power_var_int` + `power_ten_int`.
//!
//! Results must be digit-exact with PG at PG's chosen result scale. Strategy: compute with
//! BigDecimal using several guard digits beyond `rscale` (Newton iteration for sqrt; argument
//! reduction + Taylor series for exp/ln, as PG does), then round half away from zero to `rscale`.
//!
//! Result scales (NUMERIC_MIN_SIG_DIGITS = 16, display scale clamped to [0, 1000]):
//! - sqrt: `sweight = (weight + 1) * 4 / 2 - 1` (weight = base-10000 weight of the argument, see
//!   `arith` docs); `rscale = max(16 - sweight, dscale)`. Oracle: `sqrt(2::numeric)` → `1.414213562373095`
//!   (15 digits: weight 0 → sweight 1 → rscale 15), `sqrt(1e-20::numeric)` → `0.0000000001000000000000000`.
//! - exp: `val = to_f64(x) * 0.434294481903252` clamped to ±2000; `rscale = max(16 - (int)val, dscale)`.
//!   Oracle: `exp(1::numeric)` → `2.7182818284590452`, `exp(100::numeric)` →
//!   `26881171418161354484126255515800135873611119`.
//! - ln: `rscale = max(16 - estimate_ln_dweight(x), dscale)`. Oracle: `ln(10::numeric)` → `2.3025850929940457`,
//!   `ln(1e400::numeric)` → `921.03403719761827`.
//! - log(b, x): `rscale = max(16 - (ln_num_dweight - ln_base_dweight), dscale_b, dscale_x)`; log10(x) = log(10, x).
//! - power(a, b): see `numeric_power`/`power_var`: integer exponents use exact `power_var_int` with
//!   its own rscale rule; otherwise exp(b·ln(a)). Oracle: `2::numeric ^ 0.5` → `1.4142135623730950`,
//!   `10::numeric ^ 20` → `100000000000000000000`, `power(2::numeric, -3)` → `0.1250000000000000`.
//!
//! Errors (exact PG texts): sqrt of negative → 2201F `cannot take square root of a negative number`;
//! ln/log of zero → 2201E `cannot take logarithm of zero`; of negative → 2201E
//! `cannot take logarithm of a negative number`; `0 ^ negative` → 2201F `zero raised to a negative power is undefined`;
//! negative ^ non-integer → 2201F `a negative number raised to a non-integer power yields a complex result`;
//! exp/power overflow → 22003 `value overflows numeric format`. Specials follow `numeric_exp` etc.
//! (exp(-Inf)=0, exp(Inf)=Inf, ln(Inf)=Inf, sqrt(Inf)=Inf, NaN→NaN, power special cases per POSIX pow).
use super::Numeric;
use pgarbor_common::{PgError, Result};

/// `sqrt(x)`.
// CONTRACT
#[allow(unused_variables)]
pub fn sqrt(x: &Numeric) -> Result<Numeric> {
    Err(PgError::unimplemented("TYP-11", "numeric sqrt"))
}

/// `exp(x)`.
// CONTRACT
#[allow(unused_variables)]
pub fn exp(x: &Numeric) -> Result<Numeric> {
    Err(PgError::unimplemented("TYP-11", "numeric exp"))
}

/// `ln(x)`.
// CONTRACT
#[allow(unused_variables)]
pub fn ln(x: &Numeric) -> Result<Numeric> {
    Err(PgError::unimplemented("TYP-11", "numeric ln"))
}

/// `log(base, x)`.
// CONTRACT
#[allow(unused_variables)]
pub fn log(base: &Numeric, x: &Numeric) -> Result<Numeric> {
    Err(PgError::unimplemented("TYP-11", "numeric log"))
}

/// `log10(x)` / `log(x)`.
// CONTRACT
#[allow(unused_variables)]
pub fn log10(x: &Numeric) -> Result<Numeric> {
    Err(PgError::unimplemented("TYP-11", "numeric log10"))
}

/// `power(a, b)` / `a ^ b`.
// CONTRACT
#[allow(unused_variables)]
pub fn power(a: &Numeric, b: &Numeric) -> Result<Numeric> {
    Err(PgError::unimplemented("TYP-11", "numeric power"))
}
