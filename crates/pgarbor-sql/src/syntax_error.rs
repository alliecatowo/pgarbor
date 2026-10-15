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
use pgarbor_common::{PgError, Result};
use sqlparser::parser::ParserError;
use sqlparser::tokenizer::{Location, TokenWithSpan};

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
#[allow(unused_variables)]
pub fn translate_parser_error(err: &ParserError, src: &SourceMap, tokens: &[TokenWithSpan]) -> Result<PgError> {
    Err(PgError::unimplemented("SQL-03", "syntax error translation (syntax_error::translate_parser_error)"))
}

/// Translate a tokenizer error (module docs). `Ok(error)`: the translated PG error to return to the
/// client; the `Result` wrapper only exists so the body can report an unimplemented placeholder.
#[allow(unused_variables)]
pub fn translate_tokenizer_error(message: &str, location: Location, src: &SourceMap) -> Result<PgError> {
    Err(PgError::unimplemented("SQL-03", "tokenizer error translation (syntax_error::translate_tokenizer_error)"))
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
}
