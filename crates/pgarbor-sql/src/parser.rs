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
use sqlparser::ast::{Expr, Statement};
use sqlparser::dialect::PostgreSqlDialect;
use sqlparser::parser::Parser;
use sqlparser::tokenizer::{Span, Token, TokenWithSpan, Tokenizer};

use crate::classify;
use crate::diag;
use crate::extra_stmt::ExtraStatement;
use crate::parser_extra;
use crate::source::SourceMap;
use crate::syntax_error;

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
pub fn parse(sql: &str) -> Result<Vec<RawStatement>> {
    let src = Arc::new(SourceMap::new(sql));
    let dialect = PostgreSqlDialect {};
    // Step 2: tokenize the whole string once; locations are absolute in `sql`.
    let tokens = match Tokenizer::new(&dialect, sql).tokenize_with_location() {
        Ok(t) => t,
        Err(e) => {
            let err = syntax_error::translate_tokenizer_error(&e.message, e.location, &src)?;
            return Err(err);
        }
    };

    // Step 3 and 4, grammar phase for every chunk first (like pg_parse_query), analysis after.
    let mut staged: Vec<Staged> = Vec::new();
    for (start, end) in split_statements(&tokens) {
        let first = &tokens[start];
        let last = &tokens[end - 1];
        let (byte_start, first_end) = token_bytes(&src, &first.span)?;
        let (_, byte_end) = token_bytes(&src, &last.span)?;
        let pos = src.char_pos(first.span.start).ok_or_else(|| PgError::internal("statement without position"))?;

        // Chunk tokens plus a terminating EOF located at the end of the last token.
        let mut toks: Vec<TokenWithSpan> = tokens[start..end].to_vec();
        let eof_loc = last.span.end;
        toks.push(TokenWithSpan::new(Token::EOF, Span::new(eof_loc, eof_loc)));

        let parsed = if parser_extra::is_extra_statement(&toks)? {
            Parsed::Extra(parser_extra::parse_extra(toks, &src)?)
        } else {
            Parsed::Ast(Box::new(parse_ast_chunk(&toks, &src)?))
        };

        staged.push(Staged {
            parsed,
            text: src.slice(byte_start, byte_end).to_string(),
            byte_start,
            byte_end,
            pos,
            first_text: src.slice(byte_start, first_end).to_string(),
        });
    }

    // Step 4 (support check): unsupported or non-PostgreSQL statements fail at parse time.
    let mut out = Vec::with_capacity(staged.len());
    for (index, st) in staged.into_iter().enumerate() {
        classify::check_supported(&st.parsed, &st.first_text, Some(st.pos))?;
        out.push(RawStatement {
            parsed: st.parsed,
            text: st.text,
            byte_start: st.byte_start,
            byte_end: st.byte_end,
            pos: st.pos,
            index,
            source: Arc::clone(&src),
        });
    }
    Ok(out)
}

/// A parsed chunk before the support check (kept separate so every grammar error of the string
/// is reported before any "unsupported" error, as PostgreSQL's grammar phase does).
struct Staged {
    parsed: Parsed,
    text: String,
    byte_start: usize,
    byte_end: usize,
    pos: u32,
    first_text: String,
}

/// Byte range of a token span inside the query text (`SourceMap::byte_offset` of start and end).
fn token_bytes(src: &SourceMap, span: &Span) -> Result<(usize, usize)> {
    match (src.byte_offset(span.start), src.byte_offset(span.end)) {
        (Some(s), Some(e)) => Ok((s, e)),
        _ => Err(PgError::internal("token location outside the query text")),
    }
}

/// Uppercased value of an unquoted word token, `None` for every other token.
fn bare_word(t: &Token) -> Option<String> {
    match t {
        Token::Word(w) if w.quote_style.is_none() => Some(w.value.to_ascii_uppercase()),
        _ => None,
    }
}

/// Is this the head of `CREATE [OR REPLACE] FUNCTION|PROCEDURE` (the first significant words)?
fn is_function_header(head: &[&str]) -> bool {
    matches!(
        head,
        ["CREATE", "FUNCTION" | "PROCEDURE", ..] | ["CREATE", "OR", "REPLACE", "FUNCTION" | "PROCEDURE", ..]
    )
}

/// Split the token stream on top-level `;` (module docs, step 3). Returns the non-empty chunks as
/// half-open token index ranges, each trimmed to its first and last non-whitespace token. Inside a
/// `CREATE FUNCTION|PROCEDURE ... BEGIN ATOMIC ... END` body the semicolons do not split.
fn split_statements(tokens: &[TokenWithSpan]) -> Vec<(usize, usize)> {
    let mut out = Vec::new();
    let mut start = 0usize;
    let mut end = tokens.len();
    // Significant (non-whitespace) leading words of the current chunk, at most four.
    let mut head: Vec<String> = Vec::new();
    let mut prev_begin = false;
    let mut atomic_open = false;
    let mut case_depth = 0usize;
    for (i, t) in tokens.iter().enumerate() {
        match &t.token {
            Token::EOF => {
                end = i;
                break;
            }
            Token::Whitespace(_) => continue,
            Token::SemiColon if !atomic_open => {
                if let Some(r) = trim_range(tokens, start, i) {
                    out.push(r);
                }
                start = i + 1;
                head.clear();
                prev_begin = false;
                case_depth = 0;
                continue;
            }
            _ => {}
        }
        let word = bare_word(&t.token);
        let name: &str = word.as_deref().unwrap_or("");
        if head.len() < 4 {
            head.push(name.to_string());
        }
        if atomic_open {
            match name {
                "CASE" => case_depth += 1,
                "END" => {
                    if case_depth == 0 {
                        atomic_open = false;
                    } else {
                        case_depth -= 1;
                    }
                }
                _ => {}
            }
        } else if prev_begin && name == "ATOMIC" {
            let head_refs: Vec<&str> = head.iter().map(String::as_str).collect();
            if is_function_header(&head_refs) {
                atomic_open = true;
                case_depth = 0;
            }
        }
        prev_begin = name == "BEGIN";
    }
    if let Some(r) = trim_range(tokens, start, end) {
        out.push(r);
    }
    out
}

/// Half-open range `[first, last + 1)` of the non-whitespace tokens in `tokens[start..end]`.
fn trim_range(tokens: &[TokenWithSpan], start: usize, end: usize) -> Option<(usize, usize)> {
    let mut first = None;
    let mut last = None;
    for (i, t) in tokens.iter().enumerate().take(end).skip(start) {
        if !matches!(t.token, Token::Whitespace(_)) {
            if first.is_none() {
                first = Some(i);
            }
            last = Some(i);
        }
    }
    match (first, last) {
        (Some(f), Some(l)) => Some((f, l + 1)),
        _ => None,
    }
}

/// Parse one chunk (tokens end with EOF) with sqlparser; the whole chunk must be one statement.
fn parse_ast_chunk(toks: &[TokenWithSpan], src: &SourceMap) -> Result<Statement> {
    let dialect = PostgreSqlDialect {};
    let mut parser = Parser::new(&dialect)
        .with_recursion_limit(RECURSION_LIMIT)
        .with_tokens_with_locations(toks.to_vec());
    let stmt = match parser.parse_statement() {
        Ok(s) => s,
        Err(e) => {
            let err = syntax_error::translate_parser_error(&e, src, toks)?;
            return Err(err);
        }
    };
    let next = parser.peek_token();
    if !matches!(next.token, Token::EOF) {
        let (s, e) = token_bytes(src, &next.span)?;
        let near = src.slice(s, e).to_string();
        return Err(diag::syntax_error_near(&near, src.char_pos(next.span.start)));
    }
    Ok(stmt)
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
pub fn parse_expr_text(sql: &str) -> Result<Expr> {
    let dialect = PostgreSqlDialect {};
    let fail = || PgError::internal(format!("could not parse stored expression \"{sql}\""));
    let mut parser = Parser::new(&dialect).try_with_sql(sql).map_err(|_| fail())?;
    let expr = parser.parse_expr().map_err(|_| fail())?;
    if !matches!(parser.peek_token().token, Token::EOF) {
        return Err(fail());
    }
    Ok(expr)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Tokenize and split without running the extra-statement or AST parsers, so these tests do
    /// not depend on `parser_extra` (SQL-02) or the syntax-error translation (SQL-03).
    fn chunk_texts(sql: &str) -> Vec<(String, u32)> {
        let src = SourceMap::new(sql);
        let dialect = PostgreSqlDialect {};
        let tokens = Tokenizer::new(&dialect, sql).tokenize_with_location().expect("tokenizes");
        split_statements(&tokens)
            .into_iter()
            .map(|(s, e)| {
                let (b0, _) = token_bytes(&src, &tokens[s].span).expect("start");
                let (_, b1) = token_bytes(&src, &tokens[e - 1].span).expect("end");
                let pos = src.char_pos(tokens[s].span.start).expect("pos");
                (src.slice(b0, b1).to_string(), pos)
            })
            .collect()
    }

    #[test]
    fn splits_on_semicolons_with_positions() {
        assert_eq!(
            chunk_texts("select 1; select 2"),
            vec![("select 1".to_string(), 1), ("select 2".to_string(), 11)]
        );
    }

    #[test]
    fn semicolons_inside_literals_do_not_split() {
        assert_eq!(chunk_texts("select ';'; select 2").len(), 2);
        assert_eq!(chunk_texts("select $$a;b$$").len(), 1);
        assert_eq!(chunk_texts("select 'x;y' /* ; */ ").len(), 1);
    }

    #[test]
    fn empty_statements_are_dropped() {
        assert!(chunk_texts("").is_empty());
        assert!(chunk_texts(";;").is_empty());
        assert!(chunk_texts("-- c").is_empty());
        assert!(chunk_texts("  ;  -- c\n ; ").is_empty());
    }

    #[test]
    fn position_counts_across_newlines() {
        let v = chunk_texts("select 1;\nselect from from");
        assert_eq!(v.len(), 2);
        assert_eq!(v[1], ("select from from".to_string(), 11));
    }

    #[test]
    fn begin_atomic_body_is_one_statement() {
        let v = chunk_texts("create function f() returns int language sql begin atomic select 1; select 2; end");
        assert_eq!(v.len(), 1);
        assert!(v[0].0.ends_with("select 2; end"));
        let v = chunk_texts(
            "create or replace function f() returns int language sql begin atomic select case when true then 1 end; end; select 3",
        );
        assert_eq!(v.len(), 2);
        assert_eq!(v[1].0, "select 3");
    }

    #[test]
    fn quoted_begin_atomic_does_not_join() {
        // Not a function header: the semicolon still splits.
        assert_eq!(chunk_texts("select 'begin atomic'; select 2").len(), 2);
    }

    #[test]
    #[ignore = "needs SQL-02"]
    fn parse_two_statements() {
        let v = parse("select 1; select 2").expect("parses");
        assert_eq!(v.len(), 2);
        assert_eq!(v[0].text, "select 1");
        assert_eq!(v[1].text, "select 2");
        assert_eq!(v[0].pos, 1);
        assert_eq!(v[1].pos, 11);
        assert_eq!(v[1].index, 1);
    }

    #[test]
    #[ignore = "needs SQL-02"]
    fn parse_empty_strings_yield_nothing() {
        assert!(parse("").expect("parses").is_empty());
        assert!(parse(";;").expect("parses").is_empty());
        assert!(parse("-- c").expect("parses").is_empty());
    }

    #[test]
    #[ignore = "needs SQL-02"]
    fn parse_literal_semicolons() {
        assert_eq!(parse("select ';'; select 2").expect("parses").len(), 2);
        assert_eq!(parse("select $$a;b$$").expect("parses").len(), 1);
    }

    #[test]
    #[ignore = "needs SQL-02 and SQL-03"]
    fn parse_syntax_error_fails_whole_string() {
        let err = parse("select 1; select 2 3").expect_err("syntax error");
        assert_eq!(err.sqlstate.code(), "42601");
        assert_eq!(err.message, "syntax error at or near \"3\"");
        assert_eq!(err.position, Some(20));
    }

    #[test]
    #[ignore = "needs SQL-02 and SQL-03"]
    fn parse_select_1_2_is_42601_at_10() {
        let err = parse("select 1 2").expect_err("syntax error");
        assert_eq!(err.sqlstate.code(), "42601");
        assert_eq!(err.message, "syntax error at or near \"2\"");
        assert_eq!(err.position, Some(10));
    }

    #[test]
    #[ignore = "needs SQL-02"]
    fn parse_begin_atomic_is_one_statement() {
        let v = parse("create function f() returns int language sql begin atomic select 1; select 2; end")
            .expect("parses");
        assert_eq!(v.len(), 1);
    }

    #[test]
    #[ignore = "needs SQL-02"]
    fn parse_refresh_is_extra() {
        let v = parse("refresh materialized view v").expect("parses");
        assert!(v[0].as_extra().is_some());
    }

    #[test]
    #[ignore = "needs SQL-02"]
    fn parse_create_trigger_is_0a000() {
        let err = parse("create trigger t before insert on x execute function f()").expect_err("unsupported");
        assert_eq!(err.sqlstate.code(), "0A000");
    }

    #[test]
    #[ignore = "needs SQL-02"]
    fn parse_one_requires_exactly_one() {
        assert!(parse_one("select 1; select 2").is_err());
        assert!(parse_one("select 1").is_ok());
    }

    #[test]
    fn expr_text_round_trips() {
        for s in [
            "nextval('s'::regclass)",
            "'x'::character varying",
            "(a > 0)",
            "ARRAY[1, 2]",
            "(a)::text",
        ] {
            assert!(parse_expr_text(s).is_ok(), "failed: {s}");
        }
        assert!(parse_expr_text("a > 0 extra junk )").is_err());
    }
}
