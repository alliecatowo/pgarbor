//! Core numeric arithmetic with PostgreSQL result-scale rules (TYPE-05). Lease TYP-10 (sonnet).
//!
//! PG18 reference: `src/backend/utils/adt/numeric.c` — `add_var`, `sub_var`, `mul_var`, `div_var`,
//! `select_div_scale`, `mod_var`, `div_mod_var`, `round_var`, `trunc_var`, `ceil_var`, `floor_var`,
//! `numeric_add_opt_error` etc. (special-value tables in `numeric_add/sub/mul/div/mod`).
//!
//! Exact results are computed with BigDecimal, then rounded with
//! `with_scale_round(rscale, RoundingMode::HalfUp)` (half away from zero). NEVER rely on
//! bigdecimal's default division precision. Every finite result must satisfy the `Numeric`
//! invariant (scale in 0..=0x3FFF) and the size limit (at most 131072 digits before the point,
//! else 22003 `value overflows numeric format`).
//!
//! Result display scales (PG):
//! - add/sub: `max(dscale1, dscale2)`. mul: `dscale1 + dscale2` (if > 0x3FFF, round to 0x3FFF).
//! - div (`/`): `select_div_scale`, which works in base-10000 groups:
//!   `weight(x)` = index of the most significant NBASE group of |x| (x = Σ g_i·10000^w_i; e.g.
//!   weight(1) = 0, weight(12345) = 1, weight(0.5) = -1), `first(x)` = value of that leading group.
//!   `qweight = weight1 − weight2; if first1 <= first2 { qweight -= 1 }`;
//!   `rscale = 16 − qweight*4; rscale = max(rscale, dscale1, dscale2, 0); rscale = min(rscale, 1000)`.
//!   (Zero operands: weight 0, first 0.) Oracle: `1::numeric/3` → `0.33333333333333333333`,
//!   `-1.5::numeric/7` → `-0.21428571428571428571`, `1e20::numeric/7` → `14285714285714285714`,
//!   `100::numeric/0.03` → `3333.3333333333333333`, `5.0/2` → `2.5000000000000000`.
//! - `div_trunc` (`div(a,b)`): truncate toward zero to scale 0 (result dscale 0).
//! - mod (`%`, `mod(a,b)`): `a - trunc(a/b)*b`, dscale = max(dscale1, dscale2); sign of dividend.
//! - Division/mod by zero (finite zero divisor) → 22012 `division by zero`.
//!
//! Special values (PG18 `numeric_add` etc.): any NaN operand → NaN. add: Inf + −Inf = NaN, Inf + x = Inf.
//! sub: Inf − Inf = NaN. mul: Inf × 0 = NaN, Inf × (±x) = ±Inf by sign. div: x / ±Inf = 0 (finite x),
//! Inf / Inf = NaN, ±Inf / finite nonzero = ±Inf by sign, Inf / 0 → 22012 `division by zero`.
//! mod: Inf % x = NaN, x % Inf = x, x % 0 → 22012.
use super::{
    Numeric, NUMERIC_MAX_DISPLAY_SCALE, NUMERIC_MAX_DSCALE, NUMERIC_MAX_INT_DIGITS,
    NUMERIC_MIN_DISPLAY_SCALE, NUMERIC_MIN_SIG_DIGITS,
};
use bigdecimal::BigDecimal;
use num_bigint::{BigInt, Sign};
use pgarbor_common::{PgError, Result, SqlState};
use std::cmp::Ordering;

/// PG `NUMERIC_MAX_RESULT_SCALE` (clamp for explicit round/trunc scales).
const MAX_RESULT_SCALE: i32 = 2000;

fn div_by_zero() -> PgError {
    PgError::new(SqlState::DIVISION_BY_ZERO, "division by zero")
}

fn overflow() -> PgError {
    PgError::new(SqlState::NUMERIC_VALUE_OUT_OF_RANGE, "value overflows numeric format")
}

fn pow10(n: i64) -> BigInt {
    BigInt::from(10u32).pow(n.max(0) as u32)
}

fn is_zero(i: &BigInt) -> bool {
    i.sign() == Sign::NoSign
}

fn abs_int(i: BigInt) -> BigInt {
    if i.sign() == Sign::Minus {
        -i
    } else {
        i
    }
}

/// Unscaled integer and (non-negative) scale of a finite value.
fn parts(d: &BigDecimal) -> (BigInt, i64) {
    let (i, s) = d.as_bigint_and_exponent();
    if s < 0 {
        (i * pow10(-s), 0)
    } else {
        (i, s)
    }
}

/// Rescale an unscaled integer from `from` to `to` fractional digits, either rounding half away
/// from zero (`half_up`) or truncating toward zero. `to` may be negative.
fn rescale_int(i: &BigInt, from: i64, to: i64, half_up: bool) -> BigInt {
    match to.cmp(&from) {
        Ordering::Equal => i.clone(),
        Ordering::Greater => i * pow10(to - from),
        Ordering::Less => {
            let d = pow10(from - to);
            let q = i / &d;
            if !half_up {
                return q;
            }
            let r = i - &q * &d;
            let twice = abs_int(r) * 2;
            if twice >= d {
                if i.sign() == Sign::Minus {
                    q - 1
                } else {
                    q + 1
                }
            } else {
                q
            }
        }
    }
}

fn digit_count(i: &BigInt) -> i64 {
    i.magnitude().to_str_radix(10).len() as i64
}

/// Build a finite Numeric from unscaled integer and scale, enforcing the PG limits.
fn mk(i: BigInt, scale: i64) -> Result<Numeric> {
    let (i, scale) = if scale > NUMERIC_MAX_DSCALE {
        (rescale_int(&i, scale, NUMERIC_MAX_DSCALE, true), NUMERIC_MAX_DSCALE)
    } else if scale < 0 {
        (i * pow10(-scale), 0)
    } else {
        (i, scale)
    };
    // 131072 decimal digits need about 435_412 bits; skip the exact count when far below.
    if i.bits() > 435_000 && digit_count(&i) - scale > NUMERIC_MAX_INT_DIGITS {
        return Err(overflow());
    }
    Ok(Numeric::Finite(BigDecimal::new(i, scale)))
}

fn sign_of(a: &Numeric) -> i32 {
    match a {
        Numeric::Finite(d) => match d.as_bigint_and_exponent().0.sign() {
            Sign::Plus => 1,
            Sign::Minus => -1,
            Sign::NoSign => 0,
        },
        Numeric::PosInf => 1,
        Numeric::NegInf => -1,
        Numeric::NaN => 0,
    }
}

fn inf_of(sign: i32) -> Numeric {
    if sign < 0 {
        Numeric::NegInf
    } else {
        Numeric::PosInf
    }
}

/// `a + b`.
// CONTRACT
pub fn add(a: &Numeric, b: &Numeric) -> Result<Numeric> {
    match (a, b) {
        (Numeric::NaN, _) | (_, Numeric::NaN) => Ok(Numeric::NaN),
        (Numeric::PosInf, Numeric::NegInf) | (Numeric::NegInf, Numeric::PosInf) => Ok(Numeric::NaN),
        (Numeric::PosInf, _) | (_, Numeric::PosInf) => Ok(Numeric::PosInf),
        (Numeric::NegInf, _) | (_, Numeric::NegInf) => Ok(Numeric::NegInf),
        (Numeric::Finite(x), Numeric::Finite(y)) => {
            let (xi, xs) = parts(x);
            let (yi, ys) = parts(y);
            let s = xs.max(ys);
            let sum = xi * pow10(s - xs) + yi * pow10(s - ys);
            mk(sum, s)
        }
    }
}

/// `a - b`.
// CONTRACT
pub fn sub(a: &Numeric, b: &Numeric) -> Result<Numeric> {
    match (a, b) {
        (Numeric::Finite(x), Numeric::Finite(y)) => {
            let (xi, xs) = parts(x);
            let (yi, ys) = parts(y);
            let s = xs.max(ys);
            let diff = xi * pow10(s - xs) - yi * pow10(s - ys);
            mk(diff, s)
        }
        _ => add(a, &neg(b)?),
    }
}

/// `a * b`.
// CONTRACT
pub fn mul(a: &Numeric, b: &Numeric) -> Result<Numeric> {
    match (a, b) {
        (Numeric::NaN, _) | (_, Numeric::NaN) => Ok(Numeric::NaN),
        (Numeric::Finite(x), Numeric::Finite(y)) => {
            let (xi, xs) = parts(x);
            let (yi, ys) = parts(y);
            mk(xi * yi, xs + ys)
        }
        _ => {
            // At least one infinity, no NaN.
            let (sa, sb) = (sign_of(a), sign_of(b));
            if sa == 0 || sb == 0 {
                Ok(Numeric::NaN)
            } else {
                Ok(inf_of(sa * sb))
            }
        }
    }
}

/// Division special-value handling shared by `/`, `div_scale` and `div_trunc`.
/// `Ok(Some(v))` is a final result; `Ok(None)` means both operands are finite and b is nonzero.
fn div_special(a: &Numeric, b: &Numeric) -> Result<Option<Numeric>> {
    match (a, b) {
        (Numeric::NaN, _) | (_, Numeric::NaN) => Ok(Some(Numeric::NaN)),
        (Numeric::PosInf | Numeric::NegInf, Numeric::PosInf | Numeric::NegInf) => {
            Ok(Some(Numeric::NaN))
        }
        (Numeric::PosInf | Numeric::NegInf, Numeric::Finite(_)) => match sign_of(b) {
            0 => Err(div_by_zero()),
            s => Ok(Some(inf_of(sign_of(a) * s))),
        },
        (Numeric::Finite(_), Numeric::PosInf | Numeric::NegInf) => Ok(Some(Numeric::zero())),
        (Numeric::Finite(_), Numeric::Finite(_)) => {
            if sign_of(b) == 0 {
                Err(div_by_zero())
            } else {
                Ok(None)
            }
        }
    }
}

/// Quotient `(xi/10^xs) / (yi/10^ys)` computed at `rscale` fractional digits (unscaled result).
fn div_core(xi: &BigInt, xs: i64, yi: &BigInt, ys: i64, rscale: i64, half_up: bool) -> BigInt {
    let num = xi * pow10(ys + rscale);
    let den = yi * pow10(xs);
    let q = &num / &den;
    if !half_up {
        return q;
    }
    let r = &num - &q * &den;
    if is_zero(&r) {
        return q;
    }
    if abs_int(r * 2) >= abs_int(den.clone()) {
        let neg_q = (num.sign() == Sign::Minus) != (den.sign() == Sign::Minus);
        if neg_q {
            q - 1
        } else {
            q + 1
        }
    } else {
        q
    }
}

/// `a / b` with `select_div_scale`.
// CONTRACT
pub fn div(a: &Numeric, b: &Numeric) -> Result<Numeric> {
    if let Some(v) = div_special(a, b)? {
        return Ok(v);
    }
    let rscale = select_div_scale(a, b)?;
    div_scale(a, b, rscale)
}

/// `a / b` rounded to an explicit result scale (used by avg, transcendental helpers).
// CONTRACT
pub fn div_scale(a: &Numeric, b: &Numeric, rscale: i64) -> Result<Numeric> {
    if let Some(v) = div_special(a, b)? {
        return Ok(v);
    }
    if let (Numeric::Finite(x), Numeric::Finite(y)) = (a, b) {
        let rscale = rscale.clamp(0, NUMERIC_MAX_DSCALE);
        let (xi, xs) = parts(x);
        let (yi, ys) = parts(y);
        mk(div_core(&xi, xs, &yi, ys, rscale, true), rscale)
    } else {
        Err(PgError::internal("div_scale: unreachable operand combination"))
    }
}

/// Base-10000 (weight, leading group) of a finite value; zero gives (0, 0).
fn weight_first(d: &BigDecimal) -> (i64, BigInt) {
    let (i, s) = parts(d);
    if is_zero(&i) {
        return (0, BigInt::from(0));
    }
    let i = abs_int(i);
    // Digits before the decimal point (<= 0 for pure fractions).
    let n = digit_count(&i) - s;
    let w = if n >= 1 {
        (n - 1) / 4
    } else {
        // First significant digit sits at fractional position (-n + 1).
        -((-n) / 4 + 1)
    };
    // first = floor(|x| / 10000^w) = i * 10^(-4w - s)
    let e = -4 * w - s;
    let first = if e >= 0 { i * pow10(e) } else { i / pow10(-e) };
    (w, first)
}

/// The display scale `select_div_scale` would choose for `a / b` (finite operands).
// CONTRACT
pub fn select_div_scale(a: &Numeric, b: &Numeric) -> Result<i64> {
    match (a, b) {
        (Numeric::Finite(x), Numeric::Finite(y)) => {
            let (w1, f1) = weight_first(x);
            let (w2, f2) = weight_first(y);
            let mut qweight = w1 - w2;
            if f1 <= f2 {
                qweight -= 1;
            }
            let mut rscale = NUMERIC_MIN_SIG_DIGITS - qweight * 4;
            rscale = rscale.max(a.dscale()).max(b.dscale()).max(NUMERIC_MIN_DISPLAY_SCALE);
            Ok(rscale.min(NUMERIC_MAX_DISPLAY_SCALE))
        }
        _ => Err(PgError::internal("select_div_scale requires finite operands")),
    }
}

/// `div(a, b)`: truncated integer quotient.
// CONTRACT
pub fn div_trunc(a: &Numeric, b: &Numeric) -> Result<Numeric> {
    if let Some(v) = div_special(a, b)? {
        return Ok(v);
    }
    if let (Numeric::Finite(x), Numeric::Finite(y)) = (a, b) {
        let (xi, xs) = parts(x);
        let (yi, ys) = parts(y);
        mk(div_core(&xi, xs, &yi, ys, 0, false), 0)
    } else {
        Err(PgError::internal("div_trunc: unreachable operand combination"))
    }
}

/// `a % b` / `mod(a, b)`.
// CONTRACT
pub fn modulo(a: &Numeric, b: &Numeric) -> Result<Numeric> {
    match (a, b) {
        (Numeric::NaN, _) | (_, Numeric::NaN) => Ok(Numeric::NaN),
        (Numeric::PosInf | Numeric::NegInf, _) => Ok(Numeric::NaN),
        (Numeric::Finite(_), Numeric::PosInf | Numeric::NegInf) => Ok(a.clone()),
        (Numeric::Finite(x), Numeric::Finite(y)) => {
            let (xi, xs) = parts(x);
            let (yi, ys) = parts(y);
            if is_zero(&yi) {
                return Err(div_by_zero());
            }
            let s = xs.max(ys);
            let r = (xi * pow10(s - xs)) % (yi * pow10(s - ys));
            mk(r, s)
        }
    }
}

/// Unary minus (`-NaN` = NaN, `-Inf` = −Inf; never produces negative zero).
// CONTRACT
pub fn neg(a: &Numeric) -> Result<Numeric> {
    Ok(match a {
        Numeric::NaN => Numeric::NaN,
        Numeric::PosInf => Numeric::NegInf,
        Numeric::NegInf => Numeric::PosInf,
        Numeric::Finite(d) => {
            let (i, s) = parts(d);
            Numeric::Finite(BigDecimal::new(-i, s))
        }
    })
}

/// `abs(a)`.
// CONTRACT
pub fn abs(a: &Numeric) -> Result<Numeric> {
    Ok(match a {
        Numeric::NegInf | Numeric::PosInf => Numeric::PosInf,
        Numeric::NaN => Numeric::NaN,
        Numeric::Finite(d) => {
            let (i, s) = parts(d);
            Numeric::Finite(BigDecimal::new(abs_int(i), s))
        }
    })
}

fn round_or_trunc(a: &Numeric, s: i32, half_up: bool) -> Result<Numeric> {
    match a {
        Numeric::Finite(d) => {
            let (i, sc) = parts(d);
            let t = i64::from(s.clamp(-MAX_RESULT_SCALE, MAX_RESULT_SCALE));
            let r = rescale_int(&i, sc, t, half_up);
            if t < 0 {
                mk(r * pow10(-t), 0)
            } else {
                mk(r, t)
            }
        }
        other => Ok(other.clone()),
    }
}

/// `round(a, s)`: half away from zero; `s` may be negative (`round(1234.5678, -2)` → `1200`);
/// result dscale = max(s, 0). Specials pass through.
// CONTRACT
pub fn round(a: &Numeric, s: i32) -> Result<Numeric> {
    round_or_trunc(a, s, true)
}

/// `trunc(a, s)`: toward zero, same scale conventions as `round`.
// CONTRACT
pub fn trunc(a: &Numeric, s: i32) -> Result<Numeric> {
    round_or_trunc(a, s, false)
}

/// `ceil(a)` (dscale 0; `ceil(-1.5)` = `-1`).
// CONTRACT
pub fn ceil(a: &Numeric) -> Result<Numeric> {
    match a {
        Numeric::Finite(d) => {
            let (i, s) = parts(d);
            let p = pow10(s);
            let q = &i / &p;
            let has_rem = !is_zero(&(&i - &q * &p));
            mk(if has_rem && i.sign() == Sign::Plus { q + 1 } else { q }, 0)
        }
        other => Ok(other.clone()),
    }
}

/// `floor(a)` (dscale 0; `floor(-1.5)` = `-2`).
// CONTRACT
pub fn floor(a: &Numeric) -> Result<Numeric> {
    match a {
        Numeric::Finite(d) => {
            let (i, s) = parts(d);
            let p = pow10(s);
            let q = &i / &p;
            let has_rem = !is_zero(&(&i - &q * &p));
            mk(if has_rem && i.sign() == Sign::Minus { q - 1 } else { q }, 0)
        }
        other => Ok(other.clone()),
    }
}

/// `sign(a)`: -1, 0, 1 with dscale 0 (NaN → NaN, ±Inf → ±1).
// CONTRACT
pub fn sign(a: &Numeric) -> Result<Numeric> {
    Ok(match a {
        Numeric::NaN => Numeric::NaN,
        other => Numeric::from_i64(i64::from(sign_of(other))),
    })
}

/// Check the PG size limit for a finite value (≤ 131072 integer digits, ≤ 16383 fractional digits).
// CONTRACT
pub fn check_range(a: Numeric) -> Result<Numeric> {
    match a {
        Numeric::Finite(d) => {
            let (i, s) = d.as_bigint_and_exponent();
            mk(i, s)
        }
        other => Ok(other),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::str::FromStr;

    fn n(s: &str) -> Numeric {
        Numeric::from_decimal(BigDecimal::from_str(s).unwrap())
    }

    /// Plain decimal rendering using the stored scale.
    fn ds(x: &Numeric) -> String {
        match x {
            Numeric::NaN => "NaN".into(),
            Numeric::PosInf => "Infinity".into(),
            Numeric::NegInf => "-Infinity".into(),
            Numeric::Finite(d) => {
                let (i, s) = d.as_bigint_and_exponent();
                let negative = i.sign() == Sign::Minus;
                let mut digits = abs_int(i).to_string();
                let s = s as usize;
                if s > 0 {
                    while digits.len() <= s {
                        digits.insert(0, '0');
                    }
                    digits.insert(digits.len() - s, '.');
                }
                if negative {
                    format!("-{digits}")
                } else {
                    digits
                }
            }
        }
    }

    #[test]
    fn division_scales() {
        assert_eq!(ds(&div(&n("1"), &n("3")).unwrap()), "0.33333333333333333333");
        assert_eq!(ds(&div(&n("-1.5"), &n("7")).unwrap()), "-0.21428571428571428571");
        assert_eq!(ds(&div(&n("1e20"), &n("7")).unwrap()), "14285714285714285714");
        assert_eq!(ds(&div(&n("100"), &n("0.03")).unwrap()), "3333.3333333333333333");
        assert_eq!(ds(&div(&n("5.0"), &n("2")).unwrap()), "2.5000000000000000");
    }

    #[test]
    fn division_by_zero() {
        let e = div(&n("1"), &n("0")).unwrap_err();
        assert_eq!(e.sqlstate, SqlState::DIVISION_BY_ZERO);
        assert!(modulo(&n("1"), &n("0")).is_err());
        assert!(div_trunc(&n("1"), &n("0.0")).is_err());
        assert!(div(&Numeric::PosInf, &n("0")).is_err());
    }

    #[test]
    fn mod_and_div_trunc() {
        assert_eq!(ds(&modulo(&n("10.5"), &n("3")).unwrap()), "1.5");
        assert_eq!(ds(&modulo(&n("-10.5"), &n("3")).unwrap()), "-1.5");
        assert_eq!(ds(&div_trunc(&n("10.5"), &n("3")).unwrap()), "3");
        assert_eq!(ds(&div_trunc(&n("-10.5"), &n("3")).unwrap()), "-3");
    }

    #[test]
    fn rounding() {
        assert_eq!(ds(&round(&n("1234.5678"), -2).unwrap()), "1200");
        assert_eq!(ds(&round(&n("2.5"), 0).unwrap()), "3");
        assert_eq!(ds(&round(&n("-2.5"), 0).unwrap()), "-3");
        assert_eq!(ds(&round(&n("1.5"), 3).unwrap()), "1.500");
        assert_eq!(ds(&trunc(&n("-1.99"), 1).unwrap()), "-1.9");
        assert_eq!(ds(&ceil(&n("-1.5")).unwrap()), "-1");
        assert_eq!(ds(&floor(&n("-1.5")).unwrap()), "-2");
        assert_eq!(ds(&ceil(&n("1.2")).unwrap()), "2");
        assert_eq!(ds(&floor(&n("1.2")).unwrap()), "1");
    }

    #[test]
    fn specials() {
        assert!(add(&Numeric::PosInf, &Numeric::NegInf).unwrap().is_nan());
        assert!(mul(&Numeric::PosInf, &n("0")).unwrap().is_nan());
        assert_eq!(mul(&Numeric::PosInf, &n("-2")).unwrap(), Numeric::NegInf);
        assert!(sub(&Numeric::PosInf, &Numeric::PosInf).unwrap().is_nan());
        assert_eq!(div(&n("5"), &Numeric::NegInf).unwrap(), n("0"));
        assert_eq!(modulo(&n("5"), &Numeric::PosInf).unwrap(), n("5"));
        assert!(modulo(&Numeric::PosInf, &n("5")).unwrap().is_nan());
        assert_eq!(sign(&Numeric::NegInf).unwrap(), n("-1"));
    }

    #[test]
    fn add_sub_mul_scales() {
        assert_eq!(ds(&add(&n("0.1"), &n("0.2")).unwrap()), "0.3");
        assert_eq!(ds(&add(&n("1.10"), &n("2.2")).unwrap()), "3.30");
        assert_eq!(ds(&sub(&n("1"), &n("1.50")).unwrap()), "-0.50");
        assert_eq!(ds(&mul(&n("1.5"), &n("2.25")).unwrap()), "3.375");
        assert_eq!(ds(&neg(&n("0.0")).unwrap()), "0.0");
        assert_eq!(ds(&abs(&n("-3.20")).unwrap()), "3.20");
    }
}
