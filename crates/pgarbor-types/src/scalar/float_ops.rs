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
//!
//! Implementation notes (TYP-07):
//! - Transcendental and special functions use the platform libm, which is what PostgreSQL calls on the
//!   oracle host. `erf`, `erfc`, `tgamma` and `lgamma` are not in Rust's `std`, so they are declared
//!   from libm (see `libm_special`). Errno-based checks in PG (`ERANGE`/`EDOM`) are not observable
//!   here; the same conditions are detected from the result (inf, NaN, or 0 for nonzero input).
//! - Hashing uses `compare::pg_hash_bytes` / `pg_hash_bytes_extended` (PG `hash_any`), over the
//!   native-endian bytes of the widened value, exactly as `hashfunc.c` does.

use crate::catalog_data::BuiltinProc;
use crate::compare;
use crate::context::FuncCtx;
use crate::datum::{type_mismatch, Datum};
use crate::func::{FuncImpl, ScalarFn};
use pgarbor_common::{PgError, Result, SqlState};
use std::ops::{Add, Div, Mul, Neg, Sub};
use std::sync::OnceLock;

/// Radians per degree, PG `RADIANS_PER_DEGREE` (`float.h`).
const RADIANS_PER_DEGREE: f64 = 0.0174532925199432957692;
/// PG `M_PI`.
const M_PI: f64 = std::f64::consts::PI;

/// Special functions that Rust's `std` does not provide. These resolve to the platform libm, the
/// same library PostgreSQL calls.
#[cfg(not(target_arch = "wasm32"))]
unsafe extern "C" {
    safe fn erf(x: f64) -> f64;
    safe fn erfc(x: f64) -> f64;
    safe fn tgamma(x: f64) -> f64;
    safe fn lgamma(x: f64) -> f64;
}

/// wasm32 has no platform libm: use the pure-Rust `libm` crate (results may differ from glibc in the
/// last ulp; the oracle comparison is only authoritative for native builds).
#[cfg(target_arch = "wasm32")]
fn erf(x: f64) -> f64 {
    libm::erf(x)
}
#[cfg(target_arch = "wasm32")]
fn erfc(x: f64) -> f64 {
    libm::erfc(x)
}
#[cfg(target_arch = "wasm32")]
fn tgamma(x: f64) -> f64 {
    libm::tgamma(x)
}
#[cfg(target_arch = "wasm32")]
fn lgamma(x: f64) -> f64 {
    libm::lgamma(x)
}

// ---------------------------------------------------------------------------------------------
// Registry hook
// ---------------------------------------------------------------------------------------------

/// Function registry hook: map `proc.prosrc` to an implementation (see `crate::func`).
pub(crate) fn lookup(proc: &BuiltinProc) -> Option<FuncImpl> {
    let f: ScalarFn = match proc.prosrc {
        "float4pl" => float4pl,
        "float4mi" => float4mi,
        "float4mul" => float4mul,
        "float4div" => float4div,
        "float4um" => float4um,
        "float4abs" => float4abs,
        "float4up" => float4up,
        "float4larger" => float4larger,
        "float4smaller" => float4smaller,
        "float4eq" => float4eq,
        "float4ne" => float4ne,
        "float4lt" => float4lt,
        "float4le" => float4le,
        "float4gt" => float4gt,
        "float4ge" => float4ge,
        "btfloat4cmp" => btfloat4cmp,
        "float8pl" => float8pl,
        "float8mi" => float8mi,
        "float8mul" => float8mul,
        "float8div" => float8div,
        "float8um" => float8um,
        "float8abs" => float8abs,
        "float8up" => float8up,
        "float8larger" => float8larger,
        "float8smaller" => float8smaller,
        "float8eq" => float8eq,
        "float8ne" => float8ne,
        "float8lt" => float8lt,
        "float8le" => float8le,
        "float8gt" => float8gt,
        "float8ge" => float8ge,
        "btfloat8cmp" => btfloat8cmp,
        "float48pl" => float48pl,
        "float48mi" => float48mi,
        "float48mul" => float48mul,
        "float48div" => float48div,
        "float84pl" => float84pl,
        "float84mi" => float84mi,
        "float84mul" => float84mul,
        "float84div" => float84div,
        "float48eq" => float48eq,
        "float48ne" => float48ne,
        "float48lt" => float48lt,
        "float48le" => float48le,
        "float48gt" => float48gt,
        "float48ge" => float48ge,
        "float84eq" => float84eq,
        "float84ne" => float84ne,
        "float84lt" => float84lt,
        "float84le" => float84le,
        "float84gt" => float84gt,
        "float84ge" => float84ge,
        "btfloat48cmp" => btfloat48cmp,
        "btfloat84cmp" => btfloat84cmp,
        "ftod" => ftod,
        "dtof" => dtof,
        "i2tod" => i2tod,
        "i4tod" => i4tod,
        "i8tod" => i8tod,
        "i2tof" => i2tof,
        "i4tof" => i4tof,
        "i8tof" => i8tof,
        "dtoi2" => dtoi2,
        "dtoi4" => dtoi4,
        "dtoi8" => dtoi8,
        "ftoi2" => ftoi2,
        "ftoi4" => ftoi4,
        "ftoi8" => ftoi8,
        "dround" => dround,
        "dtrunc" => dtrunc,
        "dceil" => dceil,
        "dfloor" => dfloor,
        "dsign" => dsign,
        "dsqrt" => dsqrt,
        "dcbrt" => dcbrt,
        "dpow" => dpow,
        "dexp" => dexp,
        "dlog1" => dlog1,
        "dlog10" => dlog10,
        "dacos" => dacos,
        "dasin" => dasin,
        "datan" => datan,
        "datan2" => datan2,
        "dcos" => dcos,
        "dcot" => dcot,
        "dsin" => dsin,
        "dtan" => dtan,
        "dsind" => dsind,
        "dcosd" => dcosd,
        "dtand" => dtand,
        "dcotd" => dcotd,
        "dasind" => dasind,
        "dacosd" => dacosd,
        "datand" => datand,
        "datan2d" => datan2d,
        "degrees" => degrees,
        "radians" => radians,
        "dpi" => dpi,
        "dsinh" => dsinh,
        "dcosh" => dcosh,
        "dtanh" => dtanh,
        "dasinh" => dasinh,
        "dacosh" => dacosh,
        "datanh" => datanh,
        "derf" => derf,
        "derfc" => derfc,
        "dgamma" => dgamma,
        "dlgamma" => dlgamma,
        "drandom" => drandom,
        "drandom_normal" => drandom_normal,
        "setseed" => setseed,
        "width_bucket_float8" => width_bucket_float8,
        "in_range_float8_float8" => in_range_float8_float8,
        "in_range_float4_float8" => in_range_float4_float8,
        "hashfloat4" => hashfloat4,
        "hashfloat8" => hashfloat8,
        "hashfloat4extended" => hashfloat4extended,
        "hashfloat8extended" => hashfloat8extended,
        _ => return None,
    };
    Some(FuncImpl::Scalar(f))
}

/// 0A000 for a FloatOps-module proc without implementation.
pub(crate) fn unsupported(proc: &BuiltinProc) -> PgError {
    PgError::unimplemented("TYP-07", format!("function {}", proc.name))
}

// ---------------------------------------------------------------------------------------------
// Argument access and error helpers
// ---------------------------------------------------------------------------------------------

fn missing_arg(i: usize) -> PgError {
    PgError::internal(format!("float function: missing argument {i}"))
}

macro_rules! getter {
    ($name:ident, $variant:ident, $t:ty, $label:literal) => {
        fn $name(args: &[Datum], i: usize) -> Result<$t> {
            match args.get(i) {
                Some(Datum::$variant(v)) => Ok(*v),
                Some(other) => Err(type_mismatch($label, other)),
                None => Err(missing_arg(i)),
            }
        }
    };
}

getter!(arg_f4, Float4, f32, "float4");
getter!(arg_f8, Float8, f64, "float8");
getter!(arg_i2, Int2, i16, "int2");
getter!(arg_i4, Int4, i32, "int4");
getter!(arg_i8, Int8, i64, "int8");
getter!(arg_bool, Bool, bool, "bool");

/// A float4 argument widened to float8 (exact).
fn arg_f4w(args: &[Datum], i: usize) -> Result<f64> {
    Ok(f64::from(arg_f4(args, i)?))
}

fn err(state: SqlState, msg: &'static str) -> PgError {
    PgError::new(state, msg)
}

fn overflow() -> PgError {
    err(SqlState::NUMERIC_VALUE_OUT_OF_RANGE, "value out of range: overflow")
}

fn underflow() -> PgError {
    err(SqlState::NUMERIC_VALUE_OUT_OF_RANGE, "value out of range: underflow")
}

fn zero_divide() -> PgError {
    err(SqlState::DIVISION_BY_ZERO, "division by zero")
}

fn input_out_of_range() -> PgError {
    err(SqlState::NUMERIC_VALUE_OUT_OF_RANGE, "input is out of range")
}

fn log_zero() -> PgError {
    err(SqlState::INVALID_ARGUMENT_FOR_LOGARITHM, "cannot take logarithm of zero")
}

fn log_negative() -> PgError {
    err(SqlState::INVALID_ARGUMENT_FOR_LOGARITHM, "cannot take logarithm of a negative number")
}

// ---------------------------------------------------------------------------------------------
// Generic core: PG float.h inline helpers, for float4 (f32) and float8 (f64)
// ---------------------------------------------------------------------------------------------

trait Flt:
    Copy
    + PartialEq
    + PartialOrd
    + Add<Output = Self>
    + Sub<Output = Self>
    + Mul<Output = Self>
    + Div<Output = Self>
    + Neg<Output = Self>
{
    const ZERO: Self;
    fn isnan(self) -> bool;
    fn isinf(self) -> bool;
}

impl Flt for f64 {
    const ZERO: f64 = 0.0;
    fn isnan(self) -> bool {
        f64::is_nan(self)
    }
    fn isinf(self) -> bool {
        f64::is_infinite(self)
    }
}

impl Flt for f32 {
    const ZERO: f32 = 0.0;
    fn isnan(self) -> bool {
        f32::is_nan(self)
    }
    fn isinf(self) -> bool {
        f32::is_infinite(self)
    }
}

/// PG `float8_pl` / `float4_pl`.
fn pl<T: Flt>(a: T, b: T) -> Result<T> {
    let r = a + b;
    if r.isinf() && !a.isinf() && !b.isinf() {
        return Err(overflow());
    }
    Ok(r)
}

/// PG `float8_mi` / `float4_mi`.
fn mi<T: Flt>(a: T, b: T) -> Result<T> {
    let r = a - b;
    if r.isinf() && !a.isinf() && !b.isinf() {
        return Err(overflow());
    }
    Ok(r)
}

/// PG `float8_mul` / `float4_mul`.
fn mul<T: Flt>(a: T, b: T) -> Result<T> {
    let r = a * b;
    if r.isinf() && !a.isinf() && !b.isinf() {
        return Err(overflow());
    }
    if r == T::ZERO && a != T::ZERO && b != T::ZERO {
        return Err(underflow());
    }
    Ok(r)
}

/// PG `float8_div` / `float4_div`: division by zero is an error unless the dividend is NaN.
fn div<T: Flt>(a: T, b: T) -> Result<T> {
    if b == T::ZERO && !a.isnan() {
        return Err(zero_divide());
    }
    let r = a / b;
    if r.isinf() && !a.isinf() {
        return Err(overflow());
    }
    if r == T::ZERO && a != T::ZERO && !b.isinf() {
        return Err(underflow());
    }
    Ok(r)
}

/// NaN-aware `=` (NaN equals NaN).
fn f_eq<T: Flt>(a: T, b: T) -> bool {
    if a.isnan() { b.isnan() } else { !b.isnan() && a == b }
}

/// NaN-aware `<>`.
fn f_ne<T: Flt>(a: T, b: T) -> bool {
    if a.isnan() { !b.isnan() } else { b.isnan() || a != b }
}

/// NaN-aware `<` (NaN is greater than everything).
fn f_lt<T: Flt>(a: T, b: T) -> bool {
    !a.isnan() && (b.isnan() || a < b)
}

/// NaN-aware `<=`.
fn f_le<T: Flt>(a: T, b: T) -> bool {
    b.isnan() || (!a.isnan() && a <= b)
}

/// NaN-aware `>`.
fn f_gt<T: Flt>(a: T, b: T) -> bool {
    !b.isnan() && (a.isnan() || a > b)
}

/// NaN-aware `>=`.
fn f_ge<T: Flt>(a: T, b: T) -> bool {
    a.isnan() || (!b.isnan() && a >= b)
}

/// PG `float8_cmp_internal`.
fn f_cmp<T: Flt>(a: T, b: T) -> i32 {
    if f_gt(a, b) {
        1
    } else if f_lt(a, b) {
        -1
    } else {
        0
    }
}

/// PG `float8larger` / `float4larger`.
fn f_larger<T: Flt>(a: T, b: T) -> Result<T> {
    Ok(if f_gt(a, b) { a } else { b })
}

/// PG `float8smaller` / `float4smaller`.
fn f_smaller<T: Flt>(a: T, b: T) -> Result<T> {
    Ok(if f_lt(a, b) { a } else { b })
}

// ---------------------------------------------------------------------------------------------
// Datum wrappers
// ---------------------------------------------------------------------------------------------

/// Two-argument function returning a float value through a fallible generic core.
macro_rules! bin {
    ($($name:ident : $g0:ident, $g1:ident => $var:ident = $core:ident;)*) => {
        $(
            fn $name(a: &[Datum], _ctx: &FuncCtx) -> Result<Datum> {
                Ok(Datum::$var($core($g0(a, 0)?, $g1(a, 1)?)?))
            }
        )*
    };
}

/// Two-argument function returning a boolean from a pure core.
macro_rules! bin_bool {
    ($($name:ident : $g0:ident, $g1:ident => $core:ident;)*) => {
        $(
            fn $name(a: &[Datum], _ctx: &FuncCtx) -> Result<Datum> {
                Ok(Datum::Bool($core($g0(a, 0)?, $g1(a, 1)?)))
            }
        )*
    };
}

/// Two-argument function returning an int4 from a pure core.
macro_rules! bin_int4 {
    ($($name:ident : $g0:ident, $g1:ident => $core:ident;)*) => {
        $(
            fn $name(a: &[Datum], _ctx: &FuncCtx) -> Result<Datum> {
                Ok(Datum::Int4($core($g0(a, 0)?, $g1(a, 1)?)))
            }
        )*
    };
}

/// One-argument function returning a float through a fallible core.
macro_rules! un {
    ($($name:ident : $g:ident => $var:ident = $core:ident;)*) => {
        $(
            fn $name(a: &[Datum], _ctx: &FuncCtx) -> Result<Datum> {
                Ok(Datum::$var($core($g(a, 0)?)?))
            }
        )*
    };
}

// Arithmetic and comparison, same-width float4.
bin! {
    float4pl: arg_f4, arg_f4 => Float4 = pl;
    float4mi: arg_f4, arg_f4 => Float4 = mi;
    float4mul: arg_f4, arg_f4 => Float4 = mul;
    float4div: arg_f4, arg_f4 => Float4 = div;
    float4larger: arg_f4, arg_f4 => Float4 = f_larger;
    float4smaller: arg_f4, arg_f4 => Float4 = f_smaller;
    float8pl: arg_f8, arg_f8 => Float8 = pl;
    float8mi: arg_f8, arg_f8 => Float8 = mi;
    float8mul: arg_f8, arg_f8 => Float8 = mul;
    float8div: arg_f8, arg_f8 => Float8 = div;
    float8larger: arg_f8, arg_f8 => Float8 = f_larger;
    float8smaller: arg_f8, arg_f8 => Float8 = f_smaller;
    // Mixed width: float4 widened to float8 first (PG casts the float4 operand).
    float48pl: arg_f4w, arg_f8 => Float8 = pl;
    float48mi: arg_f4w, arg_f8 => Float8 = mi;
    float48mul: arg_f4w, arg_f8 => Float8 = mul;
    float48div: arg_f4w, arg_f8 => Float8 = div;
    float84pl: arg_f8, arg_f4w => Float8 = pl;
    float84mi: arg_f8, arg_f4w => Float8 = mi;
    float84mul: arg_f8, arg_f4w => Float8 = mul;
    float84div: arg_f8, arg_f4w => Float8 = div;
}

bin_bool! {
    float4eq: arg_f4, arg_f4 => f_eq;
    float4ne: arg_f4, arg_f4 => f_ne;
    float4lt: arg_f4, arg_f4 => f_lt;
    float4le: arg_f4, arg_f4 => f_le;
    float4gt: arg_f4, arg_f4 => f_gt;
    float4ge: arg_f4, arg_f4 => f_ge;
    float8eq: arg_f8, arg_f8 => f_eq;
    float8ne: arg_f8, arg_f8 => f_ne;
    float8lt: arg_f8, arg_f8 => f_lt;
    float8le: arg_f8, arg_f8 => f_le;
    float8gt: arg_f8, arg_f8 => f_gt;
    float8ge: arg_f8, arg_f8 => f_ge;
    float48eq: arg_f4w, arg_f8 => f_eq;
    float48ne: arg_f4w, arg_f8 => f_ne;
    float48lt: arg_f4w, arg_f8 => f_lt;
    float48le: arg_f4w, arg_f8 => f_le;
    float48gt: arg_f4w, arg_f8 => f_gt;
    float48ge: arg_f4w, arg_f8 => f_ge;
    float84eq: arg_f8, arg_f4w => f_eq;
    float84ne: arg_f8, arg_f4w => f_ne;
    float84lt: arg_f8, arg_f4w => f_lt;
    float84le: arg_f8, arg_f4w => f_le;
    float84gt: arg_f8, arg_f4w => f_gt;
    float84ge: arg_f8, arg_f4w => f_ge;
}

bin_int4! {
    btfloat4cmp: arg_f4, arg_f4 => f_cmp;
    btfloat8cmp: arg_f8, arg_f8 => f_cmp;
    btfloat48cmp: arg_f4w, arg_f8 => f_cmp;
    btfloat84cmp: arg_f8, arg_f4w => f_cmp;
}

fn float4um(a: &[Datum], _ctx: &FuncCtx) -> Result<Datum> {
    Ok(Datum::Float4(-arg_f4(a, 0)?))
}

fn float4abs(a: &[Datum], _ctx: &FuncCtx) -> Result<Datum> {
    Ok(Datum::Float4(arg_f4(a, 0)?.abs()))
}

fn float4up(a: &[Datum], _ctx: &FuncCtx) -> Result<Datum> {
    Ok(Datum::Float4(arg_f4(a, 0)?))
}

fn float8um(a: &[Datum], _ctx: &FuncCtx) -> Result<Datum> {
    Ok(Datum::Float8(-arg_f8(a, 0)?))
}

fn float8abs(a: &[Datum], _ctx: &FuncCtx) -> Result<Datum> {
    Ok(Datum::Float8(arg_f8(a, 0)?.abs()))
}

fn float8up(a: &[Datum], _ctx: &FuncCtx) -> Result<Datum> {
    Ok(Datum::Float8(arg_f8(a, 0)?))
}

// ---------------------------------------------------------------------------------------------
// Casts
// ---------------------------------------------------------------------------------------------

fn ftod(a: &[Datum], _ctx: &FuncCtx) -> Result<Datum> {
    Ok(Datum::Float8(f64::from(arg_f4(a, 0)?)))
}

/// PG `dtof`: float8 → float4 with overflow and underflow checks.
fn c_dtof(num: f64) -> Result<f32> {
    let result = num as f32;
    if result.is_infinite() && !num.is_infinite() {
        return Err(overflow());
    }
    if result == 0.0 && num != 0.0 {
        return Err(underflow());
    }
    Ok(result)
}

fn dtof(a: &[Datum], _ctx: &FuncCtx) -> Result<Datum> {
    Ok(Datum::Float4(c_dtof(arg_f8(a, 0)?)?))
}

fn i2tod(a: &[Datum], _ctx: &FuncCtx) -> Result<Datum> {
    Ok(Datum::Float8(f64::from(arg_i2(a, 0)?)))
}

fn i4tod(a: &[Datum], _ctx: &FuncCtx) -> Result<Datum> {
    Ok(Datum::Float8(f64::from(arg_i4(a, 0)?)))
}

fn i8tod(a: &[Datum], _ctx: &FuncCtx) -> Result<Datum> {
    Ok(Datum::Float8(arg_i8(a, 0)? as f64))
}

fn i2tof(a: &[Datum], _ctx: &FuncCtx) -> Result<Datum> {
    Ok(Datum::Float4(f32::from(arg_i2(a, 0)?)))
}

fn i4tof(a: &[Datum], _ctx: &FuncCtx) -> Result<Datum> {
    Ok(Datum::Float4(arg_i4(a, 0)? as f32))
}

fn i8tof(a: &[Datum], _ctx: &FuncCtx) -> Result<Datum> {
    Ok(Datum::Float4(arg_i8(a, 0)? as f32))
}

/// PG `FLOAT8_FITS_IN_INT16` after `rint`: `[-32768, 32768)`, NaN never fits.
fn to_i16(x: f64) -> Result<i16> {
    let r = x.round_ties_even();
    if !(r >= -32768.0 && r < 32768.0) {
        return Err(err(SqlState::NUMERIC_VALUE_OUT_OF_RANGE, "smallint out of range"));
    }
    Ok(r as i16)
}

/// PG `FLOAT8_FITS_IN_INT32` after `rint`.
fn to_i32(x: f64) -> Result<i32> {
    let r = x.round_ties_even();
    if !(r >= -2147483648.0 && r < 2147483648.0) {
        return Err(err(SqlState::NUMERIC_VALUE_OUT_OF_RANGE, "integer out of range"));
    }
    Ok(r as i32)
}

/// PG `FLOAT8_FITS_IN_INT64` after `rint`.
fn to_i64(x: f64) -> Result<i64> {
    let r = x.round_ties_even();
    if !(r >= -9223372036854775808.0 && r < 9223372036854775808.0) {
        return Err(err(SqlState::NUMERIC_VALUE_OUT_OF_RANGE, "bigint out of range"));
    }
    Ok(r as i64)
}

fn dtoi2(a: &[Datum], _ctx: &FuncCtx) -> Result<Datum> {
    Ok(Datum::Int2(to_i16(arg_f8(a, 0)?)?))
}

fn dtoi4(a: &[Datum], _ctx: &FuncCtx) -> Result<Datum> {
    Ok(Datum::Int4(to_i32(arg_f8(a, 0)?)?))
}

fn dtoi8(a: &[Datum], _ctx: &FuncCtx) -> Result<Datum> {
    Ok(Datum::Int8(to_i64(arg_f8(a, 0)?)?))
}

fn ftoi2(a: &[Datum], _ctx: &FuncCtx) -> Result<Datum> {
    Ok(Datum::Int2(to_i16(arg_f4w(a, 0)?)?))
}

fn ftoi4(a: &[Datum], _ctx: &FuncCtx) -> Result<Datum> {
    Ok(Datum::Int4(to_i32(arg_f4w(a, 0)?)?))
}

fn ftoi8(a: &[Datum], _ctx: &FuncCtx) -> Result<Datum> {
    Ok(Datum::Int8(to_i64(arg_f4w(a, 0)?)?))
}

// ---------------------------------------------------------------------------------------------
// Rounding and simple math
// ---------------------------------------------------------------------------------------------

fn c_dround(x: f64) -> Result<f64> {
    Ok(x.round_ties_even())
}

fn c_dtrunc(x: f64) -> Result<f64> {
    // PG: floor for x >= 0, otherwise -floor(-x) (keeps the sign of zero as in C).
    if x >= 0.0 { Ok(x.floor()) } else { Ok(-((-x).floor())) }
}

fn c_dceil(x: f64) -> Result<f64> {
    Ok(x.ceil())
}

fn c_dfloor(x: f64) -> Result<f64> {
    Ok(x.floor())
}

fn c_dsign(x: f64) -> Result<f64> {
    Ok(if x > 0.0 {
        1.0
    } else if x < 0.0 {
        -1.0
    } else {
        0.0
    })
}

fn c_dsqrt(x: f64) -> Result<f64> {
    if x < 0.0 {
        return Err(err(
            SqlState::INVALID_ARGUMENT_FOR_POWER_FUNCTION,
            "cannot take square root of a negative number",
        ));
    }
    let r = x.sqrt();
    if r.is_infinite() && !x.is_infinite() {
        return Err(overflow());
    }
    if r == 0.0 && x != 0.0 {
        return Err(underflow());
    }
    Ok(r)
}

fn c_dcbrt(x: f64) -> Result<f64> {
    let r = x.cbrt();
    if r.is_infinite() && !x.is_infinite() {
        return Err(overflow());
    }
    if r == 0.0 && x != 0.0 {
        return Err(underflow());
    }
    Ok(r)
}

/// PG `dpow`, including its POSIX special cases for NaN and infinite operands.
fn c_dpow(x: f64, y: f64) -> Result<f64> {
    if x.is_nan() {
        if y.is_nan() || y != 0.0 {
            return Ok(f64::NAN);
        }
        return Ok(1.0);
    }
    if y.is_nan() {
        if x != 1.0 {
            return Ok(f64::NAN);
        }
        return Ok(1.0);
    }
    if x == 0.0 && y < 0.0 {
        return Err(err(
            SqlState::INVALID_ARGUMENT_FOR_POWER_FUNCTION,
            "zero raised to a negative power is undefined",
        ));
    }
    if x < 0.0 && y.floor() != y {
        return Err(err(
            SqlState::INVALID_ARGUMENT_FOR_POWER_FUNCTION,
            "a negative number raised to a non-integer power yields a complex result",
        ));
    }
    // The infinite exponent is handled first so that an infinite base does not matter.
    if y.is_infinite() {
        let absx = x.abs();
        return Ok(if absx == 1.0 {
            1.0
        } else if y > 0.0 {
            if absx > 1.0 { y } else { 0.0 }
        } else if absx > 1.0 {
            0.0
        } else {
            -y
        });
    }
    if x.is_infinite() {
        if y == 0.0 {
            return Ok(1.0);
        }
        if x > 0.0 {
            return Ok(if y > 0.0 { x } else { 0.0 });
        }
        // x = -Inf, and x < 0 with y an integer here. Odd integer y keeps the sign of x.
        let halfy = y / 2.0;
        let yisoddinteger = halfy.floor() != halfy;
        return Ok(if y > 0.0 {
            if yisoddinteger { x } else { -x }
        } else if yisoddinteger {
            -0.0
        } else {
            0.0
        });
    }
    let r = x.powf(y);
    if r.is_infinite() {
        return Err(overflow());
    }
    if r == 0.0 && x != 0.0 {
        return Err(underflow());
    }
    Ok(r)
}

fn c_dexp(x: f64) -> Result<f64> {
    if x.is_nan() {
        return Ok(x);
    }
    if x.is_infinite() {
        // POSIX: exp(+Inf) = +Inf, exp(-Inf) = 0.
        return Ok(if x > 0.0 { x } else { 0.0 });
    }
    let r = x.exp();
    if r.is_infinite() {
        return Err(overflow());
    }
    if r == 0.0 {
        return Err(underflow());
    }
    Ok(r)
}

fn c_dlog1(x: f64) -> Result<f64> {
    if x == 0.0 {
        return Err(log_zero());
    }
    if x < 0.0 {
        return Err(log_negative());
    }
    let r = x.ln();
    if r.is_infinite() && !x.is_infinite() {
        return Err(overflow());
    }
    if r == 0.0 && x != 1.0 {
        return Err(underflow());
    }
    Ok(r)
}

fn c_dlog10(x: f64) -> Result<f64> {
    if x == 0.0 {
        return Err(log_zero());
    }
    if x < 0.0 {
        return Err(log_negative());
    }
    let r = x.log10();
    if r.is_infinite() && !x.is_infinite() {
        return Err(overflow());
    }
    if r == 0.0 && x != 1.0 {
        return Err(underflow());
    }
    Ok(r)
}

fn c_degrees(x: f64) -> Result<f64> {
    div(x, RADIANS_PER_DEGREE)
}

fn c_radians(x: f64) -> Result<f64> {
    mul(x, RADIANS_PER_DEGREE)
}

// ---------------------------------------------------------------------------------------------
// Radian trigonometry
// ---------------------------------------------------------------------------------------------

fn c_dacos(x: f64) -> Result<f64> {
    if x.is_nan() {
        return Ok(f64::NAN);
    }
    if !(-1.0..=1.0).contains(&x) {
        return Err(input_out_of_range());
    }
    let r = x.acos();
    if r.is_infinite() {
        return Err(overflow());
    }
    Ok(r)
}

fn c_dasin(x: f64) -> Result<f64> {
    if x.is_nan() {
        return Ok(f64::NAN);
    }
    if !(-1.0..=1.0).contains(&x) {
        return Err(input_out_of_range());
    }
    let r = x.asin();
    if r.is_infinite() {
        return Err(overflow());
    }
    Ok(r)
}

fn c_datan(x: f64) -> Result<f64> {
    if x.is_nan() {
        return Ok(f64::NAN);
    }
    let r = x.atan();
    if r.is_infinite() {
        return Err(overflow());
    }
    Ok(r)
}

fn c_datan2(y: f64, x: f64) -> Result<f64> {
    if y.is_nan() || x.is_nan() {
        return Ok(f64::NAN);
    }
    let r = y.atan2(x);
    if r.is_infinite() {
        return Err(overflow());
    }
    Ok(r)
}

fn c_dcos(x: f64) -> Result<f64> {
    if x.is_nan() {
        return Ok(f64::NAN);
    }
    if x.is_infinite() {
        return Err(input_out_of_range());
    }
    Ok(x.cos())
}

fn c_dsin(x: f64) -> Result<f64> {
    if x.is_nan() {
        return Ok(f64::NAN);
    }
    if x.is_infinite() {
        return Err(input_out_of_range());
    }
    Ok(x.sin())
}

fn c_dtan(x: f64) -> Result<f64> {
    if x.is_nan() {
        return Ok(f64::NAN);
    }
    if x.is_infinite() {
        return Err(input_out_of_range());
    }
    Ok(x.tan())
}

fn c_dcot(x: f64) -> Result<f64> {
    if x.is_nan() {
        return Ok(f64::NAN);
    }
    if x.is_infinite() {
        return Err(input_out_of_range());
    }
    // cot(0) = Inf is not an error here (PG does not check overflow).
    Ok(1.0 / x.tan())
}

// ---------------------------------------------------------------------------------------------
// Degree trigonometry (PG `float.c`): values at multiples of 30/45/90 degrees come out exact
// because every result is a ratio of the same sine or cosine computed the same way.
// ---------------------------------------------------------------------------------------------

/// Scaling constants PG caches in `init_degree_constants` (sin_30 etc.).
struct DegConsts {
    sin_30: f64,
    one_minus_cos_60: f64,
    asin_0_5: f64,
    acos_0_5: f64,
    atan_1_0: f64,
    tan_45: f64,
    cot_45: f64,
}

fn deg_consts() -> &'static DegConsts {
    static CONSTS: OnceLock<DegConsts> = OnceLock::new();
    CONSTS.get_or_init(init_deg_consts)
}

fn init_deg_consts() -> DegConsts {
    let base = DegConsts {
        sin_30: (30.0 * RADIANS_PER_DEGREE).sin(),
        one_minus_cos_60: 1.0 - (60.0 * RADIANS_PER_DEGREE).cos(),
        asin_0_5: 0.5f64.asin(),
        acos_0_5: 0.5f64.acos(),
        atan_1_0: 1.0f64.atan(),
        tan_45: 0.0,
        cot_45: 0.0,
    };
    let s45 = sind_q1(45.0, &base);
    let c45 = cosd_q1(45.0, &base);
    DegConsts { tan_45: s45 / c45, cot_45: c45 / s45, ..base }
}

fn sind_0_to_30(x: f64, k: &DegConsts) -> f64 {
    let sin_x = (x * RADIANS_PER_DEGREE).sin();
    (sin_x / k.sin_30) / 2.0
}

fn cosd_0_to_60(x: f64, k: &DegConsts) -> f64 {
    let one_minus_cos_x = 1.0 - (x * RADIANS_PER_DEGREE).cos();
    1.0 - (one_minus_cos_x / k.one_minus_cos_60) / 2.0
}

fn sind_q1(x: f64, k: &DegConsts) -> f64 {
    if x <= 30.0 { sind_0_to_30(x, k) } else { cosd_0_to_60(90.0 - x, k) }
}

fn cosd_q1(x: f64, k: &DegConsts) -> f64 {
    if x <= 60.0 { cosd_0_to_60(x, k) } else { sind_0_to_30(90.0 - x, k) }
}

fn asind_q1(x: f64, k: &DegConsts) -> f64 {
    if x <= 0.5 {
        (x.asin() / k.asin_0_5) * 30.0
    } else {
        90.0 - (x.acos() / k.acos_0_5) * 60.0
    }
}

fn acosd_q1(x: f64, k: &DegConsts) -> f64 {
    if x <= 0.5 {
        90.0 - (x.asin() / k.asin_0_5) * 30.0
    } else {
        (x.acos() / k.acos_0_5) * 60.0
    }
}

fn c_dacosd(x: f64) -> Result<f64> {
    if x.is_nan() {
        return Ok(f64::NAN);
    }
    let k = deg_consts();
    if !(-1.0..=1.0).contains(&x) {
        return Err(input_out_of_range());
    }
    let r = if x >= 0.0 { acosd_q1(x, k) } else { 90.0 + asind_q1(-x, k) };
    if r.is_infinite() {
        return Err(overflow());
    }
    Ok(r)
}

fn c_dasind(x: f64) -> Result<f64> {
    if x.is_nan() {
        return Ok(f64::NAN);
    }
    let k = deg_consts();
    if !(-1.0..=1.0).contains(&x) {
        return Err(input_out_of_range());
    }
    let r = if x >= 0.0 { asind_q1(x, k) } else { -asind_q1(-x, k) };
    if r.is_infinite() {
        return Err(overflow());
    }
    Ok(r)
}

fn c_datand(x: f64) -> Result<f64> {
    if x.is_nan() {
        return Ok(f64::NAN);
    }
    let k = deg_consts();
    let r = (x.atan() / k.atan_1_0) * 45.0;
    if r.is_infinite() {
        return Err(overflow());
    }
    Ok(r)
}

fn c_datan2d(y: f64, x: f64) -> Result<f64> {
    if y.is_nan() || x.is_nan() {
        return Ok(f64::NAN);
    }
    let k = deg_consts();
    let r = (y.atan2(x) / k.atan_1_0) * 45.0;
    if r.is_infinite() {
        return Err(overflow());
    }
    Ok(r)
}

// The reductions below follow PG's branch structure exactly. sin/tan/cot flip the sign for a negative
// angle and for an angle past 180; cos does neither (cos(-a) = cos(a), cos(360-a) = cos(a)). Every
// variant flips the sign when the reduced angle is past 90, because the quadrant mirror changes it.

fn c_dsind(x: f64) -> Result<f64> {
    if x.is_nan() {
        return Ok(f64::NAN);
    }
    if x.is_infinite() {
        return Err(input_out_of_range());
    }
    let k = deg_consts();
    let mut a = x % 360.0;
    let mut sign = 1.0;
    if a < 0.0 {
        a = -a;
        sign = -sign;
    }
    if a > 180.0 {
        a = 360.0 - a;
        sign = -sign;
    }
    if a > 90.0 {
        a = 180.0 - a;
    }
    let r = sign * sind_q1(a, k);
    if r.is_infinite() {
        return Err(overflow());
    }
    Ok(r)
}

fn c_dcosd(x: f64) -> Result<f64> {
    if x.is_nan() {
        return Ok(f64::NAN);
    }
    if x.is_infinite() {
        return Err(input_out_of_range());
    }
    let k = deg_consts();
    let mut a = x % 360.0;
    let mut sign = 1.0;
    if a < 0.0 {
        a = -a;
    }
    if a > 180.0 {
        a = 360.0 - a;
    }
    if a > 90.0 {
        a = 180.0 - a;
        sign = -sign;
    }
    let r = sign * cosd_q1(a, k);
    if r.is_infinite() {
        return Err(overflow());
    }
    Ok(r)
}

fn c_dtand(x: f64) -> Result<f64> {
    if x.is_nan() {
        return Ok(f64::NAN);
    }
    if x.is_infinite() {
        return Err(input_out_of_range());
    }
    let k = deg_consts();
    let mut a = x % 360.0;
    let mut sign = 1.0;
    if a < 0.0 {
        a = -a;
        sign = -sign;
    }
    if a > 180.0 {
        a = 360.0 - a;
        sign = -sign;
    }
    if a > 90.0 {
        a = 180.0 - a;
        sign = -sign;
    }
    let tan_a = sind_q1(a, k) / cosd_q1(a, k);
    let r = sign * (tan_a / k.tan_45);
    Ok(if r == 0.0 { 0.0 } else { r })
}

fn c_dcotd(x: f64) -> Result<f64> {
    if x.is_nan() {
        return Ok(f64::NAN);
    }
    if x.is_infinite() {
        return Err(input_out_of_range());
    }
    let k = deg_consts();
    let mut a = x % 360.0;
    let mut sign = 1.0;
    if a < 0.0 {
        a = -a;
        sign = -sign;
    }
    if a > 180.0 {
        a = 360.0 - a;
        sign = -sign;
    }
    if a > 90.0 {
        a = 180.0 - a;
        sign = -sign;
    }
    let cot_a = cosd_q1(a, k) / sind_q1(a, k);
    let r = sign * (cot_a / k.cot_45);
    Ok(if r == 0.0 { 0.0 } else { r })
}

// ---------------------------------------------------------------------------------------------
// Hyperbolic functions and special functions
// ---------------------------------------------------------------------------------------------

fn c_dsinh(x: f64) -> Result<f64> {
    // ERANGE in PG maps to +/-Infinity, which is what Rust's sinh already returns.
    Ok(x.sinh())
}

fn c_dcosh(x: f64) -> Result<f64> {
    let r = x.cosh();
    if r == 0.0 {
        return Err(underflow());
    }
    Ok(r)
}

fn c_dtanh(x: f64) -> Result<f64> {
    let r = x.tanh();
    if r.is_infinite() {
        return Err(overflow());
    }
    Ok(r)
}

fn c_dasinh(x: f64) -> Result<f64> {
    Ok(x.asinh())
}

fn c_dacosh(x: f64) -> Result<f64> {
    if x < 1.0 {
        return Err(input_out_of_range());
    }
    Ok(x.acosh())
}

fn c_datanh(x: f64) -> Result<f64> {
    // NaN passes PG's `< -1 || > 1` test and yields NaN.
    if x < -1.0 || x > 1.0 {
        return Err(input_out_of_range());
    }
    if x == -1.0 {
        return Ok(f64::NEG_INFINITY);
    }
    if x == 1.0 {
        return Ok(f64::INFINITY);
    }
    Ok(x.atanh())
}

fn c_derf(x: f64) -> Result<f64> {
    let r = erf(x);
    if r.is_infinite() {
        return Err(overflow());
    }
    Ok(r)
}

fn c_derfc(x: f64) -> Result<f64> {
    let r = erfc(x);
    if r.is_infinite() {
        return Err(overflow());
    }
    Ok(r)
}

fn c_dgamma(x: f64) -> Result<f64> {
    if x.is_nan() {
        return Ok(x);
    }
    if x.is_infinite() {
        if x < 0.0 {
            return Err(overflow());
        }
        return Ok(x);
    }
    let r = tgamma(x);
    if r.is_infinite() || r.is_nan() {
        return Err(overflow());
    }
    if r == 0.0 {
        return Err(underflow());
    }
    Ok(r)
}

fn c_dlgamma(x: f64) -> Result<f64> {
    let r = lgamma(x);
    if r.is_infinite() && !x.is_infinite() {
        return Err(overflow());
    }
    Ok(r)
}

// ---------------------------------------------------------------------------------------------
// Unary and binary wrappers for the math functions
// ---------------------------------------------------------------------------------------------

un! {
    dround: arg_f8 => Float8 = c_dround;
    dtrunc: arg_f8 => Float8 = c_dtrunc;
    dceil: arg_f8 => Float8 = c_dceil;
    dfloor: arg_f8 => Float8 = c_dfloor;
    dsign: arg_f8 => Float8 = c_dsign;
    dsqrt: arg_f8 => Float8 = c_dsqrt;
    dcbrt: arg_f8 => Float8 = c_dcbrt;
    dexp: arg_f8 => Float8 = c_dexp;
    dlog1: arg_f8 => Float8 = c_dlog1;
    dlog10: arg_f8 => Float8 = c_dlog10;
    degrees: arg_f8 => Float8 = c_degrees;
    radians: arg_f8 => Float8 = c_radians;
    dacos: arg_f8 => Float8 = c_dacos;
    dasin: arg_f8 => Float8 = c_dasin;
    datan: arg_f8 => Float8 = c_datan;
    dcos: arg_f8 => Float8 = c_dcos;
    dsin: arg_f8 => Float8 = c_dsin;
    dtan: arg_f8 => Float8 = c_dtan;
    dcot: arg_f8 => Float8 = c_dcot;
    dacosd: arg_f8 => Float8 = c_dacosd;
    dasind: arg_f8 => Float8 = c_dasind;
    datand: arg_f8 => Float8 = c_datand;
    dcosd: arg_f8 => Float8 = c_dcosd;
    dsind: arg_f8 => Float8 = c_dsind;
    dtand: arg_f8 => Float8 = c_dtand;
    dcotd: arg_f8 => Float8 = c_dcotd;
    dsinh: arg_f8 => Float8 = c_dsinh;
    dcosh: arg_f8 => Float8 = c_dcosh;
    dtanh: arg_f8 => Float8 = c_dtanh;
    dasinh: arg_f8 => Float8 = c_dasinh;
    dacosh: arg_f8 => Float8 = c_dacosh;
    datanh: arg_f8 => Float8 = c_datanh;
    derf: arg_f8 => Float8 = c_derf;
    derfc: arg_f8 => Float8 = c_derfc;
    dgamma: arg_f8 => Float8 = c_dgamma;
    dlgamma: arg_f8 => Float8 = c_dlgamma;
}

bin! {
    dpow: arg_f8, arg_f8 => Float8 = c_dpow;
    datan2: arg_f8, arg_f8 => Float8 = c_datan2;
    datan2d: arg_f8, arg_f8 => Float8 = c_datan2d;
}

fn dpi(_a: &[Datum], _ctx: &FuncCtx) -> Result<Datum> {
    Ok(Datum::Float8(M_PI))
}

// ---------------------------------------------------------------------------------------------
// Random numbers (session PRNG; see `SessionContext`)
// ---------------------------------------------------------------------------------------------

fn drandom(_a: &[Datum], ctx: &FuncCtx) -> Result<Datum> {
    Ok(Datum::Float8(ctx.session.random_f64()))
}

/// PG `drandom_normal(mean, stddev)`: Box-Muller from two uniform draws in (0, 1] and [0, 1).
fn drandom_normal(a: &[Datum], ctx: &FuncCtx) -> Result<Datum> {
    let mean = arg_f8(a, 0)?;
    let stddev = arg_f8(a, 1)?;
    let u1 = 1.0 - ctx.session.random_f64();
    let u2 = ctx.session.random_f64();
    let z = (-2.0 * u1.ln()).sqrt() * (2.0 * M_PI * u2).cos();
    Ok(Datum::Float8((stddev * z) + mean))
}

fn setseed(a: &[Datum], ctx: &FuncCtx) -> Result<Datum> {
    let seed = arg_f8(a, 0)?;
    if seed < -1.0 || seed > 1.0 || seed.is_nan() {
        return Err(PgError::new(
            SqlState::INVALID_PARAMETER_VALUE,
            format!("setseed parameter {} is out of allowed range [-1,1]", fmt_g(seed)),
        ));
    }
    ctx.session.set_seed(seed);
    Ok(Datum::Null)
}

/// C `printf("%g", v)` with the default precision of 6.
fn fmt_g(v: f64) -> String {
    if v.is_nan() {
        return if v.is_sign_negative() { "-nan".into() } else { "nan".into() };
    }
    if v.is_infinite() {
        return if v < 0.0 { "-inf".into() } else { "inf".into() };
    }
    if v == 0.0 {
        return if v.is_sign_negative() { "-0".into() } else { "0".into() };
    }
    let sci = format!("{:.5e}", v);
    let Some((mant, exp)) = sci.split_once('e') else {
        return format!("{v}");
    };
    let x: i32 = exp.parse().unwrap_or(0);
    if (-4..6).contains(&x) {
        let decimals = (5 - x) as usize;
        trim_fraction_zeros(&format!("{:.*}", decimals, v))
    } else {
        let sign = if x < 0 { '-' } else { '+' };
        format!("{}e{}{:02}", trim_fraction_zeros(mant), sign, x.unsigned_abs())
    }
}

fn trim_fraction_zeros(s: &str) -> String {
    if s.contains('.') {
        s.trim_end_matches('0').trim_end_matches('.').to_string()
    } else {
        s.to_string()
    }
}

// ---------------------------------------------------------------------------------------------
// Hashing (PG hashfunc.c): equal values hash equal, -0 and 0 hash as 0, all NaNs hash as NaN
// ---------------------------------------------------------------------------------------------

fn hashfloat4(a: &[Datum], _ctx: &FuncCtx) -> Result<Datum> {
    let key = arg_f4(a, 0)?;
    if key == 0.0 {
        return Ok(Datum::Int4(0));
    }
    // Widen to float8 so cross-type hashing matches hashfloat8.
    let mut key8 = f64::from(key);
    if key8.is_nan() {
        key8 = f64::NAN;
    }
    Ok(Datum::Int4(compare::pg_hash_bytes(&key8.to_ne_bytes())? as i32))
}

fn hashfloat4extended(a: &[Datum], _ctx: &FuncCtx) -> Result<Datum> {
    let key = arg_f4(a, 0)?;
    let seed = arg_i8(a, 1)? as u64;
    if key == 0.0 {
        return Ok(Datum::Int8(seed as i64));
    }
    let mut key8 = f64::from(key);
    if key8.is_nan() {
        key8 = f64::NAN;
    }
    Ok(Datum::Int8(compare::pg_hash_bytes_extended(&key8.to_ne_bytes(), seed)? as i64))
}

fn hashfloat8(a: &[Datum], _ctx: &FuncCtx) -> Result<Datum> {
    let mut key = arg_f8(a, 0)?;
    if key == 0.0 {
        return Ok(Datum::Int4(0));
    }
    if key.is_nan() {
        key = f64::NAN;
    }
    Ok(Datum::Int4(compare::pg_hash_bytes(&key.to_ne_bytes())? as i32))
}

fn hashfloat8extended(a: &[Datum], _ctx: &FuncCtx) -> Result<Datum> {
    let mut key = arg_f8(a, 0)?;
    let seed = arg_i8(a, 1)? as u64;
    if key == 0.0 {
        return Ok(Datum::Int8(seed as i64));
    }
    if key.is_nan() {
        key = f64::NAN;
    }
    Ok(Datum::Int8(compare::pg_hash_bytes_extended(&key.to_ne_bytes(), seed)? as i64))
}

// ---------------------------------------------------------------------------------------------
// width_bucket and in_range (window frame offsets)
// ---------------------------------------------------------------------------------------------

fn bucket_err(msg: &'static str) -> PgError {
    err(SqlState(*b"2201G"), msg)
}

fn bucket_overflow() -> PgError {
    err(SqlState::NUMERIC_VALUE_OUT_OF_RANGE, "integer out of range")
}

/// PG `width_bucket_float8`.
fn c_width_bucket(operand: f64, bound1: f64, bound2: f64, count: i32) -> Result<i32> {
    if count <= 0 {
        return Err(bucket_err("count must be greater than zero"));
    }
    if operand.is_nan() || bound1.is_nan() || bound2.is_nan() {
        return Err(bucket_err("operand, lower bound, and upper bound cannot be NaN"));
    }
    // An infinite operand is allowed; the bounds must be finite.
    if bound1.is_infinite() || bound2.is_infinite() {
        return Err(bucket_err("lower and upper bounds must be finite"));
    }
    if bound1 < bound2 {
        if operand < bound1 {
            return Ok(0);
        }
        if operand >= bound2 {
            return count.checked_add(1).ok_or_else(bucket_overflow);
        }
        let q = if !(bound2 - bound1).is_infinite() {
            f64::from(count) * ((operand - bound1) / (bound2 - bound1))
        } else {
            f64::from(count) * ((operand / 2.0 - bound1 / 2.0) / (bound2 / 2.0 - bound1 / 2.0))
        };
        let mut result = q as i32;
        if result >= count {
            result = count - 1;
        }
        Ok(result + 1)
    } else if bound1 > bound2 {
        if operand > bound1 {
            return Ok(0);
        }
        if operand <= bound2 {
            return count.checked_add(1).ok_or_else(bucket_overflow);
        }
        let q = if !(bound1 - bound2).is_infinite() {
            f64::from(count) * ((bound1 - operand) / (bound1 - bound2))
        } else {
            f64::from(count) * ((bound1 / 2.0 - operand / 2.0) / (bound1 / 2.0 - bound2 / 2.0))
        };
        let mut result = q as i32;
        if result >= count {
            result = count - 1;
        }
        Ok(result + 1)
    } else {
        Err(bucket_err("lower bound cannot equal upper bound"))
    }
}

fn width_bucket_float8(a: &[Datum], _ctx: &FuncCtx) -> Result<Datum> {
    let operand = arg_f8(a, 0)?;
    let bound1 = arg_f8(a, 1)?;
    let bound2 = arg_f8(a, 2)?;
    let count = arg_i4(a, 3)?;
    Ok(Datum::Int4(c_width_bucket(operand, bound1, bound2, count)?))
}

/// PG `in_range_float8_float8` / `in_range_float4_float8` (both widen to float8 first).
fn c_in_range(val: f64, base: f64, offset: f64, sub: bool, less: bool) -> Result<bool> {
    if offset.is_nan() || offset < 0.0 {
        return Err(err(
            SqlState(*b"22013"),
            "invalid preceding or following size in window function",
        ));
    }
    if val.is_nan() {
        // NaN = NaN, and NaN is greater than any non-NaN.
        return Ok(if base.is_nan() { true } else { !less });
    } else if base.is_nan() {
        return Ok(less);
    }
    if offset.is_infinite() && base.is_infinite() && (if sub { base > 0.0 } else { base < 0.0 }) {
        return Ok(true);
    }
    let sum = if sub { base - offset } else { base + offset };
    Ok(if less { val <= sum } else { val >= sum })
}

fn in_range_float8_float8(a: &[Datum], _ctx: &FuncCtx) -> Result<Datum> {
    let r = c_in_range(arg_f8(a, 0)?, arg_f8(a, 1)?, arg_f8(a, 2)?, arg_bool(a, 3)?, arg_bool(a, 4)?)?;
    Ok(Datum::Bool(r))
}

fn in_range_float4_float8(a: &[Datum], _ctx: &FuncCtx) -> Result<Datum> {
    let r = c_in_range(arg_f4w(a, 0)?, arg_f4w(a, 1)?, arg_f8(a, 2)?, arg_bool(a, 3)?, arg_bool(a, 4)?)?;
    Ok(Datum::Bool(r))
}

// ---------------------------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::context::{IoConfig, TestSession};
    use crate::datum::TypeId;
    use crate::registry::builtin_env;

    /// Call builtin proc `oid` through the public dispatcher, as the executor does.
    fn run(oid: u32, args: &[Datum]) -> Result<Datum> {
        let env = builtin_env();
        let cfg = IoConfig::default();
        let sess = TestSession::default();
        let ctx = FuncCtx {
            env,
            io: &cfg,
            session: &sess,
            arg_types: &[],
            result_type: TypeId::FLOAT8,
            collation: 0,
        };
        crate::func::call(oid, args, &ctx)
    }

    fn expect_err(r: Result<Datum>, code: &str, msg: &str) {
        match r {
            Err(e) => {
                assert_eq!(e.sqlstate.code(), code, "sqlstate for {msg}");
                assert_eq!(e.message, msg);
            }
            Ok(v) => panic!("expected error {code} {msg}, got {v:?}"),
        }
    }

    fn f8(r: Result<Datum>) -> f64 {
        match r {
            Ok(Datum::Float8(v)) => v,
            other => panic!("expected float8, got {other:?}"),
        }
    }

    #[test]
    fn overflow_and_underflow() {
        expect_err(run(216, &[Datum::Float8(1e308), Datum::Float8(10.0)]), "22003", "value out of range: overflow");
        expect_err(
            run(216, &[Datum::Float8(1e-300), Datum::Float8(1e-300)]),
            "22003",
            "value out of range: underflow",
        );
        // Subnormal results are allowed.
        assert!(f8(run(216, &[Datum::Float8(1e-308), Datum::Float8(1e-10)])) > 0.0);
    }

    #[test]
    fn division_rules() {
        expect_err(run(217, &[Datum::Float8(0.0), Datum::Float8(0.0)]), "22012", "division by zero");
        assert!(f8(run(217, &[Datum::Float8(f64::NAN), Datum::Float8(0.0)])).is_nan());
    }

    #[test]
    fn float_to_int_rounds_half_even() {
        let int4 = |v: f64| match run(317, &[Datum::Float8(v)]) {
            Ok(Datum::Int4(i)) => i,
            other => panic!("expected int4, got {other:?}"),
        };
        assert_eq!(int4(2.5), 2);
        assert_eq!(int4(-2.5), -2);
        assert_eq!(int4(1.5), 2);
        expect_err(run(317, &[Datum::Float8(f64::NAN)]), "22003", "integer out of range");
    }

    #[test]
    fn sqrt_and_power_errors() {
        expect_err(
            run(230, &[Datum::Float8(-1.0)]),
            "2201F",
            "cannot take square root of a negative number",
        );
        expect_err(
            run(232, &[Datum::Float8(0.0), Datum::Float8(-1.0)]),
            "2201F",
            "zero raised to a negative power is undefined",
        );
    }

    #[test]
    fn comparison_nan_rules() {
        // float8gt(NaN, inf) = true
        match run(297, &[Datum::Float8(f64::NAN), Datum::Float8(f64::INFINITY)]) {
            Ok(Datum::Bool(b)) => assert!(b),
            other => panic!("expected bool, got {other:?}"),
        }
    }

    #[test]
    fn round_keeps_negative_zero() {
        let r = f8(run(228, &[Datum::Float8(-0.5)]));
        assert_eq!(r, 0.0);
        assert!(r.is_sign_negative());
    }

    #[test]
    fn degree_trig_exact_points() {
        // dsind(30) and dcosd(60) are exactly 0.5 through the shared scaling constants.
        assert_eq!(f8(run(2735, &[Datum::Float8(30.0)])), 0.5);
        assert_eq!(f8(run(2736, &[Datum::Float8(60.0)])), 0.5);
        assert_eq!(f8(run(2735, &[Datum::Float8(180.0)])), 0.0);
    }

    #[test]
    fn setseed_range_message() {
        expect_err(
            run(1599, &[Datum::Float8(2.0)]),
            "22023",
            "setseed parameter 2 is out of allowed range [-1,1]",
        );
    }

    #[test]
    fn printf_g_format() {
        assert_eq!(fmt_g(2.0), "2");
        assert_eq!(fmt_g(1e-5), "1e-05");
        assert_eq!(fmt_g(123456789.0), "1.23457e+08");
        assert_eq!(fmt_g(-2.5), "-2.5");
    }

    #[test]
    fn hash_zero_and_negative_zero_agree() {
        let h = |v: f64| match run(452, &[Datum::Float8(v)]) {
            Ok(Datum::Int4(i)) => i,
            other => panic!("expected int4, got {other:?}"),
        };
        assert_eq!(h(0.0), 0);
        assert_eq!(h(-0.0), 0);
    }

    #[test]
    fn in_range_nan_and_offset_rules() {
        assert!(c_in_range(f64::NAN, f64::NAN, 1.0, false, false).unwrap());
        assert!(!c_in_range(f64::NAN, 1.0, 1.0, false, true).unwrap());
        assert!(c_in_range(5.0, 10.0, 5.0, true, false).unwrap());
        assert!(!c_in_range(4.0, 10.0, 5.0, true, false).unwrap());
        assert!(c_in_range(1.0, 1.0, -1.0, false, false).is_err());
    }

    #[test]
    fn width_bucket_basics() {
        assert_eq!(c_width_bucket(5.0, 0.0, 10.0, 10).unwrap(), 6);
        assert_eq!(c_width_bucket(-1.0, 0.0, 10.0, 10).unwrap(), 0);
        assert_eq!(c_width_bucket(10.0, 0.0, 10.0, 10).unwrap(), 11);
        assert!(c_width_bucket(1.0, 2.0, 2.0, 3).is_err());
        assert!(c_width_bucket(1.0, 0.0, 10.0, 0).is_err());
    }

    #[test]
    fn lookup_covers_every_module_proc() {
        // Every catalog row owned by this module must resolve to a scalar implementation.
        let mut owned = 0usize;
        for p in crate::catalog_data::PROCS.iter().filter(|p| p.module == crate::catalog_data::ImplModule::FloatOps) {
            owned += 1;
            assert!(lookup(p).is_some(), "no implementation for {} ({})", p.name, p.prosrc);
        }
        assert_eq!(owned, 131);
    }
}
