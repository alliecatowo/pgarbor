//! SQL text -> statements: the `sqlparser` 0.63 adapter (SQL-01). Entry point [`parse`].
//!
//! ## Contract (ARCHITECTURE §10)
//! `parse(sql)` takes the text of ONE simple-Query message (or one Parse message) and returns its
//! statements in order. It runs the PostgreSQL grammar phase for the WHOLE string before anything is
//! executed, like PG's `pg_parse_query`: a syntax error in statement 3 fails the call and statements
//! 1 and 2 are never run. Semantic errors (unknown table, type mismatch) are NOT detected here;
//! they come from the binder when each statement is executed.
//!
//! ## Algorithm (follow exactly)
//! 1. Build `SourceMap::new(sql)` and wrap it in an `Arc`.
//! 2. Tokenize the whole string once: `Tokenizer::new(&PostgreSqlDialect {}, sql)
//!    .tokenize_with_location()`. A `TokenizerError { message, location }` is translated by
//!    `syntax_error::translate_tokenizer_error` and returned (e.g. `unterminated quoted string`).
//!    Token locations are therefore absolute positions in the full string, so every later error
//!    position is already PG-style (`SourceMap::char_pos`).
//! 3. Split the token vector on top-level `Token::SemiColon`. Splitting on tokens (not on text)
//!    makes `;` inside string literals, dollar-quoted bodies (`$$ ... ; ... $$`), quoted identifiers
//!    and comments irrelevant. Comments/whitespace tokens (`Token::Whitespace`) are kept inside a
//!    chunk (they are skipped by the parser) but a chunk containing only whitespace/comments is an
//!    EMPTY statement and is dropped (PG: `select 1;;select 2` runs two statements; the empty string
//!    or `;` or `-- comment` parses to ZERO statements -> the caller sends EmptyQueryResponse).
//!    BEGIN ATOMIC bodies of `CREATE FUNCTION` contain `;` before the final `END`: when the chunk
//!    starts with `CREATE [OR REPLACE] FUNCTION|PROCEDURE` and contains the keyword sequence
//!    `BEGIN ATOMIC`, keep joining chunks until the matching `END` token has been consumed.
//! 4. For each chunk: record the byte range of its first to last non-whitespace token (use
//!    `SourceMap::byte_offset(span.start)` / `span.end`), `text = slice`, `pos = char_pos(first.start)`.
//!    Append a `Token::EOF` token (location = end of the chunk's last token) and run:
//!    * if `parser_extra::is_extra_statement(&tokens)?` -> `Parsed::Extra(parser_extra::parse_extra(..)?)`;
//!    * else `Parser::new(&PostgreSqlDialect {}).with_recursion_limit(RECURSION_LIMIT)
//!      .with_tokens_with_locations(tokens).parse_statement()`. After it returns `Ok`, the next
//!      token must be `EOF`; otherwise raise a syntax error AT that token (`select 1 2`).
//!    * a `ParserError` is translated by `syntax_error::translate_parser_error(err, &src, &tokens)`.
//!    * `Parsed::Ast(Box<Statement>)` on success. Then run `classify::check_supported(stmt, pos)`:
//!      `NotPostgres` -> 42601 at the first token, `Unsupported(feature)` -> 0A000 (so unsupported
//!      statements fail at parse time exactly like PG's grammar rejects foreign syntax; the PG
//!      "recognized but unsupported by PGArbor" 0A000 variants are raised here too, positioned at
//!      the statement start).
//! 5. PostgreSQL keywords as bare identifiers: `PostgreSqlDialect` already allows non-reserved
//!    keywords as identifiers; verify with the tests below, do not special-case.
//!
//! ## Literal and identifier notes the binder relies on
//! * `$1` tokenizes as `Token::Placeholder("$1")`; the AST holds `Value::Placeholder`.
//! * `E'..'` strings arrive as `Value::EscapedStringLiteral` with escapes UNprocessed (the binder's
//!   `literal` module decodes them), `U&'..'` as `UnicodeStringLiteral`, `$tag$..$tag$` as
//!   `DollarQuotedString`.
//! * Identifier case is preserved by sqlparser; the binder folds with `names::fold_ident`.
//!
//! ## Tests required (in `#[cfg(test)]`)
//! `select 1; select 2` -> 2 statements with correct `text` and `pos` (second pos = 11);
//! `select ';'; select 2` -> 2; `select $$a;b$$` -> 1; empty / `;;` / `-- x` -> 0; `select 1 2` ->
//! 42601 `syntax error at or near "2"` position 10; `select 1;\nselect from from` -> position counted
//! across the newline; `refresh materialized view v` -> `Parsed::Extra`; `show tables` handled as
//! `Parsed::Ast(ShowVariable)` or rejected per `classify` (document which); `create function f() ...
//! begin atomic select 1; select 2; end` -> 1 statement.
use std::sync::Arc;

use pgarbor_common::{PgError, Result};
use sqlparser::ast::Statement;

use crate::extra_stmt::ExtraStatement;
use crate::source::SourceMap;

/// Maximum expression/subquery nesting accepted by the parser (sqlparser recursion counter). PG's
/// limit is `max_stack_depth` (54001 `stack depth limit exceeded`); sqlparser reports
/// `RecursionLimitExceeded`, which `syntax_error` maps to 54001.
pub const RECURSION_LIMIT: usize = 400;

/// A parsed statement: a `sqlparser` AST or one of the hand-parsed extras.
// CONTRACT
#[derive(Clone, Debug, PartialEq)]
pub enum Parsed {
    Ast(Box<Statement>),
    Extra(ExtraStatement),
}

/// One statement of a submitted query string.
// CONTRACT
#[derive(Clone, Debug)]
pub struct RawStatement {
    pub parsed: Parsed,
    /// Exact source text of the statement: from its first to its last non-whitespace token, without
    /// the terminating `;` (what `pg_stat_activity.query` / `pg_prepared_statements.statement` show
    /// for a multi-statement string is the WHOLE string; callers wanting that use `source.text()`).
    pub text: String,
    /// Byte range of `text` inside `source.text()`.
    pub byte_start: usize,
    pub byte_end: usize,
    /// 1-based character position of the statement start in the whole query string.
    pub pos: u32,
    /// 0-based ordinal among the non-empty statements.
    pub index: usize,
    /// The whole submitted string; shared by all statements of one `parse` call. The binder takes
    /// positions from it so errors in statement N count characters of statements 1..N-1.
    pub source: Arc<SourceMap>,
}

impl RawStatement {
    /// The sqlparser statement, if this is not an extra.
    pub fn as_ast(&self) -> Option<&Statement> {
        match &self.parsed {
            Parsed::Ast(s) => Some(s),
            Parsed::Extra(_) => None,
        }
    }
    /// The extra statement, if this is one.
    pub fn as_extra(&self) -> Option<&ExtraStatement> {
        match &self.parsed {
            Parsed::Extra(e) => Some(e),
            Parsed::Ast(_) => None,
        }
    }
}

/// Parse a query string into statements (module docs). Zero statements = empty query.
///
/// Errors: 42601 syntax errors / unterminated literals (see `syntax_error`), 0A000 for recognized
/// but unsupported statements (`classify`), 54001 nesting too deep.
#[allow(unused_variables)]
pub fn parse(sql: &str) -> Result<Vec<RawStatement>> {
    Err(PgError::unimplemented("SQL-01", "SQL parsing (parser::parse)"))
}

/// Parse text that must contain exactly ONE statement (used for re-binding stored view
/// definitions, PREPARE inner text, EXECUTE of cached text): any other count is an internal error
/// (XX000 `expected one statement, found N`).
pub fn parse_one(sql: &str) -> Result<RawStatement> {
    let mut v = parse(sql)?;
    if v.len() != 1 {
        return Err(PgError::internal(format!("expected one statement, found {}", v.len())));
    }
    Ok(v.remove(0))
}

/// Parse a stand-alone expression (catalog `StoredExpr::sql` of column defaults, CHECK
/// constraints, index expressions): internally `parse_one("SELECT <expr>")` and extraction of the
/// single target expression, with positions meaningless (`None`) because the text is not user input.
///
/// Algorithm: `Parser::new(&PostgreSqlDialect{}).try_with_sql(sql)?.parse_expr()`, then require
/// `EOF`; errors -> `XX000 could not parse stored expression "<sql>"`.
#[allow(unused_variables)]
pub fn parse_expr_text(sql: &str) -> Result<sqlparser::ast::Expr> {
    Err(PgError::unimplemented("SQL-01", "stored expression parsing (parser::parse_expr_text)"))
}
