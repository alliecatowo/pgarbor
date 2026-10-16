//! Translation of `sqlparser` / tokenizer errors into PostgreSQL syntax errors (SQL-03).
//!
//! PostgreSQL's bison parser reports every grammar failure as
//! `ERROR: syntax error at or near "TOKEN"` (SQLSTATE 42601) with `LINE n:` and a caret at the
//! offending token, or `syntax error at end of input` when the input ended prematurely. Clients
//! (psql, drivers, the differential tests) compare SQLSTATE always and the message text, plus
//! position, for these. sqlparser instead reports `ParserError::ParserError("Expected: an SQL
//! statement, found: foo at Line: 1, Column: 8")`, i.e. a formatted string with a trailing
//! ` at Line: L, Column: C` produced by `Location`'s Display impl (see sqlparser
//! `src/tokenizer.rs` `impl Display for Location` and `Parser::expected`).
//!
//! ## Rules
//! `translate_parser_error(err, src, tokens)`:
//! 1. `ParserError::RecursionLimitExceeded` -> 54001 `stack depth limit exceeded` with HINT
//!    `Increase the configuration parameter "max_stack_depth" (currently 2048kB), after ensuring the
//!    platform's stack depth limit is adequate.`
//! 2. `ParserError::TokenizerError(msg)` -> same as `translate_tokenizer_message`.
//! 3. `ParserError::ParserError(msg)`: extract the location with the suffix regex-free parse
//!    `... at Line: <L>, Column: <C>` (search `rfind(" at Line: ")`, then parse the two integers;
//!    no suffix = location unknown). Find the offending token = the token in `tokens` whose
//!    `span.start` equals that location (tokens is the statement's token slice incl. EOF).
//!    * token is `Token::EOF` (or location past the last token) -> `diag::syntax_error_eof(src.end_pos())`.
//!    * otherwise -> `diag::syntax_error_near(<token source text>, pos)`, where the token text is the
//!      exact source slice `src.slice(byte_start, byte_end)` of the token (PG prints the token as
//!      written: `select 1 from where` -> near "where"; for a string literal token PG prints the
//!      literal including quotes: `near "'abc'"`). Position = `src.char_pos(location)`.
//! 4. A parser message that is not an "Expected: ..., found: ..." message (for example
//!    `Unsupported ...`, `Invalid ...`, `Cannot ...`, literal-overflow messages such as
//!    `Could not parse '999999999999999999999' as i64`) has no PG equivalent: map
//!    `Could not parse` -> number-literal handling is the binder's, so report 42601 with
//!    `syntax error at or near "<token at location>"` when a location exists, else
//!    `syntax error`. Never return 0A000 or XX000 for a parser failure.
//!
//! `translate_tokenizer_error(msg, location, src)` (location comes from `TokenizerError.location`):
//! * message contains `Unterminated string literal` / `Unterminated encoded string literal` ->
//!   42601 `unterminated quoted string at or near "'abc"` (near = source text from the opening
//!   quote to end of input; PG prints up to the end of the line? Verify with the oracle: it prints
//!   the unterminated text up to end of input), position = opening quote.
//! * `Unterminated dollar-quoted string` -> `unterminated dollar-quoted string at or near "$$abc"`.
//! * `Unterminated comment` / `Unexpected EOF while in a multi-line comment` ->
//!   `unterminated /* comment at or near "/* abc"`.
//! * `Expected close delimiter '"' before EOF` (quoted identifier) ->
//!   `unterminated quoted identifier at or near ""abc"`.
//! * empty quoted identifier -> `zero-length delimited identifier at or near """"`.
//! * anything else -> `syntax error at or near "<char at location>"`.
//!
//! Oracle fixtures (PG 18.6): `select 1 2` -> `syntax error at or near "2"` position 10;
//! `selec 1` -> `syntax error at or near "selec"` position 1; `select (` -> `syntax error at end of
//! input` position 9; `select 'abc` -> `unterminated quoted string at or near "'abc"` position 8;
//! `select 1 from` -> `syntax error at end of input` position 14.
use pgarbor_common::{PgError, Result, SqlState};
use sqlparser::parser::ParserError;
use sqlparser::tokenizer::{Location, Token, TokenWithSpan};

use crate::diag;
use crate::source::SourceMap;

/// Parse the ` at Line: L, Column: C` suffix sqlparser appends to parser/tokenizer messages.
/// Returns `None` when absent. Fully implemented (trivial).
pub fn location_from_message(msg: &str) -> Option<Location> {
    let idx = msg.rfind(" at Line: ")?;
    let rest = &msg[idx + " at Line: ".len()..];
    let (line_s, col_s) = rest.split_once(", Column: ")?;
    let line = line_s.trim().parse::<u64>().ok()?;
    let col = col_s.trim().parse::<u64>().ok()?;
    Some(Location { line, column: col })
}

/// Translate a parser error (module docs, rules 1-4).
pub fn translate_parser_error(err: &ParserError, src: &SourceMap, tokens: &[TokenWithSpan]) -> Result<PgError> {
    let error = match err {
        ParserError::RecursionLimitExceeded => stack_depth_error(),
        ParserError::TokenizerError(msg) => tokenizer_problem(msg, location_from_message(msg), src),
        ParserError::ParserError(msg) => parser_problem(msg, src, tokens),
    };
    Ok(error)
}

/// Translate a tokenizer error (module docs). `Ok(error)`: the translated PG error to return to the
/// client; the `Result` wrapper only exists so the body can report an unimplemented placeholder.
pub fn translate_tokenizer_error(message: &str, location: Location, src: &SourceMap) -> Result<PgError> {
    Ok(tokenizer_problem(message, Some(location), src))
}

/// Rule 1: 54001 with the PG HINT for `max_stack_depth`.
fn stack_depth_error() -> PgError {
    PgError::new(SqlState::STATEMENT_TOO_COMPLEX, "stack depth limit exceeded").with_hint(
        "Increase the configuration parameter \"max_stack_depth\" (currently 2048kB), after ensuring the platform's stack depth limit is adequate.",
    )
}

/// Rule 3 (and 4): a `ParserError::ParserError` message.
fn parser_problem(msg: &str, src: &SourceMap, tokens: &[TokenWithSpan]) -> PgError {
    let loc = match location_from_message(msg) {
        Some(loc) => loc,
        None => {
            // No location suffix: `Expected: ..., found: EOF` (sqlparser's EOF_TOKEN has no span).
            return if msg.starts_with("Expected: ") && msg.contains(", found: EOF") {
                diag::syntax_error_eof(src.end_pos())
            } else {
                diag::syntax("syntax error", None)
            };
        }
    };
    let found = tokens.iter().find(|t| t.span.start == loc && !matches!(t.token, Token::Whitespace(_)));
    match found {
        Some(t) => token_problem(t, src),
        // No token starts here: fall back to the character at the location (EOF when past the text).
        None => error_at_location(loc, src),
    }
}

/// The error for an offending token, printed the way PG prints it (source text, not `Display`).
pub(crate) fn token_problem(t: &TokenWithSpan, src: &SourceMap) -> PgError {
    let pos = src.char_pos(t.span.start);
    match &t.token {
        Token::EOF => diag::syntax_error_eof(src.end_pos()),
        Token::Word(w) if w.quote_style == Some('"') && w.value.is_empty() => {
            diag::unterminated("zero-length delimited identifier", token_source(src, t), pos)
        }
        _ => diag::syntax_error_near(token_source(src, t), pos),
    }
}

/// Source text of a token (exclusive end location).
fn token_source<'a>(src: &'a SourceMap, t: &TokenWithSpan) -> &'a str {
    match (src.byte_offset(t.span.start), src.byte_offset(t.span.end)) {
        (Some(s), Some(e)) => src.slice(s, e),
        _ => "",
    }
}

/// `syntax error at or near "<char at loc>"`, or end-of-input error when the location is at or past
/// the end of the text.
fn error_at_location(loc: Location, src: &SourceMap) -> PgError {
    let pos = src.char_pos(loc);
    let off = match src.byte_offset(loc) {
        Some(off) => off,
        None => return diag::syntax("syntax error", None),
    };
    match src.slice(off, off + 4).chars().next() {
        Some(ch) => diag::syntax_error_near(&ch.to_string(), pos),
        None => diag::syntax_error_eof(src.end_pos()),
    }
}

/// Kinds of construct the tokenizer can leave open at end of input.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum OpenKind {
    Comment,
    Dollar,
    QuotedIdent,
}

fn is_tag_byte(c: u8) -> bool {
    c.is_ascii_alphanumeric() || c == b'_' || c >= 0x80
}

fn is_ident_byte(c: u8) -> bool {
    is_tag_byte(c) || c == b'$'
}

/// `b[i]` is the opening quote; returns the index just past the closing quote, `None` if unterminated.
fn skip_quoted(b: &[u8], mut i: usize, quote: u8, backslash_escapes: bool) -> Option<usize> {
    i += 1;
    while i < b.len() {
        let c = b[i];
        if backslash_escapes && c == b'\\' {
            i += 2;
            continue;
        }
        if c == quote {
            if i + 1 < b.len() && b[i + 1] == quote {
                i += 2;
                continue;
            }
            return Some(i + 1);
        }
        i += 1;
    }
    None
}

/// Finds the first construct that the tokenizer leaves open at end of input (unterminated comment,
/// dollar-quoted string or quoted identifier), returning its kind and byte offset. Mirrors the
/// tokenizer's lexical rules closely enough to locate the opener, which sqlparser does not report
/// (its error location is the end of input).
fn first_unterminated(text: &str) -> Option<(OpenKind, usize)> {
    let b = text.as_bytes();
    let n = b.len();
    let mut i = 0usize;
    while i < n {
        let c = b[i];
        if c == b'\'' || c == b'"' {
            let backslash = c == b'\'' && i > 0 && (b[i - 1] == b'E' || b[i - 1] == b'e') && (i < 2 || !is_ident_byte(b[i - 2]));
            if c == b'"' {
                match skip_quoted(b, i, c, false) {
                    Some(next) => i = next,
                    None => return Some((OpenKind::QuotedIdent, i)),
                }
            } else {
                // Unterminated plain strings are reported by the tokenizer itself, not here.
                i = skip_quoted(b, i, c, backslash)?;
            }
        } else if c == b'-' && i + 1 < n && b[i + 1] == b'-' {
            while i < n && b[i] != b'\n' {
                i += 1;
            }
        } else if c == b'/' && i + 1 < n && b[i + 1] == b'*' {
            // Block comments nest in PostgreSQL.
            let start = i;
            let mut depth = 1usize;
            i += 2;
            while depth > 0 && i < n {
                if b[i] == b'/' && i + 1 < n && b[i + 1] == b'*' {
                    depth += 1;
                    i += 2;
                } else if b[i] == b'*' && i + 1 < n && b[i + 1] == b'/' {
                    depth -= 1;
                    i += 2;
                } else {
                    i += 1;
                }
            }
            if depth > 0 {
                return Some((OpenKind::Comment, start));
            }
        } else if c == b'$' && (i == 0 || !is_ident_byte(b[i - 1])) {
            // Dollar-quote opener `$tag$`; the tag may not start with a digit (`$1` is a parameter).
            let mut j = i + 1;
            while j < n && is_tag_byte(b[j]) {
                j += 1;
            }
            if j < n && b[j] == b'$' && !(j > i + 1 && b[i + 1].is_ascii_digit()) {
                let tag = &text[i..=j];
                match text[j + 1..].find(tag) {
                    Some(off) => i = j + 1 + off + tag.len(),
                    None => return Some((OpenKind::Dollar, i)),
                }
            } else {
                i += 1;
            }
        } else {
            i += 1;
        }
    }
    None
}

/// Rule 2 and the tokenizer-message table of the module docs. `loc` is the tokenizer location.
fn tokenizer_problem(message: &str, loc: Option<Location>, src: &SourceMap) -> PgError {
    let text = src.text();
    let loc_off = loc.and_then(|l| src.byte_offset(l));
    let loc_pos = loc.and_then(|l| src.char_pos(l));

    if message.contains("Unterminated string literal") || message.contains("Unterminated encoded string literal") {
        return match loc_off {
            Some(off) => diag::unterminated("unterminated quoted string", src.slice(off, text.len()), loc_pos),
            None => diag::syntax("syntax error", None),
        };
    }

    let want = if message.contains("Unterminated dollar-quoted") {
        Some(OpenKind::Dollar)
    } else if message.contains("Unterminated comment") || message.contains("Unexpected EOF while in a multi-line comment") {
        Some(OpenKind::Comment)
    } else if message.contains("Expected close delimiter") || message.contains("Expected nested delimiter") {
        Some(OpenKind::QuotedIdent)
    } else {
        None
    };

    if let Some(want) = want {
        // sqlparser reports these at end of input, so locate the opener in the text instead.
        let opener = match first_unterminated(text) {
            Some((kind, off)) if kind == want => Some(off),
            _ => loc_off,
        };
        return match opener {
            Some(off) => {
                let near = src.slice(off, text.len());
                let pos = Some(src.pos_of_byte(off));
                match want {
                    OpenKind::Comment => diag::unterminated("unterminated /* comment", near, pos),
                    OpenKind::Dollar => diag::unterminated("unterminated dollar-quoted string", near, pos),
                    OpenKind::QuotedIdent => diag::unterminated("unterminated quoted identifier", near, pos),
                }
            }
            None => diag::syntax("syntax error", None),
        };
    }

    // sqlparser rejects a misplaced digit separator (`1__0`, `1_`, `1.5_e5`) inside the literal; PG's scanner
    // reads the whole run as `integer_junk` / `real_junk` and reports it from the literal's first character.
    if message.contains("Unexpected character '_'") {
        if let Some(off) = loc_off {
            let bytes = text.as_bytes();
            let word = |b: u8| b.is_ascii_alphanumeric() || b == b'_';
            let mut start = off;
            while start > 0 && (word(bytes[start - 1]) || bytes[start - 1] == b'.') {
                start -= 1;
            }
            let mut end = off;
            while end < bytes.len() && word(bytes[end]) {
                end += 1;
            }
            if start < off && (bytes[start].is_ascii_digit() || bytes[start] == b'.') {
                return diag::syntax(
                    format!("trailing junk after numeric literal at or near \"{}\"", src.slice(start, end)),
                    Some(src.pos_of_byte(start)),
                );
            }
        }
    }

    match loc {
        Some(l) => error_at_location(l, src),
        None => diag::syntax("syntax error", None),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn suffix_parse() {
        let l = location_from_message("Expected: an SQL statement, found: foo at Line: 1, Column: 8").unwrap();
        assert_eq!((l.line, l.column), (1, 8));
        assert!(location_from_message("no location").is_none());
    }

    use sqlparser::dialect::PostgreSqlDialect;
    use sqlparser::parser::Parser;
    use sqlparser::tokenizer::Tokenizer;

    fn unwrap_translated(r: Result<PgError>) -> PgError {
        match r {
            Ok(e) => e,
            Err(_) => panic!("translation must not fail"),
        }
    }

    /// Runs the real sqlparser on `sql` (the same way `parse` does) and translates its first failure.
    fn failure(sql: &str) -> PgError {
        let src = SourceMap::new(sql);
        let dialect = PostgreSqlDialect {};
        let tokens = match Tokenizer::new(&dialect, sql).tokenize_with_location() {
            Ok(t) => t,
            Err(e) => return unwrap_translated(translate_tokenizer_error(&e.message, e.location, &src)),
        };
        let err = match Parser::new(&dialect).with_tokens_with_locations(tokens.clone()).parse_statements() {
            Ok(_) => panic!("expected a syntax error for {sql:?}"),
            Err(e) => e,
        };
        unwrap_translated(translate_parser_error(&err, &src, &tokens))
    }

    fn assert_pg(e: &PgError, message: &str, position: Option<u32>) {
        assert_eq!(e.sqlstate, SqlState::SYNTAX_ERROR, "sqlstate for {message:?}");
        assert_eq!(e.message, message);
        assert_eq!(e.position, position);
    }

    #[test]
    fn oracle_unexpected_token() {
        assert_pg(&failure("select 1 2"), "syntax error at or near \"2\"", Some(10));
    }

    #[test]
    fn oracle_unknown_keyword_at_start() {
        assert_pg(&failure("selec 1"), "syntax error at or near \"selec\"", Some(1));
    }

    #[test]
    fn oracle_end_of_input() {
        assert_pg(&failure("select ("), "syntax error at end of input", Some(9));
        assert_pg(&failure("select 1 from"), "syntax error at end of input", Some(14));
    }

    #[test]
    fn oracle_unterminated_string() {
        assert_pg(&failure("select 'abc"), "unterminated quoted string at or near \"'abc\"", Some(8));
    }

    #[test]
    fn unterminated_string_position_counts_characters() {
        assert_pg(&failure("select 'é"), "unterminated quoted string at or near \"'é\"", Some(8));
    }

    #[test]
    fn multi_line_position_counts_characters_across_newlines() {
        assert_pg(&failure("select\n1 2"), "syntax error at or near \"2\"", Some(10));
    }

    #[test]
    fn unterminated_dollar_quote_points_at_opener() {
        assert_pg(&failure("select $$abc"), "unterminated dollar-quoted string at or near \"$$abc\"", Some(8));
    }

    #[test]
    fn unterminated_comment_points_at_opener() {
        assert_pg(&failure("select 1 /* abc"), "unterminated /* comment at or near \"/* abc\"", Some(10));
    }

    #[test]
    fn unterminated_quoted_identifier() {
        assert_pg(&failure("select \"abc"), "unterminated quoted identifier at or near \"\"abc\"", Some(8));
    }

    #[test]
    fn zero_length_identifier_token() {
        let sql = "select \"\"";
        let src = SourceMap::new(sql);
        let dialect = PostgreSqlDialect {};
        let tokens = Tokenizer::new(&dialect, sql).tokenize_with_location().expect("tokenizes");
        let err = ParserError::ParserError("Expected: an expression, found: \"\" at Line: 1, Column: 8".to_string());
        let e = unwrap_translated(translate_parser_error(&err, &src, &tokens));
        assert_pg(&e, "zero-length delimited identifier at or near \"\"\"\"", Some(8));
    }

    #[test]
    fn recursion_limit_is_stack_depth_error() {
        let src = SourceMap::new("select 1");
        let e = unwrap_translated(translate_parser_error(&ParserError::RecursionLimitExceeded, &src, &[]));
        assert_eq!(e.sqlstate, SqlState::STATEMENT_TOO_COMPLEX);
        assert_eq!(e.message, "stack depth limit exceeded");
        assert_eq!(
            e.hint.as_deref(),
            Some(
                "Increase the configuration parameter \"max_stack_depth\" (currently 2048kB), after ensuring the platform's stack depth limit is adequate."
            )
        );
    }

    #[test]
    fn unknown_message_without_location_is_plain_syntax_error() {
        let src = SourceMap::new("select 1");
        let err = ParserError::ParserError("Unsupported thing".to_string());
        let e = unwrap_translated(translate_parser_error(&err, &src, &[]));
        assert_eq!(e.sqlstate, SqlState::SYNTAX_ERROR);
        assert_eq!(e.message, "syntax error");
        assert_eq!(e.position, None);
    }

    #[test]
    fn token_text_is_source_slice() {
        let src = SourceMap::new("select 'a''b' x");
        let tokens = Tokenizer::new(&PostgreSqlDialect {}, "select 'a''b' x").tokenize_with_location().expect("tokenizes");
        let lit = tokens.iter().find(|t| t.span.start.column == 8).expect("literal token");
        assert_eq!(token_source(&src, lit), "'a''b'");
    }
}
