//! `float4` / `float8` text and binary I/O (TYPE-01, TYPE-15). Lease TYP-06.
//!
//! PG18 reference: `src/backend/utils/adt/float.c` (`float8in_internal`, `float8out_internal`),
//! `src/common/d2s.c` / `f2s.c` (shortest round-trip formatting).
//!
//! Text input (`float8in`/`float4in`):
//! - Trim leading/trailing whitespace. Accept (case-insensitive) `NaN`, `Infinity`, `+Infinity`,
//!   `-Infinity`, `inf`, `+inf`, `-inf`; otherwise a C `strtod` decimal/scientific number
//!   (hex floats are NOT accepted: `0x1p3` → 22P02). No underscores.
//! - Junk / empty → 22P02 `invalid input syntax for type double precision: "abc"`
//!   (float4: `... for type real: "..."`).
//! - Overflow (finite input parses to ±inf) → 22003 `"1e400" is out of range for type double precision`
//!   (float4: `"1e100" is out of range for type real` — float4 range is checked after parsing as f64).
//! - Underflow: a nonzero input that parses to 0 → 22003 `"1e-400" is out of range for type double precision`.
//!   Subnormals are fine (`float8 '1e-320'` → `1e-320`, `float4 '1e-40'` → `1e-40`).
//!
//! Text output with `extra_float_digits > 0` (the pinned default is 1): SHORTEST round-trip digits
//! (Ryu). Format: let `exp` be the decimal exponent of the first significant digit
//! (value = d.ddd × 10^exp). float8: if `-4 <= exp < 15` print fixed notation, else scientific;
//! float4: same with `-4 <= exp < 6`. Scientific form: `<digits with '.' after the first>e<sign><at least 2 digits>`.
//! Rust's `format!("{:e}", v)` yields exactly the shortest digits (e.g. "1.2345678901234568e17"); reformat it.
//! Oracle fixtures (float8): 1e15 → `1e+15`; 1e14 → `100000000000000`; 123456789012345678 →
//! `1.2345678901234568e+17`; 0.0001 → `0.0001`; 0.00001 → `1e-05`; 9.99e14 → `999000000000000`;
//! -1.5e-7 → `-1.5e-07`; 9007199254740993 → `9.007199254740992e+15`; 0.1::float4::float8 →
//! `0.10000000149011612`; `-0` → `-0`; NaN → `NaN`; ±inf → `Infinity` / `-Infinity`.
//! (float4): 1e6 → `1e+06`; 123456 → `123456`; 1.5e-5 → `1.5e-05`; 1e15 → `1e+15`; 0.1 → `0.1`.
//! With `extra_float_digits <= 0`: C `%.*g` with precision `DBL_DIG + extra_float_digits` (15+n) for
//! float8, `FLT_DIG + n` (6+n) for float4 (e.g. extra_float_digits=0: 0.1::float4::float8 → `0.100000001490116`).
//!
//! Binary: IEEE-754 bits big-endian (4 / 8 bytes).
use crate::context::IoCtx;
use crate::datum::{Datum, TypeId};
use crate::oids;
use pgarbor_common::{PgError, Result, SqlState};

fn is_c_space(c: char) -> bool {
    matches!(c, ' ' | '\t' | '\n' | '\x0b' | '\x0c' | '\r')
}

fn bad_syntax(s: &str, tname: &str) -> PgError {
    PgError::new(
        SqlState::INVALID_TEXT_REPRESENTATION,
        format!("invalid input syntax for type {}: \"{}\"", tname, s),
    )
}

fn out_of_range(s: &str, tname: &str) -> PgError {
    PgError::new(
        SqlState::NUMERIC_VALUE_OUT_OF_RANGE,
        format!("\"{}\" is out of range for type {}", s, tname),
    )
}

/// Classification of a syntactically valid float literal.
enum Lit<'a> {
    Nan,
    Inf(bool),
    /// (numeric text without surrounding whitespace, has a nonzero mantissa digit)
    Num(&'a str, bool),
}

/// Validate the strtod-style decimal syntax (no hex, no underscores) of already trimmed text.
fn classify(t: &str) -> Option<Lit<'_>> {
    let b = t.as_bytes();
    let mut i = 0;
    let mut neg = false;
    if i < b.len() && (b[i] == b'+' || b[i] == b'-') {
        neg = b[i] == b'-';
        i += 1;
    }
    let rest = &t[i..];
    if rest.eq_ignore_ascii_case("nan") {
        return Some(Lit::Nan);
    }
    if rest.eq_ignore_ascii_case("inf") || rest.eq_ignore_ascii_case("infinity") {
        return Some(Lit::Inf(neg));
    }
    let mut digits = 0usize;
    let mut nonzero = false;
    while i < b.len() && b[i].is_ascii_digit() {
        nonzero |= b[i] != b'0';
        digits += 1;
        i += 1;
    }
    if i < b.len() && b[i] == b'.' {
        i += 1;
        while i < b.len() && b[i].is_ascii_digit() {
            nonzero |= b[i] != b'0';
            digits += 1;
            i += 1;
        }
    }
    if digits == 0 {
        return None;
    }
    if i < b.len() && (b[i] == b'e' || b[i] == b'E') {
        i += 1;
        if i < b.len() && (b[i] == b'+' || b[i] == b'-') {
            i += 1;
        }
        let st = i;
        while i < b.len() && b[i].is_ascii_digit() {
            i += 1;
        }
        if i == st {
            return None;
        }
    }
    if i != b.len() {
        return None;
    }
    Some(Lit::Num(t, nonzero))
}

/// Parse float8 text per module rules. `type_name` for messages is "double precision".
// CONTRACT
pub fn parse_float8(s: &str) -> Result<f64> {
    const T: &str = "double precision";
    let t = s.trim_matches(is_c_space);
    match classify(t) {
        None => Err(bad_syntax(s, T)),
        Some(Lit::Nan) => Ok(f64::NAN),
        Some(Lit::Inf(neg)) => Ok(if neg { f64::NEG_INFINITY } else { f64::INFINITY }),
        Some(Lit::Num(n, nonzero)) => {
            let v: f64 = n.parse().map_err(|_| bad_syntax(s, T))?;
            if v.is_infinite() || (v == 0.0 && nonzero) {
                return Err(out_of_range(s, T));
            }
            Ok(v)
        }
    }
}

/// Parse float4 text per module rules (`real`).
// CONTRACT
pub fn parse_float4(s: &str) -> Result<f32> {
    const T: &str = "real";
    let t = s.trim_matches(is_c_space);
    match classify(t) {
        None => Err(bad_syntax(s, T)),
        Some(Lit::Nan) => Ok(f32::NAN),
        Some(Lit::Inf(neg)) => Ok(if neg { f32::NEG_INFINITY } else { f32::INFINITY }),
        Some(Lit::Num(n, nonzero)) => {
            // Parse directly to f32 (correctly rounded; avoids double rounding), like strtof.
            let v: f32 = n.parse().map_err(|_| bad_syntax(s, T))?;
            if v.is_infinite() || (v == 0.0 && nonzero) {
                return Err(out_of_range(s, T));
            }
            Ok(v)
        }
    }
}

/// Split Rust `{:e}` output into (sign, digit string without '.', decimal exponent).
fn split_sci(e: &str) -> (&str, String, i32) {
    let (neg, e) = match e.strip_prefix('-') {
        Some(r) => ("-", r),
        None => ("", e),
    };
    let (m, x) = match e.split_once('e') {
        Some((m, x)) => (m, x.parse::<i32>().unwrap_or(0)),
        None => (e, 0),
    };
    let digits: String = m.chars().filter(|c| *c != '.').collect();
    (neg, digits, x)
}

fn push_exp(out: &mut String, exp: i32) {
    out.push('e');
    out.push(if exp < 0 { '-' } else { '+' });
    let a = exp.unsigned_abs();
    if a < 10 {
        out.push('0');
    }
    out.push_str(&a.to_string());
}

/// Shortest round-trip layout. `sci` is Rust `{:e}` output of a finite value.
fn layout_shortest(sci: &str, fixed_max: i32) -> String {
    let (neg, digits, exp) = split_sci(sci);
    let mut out = String::with_capacity(digits.len() + 8);
    out.push_str(neg);
    if (-4..fixed_max).contains(&exp) {
        if exp >= 0 {
            let int_len = exp as usize + 1;
            if digits.len() <= int_len {
                out.push_str(&digits);
                for _ in digits.len()..int_len {
                    out.push('0');
                }
            } else {
                out.push_str(&digits[..int_len]);
                out.push('.');
                out.push_str(&digits[int_len..]);
            }
        } else {
            out.push_str("0.");
            for _ in 0..(-exp - 1) {
                out.push('0');
            }
            out.push_str(&digits);
        }
    } else {
        out.push_str(&digits[..1]);
        if digits.len() > 1 {
            out.push('.');
            out.push_str(&digits[1..]);
        }
        push_exp(&mut out, exp);
    }
    out
}

/// Emulate C `%.{p}g` for a finite double.
fn format_g(v: f64, p: usize) -> String {
    let p = p.max(1);
    let sci = format!("{:.*e}", p - 1, v);
    let (neg, digits, x) = split_sci(&sci);
    if x < -4 || x >= p as i32 {
        let d = digits.trim_end_matches('0');
        let d = if d.is_empty() { "0" } else { d };
        let mut out = String::new();
        out.push_str(neg);
        out.push_str(&d[..1]);
        if d.len() > 1 {
            out.push('.');
            out.push_str(&d[1..]);
        }
        push_exp(&mut out, x);
        out
    } else {
        let decimals = (p as i32 - 1 - x).max(0) as usize;
        let mut f = format!("{:.*}", decimals, v);
        if f.contains('.') {
            let keep = f.trim_end_matches('0').trim_end_matches('.').len();
            f.truncate(keep);
        }
        f
    }
}

/// Format a float8 per module rules under `extra_float_digits`.
/// Example: `format_float8(1e15, 1)` → `"1e+15"`.
// CONTRACT
pub fn format_float8(v: f64, extra_float_digits: i32) -> Result<String> {
    if v.is_nan() {
        return Ok("NaN".to_string());
    }
    if v.is_infinite() {
        return Ok(if v < 0.0 { "-Infinity" } else { "Infinity" }.to_string());
    }
    if extra_float_digits > 0 {
        Ok(layout_shortest(&format!("{:e}", v), 15))
    } else {
        Ok(format_g(v, (15 + extra_float_digits).max(1) as usize))
    }
}

/// Format a float4 per module rules under `extra_float_digits`.
// CONTRACT
pub fn format_float4(v: f32, extra_float_digits: i32) -> Result<String> {
    if v.is_nan() {
        return Ok("NaN".to_string());
    }
    if v.is_infinite() {
        return Ok(if v < 0.0 { "-Infinity" } else { "Infinity" }.to_string());
    }
    if extra_float_digits > 0 {
        Ok(layout_shortest(&format!("{:e}", v), 6))
    } else {
        Ok(format_g(v as f64, (6 + extra_float_digits).max(1) as usize))
    }
}

fn is_f4(ty: TypeId) -> Result<bool> {
    match ty.oid {
        oids::FLOAT4 => Ok(true),
        oids::FLOAT8 => Ok(false),
        o => Err(PgError::internal(format!("float I/O called for non-float type oid {}", o))),
    }
}

/// `float4in` / `float8in` by `ty.oid`.
pub fn input(ctx: &IoCtx, ty: TypeId, s: &str) -> Result<Datum> {
    let _ = ctx;
    if is_f4(ty)? {
        Ok(Datum::Float4(parse_float4(s)?))
    } else {
        Ok(Datum::Float8(parse_float8(s)?))
    }
}

/// `float4out` / `float8out` using `ctx.config.extra_float_digits`.
pub fn output(ctx: &IoCtx, ty: TypeId, d: &Datum) -> Result<String> {
    let n = ctx.config.extra_float_digits;
    match (is_f4(ty)?, d) {
        (true, Datum::Float4(v)) => format_float4(*v, n),
        (false, Datum::Float8(v)) => format_float8(*v, n),
        _ => Err(PgError::internal("float output: datum does not match float type")),
    }
}

/// `float4recv` / `float8recv`.
pub fn recv(ctx: &IoCtx, ty: TypeId, buf: &[u8]) -> Result<Datum> {
    let _ = ctx;
    let f4 = is_f4(ty)?;
    let need = if f4 { 4 } else { 8 };
    if buf.len() < need {
        return Err(PgError::new(SqlState::PROTOCOL_VIOLATION, "insufficient data left in message"));
    }
    if buf.len() > need {
        return Err(PgError::new(
            SqlState::INVALID_BINARY_REPRESENTATION,
            "incorrect binary data format",
        ));
    }
    if f4 {
        let mut a = [0u8; 4];
        a.copy_from_slice(buf);
        Ok(Datum::Float4(f32::from_bits(u32::from_be_bytes(a))))
    } else {
        let mut a = [0u8; 8];
        a.copy_from_slice(buf);
        Ok(Datum::Float8(f64::from_bits(u64::from_be_bytes(a))))
    }
}

/// `float4send` / `float8send`.
pub fn send(ctx: &IoCtx, ty: TypeId, d: &Datum) -> Result<Vec<u8>> {
    let _ = ctx;
    match (is_f4(ty)?, d) {
        (true, Datum::Float4(v)) => Ok(v.to_bits().to_be_bytes().to_vec()),
        (false, Datum::Float8(v)) => Ok(v.to_bits().to_be_bytes().to_vec()),
        _ => Err(PgError::internal("float send: datum does not match float type")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::context::IoConfig;
    use crate::registry::builtin_env;

    #[test]
    fn fmt8() {
        assert_eq!(format_float8(1e15, 1).unwrap(), "1e+15");
        assert_eq!(format_float8(1e14, 1).unwrap(), "100000000000000");
        assert_eq!(format_float8(123456789012345678.0, 1).unwrap(), "1.2345678901234568e+17");
        assert_eq!(format_float8(0.0001, 1).unwrap(), "0.0001");
        assert_eq!(format_float8(1e-5, 1).unwrap(), "1e-05");
        assert_eq!(format_float8(9.99e14, 1).unwrap(), "999000000000000");
        assert_eq!(format_float8(-1.5e-7, 1).unwrap(), "-1.5e-07");
        assert_eq!(format_float8(9007199254740993.0, 1).unwrap(), "9.007199254740992e+15");
        assert_eq!(format_float8(0.1f32 as f64, 1).unwrap(), "0.10000000149011612");
        assert_eq!(format_float8(-0.0, 1).unwrap(), "-0");
        assert_eq!(format_float8(f64::NAN, 1).unwrap(), "NaN");
        assert_eq!(format_float8(f64::NEG_INFINITY, 1).unwrap(), "-Infinity");
        assert_eq!(format_float8(1.5, 1).unwrap(), "1.5");
        assert_eq!(format_float8(0.1f32 as f64, 0).unwrap(), "0.100000001490116");
    }

    #[test]
    fn fmt4() {
        assert_eq!(format_float4(1e6, 1).unwrap(), "1e+06");
        assert_eq!(format_float4(123456.0, 1).unwrap(), "123456");
        assert_eq!(format_float4(1.5e-5, 1).unwrap(), "1.5e-05");
        assert_eq!(format_float4(1e15, 1).unwrap(), "1e+15");
        assert_eq!(format_float4(0.1, 1).unwrap(), "0.1");
        assert_eq!(format_float4(0.1, 0).unwrap(), "0.1");
    }

    #[test]
    fn fmt_g() {
        assert_eq!(format_float8(100000.0, 0).unwrap(), "100000");
        assert_eq!(format_float8(1e15, 0).unwrap(), "1e+15");
        assert_eq!(format_float8(1.0 / 3.0, -12).unwrap(), "0.333");
    }

    #[test]
    fn parse() {
        assert_eq!(parse_float8(" -Infinity ").unwrap(), f64::NEG_INFINITY);
        assert!(parse_float8("NaN").unwrap().is_nan());
        assert_eq!(parse_float8("1e-320").unwrap(), 1e-320);
        assert_eq!(parse_float4("1e-40").unwrap(), 1e-40f32);
        assert_eq!(parse_float8(".5").unwrap(), 0.5);
        let e = parse_float8("1e400").unwrap_err();
        assert_eq!(e.sqlstate.code(), "22003");
        assert_eq!(e.message, "\"1e400\" is out of range for type double precision");
        let e = parse_float8("1e-400").unwrap_err();
        assert_eq!(e.sqlstate.code(), "22003");
        let e = parse_float4("1e100").unwrap_err();
        assert_eq!(e.message, "\"1e100\" is out of range for type real");
        for bad in ["abc", "", "0x1p3", "1_0", "1e", "."] {
            let e = parse_float8(bad).unwrap_err();
            assert_eq!(e.sqlstate.code(), "22P02");
            assert_eq!(e.message, format!("invalid input syntax for type double precision: \"{}\"", bad));
        }
    }

    #[test]
    fn io_roundtrip() {
        let cfg = IoConfig::default();
        let ctx = IoCtx { env: builtin_env(), config: &cfg, now: None };
        let d = input(&ctx, TypeId::FLOAT8, "1.25").unwrap();
        assert_eq!(output(&ctx, TypeId::FLOAT8, &d).unwrap(), "1.25");
        let b = send(&ctx, TypeId::FLOAT8, &d).unwrap();
        assert_eq!(b, 1.25f64.to_be_bytes().to_vec());
        assert!(matches!(recv(&ctx, TypeId::FLOAT8, &b).unwrap(), Datum::Float8(v) if v == 1.25));
        assert!(recv(&ctx, TypeId::FLOAT4, &b).is_err());
        let d4 = input(&ctx, TypeId::FLOAT4, "0.1").unwrap();
        assert_eq!(output(&ctx, TypeId::FLOAT4, &d4).unwrap(), "0.1");
    }
}
