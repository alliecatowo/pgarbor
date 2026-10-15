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
//!
//! Implementation notes (worker-level, not contract):
//! - PG's `json_in` validates with `need_escapes = false`: escapes are checked for syntax only, so lone
//!   surrogates and `\u0000` are accepted. PG's `jsonb_in` and the json functions (`need_escapes = true`)
//!   also enforce surrogate pairing and reject `\u0000`. `validate_json` follows the first rule; `parse_jsonb` and
//!   `parse_json_value` follow the second. The `'"\ud83d"'` acceptance line therefore applies to jsonb, not json.
//! - The parser is iterative (explicit frame stack) so the 6400-level limit cannot overflow the Rust stack.
//! - Error ordering follows PG: each token is lexed when the previous one is consumed, a scalar's semantic
//!   action (number conversion) runs after the following token is lexed, and the top-level end check is last.
use super::{Jsonb, jsonb_key_cmp};
use crate::numeric::io::parse_numeric;
use pgarbor_common::{PgError, Result, SqlState};

/// Maximum open containers (PG `check_stack_depth` analogue; see module docs).
const MAX_DEPTH: usize = 6400;

/// Validate `json` input text (PG `json_in`): returns Ok(()) iff PG accepts it.
// CONTRACT
pub fn validate_json(s: &str) -> Result<()> {
    run::<ValidateBuilder>(s)
}

/// Parse text into a normalized `Jsonb` (PG `jsonb_in`): duplicate keys → last wins, keys sorted,
/// numbers parsed with `numeric::io::parse_numeric`, `\u0000` → 22P05 (see module docs).
// CONTRACT
pub fn parse_jsonb(s: &str) -> Result<Jsonb> {
    run::<JsonbBuilder>(s)
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
pub fn parse_json_value(s: &str) -> Result<JsonValue> {
    run::<ValueBuilder>(s)
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

// ---------------------------------------------------------------------------------------------
// Builders: what the parser produces for each value. `DECODE` mirrors PG's `need_escapes`.
// ---------------------------------------------------------------------------------------------

trait Builder {
    type Value;
    /// Decode string contents and apply the surrogate / `\u0000` rules (PG `need_escapes`).
    const DECODE: bool;
    fn null() -> Self::Value;
    fn boolean(b: bool) -> Self::Value;
    /// `lit` is the number exactly as written in the input.
    fn number(lit: &str) -> Result<Self::Value>;
    fn string(s: String) -> Self::Value;
    fn array(items: Vec<Self::Value>) -> Self::Value;
    fn object(pairs: Vec<(String, Self::Value)>) -> Self::Value;
}

/// `json_in`: syntax check only.
struct ValidateBuilder;

impl Builder for ValidateBuilder {
    type Value = ();
    const DECODE: bool = false;
    fn null() {}
    fn boolean(_b: bool) {}
    fn number(_lit: &str) -> Result<()> {
        Ok(())
    }
    fn string(_s: String) {}
    fn array(_items: Vec<()>) {}
    fn object(_pairs: Vec<(String, ())>) {}
}

/// Order- and duplicate-preserving tree for json functions.
struct ValueBuilder;

impl Builder for ValueBuilder {
    type Value = JsonValue;
    const DECODE: bool = true;
    fn null() -> JsonValue {
        JsonValue::Null
    }
    fn boolean(b: bool) -> JsonValue {
        JsonValue::Bool(b)
    }
    fn number(lit: &str) -> Result<JsonValue> {
        Ok(JsonValue::Number(lit.to_string()))
    }
    fn string(s: String) -> JsonValue {
        JsonValue::String(s)
    }
    fn array(items: Vec<JsonValue>) -> JsonValue {
        JsonValue::Array(items)
    }
    fn object(pairs: Vec<(String, JsonValue)>) -> JsonValue {
        JsonValue::Object(pairs)
    }
}

/// Normalized jsonb tree (`jsonb_in`).
struct JsonbBuilder;

impl Builder for JsonbBuilder {
    type Value = Jsonb;
    const DECODE: bool = true;
    fn null() -> Jsonb {
        Jsonb::Null
    }
    fn boolean(b: bool) -> Jsonb {
        Jsonb::Bool(b)
    }
    fn number(lit: &str) -> Result<Jsonb> {
        parse_numeric(lit).map(Jsonb::Number)
    }
    fn string(s: String) -> Jsonb {
        Jsonb::String(s)
    }
    fn array(items: Vec<Jsonb>) -> Jsonb {
        Jsonb::Array(items)
    }
    fn object(pairs: Vec<(String, Jsonb)>) -> Jsonb {
        normalize_object(pairs)
    }
}

/// jsonb object normalization: stable sort by jsonb key order, then the LAST duplicate wins.
fn normalize_object(mut pairs: Vec<(String, Jsonb)>) -> Jsonb {
    pairs.sort_by(|a, b| jsonb_key_cmp(&a.0, &b.0));
    let mut out: Vec<(String, Jsonb)> = Vec::with_capacity(pairs.len());
    for (k, v) in pairs {
        if let Some(last) = out.last_mut() {
            if last.0 == k {
                last.1 = v;
                continue;
            }
        }
        out.push((k, v));
    }
    Jsonb::Object(out)
}

// ---------------------------------------------------------------------------------------------
// Errors
// ---------------------------------------------------------------------------------------------

fn syntax_error(detail: String) -> PgError {
    PgError::new(SqlState::INVALID_TEXT_REPRESENTATION, "invalid input syntax for type json").with_detail(detail)
}

fn depth_error() -> PgError {
    PgError::new(SqlState::STATEMENT_TOO_COMPLEX, "stack depth limit exceeded").with_hint(
        "Increase the configured \"max_stack_depth\" (currently 2048kB), after ensuring the platform's stack depth limit is adequate.",
    )
}

fn untranslatable_error() -> PgError {
    PgError::new(SqlState::UNTRANSLATABLE_CHARACTER, "unsupported Unicode escape sequence")
        .with_detail("\\u0000 cannot be converted to text.")
}

fn escape_format_error() -> PgError {
    syntax_error("\"\\u\" must be followed by four hexadecimal digits.".to_string())
}

fn high_surrogate_error() -> PgError {
    syntax_error("Unicode high surrogate must not follow a high surrogate.".to_string())
}

fn low_surrogate_error() -> PgError {
    syntax_error("Unicode low surrogate must follow a high surrogate.".to_string())
}

// ---------------------------------------------------------------------------------------------
// Lexer (jsonapi.c json_lex and friends)
// ---------------------------------------------------------------------------------------------

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Kind {
    ObjStart,
    ObjEnd,
    ArrStart,
    ArrEnd,
    Comma,
    Colon,
    Str,
    Num,
    True,
    False,
    Null,
    End,
}

#[derive(Debug)]
struct Token {
    kind: Kind,
    /// Byte offsets into the input (`start` is meaningless for `End`).
    start: usize,
    end: usize,
    /// Decoded contents of a `Str` token when decoding is enabled; empty otherwise.
    text: String,
}

/// Parser context for `report_parse_error` (jsonapi.c `JsonParseContext`).
#[derive(Clone, Copy)]
enum Ctx {
    Value,
    String,
    ArrayNext,
    ObjectStart,
    ObjectLabel,
    ObjectNext,
    End,
}

/// `JSON_ALPHANUMERIC_CHAR`: letters, digits, `_`, and any high-bit byte.
fn is_alnum(c: u8) -> bool {
    c.is_ascii_alphanumeric() || c == b'_' || c >= 0x80
}

fn hex_value(c: u8) -> Option<u32> {
    match c {
        b'0'..=b'9' => Some((c - b'0') as u32),
        b'a'..=b'f' => Some((c - b'a') as u32 + 10),
        b'A'..=b'F' => Some((c - b'A') as u32 + 10),
        _ => None,
    }
}

struct Scanner<'a> {
    src: &'a str,
    pos: usize,
    decode: bool,
    cur: Token,
}

impl<'a> Scanner<'a> {
    fn new(src: &'a str, decode: bool) -> Result<Scanner<'a>> {
        let mut sc = Scanner {
            src,
            pos: 0,
            decode,
            cur: Token { kind: Kind::End, start: src.len(), end: src.len(), text: String::new() },
        };
        sc.cur = sc.lex()?;
        Ok(sc)
    }

    /// Slice of the input, or "" if the offsets are not valid (never panics on user data).
    fn slice(&self, start: usize, end: usize) -> &'a str {
        let src: &'a str = self.src;
        src.get(start..end).unwrap_or("")
    }

    fn advance(&mut self) -> Result<()> {
        self.cur = self.lex()?;
        Ok(())
    }

    fn expect(&mut self, kind: Kind, ctx: Ctx) -> Result<()> {
        if self.cur.kind == kind { self.advance() } else { Err(self.report(ctx)) }
    }

    /// `report_parse_error` + `json_errdetail`: the error for an unexpected current token.
    fn report(&self, ctx: Ctx) -> PgError {
        if self.cur.kind == Kind::End {
            return syntax_error("The input string ended unexpectedly.".to_string());
        }
        let t = self.slice(self.cur.start, self.cur.end);
        let detail = match ctx {
            Ctx::Value => format!("Expected JSON value, but found \"{t}\"."),
            Ctx::String => format!("Expected string, but found \"{t}\"."),
            Ctx::ArrayNext => format!("Expected \",\" or \"]\", but found \"{t}\"."),
            Ctx::ObjectStart => format!("Expected string or \"}}\", but found \"{t}\"."),
            Ctx::ObjectLabel => format!("Expected \":\", but found \"{t}\"."),
            Ctx::ObjectNext => format!("Expected \",\" or \"}}\", but found \"{t}\"."),
            Ctx::End => format!("Expected end of input, but found \"{t}\"."),
        };
        syntax_error(detail)
    }

    /// Consume a `Str` token that is the current token, then the `:` after it.
    fn take_key(&mut self) -> Result<String> {
        let key = std::mem::take(&mut self.cur.text);
        self.advance()?;
        self.expect(Kind::Colon, Ctx::ObjectLabel)?;
        Ok(key)
    }

    fn invalid_token(&self, start: usize, end: usize) -> PgError {
        syntax_error(format!("Token \"{}\" is invalid.", self.slice(start, end)))
    }

    fn escape_invalid(&self, s: usize) -> PgError {
        // The escaped character, whole (it may be multi-byte).
        let len = self.src.get(s..).and_then(|t| t.chars().next()).map_or(1, |c| c.len_utf8());
        syntax_error(format!("Escape sequence \"\\{}\" is invalid.", self.slice(s, s + len)))
    }

    /// Lex the next token starting at `self.pos` (json_lex).
    fn lex(&mut self) -> Result<Token> {
        let src = self.src;
        let b = src.as_bytes();
        let n = b.len();
        let mut s = self.pos;
        while s < n && matches!(b[s], b' ' | b'\t' | b'\n' | b'\r') {
            s += 1;
        }
        if s >= n {
            self.pos = n;
            return Ok(Token { kind: Kind::End, start: n, end: n, text: String::new() });
        }
        let punct = match b[s] {
            b'{' => Some(Kind::ObjStart),
            b'}' => Some(Kind::ObjEnd),
            b'[' => Some(Kind::ArrStart),
            b']' => Some(Kind::ArrEnd),
            b',' => Some(Kind::Comma),
            b':' => Some(Kind::Colon),
            _ => None,
        };
        if let Some(kind) = punct {
            self.pos = s + 1;
            return Ok(Token { kind, start: s, end: s + 1, text: String::new() });
        }
        match b[s] {
            b'"' => {
                let (end, text) = self.lex_string(s)?;
                self.pos = end;
                Ok(Token { kind: Kind::Str, start: s, end, text })
            }
            b'-' => self.lex_number(s, s + 1),
            b'0'..=b'9' => self.lex_number(s, s),
            c if is_alnum(c) => {
                let mut p = s;
                while p < n && is_alnum(b[p]) {
                    p += 1;
                }
                let word = &b[s..p];
                let kind = if word == b"true" {
                    Kind::True
                } else if word == b"false" {
                    Kind::False
                } else if word == b"null" {
                    Kind::Null
                } else {
                    return Err(self.invalid_token(s, p));
                };
                self.pos = p;
                Ok(Token { kind, start: s, end: p, text: String::new() })
            }
            _ => Err(self.invalid_token(s, s + 1)),
        }
    }

    /// json_lex_number. `start` is the token start (may be a `-`); `first` is the first digit position.
    fn lex_number(&mut self, start: usize, first: usize) -> Result<Token> {
        let src = self.src;
        let b = src.as_bytes();
        let n = b.len();
        let mut s = first;
        let mut error = false;
        // Part 2: integer part.
        if s < n && b[s] == b'0' {
            s += 1;
        } else if s < n && (b'1'..=b'9').contains(&b[s]) {
            s += 1;
            while s < n && b[s].is_ascii_digit() {
                s += 1;
            }
        } else {
            error = true;
        }
        // Part 3: optional fraction.
        if s < n && b[s] == b'.' {
            s += 1;
            if s >= n || !b[s].is_ascii_digit() {
                error = true;
            } else {
                s += 1;
                while s < n && b[s].is_ascii_digit() {
                    s += 1;
                }
            }
        }
        // Part 4: optional exponent.
        if s < n && (b[s] == b'e' || b[s] == b'E') {
            s += 1;
            if s < n && (b[s] == b'+' || b[s] == b'-') {
                s += 1;
            }
            if s >= n || !b[s].is_ascii_digit() {
                error = true;
            } else {
                s += 1;
                while s < n && b[s].is_ascii_digit() {
                    s += 1;
                }
            }
        }
        // Trailing alphanumeric junk is part of the bad token.
        while s < n && is_alnum(b[s]) {
            s += 1;
            error = true;
        }
        self.pos = s;
        if error {
            return Err(self.invalid_token(start, s));
        }
        Ok(Token { kind: Kind::Num, start, end: s, text: String::new() })
    }

    /// json_lex_string. `start` is the opening quote. Returns (end offset after closing quote, decoded text).
    fn lex_string(&self, start: usize) -> Result<(usize, String)> {
        let src = self.src;
        let b = src.as_bytes();
        let n = b.len();
        let decode = self.decode;
        let mut out = String::new();
        // Pending high surrogate (decode mode only).
        let mut hi: Option<u32> = None;
        let mut s = start + 1;
        loop {
            if s >= n {
                return Err(self.invalid_token(start, n));
            }
            let c = b[s];
            if c == b'"' {
                if hi.is_some() {
                    return Err(low_surrogate_error());
                }
                return Ok((s + 1, out));
            }
            if c == b'\\' {
                s += 1;
                if s >= n {
                    return Err(self.invalid_token(start, n));
                }
                let e = b[s];
                if e == b'u' {
                    let mut ch: u32 = 0;
                    for _ in 0..4 {
                        s += 1;
                        if s >= n {
                            return Err(self.invalid_token(start, n));
                        }
                        match hex_value(b[s]) {
                            Some(d) => ch = ch * 16 + d,
                            None => return Err(escape_format_error()),
                        }
                    }
                    if decode {
                        if (0xD800..=0xDBFF).contains(&ch) {
                            if hi.is_some() {
                                return Err(high_surrogate_error());
                            }
                            hi = Some(ch);
                            s += 1;
                            continue;
                        }
                        if (0xDC00..=0xDFFF).contains(&ch) {
                            match hi.take() {
                                Some(h) => ch = 0x10000 + ((h - 0xD800) << 10) + (ch - 0xDC00),
                                None => return Err(low_surrogate_error()),
                            }
                        }
                        if hi.is_some() {
                            return Err(low_surrogate_error());
                        }
                        if ch == 0 {
                            return Err(untranslatable_error());
                        }
                        match char::from_u32(ch) {
                            Some(chr) => out.push(chr),
                            None => return Err(low_surrogate_error()),
                        }
                    }
                    s += 1;
                } else if decode {
                    if hi.is_some() {
                        return Err(low_surrogate_error());
                    }
                    let chr = match e {
                        b'"' => '"',
                        b'\\' => '\\',
                        b'/' => '/',
                        b'b' => '\u{08}',
                        b'f' => '\u{0C}',
                        b'n' => '\n',
                        b'r' => '\r',
                        b't' => '\t',
                        _ => return Err(self.escape_invalid(s)),
                    };
                    out.push(chr);
                    s += 1;
                } else {
                    if !matches!(e, b'"' | b'\\' | b'/' | b'b' | b'f' | b'n' | b'r' | b't') {
                        return Err(self.escape_invalid(s));
                    }
                    s += 1;
                }
            } else {
                // A run of ordinary characters up to the next quote or backslash.
                if hi.is_some() {
                    return Err(low_surrogate_error());
                }
                let mut p = s;
                while p < n && b[p] != b'"' && b[p] != b'\\' {
                    if b[p] <= 31 {
                        return Err(syntax_error(format!(
                            "Character with value 0x{:02x} must be escaped.",
                            b[p]
                        )));
                    }
                    p += 1;
                }
                if decode {
                    out.push_str(self.slice(s, p));
                }
                s = p;
            }
        }
    }
}

// ---------------------------------------------------------------------------------------------
// Parser (jsonapi.c parse_scalar / parse_object / parse_array, made iterative)
// ---------------------------------------------------------------------------------------------

/// An open container while parsing.
enum Frame<V> {
    Array(Vec<V>),
    /// Collected members plus the key of the member whose value is being parsed.
    Object(Vec<(String, V)>, String),
}

fn run<B: Builder>(src: &str) -> Result<B::Value> {
    let mut sc = Scanner::new(src, B::DECODE)?;
    let mut stack: Vec<Frame<B::Value>> = Vec::new();

    'value: loop {
        // Start of a value at the current token.
        let mut done: B::Value = match sc.cur.kind {
            Kind::ObjStart => {
                if stack.len() >= MAX_DEPTH {
                    return Err(depth_error());
                }
                sc.advance()?;
                match sc.cur.kind {
                    Kind::Str => {
                        let key = sc.take_key()?;
                        stack.push(Frame::Object(Vec::new(), key));
                        continue 'value;
                    }
                    Kind::ObjEnd => {
                        sc.advance()?;
                        B::object(Vec::new())
                    }
                    _ => return Err(sc.report(Ctx::ObjectStart)),
                }
            }
            Kind::ArrStart => {
                if stack.len() >= MAX_DEPTH {
                    return Err(depth_error());
                }
                sc.advance()?;
                if sc.cur.kind == Kind::ArrEnd {
                    sc.advance()?;
                    B::array(Vec::new())
                } else {
                    stack.push(Frame::Array(Vec::new()));
                    continue 'value;
                }
            }
            Kind::Str => {
                let s = std::mem::take(&mut sc.cur.text);
                sc.advance()?;
                B::string(s)
            }
            Kind::Num => {
                let (s, e) = (sc.cur.start, sc.cur.end);
                sc.advance()?;
                B::number(sc.slice(s, e))?
            }
            Kind::True => {
                sc.advance()?;
                B::boolean(true)
            }
            Kind::False => {
                sc.advance()?;
                B::boolean(false)
            }
            Kind::Null => {
                sc.advance()?;
                B::null()
            }
            _ => return Err(sc.report(Ctx::Value)),
        };

        // A value is complete: attach it to its parent, closing containers as needed.
        loop {
            match stack.last_mut() {
                None => {
                    if sc.cur.kind != Kind::End {
                        return Err(sc.report(Ctx::End));
                    }
                    return Ok(done);
                }
                Some(Frame::Array(items)) => {
                    items.push(done);
                    if sc.cur.kind == Kind::Comma {
                        sc.advance()?;
                        continue 'value;
                    }
                    sc.expect(Kind::ArrEnd, Ctx::ArrayNext)?;
                    let Some(Frame::Array(items)) = stack.pop() else {
                        return Err(PgError::internal("json parser frame stack corrupted"));
                    };
                    done = B::array(items);
                }
                Some(Frame::Object(pairs, key)) => {
                    pairs.push((std::mem::take(key), done));
                    if sc.cur.kind == Kind::Comma {
                        sc.advance()?;
                        if sc.cur.kind != Kind::Str {
                            return Err(sc.report(Ctx::String));
                        }
                        *key = sc.take_key()?;
                        continue 'value;
                    }
                    sc.expect(Kind::ObjEnd, Ctx::ObjectNext)?;
                    let Some(Frame::Object(pairs, _)) = stack.pop() else {
                        return Err(PgError::internal("json parser frame stack corrupted"));
                    };
                    done = B::object(pairs);
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::numeric::io::parse_numeric;

    fn detail_of<T: std::fmt::Debug>(r: Result<T>) -> (String, String) {
        let e = r.expect_err("expected an error");
        assert_eq!(e.sqlstate.code(), "22P02");
        assert_eq!(e.message, "invalid input syntax for type json");
        (e.message.clone(), e.detail.clone().unwrap_or_default())
    }

    #[test]
    fn bad_token_details() {
        assert_eq!(detail_of(validate_json("{a")).1, "Token \"a\" is invalid.");
        assert_eq!(detail_of(validate_json("01")).1, "Token \"01\" is invalid.");
        assert_eq!(detail_of(validate_json("1.")).1, "Token \"1.\" is invalid.");
        assert_eq!(detail_of(validate_json("tru")).1, "Token \"tru\" is invalid.");
        assert_eq!(detail_of(validate_json("\"abc")).1, "Token \"\"abc\" is invalid.");
        assert_eq!(detail_of(validate_json("[1,2] x")).1, "Token \"x\" is invalid.");
    }

    #[test]
    fn structure_details() {
        assert_eq!(detail_of(validate_json("{\"a\":}")).1, "Expected JSON value, but found \"}\".");
        assert_eq!(detail_of(validate_json("[1,2")).1, "The input string ended unexpectedly.");
        assert_eq!(detail_of(validate_json("")).1, "The input string ended unexpectedly.");
        assert_eq!(detail_of(validate_json("1 2")).1, "Expected end of input, but found \"2\".");
        assert_eq!(detail_of(validate_json("[1 2]")).1, "Expected \",\" or \"]\", but found \"2\".");
        assert_eq!(detail_of(validate_json("{\"a\" 1}")).1, "Expected \":\", but found \"1\".");
        assert_eq!(detail_of(validate_json("{\"a\":1 \"b\":2}")).1, "Expected \",\" or \"}\", but found \"\"b\"\".");
        assert_eq!(detail_of(validate_json("{1:2}")).1, "Expected string or \"}\", but found \"1\".");
        assert_eq!(detail_of(validate_json("{\"a\":1,1:2}")).1, "Expected string, but found \"1\".");
        assert_eq!(detail_of(validate_json("[1,]")).1, "Expected JSON value, but found \"]\".");
    }

    #[test]
    fn string_details() {
        assert_eq!(detail_of(validate_json("\"\\x\"")).1, "Escape sequence \"\\x\" is invalid.");
        assert_eq!(detail_of(validate_json("\"\\u12x4\"")).1, "\"\\u\" must be followed by four hexadecimal digits.");
        assert_eq!(detail_of(validate_json("\"a\nb\"")).1, "Character with value 0x0a must be escaped.");
        assert_eq!(
            detail_of(parse_jsonb("\"\\ud83d\"")).1,
            "Unicode low surrogate must follow a high surrogate."
        );
        assert_eq!(
            detail_of(parse_jsonb("\"\\ud83d\\ud83d\"")).1,
            "Unicode high surrogate must not follow a high surrogate."
        );
        assert_eq!(detail_of(parse_jsonb("\"\\ude00\"")).1, "Unicode low surrogate must follow a high surrogate.");
    }

    #[test]
    fn json_accepts_lone_surrogates_and_nul_escape() {
        assert!(validate_json("\"\\ud83d\"").is_ok());
        assert!(validate_json("\"\\u0000\"").is_ok());
        assert!(validate_json("{\"a\" : [1, -0.5e+10, true, false, null, \"\\/\\b\\f\\n\\r\\t\"]}\n").is_ok());
    }

    #[test]
    fn jsonb_nul_escape_is_22p05() {
        let e = parse_jsonb("\"\\u0000\"").expect_err("nul escape");
        assert_eq!(e.sqlstate.code(), "22P05");
        assert_eq!(e.message, "unsupported Unicode escape sequence");
        assert_eq!(e.detail.as_deref(), Some("\\u0000 cannot be converted to text."));
    }

    #[test]
    fn jsonb_duplicate_keys_last_wins_and_sorted() {
        let j = parse_jsonb("{\"a\":1,\"a\":2}").expect("valid");
        assert_eq!(
            j,
            Jsonb::Object(vec![("a".to_string(), Jsonb::Number(parse_numeric("2").expect("n")))])
        );
        let j = parse_jsonb("{\"b\":1,\"aa\":2,\"a\":3}").expect("valid");
        let keys: Vec<&str> = match &j {
            Jsonb::Object(p) => p.iter().map(|(k, _)| k.as_str()).collect(),
            _ => panic!("object expected"),
        };
        assert_eq!(keys, vec!["a", "b", "aa"]);
    }

    #[test]
    fn json_value_preserves_order_and_spelling() {
        let v = parse_json_value("{\"a\":1.0e2,\"a\":3}").expect("valid");
        assert_eq!(
            v,
            JsonValue::Object(vec![
                ("a".to_string(), JsonValue::Number("1.0e2".to_string())),
                ("a".to_string(), JsonValue::Number("3".to_string())),
            ])
        );
    }

    #[test]
    fn nesting_limit() {
        let ok = format!("{}{}", "[".repeat(MAX_DEPTH), "]".repeat(MAX_DEPTH));
        assert!(validate_json(&ok).is_ok());
        let too_deep = "[".repeat(MAX_DEPTH + 1);
        let e = validate_json(&too_deep).expect_err("too deep");
        assert_eq!(e.sqlstate.code(), "54001");
        assert_eq!(e.message, "stack depth limit exceeded");
    }
}
