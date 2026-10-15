//! JSON lexer/parser with PostgreSQL's exact error reporting (TYPE-08). Lease TYP-32.
//!
//! PG18 reference: `src/common/jsonapi.c` (`json_lex`, `json_lex_string`, `json_lex_number`,
//! `parse_scalar`, `parse_object`, `parse_array`, `json_errdetail`), `utils/adt/jsonfuncs.c`
//! (`json_errsave_error`, `report_json_context`).
//!
//! Grammar: RFC 8259 JSON as PG accepts it: whitespace = space, \t, \n, \r; numbers `-?(0|[1-9]\d*)(\.\d+)?([eE][+-]?\d+)?`;
//! literals `true`, `false`, `null` (case-sensitive); strings with escapes `\" \\ \/ \b \f \n \r \t \uXXXX`
//! (surrogate pairs combined). Nesting depth limit: PG uses stack-depth checks; use 6400 levels → 54001
//! `stack depth limit exceeded`.
//!
//! Every syntax error is 22P02 `invalid input syntax for type json` with a DETAIL (exact text below).
//! PG also sends CONTEXT `JSON data, line 1: <input up to the error>`; `PgError` has no context field
//! in contract v1 (amendment `pgarbor-types-02-error-context` requests one), so omit it.
//! DETAIL texts (oracle-verified):
//! - unexpected end: `The input string ended unexpectedly.` (`'[1,2'`, `''`)
//! - bad token: `Token "a" is invalid.` (token = maximal run of non-delimiter chars: `'{a'` → `a`,
//!   `'01'` → `01`, `'1.'` → `1.`, `'tru'` → `tru`, `'"abc'` (unterminated) → `"abc`, trailing `'[1,2] x'` → `x`)
//! - `Expected ":", but found "1".`; `Expected JSON value, but found "}".`;
//!   `Expected "," or "]", but found "x".`; `Expected "," or "}", but found "x".`;
//!   `Expected string, but found "1".`; `Expected string or "}", but found "1".`;
//!   `Expected array element or "]", but found "x".`; `Expected end of input, but found "x".`
//! - escapes: `Escape sequence "\x" is invalid.`; `"\u" must be followed by four hexadecimal digits.`;
//!   `Unicode high surrogate must not follow a high surrogate.`; `Unicode low surrogate must follow a high surrogate.`
//! - control char in string: `Character with value 0x0a must be escaped.`
//! - `\u0000` is accepted by `json` (kept verbatim) but rejected when converting to text/jsonb:
//!   22P05 `unsupported Unicode escape sequence`, DETAIL `\u0000 cannot be converted to text.`
use super::Jsonb;
use pgarbor_common::{PgError, Result};

/// Validate `json` input text (PG `json_in`): returns Ok(()) iff PG accepts it.
// CONTRACT
#[allow(unused_variables)]
pub fn validate_json(s: &str) -> Result<()> {
    Err(PgError::unimplemented("TYP-32", "json input"))
}

/// Parse text into a normalized `Jsonb` (PG `jsonb_in`): duplicate keys → last wins, keys sorted,
/// numbers parsed with `numeric::io::parse_numeric`, `\u0000` → 22P05 (see module docs).
// CONTRACT
#[allow(unused_variables)]
pub fn parse_jsonb(s: &str) -> Result<Jsonb> {
    Err(PgError::unimplemented("TYP-32", "jsonb input"))
}

/// Parse `json` text into a NON-normalized tree for json functions (`json_each`, `->` on json):
/// object members in input order, duplicates kept, as `(key, value)` pairs.
// CONTRACT
#[derive(Clone, Debug, PartialEq)]
pub enum JsonValue {
    Null,
    Bool(bool),
    /// Number exactly as written in the input (`1.0e2` stays `1.0e2`).
    Number(String),
    String(String),
    Array(Vec<JsonValue>),
    Object(Vec<(String, JsonValue)>),
}

/// Parse `json` text preserving order/duplicates/number spelling (see `JsonValue`).
// CONTRACT
#[allow(unused_variables)]
pub fn parse_json_value(s: &str) -> Result<JsonValue> {
    Err(PgError::unimplemented("TYP-32", "json parsing"))
}

/// Escape a string as a JSON string literal including quotes (PG `escape_json`):
/// `"` → `\"`, `\` → `\\`, \b \f \n \r \t, other chars < 0x20 → `\u00XX` (lowercase hex), rest raw.
pub fn escape_json(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\u{08}' => out.push_str("\\b"),
            '\u{0C}' => out.push_str("\\f"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}
