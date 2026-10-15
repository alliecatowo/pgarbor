//! numeric text/binary I/O and typmod enforcement (TYPE-05, TYPE-15). Lease TYP-09.
//!
//! PG18 reference: `src/backend/utils/adt/numeric.c` (`numeric_in`, `set_var_from_str`,
//! `get_str_from_var`, `numeric_send`, `numeric_recv`, `apply_typmod`, `apply_typmod_special`,
//! `numeric_typmod_precision/scale`).
//!
//! Text input:
//! - Trim leading/trailing whitespace; optional sign; then either a special (case-insensitive
//!   `NaN`, `Infinity`, `inf`, with optional sign for infinities; `-NaN` is invalid) or a number:
//!   digits with optional single `.`, optional exponent `e[+-]digits`; PG16+ also accepts
//!   `0x`/`0o`/`0b` integer literals and `_` between digits (same rules as int input).
//! - Display scale = (digits after '.') − exponent, floored at 0: `'1.50'` → 1.50, `'1e3'` → 1000,
//!   `'1.50e1'` → 15.0, `'1e-3'` → 0.001.
//! - Errors: junk → 22P02 `invalid input syntax for type numeric: "x"`; exponent beyond ±1000ish
//!   or weight overflow → 22003 `value overflows numeric format`.
//! - Then apply `ty.typmod` (`apply_typmod`).
//!
//! Typmod (`numeric(p,s)`, typmod = ((p << 16) | (s & 0x7ff)) + 4, s may be negative since PG15:
//! decode with `typmod::numeric_typmod_parts`): round half away from zero to scale `s`
//! (negative s rounds left of the point: numeric(2,-1) of 123 → 120), then if the number of digits
//! before the decimal point exceeds `p - s` → 22003 `numeric field overflow` with DETAIL
//! `A field with precision P, scale S must round to an absolute value less than 10^N.` where
//! N = p − s (when N == 0 the detail says `less than 1.`). Oracle: `123.456::numeric(4,2)` →
//! DETAIL `A field with precision 4, scale 2 must round to an absolute value less than 10^2.`
//! NaN passes any typmod; ±Infinity with a typmod → 22003 `numeric field overflow`, DETAIL
//! `A field with precision 3, scale 1 cannot hold an infinite value.`
//!
//! Text output (`numeric_out`): never scientific; exactly `dscale` fractional digits; `-` sign for
//! negatives (no `-0`: zero prints `0` / `0.00`); specials `NaN`, `Infinity`, `-Infinity`.
//!
//! Binary (`numeric_send`/`numeric_recv`), all big-endian:
//! `ndigits: i16, weight: i16, sign: u16, dscale: u16, digits: [i16; ndigits]` where digits are
//! base-10000 (NBASE) groups aligned on the decimal point, `weight` = index of the first group
//! (value = Σ digit[i] × 10000^(weight − i)), leading/trailing zero groups stripped, zero is
//! `ndigits=0, weight=0`. `sign`: 0x0000 positive, 0x4000 negative, 0xC000 NaN, 0xD000 +Infinity,
//! 0xF000 −Infinity. recv validates: bad sign → 22P03 `invalid sign in external "numeric" value`,
//! digit outside 0..9999 → 22P03 `invalid digit in external "numeric" value`, dscale > 0x3FFF →
//! 22P03 `invalid scale in external "numeric" value`; then rounds to dscale and applies typmod.
//! Example: 1.50 → ndigits 2, weight 0, sign 0, dscale 2, digits [1, 5000].
//!
//! Implementation note: like PG's `numeric_recv`, binary input *truncates* digits that dscale would
//! hide (it does not round).
use super::{Numeric, NUMERIC_MAX_DSCALE, NUMERIC_MAX_INT_DIGITS};
use crate::context::IoCtx;
use crate::datum::{Datum, TypeId};
use crate::typmod::numeric_typmod_parts;
use bigdecimal::{BigDecimal, RoundingMode};
use num_bigint::{BigInt, Sign};
use pgarbor_common::{PgError, Result, SqlState};

/// Largest accepted exponent magnitude while parsing (`PG_INT32_MAX / 2`).
const MAX_PARSE_EXPONENT: i64 = (i32::MAX / 2) as i64;

/// C-locale `isspace`.
fn is_space(b: u8) -> bool {
    matches!(b, b' ' | b'\t' | b'\n' | 0x0b | 0x0c | b'\r')
}

fn invalid_syntax(s: &str) -> PgError {
    PgError::new(SqlState::INVALID_TEXT_REPRESENTATION, format!("invalid input syntax for type numeric: \"{s}\""))
}

fn overflows() -> PgError {
    PgError::new(SqlState::NUMERIC_VALUE_OUT_OF_RANGE, "value overflows numeric format")
}

fn bad_binary(msg: &str) -> PgError {
    PgError::new(SqlState::INVALID_BINARY_REPRESENTATION, msg.to_string())
}

fn is_radix_digit(c: u8, radix: u32) -> bool {
    (c as char).is_digit(radix)
}

/// Split a decimal into (negative, magnitude digits without leading zeros ("0" for zero), scale >= 0).
fn decompose(d: &BigDecimal) -> (bool, String, i64) {
    let (int, scale) = d.as_bigint_and_exponent();
    let s = int.to_str_radix(10);
    let (neg, mut digs) = match s.strip_prefix('-') {
        Some(r) => (true, r.to_string()),
        None => (false, s),
    };
    let mut scale = scale;
    if scale < 0 {
        if digs != "0" {
            digs.extend(std::iter::repeat('0').take((-scale) as usize));
        }
        scale = 0;
    }
    (neg, digs, scale)
}

fn decimal_to_string(d: &BigDecimal) -> String {
    let (neg, digs, scale) = decompose(d);
    let scale = scale as usize;
    let mut out = String::with_capacity(digs.len() + scale + 3);
    if neg {
        out.push('-');
    }
    if scale == 0 {
        out.push_str(&digs);
    } else if digs.len() > scale {
        let split = digs.len() - scale;
        out.push_str(&digs[..split]);
        out.push('.');
        out.push_str(&digs[split..]);
    } else {
        out.push_str("0.");
        out.extend(std::iter::repeat('0').take(scale - digs.len()));
        out.push_str(&digs);
    }
    out
}

/// Parse numeric text (no typmod). Example: `parse_numeric(" -1.50 ")` → Finite(-1.50).
// CONTRACT
pub fn parse_numeric(s: &str) -> Result<Numeric> {
    let b = s.as_bytes();
    let len = b.len();
    let mut i = 0usize;
    while i < len && is_space(b[i]) {
        i += 1;
    }
    let numstart = i;
    let mut neg = false;
    if i < len && b[i] == b'+' {
        i += 1;
    } else if i < len && b[i] == b'-' {
        neg = true;
        i += 1;
    }

    let trailing_ok = |mut j: usize| -> bool {
        while j < len {
            if !is_space(b[j]) {
                return false;
            }
            j += 1;
        }
        true
    };

    let starts = |from: usize, word: &str| -> bool {
        let w = word.as_bytes();
        len >= from + w.len() && b[from..from + w.len()].eq_ignore_ascii_case(w)
    };

    let first_is_num = i < len && (b[i].is_ascii_digit() || b[i] == b'.');
    if !first_is_num {
        let (res, end) = if starts(numstart, "nan") {
            (Numeric::NaN, numstart + 3)
        } else if starts(i, "infinity") {
            (if neg { Numeric::NegInf } else { Numeric::PosInf }, i + 8)
        } else if starts(i, "inf") {
            (if neg { Numeric::NegInf } else { Numeric::PosInf }, i + 3)
        } else {
            return Err(invalid_syntax(s));
        };
        if !trailing_ok(end) {
            return Err(invalid_syntax(s));
        }
        return Ok(res);
    }

    // Non-decimal integer literal (0x / 0o / 0b).
    if b[i] == b'0' && i + 1 < len {
        let radix = match b[i + 1] {
            b'x' | b'X' => 16u32,
            b'o' | b'O' => 8,
            b'b' | b'B' => 2,
            _ => 10,
        };
        if radix != 10 {
            let mut j = i + 2;
            let mut dig: Vec<u8> = Vec::new();
            while j < len {
                let c = b[j];
                if is_radix_digit(c, radix) {
                    dig.push(c);
                    j += 1;
                } else if c == b'_' {
                    j += 1;
                    if !(j < len && is_radix_digit(b[j], radix)) {
                        return Err(invalid_syntax(s));
                    }
                } else {
                    break;
                }
            }
            if dig.is_empty() {
                return Err(invalid_syntax(s));
            }
            let v = BigInt::parse_bytes(&dig, radix).ok_or_else(|| invalid_syntax(s))?;
            if !trailing_ok(j) {
                return Err(invalid_syntax(s));
            }
            if v.to_str_radix(10).len() as i64 > NUMERIC_MAX_INT_DIGITS {
                return Err(overflows());
            }
            let v = if neg { -v } else { v };
            return Ok(Numeric::Finite(BigDecimal::new(v, 0)));
        }
    }

    // Decimal number.
    let mut j = i;
    let mut have_dp = false;
    if b[j] == b'.' {
        have_dp = true;
        j += 1;
    }
    if !(j < len && b[j].is_ascii_digit()) {
        return Err(invalid_syntax(s));
    }
    let mut digs: Vec<u8> = Vec::with_capacity(len - j);
    let mut frac: i64 = 0;
    while j < len {
        let c = b[j];
        if c.is_ascii_digit() {
            digs.push(c);
            j += 1;
            if have_dp {
                frac += 1;
            }
        } else if c == b'.' {
            if have_dp {
                return Err(invalid_syntax(s));
            }
            have_dp = true;
            j += 1;
            if j < len && b[j] == b'_' {
                return Err(invalid_syntax(s));
            }
        } else if c == b'_' {
            j += 1;
            if !(j < len && b[j].is_ascii_digit()) {
                return Err(invalid_syntax(s));
            }
        } else {
            break;
        }
    }

    let mut exp: i64 = 0;
    if j < len && (b[j] == b'e' || b[j] == b'E') {
        j += 1;
        let mut eneg = false;
        if j < len && b[j] == b'+' {
            j += 1;
        } else if j < len && b[j] == b'-' {
            eneg = true;
            j += 1;
        }
        if !(j < len && b[j].is_ascii_digit()) {
            return Err(invalid_syntax(s));
        }
        while j < len {
            let c = b[j];
            if c.is_ascii_digit() {
                exp = exp * 10 + (c - b'0') as i64;
                j += 1;
                if exp > MAX_PARSE_EXPONENT {
                    return Err(overflows());
                }
            } else if c == b'_' {
                j += 1;
                if !(j < len && b[j].is_ascii_digit()) {
                    return Err(invalid_syntax(s));
                }
            } else {
                break;
            }
        }
        if eneg {
            exp = -exp;
        }
    }
    if !trailing_ok(j) {
        return Err(invalid_syntax(s));
    }

    let scale = (frac - exp).max(0);
    if scale > NUMERIC_MAX_DSCALE {
        return Err(overflows());
    }
    let first = match digs.iter().position(|&c| c != b'0') {
        Some(p) => p,
        None => return Ok(Numeric::Finite(BigDecimal::new(BigInt::from(0), scale))),
    };
    let mut sig: Vec<u8> = digs[first..].to_vec();
    let shift = exp - frac;
    if sig.len() as i64 + shift > NUMERIC_MAX_INT_DIGITS {
        return Err(overflows());
    }
    if shift > 0 {
        sig.resize(sig.len() + shift as usize, b'0');
    }
    let v = BigInt::parse_bytes(&sig, 10).ok_or_else(|| invalid_syntax(s))?;
    let v = if neg { -v } else { v };
    Ok(Numeric::Finite(BigDecimal::new(v, scale)))
}

/// Canonical text form (`numeric_out`). Example: Finite(1.50) → "1.50".
// CONTRACT
pub fn format_numeric(n: &Numeric) -> Result<String> {
    Ok(match n {
        Numeric::NaN => "NaN".to_string(),
        Numeric::PosInf => "Infinity".to_string(),
        Numeric::NegInf => "-Infinity".to_string(),
        Numeric::Finite(d) => decimal_to_string(d),
    })
}

/// Scientific form used by `to_char(..., 'EEEE')` and float conversion helpers (`numeric_out_sci`):
/// `scale` digits after the point, exponent `e+NN`/`e-NN` (at least 2 digits). 1234.5, 2 → "1.23e+03".
// CONTRACT
pub fn format_numeric_sci(n: &Numeric, scale: i32) -> Result<String> {
    let d = match n {
        Numeric::Finite(d) => d,
        other => return format_numeric(other),
    };
    let rscale = scale.max(0) as i64;
    let (int, dscale) = d.as_bigint_and_exponent();
    if int.sign() == Sign::NoSign {
        let z = BigDecimal::new(BigInt::from(0), rscale);
        return Ok(format!("{}e+00", decimal_to_string(&z)));
    }
    let (_, digs, _) = decompose(d);
    let exponent = digs.len() as i64 - dscale - 1;
    let sig = BigDecimal::new(int, dscale + exponent).with_scale_round(rscale, RoundingMode::HalfUp);
    Ok(format!("{}e{:+03}", decimal_to_string(&sig), exponent))
}

fn typmod_overflow_special(p: i32, s: i32) -> PgError {
    PgError::new(SqlState::NUMERIC_VALUE_OUT_OF_RANGE, "numeric field overflow")
        .with_detail(format!("A field with precision {p}, scale {s} cannot hold an infinite value."))
}

/// Enforce a `numeric(p,s)` typmod (no-op for typmod < 0). See module docs for errors.
// CONTRACT
pub fn apply_typmod(n: Numeric, typmod: i32) -> Result<Numeric> {
    let (p, s) = match numeric_typmod_parts(typmod) {
        Some(ps) => ps,
        None => return Ok(n),
    };
    match n {
        Numeric::NaN => Ok(Numeric::NaN),
        Numeric::PosInf | Numeric::NegInf => Err(typmod_overflow_special(p, s)),
        Numeric::Finite(d) => {
            let r = d.with_scale_round(s as i64, RoundingMode::HalfUp);
            let (int, _) = r.as_bigint_and_exponent();
            if int.sign() != Sign::NoSign {
                let maxdigits = (p - s) as i64;
                let ddigits = r.digits() as i64 - s as i64;
                if ddigits > maxdigits {
                    let lim = if maxdigits != 0 { format!("10^{maxdigits}") } else { "1".to_string() };
                    return Err(PgError::new(SqlState::NUMERIC_VALUE_OUT_OF_RANGE, "numeric field overflow")
                        .with_detail(format!(
                            "A field with precision {p}, scale {s} must round to an absolute value less than {lim}."
                        )));
                }
            }
            let r = if s < 0 { r.with_scale(0) } else { r };
            Ok(Numeric::Finite(r))
        }
    }
}

const SIGN_POS: u16 = 0x0000;
const SIGN_NEG: u16 = 0x4000;
const SIGN_NAN: u16 = 0xC000;
const SIGN_PINF: u16 = 0xD000;
const SIGN_NINF: u16 = 0xF000;

/// Encode PG binary format (see module docs).
// CONTRACT
pub fn to_binary(n: &Numeric) -> Result<Vec<u8>> {
    let special = |sign: u16| -> Vec<u8> {
        let mut out = Vec::with_capacity(8);
        out.extend_from_slice(&0u16.to_be_bytes());
        out.extend_from_slice(&0u16.to_be_bytes());
        out.extend_from_slice(&sign.to_be_bytes());
        out.extend_from_slice(&0u16.to_be_bytes());
        out
    };
    let d = match n {
        Numeric::NaN => return Ok(special(SIGN_NAN)),
        Numeric::PosInf => return Ok(special(SIGN_PINF)),
        Numeric::NegInf => return Ok(special(SIGN_NINF)),
        Numeric::Finite(d) => d,
    };
    let (neg, digs, scale) = decompose(d);
    let scale_us = scale as usize;
    let (ip, fp): (String, String) = if digs.len() > scale_us {
        let split = digs.len() - scale_us;
        (digs[..split].trim_start_matches('0').to_string(), digs[split..].to_string())
    } else {
        let mut f = "0".repeat(scale_us - digs.len());
        f.push_str(&digs);
        (String::new(), f)
    };
    let ip_pad = (4 - ip.len() % 4) % 4;
    let fp_pad = (4 - fp.len() % 4) % 4;
    let mut all = String::with_capacity(ip.len() + fp.len() + 8);
    for _ in 0..ip_pad {
        all.push('0');
    }
    all.push_str(&ip);
    all.push_str(&fp);
    for _ in 0..fp_pad {
        all.push('0');
    }
    let int_groups = (ip.len() + ip_pad) / 4;
    let mut groups: Vec<u16> = all
        .as_bytes()
        .chunks(4)
        .map(|c| c.iter().fold(0u16, |acc, &x| acc * 10 + (x - b'0') as u16))
        .collect();
    let mut weight = int_groups as i32 - 1;
    let lead = groups.iter().take_while(|&&g| g == 0).count();
    if lead > 0 {
        groups.drain(..lead);
        weight -= lead as i32;
    }
    while groups.last() == Some(&0) {
        groups.pop();
    }
    if groups.is_empty() {
        weight = 0;
    }
    let sign = if neg && !groups.is_empty() { SIGN_NEG } else { SIGN_POS };
    let dscale = scale.min(NUMERIC_MAX_DSCALE) as u16;
    let mut out = Vec::with_capacity(8 + groups.len() * 2);
    out.extend_from_slice(&(groups.len() as u16).to_be_bytes());
    out.extend_from_slice(&(weight as i16).to_be_bytes());
    out.extend_from_slice(&sign.to_be_bytes());
    out.extend_from_slice(&dscale.to_be_bytes());
    for g in groups {
        out.extend_from_slice(&g.to_be_bytes());
    }
    Ok(out)
}

/// Decode PG binary format (see module docs), without typmod.
// CONTRACT
pub fn from_binary(buf: &[u8]) -> Result<Numeric> {
    if buf.len() < 8 {
        return Err(PgError::new(SqlState::PROTOCOL_VIOLATION, "insufficient data left in message"));
    }
    let nd = u16::from_be_bytes([buf[0], buf[1]]) as usize;
    let weight = i16::from_be_bytes([buf[2], buf[3]]) as i64;
    let sign = u16::from_be_bytes([buf[4], buf[5]]);
    let dscale = u16::from_be_bytes([buf[6], buf[7]]) as i64;
    if !matches!(sign, SIGN_POS | SIGN_NEG | SIGN_NAN | SIGN_PINF | SIGN_NINF) {
        return Err(bad_binary("invalid sign in external \"numeric\" value"));
    }
    if dscale > NUMERIC_MAX_DSCALE {
        return Err(bad_binary("invalid scale in external \"numeric\" value"));
    }
    if buf.len() < 8 + nd * 2 {
        return Err(PgError::new(SqlState::PROTOCOL_VIOLATION, "insufficient data left in message"));
    }
    let mut digits: Vec<i16> = Vec::with_capacity(nd);
    for k in 0..nd {
        let o = 8 + k * 2;
        let g = i16::from_be_bytes([buf[o], buf[o + 1]]);
        if !(0..10000).contains(&g) {
            return Err(bad_binary("invalid digit in external \"numeric\" value"));
        }
        digits.push(g);
    }
    if buf.len() > 8 + nd * 2 {
        return Err(bad_binary("incorrect binary data format"));
    }
    match sign {
        SIGN_NAN => return Ok(Numeric::NaN),
        SIGN_PINF => return Ok(Numeric::PosInf),
        SIGN_NINF => return Ok(Numeric::NegInf),
        _ => {}
    }
    if digits.iter().all(|&g| g == 0) {
        return Ok(Numeric::Finite(BigDecimal::new(BigInt::from(0), dscale)));
    }
    let mut text = String::with_capacity(nd * 4 + 8);
    for g in &digits {
        text.push_str(&format!("{:04}", g));
    }
    // value = text * 10^e
    let e = 4 * (weight - (nd as i64 - 1));
    let scale: i64;
    if e >= 0 {
        text.extend(std::iter::repeat('0').take(e as usize));
        scale = 0;
    } else {
        scale = -e;
    }
    let mut v = BigInt::parse_bytes(text.as_bytes(), 10)
        .ok_or_else(|| bad_binary("invalid digit in external \"numeric\" value"))?;
    if sign == SIGN_NEG {
        v = -v;
    }
    let d = BigDecimal::new(v, scale);
    let d = if scale > dscale { d.with_scale_round(dscale, RoundingMode::Down) } else { d.with_scale(dscale) };
    Ok(Numeric::Finite(d))
}

/// `numeric_in` (+ typmod).
pub fn input(_ctx: &IoCtx, ty: TypeId, s: &str) -> Result<Datum> {
    let n = parse_numeric(s)?;
    Ok(Datum::Numeric(apply_typmod(n, ty.typmod)?))
}

/// `numeric_out`.
pub fn output(_ctx: &IoCtx, _ty: TypeId, d: &Datum) -> Result<String> {
    match d.as_numeric() {
        Some(n) => format_numeric(n),
        None => Err(PgError::internal("numeric output called with a non-numeric datum")),
    }
}

/// `numeric_recv` (+ typmod).
pub fn recv(_ctx: &IoCtx, ty: TypeId, buf: &[u8]) -> Result<Datum> {
    let n = from_binary(buf)?;
    Ok(Datum::Numeric(apply_typmod(n, ty.typmod)?))
}

/// `numeric_send`.
pub fn send(_ctx: &IoCtx, _ty: TypeId, d: &Datum) -> Result<Vec<u8>> {
    match d.as_numeric() {
        Some(n) => to_binary(n),
        None => Err(PgError::internal("numeric send called with a non-numeric datum")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::context::IoConfig;
    use crate::registry::builtin_env;
    use crate::typmod::numeric_typmod;

    fn rt(s: &str) -> String {
        format_numeric(&parse_numeric(s).unwrap()).unwrap()
    }

    #[test]
    fn text_input() {
        assert_eq!(rt("1.50"), "1.50");
        assert_eq!(rt("1e3"), "1000");
        assert_eq!(rt("1.50e1"), "15.0");
        assert_eq!(rt(" -0.0 "), "0.0");
        assert_eq!(rt("1e-3"), "0.001");
        assert_eq!(rt(".5"), "0.5");
        assert_eq!(rt("5."), "5");
        assert_eq!(rt("1_000.000_1"), "1000.0001");
        assert_eq!(rt("0x1F"), "31");
        assert_eq!(rt("-0b101"), "-5");
        assert_eq!(rt("0o17"), "15");
        assert_eq!(rt("NaN"), "NaN");
        assert_eq!(rt(" nan "), "NaN");
        assert_eq!(rt("-Infinity"), "-Infinity");
        assert_eq!(rt("inf"), "Infinity");
        assert_eq!(rt("+INF"), "Infinity");
        assert_eq!(rt("0e100"), "0");
    }

    #[test]
    fn text_errors() {
        for bad in ["x", "", "-NaN", "1.2.3", "1e", "1_", ".", "0x", "1 2", "1._5"] {
            let e = parse_numeric(bad).unwrap_err();
            assert_eq!(e.sqlstate, SqlState::INVALID_TEXT_REPRESENTATION, "{bad}");
            assert_eq!(e.message, format!("invalid input syntax for type numeric: \"{bad}\""));
        }
        let e = parse_numeric("1e2000000000").unwrap_err();
        assert_eq!(e.sqlstate, SqlState::NUMERIC_VALUE_OUT_OF_RANGE);
        assert_eq!(e.message, "value overflows numeric format");
    }

    #[test]
    fn typmod() {
        let tm = numeric_typmod(4, 2);
        let e = apply_typmod(parse_numeric("123.456").unwrap(), tm).unwrap_err();
        assert_eq!(e.sqlstate, SqlState::NUMERIC_VALUE_OUT_OF_RANGE);
        assert_eq!(e.message, "numeric field overflow");
        assert_eq!(
            e.detail.as_deref(),
            Some("A field with precision 4, scale 2 must round to an absolute value less than 10^2.")
        );
        let e = apply_typmod(Numeric::PosInf, numeric_typmod(3, 1)).unwrap_err();
        assert_eq!(e.detail.as_deref(), Some("A field with precision 3, scale 1 cannot hold an infinite value."));
        let r = apply_typmod(parse_numeric("1.005").unwrap(), numeric_typmod(5, 2)).unwrap();
        assert_eq!(format_numeric(&r).unwrap(), "1.01");
        let r = apply_typmod(parse_numeric("123").unwrap(), numeric_typmod(2, -1)).unwrap();
        assert_eq!(format_numeric(&r).unwrap(), "120");
        let r = apply_typmod(parse_numeric("5").unwrap(), numeric_typmod(3, 2)).unwrap();
        assert_eq!(format_numeric(&r).unwrap(), "5.00");
        assert!(apply_typmod(Numeric::NaN, numeric_typmod(3, 1)).unwrap().is_nan());
        assert!(apply_typmod(parse_numeric("0.0001234").unwrap(), numeric_typmod(5, 6)).is_ok());
        assert!(apply_typmod(parse_numeric("0.1234").unwrap(), numeric_typmod(5, 6)).is_err());
        let r = apply_typmod(parse_numeric("12345").unwrap(), -1).unwrap();
        assert_eq!(format_numeric(&r).unwrap(), "12345");
    }

    #[test]
    fn sci() {
        let n = parse_numeric("1234.5").unwrap();
        assert_eq!(format_numeric_sci(&n, 2).unwrap(), "1.23e+03");
        let n = parse_numeric("0.00123").unwrap();
        assert_eq!(format_numeric_sci(&n, 1).unwrap(), "1.2e-03");
        assert_eq!(format_numeric_sci(&Numeric::zero(), 2).unwrap(), "0.00e+00");
    }

    #[test]
    fn binary() {
        let n = parse_numeric("1.50").unwrap();
        assert_eq!(to_binary(&n).unwrap(), vec![0, 2, 0, 0, 0, 0, 0, 2, 0, 1, 0x13, 0x88]);
        for s in ["0", "-12345.6789", "NaN", "Infinity", "-Infinity", "0.5", "0.00001", "100000000", "1.50", "0.00"] {
            let n = parse_numeric(s).unwrap();
            let back = from_binary(&to_binary(&n).unwrap()).unwrap();
            assert_eq!(format_numeric(&back).unwrap(), format_numeric(&n).unwrap(), "{s}");
        }
        assert_eq!(to_binary(&Numeric::zero()).unwrap(), vec![0, 0, 0, 0, 0, 0, 0, 0]);
        let e = from_binary(&[0, 0, 0, 0, 0x12, 0x34, 0, 0]).unwrap_err();
        assert_eq!(e.sqlstate, SqlState::INVALID_BINARY_REPRESENTATION);
        assert_eq!(e.message, "invalid sign in external \"numeric\" value");
        let e = from_binary(&[0, 1, 0, 0, 0, 0, 0, 0, 0x27, 0x10]).unwrap_err();
        assert_eq!(e.message, "invalid digit in external \"numeric\" value");
        let e = from_binary(&[0, 0, 0, 0, 0, 0, 0x40, 0]).unwrap_err();
        assert_eq!(e.message, "invalid scale in external \"numeric\" value");
    }

    #[test]
    fn datum_io() {
        let cfg = IoConfig::default();
        let ctx = IoCtx::new(builtin_env(), &cfg);
        let ty = TypeId::new(crate::oids::NUMERIC, numeric_typmod(5, 2));
        let d = input(&ctx, ty, "1.005").unwrap();
        assert_eq!(output(&ctx, ty, &d).unwrap(), "1.01");
        let bytes = send(&ctx, ty, &d).unwrap();
        let d2 = recv(&ctx, ty, &bytes).unwrap();
        assert_eq!(output(&ctx, ty, &d2).unwrap(), "1.01");
    }
}
