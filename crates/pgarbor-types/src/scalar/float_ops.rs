//! Float operators, casts and math functions (TYPE-14). Lease TYP-07.
//!
//! PG18 reference: `src/backend/utils/adt/float.c`, `src/include/utils/float.h`.
//! Owned procs (ImplModule::FloatOps; `grep 'module: ImplModule::FloatOps' src/catalog_data/procs.rs`):
//! - arithmetic `float{4,8}{pl,mi,mul,div}`, mixed `float48*`/`float84*`, `float{4,8}{um,up,abs}`,
//!   `float{4,8}{larger,smaller}`; comparison `float{4,8}{eq,ne,lt,le,gt,ge}`, `float48eq`...,
//!   `btfloat{4,8}cmp`, `btfloat48cmp`, `btfloat84cmp`; `hashfloat4/8(+extended)`;
//! - casts `ftod dtof i2tod i4tod i8tod i2tof i4tof i8tof dtoi2 dtoi4 dtoi8 ftoi2 ftoi4 ftoi8`;
//! - math `dround dtrunc dceil dfloor dsign dsqrt dcbrt dpow dexp dlog1 (ln) dlog10 dpi radians degrees`,
//!   trig `dsin dcos dtan dcot dasin dacos datan datan2` + degree variants `dsind dcosd dtand dcotd dasind dacosd datand datan2d`,
//!   hyperbolic `dsinh dcosh dtanh dasinh dacosh datanh`, `derf derfc dgamma dlgamma`;
//! - `drandom` (`random()` = `ctx.session.random_f64()`), `drandom_normal(mean, stddev)` (Box-Muller from
//!   two `random_f64` draws), `setseed` (arg outside [-1,1] → 22023
//!   `setseed parameter %g is out of allowed range [-1,1]`; else `ctx.session.set_seed`, returns void = `Datum::Null`);
//! - `width_bucket_float8`, `in_range_float{4,8}_float8`.
//!
//! Semantics (oracle-verified, PG `float_overflow_error` etc.):
//! - Comparison treats NaN as EQUAL to NaN and GREATER than every other value (including +Infinity);
//!   `-0 = 0`. (`'NaN'::float8 > 'Infinity'` is true.)
//! - Overflow: a finite-inputs operation producing ±inf → 22003 `value out of range: overflow`;
//!   a nonzero-inputs result of exactly 0 → 22003 `value out of range: underflow`
//!   (`1e-300::float8 * 1e-300`). Subnormal results are allowed (`1e-308 * 1e-10` → `1e-318`).
//! - Division by zero (`/`): 22012 `division by zero`, including `0.0/0`; but `'NaN'::float8 / 0` = NaN
//!   (PG `float8_div`: error only when `val2 == 0.0 && !isnan(val1)`).
//! - float → int casts round half to EVEN (`rint`): 1.5→2, 2.5→2, -2.5→-2; NaN or out of range →
//!   22003 `integer out of range` / `smallint out of range` / `bigint out of range`.
//! - `round(float8)` = rint (half even; `round(-0.5::float8)` = `-0`). `trunc`/`ceil`/`floor` as C.
//! - `sqrt` of negative → 2201F `cannot take square root of a negative number`; `ln`/`log10` of 0 →
//!   2201E `cannot take logarithm of zero`, of negative → 2201E `cannot take logarithm of a negative number`.
//! - `power`: `0 ^ negative` → 2201F `zero raised to a negative power is undefined`; negative base with
//!   non-integer exponent → 2201F `a negative number raised to a non-integer power yields a complex result`;
//!   PG18 also special-cases NaN/inf inputs per POSIX `pow` (see `dpow`).
//! - Trig input out of domain (`asin(2)`) → 22003 `input is out of range`.
//! - Known libm difference: glibc `cbrt(27)` prints `3.0000000000000004` in the oracle; Rust's `cbrt`
//!   returns exactly 3. Use `f64::cbrt` anyway and record the mismatch in the assumption ledger.
use crate::catalog_data::BuiltinProc;
use crate::func::FuncImpl;
use pgarbor_common::PgError;

/// Function registry hook: map `proc.prosrc` to an implementation (see `crate::func`).
#[allow(unused_variables)]
pub(crate) fn lookup(proc: &BuiltinProc) -> Option<FuncImpl> {
    None
}

/// 0A000 for a FloatOps-module proc without implementation.
pub(crate) fn unsupported(proc: &BuiltinProc) -> PgError {
    PgError::unimplemented("TYP-07", format!("function {}", proc.name))
}
