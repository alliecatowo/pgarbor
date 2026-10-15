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
use super::{Numeric, NUMERIC_MAX_DISPLAY_SCALE, NUMERIC_MIN_DISPLAY_SCALE, NUMERIC_MIN_SIG_DIGITS};
use bigdecimal::{BigDecimal, RoundingMode};
use num_bigint::{BigInt, Sign};
use pgarbor_common::{PgError, Result, SqlState};

/// `NUMERIC_MAX_RESULT_SCALE` (= 2 * NUMERIC_MAX_PRECISION).
const MAX_RESULT_SCALE: f64 = 2000.0;
/// `NUMERIC_WEIGHT_MAX` (base-10000 weight limit).
const WEIGHT_MAX: i64 = 32767;
/// log10(e) as used by PG.
const LOG10_E: f64 = 0.434294481903252;
const LOG2_10: f64 = 3.321928094887362;

// ---------------------------------------------------------------------------------------------
// Errors
// ---------------------------------------------------------------------------------------------

fn err_overflow() -> PgError {
    PgError::new(SqlState::NUMERIC_VALUE_OUT_OF_RANGE, "value overflows numeric format")
}
fn err_sqrt_neg() -> PgError {
    PgError::new(SqlState::INVALID_ARGUMENT_FOR_POWER_FUNCTION, "cannot take square root of a negative number")
}
fn err_ln_zero() -> PgError {
    PgError::new(SqlState::INVALID_ARGUMENT_FOR_LOGARITHM, "cannot take logarithm of zero")
}
fn err_ln_neg() -> PgError {
    PgError::new(SqlState::INVALID_ARGUMENT_FOR_LOGARITHM, "cannot take logarithm of a negative number")
}
fn err_div_zero() -> PgError {
    PgError::new(SqlState::DIVISION_BY_ZERO, "division by zero")
}
fn err_zero_neg_power() -> PgError {
    PgError::new(SqlState::INVALID_ARGUMENT_FOR_POWER_FUNCTION, "zero raised to a negative power is undefined")
}
fn err_complex() -> PgError {
    PgError::new(
        SqlState::INVALID_ARGUMENT_FOR_POWER_FUNCTION,
        "a negative number raised to a non-integer power yields a complex result",
    )
}

// ---------------------------------------------------------------------------------------------
// Small helpers over BigInt / BigDecimal
// ---------------------------------------------------------------------------------------------

fn pow10(n: i64) -> BigInt {
    BigInt::from(10u32).pow(n.max(0) as u32)
}

fn abs_int(x: &BigInt) -> BigInt {
    if x.sign() == Sign::Minus { -x } else { x.clone() }
}

/// Integer division rounding half away from zero. `d` must be nonzero.
fn round_div(n: &BigInt, d: &BigInt) -> BigInt {
    let neg = (n.sign() == Sign::Minus) != (d.sign() == Sign::Minus);
    let a = abs_int(n);
    let b = abs_int(d);
    let q = (a * 2u32 + &b) / (&b * 2u32);
    if neg { -q } else { q }
}

fn dec_sign(d: &BigDecimal) -> i32 {
    match d.as_bigint_and_exponent().0.sign() {
        Sign::Minus => -1,
        Sign::NoSign => 0,
        Sign::Plus => 1,
    }
}

fn dec_scale(d: &BigDecimal) -> i64 {
    d.as_bigint_and_exponent().1.max(0)
}

fn round_to(d: &BigDecimal, scale: i64) -> BigDecimal {
    d.with_scale_round(scale, RoundingMode::HalfUp)
}

fn zero_at(scale: i64) -> BigDecimal {
    BigDecimal::new(BigInt::from(0), scale)
}

/// `numericvar_to_double_no_overflow`: nearest f64 (inf on overflow, 0 on underflow).
fn to_f64(d: &BigDecimal) -> f64 {
    let (u, s) = d.as_bigint_and_exponent();
    format!("{}e{}", u, -s).parse::<f64>().unwrap_or(0.0)
}

fn clamp_rscale(r: i64) -> i64 {
    r.max(NUMERIC_MIN_DISPLAY_SCALE).min(NUMERIC_MAX_DISPLAY_SCALE)
}

/// Base-10000 view of a nonzero value: weight of the first group and the groups (trailing zero
/// groups stripped), mirroring a normalized `NumericVar`.
struct NBase {
    weight: i64,
    digits: Vec<i64>,
}

fn nbase(d: &BigDecimal) -> Option<NBase> {
    let (u, s) = d.as_bigint_and_exponent();
    if u.sign() == Sign::NoSign {
        return None;
    }
    let ds = abs_int(&u).to_string();
    let bytes = ds.as_bytes();
    let n = bytes.len();
    let dexp = n as i64 - 1 - s;
    let weight = dexp.div_euclid(4);
    let lead = (dexp - 4 * weight + 1) as usize;
    let mut digits: Vec<i64> = Vec::with_capacity(n / 4 + 2);
    let mut pos = 0usize;
    let mut take = lead;
    while pos < n {
        let mut v = 0i64;
        for i in 0..take {
            let c = if pos + i < n { (bytes[pos + i] - b'0') as i64 } else { 0 };
            v = v * 10 + c;
        }
        digits.push(v);
        pos += take;
        take = 4;
    }
    while digits.last() == Some(&0) {
        digits.pop();
    }
    Some(NBase { weight, digits })
}

/// Base-10000 weight (0 for zero), as `NumericVar.weight`.
fn weight_of(d: &BigDecimal) -> i64 {
    nbase(d).map(|n| n.weight).unwrap_or(0)
}

/// Is the value integral (`ndigits <= weight + 1`)?
fn is_integral(d: &BigDecimal) -> bool {
    d.is_integer()
}

/// `a / b` rounded half away from zero to `rscale` fractional digits (`div_var` with round=true).
fn div_round(a: &BigDecimal, b: &BigDecimal, rscale: i64) -> Result<BigDecimal> {
    let (ua, sa) = a.as_bigint_and_exponent();
    let (ub, sb) = b.as_bigint_and_exponent();
    if ub.sign() == Sign::NoSign {
        return Err(err_div_zero());
    }
    let n = ua * pow10(rscale + sb);
    let d = ub * pow10(sa);
    Ok(BigDecimal::new(round_div(&n, &d), rscale))
}

// ---------------------------------------------------------------------------------------------
// sqrt
// ---------------------------------------------------------------------------------------------

/// `sqrt_var`: correctly rounded (half up) square root at `rscale`.
fn sqrt_dec(d: &BigDecimal, rscale: i64) -> Result<BigDecimal> {
    let sign = dec_sign(d);
    if sign == 0 {
        return Ok(zero_at(rscale));
    }
    if sign < 0 {
        return Err(err_sqrt_neg());
    }
    let (u, s) = d.as_bigint_and_exponent();
    // t = floor(sqrt(value) * 10^(rscale+1))
    let e = 2 * (rscale + 1) - s;
    let n = if e >= 0 { u * pow10(e) } else { u / pow10(-e) };
    let t = n.sqrt();
    let r = (t + 5u32) / 10u32;
    Ok(BigDecimal::new(r, rscale))
}

/// `sqrt(x)`.
// CONTRACT
pub fn sqrt(x: &Numeric) -> Result<Numeric> {
    let d = match x {
        Numeric::NaN => return Ok(Numeric::NaN),
        Numeric::PosInf => return Ok(Numeric::PosInf),
        Numeric::NegInf => return Err(err_sqrt_neg()),
        Numeric::Finite(d) => d,
    };
    let sweight = weight_of(d) * 2 + 1;
    let rscale = clamp_rscale((NUMERIC_MIN_SIG_DIGITS - sweight).max(dec_scale(d)));
    Ok(Numeric::from_decimal(sqrt_dec(d, rscale)?))
}

// ---------------------------------------------------------------------------------------------
// exp
// ---------------------------------------------------------------------------------------------

/// `exp_var`: e^arg rounded to `rscale`.
fn exp_dec(arg: &BigDecimal, rscale: i64) -> Result<BigDecimal> {
    let mut val = to_f64(arg);
    if val.abs() >= MAX_RESULT_SCALE * 3.0 {
        if val > 0.0 {
            return Err(err_overflow());
        }
        return Ok(zero_at(rscale));
    }
    let dweight = (val * LOG10_E) as i64;
    let mut ndiv2: i64 = 0;
    if val.abs() > 0.01 {
        ndiv2 = 1;
        val /= 2.0;
        while val.abs() > 0.01 {
            ndiv2 += 1;
            val /= 2.0;
        }
    }
    // Working precision: absolute error 10^-w on a value of magnitude 10^dweight, with the
    // relative error doubling at each of the ndiv2 squarings.
    let w = rscale.max(0) + dweight.max(0) + ndiv2 + 30;
    let (u, s) = arg.as_bigint_and_exponent();
    let mut x = if s <= w { u * pow10(w - s) } else { u / pow10(s - w) };
    if ndiv2 > 0 {
        x = x / (BigInt::from(1u32) << (ndiv2 as usize));
    }
    let one = pow10(w);
    // Taylor series: sum x^i / i!
    let mut sum = one.clone();
    let mut term = one.clone();
    let mut i: u32 = 1;
    loop {
        term = (&term * &x) / &one / i;
        if term.sign() == Sign::NoSign {
            break;
        }
        sum += &term;
        i += 1;
    }
    for _ in 0..ndiv2 {
        sum = (&sum * &sum) / &one;
    }
    Ok(round_to(&BigDecimal::new(sum, w), rscale))
}

/// `exp(x)`.
// CONTRACT
pub fn exp(x: &Numeric) -> Result<Numeric> {
    let d = match x {
        Numeric::NaN => return Ok(Numeric::NaN),
        Numeric::PosInf => return Ok(Numeric::PosInf),
        Numeric::NegInf => return Ok(Numeric::zero()),
        Numeric::Finite(d) => d,
    };
    let mut val = to_f64(d) * LOG10_E;
    if val.is_nan() {
        val = 0.0;
    }
    let val = val.max(-MAX_RESULT_SCALE).min(MAX_RESULT_SCALE);
    let rscale = clamp_rscale((NUMERIC_MIN_SIG_DIGITS - (val as i64)).max(dec_scale(d)));
    Ok(Numeric::from_decimal(exp_dec(d, rscale)?))
}

// ---------------------------------------------------------------------------------------------
// ln / log
// ---------------------------------------------------------------------------------------------

/// Number of decimal digits of a small nonnegative integer (at least 1).
fn ndigits10(mut v: i64) -> i64 {
    let mut n = 0;
    while v > 0 {
        n += 1;
        v /= 10;
    }
    n.max(1)
}

/// `estimate_ln_dweight`.
fn estimate_ln_dweight(d: &BigDecimal) -> i64 {
    if dec_sign(d) <= 0 {
        return 0;
    }
    let lo = BigDecimal::new(BigInt::from(9), 1);
    let hi = BigDecimal::new(BigInt::from(11), 1);
    if *d >= lo && *d <= hi {
        let x = d - BigDecimal::from(1i32);
        match nbase(&x) {
            Some(nb) => nb.weight * 4 + (ndigits10(nb.digits[0]) - 1),
            None => 0,
        }
    } else {
        match nbase(d) {
            Some(nb) => {
                let mut digits = nb.digits[0];
                let mut dweight = nb.weight * 4;
                if nb.digits.len() > 1 {
                    digits = digits * 10000 + nb.digits[1];
                    dweight -= 4;
                }
                let ln_var = (digits as f64).ln() + (dweight as f64) * 2.302585092994046;
                let l = ln_var.abs().log10();
                if l.is_finite() { l as i64 } else { 0 }
            }
            None => 0,
        }
    }
}

/// ln(2) as a fixed-point integer scaled by 10^w (2 * atanh(1/3)).
fn ln2_fixed(w: i64) -> BigInt {
    let one = pow10(w);
    let mut t = &one / 3u32;
    let mut sum = BigInt::from(0);
    let mut i: u32 = 0;
    loop {
        sum += &t / (2 * i + 1);
        t = t / 9u32;
        if t.sign() == Sign::NoSign {
            break;
        }
        i += 1;
    }
    sum * 2u32
}

/// `ln_var`: natural logarithm rounded to `rscale`.
fn ln_dec(x: &BigDecimal, rscale: i64) -> Result<BigDecimal> {
    let sign = dec_sign(x);
    if sign == 0 {
        return Err(err_ln_zero());
    }
    if sign < 0 {
        return Err(err_ln_neg());
    }
    let (u, s) = x.as_bigint_and_exponent();
    let ds = u.to_string();
    let dexp = ds.len() as i64 - 1 - s;
    // Choose k with x / 2^k roughly in [0.7, 1.42].
    let cnt = ds.len().min(15);
    let lead: f64 = ds[..cnt].parse::<f64>().unwrap_or(1.0) / 10f64.powi(cnt as i32 - 1);
    let est = (lead.log10() + dexp as f64) * LOG2_10;
    let k = est.round() as i64;
    let w = rscale.max(0) + 30 + ndigits10(k.abs());
    let one = pow10(w);
    let mut n = u * &one;
    let mut dd = pow10(s);
    if k >= 0 {
        dd = dd << (k as usize);
    } else {
        n = n << ((-k) as usize);
    }
    let y = n / dd;
    // ln(y) = 2 atanh(z), z = (y-1)/(y+1)
    let z = (&y - &one) * &one / (&y + &one);
    let z2 = (&z * &z) / &one;
    let mut term = z;
    let mut sum = BigInt::from(0);
    let mut i: u32 = 0;
    while term.sign() != Sign::NoSign {
        sum += &term / (2 * i + 1);
        term = (&term * &z2) / &one;
        i += 1;
    }
    let mut res = sum * 2u32;
    if k != 0 {
        res += ln2_fixed(w) * BigInt::from(k);
    }
    Ok(round_to(&BigDecimal::new(res, w), rscale))
}

/// `ln(x)`.
// CONTRACT
pub fn ln(x: &Numeric) -> Result<Numeric> {
    let d = match x {
        Numeric::NaN => return Ok(Numeric::NaN),
        Numeric::PosInf => return Ok(Numeric::PosInf),
        Numeric::NegInf => return Err(err_ln_neg()),
        Numeric::Finite(d) => d,
    };
    let ln_dweight = estimate_ln_dweight(d);
    let rscale = clamp_rscale((NUMERIC_MIN_SIG_DIGITS - ln_dweight).max(dec_scale(d)));
    Ok(Numeric::from_decimal(ln_dec(d, rscale)?))
}

fn numeric_sign(n: &Numeric) -> i32 {
    match n {
        Numeric::NaN => 0,
        Numeric::PosInf => 1,
        Numeric::NegInf => -1,
        Numeric::Finite(d) => dec_sign(d),
    }
}

/// `log_var`.
fn log_dec(base: &BigDecimal, num: &BigDecimal) -> Result<BigDecimal> {
    let ln_base_dweight = estimate_ln_dweight(base);
    let ln_num_dweight = estimate_ln_dweight(num);
    let result_dweight = ln_num_dweight - ln_base_dweight;
    let rscale = clamp_rscale(
        (NUMERIC_MIN_SIG_DIGITS - result_dweight).max(dec_scale(base)).max(dec_scale(num)),
    );
    let ln_base_rscale = (rscale + result_dweight - ln_base_dweight + 8).max(NUMERIC_MIN_DISPLAY_SCALE);
    let ln_num_rscale = (rscale + result_dweight - ln_num_dweight + 8).max(NUMERIC_MIN_DISPLAY_SCALE);
    let ln_base = ln_dec(base, ln_base_rscale)?;
    let ln_num = ln_dec(num, ln_num_rscale)?;
    div_round(&ln_num, &ln_base, rscale)
}

/// `log(base, x)`.
// CONTRACT
pub fn log(base: &Numeric, x: &Numeric) -> Result<Numeric> {
    if base.is_nan() || x.is_nan() {
        return Ok(Numeric::NaN);
    }
    if base.is_infinite() || x.is_infinite() {
        let s1 = numeric_sign(base);
        let s2 = numeric_sign(x);
        if s1 < 0 || s2 < 0 {
            return Err(err_ln_neg());
        }
        if s1 == 0 || s2 == 0 {
            return Err(err_ln_zero());
        }
        if matches!(base, Numeric::PosInf) {
            if matches!(x, Numeric::PosInf) {
                return Ok(Numeric::NaN);
            }
            return Ok(Numeric::zero());
        }
        return Ok(Numeric::PosInf);
    }
    match (base, x) {
        (Numeric::Finite(b), Numeric::Finite(n)) => Ok(Numeric::from_decimal(log_dec(b, n)?)),
        _ => Ok(Numeric::NaN),
    }
}

/// `log10(x)` / `log(x)`.
// CONTRACT
pub fn log10(x: &Numeric) -> Result<Numeric> {
    log(&Numeric::from_i64(10), x)
}

// ---------------------------------------------------------------------------------------------
// power
// ---------------------------------------------------------------------------------------------

/// Parity of an integral value.
fn is_odd(d: &BigDecimal) -> bool {
    let i = d.with_scale_round(0, RoundingMode::Down).as_bigint_and_exponent().0;
    let two = BigInt::from(2u32);
    (i % two).sign() != Sign::NoSign
}

/// `power_var_int`.
fn power_var_int(base: &BigDecimal, exp: i32, exp_dscale: i64) -> Result<BigDecimal> {
    let nb = nbase(base);
    let f: f64 = match &nb {
        Some(nb) => {
            let mut f = nb.digits[0] as f64;
            let mut p = nb.weight * 4;
            let mut i = 1usize;
            while i < nb.digits.len() && i * 4 < 16 {
                f = f * 10000.0 + nb.digits[i] as f64;
                p -= 4;
                i += 1;
            }
            (exp as f64) * (f.log10() + p as f64)
        }
        None => 0.0,
    };
    if f > ((WEIGHT_MAX + 1) * 4) as f64 {
        return Err(err_overflow());
    }
    if f + 1.0 < -(NUMERIC_MAX_DISPLAY_SCALE as f64) {
        return Ok(zero_at(NUMERIC_MAX_DISPLAY_SCALE));
    }
    let rscale = clamp_rscale((NUMERIC_MIN_SIG_DIGITS - (f as i64)).max(dec_scale(base)).max(exp_dscale));

    match exp {
        0 => return Ok(BigDecimal::new(pow10(rscale), rscale)),
        1 => return Ok(round_to(base, rscale)),
        -1 => return div_round(&BigDecimal::from(1i32), base, rscale),
        2 => return Ok(round_to(&(base * base), rscale)),
        _ => {}
    }
    if nb.is_none() {
        if exp < 0 {
            return Err(err_div_zero());
        }
        return Ok(zero_at(rscale));
    }

    let sig_digits = 1 + rscale + (f as i64) + ((exp as f64).abs().ln() as i64) + 8;
    let mut neg = exp < 0;
    let mut mask: u32 = exp.unsigned_abs();
    let mut base_prod = base.clone();
    let mut result = if mask & 1 == 1 { base.clone() } else { BigDecimal::from(1i32) };
    loop {
        mask >>= 1;
        if mask == 0 {
            break;
        }
        let mut local = sig_digits - 2 * weight_of(&base_prod) * 4;
        local = local.min(2 * dec_scale(&base_prod)).max(NUMERIC_MIN_DISPLAY_SCALE);
        base_prod = round_to(&(&base_prod * &base_prod), local);
        if mask & 1 == 1 {
            let mut l2 = sig_digits - (weight_of(&base_prod) + weight_of(&result)) * 4;
            l2 = l2.min(dec_scale(&base_prod) + dec_scale(&result)).max(NUMERIC_MIN_DISPLAY_SCALE);
            result = round_to(&(&base_prod * &result), l2);
        }
        if weight_of(&base_prod) > WEIGHT_MAX || weight_of(&result) > WEIGHT_MAX {
            if !neg {
                return Err(err_overflow());
            }
            result = BigDecimal::from(0i32);
            neg = false;
            break;
        }
    }
    if neg {
        div_round(&BigDecimal::from(1i32), &result, rscale)
    } else {
        Ok(round_to(&result, rscale))
    }
}

/// `power_var`.
fn power_var(base: &BigDecimal, exp: &BigDecimal) -> Result<BigDecimal> {
    if is_integral(exp) {
        let i = exp.with_scale_round(0, RoundingMode::Down).as_bigint_and_exponent().0;
        if let Ok(v) = i64::try_from(&i) {
            if v >= i32::MIN as i64 && v <= i32::MAX as i64 {
                return power_var_int(base, v as i32, dec_scale(exp));
            }
        }
    }
    if dec_sign(base) == 0 {
        return Ok(zero_at(NUMERIC_MIN_SIG_DIGITS));
    }
    let mut res_neg = false;
    let abs_base;
    let mut b = base;
    if dec_sign(base) < 0 {
        if !is_integral(exp) {
            return Err(err_complex());
        }
        res_neg = is_odd(exp);
        abs_base = base.abs();
        b = &abs_base;
    }
    let ln_dweight = estimate_ln_dweight(b);
    let local_rscale = (8 - ln_dweight).max(NUMERIC_MIN_DISPLAY_SCALE);
    let ln_base = ln_dec(b, local_rscale)?;
    let ln_num = round_to(&(&ln_base * exp), local_rscale);
    let mut val = to_f64(&ln_num);
    if val.abs() > MAX_RESULT_SCALE * 3.01 {
        if val > 0.0 {
            return Err(err_overflow());
        }
        return Ok(zero_at(NUMERIC_MAX_DISPLAY_SCALE));
    }
    val *= LOG10_E;
    let rscale = clamp_rscale((NUMERIC_MIN_SIG_DIGITS - (val as i64)).max(dec_scale(b)).max(dec_scale(exp)));
    let sig_digits = (rscale + (val as i64)).max(0);
    let local_rscale = (sig_digits - ln_dweight + 8).max(NUMERIC_MIN_DISPLAY_SCALE);
    let ln_base = ln_dec(b, local_rscale)?;
    let ln_num = round_to(&(&ln_base * exp), local_rscale);
    let r = exp_dec(&ln_num, rscale)?;
    if res_neg && dec_sign(&r) != 0 { Ok(-r) } else { Ok(r) }
}

/// `power(a, b)` / `a ^ b`.
// CONTRACT
pub fn power(a: &Numeric, b: &Numeric) -> Result<Numeric> {
    if !a.is_finite() || !b.is_finite() {
        let one = BigDecimal::from(1i32);
        if a.is_nan() {
            if let Numeric::Finite(d) = b {
                if dec_sign(d) == 0 {
                    return Ok(Numeric::from_i64(1));
                }
            }
            return Ok(Numeric::NaN);
        }
        if b.is_nan() {
            if let Numeric::Finite(d) = a {
                if *d == one {
                    return Ok(Numeric::from_i64(1));
                }
            }
            return Ok(Numeric::NaN);
        }
        let sign1 = numeric_sign(a);
        let sign2 = numeric_sign(b);
        if sign1 == 0 && sign2 < 0 {
            return Err(err_zero_neg_power());
        }
        let b_integral = match b {
            Numeric::Finite(d) => is_integral(d),
            _ => true,
        };
        if sign1 < 0 && !b_integral {
            return Err(err_complex());
        }
        if let Numeric::Finite(d) = a {
            if *d == one {
                return Ok(Numeric::from_i64(1));
            }
        }
        if sign2 == 0 {
            return Ok(Numeric::from_i64(1));
        }
        if sign1 == 0 && sign2 > 0 {
            return Ok(Numeric::zero());
        }
        if b.is_infinite() {
            let abs_x_gt_one = match a {
                Numeric::Finite(d) => {
                    if *d == -BigDecimal::from(1i32) {
                        return Ok(Numeric::from_i64(1));
                    }
                    d.abs() > one
                }
                _ => true,
            };
            return if abs_x_gt_one == (sign2 > 0) { Ok(Numeric::PosInf) } else { Ok(Numeric::zero()) };
        }
        if matches!(a, Numeric::PosInf) {
            return if sign2 > 0 { Ok(Numeric::PosInf) } else { Ok(Numeric::zero()) };
        }
        // a is -Inf, b finite
        if sign2 < 0 {
            return Ok(Numeric::zero());
        }
        if let Numeric::Finite(d) = b {
            if is_integral(d) && is_odd(d) {
                return Ok(Numeric::NegInf);
            }
        }
        return Ok(Numeric::PosInf);
    }
    match (a, b) {
        (Numeric::Finite(x), Numeric::Finite(y)) => {
            if dec_sign(x) == 0 && dec_sign(y) < 0 {
                return Err(err_zero_neg_power());
            }
            Ok(Numeric::from_decimal(power_var(x, y)?))
        }
        _ => Ok(Numeric::NaN),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::str::FromStr;

    fn n(s: &str) -> Numeric {
        Numeric::from_decimal(BigDecimal::from_str(s).unwrap())
    }
    fn s(x: Result<Numeric>) -> String {
        x.unwrap().as_decimal().unwrap().to_string()
    }

    #[test]
    fn sqrt_cases() {
        assert_eq!(s(sqrt(&n("2"))), "1.414213562373095");
        assert_eq!(s(sqrt(&n("0.00000000000000000001"))), "0.0000000001000000000000000");
        assert_eq!(sqrt(&n("-1")).unwrap_err().sqlstate, SqlState::INVALID_ARGUMENT_FOR_POWER_FUNCTION);
        assert_eq!(s(sqrt(&n("0"))), "0.000000000000000");
    }

    #[test]
    fn exp_cases() {
        assert_eq!(s(exp(&n("1"))), "2.7182818284590452");
        assert_eq!(s(exp(&n("100"))), "26881171418161354484126255515800135873611119");
        assert_eq!(s(exp(&n("0"))), "1.0000000000000000");
        assert_eq!(exp(&n("7000")).unwrap_err().sqlstate, SqlState::NUMERIC_VALUE_OUT_OF_RANGE);
    }

    #[test]
    fn ln_cases() {
        assert_eq!(s(ln(&n("10"))), "2.3025850929940457");
        assert_eq!(s(ln(&n("1"))), "0.0000000000000000");
        let e = ln(&n("0")).unwrap_err();
        assert_eq!(e.sqlstate, SqlState::INVALID_ARGUMENT_FOR_LOGARITHM);
        assert_eq!(e.message, "cannot take logarithm of zero");
        let e = ln(&n("-1")).unwrap_err();
        assert_eq!(e.message, "cannot take logarithm of a negative number");
    }

    #[test]
    fn log_cases() {
        assert_eq!(s(log10(&n("100"))), "2.0000000000000000");
        assert_eq!(s(log(&n("2"), &n("8"))), "3.0000000000000000");
        assert_eq!(log(&n("1"), &n("8")).unwrap_err().sqlstate, SqlState::DIVISION_BY_ZERO);
    }

    #[test]
    fn power_cases() {
        assert_eq!(s(power(&n("2"), &n("0.5"))), "1.4142135623730950");
        assert_eq!(s(power(&n("10"), &n("20"))), "100000000000000000000");
        assert_eq!(s(power(&n("2"), &n("-3"))), "0.1250000000000000");
        assert_eq!(s(power(&n("0"), &n("0"))), "1.0000000000000000");
        assert_eq!(
            power(&n("0"), &n("-1")).unwrap_err().sqlstate,
            SqlState::INVALID_ARGUMENT_FOR_POWER_FUNCTION
        );
        assert_eq!(
            power(&n("-2"), &n("0.5")).unwrap_err().sqlstate,
            SqlState::INVALID_ARGUMENT_FOR_POWER_FUNCTION
        );
        assert_eq!(s(power(&n("-2"), &n("3"))), "-8.0000000000000000");
        assert_eq!(power(&Numeric::NaN, &n("0")).unwrap(), Numeric::from_i64(1));
    }
}
