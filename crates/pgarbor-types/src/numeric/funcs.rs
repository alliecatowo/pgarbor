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
use super::io::format_numeric;
use super::{arith, transcendental, Numeric};
use crate::catalog_data::BuiltinProc;
use crate::compare::{pg_hash_bytes, pg_hash_bytes_extended};
use crate::context::FuncCtx;
use crate::datum::Datum;
use crate::func::{FuncImpl, ScalarFn};
use bigdecimal::{BigDecimal, RoundingMode, ToPrimitive, Zero};
use num_bigint::{BigInt, Sign};
use pgarbor_common::{PgError, Result, SqlState};
use std::cmp::Ordering;
use std::str::FromStr;

const NUMERIC_VALUE_OUT_OF_RANGE: SqlState = SqlState(*b"22003");
const FEATURE_NOT_SUPPORTED: SqlState = SqlState(*b"0A000");
const INVALID_PARAMETER_VALUE: SqlState = SqlState(*b"22023");
const INVALID_PRECEDING_OR_FOLLOWING_SIZE: SqlState = SqlState(*b"22013");
const INVALID_ARGUMENT_FOR_WIDTH_BUCKET: SqlState = SqlState(*b"2201G");

/// Largest n with a representable `factorial(n)` (PG `numeric_fac` limit).
const FACTORIAL_MAX: i64 = 32177;

/// numeric → i64 (half away from zero), PG errors per module docs.
// CONTRACT
pub fn to_i64(n: &Numeric) -> Result<i64> {
    let v = int_from_numeric(n, "bigint")?;
    v.to_i64().ok_or_else(|| PgError::new(NUMERIC_VALUE_OUT_OF_RANGE, "bigint out of range"))
}

/// numeric → i32.
// CONTRACT
pub fn to_i32(n: &Numeric) -> Result<i32> {
    let v = int_from_numeric(n, "integer")?;
    v.to_i32().ok_or_else(|| PgError::new(NUMERIC_VALUE_OUT_OF_RANGE, "integer out of range"))
}

/// numeric → i16.
// CONTRACT
pub fn to_i16(n: &Numeric) -> Result<i16> {
    let v = int_from_numeric(n, "smallint")?;
    v.to_i16().ok_or_else(|| PgError::new(NUMERIC_VALUE_OUT_OF_RANGE, "smallint out of range"))
}

/// numeric → f64 (`numeric_float8`).
// CONTRACT
pub fn to_f64(n: &Numeric) -> Result<f64> {
    match n {
        Numeric::NaN => Ok(f64::NAN),
        Numeric::PosInf => Ok(f64::INFINITY),
        Numeric::NegInf => Ok(f64::NEG_INFINITY),
        Numeric::Finite(d) => {
            let text = format_numeric(n)?;
            let v = text
                .parse::<f64>()
                .map_err(|_| PgError::internal(format!("cannot parse numeric text {text} as double precision")))?;
            // float8in_internal: an infinite result, or a zero from a nonzero input, is out of range.
            if v.is_infinite() || (v == 0.0 && !d.is_zero()) {
                return Err(PgError::new(
                    NUMERIC_VALUE_OUT_OF_RANGE,
                    format!("\"{text}\" is out of range for type double precision"),
                ));
            }
            Ok(v)
        }
    }
}

/// f64 → numeric (`float8_numeric`, 15 significant digits).
// CONTRACT
pub fn from_f64(v: f64) -> Result<Numeric> {
    if v.is_nan() {
        return Ok(Numeric::NaN);
    }
    if v.is_infinite() {
        return Ok(if v < 0.0 { Numeric::NegInf } else { Numeric::PosInf });
    }
    numeric_from_g(v, 15)
}

/// f32 → numeric (`float4_numeric`, 6 significant digits).
// CONTRACT
pub fn from_f32(v: f32) -> Result<Numeric> {
    if v.is_nan() {
        return Ok(Numeric::NaN);
    }
    if v.is_infinite() {
        return Ok(if v < 0.0 { Numeric::NegInf } else { Numeric::PosInf });
    }
    numeric_from_g(f64::from(v), 6)
}

/// Function registry hook (see `crate::func`).
pub(crate) fn lookup(proc: &BuiltinProc) -> Option<FuncImpl> {
    // SQL-language builtins have no C symbol: they are matched by OID.
    let f: ScalarFn = match proc.oid {
        1708 => f_round_sql,
        1710 => f_trunc_sql,
        1481 | 1741 => f_log10,
        _ => match proc.prosrc {
            "hash_numeric" => f_hash_numeric,
            "hash_numeric_extended" => f_hash_numeric_extended,
            "numeric_fac" => f_factorial,
            "numeric_abs" => f_abs,
            "numeric_sign" => f_sign,
            "numeric_round" => f_round,
            "numeric_trunc" => f_trunc,
            "numeric_ceil" => f_ceil,
            "numeric_floor" => f_floor,
            "numeric_eq" => f_eq,
            "numeric_ne" => f_ne,
            "numeric_gt" => f_gt,
            "numeric_ge" => f_ge,
            "numeric_lt" => f_lt,
            "numeric_le" => f_le,
            "numeric_add" => f_add,
            "numeric_sub" => f_sub,
            "numeric_mul" => f_mul,
            "numeric_div" => f_div,
            "numeric_mod" => f_mod,
            "numeric_div_trunc" => f_div_trunc,
            "numeric_uminus" => f_uminus,
            "numeric_uplus" => f_uplus,
            "numeric_sqrt" => f_sqrt,
            "numeric_exp" => f_exp,
            "numeric_ln" => f_ln,
            "numeric_log" => f_log,
            "numeric_power" => f_power,
            "numeric_smaller" => f_smaller,
            "numeric_larger" => f_larger,
            "numeric_cmp" => f_cmp,
            "int2_numeric" => f_int2_numeric,
            "int4_numeric" => f_int4_numeric,
            "int8_numeric" => f_int8_numeric,
            "float4_numeric" => f_float4_numeric,
            "float8_numeric" => f_float8_numeric,
            "numeric_int2" => f_numeric_int2,
            "numeric_int4" => f_numeric_int4,
            "numeric_int8" => f_numeric_int8,
            "numeric_float4" => f_numeric_float4,
            "numeric_float8" => f_numeric_float8,
            "width_bucket_numeric" => f_width_bucket,
            "numeric_scale" => f_scale,
            "numeric_min_scale" => f_min_scale,
            "numeric_trim_scale" => f_trim_scale,
            "in_range_numeric_numeric" => f_in_range,
            "numeric_gcd" => f_gcd,
            "numeric_lcm" => f_lcm,
            "numeric_random" => f_random,
            _ => return None,
        },
    };
    Some(FuncImpl::Scalar(f))
}

/// 0A000 for a Numeric-module proc without implementation.
pub(crate) fn unsupported(proc: &BuiltinProc) -> PgError {
    PgError::unimplemented("TYP-12", format!("function {}", proc.name))
}

// ---------------------------------------------------------------------------------------------
// Argument access helpers. Arguments arrive already coerced to the declared types, so a mismatch
// is an internal error (XX000), never a panic.
// ---------------------------------------------------------------------------------------------

fn arg(args: &[Datum], i: usize) -> Result<&Datum> {
    match args.get(i) {
        Some(d) => Ok(d),
        None => Err(PgError::internal(format!("numeric function: missing argument {i}"))),
    }
}

fn bad_arg(expected: &str, got: &Datum) -> PgError {
    PgError::internal(format!("numeric function: expected {expected} argument, got {got:?}"))
}

fn nm(d: &Datum) -> Result<&Numeric> {
    match d {
        Datum::Numeric(n) => Ok(n),
        other => Err(bad_arg("numeric", other)),
    }
}

fn i16_of(d: &Datum) -> Result<i16> {
    match d {
        Datum::Int2(v) => Ok(*v),
        other => Err(bad_arg("smallint", other)),
    }
}

fn i32_of(d: &Datum) -> Result<i32> {
    match d {
        Datum::Int4(v) => Ok(*v),
        other => Err(bad_arg("integer", other)),
    }
}

fn i64_of(d: &Datum) -> Result<i64> {
    match d {
        Datum::Int8(v) => Ok(*v),
        other => Err(bad_arg("bigint", other)),
    }
}

fn f32_of(d: &Datum) -> Result<f32> {
    match d {
        Datum::Float4(v) => Ok(*v),
        other => Err(bad_arg("real", other)),
    }
}

fn f64_of(d: &Datum) -> Result<f64> {
    match d {
        Datum::Float8(v) => Ok(*v),
        other => Err(bad_arg("double precision", other)),
    }
}

fn bool_of(d: &Datum) -> Result<bool> {
    match d {
        Datum::Bool(v) => Ok(*v),
        other => Err(bad_arg("boolean", other)),
    }
}

macro_rules! num_binary {
    ($name:ident, $op:path) => {
        fn $name(args: &[Datum], _ctx: &FuncCtx) -> Result<Datum> {
            let a = nm(arg(args, 0)?)?;
            let b = nm(arg(args, 1)?)?;
            Ok(Datum::Numeric($op(a, b)?))
        }
    };
}

macro_rules! num_unary {
    ($name:ident, $op:path) => {
        fn $name(args: &[Datum], _ctx: &FuncCtx) -> Result<Datum> {
            let a = nm(arg(args, 0)?)?;
            Ok(Datum::Numeric($op(a)?))
        }
    };
}

macro_rules! num_compare {
    ($name:ident, $pred:expr) => {
        fn $name(args: &[Datum], _ctx: &FuncCtx) -> Result<Datum> {
            let a = nm(arg(args, 0)?)?;
            let b = nm(arg(args, 1)?)?;
            let pred: fn(Ordering) -> bool = $pred;
            Ok(Datum::Bool(pred(a.pg_cmp(b))))
        }
    };
}

num_binary!(f_add, arith::add);
num_binary!(f_sub, arith::sub);
num_binary!(f_mul, arith::mul);
num_binary!(f_div, arith::div);
num_binary!(f_mod, arith::modulo);
num_binary!(f_div_trunc, arith::div_trunc);
num_binary!(f_power, transcendental::power);
num_binary!(f_log, transcendental::log);

num_unary!(f_uminus, arith::neg);
num_unary!(f_abs, arith::abs);
num_unary!(f_sign, arith::sign);
num_unary!(f_ceil, arith::ceil);
num_unary!(f_floor, arith::floor);
num_unary!(f_sqrt, transcendental::sqrt);
num_unary!(f_exp, transcendental::exp);
num_unary!(f_ln, transcendental::ln);
num_unary!(f_log10, transcendental::log10);

num_compare!(f_eq, |o: Ordering| o == Ordering::Equal);
num_compare!(f_ne, |o: Ordering| o != Ordering::Equal);
num_compare!(f_gt, |o: Ordering| o == Ordering::Greater);
num_compare!(f_ge, |o: Ordering| o != Ordering::Less);
num_compare!(f_lt, |o: Ordering| o == Ordering::Less);
num_compare!(f_le, |o: Ordering| o != Ordering::Greater);

fn f_uplus(args: &[Datum], _ctx: &FuncCtx) -> Result<Datum> {
    Ok(Datum::Numeric(nm(arg(args, 0)?)?.clone()))
}

fn f_round(args: &[Datum], _ctx: &FuncCtx) -> Result<Datum> {
    let a = nm(arg(args, 0)?)?;
    let s = i32_of(arg(args, 1)?)?;
    Ok(Datum::Numeric(arith::round(a, s)?))
}

fn f_trunc(args: &[Datum], _ctx: &FuncCtx) -> Result<Datum> {
    let a = nm(arg(args, 0)?)?;
    let s = i32_of(arg(args, 1)?)?;
    Ok(Datum::Numeric(arith::trunc(a, s)?))
}

/// SQL-language `round(numeric)` = round(x, 0).
fn f_round_sql(args: &[Datum], _ctx: &FuncCtx) -> Result<Datum> {
    let a = nm(arg(args, 0)?)?;
    Ok(Datum::Numeric(arith::round(a, 0)?))
}

/// SQL-language `trunc(numeric)` = trunc(x, 0).
fn f_trunc_sql(args: &[Datum], _ctx: &FuncCtx) -> Result<Datum> {
    let a = nm(arg(args, 0)?)?;
    Ok(Datum::Numeric(arith::trunc(a, 0)?))
}

fn f_smaller(args: &[Datum], _ctx: &FuncCtx) -> Result<Datum> {
    let a = nm(arg(args, 0)?)?;
    let b = nm(arg(args, 1)?)?;
    Ok(Datum::Numeric(if a.pg_cmp(b) == Ordering::Less { a.clone() } else { b.clone() }))
}

fn f_larger(args: &[Datum], _ctx: &FuncCtx) -> Result<Datum> {
    let a = nm(arg(args, 0)?)?;
    let b = nm(arg(args, 1)?)?;
    Ok(Datum::Numeric(if a.pg_cmp(b) == Ordering::Greater { a.clone() } else { b.clone() }))
}

fn f_cmp(args: &[Datum], _ctx: &FuncCtx) -> Result<Datum> {
    let a = nm(arg(args, 0)?)?;
    let b = nm(arg(args, 1)?)?;
    let v = match a.pg_cmp(b) {
        Ordering::Less => -1,
        Ordering::Equal => 0,
        Ordering::Greater => 1,
    };
    Ok(Datum::Int4(v))
}

// ---------------------------------------------------------------------------------------------
// Conversions
// ---------------------------------------------------------------------------------------------

fn f_int2_numeric(args: &[Datum], _ctx: &FuncCtx) -> Result<Datum> {
    Ok(Datum::Numeric(Numeric::from_i64(i64::from(i16_of(arg(args, 0)?)?))))
}

fn f_int4_numeric(args: &[Datum], _ctx: &FuncCtx) -> Result<Datum> {
    Ok(Datum::Numeric(Numeric::from_i64(i64::from(i32_of(arg(args, 0)?)?))))
}

fn f_int8_numeric(args: &[Datum], _ctx: &FuncCtx) -> Result<Datum> {
    Ok(Datum::Numeric(Numeric::from_i64(i64_of(arg(args, 0)?)?)))
}

fn f_float4_numeric(args: &[Datum], _ctx: &FuncCtx) -> Result<Datum> {
    Ok(Datum::Numeric(from_f32(f32_of(arg(args, 0)?)?)?))
}

fn f_float8_numeric(args: &[Datum], _ctx: &FuncCtx) -> Result<Datum> {
    Ok(Datum::Numeric(from_f64(f64_of(arg(args, 0)?)?)?))
}

fn f_numeric_int2(args: &[Datum], _ctx: &FuncCtx) -> Result<Datum> {
    Ok(Datum::Int2(to_i16(nm(arg(args, 0)?)?)?))
}

fn f_numeric_int4(args: &[Datum], _ctx: &FuncCtx) -> Result<Datum> {
    Ok(Datum::Int4(to_i32(nm(arg(args, 0)?)?)?))
}

fn f_numeric_int8(args: &[Datum], _ctx: &FuncCtx) -> Result<Datum> {
    Ok(Datum::Int8(to_i64(nm(arg(args, 0)?)?)?))
}

fn f_numeric_float4(args: &[Datum], _ctx: &FuncCtx) -> Result<Datum> {
    Ok(Datum::Float4(to_f32(nm(arg(args, 0)?)?)?))
}

fn f_numeric_float8(args: &[Datum], _ctx: &FuncCtx) -> Result<Datum> {
    Ok(Datum::Float8(to_f64(nm(arg(args, 0)?)?)?))
}

/// numeric → f32 (`numeric_float4`): correctly rounded parse of the decimal text, as `float4in`.
fn to_f32(n: &Numeric) -> Result<f32> {
    match n {
        Numeric::NaN => Ok(f32::NAN),
        Numeric::PosInf => Ok(f32::INFINITY),
        Numeric::NegInf => Ok(f32::NEG_INFINITY),
        Numeric::Finite(d) => {
            let text = format_numeric(n)?;
            let v = text
                .parse::<f32>()
                .map_err(|_| PgError::internal(format!("cannot parse numeric text {text} as real")))?;
            if v.is_infinite() || (v == 0.0 && !d.is_zero()) {
                return Err(PgError::new(
                    NUMERIC_VALUE_OUT_OF_RANGE,
                    format!("\"{text}\" is out of range for type real"),
                ));
            }
            Ok(v)
        }
    }
}

/// numeric → integer of the given width: round half away from zero, then the caller range-checks.
/// NaN / infinity → 0A000 `cannot convert NaN to <type>` / `cannot convert infinity to <type>`.
fn int_from_numeric(n: &Numeric, type_name: &str) -> Result<BigInt> {
    match n {
        Numeric::NaN => Err(PgError::new(FEATURE_NOT_SUPPORTED, format!("cannot convert NaN to {type_name}"))),
        Numeric::PosInf | Numeric::NegInf => Err(PgError::new(
            FEATURE_NOT_SUPPORTED,
            format!("cannot convert infinity to {type_name}"),
        )),
        Numeric::Finite(d) => Ok(d.with_scale_round(0, RoundingMode::HalfUp).into_bigint_and_exponent().0),
    }
}

/// C `%.{prec}g` formatting of a finite double: `prec` significant digits, trailing zeros removed.
fn format_g(v: f64, prec: usize) -> String {
    if v == 0.0 {
        return if v.is_sign_negative() { "-0".to_string() } else { "0".to_string() };
    }
    let prec = prec.max(1);
    let sci = format!("{:.*e}", prec - 1, v.abs());
    let (mantissa, exp_text) = match sci.split_once('e') {
        Some(parts) => parts,
        None => (sci.as_str(), "0"),
    };
    let exp: i32 = exp_text.parse().unwrap_or(0);
    let all_digits: String = mantissa.chars().filter(|c| c.is_ascii_digit()).collect();
    let trimmed = all_digits.trim_end_matches('0');
    let digits = if trimmed.is_empty() { "0" } else { trimmed };

    let body = if exp < -4 || exp >= prec as i32 {
        let mut s = String::with_capacity(digits.len() + 6);
        s.push_str(&digits[..1]);
        if digits.len() > 1 {
            s.push('.');
            s.push_str(&digits[1..]);
        }
        s.push('e');
        s.push(if exp < 0 { '-' } else { '+' });
        s.push_str(&format!("{:02}", exp.unsigned_abs()));
        s
    } else if exp >= 0 {
        let int_len = (exp + 1) as usize;
        if digits.len() <= int_len {
            let mut s = digits.to_string();
            s.extend(std::iter::repeat('0').take(int_len - digits.len()));
            s
        } else {
            format!("{}.{}", &digits[..int_len], &digits[int_len..])
        }
    } else {
        format!("0.{}{}", "0".repeat((-exp - 1) as usize), digits)
    };
    if v < 0.0 {
        format!("-{body}")
    } else {
        body
    }
}

/// Numeric value of `%.{prec}g` text (`float8_numeric` / `float4_numeric`).
fn numeric_from_g(v: f64, prec: usize) -> Result<Numeric> {
    let text = format_g(v, prec);
    let d = BigDecimal::from_str(&text)
        .map_err(|_| PgError::internal(format!("cannot convert {text} to numeric")))?;
    Ok(Numeric::from_decimal(d))
}

// ---------------------------------------------------------------------------------------------
// Scale functions
// ---------------------------------------------------------------------------------------------

/// Display scale (dscale) of a finite value.
fn dscale_of(d: &BigDecimal) -> i64 {
    d.as_bigint_and_exponent().1.max(0)
}

/// `get_min_scale`: the fewest fractional digits that represent the value exactly.
fn min_scale_of(d: &BigDecimal) -> i64 {
    let (digits, scale) = d.as_bigint_and_exponent();
    if digits.sign() == Sign::NoSign {
        return 0;
    }
    let text = digits.magnitude().to_string();
    let trailing_zeros = text.bytes().rev().take_while(|&b| b == b'0').count() as i64;
    (scale - trailing_zeros).max(0)
}

fn f_scale(args: &[Datum], _ctx: &FuncCtx) -> Result<Datum> {
    match nm(arg(args, 0)?)? {
        Numeric::Finite(d) => Ok(Datum::Int4(dscale_of(d) as i32)),
        _ => Ok(Datum::Null),
    }
}

fn f_min_scale(args: &[Datum], _ctx: &FuncCtx) -> Result<Datum> {
    match nm(arg(args, 0)?)? {
        Numeric::Finite(d) => Ok(Datum::Int4(min_scale_of(d) as i32)),
        _ => Ok(Datum::Null),
    }
}

fn f_trim_scale(args: &[Datum], _ctx: &FuncCtx) -> Result<Datum> {
    match nm(arg(args, 0)?)? {
        Numeric::Finite(d) => {
            // Every removed fractional digit is zero, so this truncation is exact.
            let scale = min_scale_of(d);
            Ok(Datum::Numeric(Numeric::Finite(d.with_scale(scale))))
        }
        other => Ok(Datum::Numeric(other.clone())),
    }
}

// ---------------------------------------------------------------------------------------------
// factorial, gcd, lcm
// ---------------------------------------------------------------------------------------------

fn f_factorial(args: &[Datum], _ctx: &FuncCtx) -> Result<Datum> {
    let n = i64_of(arg(args, 0)?)?;
    if n < 0 {
        return Err(PgError::new(NUMERIC_VALUE_OUT_OF_RANGE, "factorial of a negative number is undefined"));
    }
    if n <= 1 {
        return Ok(Datum::Numeric(Numeric::from_i64(1)));
    }
    if n > FACTORIAL_MAX {
        return Err(PgError::new(NUMERIC_VALUE_OUT_OF_RANGE, "value overflows numeric format"));
    }
    let mut acc = BigInt::from(1);
    for k in 2..=n {
        acc *= BigInt::from(k);
    }
    Ok(Datum::Numeric(Numeric::from_decimal(BigDecimal::from(acc))))
}

/// Align two decimals to their common scale and return the unscaled integers.
fn align(a: &BigDecimal, b: &BigDecimal) -> (BigInt, BigInt, i64) {
    let s = dscale_of(a).max(dscale_of(b));
    let ai = a.with_scale(s).into_bigint_and_exponent().0;
    let bi = b.with_scale(s).into_bigint_and_exponent().0;
    (ai, bi, s)
}

/// Exact remainder `a mod b` of non-negative finite decimals (`b` nonzero).
fn exact_rem(a: &BigDecimal, b: &BigDecimal) -> BigDecimal {
    let (ai, bi, s) = align(a, b);
    BigDecimal::new(ai % bi, s)
}

/// `gcd_var`: Euclid on absolute values; result dscale = max of the input dscales.
fn gcd_dec(a: &BigDecimal, b: &BigDecimal) -> BigDecimal {
    let res_scale = dscale_of(a).max(dscale_of(b));
    let mut x = a.abs();
    let mut y = b.abs();
    if x < y {
        std::mem::swap(&mut x, &mut y);
    }
    // Here x >= y.
    let g = if y.is_zero() {
        x
    } else {
        loop {
            let r = exact_rem(&x, &y);
            if r.is_zero() {
                break y;
            }
            x = y;
            y = r;
        }
    };
    g.with_scale(res_scale)
}

fn f_gcd(args: &[Datum], _ctx: &FuncCtx) -> Result<Datum> {
    match (nm(arg(args, 0)?)?, nm(arg(args, 1)?)?) {
        (Numeric::Finite(x), Numeric::Finite(y)) => Ok(Datum::Numeric(Numeric::Finite(gcd_dec(x, y)))),
        _ => Ok(Datum::Numeric(Numeric::NaN)),
    }
}

fn f_lcm(args: &[Datum], _ctx: &FuncCtx) -> Result<Datum> {
    match (nm(arg(args, 0)?)?, nm(arg(args, 1)?)?) {
        (Numeric::Finite(x), Numeric::Finite(y)) => {
            let res_scale = dscale_of(x).max(dscale_of(y));
            if x.is_zero() || y.is_zero() {
                return Ok(Datum::Numeric(Numeric::Finite(BigDecimal::from(0i64).with_scale(res_scale))));
            }
            // lcm(x, y) = |y * (x / gcd(x, y))|; x / gcd is an exact integer.
            let g = gcd_dec(x, y);
            let (xi, gi, _) = align(x, &g);
            let q = xi / gi;
            let (yi, sy) = y.as_bigint_and_exponent();
            let prod = BigDecimal::new(yi, sy) * BigDecimal::new(q, 0);
            Ok(Datum::Numeric(Numeric::Finite(prod.abs().with_scale(res_scale))))
        }
        _ => Ok(Datum::Numeric(Numeric::NaN)),
    }
}

// ---------------------------------------------------------------------------------------------
// width_bucket, in_range, random
// ---------------------------------------------------------------------------------------------

/// `width_bucket(operand, bound1, bound2, count)` (`width_bucket_numeric`).
fn f_width_bucket(args: &[Datum], _ctx: &FuncCtx) -> Result<Datum> {
    let op = nm(arg(args, 0)?)?;
    let b1 = nm(arg(args, 1)?)?;
    let b2 = nm(arg(args, 2)?)?;
    let count = i32_of(arg(args, 3)?)?;
    Ok(Datum::Int4(width_bucket(op, b1, b2, count)?))
}

fn width_bucket(op: &Numeric, b1: &Numeric, b2: &Numeric, count: i32) -> Result<i32> {
    if count <= 0 {
        return Err(PgError::new(INVALID_ARGUMENT_FOR_WIDTH_BUCKET, "count must be greater than zero"));
    }
    if !(op.is_finite() && b1.is_finite() && b2.is_finite()) {
        if op.is_nan() || b1.is_nan() || b2.is_nan() {
            return Err(PgError::new(
                INVALID_ARGUMENT_FOR_WIDTH_BUCKET,
                "operand, lower bound, and upper bound cannot be NaN",
            ));
        }
        if b1.is_infinite() || b2.is_infinite() {
            return Err(PgError::new(INVALID_ARGUMENT_FOR_WIDTH_BUCKET, "lower and upper bounds must be finite"));
        }
    }
    let order = b1.pg_cmp(b2);
    if order == Ordering::Equal {
        return Err(PgError::new(INVALID_ARGUMENT_FOR_WIDTH_BUCKET, "lower bound cannot equal upper bound"));
    }
    let ascending = order == Ordering::Less;
    let below = if ascending {
        op.pg_cmp(b1) == Ordering::Less
    } else {
        op.pg_cmp(b1) == Ordering::Greater
    };
    let beyond = if ascending {
        op.pg_cmp(b2) != Ordering::Less
    } else {
        op.pg_cmp(b2) != Ordering::Greater
    };
    let result = if below {
        BigInt::from(0)
    } else if beyond {
        BigInt::from(i64::from(count) + 1)
    } else {
        match (op, b1, b2) {
            (Numeric::Finite(o), Numeric::Finite(l), Numeric::Finite(u)) => bucket_quotient(o, l, u, count) + BigInt::from(1),
            _ => return Err(PgError::internal("width_bucket: non-finite operand inside the bucket range")),
        }
    };
    result
        .to_i32()
        .ok_or_else(|| PgError::new(NUMERIC_VALUE_OUT_OF_RANGE, "integer out of range"))
}

/// `trunc((operand - bound1) * count / (bound2 - bound1))`, computed exactly.
fn bucket_quotient(op: &BigDecimal, b1: &BigDecimal, b2: &BigDecimal, count: i32) -> BigInt {
    let num = (op - b1) * BigDecimal::from(i64::from(count));
    let den = b2 - b1;
    let (n, d, _) = align(&num, &den);
    // BigInt division truncates toward zero, matching `div_var(..., rscale 0, round false)`.
    n / d
}

/// `in_range_numeric_numeric(val, base, offset, sub, less)`.
fn f_in_range(args: &[Datum], _ctx: &FuncCtx) -> Result<Datum> {
    let val = nm(arg(args, 0)?)?;
    let base = nm(arg(args, 1)?)?;
    let offset = nm(arg(args, 2)?)?;
    let sub = bool_of(arg(args, 3)?)?;
    let less = bool_of(arg(args, 4)?)?;
    Ok(Datum::Bool(in_range(val, base, offset, sub, less)?))
}

fn in_range(val: &Numeric, base: &Numeric, offset: &Numeric, sub: bool, less: bool) -> Result<bool> {
    // Reject negative (including -Inf) or NaN offsets.
    let offset_ok = match offset {
        Numeric::Finite(d) => d.sign() != Sign::Minus,
        Numeric::PosInf => true,
        Numeric::NegInf | Numeric::NaN => false,
    };
    if !offset_ok {
        return Err(PgError::new(
            INVALID_PRECEDING_OR_FOLLOWING_SIZE,
            "invalid preceding or following size in window function",
        ));
    }
    // NaN sorts after every non-NaN value (cf. cmp_numerics).
    if val.is_nan() {
        return Ok(if base.is_nan() { true } else { !less });
    }
    if base.is_nan() {
        return Ok(less);
    }
    // Infinite offset (necessarily +Inf here).
    if let Numeric::PosInf = offset {
        let base_hit = if sub {
            matches!(base, Numeric::PosInf)
        } else {
            matches!(base, Numeric::NegInf)
        };
        let result = if base_hit {
            true
        } else if sub {
            if less {
                matches!(val, Numeric::NegInf)
            } else {
                true
            }
        } else if less {
            true
        } else {
            matches!(val, Numeric::PosInf)
        };
        return Ok(result);
    }
    if let (Numeric::Finite(v), Numeric::Finite(b), Numeric::Finite(o)) = (val, base, offset) {
        let sum = if sub { b - o } else { b + o };
        let ord = v.cmp(&sum);
        return Ok(if less {
            ord != Ordering::Greater
        } else {
            ord != Ordering::Less
        });
    }
    // Remaining cases involve an infinite val or base; the offset is finite here.
    match val {
        Numeric::PosInf => return Ok(if matches!(base, Numeric::PosInf) { true } else { !less }),
        Numeric::NegInf => return Ok(if matches!(base, Numeric::NegInf) { true } else { less }),
        _ => {}
    }
    match base {
        Numeric::NegInf => Ok(!less),
        _ => Ok(less),
    }
}

/// `random(min numeric, max numeric)`: uniform over the multiples of 10^-dscale in [min, max].
fn f_random(args: &[Datum], ctx: &FuncCtx) -> Result<Datum> {
    let lo = nm(arg(args, 0)?)?;
    let hi = nm(arg(args, 1)?)?;
    if lo.is_nan() {
        return Err(PgError::new(INVALID_PARAMETER_VALUE, "lower bound cannot be NaN"));
    }
    if !lo.is_finite() {
        return Err(PgError::new(INVALID_PARAMETER_VALUE, "lower bound cannot be infinity"));
    }
    if hi.is_nan() {
        return Err(PgError::new(INVALID_PARAMETER_VALUE, "upper bound cannot be NaN"));
    }
    if !hi.is_finite() {
        return Err(PgError::new(INVALID_PARAMETER_VALUE, "upper bound cannot be infinity"));
    }
    let (l, h) = match (lo, hi) {
        (Numeric::Finite(l), Numeric::Finite(h)) => (l, h),
        _ => return Err(PgError::internal("random: non-finite bound after validation")),
    };
    if l > h {
        return Err(PgError::new(
            INVALID_PARAMETER_VALUE,
            "lower bound must be less than or equal to upper bound",
        ));
    }
    let s = dscale_of(l).max(dscale_of(h));
    let lo_i = l.with_scale(s).into_bigint_and_exponent().0;
    let hi_i = h.with_scale(s).into_bigint_and_exponent().0;
    let span = &hi_i - &lo_i;
    let value = if span.sign() == Sign::NoSign {
        lo_i
    } else {
        // Draw 8 spare bytes beyond the span's width so the modulo bias is negligible.
        let nbytes = (span.bits() as usize).div_ceil(8) + 8;
        let mut buf = vec![0u8; nbytes];
        ctx.session.fill_random(&mut buf);
        let r = BigInt::from_bytes_le(Sign::Plus, &buf) % (&span + BigInt::from(1));
        lo_i + r
    };
    Ok(Datum::Numeric(Numeric::Finite(BigDecimal::new(value, s))))
}

// ---------------------------------------------------------------------------------------------
// hash_numeric (+ extended)
// ---------------------------------------------------------------------------------------------

/// NBASE (base-10000) digit groups of |d| exactly as PostgreSQL stores them after stripping
/// leading and trailing zero groups, plus the weight of the first group. Zero → empty groups.
/// The representation depends only on the value, never on dscale.
fn nbase_groups(d: &BigDecimal) -> (Vec<u16>, i64) {
    let (int_val, scale) = d.as_bigint_and_exponent();
    if int_val.sign() == Sign::NoSign {
        return (Vec::new(), 0);
    }
    let frac_len = scale.max(0) as usize;
    let mut digits: Vec<u8> = int_val.magnitude().to_string().into_bytes();
    if digits.len() <= frac_len {
        // Ensure at least one integer digit.
        let mut padded = vec![b'0'; frac_len + 1 - digits.len()];
        padded.extend_from_slice(&digits);
        digits = padded;
    }
    let int_len = digits.len() - frac_len;
    // Integer groups are aligned from the decimal point to the left, fraction groups to the right.
    let lead_pad = (4 - int_len % 4) % 4;
    let trail_pad = (4 - frac_len % 4) % 4;
    let mut all: Vec<u8> = Vec::with_capacity(lead_pad + digits.len() + trail_pad);
    all.extend(std::iter::repeat(b'0').take(lead_pad));
    all.extend_from_slice(&digits);
    all.extend(std::iter::repeat(b'0').take(trail_pad));
    let int_groups = ((int_len + lead_pad) / 4) as i64;
    let groups: Vec<u16> = all
        .chunks(4)
        .map(|c| c.iter().fold(0u16, |acc, &b| acc * 10 + u16::from(b - b'0')))
        .collect();
    let first = groups.iter().position(|&g| g != 0);
    let last = groups.iter().rposition(|&g| g != 0);
    match (first, last) {
        (Some(f), Some(l)) => (groups[f..=l].to_vec(), (int_groups - 1) - f as i64),
        _ => (Vec::new(), 0),
    }
}

fn group_bytes(groups: &[u16]) -> Vec<u8> {
    groups.iter().flat_map(|g| g.to_ne_bytes()).collect()
}

/// `hash_numeric`: hash of the NBASE digits (leading/trailing zero groups ignored) XOR weight.
/// Relies on `compare::pg_hash_bytes` (TYP-49); until that lands this reports its 0A000.
fn f_hash_numeric(args: &[Datum], _ctx: &FuncCtx) -> Result<Datum> {
    let d = match nm(arg(args, 0)?)? {
        Numeric::Finite(d) => d,
        _ => return Ok(Datum::Int4(0)),
    };
    let (groups, weight) = nbase_groups(d);
    if groups.is_empty() {
        return Ok(Datum::Int4(-1));
    }
    let h = pg_hash_bytes(&group_bytes(&groups))?;
    Ok(Datum::Int4((h ^ (weight as i32 as u32)) as i32))
}

/// `hash_numeric_extended(numeric, seed)`.
fn f_hash_numeric_extended(args: &[Datum], _ctx: &FuncCtx) -> Result<Datum> {
    let n = nm(arg(args, 0)?)?;
    let seed = i64_of(arg(args, 1)?)? as u64;
    let d = match n {
        Numeric::Finite(d) => d,
        _ => return Ok(Datum::Int8(seed as i64)),
    };
    let (groups, weight) = nbase_groups(d);
    if groups.is_empty() {
        return Ok(Datum::Int8(seed.wrapping_sub(1) as i64));
    }
    let h = pg_hash_bytes_extended(&group_bytes(&groups), seed)?;
    Ok(Datum::Int8((h ^ (weight as u64)) as i64))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::context::{IoConfig, TestSession};
    use crate::datum::TypeId;
    use crate::registry::builtin_env;

    fn with_ctx<R>(f: impl FnOnce(&FuncCtx) -> R) -> R {
        let env = builtin_env();
        let io = IoConfig::default();
        let session = TestSession::default();
        let ctx = FuncCtx {
            env,
            io: &io,
            session: &session,
            arg_types: &[],
            result_type: TypeId::NUMERIC,
            collation: 0,
        };
        f(&ctx)
    }

    fn num(s: &str) -> Numeric {
        Numeric::from_decimal(BigDecimal::from_str(s).expect("valid decimal literal"))
    }

    fn text_of(d: &Datum) -> String {
        match d {
            Datum::Numeric(n) => format_numeric(n).expect("format"),
            other => panic!("expected numeric datum, got {other:?}"),
        }
    }

    fn sqlstate_of(r: Result<Datum>) -> String {
        match r {
            Err(e) => e.sqlstate.code().to_string(),
            Ok(v) => panic!("expected an error, got {v:?}"),
        }
    }

    #[test]
    fn to_integers_round_half_away_from_zero() {
        assert_eq!(to_i32(&num("2.5")).expect("ok"), 3);
        assert_eq!(to_i32(&num("-2.5")).expect("ok"), -3);
        assert_eq!(to_i64(&num("1.4999")).expect("ok"), 1);
        assert_eq!(to_i16(&num("-7.5")).expect("ok"), -8);
    }

    #[test]
    fn to_integers_reject_specials_and_overflow() {
        let e = to_i32(&Numeric::NaN).expect_err("NaN");
        assert_eq!(e.sqlstate.code(), "0A000");
        assert_eq!(e.message, "cannot convert NaN to integer");
        let e = to_i64(&Numeric::PosInf).expect_err("inf");
        assert_eq!(e.sqlstate.code(), "0A000");
        assert_eq!(e.message, "cannot convert infinity to bigint");
        let e = to_i16(&num("40000")).expect_err("range");
        assert_eq!(e.sqlstate.code(), "22003");
        assert_eq!(e.message, "smallint out of range");
    }

    #[test]
    fn float_to_numeric_uses_pg_precision() {
        assert_eq!(text_of(&Datum::Numeric(from_f64(0.1).expect("ok"))), "0.1");
        assert_eq!(text_of(&Datum::Numeric(from_f64(1.0 / 3.0).expect("ok"))), "0.333333333333333");
        assert_eq!(text_of(&Datum::Numeric(from_f64(1e20).expect("ok"))), "100000000000000000000");
        assert_eq!(text_of(&Datum::Numeric(from_f64(1.23456789e-7).expect("ok"))), "0.000000123456789");
        assert_eq!(text_of(&Datum::Numeric(from_f32(1.1f32).expect("ok"))), "1.1");
        assert!(from_f64(f64::NAN).expect("ok").is_nan());
        assert_eq!(from_f64(f64::NEG_INFINITY).expect("ok"), Numeric::NegInf);
    }

    #[test]
    fn numeric_to_float_round_trips() {
        assert_eq!(to_f64(&num("0.1")).expect("ok"), 0.1);
        assert_eq!(to_f32(&num("1.1")).expect("ok"), 1.1f32);
        assert!(to_f64(&Numeric::NaN).expect("ok").is_nan());
        let e = to_f64(&num("1e400")).expect_err("overflow");
        assert_eq!(e.sqlstate.code(), "22003");
    }

    #[test]
    fn scale_functions() {
        with_ctx(|ctx| {
            let scale = f_scale(&[Datum::Numeric(num("1.50"))], ctx).expect("ok");
            assert_eq!(scale, Datum::Int4(2));
            assert_eq!(f_scale(&[Datum::Numeric(Numeric::NaN)], ctx).expect("ok"), Datum::Null);
            let min = f_min_scale(&[Datum::Numeric(num("1.500"))], ctx).expect("ok");
            assert_eq!(min, Datum::Int4(1));
            let trimmed = f_trim_scale(&[Datum::Numeric(num("1.500"))], ctx).expect("ok");
            assert_eq!(text_of(&trimmed), "1.5");
            let trimmed = f_trim_scale(&[Datum::Numeric(num("100.00"))], ctx).expect("ok");
            assert_eq!(text_of(&trimmed), "100");
        });
    }

    #[test]
    fn factorial_values_and_errors() {
        with_ctx(|ctx| {
            let v = f_factorial(&[Datum::Int8(20)], ctx).expect("ok");
            assert_eq!(text_of(&v), "2432902008176640000");
            assert_eq!(text_of(&f_factorial(&[Datum::Int8(0)], ctx).expect("ok")), "1");
            let r = f_factorial(&[Datum::Int8(-1)], ctx);
            assert_eq!(sqlstate_of(r), "22003");
            let r = f_factorial(&[Datum::Int8(FACTORIAL_MAX + 1)], ctx);
            assert_eq!(sqlstate_of(r), "22003");
        });
    }

    #[test]
    fn gcd_and_lcm_keep_fractional_digits() {
        with_ctx(|ctx| {
            let g = f_gcd(&[Datum::Numeric(num("12.5")), Datum::Numeric(num("7.5"))], ctx).expect("ok");
            assert_eq!(text_of(&g), "2.5");
            let l = f_lcm(&[Datum::Numeric(num("1.5")), Datum::Numeric(num("2.5"))], ctx).expect("ok");
            assert_eq!(text_of(&l), "7.5");
            let l = f_lcm(&[Datum::Numeric(num("4")), Datum::Numeric(num("-6"))], ctx).expect("ok");
            assert_eq!(text_of(&l), "12");
        });
    }

    #[test]
    fn width_bucket_matches_pg_example() {
        with_ctx(|ctx| {
            let args = [
                Datum::Numeric(num("5.35")),
                Datum::Numeric(num("0.024")),
                Datum::Numeric(num("10.06")),
                Datum::Int4(5),
            ];
            assert_eq!(f_width_bucket(&args, ctx).expect("ok"), Datum::Int4(3));
            let below = [
                Datum::Numeric(num("-1")),
                Datum::Numeric(num("0")),
                Datum::Numeric(num("10")),
                Datum::Int4(5),
            ];
            assert_eq!(f_width_bucket(&below, ctx).expect("ok"), Datum::Int4(0));
            let above = [
                Datum::Numeric(num("10")),
                Datum::Numeric(num("0")),
                Datum::Numeric(num("10")),
                Datum::Int4(5),
            ];
            assert_eq!(f_width_bucket(&above, ctx).expect("ok"), Datum::Int4(6));
            let bad = [
                Datum::Numeric(num("1")),
                Datum::Numeric(num("0")),
                Datum::Numeric(num("10")),
                Datum::Int4(0),
            ];
            assert_eq!(sqlstate_of(f_width_bucket(&bad, ctx)), "2201G");
        });
    }

    #[test]
    fn in_range_basic_window_semantics() {
        // 5 <= 3 + 2 (following, less-or-equal form)
        assert!(in_range(&num("5"), &num("3"), &num("2"), false, true).expect("ok"));
        // 6 >= 3 - 2 (preceding, greater-or-equal form) is true
        assert!(in_range(&num("6"), &num("3"), &num("2"), true, false).expect("ok"));
        // Negative offsets are rejected per the spec.
        let e = in_range(&num("1"), &num("1"), &num("-1"), false, true).expect_err("negative");
        assert_eq!(e.sqlstate.code(), "22013");
    }

    #[test]
    fn random_stays_in_bounds() {
        with_ctx(|ctx| {
            for _ in 0..20 {
                let v = f_random(&[Datum::Numeric(num("1.5")), Datum::Numeric(num("2.5"))], ctx).expect("ok");
                let n = match &v {
                    Datum::Numeric(Numeric::Finite(d)) => d.clone(),
                    other => panic!("unexpected {other:?}"),
                };
                assert!(n >= BigDecimal::from_str("1.5").expect("lit"));
                assert!(n <= BigDecimal::from_str("2.5").expect("lit"));
            }
            let r = f_random(&[Datum::Numeric(num("3")), Datum::Numeric(num("1"))], ctx);
            assert_eq!(sqlstate_of(r), "22023");
        });
    }

    #[test]
    fn nbase_groups_strip_zero_groups() {
        // 1.50 and 1.5 share the value; groups [1, 5000], weight 0.
        let (g, w) = nbase_groups(&BigDecimal::from_str("1.50").expect("lit"));
        assert_eq!((g, w), (vec![1u16, 5000], 0));
        // 10000 = one group at weight 1.
        let (g, w) = nbase_groups(&BigDecimal::from_str("10000").expect("lit"));
        assert_eq!((g, w), (vec![1u16], 1));
        // 0.0001 = one group at weight -1.
        let (g, w) = nbase_groups(&BigDecimal::from_str("0.0001").expect("lit"));
        assert_eq!((g, w), (vec![1u16], -1));
        let (g, _) = nbase_groups(&BigDecimal::from_str("0").expect("lit"));
        assert!(g.is_empty());
    }

    #[test]
    fn hash_of_zero_and_specials() {
        with_ctx(|ctx| {
            assert_eq!(f_hash_numeric(&[Datum::Numeric(Numeric::NaN)], ctx).expect("ok"), Datum::Int4(0));
            assert_eq!(f_hash_numeric(&[Datum::Numeric(Numeric::PosInf)], ctx).expect("ok"), Datum::Int4(0));
            assert_eq!(f_hash_numeric(&[Datum::Numeric(num("0"))], ctx).expect("ok"), Datum::Int4(-1));
            let seed = 7i64;
            assert_eq!(
                f_hash_numeric_extended(&[Datum::Numeric(num("0")), Datum::Int8(seed)], ctx).expect("ok"),
                Datum::Int8(6)
            );
        });
    }

    #[test]
    #[ignore = "needs TYP-49"]
    fn hash_ignores_trailing_zeros() {
        with_ctx(|ctx| {
            let a = f_hash_numeric(&[Datum::Numeric(num("1.5"))], ctx).expect("ok");
            let b = f_hash_numeric(&[Datum::Numeric(num("1.50"))], ctx).expect("ok");
            assert_eq!(a, b);
        });
    }

    #[test]
    #[ignore = "needs TYP-11"]
    fn ln_of_ten_matches_pg() {
        with_ctx(|ctx| {
            let v = f_ln(&[Datum::Numeric(num("10"))], ctx).expect("ok");
            assert_eq!(text_of(&v), "2.3025850929940457");
        });
    }
}
