//! numeric SQL functions, comparisons and conversions (TYPE-05, TYPE-14). Lease TYP-12.
//!
//! PG18 reference: `src/backend/utils/adt/numeric.c`.
//! Owned procs (ImplModule::Numeric; `grep 'module: ImplModule::Numeric' src/catalog_data/procs.rs`):
//! `numeric_add numeric_sub numeric_mul numeric_div numeric_div_trunc numeric_mod numeric_uminus
//! numeric_uplus numeric_abs numeric_sign numeric_round numeric_trunc numeric_ceil numeric_floor
//! numeric_sqrt numeric_exp numeric_ln numeric_log numeric_power numeric_fac (factorial(int8))
//! numeric_gcd numeric_lcm numeric_scale numeric_min_scale numeric_trim_scale numeric_inc
//! numeric_larger numeric_smaller numeric_eq/ne/lt/le/gt/ge numeric_cmp hash_numeric(+extended)
//! numeric_random (random(min numeric, max numeric)) width_bucket_numeric in_range_numeric_numeric
//! int2_numeric int4_numeric int8_numeric float4_numeric float8_numeric numeric_int2 numeric_int4
//! numeric_int8 numeric_float4 numeric_float8`, and the SQL-language builtins (match by `proc.oid`):
//! `round(numeric)` (1708) = round(x, 0), `trunc(numeric)` (1710), `log(numeric)` (1741) = log10,
//! `log10(numeric)` (1481).
//!
//! Wrappers delegate to `arith` / `transcendental`. Semantics (oracle-verified):
//! - numeric → int2/int4/int8 rounds half away from zero (2.5 → 3, -2.5 → -3); out of range →
//!   22003 `smallint out of range` / `integer out of range` / `bigint out of range`;
//!   NaN → 0A000 `cannot convert NaN to smallint|integer|bigint`; ±Inf → 0A000
//!   `cannot convert infinity to smallint|integer|bigint`.
//! - float8 → numeric: NaN → NaN, ±inf → ±Infinity, finite → text of `%.15g` (DBL_DIG) parsed as
//!   numeric: `0.1::float8::numeric` = 0.1, `(1/3::float8)::numeric` = 0.333333333333333,
//!   `1e20::float8::numeric` = 100000000000000000000, `1.23456789e-7` → 0.000000123456789.
//!   float4 → numeric uses `%.6g` (FLT_DIG): `1.1::float4::numeric` = 1.1.
//! - numeric → float8/float4: correctly rounded parse of the decimal text; NaN/Inf map to IEEE;
//!   overflow → 22003 `value out of range: overflow`.
//! - `scale(1.50)` = 2 (NULL for NaN/Inf); `min_scale(1.500)` = 1; `trim_scale(1.500)` = 1.5.
//! - `factorial(n)`: n < 0 → 22003 `factorial of a negative number is undefined`;
//!   `factorial(20)` = 2432902008176640000. gcd/lcm per numeric.c (`gcd(12.5, 7.5)` = 2.5,
//!   `lcm(1.5, 2.5)` = 7.5; result dscale = max of input dscales).
//! - `width_bucket(5.35, 0.024, 10.06, 5)` = 3; count <= 0 → 2201G
//!   `count must be greater than zero`; bounds equal → 2201G `lower bound cannot equal upper bound`;
//!   NaN operand → 2201G `operand, lower bound, and upper bound cannot be NaN`.
use super::Numeric;
use crate::catalog_data::BuiltinProc;
use crate::func::FuncImpl;
use pgarbor_common::{PgError, Result};

/// numeric → i64 (half away from zero), PG errors per module docs.
// CONTRACT
#[allow(unused_variables)]
pub fn to_i64(n: &Numeric) -> Result<i64> {
    Err(PgError::unimplemented("TYP-12", "numeric to bigint"))
}

/// numeric → i32.
// CONTRACT
#[allow(unused_variables)]
pub fn to_i32(n: &Numeric) -> Result<i32> {
    Err(PgError::unimplemented("TYP-12", "numeric to integer"))
}

/// numeric → i16.
// CONTRACT
#[allow(unused_variables)]
pub fn to_i16(n: &Numeric) -> Result<i16> {
    Err(PgError::unimplemented("TYP-12", "numeric to smallint"))
}

/// numeric → f64 (`numeric_float8`).
// CONTRACT
#[allow(unused_variables)]
pub fn to_f64(n: &Numeric) -> Result<f64> {
    Err(PgError::unimplemented("TYP-12", "numeric to double precision"))
}

/// f64 → numeric (`float8_numeric`, 15 significant digits).
// CONTRACT
#[allow(unused_variables)]
pub fn from_f64(v: f64) -> Result<Numeric> {
    Err(PgError::unimplemented("TYP-12", "double precision to numeric"))
}

/// f32 → numeric (`float4_numeric`, 6 significant digits).
// CONTRACT
#[allow(unused_variables)]
pub fn from_f32(v: f32) -> Result<Numeric> {
    Err(PgError::unimplemented("TYP-12", "real to numeric"))
}

/// Function registry hook (see `crate::func`).
#[allow(unused_variables)]
pub(crate) fn lookup(proc: &BuiltinProc) -> Option<FuncImpl> {
    None
}

/// 0A000 for a Numeric-module proc without implementation.
pub(crate) fn unsupported(proc: &BuiltinProc) -> PgError {
    PgError::unimplemented("TYP-12", format!("function {}", proc.name))
}
