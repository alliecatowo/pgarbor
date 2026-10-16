//! Literals: `sqlparser::ast::Value` -> typed constants (SQL-08).
//! PG references: `parse_expr.c` (`make_const`), `parse_node.c` (`make_const`), `scan.l` (literal
//! lexing rules), `gram.y` `AexprConst`, `parse_type.c`/`parse_coerce.c` for typed literals.
//!
//! ## Typing rules (PG `make_const`)
//! * Integer literal `123`: `int4` if it fits in i32, else `int8` if it fits in i64, else `numeric`.
//!   `-` is a separate unary operator (`-5` = `-(5)`, constant folded to `-5` int4; `-2147483648` is
//!   int4 after folding because PG's lexer/grammar `doNegate` folds the sign into the literal: do the
//!   same for a unary minus applied directly to a numeric literal).
//! * Decimal/exponent literal `1.5`, `1e3`, `1.` -> `numeric` with scale from the text (`1.50` has
//!   scale 2; `1e3` numeric 1000, scale 0).
//! * Hex/octal/binary integer literals (`0x1F`, `0o17`, `0b101`, PG 16+) and `1_000` underscores:
//!   int4/int8/numeric by magnitude; sqlparser exposes them as `Value::Number` text or
//!   `HexStringLiteral`; follow `scan.l` (`integer_junk`, `numeric_junk` -> 42601 `trailing junk
//!   after numeric literal at or near "1a"`).
//! * String literal `'abc'` (also `E'..'`, `$$..$$`, `U&'..'`, national `N'..'`): type `unknown`
//!   with value `Datum::Text(decoded)`; represented as `BoundExpr::constant(Datum::Text(s),
//!   TypeId::UNKNOWN)`. Conversion to a concrete type happens in `coerce` (position-aware).
//!   `E'..'` escapes decoded here: `\n \t \r \b \f \\ \' \" \ooo \xhh \uXXXX \UXXXXXXXX`,
//!   invalid -> 22P05? (`invalid Unicode escape`, 22025 `invalid escape string`...). `U&'\0041'`
//!   with optional `UESCAPE`. Standard strings (`standard_conforming_strings=on`, the pinned
//!   setting) treat backslash literally; sqlparser already folds `''`.
//! * `NULL` -> `Const(Datum::Null)` typed `unknown`.
//! * `TRUE`/`FALSE` -> `bool` constants. `X'1F'` hex-string and `B'101'` bit-string literals: bit
//!   types are unsupported -> 0A000 (`bit string literal`).
//! * Typed literal `DATE '2020-01-01'`, `TIMESTAMP '..'`, `INTERVAL '1 day' DAY TO SECOND`,
//!   `type 'str'` via `Expr::TypedString`: resolve the type (`types`), then convert the string with
//!   `pgarbor_types::io::input` NOW (constant) so errors carry the literal's position:
//!   `22007 invalid input syntax for type date: "xx"` at the string token. Special datetime inputs
//!   (`'now'`, `'today'`, ...) use `Binder::io_ctx()` (with the statement timestamp). Result is a
//!   `Const` of the target type (typmod applied by `input`).
//! * Placeholder values (`$1`) go to `params::bind_param`.
//!
//! Positions: errors use the `ValueWithSpan` span start.
use pgarbor_common::{PgError, Result, SqlState};
use pgarbor_types::io as type_io;
use pgarbor_types::numeric::io::parse_numeric;
use pgarbor_types::{Datum, TypeId};
use sqlparser::ast::{DataType, Value, ValueWithSpan};
use sqlparser::tokenizer::Span;

use super::context::Binder;
use super::params::bind_param;
use super::types::resolve_data_type;
use crate::diag;
use crate::ir::BoundExpr;
use crate::source::SourceMap;

/// Bind a literal value (typing rules above). `negate`: the literal is the direct operand of a unary
/// minus and the sign must be folded into the constant.
pub fn bind_value(b: &mut Binder, v: &ValueWithSpan, negate: bool) -> Result<BoundExpr> {
    let pos = b.pos_span(v.span.clone());
    match &v.value {
        Value::Number(text, _) => numeric_literal(b, text.as_str(), negate, pos),
        Value::Boolean(t) => {
            reject_negate(negate, "boolean", pos)?;
            Ok(BoundExpr::constant(Datum::Bool(*t), TypeId::BOOL))
        }
        Value::Null => {
            reject_negate(negate, "unknown", pos)?;
            Ok(BoundExpr::null(TypeId::UNKNOWN))
        }
        Value::SingleQuotedString(_)
        | Value::DollarQuotedString(_)
        | Value::EscapedStringLiteral(_)
        | Value::UnicodeStringLiteral(_)
        | Value::NationalStringLiteral(_) => {
            reject_negate(negate, "unknown", pos)?;
            let s = string_value(b, v)?;
            Ok(unknown_text(s))
        }
        Value::HexStringLiteral(digits) => {
            // sqlparser yields `HexStringLiteral` for both `0x1F` (integer, PG 16+) and `X'1F'`
            // (bit string); the source text tells them apart.
            let is_hex_int = match raw_token(b.src, &v.span) {
                Some(raw) => {
                    let rb = raw.as_bytes();
                    rb.len() >= 2 && rb[0] == b'0' && (rb[1] == b'x' || rb[1] == b'X')
                }
                None => false,
            };
            if is_hex_int {
                let text = format!("0x{digits}");
                numeric_literal(b, &text, negate, pos)
            } else {
                check_bit_digits(digits, false, pos)?;
                Err(diag::unsupported_construct("bit string literal", pos))
            }
        }
        // `B'101'` is a PG bit-string literal (sqlparser: byte string); no bit datum exists yet.
        Value::SingleQuotedByteStringLiteral(digits) => {
            check_bit_digits(digits, true, pos)?;
            Err(diag::unsupported_construct("bit string literal", pos))
        }
        Value::Placeholder(text) => {
            if negate {
                return Err(PgError::internal(
                    "negation flag set on a parameter placeholder (the unary operator must bind it)",
                ));
            }
            bind_param(b, text.as_str(), pos)
        }
        other => Err(diag::syntax_error_near(&other.to_string(), pos)),
    }
}

/// `type 'string'` / `DATE '..'`: resolve `data_type`, convert `value` (must be a string-like
/// literal: `SingleQuotedString`, `EscapedStringLiteral`, `DollarQuotedString`, ...) with
/// `io::input` and return the typed constant.
pub fn bind_typed_string(b: &mut Binder, data_type: &DataType, value: &ValueWithSpan) -> Result<BoundExpr> {
    let pos = b.pos_span(value.span.clone());
    let resolved = resolve_data_type(b, data_type, pos)?;
    let text = string_value(b, value)?;
    let ctx = b.io_ctx();
    let datum = type_io::input(&ctx, resolved.id, &text).map_err(|e| diag::with_position(e, pos))?;
    Ok(BoundExpr::constant(datum, resolved.id))
}

/// Classify numeric literal text into the narrowest of int4 / int8 / numeric and produce the
/// constant. `text` is the literal text WITHOUT sign (`"2147483648"` -> int8). Unit-testable helper.
pub fn numeric_literal(b: &mut Binder, text: &str, negate: bool, pos: Option<u32>) -> Result<BoundExpr> {
    // The binder is not needed for literal typing; keep the parameter for the frozen signature.
    let _ = b;
    let (datum, ty) = number_to_const(text, negate).map_err(|e| diag::with_position(e, pos))?;
    Ok(BoundExpr::constant(datum, ty))
}

/// Decode an `E'...'` escape-string body (the text between the quotes, still containing
/// backslash escapes) into the string value. Errors: 22025 `invalid escape string`? Use PG's
/// messages from `scan.l`: `invalid Unicode escape`, `invalid Unicode surrogate pair`,
/// `invalid hexadecimal digit`? (see `addunicode`, `check_escape_warning`), all 22P05 /
/// 22025 / 42601 as in the grammar; position = start of the string token.
pub fn decode_escape_string(body: &str, pos: Option<u32>) -> Result<String> {
    decode_e_body(body).map_err(|e| diag::with_position(e, pos))
}

/// The `unknown`-typed constant for a string value (shared constructor, fully implemented).
pub fn unknown_text(s: String) -> BoundExpr {
    BoundExpr::constant(pgarbor_types::Datum::Text(s), TypeId::UNKNOWN)
}

/// Is this expression an `unknown`-typed string/NULL literal constant (awaiting coercion)?
pub fn is_unknown_literal(e: &BoundExpr) -> bool {
    e.ty.oid == pgarbor_types::oids::UNKNOWN && matches!(e.kind, crate::ir::ExprKind::Const(_))
}

#[allow(dead_code)]
fn _value_marker(_: &Value) {}

// ---------------------------------------------------------------------------------------------
// Helpers: source access and string values
// ---------------------------------------------------------------------------------------------

/// `bit_in` digit validation: every character of a `B'..'` (binary) or `X'..'` (hex) literal must be
/// a digit of that base, else 22P02 `"c" is not a valid binary digit` / `... hexadecimal digit`.
fn check_bit_digits(digits: &str, binary: bool, pos: Option<u32>) -> Result<()> {
    for c in digits.chars() {
        let ok = if binary { c == '0' || c == '1' } else { c.is_ascii_hexdigit() };
        if !ok {
            let kind = if binary { "binary" } else { "hexadecimal" };
            let mut e = PgError::new(
                SqlState::INVALID_TEXT_REPRESENTATION,
                format!("\"{c}\" is not a valid {kind} digit"),
            );
            if let Some(p) = pos {
                e = e.with_position(p);
            }
            return Err(e);
        }
    }
    Ok(())
}

/// Source text covered by a token span (sqlparser spans are inclusive of the last character).
fn raw_token<'s>(src: &'s SourceMap, span: &Span) -> Option<&'s str> {
    let start = src.byte_offset(span.start.clone())?;
    let end = src.byte_offset(span.end.clone())?;
    src.text().get(start..end.checked_add(1)?)
}

/// The body of a quoted literal such as `'...'` preceded by `prefix_len` bytes of prefix
/// (`E` -> 1, `U&` -> 2). `None` if the raw token does not have that shape.
fn quoted_body(raw: &str, prefix_len: usize) -> Option<&str> {
    let inner = raw.get(prefix_len..)?;
    inner.strip_prefix('\'')?.strip_suffix('\'')
}

/// Decoded text of any string-like literal. `E'..'` and `U&'..'` are decoded from the raw source
/// text so that backslashes are processed exactly once; sqlparser's already-decoded value is only
/// used when the raw token is not available.
fn string_value(b: &Binder, v: &ValueWithSpan) -> Result<String> {
    let pos = b.pos_span(v.span.clone());
    match &v.value {
        Value::SingleQuotedString(s) | Value::NationalStringLiteral(s) => Ok(s.clone()),
        Value::DollarQuotedString(d) => Ok(d.value.clone()),
        Value::EscapedStringLiteral(decoded) => match raw_token(b.src, &v.span).and_then(|r| quoted_body(r, 1)) {
            Some(body) => decode_escape_string(body, pos),
            None => Ok(decoded.clone()),
        },
        Value::UnicodeStringLiteral(decoded) => match raw_token(b.src, &v.span).and_then(|r| quoted_body(r, 2)) {
            Some(body) => decode_uescape_string(body, pos),
            None => Ok(decoded.clone()),
        },
        other => Err(diag::syntax_error_near(&other.to_string(), pos)),
    }
}

/// `negate` on a non-numeric literal cannot be folded: PG reports the missing unary operator.
fn reject_negate(negate: bool, operand_type: &str, pos: Option<u32>) -> Result<()> {
    if !negate {
        return Ok(());
    }
    let err = if operand_type == "boolean" {
        PgError::new(SqlState::UNDEFINED_FUNCTION, "operator does not exist: - boolean")
    } else {
        PgError::new(SqlState::AMBIGUOUS_FUNCTION, "operator is not unique: - unknown")
    };
    Err(diag::with_position(err, pos))
}

// ---------------------------------------------------------------------------------------------
// Helpers: numeric literals (scan.l integer/decimal rules, make_const)
// ---------------------------------------------------------------------------------------------

fn junk_error(text: &str) -> PgError {
    PgError::syntax(format!("trailing junk after numeric literal at or near \"{text}\""), None)
}

/// Remove `_` digit separators (PG 16+). An underscore must sit between two alphanumeric
/// characters (digits, or hex letters after `0x`); anything else is trailing junk.
fn strip_underscores(text: &str) -> Result<String> {
    if !text.contains('_') {
        return Ok(text.to_string());
    }
    let bytes = text.as_bytes();
    let mut out = String::with_capacity(text.len());
    for (i, &c) in bytes.iter().enumerate() {
        if c == b'_' {
            let before_ok = i > 0 && bytes[i - 1].is_ascii_alphanumeric();
            let after_ok = bytes.get(i + 1).map_or(false, |n| n.is_ascii_alphanumeric());
            if !(before_ok && after_ok) {
                return Err(junk_error(text));
            }
        } else {
            out.push(c as char);
        }
    }
    Ok(out)
}

/// Prefix the sign when the literal was the operand of a unary minus.
fn signed(negate: bool, digits: &str) -> String {
    if negate {
        format!("-{digits}")
    } else {
        digits.to_string()
    }
}

/// Narrowest integer type for a signed decimal integer string (`make_const`).
fn integer_const(s: &str) -> Result<(Datum, TypeId)> {
    if let Ok(v) = s.parse::<i32>() {
        return Ok((Datum::Int4(v), TypeId::INT4));
    }
    if let Ok(v) = s.parse::<i64>() {
        return Ok((Datum::Int8(v), TypeId::INT8));
    }
    let n = parse_numeric(s)?;
    Ok((Datum::Numeric(n), TypeId::NUMERIC))
}

/// Convert digits in `radix` (2, 8 or 16) to a decimal digit string of arbitrary length.
fn radix_to_decimal(digits: &str, radix: u32, text: &str) -> Result<String> {
    if digits.is_empty() {
        return Err(junk_error(text));
    }
    // Little-endian limbs in base 10^9.
    let mut limbs: Vec<u64> = vec![0];
    for c in digits.chars() {
        let d = match c.to_digit(radix) {
            Some(d) => u64::from(d),
            None => return Err(junk_error(text)),
        };
        let mut carry = d;
        for limb in limbs.iter_mut() {
            let v = *limb * u64::from(radix) + carry;
            *limb = v % 1_000_000_000;
            carry = v / 1_000_000_000;
        }
        while carry > 0 {
            limbs.push(carry % 1_000_000_000);
            carry /= 1_000_000_000;
        }
    }
    let mut out = String::with_capacity(limbs.len() * 9);
    for (idx, limb) in limbs.iter().rev().enumerate() {
        if idx == 0 {
            out.push_str(&limb.to_string());
        } else {
            out.push_str(&format!("{limb:09}"));
        }
    }
    Ok(out)
}

/// Classify and convert the literal text (without sign) into a typed constant.
fn number_to_const(text: &str, negate: bool) -> Result<(Datum, TypeId)> {
    let clean = strip_underscores(text)?;
    let bytes = clean.as_bytes();
    let radix: u32 = if bytes.len() >= 2 && bytes[0] == b'0' {
        match bytes[1] {
            b'x' | b'X' => 16,
            b'o' | b'O' => 8,
            b'b' | b'B' => 2,
            _ => 10,
        }
    } else {
        10
    };
    if radix != 10 {
        // The prefix is two ASCII bytes, so slicing at 2 is on a char boundary.
        let dec = radix_to_decimal(&clean[2..], radix, text)?;
        return integer_const(&signed(negate, &dec));
    }
    if clean.is_empty() {
        return Err(junk_error(text));
    }
    if clean.bytes().all(|c| c.is_ascii_digit()) {
        return integer_const(&signed(negate, &clean));
    }
    // Decimal point or exponent: numeric, scale taken from the text.
    let n = parse_numeric(&signed(negate, &clean))?;
    Ok((Datum::Numeric(n), TypeId::NUMERIC))
}

// ---------------------------------------------------------------------------------------------
// Helpers: escape strings (E'..', scan.l xe* rules and addunicode)
// ---------------------------------------------------------------------------------------------

fn hex_val(c: u8) -> Option<u32> {
    match c {
        b'0'..=b'9' => Some(u32::from(c - b'0')),
        b'a'..=b'f' => Some(u32::from(c - b'a') + 10),
        b'A'..=b'F' => Some(u32::from(c - b'A') + 10),
        _ => None,
    }
}

/// Exactly `n` hex digits starting at `start`.
fn read_hex(bytes: &[u8], start: usize, n: usize) -> Option<u32> {
    let mut val: u32 = 0;
    for k in 0..n {
        let d = hex_val(*bytes.get(start + k)?)?;
        val = val * 16 + d;
    }
    Some(val)
}

fn is_high_surrogate(c: u32) -> bool {
    (0xD800..=0xDBFF).contains(&c)
}

fn is_low_surrogate(c: u32) -> bool {
    (0xDC00..=0xDFFF).contains(&c)
}

fn combine_surrogates(high: u32, low: u32) -> u32 {
    0x10000 + ((high - 0xD800) << 10) + (low - 0xDC00)
}

fn char_of(c: u32) -> Result<char> {
    char::from_u32(c).ok_or_else(|| PgError::syntax("invalid Unicode escape value", None))
}

fn invalid_unicode_escape() -> PgError {
    PgError::new(SqlState::INVALID_ESCAPE_SEQUENCE, "invalid Unicode escape")
        .with_hint("Unicode escapes must be \\uXXXX or \\UXXXXXXXX.")
}

fn surrogate_error_near(near: &str) -> PgError {
    PgError::syntax(format!("invalid Unicode surrogate pair at or near \"{near}\""), None)
}

/// Append one code point of an `E'..'` string (`addunicode`): validity first, then the UTF-16
/// surrogate pairing rules.
fn push_e_codepoint(c: u32, out: &mut Vec<u8>, pending: &mut Option<u32>, near: &str) -> Result<()> {
    if c == 0 || c > 0x10FFFF {
        return Err(PgError::syntax(format!("invalid Unicode escape value at or near \"{near}\""), None));
    }
    if let Some(high) = pending.take() {
        if is_low_surrogate(c) {
            let ch = char_of(combine_surrogates(high, c))?;
            let mut buf = [0u8; 4];
            out.extend_from_slice(ch.encode_utf8(&mut buf).as_bytes());
            return Ok(());
        }
        return Err(surrogate_error_near(near));
    }
    if is_high_surrogate(c) {
        *pending = Some(c);
        return Ok(());
    }
    if is_low_surrogate(c) {
        return Err(surrogate_error_near(near));
    }
    let ch = char_of(c)?;
    let mut buf = [0u8; 4];
    out.extend_from_slice(ch.encode_utf8(&mut buf).as_bytes());
    Ok(())
}

fn decode_e_body(body: &str) -> Result<String> {
    let bytes = body.as_bytes();
    // Raw bytes: `\x` and octal escapes may produce bytes that only form valid UTF-8 together.
    let mut out: Vec<u8> = Vec::with_capacity(bytes.len());
    let mut pending: Option<u32> = None;
    let mut i = 0usize;
    while i < bytes.len() {
        let c = bytes[i];
        if c == b'\\' {
            let near = &body[i..];
            let next = match bytes.get(i + 1) {
                Some(n) => *n,
                None => return Err(PgError::syntax("unterminated quoted string", None)),
            };
            if next != b'u' && next != b'U' && pending.is_some() {
                return Err(surrogate_error_near(near));
            }
            match next {
                b'b' => {
                    out.push(0x08);
                    i += 2;
                }
                b'f' => {
                    out.push(0x0C);
                    i += 2;
                }
                b'n' => {
                    out.push(b'\n');
                    i += 2;
                }
                b'r' => {
                    out.push(b'\r');
                    i += 2;
                }
                b't' => {
                    out.push(b'\t');
                    i += 2;
                }
                b'0'..=b'7' => {
                    // Up to three octal digits; the value is truncated to one byte like PG's
                    // `unescape_single_char`.
                    let mut val: u32 = 0;
                    let mut j = i + 1;
                    let mut n = 0;
                    while n < 3 && j < bytes.len() && (b'0'..=b'7').contains(&bytes[j]) {
                        val = val * 8 + u32::from(bytes[j] - b'0');
                        j += 1;
                        n += 1;
                    }
                    out.push((val & 0xFF) as u8);
                    i = j;
                }
                b'x' => {
                    // `\x` followed by one or two hex digits; without digits it stands for `x`.
                    let mut val: u32 = 0;
                    let mut j = i + 2;
                    let mut n = 0;
                    while n < 2 && j < bytes.len() {
                        match hex_val(bytes[j]) {
                            Some(d) => {
                                val = val * 16 + d;
                                j += 1;
                                n += 1;
                            }
                            None => break,
                        }
                    }
                    if n == 0 {
                        out.push(b'x');
                        i += 2;
                    } else {
                        out.push((val & 0xFF) as u8);
                        i = j;
                    }
                }
                b'u' | b'U' => {
                    let ndig = if next == b'u' { 4 } else { 8 };
                    let val = match read_hex(bytes, i + 2, ndig) {
                        Some(v) => v,
                        None => return Err(invalid_unicode_escape()),
                    };
                    i += 2 + ndig;
                    push_e_codepoint(val, &mut out, &mut pending, near)?;
                }
                _ => {
                    // Any other character stands for itself (`\\`, `\'`, `\"`, unknown escapes).
                    let ch = match body.get(i + 1..).and_then(|s| s.chars().next()) {
                        Some(ch) => ch,
                        None => return Err(PgError::syntax("unterminated quoted string", None)),
                    };
                    let mut buf = [0u8; 4];
                    out.extend_from_slice(ch.encode_utf8(&mut buf).as_bytes());
                    i += 1 + ch.len_utf8();
                }
            }
        } else if c == b'\'' && bytes.get(i + 1) == Some(&b'\'') {
            if pending.is_some() {
                return Err(surrogate_error_near(&body[i..]));
            }
            out.push(b'\'');
            i += 2;
        } else {
            if pending.is_some() {
                return Err(surrogate_error_near(&body[i..]));
            }
            out.push(c);
            i += 1;
        }
    }
    if pending.is_some() {
        return Err(PgError::syntax("invalid Unicode surrogate pair at end of input", None));
    }
    if out.contains(&0) {
        return Err(PgError::new(
            SqlState::CHARACTER_NOT_IN_REPERTOIRE,
            "invalid byte sequence for encoding \"UTF8\": 0x00",
        ));
    }
    match String::from_utf8(out) {
        Ok(s) => Ok(s),
        Err(e) => {
            let idx = e.utf8_error().valid_up_to();
            let byte = e.as_bytes().get(idx).copied().unwrap_or(0);
            Err(PgError::new(
                SqlState::CHARACTER_NOT_IN_REPERTOIRE,
                format!("invalid byte sequence for encoding \"UTF8\": 0x{byte:02x}"),
            ))
        }
    }
}

// ---------------------------------------------------------------------------------------------
// Helpers: Unicode escape strings (U&'..' with the default escape character '\', parser.c
// `str_udeescape`)
// ---------------------------------------------------------------------------------------------

/// Decode a `U&'..'` body. Forms: `\XXXX`, `\+XXXXXX`, `\\` (one backslash), `''` (one quote).
fn decode_u_body(body: &str) -> Result<String> {
    let bytes = body.as_bytes();
    let mut out = String::with_capacity(body.len());
    let mut pending: Option<u32> = None;
    let mut i = 0usize;
    while i < bytes.len() {
        let c = bytes[i];
        if c == b'\\' {
            if bytes.get(i + 1) == Some(&b'\\') {
                if pending.is_some() {
                    return Err(surrogate_pair_error());
                }
                out.push('\\');
                i += 2;
                continue;
            }
            let plus = bytes.get(i + 1) == Some(&b'+');
            let (start, ndig) = if plus { (i + 2, 6) } else { (i + 1, 4) };
            let cp = match read_hex(bytes, start, ndig) {
                Some(v) => v,
                None => return Err(invalid_unicode_escape()),
            };
            i = start + ndig;
            if cp == 0 || cp > 0x10FFFF {
                return Err(PgError::syntax("invalid Unicode escape value", None));
            }
            if let Some(high) = pending.take() {
                if !is_low_surrogate(cp) {
                    return Err(surrogate_pair_error());
                }
                out.push(char_of(combine_surrogates(high, cp))?);
            } else if is_high_surrogate(cp) {
                pending = Some(cp);
            } else if is_low_surrogate(cp) {
                return Err(surrogate_pair_error());
            } else {
                out.push(char_of(cp)?);
            }
        } else if c == b'\'' && bytes.get(i + 1) == Some(&b'\'') {
            if pending.is_some() {
                return Err(surrogate_pair_error());
            }
            out.push('\'');
            i += 2;
        } else {
            if pending.is_some() {
                return Err(surrogate_pair_error());
            }
            let ch = match body.get(i..).and_then(|s| s.chars().next()) {
                Some(ch) => ch,
                None => break,
            };
            out.push(ch);
            i += ch.len_utf8();
        }
    }
    if pending.is_some() {
        return Err(surrogate_pair_error());
    }
    Ok(out)
}

fn surrogate_pair_error() -> PgError {
    PgError::new(SqlState::INVALID_ESCAPE_SEQUENCE, "invalid Unicode surrogate pair")
}

/// Decode a `U&'..'` body with position.
fn decode_uescape_string(body: &str, pos: Option<u32>) -> Result<String> {
    decode_u_body(body).map_err(|e| diag::with_position(e, pos))
}

#[cfg(test)]
mod tests {
    use super::*;
    use pgarbor_types::oids;

    fn number(text: &str, negate: bool) -> (Datum, TypeId) {
        number_to_const(text, negate).expect("numeric literal must classify")
    }

    fn numeric_dscale(d: &Datum) -> i64 {
        match d {
            Datum::Numeric(n) => n.dscale(),
            _ => -1,
        }
    }

    #[test]
    fn integer_literals_pick_narrowest_type() {
        let (d, t) = number("2147483647", false);
        assert!(matches!(d, Datum::Int4(2147483647)));
        assert_eq!(t.oid, oids::INT4);

        let (d, t) = number("2147483648", false);
        assert!(matches!(d, Datum::Int8(2147483648)));
        assert_eq!(t.oid, oids::INT8);

        let (d, t) = number("9223372036854775808", false);
        assert!(matches!(d, Datum::Numeric(_)));
        assert_eq!(t.oid, oids::NUMERIC);
    }

    #[test]
    fn negative_literal_folds_sign_before_classification() {
        let (d, t) = number("2147483648", true);
        assert!(matches!(d, Datum::Int4(i32::MIN)));
        assert_eq!(t.oid, oids::INT4);

        let (d, t) = number("9223372036854775808", true);
        assert!(matches!(d, Datum::Int8(i64::MIN)));
        assert_eq!(t.oid, oids::INT8);
    }

    #[test]
    fn decimal_literals_are_numeric_with_text_scale() {
        let (d, t) = number("1.50", false);
        assert_eq!(t.oid, oids::NUMERIC);
        assert_eq!(numeric_dscale(&d), 2);

        let (d, t) = number("1e3", false);
        assert_eq!(t.oid, oids::NUMERIC);
        assert_eq!(numeric_dscale(&d), 0);
    }

    #[test]
    fn radix_and_underscore_literals() {
        assert!(matches!(number("0x1F", false).0, Datum::Int4(31)));
        assert!(matches!(number("0o17", false).0, Datum::Int4(15)));
        assert!(matches!(number("0b101", false).0, Datum::Int4(5)));
        assert!(matches!(number("1_000", false).0, Datum::Int4(1000)));
        assert!(matches!(number("0xFFFFFFFF", false).0, Datum::Int8(4294967295)));
        assert!(matches!(number("0xFF_FF", false).0, Datum::Int4(65535)));
    }

    #[test]
    fn bad_literals_are_rejected() {
        assert!(number_to_const("0x", false).is_err());
        assert!(number_to_const("1__0", false).is_err());
        assert!(number_to_const("1_", false).is_err());
        assert!(number_to_const("0b102", false).is_err());
    }

    #[test]
    fn escape_string_decodes_control_and_octal_hex() {
        assert_eq!(decode_escape_string(r"a\nb", None).expect("decode"), "a\nb");
        assert_eq!(decode_escape_string(r"\t\r\b\f", None).expect("decode"), "\t\r\u{8}\u{c}");
        assert_eq!(decode_escape_string(r"\101", None).expect("decode"), "A");
        assert_eq!(decode_escape_string(r"\x41", None).expect("decode"), "A");
        assert_eq!(decode_escape_string(r"\xg", None).expect("decode"), "xg");
        assert_eq!(decode_escape_string(r"\q", None).expect("decode"), "q");
        assert_eq!(decode_escape_string(r"\\\'", None).expect("decode"), "\\'");
        assert_eq!(decode_escape_string("''", None).expect("decode"), "'");
    }

    #[test]
    fn escape_string_unicode_and_surrogates() {
        assert_eq!(decode_escape_string(r"é", None).expect("decode"), "\u{e9}");
        assert_eq!(decode_escape_string(r"\U0001F600", None).expect("decode"), "\u{1F600}");
        assert_eq!(decode_escape_string(r"😀", None).expect("decode"), "\u{1F600}");
        assert!(decode_escape_string(r"\uD83D", None).is_err());
        assert!(decode_escape_string(r"\uDE00", None).is_err());
        assert!(decode_escape_string(r"\u0000", None).is_err());
        assert!(decode_escape_string(r"\U00110000", None).is_err());
        assert!(decode_escape_string(r"\u12", None).is_err());
    }

    #[test]
    fn escape_string_invalid_bytes_and_nul() {
        assert!(decode_escape_string(r"\xff", None).is_err());
        assert!(decode_escape_string(r"\000", None).is_err());
        assert_eq!(decode_escape_string(r"\xc3\xa9", None).expect("decode"), "\u{e9}");
    }

    #[test]
    fn unicode_string_escapes() {
        assert_eq!(decode_u_body(r"\0041").expect("decode"), "A");
        assert_eq!(decode_u_body(r"\+01F600").expect("decode"), "\u{1F600}");
        assert_eq!(decode_u_body(r"\\").expect("decode"), "\\");
        assert_eq!(decode_u_body(r"\d83d\de00").expect("decode"), "\u{1F600}");
        assert!(decode_u_body(r"\d83d").is_err());
        assert!(decode_u_body(r"\zz").is_err());
    }
}
