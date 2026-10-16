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
use sqlparser::ast::{AlterTableOperation, ColumnOption, Expr, GeneratedAs, GeneratedExpressionMode, Statement};
use sqlparser::dialect::PostgreSqlDialect;
use sqlparser::parser::Parser;
use sqlparser::keywords::Keyword;
use sqlparser::tokenizer::{Span, Token, TokenWithSpan, Tokenizer, Word};

use crate::classify;
use crate::diag;
use crate::extra_stmt::ExtraStatement;
use crate::fk_set_opts::{self, FkSetGroup};
use crate::identity_opts::{self, IdentityGroup};
use crate::parser_extra;
use crate::source::SourceMap;
use crate::strict_check;
use crate::syntax_error;
use crate::view_trailer_opts::{self, ViewTrailer};

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
    /// Identity option lists of this statement, one per `AS IDENTITY` in source order (see
    /// `identity_opts`). The binder consumes them in the same order.
    pub identity_groups: Vec<IdentityGroup>,
    /// `ON DELETE SET NULL|SET DEFAULT` occurrences of this statement, in source order (see `fk_set_opts`).
    /// The binder consumes the ones that belong to the foreign keys it binds.
    pub fk_set_groups: Vec<FkSetGroup>,
    /// Trailing `WITH [NO] DATA` / `WITH [CASCADED|LOCAL] CHECK OPTION` of a `CREATE [MATERIALIZED] VIEW`
    /// (see `view_trailer_opts`); `None` for every other statement.
    pub view_trailer: Option<ViewTrailer>,
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
    /// Completion tags for syntax distinctions intentionally shared by bound session statements.
    pub fn session_completion_tag(&self) -> Option<&'static str> {
        match self.as_ast()? {
            Statement::StartTransaction { begin: false, .. } => Some("START TRANSACTION"),
            Statement::Reset(_) => Some("RESET"),
            _ => None,
        }
    }
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
        let toks = merge_operator_names(toks, &src)?;
        let toks = spell_operator_runs(toks, &src)?;
        let toks = join_composite_compare_ops(toks);

        let mut identity_groups = Vec::new();
        let mut fk_set_groups = Vec::new();
        let mut view_trailer = None;
        if let Some(at) = create_schema_element_at(&toks) {
            // Parsed and located, but not executed: schema elements need the binder to create objects in
            // the new schema (PG `transformCreateSchemaStmtElements`). Reject the whole statement, so nothing
            // is created (PG runs the elements in the same statement, atomically).
            // gram.y: `CREATE SCHEMA IF NOT EXISTS ... <elements>` is 0A000 with its own message, not a gap.
            let header: Vec<String> = toks[..at].iter().filter_map(|t| bare_word(&t.token)).collect();
            let if_not_exists = header.windows(3).any(|w| w[0] == "IF" && w[1] == "NOT" && w[2] == "EXISTS");
            let err = if if_not_exists {
                PgError::new(
                    pgarbor_common::SqlState::FEATURE_NOT_SUPPORTED,
                    "CREATE SCHEMA IF NOT EXISTS cannot include schema elements",
                )
            } else {
                PgError::unimplemented("SQL-35", "CREATE SCHEMA with schema elements")
            };
            return Err(match src.char_pos(toks[at].span.start) {
                Some(pos) => err.with_position(pos),
                None => err,
            });
        }
        let parsed = if parser_extra::is_extra_statement(&toks)? {
            Parsed::Extra(parser_extra::parse_extra(toks, &src)?)
        } else {
            strict_check::check(&toks, &src)?;
            let (toks, trailer) = view_trailer_opts::extract(&toks);
            view_trailer = trailer;
            let (ast_toks, groups) = identity_opts::extract(&toks, &src)?;
            identity_groups = groups;
            let (ast_toks, set_groups) = fk_set_opts::extract(&ast_toks);
            fk_set_groups = set_groups;
            Parsed::Ast(Box::new(parse_ast_chunk(&ast_toks, &src)?))
        };

        staged.push(Staged {
            parsed,
            identity_groups,
            fk_set_groups,
            view_trailer,
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
            identity_groups: st.identity_groups,
            fk_set_groups: st.fk_set_groups,
            view_trailer: st.view_trailer,
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
    identity_groups: Vec<IdentityGroup>,
    fk_set_groups: Vec<FkSetGroup>,
    view_trailer: Option<ViewTrailer>,
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

/// Name of the marker argument that stands for a `VARIADIC` call argument (PG `func_arg_expr`
/// after `VARIADIC`). sqlparser 0.63 has no `VARIADIC expr` argument form, so [`mark_variadic_args`]
/// rewrites the keyword into a synthetic named argument: a synthetic identifier containing a NUL,
/// which PostgreSQL identifiers cannot contain. The binder recognizes it and clears it
/// (`expr_func::variadic_call`).
pub const VARIADIC_MARKER: &str = "\0pgarbor variadic";

/// Name of the marker that stands for an unnamed `CREATE PROCEDURE` parameter (PG accepts
/// `CREATE PROCEDURE p(integer)`, while sqlparser's `ProcedureParam` always has a name). Inserted by
/// [`unnamed_procedure_params`] with the same technique as [`VARIADIC_MARKER`]; the binder maps it
/// back to an empty parameter name (`ddl_function::create_procedure`).
pub const UNNAMED_PARAM_MARKER: &str = "\0pgarbor unnamed";

/// `COLLATION FOR (expr)` (PG gram.y `COLLATION FOR '(' a_expr ')'`) is parsed as the call `pg_collation_for(expr)`:
/// sqlparser has no such form. The two keywords become one function name here, and the binder recognizes the
/// name (`expr_func::bind_special_function`).
fn collation_for_as_function(toks: &[TokenWithSpan]) -> Vec<TokenWithSpan> {
    let sig: Vec<usize> = (0..toks.len()).filter(|&i| !matches!(toks[i].token, Token::Whitespace(_))).collect();
    let mut out = toks.to_vec();
    let mut drop = vec![false; toks.len()];
    for k in 0..sig.len().saturating_sub(2) {
        let (a, b, c) = (sig[k], sig[k + 1], sig[k + 2]);
        if bare_word(&toks[a].token).as_deref() == Some("COLLATION")
            && bare_word(&toks[b].token).as_deref() == Some("FOR")
            && matches!(toks[c].token, Token::LParen)
        {
            out[a] = TokenWithSpan::new(Token::make_word("pg_collation_for", None), toks[a].span);
            for d in drop.iter_mut().take(b + 1).skip(a + 1) {
                *d = true;
            }
        }
    }
    out.into_iter().zip(drop).filter(|(_, d)| !d).map(|(t, _)| t).collect()
}

/// `ALTER PROCEDURE` and `ALTER ROUTINE` are parsed as `ALTER FUNCTION` (sqlparser has no procedure or
/// routine form): the keyword is rewritten here, and the binder recognizes the form from the statement
/// text ([`is_alter_procedure_text`], [`is_alter_routine_text`]).
fn alter_procedure_as_function(toks: &[TokenWithSpan]) -> Vec<TokenWithSpan> {
    let mut out = toks.to_vec();
    let sig: Vec<usize> = (0..toks.len()).filter(|&i| !matches!(toks[i].token, Token::Whitespace(_))).collect();
    if let [a, b, ..] = sig.as_slice() {
        let object = bare_word(&toks[*b].token);
        if bare_word(&toks[*a].token).as_deref() == Some("ALTER") && matches!(object.as_deref(), Some("PROCEDURE" | "ROUTINE")) {
            out[*b] = TokenWithSpan::new(Token::make_keyword("FUNCTION"), toks[*b].span);
        }
    }
    out
}

/// `CREATE [OR REPLACE] PROCEDURE` is parsed as `CREATE [OR REPLACE] FUNCTION`. sqlparser's own
/// `CREATE PROCEDURE` is the T-SQL form (a statement-block body); PostgreSQL's procedure takes the function
/// grammar (`AS 'text'`, `AS $$..$$`, `LANGUAGE`, `SECURITY`). The binder recognizes the form from the
/// statement text ([`is_create_procedure_text`]) and creates a procedure.
fn create_procedure_as_function(toks: &[TokenWithSpan]) -> Vec<TokenWithSpan> {
    let mut out = toks.to_vec();
    let words: Vec<(usize, String)> = (0..toks.len()).filter_map(|i| bare_word(&toks[i].token).map(|w| (i, w))).take(4).collect();
    let names: Vec<&str> = words.iter().map(|(_, w)| w.as_str()).collect();
    let at = match names.as_slice() {
        ["CREATE", "PROCEDURE", ..] => words[1].0,
        ["CREATE", "OR", "REPLACE", "PROCEDURE", ..] => words[3].0,
        _ => return out,
    };
    out[at] = TokenWithSpan::new(Token::make_keyword("FUNCTION"), toks[at].span);
    out
}

/// Does this statement text start with `CREATE [OR REPLACE] PROCEDURE` (the first unquoted words)? The binder
/// uses it, because the AST is `CREATE FUNCTION` for both forms (see [`create_procedure_as_function`]).
pub fn is_create_procedure_text(text: &str) -> bool {
    let dialect = PostgreSqlDialect {};
    let Ok(toks) = Tokenizer::new(&dialect, text).tokenize() else {
        return false;
    };
    let words: Vec<String> = toks
        .iter()
        .filter_map(|t| match t {
            Token::Word(w) if w.quote_style.is_none() => Some(w.value.to_ascii_uppercase()),
            _ => None,
        })
        .take(4)
        .collect();
    let names: Vec<&str> = words.iter().map(String::as_str).collect();
    matches!(names.as_slice(), ["CREATE", "PROCEDURE", ..] | ["CREATE", "OR", "REPLACE", "PROCEDURE", ..])
}

/// `LANGUAGE 'name'` (PG `NonReservedWord_or_Sconst`) in `CREATE [OR REPLACE] FUNCTION|PROCEDURE` becomes a
/// double-quoted name, so the language is matched exactly as written (`'SQL'` is not `sql`). sqlparser reads
/// the `LANGUAGE` item as an identifier only.
fn quoted_language_as_identifier(toks: &[TokenWithSpan]) -> Vec<TokenWithSpan> {
    let head: Vec<String> = toks.iter().filter_map(|t| bare_word(&t.token)).take(4).collect();
    let head_refs: Vec<&str> = head.iter().map(String::as_str).collect();
    if !is_function_header(&head_refs) {
        return toks.to_vec();
    }
    let mut out = Vec::with_capacity(toks.len());
    let mut after_language = false;
    for t in toks {
        match &t.token {
            Token::Whitespace(_) => {
                out.push(t.clone());
                continue;
            }
            Token::SingleQuotedString(name) if after_language => {
                let word = Word { value: name.clone(), quote_style: Some('"'), keyword: Keyword::NoKeyword };
                out.push(TokenWithSpan::new(Token::Word(word), t.span));
            }
            _ => out.push(t.clone()),
        }
        after_language = bare_word(&t.token).as_deref() == Some("LANGUAGE");
    }
    out
}

/// Index of the `BEGIN` of a `BEGIN ATOMIC` body of `CREATE [OR REPLACE] FUNCTION|PROCEDURE` (the SQL-standard
/// body). sqlparser 0.63 has no such body, so the caller reports it as 0A000 (SQL-37), not a syntax error.
fn begin_atomic_body(toks: &[TokenWithSpan]) -> Option<usize> {
    let sig: Vec<usize> = (0..toks.len()).filter(|&i| !matches!(toks[i].token, Token::Whitespace(_))).collect();
    let head: Vec<String> = sig.iter().filter_map(|&i| bare_word(&toks[i].token)).take(4).collect();
    let head_refs: Vec<&str> = head.iter().map(String::as_str).collect();
    if !is_function_header(&head_refs) {
        return None;
    }
    sig.windows(2)
        .find(|w| bare_word(&toks[w[0]].token).as_deref() == Some("BEGIN") && bare_word(&toks[w[1]].token).as_deref() == Some("ATOMIC"))
        .map(|w| w[0])
}

/// `DROP ROUTINE` is parsed as `DROP FUNCTION` (sqlparser has no ROUTINE form for DROP): the keyword is
/// rewritten here, and the binder recognizes the form from the statement text ([`is_drop_routine_text`]).
fn drop_routine_as_function(toks: &[TokenWithSpan]) -> Vec<TokenWithSpan> {
    let mut out = toks.to_vec();
    let sig: Vec<usize> = (0..toks.len()).filter(|&i| !matches!(toks[i].token, Token::Whitespace(_))).collect();
    if let [a, b, ..] = sig.as_slice() {
        if bare_word(&toks[*a].token).as_deref() == Some("DROP") && bare_word(&toks[*b].token).as_deref() == Some("ROUTINE") {
            out[*b] = TokenWithSpan::new(Token::make_keyword("FUNCTION"), toks[*b].span);
        }
    }
    out
}

/// Joins the operator name of `CREATE|ALTER|DROP OPERATOR` into one word token.
///
/// PostgreSQL's lexer reads a run of operator characters as ONE operator (`===`, `====`), but
/// sqlparser splits such a run into several tokens (`==` then `=`), and its operator-name parser
/// takes exactly one token. In these statements, a run of directly adjacent operator tokens that
/// follows `OPERATOR`, `IF EXISTS`, or `COMMUTATOR =` / `NEGATOR =` (PG `any_operator`, `all_Op`)
/// becomes one word token whose text is the operator name. Any other statement is returned unchanged,
/// so an expression such as `x = -1` is never touched.
fn merge_operator_names(toks: Vec<TokenWithSpan>, src: &SourceMap) -> Result<Vec<TokenWithSpan>> {
    let mut sig = toks.iter().filter(|t| !matches!(t.token, Token::Whitespace(_)));
    let is_operator_ddl = matches!(sig.next().and_then(|t| bare_word(&t.token)).as_deref(), Some("CREATE" | "ALTER" | "DROP"))
        && matches!(sig.next().and_then(|t| bare_word(&t.token)).as_deref(), Some("OPERATOR"));
    if !is_operator_ddl {
        return Ok(toks);
    }
    let mut out: Vec<TokenWithSpan> = Vec::with_capacity(toks.len());
    let mut i = 0;
    while i < toks.len() {
        if operator_symbol(&toks[i], src) && operator_name_follows(&out) {
            let mut end = i + 1;
            while end < toks.len() && operator_symbol(&toks[end], src) && toks[end].span.start == toks[end - 1].span.end {
                end += 1;
            }
            let (start_byte, _) = token_bytes(src, &toks[i].span)?;
            let (_, end_byte) = token_bytes(src, &toks[end - 1].span)?;
            let name = src.slice(start_byte, end_byte);
            let span = Span::new(toks[i].span.start, toks[end - 1].span.end);
            out.push(TokenWithSpan::new(Token::make_word(name, None), span));
            i = end;
        } else {
            out.push(toks[i].clone());
            i += 1;
        }
    }
    Ok(out)
}

/// Is this token made only of operator characters (`=`, `==`, `<>`, `!`, `->`, ...)?
fn operator_symbol(t: &TokenWithSpan, src: &SourceMap) -> bool {
    match token_bytes(src, &t.span) {
        Ok((s, e)) => {
            let text = src.slice(s, e);
            !text.is_empty() && text.chars().all(|c| "+-*/<>=~!@#%^&|`?".contains(c))
        }
        Err(_) => false,
    }
}

/// Does an operator name start at the next token, given the tokens already emitted? True after
/// `OPERATOR`, after `EXISTS` (`DROP OPERATOR IF EXISTS`), and after `=` in `COMMUTATOR = ` or
/// `NEGATOR = `.
fn operator_name_follows(out: &[TokenWithSpan]) -> bool {
    let mut sig = out.iter().rev().filter(|t| !matches!(t.token, Token::Whitespace(_)));
    match sig.next() {
        Some(last) if matches!(last.token, Token::Eq) => {
            matches!(sig.next().and_then(|t| bare_word(&t.token)).as_deref(), Some("COMMUTATOR" | "NEGATOR"))
        }
        Some(last) => matches!(bare_word(&last.token).as_deref(), Some("OPERATOR" | "EXISTS")),
        None => false,
    }
}

/// Spells the operator runs that PostgreSQL's lexer reads as one operator (`===`, `!==`, `=!`).
///
/// sqlparser's tokenizer special-cases `=` and `!`, so a run that starts with them splits into
/// several tokens (`==` then `=`, `!=` then `=`). Outside operator DDL (see [`merge_operator_names`]):
/// * a split run after an operand (`flag === true`, `x !== y`) becomes one `Token::CustomBinaryOperator`,
///   which the binder resolves by name;
/// * a split run inside `OPERATOR( ... )` (`OPERATOR(schema.===)`) becomes one word, like a name part.
///
/// Runs that sqlparser already reads as one token, runs with `+`, `-`, `*` or `/` (PostgreSQL trims a
/// trailing `+`/`-` from a multi-character operator, so `x=-1` keeps its meaning), and runs that do not
/// follow an operand are left unchanged, so their errors stay as before.
fn spell_operator_runs(toks: Vec<TokenWithSpan>, src: &SourceMap) -> Result<Vec<TokenWithSpan>> {
    let mut out: Vec<TokenWithSpan> = Vec::with_capacity(toks.len());
    let mut in_operator_paren = false;
    let mut i = 0;
    while i < toks.len() {
        if let Some((end, name)) = split_operator_run(&toks, i, src)? {
            let token = if in_operator_paren {
                Some(Token::make_word(&name, None))
            } else if follows_operand(&out) {
                Some(Token::CustomBinaryOperator(name))
            } else {
                None
            };
            if let Some(token) = token {
                out.push(TokenWithSpan::new(token, Span::new(toks[i].span.start, toks[end - 1].span.end)));
                i = end;
                continue;
            }
        }
        match &toks[i].token {
            Token::LParen if last_significant(&out).and_then(bare_word).as_deref() == Some("OPERATOR") => {
                in_operator_paren = true;
            }
            Token::RParen => in_operator_paren = false,
            _ => {}
        }
        out.push(toks[i].clone());
        i += 1;
    }
    Ok(out)
}

/// The run of adjacent operator tokens that starts at `toks[i]`, as its end index and its text. `None`
/// unless the run is at least two tokens, starts with `=` or `!`, and holds only characters that PostgreSQL
/// keeps in a multi-character operator.
fn split_operator_run(toks: &[TokenWithSpan], i: usize, src: &SourceMap) -> Result<Option<(usize, String)>> {
    if !operator_symbol(&toks[i], src) {
        return Ok(None);
    }
    let mut end = i + 1;
    while end < toks.len() && operator_symbol(&toks[end], src) && toks[end].span.start == toks[end - 1].span.end {
        end += 1;
    }
    if end - i < 2 {
        return Ok(None);
    }
    let (start_byte, _) = token_bytes(src, &toks[i].span)?;
    let (_, end_byte) = token_bytes(src, &toks[end - 1].span)?;
    let name = src.slice(start_byte, end_byte);
    let plain = name.chars().all(|c| "=!<>~@#%^&|`?".contains(c));
    if !plain || !(name.starts_with('=') || name.starts_with('!')) {
        return Ok(None);
    }
    Ok(Some((end, name.to_string())))
}

/// Clause words that cannot end an operand, so an operator after one of them is not a binary operator.
const OPERAND_BREAKING_WORDS: [&str; 20] = [
    "SELECT", "WHERE", "AND", "OR", "NOT", "ON", "WHEN", "THEN", "ELSE", "CASE", "BY", "HAVING", "SET", "RETURNING",
    "AS", "IN", "IS", "VALUES", "FROM", "JOIN",
];

/// The last token already emitted that is not whitespace.
fn last_significant(out: &[TokenWithSpan]) -> Option<&Token> {
    out.iter().rev().map(|t| &t.token).find(|t| !matches!(t, Token::Whitespace(_)))
}

/// Does the last significant token already emitted end an operand, so that a following operator is binary?
fn follows_operand(out: &[TokenWithSpan]) -> bool {
    match last_significant(out) {
        Some(Token::RParen | Token::RBracket | Token::Number(..) | Token::SingleQuotedString(_) | Token::Placeholder(_)) => {
            true
        }
        Some(Token::Word(w)) => {
            w.quote_style.is_some() || !OPERAND_BREAKING_WORDS.contains(&w.value.to_ascii_uppercase().as_str())
        }
        _ => false,
    }
}

/// Does this statement text start with `ALTER PROCEDURE` (the first two unquoted words)? Used by the
/// binder, which sees `ALTER FUNCTION` in the AST for both forms (see [`alter_procedure_as_function`]).
pub fn is_alter_procedure_text(text: &str) -> bool {
    verb_object_text(text, "ALTER", "PROCEDURE")
}

/// Does this statement text start with `ALTER ROUTINE` (the first two unquoted words)? The binder then
/// resolves any routine kind (PG `OBJECT_ROUTINE`), see [`alter_procedure_as_function`].
pub fn is_alter_routine_text(text: &str) -> bool {
    verb_object_text(text, "ALTER", "ROUTINE")
}

/// Does this statement text start with `DROP ROUTINE` (the first two unquoted words)? The binder then
/// resolves any routine kind (PG `OBJECT_ROUTINE`), see [`drop_routine_as_function`].
pub fn is_drop_routine_text(text: &str) -> bool {
    verb_object_text(text, "DROP", "ROUTINE")
}

/// Does this statement text start with `DROP DOMAIN` (the first two unquoted words)? The binder then reads a
/// `DROP TYPE` AST (from [`drop_domain_list_as_type`]) as a domain drop.
pub fn is_drop_domain_text(text: &str) -> bool {
    verb_object_text(text, "DROP", "DOMAIN")
}

/// Do the first two unquoted words of `text` read `<verb> <object>`?
fn verb_object_text(text: &str, verb: &str, object: &str) -> bool {
    let dialect = PostgreSqlDialect {};
    let Ok(toks) = Tokenizer::new(&dialect, text).tokenize() else {
        return false;
    };
    let words: Vec<String> = toks
        .iter()
        .filter_map(|t| match t {
            Token::Word(w) if w.quote_style.is_none() => Some(w.value.to_ascii_uppercase()),
            _ => None,
        })
        .take(2)
        .collect();
    matches!(words.as_slice(), [a, b] if a == verb && b == object)
}

/// `ROUTINE` and `ROUTINES` object-kind keywords of GRANT/REVOKE, as (token index, keyword). sqlparser
/// 0.63 has neither: `ON ROUTINE name` (any routine kind) and `ALL ROUTINES IN SCHEMA s` (functions and
/// procedures). `ROUTINE` counts only right after `ON` and before a name, so `ON routine TO r` and
/// `ON routine.t` still name a table (PG keeps ROUTINE unreserved).
fn routine_object_keywords(toks: &[TokenWithSpan]) -> Vec<(usize, &'static str)> {
    let sig: Vec<usize> = (0..toks.len()).filter(|&i| !matches!(toks[i].token, Token::Whitespace(_))).collect();
    let mut out = Vec::new();
    if !matches!(sig.first().and_then(|&i| bare_word(&toks[i].token)).as_deref(), Some("GRANT" | "REVOKE")) {
        return out;
    }
    for (k, &i) in sig.iter().enumerate() {
        let prev = k.checked_sub(1).and_then(|p| bare_word(&toks[sig[p]].token));
        let next = sig.get(k + 1).map(|&j| &toks[j].token);
        let next_word = next.and_then(bare_word);
        match bare_word(&toks[i].token).as_deref() {
            Some("ROUTINE")
                if prev.as_deref() == Some("ON")
                    && matches!(next, Some(Token::Word(_)))
                    && !matches!(next_word.as_deref(), Some("TO" | "FROM")) =>
            {
                out.push((i, "ROUTINE"));
            }
            Some("ROUTINES") if prev.as_deref() == Some("ALL") && next_word.as_deref() == Some("IN") => {
                out.push((i, "ROUTINES"));
            }
            _ => {}
        }
    }
    out
}

/// Rewrite the keywords of [`routine_object_keywords`] into the sqlparser forms: `ROUTINE` becomes
/// `FUNCTION` and `ROUTINES` becomes `FUNCTIONS`. The binder reads the original keyword back from the
/// statement text ([`grant_routine_text`], `dispatch::GRANT_ROUTINE_KEY`).
fn routine_grant_as_function(toks: &[TokenWithSpan]) -> Vec<TokenWithSpan> {
    let mut out = toks.to_vec();
    for (i, keyword) in routine_object_keywords(toks) {
        let replacement = if keyword == "ROUTINE" { "FUNCTION" } else { "FUNCTIONS" };
        out[i] = TokenWithSpan::new(Token::make_keyword(replacement), toks[i].span);
    }
    out
}

/// The routine object keyword of a GRANT/REVOKE statement text: `"routine"` for `ON ROUTINE`,
/// `"all routines"` for `ALL ROUTINES IN SCHEMA`, `None` for every other statement.
pub fn grant_routine_text(text: &str) -> Option<&'static str> {
    let dialect = PostgreSqlDialect {};
    let toks = Tokenizer::new(&dialect, text).tokenize_with_location().ok()?;
    routine_object_keywords(&toks).first().map(|(_, keyword)| if *keyword == "ROUTINE" { "routine" } else { "all routines" })
}

/// `JSON_TABLE` named columns without a `PATH` clause take the default path `$."name"` (PG gram.y
/// `makeJsonTablePath`). sqlparser 0.63 requires `PATH`, so the default is spelled out before the column's
/// closing `,` or `)`. Only the `COLUMNS (...)` lists of a `JSON_TABLE` call are rewritten.
fn json_table_default_paths(toks: &[TokenWithSpan]) -> Vec<TokenWithSpan> {
    let sig: Vec<usize> = (0..toks.len()).filter(|&i| !matches!(toks[i].token, Token::Whitespace(_))).collect();
    if !sig.iter().any(|&i| bare_word(&toks[i].token).as_deref() == Some("JSON_TABLE")) {
        return toks.to_vec();
    }
    // (token index, token) pairs: each token is inserted right before the token at that index.
    let mut inserts: Vec<(usize, Token)> = Vec::new();
    for k in 0..sig.len() {
        let opens_columns = bare_word(&toks[sig[k]].token).as_deref() == Some("COLUMNS")
            && matches!(sig.get(k + 1).map(|&j| &toks[j].token), Some(Token::LParen));
        if !opens_columns {
            continue;
        }
        let mut depth = 0usize;
        let mut item: Vec<usize> = Vec::new();
        for &j in &sig[k + 2..] {
            let ends_item = match toks[j].token {
                Token::LParen => {
                    depth += 1;
                    false
                }
                Token::RParen if depth == 0 => true,
                Token::RParen => {
                    depth -= 1;
                    false
                }
                Token::Comma if depth == 0 => true,
                _ => false,
            };
            if !ends_item {
                item.push(j);
                continue;
            }
            if let Some(path) = default_json_path(toks, &item) {
                inserts.push((j, Token::make_keyword("PATH")));
                inserts.push((j, Token::SingleQuotedString(path)));
            }
            item.clear();
            if matches!(toks[j].token, Token::RParen) {
                break;
            }
        }
    }
    let mut out = Vec::with_capacity(toks.len() + inserts.len());
    for (i, t) in toks.iter().enumerate() {
        for (at, token) in &inserts {
            if *at == i {
                out.push(TokenWithSpan::new(token.clone(), t.span));
            }
        }
        out.push(t.clone());
    }
    out
}

/// The default `PATH` (`$."name"`) of one `JSON_TABLE` column; `item` holds the significant token indexes
/// of the column definition. `None` for an empty item, a `NESTED` or `FOR ORDINALITY` column, or a column
/// that already has a `PATH` clause.
fn default_json_path(toks: &[TokenWithSpan], item: &[usize]) -> Option<String> {
    let first = *item.first()?;
    let name = match &toks[first].token {
        Token::Word(w) => w.value.clone(),
        _ => return None,
    };
    let has_path_clause = item
        .iter()
        .any(|&j| matches!(bare_word(&toks[j].token).as_deref(), Some("PATH" | "FOR" | "NESTED")));
    if has_path_clause {
        None
    } else {
        Some(format!("$.\"{name}\""))
    }
}

/// `CREATE [GLOBAL|LOCAL] [TEMP|TEMPORARY|UNLOGGED] TABLE name (col, col) ... AS query`: PostgreSQL names
/// the query's output columns with a bare-name list (gram.y `create_as_target`, `opt_column_list`). sqlparser
/// 0.63 reads only typed column definitions, so the list is cut out of the tokens here (see
/// [`parse_ast_tokens`]) and its names are attached by [`attach_ctas_column_names`].
/// Returns the token indices of the `(` and of the `)` (both inclusive, into `toks`) and the names.
fn ctas_column_name_list(toks: &[TokenWithSpan]) -> Option<(usize, usize, Vec<sqlparser::ast::Ident>)> {
    let sig: Vec<usize> = (0..toks.len()).filter(|&i| !matches!(toks[i].token, Token::Whitespace(_))).collect();
    let tok = |k: usize| sig.get(k).map(|&i| &toks[i].token);
    let word = |k: usize| tok(k).and_then(bare_word);
    if word(0).as_deref() != Some("CREATE") {
        return None;
    }
    let mut k = 1;
    while matches!(word(k).as_deref(), Some("GLOBAL" | "LOCAL" | "TEMP" | "TEMPORARY" | "UNLOGGED")) {
        k += 1;
    }
    if word(k).as_deref() != Some("TABLE") {
        return None;
    }
    k += 1;
    // The table name (possibly schema-qualified, with any IF NOT EXISTS words) runs up to the `(`.
    while matches!(tok(k), Some(Token::Word(_) | Token::Period)) {
        k += 1;
    }
    if !matches!(tok(k), Some(Token::LParen)) {
        return None;
    }
    let from = sig[k];
    k += 1;
    let mut names = Vec::new();
    loop {
        let Some(Token::Word(w)) = tok(k) else {
            return None;
        };
        names.push(w.clone().into_ident(toks[sig[k]].span));
        k += 1;
        match tok(k) {
            Some(Token::Comma) => k += 1,
            Some(Token::RParen) => break,
            _ => return None,
        }
    }
    let to = sig[k];
    k += 1;
    if (k..sig.len()).any(|j| bare_word(&toks[sig[j]].token).as_deref() == Some("AS")) {
        Some((from, to, names))
    } else {
        None
    }
}

/// Attach the names of a CTAS column list (see [`ctas_column_name_list`]) as untyped column definitions. This
/// is the `CreateTable.columns` form that `bind_create_table_as` reads as the output column names.
fn attach_ctas_column_names(stmt: &mut Statement, names: Vec<sqlparser::ast::Ident>) {
    if let Statement::CreateTable(ct) = stmt {
        ct.columns = names
            .into_iter()
            .map(|name| sqlparser::ast::ColumnDef {
                name,
                data_type: sqlparser::ast::DataType::Unspecified,
                options: Vec::new(),
            })
            .collect();
    }
}

/// `DROP DOMAIN name [, name ...]` with more than one name is parsed as `DROP TYPE` (sqlparser's `DropDomain`
/// takes one name): the keyword is rewritten here, and the binder recognizes the form from the statement text
/// ([`is_drop_domain_text`]). A single name keeps sqlparser's `DropDomain`.
fn drop_domain_list_as_type(toks: &[TokenWithSpan]) -> Vec<TokenWithSpan> {
    let mut out = toks.to_vec();
    let sig: Vec<usize> = (0..toks.len()).filter(|&i| !matches!(toks[i].token, Token::Whitespace(_))).collect();
    if let [a, b, rest @ ..] = sig.as_slice() {
        let is_drop_domain = bare_word(&toks[*a].token).as_deref() == Some("DROP")
            && bare_word(&toks[*b].token).as_deref() == Some("DOMAIN");
        let has_comma = rest.iter().any(|&i| matches!(toks[i].token, Token::Comma));
        if is_drop_domain && has_comma {
            out[*b] = TokenWithSpan::new(Token::make_keyword("TYPE"), toks[*b].span);
        }
    }
    out
}

/// Insert [`UNNAMED_PARAM_MARKER`] before the type of each unnamed parameter of
/// `CREATE [OR REPLACE] PROCEDURE name(...)`. A parameter is unnamed when the text after its optional
/// mode (IN/OUT/INOUT) and before any `DEFAULT`/`=` is exactly one data type: `integer`,
/// `double precision`, `t`. `x integer` and `x double precision` are named.
fn unnamed_procedure_params(toks: &[TokenWithSpan]) -> Vec<TokenWithSpan> {
    let sig: Vec<usize> = (0..toks.len()).filter(|&i| !matches!(toks[i].token, Token::Whitespace(_))).collect();
    let head: Vec<String> = sig.iter().filter_map(|&i| bare_word(&toks[i].token)).take(4).collect();
    let head_refs: Vec<&str> = head.iter().map(String::as_str).collect();
    if !matches!(head_refs.as_slice(), ["CREATE", "PROCEDURE", ..] | ["CREATE", "OR", "REPLACE", "PROCEDURE", ..]) {
        return toks.to_vec();
    }
    // The name (`word [. word]*`) follows PROCEDURE; the parameter list opens at the next `(`.
    let Some(proc_at) = sig.iter().position(|&i| bare_word(&toks[i].token).as_deref() == Some("PROCEDURE")) else {
        return toks.to_vec();
    };
    let mut k = proc_at + 1;
    while k < sig.len() && matches!(toks[sig[k]].token, Token::Word(_) | Token::Period) {
        k += 1;
    }
    if k >= sig.len() || !matches!(toks[sig[k]].token, Token::LParen) {
        return toks.to_vec();
    }
    // Split the list on top-level commas; each item is a list of significant token indexes.
    let mut marks: Vec<usize> = Vec::new();
    let mut item: Vec<usize> = Vec::new();
    let mut depth = 0usize;
    for &i in &sig[k + 1..] {
        let closes_list = match toks[i].token {
            Token::LParen => {
                depth += 1;
                false
            }
            Token::RParen if depth == 0 => true,
            Token::RParen => {
                depth -= 1;
                false
            }
            Token::Comma if depth == 0 => true,
            _ => false,
        };
        if !closes_list {
            item.push(i);
            continue;
        }
        if let Some(at) = unnamed_param_at(toks, &item) {
            marks.push(at);
        }
        item.clear();
        if matches!(toks[i].token, Token::RParen) {
            break;
        }
    }
    let mut out = Vec::with_capacity(toks.len() + marks.len());
    for (i, t) in toks.iter().enumerate() {
        if marks.contains(&i) {
            let word = Word { value: UNNAMED_PARAM_MARKER.to_string(), quote_style: Some('"'), keyword: Keyword::NoKeyword };
            out.push(TokenWithSpan::new(Token::Word(word), t.span));
        }
        out.push(t.clone());
    }
    out
}

/// Token index where the unnamed marker goes for one parameter (its type), `None` when the parameter
/// is named or has no type. `item` holds significant token indexes of the parameter.
fn unnamed_param_at(toks: &[TokenWithSpan], item: &[usize]) -> Option<usize> {
    let mode_len = match item.first().and_then(|&i| bare_word(&toks[i].token)).as_deref() {
        Some("IN" | "OUT" | "INOUT") => 1,
        _ => 0,
    };
    let type_toks: Vec<usize> = item
        .get(mode_len..)?
        .iter()
        .copied()
        .take_while(|&i| !matches!(toks[i].token, Token::Eq) && bare_word(&toks[i].token).as_deref() != Some("DEFAULT"))
        .collect();
    let first = *type_toks.first()?;
    let last = *type_toks.last()?;
    let mut parts: Vec<TokenWithSpan> = type_toks.iter().map(|&i| toks[i].clone()).collect();
    parts.push(TokenWithSpan::new(Token::EOF, Span::new(toks[last].span.end, toks[last].span.end)));
    let dialect = PostgreSqlDialect {};
    let mut p = Parser::new(&dialect).with_tokens_with_locations(parts);
    let is_type_only = p.parse_data_type().is_ok() && matches!(p.peek_token().token, Token::EOF);
    if is_type_only {
        Some(first)
    } else {
        None
    }
}

/// Insert `= TRUE` after each bare option name in the `WITH (...)` list of `CREATE [OR REPLACE]
/// [TEMP|TEMPORARY] [RECURSIVE] [MATERIALIZED] VIEW` (PG `def_elem` without `= arg`, which means
/// boolean true). sqlparser's PostgreSQL option list requires `name = value`, so the bare form is
/// rewritten here; the binder then reads the same value it reads for `name = true`.
fn bare_view_options(toks: &[TokenWithSpan]) -> Vec<TokenWithSpan> {
    let sig: Vec<usize> = (0..toks.len()).filter(|&i| !matches!(toks[i].token, Token::Whitespace(_))).collect();
    let Some(&first) = sig.first() else {
        return toks.to_vec();
    };
    let word = |k: usize| bare_word(&toks[sig[k]].token);
    if bare_word(&toks[first].token).as_deref() != Some("CREATE") {
        return toks.to_vec();
    }
    let mut view_k = 1;
    while view_k < sig.len() && matches!(word(view_k).as_deref(), Some("OR" | "REPLACE" | "TEMP" | "TEMPORARY" | "RECURSIVE" | "MATERIALIZED")) {
        view_k += 1;
    }
    if view_k >= sig.len() || word(view_k).as_deref() != Some("VIEW") {
        return toks.to_vec();
    }
    // The `WITH (` that follows the view name, before the query (`AS`).
    let with_at = (view_k + 1..sig.len())
        .take_while(|&k| word(k).as_deref() != Some("AS"))
        .find(|&k| word(k).as_deref() == Some("WITH") && sig.get(k + 1).is_some_and(|&j| matches!(toks[j].token, Token::LParen)));
    let Some(with_at) = with_at else {
        return toks.to_vec();
    };
    // Each top-level item of the list that is a single word is a bare option name.
    let mut bare: Vec<usize> = Vec::new();
    let mut item: Vec<usize> = Vec::new();
    let mut depth = 0usize;
    for &i in &sig[with_at + 2..] {
        let closes_item = depth == 0 && matches!(toks[i].token, Token::Comma | Token::RParen);
        if !closes_item {
            match toks[i].token {
                Token::LParen => depth += 1,
                Token::RParen => depth -= 1,
                _ => {}
            }
            item.push(i);
            continue;
        }
        if let [only] = item.as_slice() {
            if matches!(toks[*only].token, Token::Word(_)) {
                bare.push(*only);
            }
        }
        item.clear();
        if matches!(toks[i].token, Token::RParen) {
            break;
        }
    }
    if bare.is_empty() {
        return toks.to_vec();
    }
    let mut out = Vec::with_capacity(toks.len() + 2 * bare.len());
    for (i, t) in toks.iter().enumerate() {
        out.push(t.clone());
        if bare.contains(&i) {
            out.push(TokenWithSpan::new(Token::Eq, t.span));
            out.push(TokenWithSpan::new(Token::make_keyword("TRUE"), t.span));
        }
    }
    out
}

/// Rewrite each `VARIADIC` keyword that starts a call argument (directly after `(` or `,`) into the
/// marker argument [`VARIADIC_MARKER`]. Only statements other than `CREATE`/`ALTER`/`DROP` are
/// rewritten: there `VARIADIC` is an argument mode in a function signature, which sqlparser parses.
fn mark_variadic_args(toks: &[TokenWithSpan]) -> Vec<TokenWithSpan> {
    let head: Vec<String> = toks.iter().filter_map(|t| bare_word(&t.token)).take(4).collect();
    let words: Vec<&str> = head.iter().map(String::as_str).collect();
    if is_function_header(&words) || matches!(words.as_slice(), ["ALTER" | "DROP", "FUNCTION" | "PROCEDURE" | "ROUTINE", ..]) {
        return toks.to_vec();
    }
    let mut out = Vec::with_capacity(toks.len());
    let mut prev: Option<&Token> = None;
    for t in toks {
        if matches!(t.token, Token::Whitespace(_)) {
            out.push(t.clone());
            continue;
        }
        let starts_arg = matches!(prev, Some(Token::LParen | Token::Comma));
        if starts_arg && bare_word(&t.token).as_deref() == Some("VARIADIC") {
            let word = Word { value: VARIADIC_MARKER.to_string(), quote_style: Some('"'), keyword: Keyword::NoKeyword };
            out.push(TokenWithSpan::new(Token::Word(word), t.span));
            out.push(TokenWithSpan::new(Token::RArrow, t.span));
        } else {
            out.push(t.clone());
        }
        prev = Some(&t.token);
    }
    out
}

/// Index of the next token at or after `from` that is not whitespace or a comment.
fn next_significant(toks: &[TokenWithSpan], from: usize) -> Option<usize> {
    (from..toks.len()).find(|&j| !matches!(toks[j].token, Token::Whitespace(_)))
}

/// Name of the marker identifier that stands for the indirection (subscript or field) of a column in an
/// UPDATE `SET` target (`SET a[2] = ..`, `SET c.f = ..`) or an INSERT column list (`INSERT INTO t
/// (b[1:3], c.x)`). sqlparser 0.63 parses neither form there, so [`mark_subscript_targets`] rewrites
/// them: in a `SET` target the indirection becomes `.<marker>` (a qualified name the binder rejects
/// with 0A000 SQL-14), and in an INSERT column list the whole entry becomes one quoted identifier that
/// `update_delete`/`insert` reject with 0A000. Like the other markers it holds a NUL, which PostgreSQL
/// identifiers cannot contain.
pub const SUBSCRIPT_TARGET_MARKER: &str = "\0pgarbor subscript target";

/// One indirected target found by [`scan_target_list`]: (name token index, first suffix token index,
/// last suffix token index, INSERT column list (the whole entry is replaced)).
type IndirectedTarget = (usize, usize, usize, bool);

/// Name of the marker identifier that stands for one expression element of an `INSERT .. ON CONFLICT
/// (elem, ..)` inference list (PG `index_elem` with an expression, `gram.y`). sqlparser 0.63's
/// `ConflictTarget::Columns` takes identifiers only, so [`mark_conflict_expressions`] replaces such an
/// element with one quoted identifier: this prefix followed by the element's source text, which
/// `binder::on_conflict` parses and binds. Like the other markers it holds a NUL.
pub const CONFLICT_EXPR_MARKER: &str = "\0pgarbor conflict expr\0";

/// Rewrite the expression elements of the `ON CONFLICT (elem, ..)` inference lists of an INSERT into
/// [`CONFLICT_EXPR_MARKER`] identifiers. A plain identifier element is left alone, and so is an element
/// sqlparser does not read as one expression (its syntax error is reported as before). An element with
/// trailing index options (operator class, ordering) is 0A000 SQL-27. Other statements are unchanged.
fn mark_conflict_expressions(toks: &[TokenWithSpan], src: &SourceMap) -> Result<Vec<TokenWithSpan>> {
    let is_insert = next_significant(toks, 0).is_some_and(|h| bare_word(&toks[h].token).as_deref() == Some("INSERT"));
    if !is_insert {
        return Ok(toks.to_vec());
    }
    // (first, last) significant token index of each expression element, with its marker token.
    let mut found: Vec<(usize, usize, TokenWithSpan)> = Vec::new();
    for on in 0..toks.len() {
        if bare_word(&toks[on].token).as_deref() != Some("ON") {
            continue;
        }
        let Some(conflict) = next_significant(toks, on + 1) else { continue };
        if bare_word(&toks[conflict].token).as_deref() != Some("CONFLICT") {
            continue;
        }
        let Some(open) = next_significant(toks, conflict + 1) else { continue };
        if matches!(toks[open].token, Token::LParen) {
            scan_conflict_elements(toks, open, src, &mut found)?;
        }
    }
    if found.is_empty() {
        return Ok(toks.to_vec());
    }
    found.sort_unstable_by_key(|f| f.0);
    let mut out: Vec<TokenWithSpan> = Vec::with_capacity(toks.len());
    let mut at = 0usize;
    for (first, last, marker) in found {
        out.extend_from_slice(&toks[at..first]);
        out.push(marker);
        at = last + 1;
    }
    out.extend_from_slice(&toks[at..]);
    Ok(out)
}

/// Visit the elements of the inference list whose `(` is token `open` (module of
/// [`mark_conflict_expressions`]). Elements are separated by depth-0 commas and end at the matching `)`.
fn scan_conflict_elements(
    toks: &[TokenWithSpan],
    open: usize,
    src: &SourceMap,
    found: &mut Vec<(usize, usize, TokenWithSpan)>,
) -> Result<()> {
    let mut elem: Option<(usize, usize)> = None;
    let mut depth = 0usize;
    let mut i = open + 1;
    while i < toks.len() {
        let t = &toks[i].token;
        if matches!(t, Token::Whitespace(_)) {
            i += 1;
            continue;
        }
        if depth == 0 && matches!(t, Token::RParen | Token::Comma) {
            if let Some((first, last)) = elem.take() {
                conflict_element(toks, first, last, src, found)?;
            }
            if matches!(t, Token::RParen) {
                break;
            }
            i += 1;
            continue;
        }
        match t {
            Token::LParen | Token::LBracket => depth += 1,
            Token::RParen | Token::RBracket => depth = depth.saturating_sub(1),
            _ => {}
        }
        elem = Some(elem.map_or((i, i), |(first, _)| (first, i)));
        i += 1;
    }
    Ok(())
}

/// One inference element spanning tokens `first..=last`: a plain identifier is kept; anything else must
/// parse as one expression and is recorded for replacement by a [`CONFLICT_EXPR_MARKER`] identifier.
fn conflict_element(
    toks: &[TokenWithSpan],
    first: usize,
    last: usize,
    src: &SourceMap,
    found: &mut Vec<(usize, usize, TokenWithSpan)>,
) -> Result<()> {
    if first == last && matches!(toks[first].token, Token::Word(_)) {
        return Ok(());
    }
    let dialect = PostgreSqlDialect {};
    let mut elem_toks: Vec<TokenWithSpan> = toks[first..=last].to_vec();
    elem_toks.push(TokenWithSpan::new(Token::EOF, Span::new(toks[last].span.end, toks[last].span.end)));
    let mut parser = Parser::new(&dialect).with_tokens_with_locations(elem_toks);
    if parser.parse_expr().is_err() {
        return Ok(());
    }
    if !matches!(parser.peek_token().token, Token::EOF) {
        let err = PgError::unimplemented("SQL-27", "ON CONFLICT inference element with an operator class or ordering");
        return Err(match src.char_pos(toks[first].span.start) {
            Some(pos) => err.with_position(pos),
            None => err,
        });
    }
    let (start, _) = token_bytes(src, &toks[first].span)?;
    let (_, end) = token_bytes(src, &toks[last].span)?;
    let marker = format!("{CONFLICT_EXPR_MARKER}{}", src.slice(start, end));
    let span = Span::new(toks[first].span.start, toks[last].span.end);
    found.push((first, last, TokenWithSpan::new(Token::make_word(&marker, Some('"')), span)));
    Ok(())
}

/// Rewrite the indirected column targets of UPDATE `SET` lists, of INSERT column lists and of the
/// `ON CONFLICT .. DO UPDATE SET` list of an INSERT, so that sqlparser accepts them (see
/// [`SUBSCRIPT_TARGET_MARKER`]). Every other token is unchanged, so a statement without such a target
/// is returned as it is.
fn mark_subscript_targets(toks: &[TokenWithSpan]) -> Vec<TokenWithSpan> {
    let mut found: Vec<IndirectedTarget> = Vec::new();
    if let Some(head) = next_significant(toks, 0) {
        match bare_word(&toks[head].token).as_deref() {
            Some("UPDATE") => {
                if let Some(set) = find_depth0_word(toks, head + 1, "SET") {
                    scan_target_list(toks, set + 1, &["WHERE", "FROM", "RETURNING"], false, &mut found);
                }
            }
            Some("INSERT") => {
                if let Some(open) = insert_column_list_open(toks, head) {
                    scan_target_list(toks, open, &[], true, &mut found);
                }
                if let Some(do_at) = find_depth0_word(toks, head + 1, "DO") {
                    if let Some(upd) = next_significant(toks, do_at + 1) {
                        if bare_word(&toks[upd].token).as_deref() == Some("UPDATE") {
                            if let Some(set) = find_depth0_word(toks, upd + 1, "SET") {
                                scan_target_list(toks, set + 1, &["WHERE", "RETURNING"], false, &mut found);
                            }
                        }
                    }
                }
            }
            _ => {}
        }
    }
    if found.is_empty() {
        return toks.to_vec();
    }
    found.sort_unstable();
    let mut out: Vec<TokenWithSpan> = Vec::with_capacity(toks.len() + 2 * found.len());
    let mut at = 0usize;
    for (name, first, last, whole_entry) in found {
        if whole_entry {
            out.extend_from_slice(&toks[at..name]);
            out.push(TokenWithSpan::new(Token::make_word(SUBSCRIPT_TARGET_MARKER, Some('"')), toks[name].span));
        } else {
            out.extend_from_slice(&toks[at..first]);
            out.push(TokenWithSpan::new(Token::Period, toks[first].span));
            out.push(TokenWithSpan::new(Token::make_word(SUBSCRIPT_TARGET_MARKER, Some('"')), toks[first].span));
        }
        at = last + 1;
    }
    out.extend_from_slice(&toks[at..]);
    out
}

/// Scan one comma-separated target list starting at token `start`. The list ends at the first depth-0
/// word in `stops`, at a depth-0 `)` (the enclosing list), or at the end of the tokens. An entry that
/// begins with an indirected column name is recorded in `found`; `insert_list` selects the INSERT
/// column-list form (dotted field names too, whole-entry replacement).
fn scan_target_list(
    toks: &[TokenWithSpan],
    start: usize,
    stops: &[&str],
    insert_list: bool,
    found: &mut Vec<IndirectedTarget>,
) {
    let mut depth = 0usize;
    let mut entry_start = true;
    let mut i = start;
    while i < toks.len() {
        let t = &toks[i].token;
        if matches!(t, Token::Whitespace(_)) {
            i += 1;
            continue;
        }
        if depth == 0 {
            if matches!(t, Token::RParen) {
                return;
            }
            if bare_word(t).is_some_and(|w| stops.contains(&w.as_str())) {
                return;
            }
            if entry_start {
                if let Some((first, last)) = indirection_extent(toks, i, insert_list) {
                    found.push((i, first, last, insert_list));
                    i = last + 1;
                    entry_start = false;
                    continue;
                }
            }
        }
        entry_start = false;
        match t {
            Token::LParen | Token::LBracket => depth += 1,
            Token::RParen | Token::RBracket => depth = depth.saturating_sub(1),
            Token::Comma if depth == 0 => entry_start = true,
            _ => {}
        }
        i += 1;
    }
}

/// Extent of the indirection that follows the column name at token `i`: `[..]` groups and, when `dots`
/// is set, `.name` parts. Returns (first suffix token, last suffix token), or `None` when there is none.
fn indirection_extent(toks: &[TokenWithSpan], i: usize, dots: bool) -> Option<(usize, usize)> {
    if !matches!(toks[i].token, Token::Word(_)) {
        return None;
    }
    let mut last = i;
    let mut first: Option<usize> = None;
    while let Some(k) = next_significant(toks, last + 1) {
        let end = match toks[k].token {
            Token::LBracket => match bracket_close(toks, k) {
                Some(close) => close,
                None => break,
            },
            Token::Period if dots => match next_significant(toks, k + 1) {
                Some(w) if matches!(toks[w].token, Token::Word(_)) => w,
                _ => break,
            },
            _ => break,
        };
        first.get_or_insert(k);
        last = end;
    }
    first.map(|f| (f, last))
}

/// Index of the `]` that closes the `[` at token `open`.
fn bracket_close(toks: &[TokenWithSpan], open: usize) -> Option<usize> {
    let mut depth = 0usize;
    for (j, t) in toks.iter().enumerate().skip(open) {
        match t.token {
            Token::LBracket => depth += 1,
            Token::RBracket => {
                depth = depth.saturating_sub(1);
                if depth == 0 {
                    return Some(j);
                }
            }
            _ => {}
        }
    }
    None
}

/// Index (in `toks`) of the first schema element of `CREATE SCHEMA [IF NOT EXISTS] [name] [AUTHORIZATION role]
/// schema_element...` (PG gram.y `CreateSchemaStmt`), or `None` when the chunk is not a CREATE SCHEMA or has no
/// elements. A schema element starts with an unquoted `CREATE` or `GRANT` at depth zero after the header; the
/// header itself cannot contain either word unquoted, so the first such token is the element list.
fn create_schema_element_at(toks: &[TokenWithSpan]) -> Option<usize> {
    let sig: Vec<usize> = (0..toks.len()).filter(|&i| !matches!(toks[i].token, Token::Whitespace(_))).collect();
    let first = bare_word(&toks.get(*sig.first()?)?.token)?;
    let second = bare_word(&toks.get(*sig.get(1)?)?.token)?;
    if first != "CREATE" || second != "SCHEMA" {
        return None;
    }
    let mut depth = 0usize;
    for &i in sig.iter().skip(2) {
        match toks[i].token {
            Token::LParen | Token::LBracket => depth += 1,
            Token::RParen | Token::RBracket => depth = depth.saturating_sub(1),
            _ => {}
        }
        if depth == 0 && matches!(bare_word(&toks[i].token).as_deref(), Some("CREATE") | Some("GRANT")) {
            return Some(i);
        }
    }
    None
}

/// Index of the first token at or after `from` that is the unquoted word `word` (uppercase) at parenthesis
/// and bracket depth zero.
fn find_depth0_word(toks: &[TokenWithSpan], from: usize, word: &str) -> Option<usize> {
    let mut depth = 0usize;
    for (i, t) in toks.iter().enumerate().skip(from) {
        match t.token {
            Token::LParen | Token::LBracket => depth += 1,
            Token::RParen | Token::RBracket => depth = depth.saturating_sub(1),
            _ => {}
        }
        if depth == 0 && bare_word(&t.token).as_deref() == Some(word) {
            return Some(i);
        }
    }
    None
}

/// For an INSERT whose table name is followed directly by a column list, the token index just after that
/// list's `(`. A `VALUES`, `SELECT`, `WITH`, `TABLE` or `DEFAULT` keyword before the `(`, or a query inside
/// the parentheses, means there is no column list.
fn insert_column_list_open(toks: &[TokenWithSpan], head: usize) -> Option<usize> {
    let into = find_depth0_word(toks, head + 1, "INTO")?;
    let mut k = next_significant(toks, into + 1)?;
    loop {
        match &toks[k].token {
            Token::LParen => {
                let first = next_significant(toks, k + 1)?;
                let query_start = matches!(
                    bare_word(&toks[first].token).as_deref(),
                    Some("SELECT" | "WITH" | "VALUES" | "TABLE")
                );
                return if query_start { None } else { Some(k + 1) };
            }
            Token::Period => {}
            Token::Word(w) => {
                let keyword = w.quote_style.is_none()
                    && matches!(w.value.to_ascii_uppercase().as_str(), "VALUES" | "SELECT" | "DEFAULT" | "WITH" | "TABLE" | "OVERRIDING");
                if keyword {
                    return None;
                }
            }
            _ => return None,
        }
        k = next_significant(toks, k + 1)?;
    }
}

/// PostgreSQL `TABLE name [clauses]` (gram.y simple_select) is `SELECT * FROM name [clauses]`. sqlparser
/// reads TABLE only inside a query body, and its table form consumes three tokens (so `ORDER BY` after
/// the name is lost), so a leading TABLE is rewritten to `SELECT * FROM` before parsing.
fn table_statement_as_select(toks: &[TokenWithSpan]) -> Option<Vec<TokenWithSpan>> {
    let j = next_significant(toks, 0)?;
    if bare_word(&toks[j].token).as_deref() != Some("TABLE") {
        return None;
    }
    let span = toks[j].span;
    let mut out = Vec::with_capacity(toks.len() + 2);
    out.extend_from_slice(&toks[..j]);
    out.push(TokenWithSpan::new(Token::make_keyword("SELECT"), span));
    out.push(TokenWithSpan::new(Token::Mul, span));
    out.push(TokenWithSpan::new(Token::make_keyword("FROM"), span));
    out.extend_from_slice(&toks[j + 1..]);
    Some(out)
}

/// `ANY`, `ALL` or `SOME` followed by `( VALUES ...`: sqlparser 0.63 reads the parenthesized right operand as a
/// sub-query only when `SELECT` or `WITH` follows the `(` (`peek_sub_query`). A bare `VALUES` list is then parsed
/// as one expression, and the operand fails at the comma after its first row. Each such list is rewritten to
/// `( SELECT * FROM ( VALUES ... ) )`, the `select_with_parens` form PostgreSQL's gram.y reads.
fn values_quantified_operand(toks: &[TokenWithSpan]) -> Vec<TokenWithSpan> {
    let mut out = Vec::with_capacity(toks.len() + 8);
    // One entry per open parenthesis: `true` when a `SELECT * FROM (` wrapper was inserted after it.
    let mut open: Vec<bool> = Vec::new();
    // Bare word (upper case) of the previous significant token.
    let mut prev: Option<String> = None;
    for (i, t) in toks.iter().enumerate() {
        match t.token {
            Token::LParen => {
                let wrap = matches!(prev.as_deref(), Some("ANY" | "ALL" | "SOME"))
                    && next_significant(toks, i + 1).is_some_and(|j| bare_word(&toks[j].token).as_deref() == Some("VALUES"));
                out.push(t.clone());
                if wrap {
                    out.push(TokenWithSpan::new(Token::make_keyword("SELECT"), t.span));
                    out.push(TokenWithSpan::new(Token::Mul, t.span));
                    out.push(TokenWithSpan::new(Token::make_keyword("FROM"), t.span));
                    out.push(TokenWithSpan::new(Token::LParen, t.span));
                }
                open.push(wrap);
            }
            Token::RParen => {
                if open.pop() == Some(true) {
                    out.push(TokenWithSpan::new(Token::RParen, t.span));
                }
                out.push(t.clone());
            }
            _ => out.push(t.clone()),
        }
        if !matches!(t.token, Token::Whitespace(_)) {
            prev = bare_word(&t.token);
        }
    }
    out
}

/// For `GENERATED ALWAYS AS (` starting at token `i`, the index of the `)` closing that parenthesis.
fn generated_expr_close(toks: &[TokenWithSpan], i: usize) -> Option<usize> {
    let always = next_significant(toks, i + 1)?;
    if bare_word(&toks[always].token).as_deref() != Some("ALWAYS") {
        return None;
    }
    let as_kw = next_significant(toks, always + 1)?;
    if bare_word(&toks[as_kw].token).as_deref() != Some("AS") {
        return None;
    }
    let open = next_significant(toks, as_kw + 1)?;
    if !matches!(toks[open].token, Token::LParen) {
        return None;
    }
    let mut depth = 0usize;
    for (j, t) in toks.iter().enumerate().skip(open) {
        match t.token {
            Token::LParen => depth += 1,
            Token::RParen => {
                depth = depth.saturating_sub(1);
                if depth == 0 {
                    return Some(j);
                }
            }
            _ => {}
        }
    }
    None
}

/// sqlparser 0.63 reads `GENERATED ALWAYS AS (expr)` under the PostgreSQL dialect only when `STORED`
/// follows the expression, so the default kind (PG 18: VIRTUAL) and an explicit `VIRTUAL` fail at
/// the next token. Each such clause is rewritten to end with a `STORED` token (replacing `VIRTUAL`
/// when present) and its kind is recorded, in source order, as `true` for VIRTUAL.
/// [`restore_virtual_generated`] then undoes the rewrite on the AST.
fn mark_virtual_generated(toks: &[TokenWithSpan]) -> (Vec<TokenWithSpan>, Vec<bool>) {
    let mut out = Vec::with_capacity(toks.len() + 4);
    let mut flags = Vec::new();
    let mut i = 0;
    while i < toks.len() {
        let close = match bare_word(&toks[i].token).as_deref() {
            Some("GENERATED") => generated_expr_close(toks, i),
            _ => None,
        };
        let Some(close) = close else {
            out.push(toks[i].clone());
            i += 1;
            continue;
        };
        out.extend_from_slice(&toks[i..=close]);
        match next_significant(toks, close + 1) {
            Some(k) if bare_word(&toks[k].token).as_deref() == Some("STORED") => {
                out.extend_from_slice(&toks[close + 1..=k]);
                flags.push(false);
                i = k + 1;
            }
            Some(k) if bare_word(&toks[k].token).as_deref() == Some("VIRTUAL") => {
                out.extend_from_slice(&toks[close + 1..k]);
                out.push(TokenWithSpan::new(Token::make_keyword("STORED"), toks[k].span));
                flags.push(true);
                i = k + 1;
            }
            _ => {
                out.push(TokenWithSpan::new(Token::make_keyword("STORED"), toks[close].span));
                flags.push(true);
                i = close + 1;
            }
        }
    }
    (out, flags)
}

/// Undo [`mark_virtual_generated`] on the parsed statement: every generated-expression option of
/// the statement's column definitions (CREATE TABLE columns and ALTER TABLE ADD COLUMN), in source
/// order, must match one recorded flag; flagged ones become `GENERATED ALWAYS AS (..) VIRTUAL`.
fn restore_virtual_generated(stmt: &mut Statement, flags: &[bool]) -> Result<()> {
    let mut defs: Vec<&mut sqlparser::ast::ColumnDef> = Vec::new();
    match stmt {
        Statement::CreateTable(ct) => defs.extend(ct.columns.iter_mut()),
        Statement::AlterTable(at) => {
            for op in at.operations.iter_mut() {
                if let AlterTableOperation::AddColumn { column_def, .. } = op {
                    defs.push(column_def);
                }
            }
        }
        _ => {}
    }
    let mut gens: Vec<&mut ColumnOption> = Vec::new();
    for def in defs {
        for opt in def.options.iter_mut() {
            if matches!(opt.option, ColumnOption::Generated { generation_expr: Some(_), .. }) {
                gens.push(&mut opt.option);
            }
        }
    }
    if gens.len() != flags.len() {
        return Err(PgError::internal("generated column clauses do not match the statement"));
    }
    for (opt, is_virtual) in gens.into_iter().zip(flags.iter().copied()) {
        if !is_virtual {
            continue;
        }
        if let ColumnOption::Generated { generated_as, generation_expr_mode, .. } = opt {
            *generated_as = GeneratedAs::Always;
            *generation_expr_mode = Some(GeneratedExpressionMode::Virtual);
        }
    }
    Ok(())
}

/// Marker names of the calls that stand for `ALTER COLUMN c SET EXPRESSION AS (e)` and
/// `ALTER COLUMN c DROP EXPRESSION [IF EXISTS]` (PG 17+). sqlparser 0.63 has neither subcommand, so
/// [`mark_expression_alters`] rewrites them into `SET DEFAULT <marker>(..)`, and the binder
/// (`ddl_alter`) recognizes the marker name. Like [`VARIADIC_MARKER`], each holds a NUL, which
/// PostgreSQL identifiers cannot contain.
pub const SET_EXPRESSION_MARKER: &str = "\0pgarbor set expression";
pub const DROP_EXPRESSION_MARKER: &str = "\0pgarbor drop expression";
pub const DROP_EXPRESSION_IF_EXISTS_MARKER: &str = "\0pgarbor drop expression if exists";
/// Marker of the call that stands for `ALTER COLUMN c SET STATISTICS n` (`n` is the sole argument).
/// sqlparser 0.63 has no statistics subcommand; [`expression_alter_at`] rewrites it like the
/// expression subcommands, and the binder (`ddl_alter`) reads the integer argument.
pub const SET_STATISTICS_MARKER: &str = "\0pgarbor set statistics";
/// Marker of the call that stands for the identity items of `ALTER COLUMN c SET GENERATED ...`,
/// `SET <sequence option>` and `RESTART [WITH n]` (PG `alter_identity_column_option_list`). Its
/// arguments are one call per item, named [`IDENTITY_OPTION_PREFIX`] plus the item key (see
/// [`identity_alter_at`]); the binder (`ddl_alter`) reads them. sqlparser 0.63 has no such clause.
pub const IDENTITY_ALTER_MARKER: &str = "\0pgarbor alter identity";
/// Prefix of the item calls inside [`IDENTITY_ALTER_MARKER`]: `<prefix><key>(value?)`.
pub const IDENTITY_OPTION_PREFIX: &str = "\0pgarbor identity ";

/// Rewrite the expression subcommands of `ALTER TABLE` (see the marker constants) into
/// `SET DEFAULT <marker>(e)` and `SET DEFAULT <marker>()`. Every other statement is unchanged.
fn mark_expression_alters(toks: &[TokenWithSpan]) -> Vec<TokenWithSpan> {
    let head: Vec<String> = toks.iter().filter_map(|t| bare_word(&t.token)).take(2).collect();
    if !matches!(head.as_slice(), [a, b] if a == "ALTER" && b == "TABLE") {
        return toks.to_vec();
    }
    let mut out = Vec::with_capacity(toks.len());
    let mut i = 0;
    while i < toks.len() {
        match expression_alter_at(toks, i).or_else(|| identity_alter_at(toks, i)) {
            Some((last, replacement)) => {
                out.extend(replacement);
                i = last + 1;
            }
            None => {
                out.push(toks[i].clone());
                i += 1;
            }
        }
    }
    out
}

/// For `ALTER [COLUMN] c SET EXPRESSION AS`, `ALTER [COLUMN] c DROP EXPRESSION [IF EXISTS]` or
/// `ALTER [COLUMN] c SET STATISTICS n` with `ALTER` at token `i`: the index of the last token the
/// rewrite replaces, and the replacement. The parenthesized expression after `AS` (and the number
/// of `SET STATISTICS`) stays in the token stream.
fn expression_alter_at(toks: &[TokenWithSpan], i: usize) -> Option<(usize, Vec<TokenWithSpan>)> {
    let word = |k: usize, w: &str| bare_word(&toks[k].token).as_deref() == Some(w);
    if !word(i, "ALTER") {
        return None;
    }
    let mut j = next_significant(toks, i + 1)?;
    if word(j, "COLUMN") {
        j = next_significant(toks, j + 1)?;
    }
    if !matches!(toks[j].token, Token::Word(_)) {
        return None;
    }
    let verb = next_significant(toks, j + 1)?;
    let object = next_significant(toks, verb + 1)?;
    let span = toks[verb].span;
    let marker = |name: &str| TokenWithSpan::new(Token::Word(Word { value: name.to_string(), quote_style: Some('"'), keyword: Keyword::NoKeyword }), span);
    // `ALTER [COLUMN] c ` stays; only the subcommand from the verb on is rewritten.
    let prefixed = |rest: Vec<TokenWithSpan>| -> Vec<TokenWithSpan> {
        let mut out: Vec<TokenWithSpan> = toks[i..verb].to_vec();
        out.extend(rest);
        out
    };
    if word(verb, "SET") && word(object, "STATISTICS") {
        // `SET STATISTICS SignedIconst`: an optional sign, then the number. Both stay in the call.
        let first = next_significant(toks, object + 1)?;
        let last = if matches!(toks[first].token, Token::Minus) { next_significant(toks, first + 1)? } else { first };
        if !matches!(toks[last].token, Token::Number(_, _)) {
            return None;
        }
        let mut replacement = vec![
            TokenWithSpan::new(Token::make_keyword("SET"), span),
            TokenWithSpan::new(Token::make_keyword("DEFAULT"), span),
            marker(SET_STATISTICS_MARKER),
            TokenWithSpan::new(Token::LParen, span),
        ];
        replacement.extend(toks[first..=last].iter().cloned());
        replacement.push(TokenWithSpan::new(Token::RParen, span));
        return Some((last, prefixed(replacement)));
    }
    if word(verb, "SET") && word(object, "EXPRESSION") {
        let as_kw = next_significant(toks, object + 1)?;
        if !word(as_kw, "AS") {
            return None;
        }
        let replacement = vec![TokenWithSpan::new(Token::make_keyword("SET"), span), TokenWithSpan::new(Token::make_keyword("DEFAULT"), span), marker(SET_EXPRESSION_MARKER)];
        return Some((as_kw, prefixed(replacement)));
    }
    if word(verb, "DROP") && word(object, "EXPRESSION") {
        let mut last = object;
        let mut name = DROP_EXPRESSION_MARKER;
        if let Some(if_kw) = next_significant(toks, object + 1).filter(|&k| word(k, "IF")) {
            if let Some(exists) = next_significant(toks, if_kw + 1).filter(|&k| word(k, "EXISTS")) {
                last = exists;
                name = DROP_EXPRESSION_IF_EXISTS_MARKER;
            }
        }
        let replacement = vec![
            TokenWithSpan::new(Token::make_keyword("SET"), span),
            TokenWithSpan::new(Token::make_keyword("DEFAULT"), span),
            marker(name),
            TokenWithSpan::new(Token::LParen, span),
            TokenWithSpan::new(Token::RParen, span),
        ];
        return Some((last, prefixed(replacement)));
    }
    None
}

/// The index of the next significant token after `from` when it is the word `w` (else `from`).
fn opt_word(toks: &[TokenWithSpan], from: usize, w: &str) -> usize {
    match next_significant(toks, from + 1) {
        Some(n) if bare_word(&toks[n].token).as_deref() == Some(w) => n,
        _ => from,
    }
}

/// The index of the next significant token after `from`, when it is the number `[+|-]n` (its first
/// and last token indexes).
fn number_span(toks: &[TokenWithSpan], from: usize) -> Option<(usize, usize)> {
    let first = next_significant(toks, from)?;
    let last = if matches!(toks[first].token, Token::Minus | Token::Plus) { next_significant(toks, first + 1)? } else { first };
    matches!(toks[last].token, Token::Number(_, _)).then_some((first, last))
}

/// One identity item of `ALTER COLUMN c` (PG `alter_identity_column_option`) starting at token `k`:
/// `SET GENERATED {ALWAYS | BY DEFAULT}`, `SET <sequence option>` (`INCREMENT [BY] n`, `START [WITH]
/// n`, `MINVALUE n | NO MINVALUE`, `MAXVALUE n | NO MAXVALUE`, `CACHE n`, `[NO] CYCLE`) or
/// `RESTART [[WITH] n]`. Returns the index of the item's last token and its call
/// `<IDENTITY_OPTION_PREFIX><key>(value?)`. `None` when the token does not start an item.
fn identity_item_at(toks: &[TokenWithSpan], k: usize) -> Option<(usize, Vec<TokenWithSpan>)> {
    let word = |k: usize, w: &str| bare_word(&toks[k].token).as_deref() == Some(w);
    let set = word(k, "SET");
    if !set && !word(k, "RESTART") {
        return None;
    }
    let head = if set { next_significant(toks, k + 1)? } else { k };
    // `last` is the item's last keyword token; `value` is the number that follows it, if the item takes one.
    let (key, last, value): (&str, usize, Option<(usize, usize)>) = if !set {
        // `RESTART [WITH] [n]`: the number is optional, and `WITH` must be followed by it.
        let last = opt_word(toks, head, "WITH");
        match number_span(toks, last + 1) {
            Some(span) => ("restart", span.1, Some(span)),
            None if last == head => ("restart", head, None),
            None => return None,
        }
    } else {
        match bare_word(&toks[head].token)?.as_str() {
            "GENERATED" => {
                let kind = next_significant(toks, head + 1)?;
                if word(kind, "ALWAYS") {
                    ("always", kind, None)
                } else if word(kind, "BY") {
                    let default = next_significant(toks, kind + 1)?;
                    if !word(default, "DEFAULT") {
                        return None;
                    }
                    ("by default", default, None)
                } else {
                    return None;
                }
            }
            "INCREMENT" | "START" => {
                let key = if word(head, "INCREMENT") { "increment" } else { "start" };
                let last = opt_word(toks, head, if key == "increment" { "BY" } else { "WITH" });
                (key, last, Some(number_span(toks, last + 1)?))
            }
            "MINVALUE" | "MAXVALUE" | "CACHE" => {
                let key = match bare_word(&toks[head].token)?.as_str() {
                    "MINVALUE" => "minvalue",
                    "MAXVALUE" => "maxvalue",
                    _ => "cache",
                };
                (key, head, Some(number_span(toks, head + 1)?))
            }
            "CYCLE" => ("cycle", head, None),
            "NO" => {
                let what = next_significant(toks, head + 1)?;
                match bare_word(&toks[what].token)?.as_str() {
                    "CYCLE" => ("no cycle", what, None),
                    "MINVALUE" => ("no minvalue", what, None),
                    "MAXVALUE" => ("no maxvalue", what, None),
                    _ => return None,
                }
            }
            _ => return None,
        }
    };
    let span = toks[k].span;
    let mut call = vec![
        TokenWithSpan::new(Token::Word(Word { value: format!("{IDENTITY_OPTION_PREFIX}{key}"), quote_style: Some('"'), keyword: Keyword::NoKeyword }), span),
        TokenWithSpan::new(Token::LParen, span),
    ];
    if let Some((first, last_number)) = value {
        call.extend(toks[first..=last_number].iter().cloned());
    }
    call.push(TokenWithSpan::new(Token::RParen, span));
    Some((value.map_or(last, |(_, end)| end), call))
}

/// For `ALTER [COLUMN] c SET GENERATED ...`, `ALTER [COLUMN] c SET <sequence option>` and `ALTER
/// [COLUMN] c RESTART` (one or more items, see [`identity_item_at`]) with `ALTER` at token `i`: the
/// index of the last token the rewrite replaces, and the replacement. The replacement keeps
/// `ALTER [COLUMN] c` and writes `SET DEFAULT <IDENTITY_ALTER_MARKER>(<item calls>)`.
fn identity_alter_at(toks: &[TokenWithSpan], i: usize) -> Option<(usize, Vec<TokenWithSpan>)> {
    let word = |k: usize, w: &str| bare_word(&toks[k].token).as_deref() == Some(w);
    if !word(i, "ALTER") {
        return None;
    }
    let mut j = next_significant(toks, i + 1)?;
    if word(j, "COLUMN") {
        j = next_significant(toks, j + 1)?;
    }
    if !matches!(toks[j].token, Token::Word(_)) {
        return None;
    }
    let mut last = j;
    let mut items: Vec<Vec<TokenWithSpan>> = Vec::new();
    while let Some(next) = next_significant(toks, last + 1) {
        match identity_item_at(toks, next) {
            Some((end, call)) => {
                items.push(call);
                last = end;
            }
            None => break,
        }
    }
    if items.is_empty() {
        return None;
    }
    let span = toks[i].span;
    let mut replacement: Vec<TokenWithSpan> = toks[i..=j].to_vec();
    replacement.push(TokenWithSpan::new(Token::make_keyword("SET"), span));
    replacement.push(TokenWithSpan::new(Token::make_keyword("DEFAULT"), span));
    replacement.push(TokenWithSpan::new(Token::Word(Word { value: IDENTITY_ALTER_MARKER.to_string(), quote_style: Some('"'), keyword: Keyword::NoKeyword }), span));
    replacement.push(TokenWithSpan::new(Token::LParen, span));
    for (n, call) in items.into_iter().enumerate() {
        if n > 0 {
            replacement.push(TokenWithSpan::new(Token::Comma, span));
        }
        replacement.extend(call);
    }
    replacement.push(TokenWithSpan::new(Token::RParen, span));
    Some((last, replacement))
}

/// Index of the `SET` keyword of a `CREATE [OR REPLACE] PROCEDURE` clause (PG `opt_createproc_opt_list`,
/// `SET configuration_parameter`). The clause follows the parameter list and comes before the body
/// (`AS` or `BEGIN ATOMIC`). sqlparser 0.63 has no such clause, and PGArbor does not apply it on CALL
/// yet (lease SQL-37), so the caller reports it as 0A000.
fn procedure_set_clause(toks: &[TokenWithSpan]) -> Option<usize> {
    let sig: Vec<usize> = (0..toks.len()).filter(|&i| !matches!(toks[i].token, Token::Whitespace(_))).collect();
    let head: Vec<String> = sig.iter().take(4).filter_map(|&i| bare_word(&toks[i].token)).collect();
    let is_procedure = matches!(head.as_slice(), [a, b, ..] if a == "CREATE" && b == "PROCEDURE")
        || matches!(head.as_slice(), [a, b, c, d, ..] if a == "CREATE" && b == "OR" && c == "REPLACE" && d == "PROCEDURE");
    if !is_procedure {
        return None;
    }
    let open = sig.iter().position(|&i| matches!(toks[i].token, Token::LParen))?;
    let mut depth = 0usize;
    for &i in &sig[open..] {
        match toks[i].token {
            Token::LParen => depth += 1,
            Token::RParen => depth = depth.saturating_sub(1),
            _ if depth == 0 => match bare_word(&toks[i].token).as_deref() {
                Some("SET") => return Some(i),
                Some("AS" | "BEGIN") => return None,
                _ => {}
            },
            _ => {}
        }
    }
    None
}

/// `GRANT` or `REVOKE` of a privilege on a configuration parameter (`ON PARAMETER name TO|FROM role`,
/// PG 15+). sqlparser has no such object kind, and PGArbor keeps no `pg_parameter_acl` yet (lease
/// SQL-39). A table named `parameter` (`ON parameter TO role`) does not match: the word after the name
/// is not an identifier there.
fn parameter_privilege_statement(toks: &[TokenWithSpan]) -> bool {
    let words: Vec<String> = toks
        .iter()
        .filter(|t| !matches!(t.token, Token::Whitespace(_)))
        .map(|t| match &t.token {
            Token::Word(w) if w.quote_style.is_none() => w.value.to_ascii_uppercase(),
            Token::Word(_) => String::from("(quoted)"),
            _ => String::new(),
        })
        .collect();
    matches!(words.first().map(String::as_str), Some("GRANT" | "REVOKE"))
        && words.windows(3).any(|w| w[0] == "ON" && w[1] == "PARAMETER" && !matches!(w[2].as_str(), "TO" | "FROM"))
}

/// `WHERE CURRENT OF cursor` (UPDATE or DELETE positioned on a cursor) is PostgreSQL grammar that
/// sqlparser 0.63 does not read and PGArbor does not execute yet (lease SQL-28), so it fails here
/// with its own 0A000 instead of a syntax error.
fn reject_where_current_of(toks: &[TokenWithSpan]) -> Result<()> {
    let word = |k: usize, w: &str| bare_word(&toks[k].token).as_deref() == Some(w);
    for i in 0..toks.len() {
        if !word(i, "WHERE") {
            continue;
        }
        let Some(current) = next_significant(toks, i + 1) else { continue };
        let Some(of) = next_significant(toks, current + 1) else { continue };
        if word(current, "CURRENT") && word(of, "OF") {
            return Err(PgError::unimplemented("SQL-28", "WHERE CURRENT OF cursor"));
        }
    }
    Ok(())
}

/// PostgreSQL takes no indirection after an `ARRAY[..]` constructor (gram.y `ARRAY array_expr` is a `c_expr`
/// without `opt_indirection`), so `ARRAY[1,2][1]` is a syntax error at the `[`. sqlparser reads the subscript
/// onto any expression, so it is rejected here.
fn reject_array_constructor_subscript(toks: &[TokenWithSpan], src: &SourceMap) -> Result<()> {
    for i in 0..toks.len() {
        if bare_word(&toks[i].token).as_deref() != Some("ARRAY") {
            continue;
        }
        let Some(open) = next_significant(toks, i + 1) else { continue };
        if !matches!(toks[open].token, Token::LBracket) {
            continue;
        }
        let Some(close) = bracket_close(toks, open) else { continue };
        let Some(next) = next_significant(toks, close + 1) else { continue };
        if matches!(toks[next].token, Token::LBracket) {
            return Err(diag::syntax_error_near("[", src.char_pos(toks[next].span.start)));
        }
    }
    Ok(())
}

/// `UNIQUE [NULLS [NOT] DISTINCT] [(cols)] [INCLUDE (cols)] WITH (storage parameters)` and the same clause
/// after `PRIMARY KEY` (PG gram.y `ConstraintElem`: `opt_definition` sets the backing index's reloptions).
/// sqlparser's unique and primary key constraints carry only `USING` and `COMMENT` index options, so the
/// clause is rejected here as 0A000 at its `WITH` until the constraint AST can carry reloptions.
/// `CREATE UNIQUE INDEX ... WITH (...)` is not affected: `INDEX` follows `UNIQUE` there.
fn reject_constraint_storage_params(toks: &[TokenWithSpan], src: &SourceMap) -> Result<()> {
    let sig: Vec<usize> = (0..toks.len()).filter(|&i| !matches!(toks[i].token, Token::Whitespace(_) | Token::EOF)).collect();
    let word = |k: usize| sig.get(k).and_then(|&i| bare_word(&toks[i].token));
    let is_lparen = |k: usize| sig.get(k).is_some_and(|&i| matches!(toks[i].token, Token::LParen));
    for k in 0..sig.len() {
        let mut j = match word(k).as_deref() {
            Some("UNIQUE") => k + 1,
            Some("PRIMARY") if word(k + 1).as_deref() == Some("KEY") => k + 2,
            _ => continue,
        };
        while matches!(word(j).as_deref(), Some("NULLS" | "NOT" | "DISTINCT")) {
            j += 1;
        }
        if is_lparen(j) {
            j = after_group(toks, &sig, j);
        }
        if word(j).as_deref() == Some("INCLUDE") && is_lparen(j + 1) {
            j = after_group(toks, &sig, j + 1);
        }
        if word(j).as_deref() == Some("WITH") && is_lparen(j + 1) {
            let err = PgError::unimplemented("SQL-01", "storage parameters on a UNIQUE or PRIMARY KEY constraint");
            return Err(match src.char_pos(toks[sig[j]].span.start) {
                Some(pos) => err.with_position(pos),
                None => err,
            });
        }
    }
    Ok(())
}

/// Significant index just after the `)` that closes the `(` at significant index `open`; `open` itself
/// when the parentheses never balance.
fn after_group(toks: &[TokenWithSpan], sig: &[usize], open: usize) -> usize {
    let mut depth = 0usize;
    for k in open..sig.len() {
        match toks[sig[k]].token {
            Token::LParen => depth += 1,
            Token::RParen => {
                depth = depth.saturating_sub(1);
                if depth == 0 {
                    return k + 1;
                }
            }
            _ => {}
        }
    }
    open
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
    // Parenthesis depth of the current chunk. A `CREATE [OR REPLACE] RULE` action list
    // `DO ALSO ( stmt; stmt )` keeps its semicolons inside parentheses, so they do not split.
    let mut paren_depth = 0usize;
    for (i, t) in tokens.iter().enumerate() {
        let rule_head: Vec<&str> = head.iter().map(String::as_str).collect();
        let rule_chunk = matches!(rule_head.as_slice(), ["CREATE", "RULE", ..] | ["CREATE", "OR", "REPLACE", "RULE", ..]);
        match &t.token {
            Token::EOF => {
                end = i;
                break;
            }
            Token::Whitespace(_) => continue,
            Token::SemiColon if !atomic_open && !(rule_chunk && paren_depth > 0) => {
                if let Some(r) = trim_range(tokens, start, i) {
                    out.push(r);
                }
                start = i + 1;
                head.clear();
                prev_begin = false;
                case_depth = 0;
                paren_depth = 0;
                continue;
            }
            Token::LParen => paren_depth += 1,
            Token::RParen => paren_depth = paren_depth.saturating_sub(1),
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

/// Joins `*` with a directly following `=`, `<>`, `<`, `<=`, `>` or `>=` into one operator token. PostgreSQL's
/// lexer reads `*=`, `*<>`, `*<`, `*<=`, `*>`, `*>=` (the record binary comparisons, `catalog_data::operators`)
/// as one operator each, but sqlparser's `*` never starts an operator name, so the run splits into two tokens.
/// Only these exact spellings are joined: `2 * -1`, `count(*)` and `t.*` keep their tokens.
fn join_composite_compare_ops(toks: Vec<TokenWithSpan>) -> Vec<TokenWithSpan> {
    let mut out: Vec<TokenWithSpan> = Vec::with_capacity(toks.len());
    let mut i = 0;
    while i < toks.len() {
        if matches!(toks[i].token, Token::Mul) {
            if let Some(next) = toks.get(i + 1) {
                let suffix = match next.token {
                    Token::Eq => Some("="),
                    Token::Neq => Some("<>"),
                    Token::Lt => Some("<"),
                    Token::LtEq => Some("<="),
                    Token::Gt => Some(">"),
                    Token::GtEq => Some(">="),
                    _ => None,
                };
                if let (Some(suffix), true) = (suffix, next.span.start == toks[i].span.end) {
                    let span = Span::new(toks[i].span.start, next.span.end);
                    out.push(TokenWithSpan::new(Token::CustomBinaryOperator(format!("*{suffix}")), span));
                    i += 2;
                    continue;
                }
            }
        }
        out.push(toks[i].clone());
        i += 1;
    }
    out
}

/// Rewrites PostgreSQL's typed literal `type_name 'text'` (gram.y `ConstTypename Sconst`, where the type is any
/// identifier) into `CAST('text' AS type_name)`. sqlparser reads `type 'text'` only for its built-in type
/// keywords; for any other word it takes the word as a column name, so `inet '10.0.0.1' = x` fails at `=`.
/// Only plain words qualify (quoted, not a keyword, or `name`), so a clause keyword such as `SELECT 'a'` or
/// `AND 'b'` never becomes a type. Used only when the chunk fails to parse as written ([`parse_ast_chunk`]).
/// `None` when the tokens hold no such pair.
fn typed_literal_as_cast(toks: &[TokenWithSpan]) -> Option<Vec<TokenWithSpan>> {
    let mut out: Vec<TokenWithSpan> = Vec::with_capacity(toks.len() + 4);
    let mut changed = false;
    let mut i = 0;
    while i < toks.len() {
        if let Token::Word(w) = &toks[i].token {
            let plain = w.quote_style.is_some() || matches!(w.keyword, Keyword::NoKeyword | Keyword::NAME);
            if let (true, Some(j)) = (plain, next_significant(toks, i + 1)) {
                if matches!(toks[j].token, Token::SingleQuotedString(_)) {
                    out.push(TokenWithSpan::new(Token::make_keyword("CAST"), toks[i].span));
                    out.push(TokenWithSpan::new(Token::LParen, toks[i].span));
                    out.push(toks[j].clone());
                    out.push(TokenWithSpan::new(Token::make_keyword("AS"), toks[j].span));
                    out.push(toks[i].clone());
                    out.push(TokenWithSpan::new(Token::RParen, toks[j].span));
                    changed = true;
                    i = j + 1;
                    continue;
                }
            }
        }
        out.push(toks[i].clone());
        i += 1;
    }
    changed.then_some(out)
}

/// Parse one chunk (tokens end with EOF) with sqlparser; the whole chunk must be one statement. A chunk that
/// fails as written is retried with its typed literals rewritten ([`typed_literal_as_cast`]); the retry is used
/// only when it parses, otherwise the original error is returned.
fn parse_ast_chunk(toks: &[TokenWithSpan], src: &SourceMap) -> Result<Statement> {
    let err = match parse_ast_tokens(toks, src) {
        Ok(stmt) => return Ok(stmt),
        Err(err) => err,
    };
    let Some(rewritten) = typed_literal_as_cast(toks) else {
        return Err(err);
    };
    parse_ast_tokens(&rewritten, src).or(Err(err))
}

/// The sqlparser parse of one chunk (see [`parse_ast_chunk`]).
fn parse_ast_tokens(toks: &[TokenWithSpan], src: &SourceMap) -> Result<Statement> {
    let (toks, ctas_names) = match ctas_column_name_list(toks) {
        Some((from, to, names)) => {
            let kept: Vec<TokenWithSpan> = toks[..from].iter().chain(&toks[to + 1..]).cloned().collect();
            (kept, Some(names))
        }
        None => (toks.to_vec(), None),
    };
    let toks = signed_connection_limit(&toks);
    let toks = mark_subscript_targets(&toks);
    let toks = mark_conflict_expressions(&toks, src)?;
    let toks = mark_variadic_args(&toks);
    let toks = unnamed_procedure_params(&toks);
    let toks = bare_view_options(&toks);
    let toks = alter_procedure_as_function(&toks);
    let toks = drop_routine_as_function(&toks);
    let toks = routine_grant_as_function(&toks);
    let toks = collation_for_as_function(&toks);
    let toks = drop_domain_list_as_type(&toks);
    if let Some(at) = procedure_set_clause(&toks) {
        let err = PgError::unimplemented("SQL-37", "CREATE PROCEDURE with a SET clause");
        return Err(match src.char_pos(toks[at].span.start) {
            Some(pos) => err.with_position(pos),
            None => err,
        });
    }
    if let Some(at) = begin_atomic_body(&toks) {
        let err = PgError::unimplemented("SQL-37", "BEGIN ATOMIC function and procedure bodies");
        return Err(match src.char_pos(toks[at].span.start) {
            Some(pos) => err.with_position(pos),
            None => err,
        });
    }
    let toks = create_procedure_as_function(&toks);
    let toks = quoted_language_as_identifier(&toks);
    if parameter_privilege_statement(&toks) {
        return Err(PgError::unimplemented("SQL-39", "GRANT or REVOKE on a configuration parameter"));
    }
    let toks = json_table_default_paths(&toks);
    let toks = mark_expression_alters(&toks);
    reject_where_current_of(&toks)?;
    reject_array_constructor_subscript(&toks, src)?;
    reject_constraint_storage_params(&toks, src)?;
    let toks = values_quantified_operand(&toks);
    let (toks, virtual_flags) = mark_virtual_generated(&toks);
    let table_select = table_statement_as_select(&toks);
    let toks = table_select.as_deref().unwrap_or(toks.as_slice());
    let dialect = PostgreSqlDialect {};
    let mut parser = Parser::new(&dialect)
        .with_recursion_limit(RECURSION_LIMIT)
        .with_tokens_with_locations(toks.to_vec());
    let head: Vec<_> = toks.iter().filter_map(|t| bare_word(&t.token)).take(3).collect();
    let is_sequence = matches!(head.as_slice(), [a,b,..] if a == "CREATE" && b == "SEQUENCE")
        || matches!(head.as_slice(), [a,b,c] if a == "CREATE" && matches!(b.as_str(), "TEMP" | "TEMPORARY") && c == "SEQUENCE");
    let parsed = if is_sequence { parse_sequence_flexible(&mut parser) } else { parser.parse_statement() };
    let mut stmt = match parsed {
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
    if let Some(names) = ctas_names {
        attach_ctas_column_names(&mut stmt, names);
    }
    restore_virtual_generated(&mut stmt, &virtual_flags)?;
    Ok(stmt)
}

// sqlparser 0.63 fixes SeqOptList order; PG gram.y permits any order and repetitions
// (repetitions are rejected by binding, before any catalog changes).
fn parse_sequence_flexible(p: &mut Parser<'_>) -> std::result::Result<Statement, sqlparser::parser::ParserError> {
    use sqlparser::keywords::Keyword as K;
    use sqlparser::ast::SequenceOptions as O;
    p.expect_keyword_is(K::CREATE)?;
    let temporary = p.parse_keyword(K::TEMP) || p.parse_keyword(K::TEMPORARY);
    p.expect_keyword_is(K::SEQUENCE)?;
    let if_not_exists = p.parse_keywords(&[K::IF,K::NOT,K::EXISTS]);
    let name=p.parse_object_name(false)?;
    let mut data_type=None;let mut owned_by=None;let mut sequence_options=Vec::new();
    loop {
        if p.parse_keyword(K::AS) {
            if data_type.is_some() { return Err(sqlparser::parser::ParserError::ParserError("conflicting or redundant options".into())); }
            data_type=Some(p.parse_data_type()?);
        } else if p.parse_keyword(K::INCREMENT) {
            let by=p.parse_keyword(K::BY);sequence_options.push(O::IncrementBy(p.parse_number()?,by));
        } else if p.parse_keyword(K::MINVALUE) {sequence_options.push(O::MinValue(Some(p.parse_number()?)));}
        else if p.parse_keyword(K::MAXVALUE) {sequence_options.push(O::MaxValue(Some(p.parse_number()?)));}
        else if p.parse_keyword(K::START) {let with=p.parse_keyword(K::WITH);sequence_options.push(O::StartWith(p.parse_number()?,with));}
        else if p.parse_keyword(K::CACHE) {sequence_options.push(O::Cache(p.parse_number()?));}
        else if p.parse_keyword(K::CYCLE) {sequence_options.push(O::Cycle(false));}
        else if p.parse_keyword(K::NO) {
            if p.parse_keyword(K::MINVALUE) {sequence_options.push(O::MinValue(None));}
            else if p.parse_keyword(K::MAXVALUE) {sequence_options.push(O::MaxValue(None));}
            else {p.expect_keyword_is(K::CYCLE)?;sequence_options.push(O::Cycle(true));}
        } else if p.parse_keyword(K::OWNED) {
            p.expect_keyword_is(K::BY)?;
            if owned_by.is_some() {return Err(sqlparser::parser::ParserError::ParserError("conflicting or redundant options".into()));}
            owned_by=Some(p.parse_object_name(false)?);
        } else {break;}
    }
    Ok(Statement::CreateSequence {temporary,if_not_exists,name,data_type,sequence_options,owned_by})
}
// The role parser's numeric-value path omits unary signs. Fold only the signed
// CONNECTION LIMIT literal; retain its original source span and every other token.
fn signed_connection_limit(toks: &[TokenWithSpan]) -> Vec<TokenWithSpan> {
    let mut out=Vec::with_capacity(toks.len());let mut i=0;
    let significant: Vec<_>=toks.iter().enumerate().filter(|(_,t)|!matches!(t.token,Token::Whitespace(_))).collect();
    while i<significant.len() {
        let (_,t)=significant[i];
        if i>=2 && bare_word(&significant[i-2].1.token).as_deref()==Some("CONNECTION")
            && bare_word(&significant[i-1].1.token).as_deref()==Some("LIMIT")
            && matches!(t.token,Token::Minus | Token::Plus) {
            if let Some((_,next))=significant.get(i+1) {
                if let Token::Number(n,long)=&next.token {
                    let sign=if matches!(t.token,Token::Minus) {"-"} else {""};
                    out.push(TokenWithSpan::new(Token::Number(format!("{sign}{n}"),*long),Span::new(t.span.start,next.span.end)));
                    i+=2;continue;
                }
            }
        }
        out.push(t.clone());i+=1;
    }
    out
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
    let tokens = Tokenizer::new(&dialect, sql).tokenize_with_location().map_err(|_| fail())?;
    let tokens = mark_variadic_args(&tokens);
    let mut parser = Parser::new(&dialect).with_tokens_with_locations(tokens);
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

    /// Indirected SET targets and INSERT column entries are marked; subscripts in values, query bodies and
    /// WHERE clauses are expressions and stay as they are.
    #[test]
    fn subscripted_targets_are_marked_only_in_target_lists() {
        let marked = |sql: &str| -> bool {
            let dialect = PostgreSqlDialect {};
            let toks = Tokenizer::new(&dialect, sql).tokenize_with_location().expect("tokenizes");
            mark_subscript_targets(&toks)
                .iter()
                .any(|t| matches!(&t.token, Token::Word(w) if w.value == SUBSCRIPT_TARGET_MARKER))
        };
        assert!(marked("update t set a[2] = 1, b[1:2][3] = 4 where k = 1"));
        assert!(marked("insert into t (k, b[1:3][1:3]) values (1, '{}')"));
        assert!(marked("insert into t (k, c.x) values (1, 5)"));
        assert!(marked("insert into t (k) values (1) on conflict (k) do update set b[1] = 2"));
        assert!(!marked("update t set c.x = 9 where a[1] = 2"));
        assert!(!marked("insert into t values (a[1])"));
        assert!(!marked("insert into t (select a[1] from u)"));
        assert!(!marked("select a[1], b[2:3] from t"));
    }

    /// `COLLATION FOR (expr)` becomes the call `pg_collation_for(expr)`; other `COLLATION` uses are untouched.
    #[test]
    fn collation_for_rewrites_to_function_name() {
        let lex = |sql: &str| {
            Tokenizer::new(&PostgreSqlDialect {}, sql).tokenize_with_location().expect("test SQL tokenizes")
        };
        let words = |toks: &[TokenWithSpan]| -> Vec<String> { toks.iter().filter_map(|t| bare_word(&t.token)).collect() };
        let out = collation_for_as_function(&lex("SELECT COLLATION FOR (1) AS c"));
        assert_eq!(words(&out), vec!["SELECT", "PG_COLLATION_FOR", "AS", "C"]);
        let plain = lex("SELECT collation_name FROM pg_collation");
        assert_eq!(collation_for_as_function(&plain).len(), plain.len());
    }

    /// Expression elements of an ON CONFLICT inference list parse; trailing index options are 0A000.
    #[test]
    fn conflict_expression_elements_parse() {
        assert!(parse("insert into t values (21, 'QUINCE') on conflict (lower(fruit)) do update set fruit = excluded.fruit").is_ok());
        assert!(parse("insert into t values (1, 2) on conflict ((a + 1), b) do nothing").is_ok());
        let err = parse("insert into t values (1) on conflict (a + 1 desc) do nothing").err().expect("rejected");
        assert_eq!(err.sqlstate, pgarbor_common::SqlState::FEATURE_NOT_SUPPORTED);
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
    fn view_trailing_clauses_are_side_channel() {
        let mat = parse("CREATE MATERIALIZED VIEW m AS SELECT 1 WITH NO DATA").expect("parses");
        assert!(matches!(mat[0].as_ast(), Some(Statement::CreateView { .. })));
        assert_eq!(mat[0].view_trailer.as_ref().and_then(|t| t.with_data), Some(false));
        let data = parse("create materialized view m as select 1 with data").expect("parses");
        assert_eq!(data[0].view_trailer.as_ref().and_then(|t| t.with_data), Some(true));
        let local = parse("CREATE VIEW v AS SELECT a FROM t WITH LOCAL CHECK OPTION").expect("parses");
        assert_eq!(local[0].view_trailer.as_ref().and_then(|t| t.check_option.as_deref()), Some("local"));
        let bare = parse("CREATE VIEW v AS SELECT a FROM t WHERE a > 0 WITH CHECK OPTION").expect("parses");
        assert_eq!(bare[0].view_trailer.as_ref().and_then(|t| t.check_option.as_deref()), Some("cascaded"));
        // PostgreSQL grammar: WITH [NO] DATA only after a materialized view's query, WITH CHECK OPTION only
        // after a plain view's query; anything else is a syntax error at the WITH.
        assert!(parse("CREATE VIEW v AS SELECT 1 WITH NO DATA").is_err());
        assert!(parse("CREATE MATERIALIZED VIEW m AS SELECT 1 WITH CHECK OPTION").is_err());
    }

    #[test]
    fn parse_virtual_generated_and_expression_alters() {
        assert!(parse_one("CREATE TABLE t (a int, b int GENERATED ALWAYS AS (a * 2) VIRTUAL)").is_ok());
        assert!(parse_one("ALTER TABLE t ALTER COLUMN b SET EXPRESSION AS (a * 3)").is_ok());
        assert!(parse_one("ALTER TABLE t ALTER b DROP EXPRESSION IF EXISTS").is_ok());
    }

    #[test]
    fn alter_identity_items_parse_as_marker_call() {
        let set = parse("ALTER TABLE t ALTER COLUMN id SET GENERATED ALWAYS").expect("parse");
        assert!(matches!(set[0].as_ast(), Some(Statement::AlterTable(_))));
        let items = parse("ALTER TABLE itest6 ALTER COLUMN a SET GENERATED BY DEFAULT SET INCREMENT BY 2 SET START WITH 100 RESTART");
        assert!(items.is_ok());
        assert!(parse("ALTER TABLE t ALTER COLUMN a RESTART WITH 5").is_ok());
        assert!(parse("ALTER TABLE t ALTER COLUMN a SET NO MINVALUE SET CYCLE").is_ok());
        // A bare `SET` of a non-identity item keeps its own parse path.
        assert!(parse("ALTER TABLE t ALTER COLUMN a SET NOT NULL").is_ok());
    }

    #[test]
    fn where_current_of_is_leased_0a000() {
        let err = parse_one("DELETE FROM t WHERE CURRENT OF c").err();
        assert!(err.is_some_and(|e| e.sqlstate == pgarbor_common::SqlState::FEATURE_NOT_SUPPORTED));
    }

    #[test]
    fn nested_array_literal_and_subscripted_set_target_parse() {
        assert!(parse_one("SELECT ARRAY[[1,2],[3,4]]").is_ok());
        assert!(parse_one("UPDATE sa SET jf['a'][0] = '1' WHERE id = 1").is_ok());
    }

    #[test]
    fn subscript_after_array_constructor_is_syntax_error_at_bracket() {
        let err = parse_one("SELECT ARRAY[10,20,30][4]").err();
        assert!(err.is_some_and(|e| e.sqlstate == pgarbor_common::SqlState::SYNTAX_ERROR && e.position == Some(23)));
        assert!(parse_one("SELECT (ARRAY[10,20,30])[2]").is_ok());
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

    #[test]
    fn quantified_values_operand_parses() {
        let stmts = parse("SELECT (1, 2) < ANY (VALUES (1, 3), (0, 9)) AS ra, (1, 2) = ALL (VALUES (1, NULL::int)) AS rd")
            .expect("parse");
        assert_eq!(stmts.len(), 1);
    }

    #[test]
    fn ctas_column_names_attach_as_untyped_columns() {
        let stmt = parse_one("CREATE TABLE ctas_ok (ii, jj) AS SELECT 1, 2").expect("parse");
        assert!(matches!(stmt.as_ast(), Some(Statement::CreateTable(ct)) if ct.columns.len() == 2));
    }

    #[test]
    fn generated_clause_kinds_parse() {
        let sql = "CREATE TABLE t (a int, v int GENERATED ALWAYS AS (a * 10) VIRTUAL, s int GENERATED ALWAYS AS (a) STORED, w int GENERATED ALWAYS AS (a) NOT NULL)";
        let stmts = parse(sql).expect("parse");
        let Some(Statement::CreateTable(ct)) = stmts[0].as_ast() else { panic!("expected CREATE TABLE") };
        let modes: Vec<_> = ct
            .columns
            .iter()
            .filter_map(|c| {
                c.options.iter().find_map(|o| match &o.option {
                    ColumnOption::Generated { generation_expr_mode, .. } => Some(*generation_expr_mode),
                    _ => None,
                })
            })
            .collect();
        assert_eq!(
            modes,
            vec![
                Some(GeneratedExpressionMode::Virtual),
                Some(GeneratedExpressionMode::Stored),
                Some(GeneratedExpressionMode::Virtual),
            ]
        );
        let alter = parse("ALTER TABLE t ADD COLUMN b int GENERATED ALWAYS AS (a * 10) VIRTUAL NOT NULL").expect("parse");
        assert!(matches!(alter[0].as_ast(), Some(Statement::AlterTable(_))));
    }

    #[test]
    fn table_statement_parses_as_query() {
        let stmts = parse("TABLE gtestc ORDER BY f1").expect("parse");
        assert!(matches!(stmts[0].as_ast(), Some(Statement::Query(_))));
    }

    #[test]
    fn operator_names_with_repeated_symbols_parse() {
        // PostgreSQL reads `===` and `====` as one operator name; these reach the support check, not 42601.
        let e = parse_one("ALTER OPERATOR === (boolean, real) SET (MERGES = false)").err().expect("unsupported");
        assert_eq!(e.sqlstate.code(), "0A000");
        assert!(e.message.starts_with("ALTER OPERATOR is not supported"));
        let e = parse_one("CREATE OPERATOR === (LEFTARG = boolean, RIGHTARG = real, PROCEDURE = f, COMMUTATOR = ====)")
            .err()
            .expect("unsupported");
        assert_eq!(e.sqlstate.code(), "0A000");
        assert!(e.message.starts_with("CREATE OPERATOR is not supported"));
        let e = parse_one("ALTER OPERATOR === (boolean, real) SET (MERGES = )").err().expect("syntax error");
        assert_eq!(e.sqlstate.code(), "42601");
        // `NONE` resets RESTRICT and JOIN; a quoted attribute name is case-sensitive and not recognized (42601).
        let e = parse_one("ALTER OPERATOR === (boolean, boolean) SET (RESTRICT = NONE, JOIN = NONE)").err().expect("unsupported");
        assert_eq!(e.sqlstate.code(), "0A000");
        let e = parse_one("ALTER OPERATOR === (boolean, boolean) SET (\"Restrict\" = contsel)").err().expect("syntax error");
        assert_eq!(e.sqlstate.code(), "42601");
        // Outside operator DDL the tokens are untouched: `= -1` stays an expression.
        assert!(parse("SELECT 1 WHERE 2 = -1").is_ok());
    }

    #[test]
    fn typed_literals_of_custom_types_and_composite_compare_ops_parse() {
        // `inet '...'` and `name '...'` are typed literals (gram.y ConstTypename Sconst), not column aliases.
        assert!(parse("SELECT inet '10.0.0.1' = cidr '10.0.0.1/32' AS a, name 'x' = name 'x ' AS b").is_ok());
        // `*=` and `*<>` are single record comparison operators.
        assert!(parse("SELECT ROW(1, 2)::bin_pair *= ROW(1, 2)::bin_pair AS s, ROW(1, 2)::bin_pair *<> ROW(1, 3)::bin_pair AS d").is_ok());
        // A plain alias after an expression is still a syntax error, and `*` stays multiplication.
        assert!(parse("SELECT 1 foo 'bar'").is_err());
        assert!(parse("SELECT 2 * -1, count(*) FROM t").is_ok());
    }

    #[test]
    fn operator_runs_in_expressions_parse() {
        // A split run after an operand is one binary operator; inside OPERATOR( ... ) it is one name part.
        let q = parse_one("SELECT true === NULL::boolean, ('((0,0),(2,2))'::box) !== ('((0,0),(1,1))'::box)").expect("parse");
        assert!(matches!(q.as_ast(), Some(Statement::Query(_))));
        assert!(parse("SELECT 2::int8 OPERATOR(cmd_ao_dst2.===) 2::int8").is_ok());
        assert!(parse("SELECT 1 WHERE 2=-1").is_ok());
    }

    #[test]
    fn alter_procedure_parses_as_alter_function_and_unnamed_params_parse() {
        let alter = parse("ALTER PROCEDURE p(integer, x double precision) RENAME TO q").expect("parse");
        assert!(matches!(alter[0].as_ast(), Some(Statement::AlterFunction(_))));
        assert!(is_alter_procedure_text("  alter /* c */ procedure p(integer)"));
        assert!(!is_alter_procedure_text("ALTER FUNCTION p(integer) RENAME TO q"));
    }

    #[test]
    fn alter_column_set_statistics_parses_as_marker_call() {
        let stmt = parse("ALTER TABLE an_st ALTER COLUMN b SET STATISTICS -1").expect("parse");
        assert!(matches!(stmt[0].as_ast(), Some(Statement::AlterTable(_))));
        let zero = parse("ALTER TABLE an_st ALTER b SET STATISTICS 0").expect("parse");
        assert!(matches!(zero[0].as_ast(), Some(Statement::AlterTable(_))));
        assert!(parse("ALTER TABLE an_st ALTER b SET STATISTICS").is_err());
    }

    #[test]
    fn create_procedure_set_clause_and_parameter_grant_are_leased_0a000() {
        let set = parse_one("CREATE PROCEDURE wm(INOUT v text) LANGUAGE plpgsql SET work_mem = '1MB' AS $$ BEGIN END $$").err();
        assert!(set.is_some_and(|e| e.sqlstate == pgarbor_common::SqlState::FEATURE_NOT_SUPPORTED));
        // A SET inside a BEGIN ATOMIC body is an UPDATE clause, not a procedure SET clause.
        assert!(!parse_one("CREATE PROCEDURE pa() BEGIN ATOMIC UPDATE t SET x = 1; END").err().is_some_and(|e| e.message.contains("SET clause")));
        let grant = parse_one("GRANT SET ON PARAMETER work_mem TO dpd_g").err();
        assert!(grant.is_some_and(|e| e.sqlstate == pgarbor_common::SqlState::FEATURE_NOT_SUPPORTED));
        // A table named parameter is not a parameter privilege.
        let table = parse_one("GRANT SELECT ON parameter TO dpd_g").err();
        assert!(!table.is_some_and(|e| e.sqlstate == pgarbor_common::SqlState::FEATURE_NOT_SUPPORTED));
    }

    #[test]
    fn alter_routine_parses_as_alter_function_for_any_routine_kind() {
        let alter = parse("ALTER ROUTINE foo(integer) STABLE PARALLEL SAFE SECURITY DEFINER").expect("parse");
        assert!(matches!(alter[0].as_ast(), Some(Statement::AlterFunction(_))));
        assert!(is_alter_routine_text("  alter /* c */ routine foo"));
        assert!(!is_alter_routine_text("ALTER PROCEDURE foo RENAME TO bar"));
        assert!(!is_alter_procedure_text("ALTER ROUTINE foo RENAME TO bar"));
    }

    #[test]
    fn unnamed_int_procedure_param_parses() {
        let create = parse("CREATE PROCEDURE pdup(int) LANGUAGE sql AS 'SELECT 1'").expect("parse");
        assert!(matches!(create[0].as_ast(), Some(Statement::CreateFunction(_))));
        assert!(is_create_procedure_text("  create /* c */ or replace procedure pdup(int)"));
        assert!(!is_create_procedure_text("CREATE FUNCTION pdup(int) LANGUAGE sql AS 'SELECT 1'"));
        let drop = parse("DROP PROCEDURE pdup(int), pdup(int)").expect("parse");
        assert!(matches!(drop[0].as_ast(), Some(Statement::DropProcedure { .. })));
    }

    #[test]
    fn drop_routine_parses_as_drop_function() {
        let drop = parse("DROP ROUTINE am(IN text, IN text) CASCADE").expect("parse");
        assert!(matches!(drop[0].as_ast(), Some(Statement::DropFunction(_))));
        assert!(is_drop_routine_text("  drop /* c */ routine am"));
        assert!(!is_drop_routine_text("DROP FUNCTION am"));
        assert!(!is_drop_routine_text("ALTER ROUTINE am RENAME TO bm"));
    }

    #[test]
    fn create_procedure_string_body_and_quoted_language_parse() {
        let create = parse("CREATE PROCEDURE lq() LANGUAGE 'SQL' AS 'SELECT 1'").expect("parse");
        assert!(matches!(
            create[0].as_ast(),
            Some(Statement::CreateFunction(cf)) if cf.language.as_ref().is_some_and(|l| l.value == "SQL" && l.quote_style == Some('"'))
        ));
        let dollar = parse("CREATE PROCEDURE lp() LANGUAGE plpgsql AS $$ BEGIN NULL; END $$").expect("parse");
        assert!(matches!(dollar[0].as_ast(), Some(Statement::CreateFunction(_))));
        let atomic = parse_one("CREATE PROCEDURE ab() LANGUAGE sql BEGIN ATOMIC SELECT 1; END").err().expect("error");
        assert_eq!(atomic.sqlstate, pgarbor_common::SqlState::FEATURE_NOT_SUPPORTED);
    }

    #[test]
    fn constraint_storage_parameters_are_0a000_at_with() {
        let err = parse("CREATE TABLE t (a int, UNIQUE (a) WITH (fillfactor=70))").err().expect("error");
        assert_eq!(err.message, "storage parameters on a UNIQUE or PRIMARY KEY constraint is not supported by PGArbor yet");
        assert!(parse("CREATE UNIQUE INDEX i ON t (a) WITH (fillfactor=70)").is_ok());
    }

    #[test]
    fn bare_view_option_name_parses_as_true_option() {
        let view = parse("CREATE VIEW sbx WITH (security_barrier, check_option = local) AS SELECT 1").expect("parse");
        assert!(matches!(view[0].as_ast(), Some(Statement::CreateView { .. })));
        let text = Tokenizer::new(&PostgreSqlDialect {}, "CREATE VIEW v WITH (security_barrier) AS SELECT 1")
            .tokenize_with_location()
            .expect("tokenize");
        let out = bare_view_options(&text);
        let eq = out.iter().filter(|t| matches!(t.token, Token::Eq)).count();
        assert_eq!(eq, 1);
        let untouched = Tokenizer::new(&PostgreSqlDialect {}, "CREATE TABLE t (a int) WITH (fillfactor)")
            .tokenize_with_location()
            .expect("tokenize");
        assert_eq!(bare_view_options(&untouched).len(), untouched.len());
    }

    #[test]
    fn unnamed_procedure_params_get_marker_only_when_type_only() {
        let toks = Tokenizer::new(&PostgreSqlDialect {}, "CREATE PROCEDURE p(integer, IN x int, double precision, t DEFAULT 1)")
            .tokenize_with_location()
            .expect("tokenize");
        let out = unnamed_procedure_params(&toks);
        let marked: Vec<String> = out
            .iter()
            .enumerate()
            .filter(|(_, t)| matches!(&t.token, Token::Word(w) if w.value == UNNAMED_PARAM_MARKER))
            .map(|(i, _)| match &out[i + 1].token {
                Token::Word(w) => w.value.clone(),
                _ => String::new(),
            })
            .collect();
        assert_eq!(marked, vec!["integer".to_string(), "double".to_string(), "t".to_string()]);
    }

    #[test]
    fn create_rule_action_list_semicolons_do_not_split() {
        // `DO ALSO ( stmt; stmt )`: the semicolon inside the parentheses belongs to the rule.
        let sql = "CREATE RULE r AS ON UPDATE TO t DO ALSO (UPDATE u SET a = 1; UPDATE v SET b = 2); SELECT 1";
        let toks = Tokenizer::new(&PostgreSqlDialect {}, sql).tokenize_with_location().expect("tokenize");
        let ranges = split_statements(&toks);
        assert_eq!(ranges.len(), 2);
        let (first, last) = ranges[0];
        assert!(matches!(&toks[first].token, Token::Word(w) if w.value == "CREATE"));
        assert!(matches!(&toks[last - 1].token, Token::RParen));
    }

    #[test]
    fn json_table_column_without_path_parses() {
        assert!(parse("SELECT * FROM JSON_TABLE(NULL::jsonb, '$' COLUMNS (foo int)) bar").is_ok());
    }

    #[test]
    fn ctas_with_column_name_list_parses_as_untyped_columns() {
        let stmt = parse("CREATE TEMP TABLE temptest (col) ON COMMIT DELETE ROWS AS SELECT 1").expect("parse");
        assert!(matches!(stmt[0].as_ast(), Some(Statement::CreateTable(ct)) if ct.columns.len() == 1 && ct.query.is_some()));
        assert!(parse("CREATE TABLE renamed (a, b) AS SELECT 1 AS x, 'two' AS y").is_ok());
    }

    #[test]
    fn drop_domain_name_list_parses_as_drop_type_rewrite() {
        let list = parse("DROP DOMAIN IF EXISTS d_two, no_such_domain").expect("parse");
        assert!(matches!(list[0].as_ast(), Some(Statement::Drop { .. })));
        let single = parse("DROP DOMAIN d_one").expect("parse");
        assert!(matches!(single[0].as_ast(), Some(Statement::DropDomain(_))));
        assert!(is_drop_domain_text("  drop /* c */ domain d"));
    }

    #[test]
    fn grant_routine_keyword_is_read_from_text() {
        assert_eq!(grant_routine_text("GRANT EXECUTE ON ROUTINE f TO r"), Some("routine"));
        assert_eq!(grant_routine_text("REVOKE EXECUTE ON ALL ROUTINES IN SCHEMA s FROM PUBLIC"), Some("all routines"));
        assert_eq!(grant_routine_text("GRANT SELECT ON routine TO r"), None);
        assert_eq!(grant_routine_text("GRANT EXECUTE ON FUNCTION f TO r"), None);
    }

    #[test]
    fn create_schema_elements_are_leased_0a000_at_the_element() {
        // The element keyword is at character 19; PostgreSQL accepts the statement, PGArbor does not run it yet.
        let e = parse("CREATE SCHEMA s20 CREATE SEQUENCE seq20").err().expect("unsupported");
        assert_eq!(e.sqlstate.code(), "0A000");
        assert_eq!(e.position, Some(19));
        assert!(parse("CREATE SCHEMA s20").is_ok());
        // gram.y: IF NOT EXISTS with elements has its own 0A000 message.
        let e = parse("CREATE SCHEMA IF NOT EXISTS s20 CREATE SEQUENCE seq20").err().expect("unsupported");
        assert_eq!(e.sqlstate.code(), "0A000");
        assert_eq!(e.message, "CREATE SCHEMA IF NOT EXISTS cannot include schema elements");
    }
}
