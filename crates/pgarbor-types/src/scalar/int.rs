//! `int2` / `int4` / `int8` text and binary I/O (TYPE-01, TYPE-15). Lease TYP-04.
//!
//! PG18 reference: `src/backend/utils/adt/numutils.c` (`pg_strtoint16/32/64_safe`), `int.c`, `int8.c`.
//!
//! Text input rules (PG16+ `pg_strtointNN_safe`):
//! - Leading and trailing whitespace (space, \t, \n, \r, \v, \f) allowed; optional `+`/`-` sign.
//! - Decimal digits, OR prefixed non-decimal literals `0x`/`0X` (hex), `0o`/`0O` (octal),
//!   `0b`/`0B` (binary) — PG16+ accepts these in input: `'0x1A'::int4` = 26.
//! - Underscores between digits are allowed (`'1_000'::int4` = 1000). In decimal literals an
//!   underscore may not be first and must be followed by a digit. In prefixed literals the
//!   source only requires the underscore to be followed by a digit of that base (so `'0x_1'`
//!   is accepted as 1, exactly as PG's `pg_strtointNN_safe` does).
//! - Empty / only whitespace / junk → 22P02 `invalid input syntax for type integer: "<input>"`
//!   (type names in messages: int2 → `smallint`, int4 → `integer`, int8 → `bigint`).
//! - Out of range → 22003 `value "<input>" is out of range for type integer` (resp. smallint/bigint).
//!   Example (oracle): `'99999999999'::int4` → `value "99999999999" is out of range for type integer`.
//! - The `<input>` in messages is the ORIGINAL string (untrimmed).
//!
//! Text output: plain decimal (`-123`). Binary: big-endian two's complement of width 2/4/8;
//! recv requires exactly that many bytes.
use crate::context::IoCtx;
use crate::datum::{type_mismatch, Datum, TypeId};
use crate::io::{invalid_binary, invalid_input_syntax};
use crate::oids::{INT2, INT4, INT8};
use pgarbor_common::{PgError, Result, SqlState};

/// The three integer widths handled by this module.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum IntKind {
    I2,
    I4,
    I8,
}

impl IntKind {
    /// Width selected by the type OID (int2 / int4 / int8).
    fn of(ty: TypeId) -> Result<IntKind> {
        match ty.oid {
            INT2 => Ok(IntKind::I2),
            INT4 => Ok(IntKind::I4),
            INT8 => Ok(IntKind::I8),
            other => Err(PgError::internal(format!("integer I/O called for non-integer type oid {other}"))),
        }
    }

    /// Value bits, including the sign bit.
    fn bits(self) -> u32 {
        match self {
            IntKind::I2 => 16,
            IntKind::I4 => 32,
            IntKind::I8 => 64,
        }
    }

    /// Binary width in bytes.
    fn width(self) -> usize {
        match self {
            IntKind::I2 => 2,
            IntKind::I4 => 4,
            IntKind::I8 => 8,
        }
    }

    /// SQL display name used in error messages.
    fn sql_name(self) -> &'static str {
        match self {
            IntKind::I2 => "smallint",
            IntKind::I4 => "integer",
            IntKind::I8 => "bigint",
        }
    }

    /// Internal datum-type name used in mismatch diagnostics.
    fn datum_name(self) -> &'static str {
        match self {
            IntKind::I2 => "Int2",
            IntKind::I4 => "Int4",
            IntKind::I8 => "Int8",
        }
    }
}

/// C-locale `isspace`: space, \t, \n, \v, \f, \r.
fn is_c_space(c: u8) -> bool {
    matches!(c, b' ' | b'\t' | b'\n' | 0x0b | 0x0c | b'\r')
}

/// Value of `c` as a digit of `base` (2, 8, 10 or 16), or None.
fn digit_value(c: u8, base: u64) -> Option<u64> {
    let v = match c {
        b'0'..=b'9' => u64::from(c - b'0'),
        b'a'..=b'f' => u64::from(c - b'a') + 10,
        b'A'..=b'F' => u64::from(c - b'A') + 10,
        _ => return None,
    };
    if v < base {
        Some(v)
    } else {
        None
    }
}

/// Port of PG's `pg_strtoint{16,32,64}_safe` slow path, generic over the width.
///
/// The magnitude is accumulated unsigned and checked against `limit / base` before each
/// multiplication, so out-of-range is reported eagerly, in the same order PG reports it.
fn scan_integer(s: &str, kind: IntKind) -> Result<i64> {
    // A C string ends at the first NUL byte.
    let full = s.as_bytes();
    let bytes = match full.iter().position(|&c| c == 0) {
        Some(p) => &full[..p],
        None => full,
    };
    let end = bytes.len();
    let at = |i: usize| -> u8 { bytes.get(i).copied().unwrap_or(0) };
    let out_of_range = || {
        PgError::new(
            SqlState::NUMERIC_VALUE_OUT_OF_RANGE,
            format!("value \"{s}\" is out of range for type {}", kind.sql_name()),
        )
    };
    let invalid = || invalid_input_syntax(kind.sql_name(), s);

    let mut i = 0usize;
    while i < end && is_c_space(bytes[i]) {
        i += 1;
    }
    let mut neg = false;
    if at(i) == b'-' {
        neg = true;
        i += 1;
    } else if at(i) == b'+' {
        i += 1;
    }

    // Digit base and first digit position.
    let (base, first): (u64, usize) = if at(i) == b'0' {
        match at(i + 1) {
            b'x' | b'X' => (16, i + 2),
            b'o' | b'O' => (8, i + 2),
            b'b' | b'B' => (2, i + 2),
            _ => (10, i),
        }
    } else {
        (10, i)
    };

    let limit: u64 = 1u64 << (kind.bits() - 1);
    let threshold = limit / base;
    let mut mag: u64 = 0;
    let mut p = first;
    loop {
        let c = at(p);
        if let Some(d) = digit_value(c, base) {
            if mag > threshold {
                return Err(out_of_range());
            }
            mag = mag * base + d;
            p += 1;
        } else if c == b'_' {
            // Decimal: an underscore may not come first. All bases: it must be followed by a digit.
            if base == 10 && p == first {
                return Err(invalid());
            }
            p += 1;
            if digit_value(at(p), base).is_none() {
                return Err(invalid());
            }
        } else {
            break;
        }
    }

    // At least one digit is required.
    if p == first {
        return Err(invalid());
    }
    // Trailing whitespace is allowed, nothing else.
    while p < end && is_c_space(bytes[p]) {
        p += 1;
    }
    if p != end {
        return Err(invalid());
    }

    if neg {
        if mag > limit {
            return Err(out_of_range());
        }
        Ok((-(mag as i128)) as i64)
    } else {
        if mag > limit - 1 {
            return Err(out_of_range());
        }
        Ok(mag as i64)
    }
}

/// Parse an int2 per the module rules (errors carry PG messages).
// CONTRACT
#[allow(unused_variables)]
pub fn parse_int2(s: &str) -> Result<i16> {
    Ok(scan_integer(s, IntKind::I2)? as i16)
}

/// Parse an int4 per the module rules.
// CONTRACT
#[allow(unused_variables)]
pub fn parse_int4(s: &str) -> Result<i32> {
    Ok(scan_integer(s, IntKind::I4)? as i32)
}

/// Parse an int8 per the module rules.
// CONTRACT
#[allow(unused_variables)]
pub fn parse_int8(s: &str) -> Result<i64> {
    scan_integer(s, IntKind::I8)
}

/// Integer value of a datum, checking that its variant matches the width of `kind`.
fn datum_value(kind: IntKind, d: &Datum) -> Result<i64> {
    match (kind, d) {
        (IntKind::I2, Datum::Int2(v)) => Ok(i64::from(*v)),
        (IntKind::I4, Datum::Int4(v)) => Ok(i64::from(*v)),
        (IntKind::I8, Datum::Int8(v)) => Ok(*v),
        _ => Err(type_mismatch(kind.datum_name(), d)),
    }
}

/// `int2in` / `int4in` / `int8in` by `ty.oid` → `Datum::Int2/Int4/Int8`.
#[allow(unused_variables)]
pub fn input(ctx: &IoCtx, ty: TypeId, s: &str) -> Result<Datum> {
    let kind = IntKind::of(ty)?;
    let v = scan_integer(s, kind)?;
    Ok(match kind {
        IntKind::I2 => Datum::Int2(v as i16),
        IntKind::I4 => Datum::Int4(v as i32),
        IntKind::I8 => Datum::Int8(v),
    })
}

/// `int2out` / `int4out` / `int8out`.
#[allow(unused_variables)]
pub fn output(ctx: &IoCtx, ty: TypeId, d: &Datum) -> Result<String> {
    let kind = IntKind::of(ty)?;
    Ok(datum_value(kind, d)?.to_string())
}

/// `int2recv` / `int4recv` / `int8recv` (2/4/8 bytes big-endian).
#[allow(unused_variables)]
pub fn recv(ctx: &IoCtx, ty: TypeId, buf: &[u8]) -> Result<Datum> {
    let kind = IntKind::of(ty)?;
    if buf.len() != kind.width() {
        return Err(invalid_binary(""));
    }
    Ok(match kind {
        IntKind::I2 => {
            let a: [u8; 2] = buf.try_into().map_err(|_| invalid_binary(""))?;
            Datum::Int2(i16::from_be_bytes(a))
        }
        IntKind::I4 => {
            let a: [u8; 4] = buf.try_into().map_err(|_| invalid_binary(""))?;
            Datum::Int4(i32::from_be_bytes(a))
        }
        IntKind::I8 => {
            let a: [u8; 8] = buf.try_into().map_err(|_| invalid_binary(""))?;
            Datum::Int8(i64::from_be_bytes(a))
        }
    })
}

/// `int2send` / `int4send` / `int8send`.
#[allow(unused_variables)]
pub fn send(ctx: &IoCtx, ty: TypeId, d: &Datum) -> Result<Vec<u8>> {
    let kind = IntKind::of(ty)?;
    let v = datum_value(kind, d)?;
    Ok(match kind {
        IntKind::I2 => (v as i16).to_be_bytes().to_vec(),
        IntKind::I4 => (v as i32).to_be_bytes().to_vec(),
        IntKind::I8 => v.to_be_bytes().to_vec(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::context::{IoConfig, IoCtx};
    use crate::registry;

    fn text_in(ty: TypeId, s: &str) -> Result<Datum> {
        let cfg = IoConfig::default();
        let ctx = IoCtx::new(registry::builtin_env(), &cfg);
        input(&ctx, ty, s)
    }

    fn text_out(ty: TypeId, d: &Datum) -> Result<String> {
        let cfg = IoConfig::default();
        let ctx = IoCtx::new(registry::builtin_env(), &cfg);
        output(&ctx, ty, d)
    }

    fn err_of<T: std::fmt::Debug>(r: Result<T>) -> PgError {
        match r {
            Ok(v) => panic!("expected an error, got {v:?}"),
            Err(e) => e,
        }
    }

    #[test]
    fn prefixed_and_decimal_literals() {
        assert_eq!(parse_int4("0x1A").unwrap(), 26);
        assert_eq!(parse_int4("0X1a").unwrap(), 26);
        assert_eq!(parse_int4("0o17").unwrap(), 15);
        assert_eq!(parse_int2("-0b101").unwrap(), -5);
        assert_eq!(parse_int8("0x7FFFFFFFFFFFFFFF").unwrap(), i64::MAX);
        assert_eq!(parse_int8("-9223372036854775808").unwrap(), i64::MIN);
        assert_eq!(parse_int4("+42").unwrap(), 42);
        assert_eq!(parse_int4("-0x10").unwrap(), -16);
        assert_eq!(parse_int4("0").unwrap(), 0);
        assert_eq!(parse_int4("007").unwrap(), 7);
    }

    #[test]
    fn underscores_and_whitespace() {
        assert_eq!(parse_int4("1_000").unwrap(), 1000);
        assert_eq!(parse_int4("0x_1").unwrap(), 1);
        assert_eq!(parse_int4("0b1_0_1").unwrap(), 5);
        assert_eq!(parse_int4(" 12 ").unwrap(), 12);
        assert_eq!(parse_int4("\t\n12\r\x0b\x0c").unwrap(), 12);
        assert_eq!(parse_int4("1__0").unwrap_err().sqlstate, SqlState::INVALID_TEXT_REPRESENTATION);
        assert_eq!(parse_int4("_1").unwrap_err().sqlstate, SqlState::INVALID_TEXT_REPRESENTATION);
        assert_eq!(parse_int4("1_").unwrap_err().sqlstate, SqlState::INVALID_TEXT_REPRESENTATION);
        assert_eq!(parse_int4("0x").unwrap_err().sqlstate, SqlState::INVALID_TEXT_REPRESENTATION);
        assert_eq!(parse_int4("0x_").unwrap_err().sqlstate, SqlState::INVALID_TEXT_REPRESENTATION);
    }

    #[test]
    fn invalid_syntax_messages_use_original_input() {
        for s in ["", "   ", "+", "-", "abc", "12a", "0b12", "0o9", "1 2", "0a"] {
            let e = err_of(text_in(TypeId::INT4, s));
            assert_eq!(e.sqlstate, SqlState::INVALID_TEXT_REPRESENTATION, "input {s:?}");
        }
        let e = err_of(text_in(TypeId::INT4, "  1x  "));
        assert_eq!(e.message, "invalid input syntax for type integer: \"  1x  \"");
        let e = err_of(text_in(TypeId::INT2, "zz"));
        assert_eq!(e.message, "invalid input syntax for type smallint: \"zz\"");
        let e = err_of(text_in(TypeId::INT8, "zz"));
        assert_eq!(e.message, "invalid input syntax for type bigint: \"zz\"");
    }

    #[test]
    fn out_of_range_messages() {
        let e = err_of(text_in(TypeId::INT4, "99999999999"));
        assert_eq!(e.sqlstate, SqlState::NUMERIC_VALUE_OUT_OF_RANGE);
        assert_eq!(e.message, "value \"99999999999\" is out of range for type integer");
        assert_eq!(parse_int4("-2147483648").unwrap(), i32::MIN);
        assert_eq!(err_of(parse_int4("2147483648")).sqlstate, SqlState::NUMERIC_VALUE_OUT_OF_RANGE);
        assert_eq!(err_of(parse_int4("-2147483649")).sqlstate, SqlState::NUMERIC_VALUE_OUT_OF_RANGE);
        assert_eq!(parse_int2("-32768").unwrap(), i16::MIN);
        assert_eq!(parse_int2("32767").unwrap(), i16::MAX);
        let e = err_of(text_in(TypeId::INT2, "32768"));
        assert_eq!(e.message, "value \"32768\" is out of range for type smallint");
        let e = err_of(text_in(TypeId::INT8, "9223372036854775808"));
        assert_eq!(e.message, "value \"9223372036854775808\" is out of range for type bigint");
        assert_eq!(err_of(parse_int4("0xFFFFFFFF")).sqlstate, SqlState::NUMERIC_VALUE_OUT_OF_RANGE);
        assert_eq!(err_of(parse_int8("0x8000000000000000")).sqlstate, SqlState::NUMERIC_VALUE_OUT_OF_RANGE);
        assert_eq!(err_of(parse_int8("99999999999999999999999")).sqlstate, SqlState::NUMERIC_VALUE_OUT_OF_RANGE);
    }

    #[test]
    fn input_builds_the_right_variant() {
        assert_eq!(text_in(TypeId::INT2, "-7").unwrap().as_i16(), Some(-7));
        assert_eq!(text_in(TypeId::INT4, "0x1A").unwrap().as_i32(), Some(26));
        assert_eq!(text_in(TypeId::INT8, "1_000").unwrap().as_i64(), Some(1000));
    }

    #[test]
    fn output_is_decimal() {
        assert_eq!(text_out(TypeId::INT4, &Datum::Int4(-123)).unwrap(), "-123");
        assert_eq!(text_out(TypeId::INT2, &Datum::Int2(i16::MIN)).unwrap(), "-32768");
        assert_eq!(text_out(TypeId::INT8, &Datum::Int8(i64::MIN)).unwrap(), "-9223372036854775808");
        assert!(text_out(TypeId::INT4, &Datum::Int8(1)).is_err());
    }

    #[test]
    fn binary_round_trip_and_bytes() {
        let cfg = IoConfig::default();
        let ctx = IoCtx::new(registry::builtin_env(), &cfg);
        assert_eq!(send(&ctx, TypeId::INT4, &Datum::Int4(42)).unwrap(), vec![0, 0, 0, 42]);
        assert_eq!(send(&ctx, TypeId::INT2, &Datum::Int2(-2)).unwrap(), vec![0xff, 0xfe]);
        assert_eq!(
            send(&ctx, TypeId::INT8, &Datum::Int8(1)).unwrap(),
            vec![0, 0, 0, 0, 0, 0, 0, 1]
        );
        assert_eq!(recv(&ctx, TypeId::INT4, &[0, 0, 0, 42]).unwrap().as_i32(), Some(42));
        assert_eq!(recv(&ctx, TypeId::INT2, &[0xff, 0xfe]).unwrap().as_i16(), Some(-2));
        assert_eq!(recv(&ctx, TypeId::INT8, &[0xff; 8]).unwrap().as_i64(), Some(-1));
    }

    #[test]
    fn recv_requires_exact_length() {
        let cfg = IoConfig::default();
        let ctx = IoCtx::new(registry::builtin_env(), &cfg);
        let e = err_of(recv(&ctx, TypeId::INT4, &[0, 0, 42]));
        assert_eq!(e.sqlstate, SqlState::INVALID_BINARY_REPRESENTATION);
        let e = err_of(recv(&ctx, TypeId::INT4, &[0, 0, 0, 42, 0]));
        assert_eq!(e.sqlstate, SqlState::INVALID_BINARY_REPRESENTATION);
        let e = err_of(recv(&ctx, TypeId::INT2, &[0, 0, 0]));
        assert_eq!(e.sqlstate, SqlState::INVALID_BINARY_REPRESENTATION);
        let e = err_of(recv(&ctx, TypeId::INT8, &[0; 4]));
        assert_eq!(e.sqlstate, SqlState::INVALID_BINARY_REPRESENTATION);
    }
}
