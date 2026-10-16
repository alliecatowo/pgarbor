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
    /// `OVERRIDING {SYSTEM|USER} VALUE` of an `INSERT` statement (see `extract_insert_overriding`);
    /// `None` for every other statement.
    pub overriding: Option<crate::ir::stmt::Overriding>,
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
        if let Parsed::Extra(ExtraStatement::TransactionModes { kind: crate::extra_stmt::TxnCommandKind::StartTransaction, .. }) =
            &self.parsed
        {
            return Some("START TRANSACTION");
        }
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
        let toks = pg_lexical_tokens(toks);
        let toks = unicode_escaped_identifiers(toks, &src)?;
        let toks = merge_operator_names(toks, &src)?;
        let toks = spell_operator_runs(toks, &src)?;
        let toks = prefix_qualified_operators(toks, &src);
        let toks = tablesample_seed_exprs(toks);
        let toks = at_local(toks);
        let toks = trigger_exec_args(toks);
        let toks = legacy_named_arguments(toks);
        let toks = join_composite_compare_ops(toks);

        let mut identity_groups = Vec::new();
        let mut fk_set_groups = Vec::new();
        let mut view_trailer = None;
        let mut overriding = None;
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
            let (toks, ov) = extract_insert_overriding(toks, &src)?;
            overriding = ov;
            strict_check::check(&toks, &src)?;
            let toks = strip_like_options(toks);
            let toks = strip_drop_index_concurrently(toks)?;
            let toks = comment_dollar_text(toks);
            let toks = pct_type_refs(toks);
            let toks = strip_legacy_operator_options(toks);
            let toks = function_cost_attrs(toks);
            let toks = strip_frame_exclusion(toks);
            let toks = copy_on_option_values(toks);
            let toks = flatten_nested_grouping_sets(toks);
            let toks = gist_siglen_params(toks, &src)?;
            let (toks, trailer) = view_trailer_opts::extract(&toks);
            let recursive = recursive_view(&toks);
            let is_recursive_view = recursive.is_some();
            let toks = recursive.unwrap_or(toks);
            if is_recursive_view && trailer.as_ref().is_some_and(|t| t.check_option.is_some()) {
                return Err(PgError::new(
                    pgarbor_common::SqlState::FEATURE_NOT_SUPPORTED,
                    "WITH CHECK OPTION not supported on recursive views",
                ));
            }
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
            overriding,
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
            overriding: st.overriding,
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
    overriding: Option<crate::ir::stmt::Overriding>,
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

/// `CREATE INDEX .. USING gist (col [opclass] (siglen = n))`: gram.y `index_elem` has no operator-class
/// parameter list in sqlparser 0.63, and `DEFAULT` is not an operator class name (syntax error at it).
/// For the GiST `tsvector_ops` class the parameter list is validated here like PG's reloption parser
/// (`parse_one_reloption`: 22023) and then removed from the tokens, so the statement reaches the binder
/// (which rejects the GiST method with 0A000). Other operator classes keep their tokens.
fn gist_siglen_params(toks: Vec<TokenWithSpan>, src: &SourceMap) -> Result<Vec<TokenWithSpan>> {
    let sig: Vec<usize> = (0..toks.len()).filter(|&i| !matches!(toks[i].token, Token::Whitespace(_))).collect();
    let word = |k: usize| sig.get(k).and_then(|&i| bare_word(&toks[i].token));
    if word(0).as_deref() != Some("CREATE") {
        return Ok(toks);
    }
    let Some(on) = (1..sig.len().min(12)).find(|&k| word(k).as_deref() == Some("ON")) else { return Ok(toks) };
    if !(1..on).any(|k| word(k).as_deref() == Some("INDEX")) {
        return Ok(toks);
    }
    let Some(lp) = (on + 1..sig.len()).find(|&k| matches!(toks[sig[k]].token, Token::LParen)) else {
        return Ok(toks);
    };
    let gist = (on + 1..lp).any(|k| word(k).as_deref() == Some("GIST"));
    let mut remove = vec![false; toks.len()];
    let mut depth = 0usize;
    let mut k = lp;
    while k < sig.len() {
        match &toks[sig[k]].token {
            Token::LParen => {
                if depth == 1
                    && matches!(toks[sig[k - 1]].token, Token::Word(_))
                    && !matches!(toks[sig[k - 2]].token, Token::LParen | Token::Comma)
                {
                    let mut d = 0usize;
                    let mut end = None;
                    for m in k..sig.len() {
                        match toks[sig[m]].token {
                            Token::LParen => d += 1,
                            Token::RParen => {
                                d = d.saturating_sub(1);
                                if d == 0 {
                                    end = Some(m);
                                    break;
                                }
                            }
                            _ => {}
                        }
                    }
                    let Some(end) = end else { return Ok(toks) };
                    let using = (on + 1..lp).find(|&m| word(m).as_deref() == Some("USING")).and_then(|m| word(m + 1));
                    let no_opt_am = matches!(using.as_deref(), None | Some("BTREE") | Some("HASH"));
                    if no_opt_am && end > k + 1 {
                        // index_opclass_options: btree/hash have no options support procedure.
                        if let Token::Word(w) = &toks[sig[k - 1]].token {
                            return Err(PgError::new(
                                pgarbor_common::SqlState::INVALID_PARAMETER_VALUE,
                                format!(
                                    "operator class {} has no options",
                                    if w.quote_style.is_none() { w.value.to_ascii_lowercase() } else { w.value.clone() }
                                ),
                            ));
                        }
                    }
                    if gist && word(k - 1).as_deref() == Some("TSVECTOR_OPS") {
                        validate_tsvector_gist_options(&toks, &sig[k + 1..end], src)?;
                        for r in remove[sig[k]..=sig[end]].iter_mut() {
                            *r = true;
                        }
                    }
                    k = end + 1;
                    continue;
                }
                depth += 1;
            }
            Token::RParen => {
                depth = depth.saturating_sub(1);
                if depth == 0 {
                    break;
                }
            }
            Token::Word(_) if depth == 1 && word(k).as_deref() == Some("DEFAULT") => {
                return Err(syntax_error::token_problem(&toks[sig[k]], src));
            }
            _ => {}
        }
        k += 1;
    }
    if !remove.iter().any(|r| *r) {
        return Ok(toks);
    }
    Ok(toks.into_iter().enumerate().filter(|(i, _)| !remove[*i]).map(|(_, t)| t).collect())
}

/// The `name = value` list of a GiST `tsvector_ops` class (`sig` holds the token indexes between the
/// parentheses): only `siglen`, an integer in 1..=2024.
fn validate_tsvector_gist_options(toks: &[TokenWithSpan], sig: &[usize], src: &SourceMap) -> Result<()> {
    let bad = |m: usize| match sig.get(m) {
        Some(&i) => syntax_error::token_problem(&toks[i], src),
        None => diag::syntax_error_near(")", None),
    };
    let mut m = 0;
    while m < sig.len() {
        let key = match &toks[sig[m]].token {
            Token::Word(w) => w.value.clone(),
            _ => return Err(bad(m)),
        };
        if !matches!(sig.get(m + 1).map(|&i| &toks[i].token), Some(Token::Eq)) {
            return Err(bad(m + 1));
        }
        m += 2;
        let mut text = String::new();
        if matches!(sig.get(m).map(|&i| &toks[i].token), Some(Token::Minus)) {
            text.push('-');
            m += 1;
        }
        match sig.get(m).map(|&i| &toks[i].token) {
            Some(Token::Number(n, _)) => text.push_str(n),
            Some(Token::SingleQuotedString(v)) => text.push_str(v),
            Some(Token::Word(w)) => text.push_str(&w.value),
            _ => return Err(bad(m)),
        }
        m += 1;
        if key.eq_ignore_ascii_case("siglen") {
            let Ok(v) = text.trim().parse::<i64>() else {
                return Err(PgError::new(
                    pgarbor_common::SqlState::INVALID_PARAMETER_VALUE,
                    format!("invalid value for integer option \"siglen\": {text}"),
                ));
            };
            if !(1..=2024).contains(&v) {
                return Err(PgError::new(
                    pgarbor_common::SqlState::INVALID_PARAMETER_VALUE,
                    format!("value {text} out of bounds for option \"siglen\""),
                )
                .with_detail("Valid values are between \"1\" and \"2024\"."));
            }
        } else {
            return Err(PgError::new(
                pgarbor_common::SqlState::INVALID_PARAMETER_VALUE,
                format!("unrecognized parameter \"{key}\""),
            ));
        }
        if m < sig.len() {
            if !matches!(toks[sig[m]].token, Token::Comma) {
                return Err(bad(m));
            }
            m += 1;
        }
    }
    Ok(())
}

/// Index of the `)` matching the `(` at `open` in `s`.
fn gs_matching_paren(s: &[TokenWithSpan], open: usize) -> Option<usize> {
    let mut depth = 0usize;
    for (m, t) in s.iter().enumerate().skip(open) {
        match t.token {
            Token::LParen => depth += 1,
            Token::RParen => {
                depth = depth.saturating_sub(1);
                if depth == 0 {
                    return Some(m);
                }
            }
            _ => {}
        }
    }
    None
}

/// Ranges `[a, b)` of `s[from..to]` split at the commas of nesting depth zero.
fn gs_split_commas(s: &[TokenWithSpan], from: usize, to: usize) -> Vec<(usize, usize)> {
    let mut ranges = Vec::new();
    let mut depth = 0usize;
    let mut start = from;
    for m in from..to {
        match s[m].token {
            Token::LParen => depth += 1,
            Token::RParen => depth = depth.saturating_sub(1),
            Token::Comma if depth == 0 => {
                ranges.push((start, m));
                start = m + 1;
            }
            _ => {}
        }
    }
    ranges.push((start, to));
    ranges
}

/// Expand the elements of the `GROUPING SETS (...)` body `s[from..to]` into `out`: a nested
/// `GROUPING SETS (..)` contributes its own elements, `CUBE`/`ROLLUP` their explicit sets (gram.y
/// `group_by_item`). Returns false when the body is not of that shape (the caller keeps the tokens).
fn gs_expand(s: &[TokenWithSpan], from: usize, to: usize, out: &mut Vec<Vec<TokenWithSpan>>, changed: &mut bool) -> bool {
    let mk = |t: Token| TokenWithSpan::new(t, Span::empty());
    let word = |k: usize| s.get(k).and_then(|t| bare_word(&t.token));
    for (a, b) in gs_split_commas(s, from, to) {
        if a >= b {
            return false;
        }
        let closes_at_end = |open: usize| gs_matching_paren(s, open) == Some(b - 1);
        let is_gs = word(a).as_deref() == Some("GROUPING")
            && word(a + 1).as_deref() == Some("SETS")
            && matches!(s.get(a + 2).map(|t| &t.token), Some(Token::LParen))
            && closes_at_end(a + 2);
        let cube_or_rollup = match word(a).as_deref() {
            Some("CUBE") => Some(true),
            Some("ROLLUP") => Some(false),
            _ => None,
        };
        if is_gs {
            if a + 3 >= b - 1 || !gs_expand(s, a + 3, b - 1, out, changed) {
                return false;
            }
            *changed = true;
        } else if let (Some(is_cube), true) = (
            cube_or_rollup,
            matches!(s.get(a + 1).map(|t| &t.token), Some(Token::LParen)) && closes_at_end(a + 1),
        ) {
            if a + 2 >= b - 1 {
                return false;
            }
            // Units: one expression or a parenthesized list each.
            let mut units: Vec<Vec<TokenWithSpan>> = Vec::new();
            for (ua, ub) in gs_split_commas(s, a + 2, b - 1) {
                if ua >= ub {
                    return false;
                }
                let wrapped = matches!(s[ua].token, Token::LParen) && gs_matching_paren(s, ua) == Some(ub - 1);
                units.push(if wrapped { s[ua + 1..ub - 1].to_vec() } else { s[ua..ub].to_vec() });
            }
            let n = units.len();
            if n > 12 {
                return false;
            }
            let picks: Vec<Vec<usize>> = if is_cube {
                (0..(1usize << n)).map(|m| (0..n).filter(|j| (m >> j) & 1 == 1).collect()).collect()
            } else {
                (0..=n).rev().map(|k| (0..k).collect()).collect()
            };
            for pick in picks {
                let mut e = vec![mk(Token::LParen)];
                let mut first = true;
                for j in pick {
                    if units[j].is_empty() {
                        continue;
                    }
                    if !first {
                        e.push(mk(Token::Comma));
                    }
                    first = false;
                    e.extend(units[j].iter().cloned());
                }
                e.push(mk(Token::RParen));
                out.push(e);
            }
            *changed = true;
        } else {
            out.push(s[a..b].to_vec());
        }
    }
    true
}

/// `GROUP BY GROUPING SETS (a, GROUPING SETS (b, CUBE (c)), ROLLUP (d, e))`: sqlparser 0.63 reads only
/// plain expressions and tuples as grouping-set elements. A nested `GROUPING SETS`, `CUBE` or `ROLLUP`
/// element is equivalent to its flattened sets (PG `flatten_grouping_sets`), so it is rewritten here into
/// explicit tuples. Statements without such an element are returned unchanged.
fn flatten_nested_grouping_sets(toks: Vec<TokenWithSpan>) -> Vec<TokenWithSpan> {
    let has_gs = toks.windows(1).any(|w| bare_word(&w[0].token).as_deref() == Some("SETS"));
    if !has_gs {
        return toks;
    }
    let s: Vec<TokenWithSpan> = toks.iter().filter(|t| !matches!(t.token, Token::Whitespace(_))).cloned().collect();
    let word = |k: usize| s.get(k).and_then(|t| bare_word(&t.token));
    let mut out: Vec<TokenWithSpan> = Vec::with_capacity(s.len());
    let mut changed = false;
    let mut k = 0;
    while k < s.len() {
        if word(k).as_deref() == Some("GROUPING")
            && word(k + 1).as_deref() == Some("SETS")
            && matches!(s.get(k + 2).map(|t| &t.token), Some(Token::LParen))
        {
            if let Some(close) = gs_matching_paren(&s, k + 2) {
                let mut elems: Vec<Vec<TokenWithSpan>> = Vec::new();
                let mut ch = false;
                if close > k + 3 && gs_expand(&s, k + 3, close, &mut elems, &mut ch) && ch {
                    out.extend(s[k..k + 3].iter().cloned());
                    for (i, e) in elems.into_iter().enumerate() {
                        if i > 0 {
                            out.push(TokenWithSpan::new(Token::Comma, Span::empty()));
                        }
                        out.extend(e);
                    }
                    out.push(s[close].clone());
                    k = close + 1;
                    changed = true;
                    continue;
                }
            }
        }
        out.push(s[k].clone());
        k += 1;
    }
    if changed {
        out
    } else {
        toks
    }
}

/// Window frame `EXCLUDE CURRENT ROW | GROUP | TIES | NO OTHERS` (gram.y `opt_window_exclusion_clause`).
/// sqlparser 0.63 `WindowFrame` has no exclusion field, so the clause is removed from the tokens here
/// (only directly after a frame bound, i.e. after `PRECEDING`, `FOLLOWING` or `CURRENT ROW`) and the
/// binder re-reads it from the query text (`binder::window::frame_exclusion`).
/// `COPY ... (HEADER on, FREEZE on)`: gram.y `copy_generic_opt_arg` takes `opt_boolean_or_string`,
/// which includes the reserved word `ON`; sqlparser 0.63 does not, so a bare `ON` option value is
/// replaced by the equivalent `true` (copy.c `defGetBoolean` treats both alike).
fn copy_on_option_values(mut toks: Vec<TokenWithSpan>) -> Vec<TokenWithSpan> {
    let sig: Vec<usize> = (0..toks.len()).filter(|&i| !matches!(toks[i].token, Token::Whitespace(_))).collect();
    if sig.first().and_then(|&i| bare_word(&toks[i].token)).as_deref() != Some("COPY") {
        return toks;
    }
    let mut depth = 0usize;
    for k in 0..sig.len() {
        match toks[sig[k]].token {
            Token::LParen => depth += 1,
            Token::RParen => depth = depth.saturating_sub(1),
            _ => {}
        }
        if depth == 0 || k == 0 || bare_word(&toks[sig[k]].token).as_deref() != Some("ON") {
            continue;
        }
        let prev_is_name = matches!(toks[sig[k - 1]].token, Token::Word(_));
        let next_ends = sig.get(k + 1).is_some_and(|&i| matches!(toks[i].token, Token::Comma | Token::RParen));
        if prev_is_name && next_ends {
            toks[sig[k]].token = Token::make_word("true", None);
        }
    }
    toks
}

fn strip_frame_exclusion(toks: Vec<TokenWithSpan>) -> Vec<TokenWithSpan> {
    let sig: Vec<usize> = (0..toks.len()).filter(|&i| !matches!(toks[i].token, Token::Whitespace(_))).collect();
    let word = |k: usize| sig.get(k).and_then(|&i| bare_word(&toks[i].token));
    let mut remove = vec![false; toks.len()];
    let mut any = false;
    for k in 1..sig.len() {
        if word(k).as_deref() != Some("EXCLUDE") {
            continue;
        }
        if !matches!(word(k - 1).as_deref(), Some("PRECEDING" | "FOLLOWING" | "ROW")) {
            continue;
        }
        let len = match (word(k + 1).as_deref(), word(k + 2).as_deref()) {
            (Some("CURRENT"), Some("ROW")) | (Some("NO"), Some("OTHERS")) => 3,
            (Some("GROUP" | "TIES"), _) => 2,
            _ => continue,
        };
        for r in remove[sig[k]..=sig[k + len - 1]].iter_mut() {
            *r = true;
        }
        any = true;
    }
    if !any {
        return toks;
    }
    toks.into_iter().enumerate().filter(|(i, _)| !remove[*i]).map(|(_, t)| t).collect()
}

/// `CREATE TABLE t (LIKE src INCLUDING ALL EXCLUDING COMMENTS ...)`: PG's `TableLikeOptionList`. sqlparser
/// 0.63 only reads a lone `INCLUDING|EXCLUDING DEFAULTS`, so any other option list is removed from the
/// tokens here (an unknown option word is left in place and stays a syntax error). The LIKE clause itself
/// is still rejected by the binder (`CREATE TABLE ... LIKE`, 0A000), so the options are not needed yet.
fn strip_like_options(toks: Vec<TokenWithSpan>) -> Vec<TokenWithSpan> {
    const OPTS: [&str; 10] = [
        "ALL", "COMMENTS", "COMPRESSION", "CONSTRAINTS", "DEFAULTS", "GENERATED", "IDENTITY", "INDEXES",
        "STATISTICS", "STORAGE",
    ];
    let sig: Vec<usize> = (0..toks.len()).filter(|&i| !matches!(toks[i].token, Token::Whitespace(_))).collect();
    let word = |k: usize| sig.get(k).and_then(|&i| bare_word(&toks[i].token));
    if word(0).as_deref() != Some("CREATE") {
        return toks;
    }
    let Some(l) = (0..sig.len().saturating_sub(1))
        .find(|&k| matches!(toks[sig[k]].token, Token::LParen) && word(k + 1).as_deref() == Some("LIKE"))
    else {
        return toks;
    };
    // Source name: words and dots.
    let mut k = l + 2;
    while k < sig.len() && matches!(toks[sig[k]].token, Token::Word(_) | Token::Period) {
        if bare_word(&toks[sig[k]].token).is_some_and(|w| w == "INCLUDING" || w == "EXCLUDING") {
            break;
        }
        k += 1;
    }
    let opts_start = k;
    let mut remove = Vec::new();
    while k + 1 < sig.len() {
        let w = word(k);
        if !matches!(w.as_deref(), Some("INCLUDING") | Some("EXCLUDING")) {
            break;
        }
        match word(k + 1) {
            Some(o) if OPTS.contains(&o.as_str()) => {
                remove.push(sig[k]);
                remove.push(sig[k + 1]);
                k += 2;
            }
            _ => return toks,
        }
    }
    if remove.is_empty() || !matches!(sig.get(k).map(|&i| &toks[i].token), Some(Token::RParen)) {
        return toks;
    }
    if remove.len() == 2 && word(opts_start + 1).as_deref() == Some("DEFAULTS") {
        return toks;
    }
    toks.into_iter().enumerate().filter(|(i, _)| !remove.contains(i)).map(|(_, t)| t).collect()
}

/// `DROP INDEX CONCURRENTLY [IF EXISTS] name`: sqlparser 0.63 has no `CONCURRENTLY` here. The keyword is
/// removed (the drop itself is the same operation). PG's `DropCommand` rejects the multi-name and
/// `CASCADE` forms with 0A000, which is raised here from the tokens.
fn strip_drop_index_concurrently(toks: Vec<TokenWithSpan>) -> Result<Vec<TokenWithSpan>> {
    let sig: Vec<usize> = (0..toks.len()).filter(|&i| !matches!(toks[i].token, Token::Whitespace(_))).collect();
    let word = |k: usize| sig.get(k).and_then(|&i| bare_word(&toks[i].token));
    if word(0).as_deref() != Some("DROP") || word(1).as_deref() != Some("INDEX") || word(2).as_deref() != Some("CONCURRENTLY") {
        return Ok(toks);
    }
    let mut depth = 0usize;
    for &i in &sig[3..] {
        match &toks[i].token {
            Token::LParen => depth += 1,
            Token::RParen => depth = depth.saturating_sub(1),
            Token::Comma if depth == 0 => {
                return Err(PgError::new(
                    pgarbor_common::SqlState::FEATURE_NOT_SUPPORTED,
                    "DROP INDEX CONCURRENTLY does not support dropping multiple objects",
                ));
            }
            _ => {}
        }
        if depth == 0 && bare_word(&toks[i].token).as_deref() == Some("CASCADE") {
            return Err(PgError::new(
                pgarbor_common::SqlState::FEATURE_NOT_SUPPORTED,
                "DROP INDEX CONCURRENTLY does not support CASCADE",
            ));
        }
    }
    let at = sig[2];
    Ok(toks.into_iter().enumerate().filter(|(i, _)| *i != at).map(|(_, t)| t).collect())
}

/// Marker `SET` parameter names that stand for the `COST n`, `ROWS n` and `[NOT] LEAKPROOF` attributes of
/// `CREATE FUNCTION` (names contain a NUL, which PostgreSQL identifiers cannot). sqlparser 0.63 has no AST
/// for them, so [`function_cost_attrs`] rewrites each into `SET "<marker>" TO <number>` and the binder
/// (`ddl_function::take_attr_markers`) takes them out of `set_params` again.
pub const FN_ATTR_COST: &str = "\0pgarbor function attribute cost";
pub const FN_ATTR_ROWS: &str = "\0pgarbor function attribute rows";
pub const FN_ATTR_LEAKPROOF: &str = "\0pgarbor function attribute leakproof";

/// `CREATE [OR REPLACE] FUNCTION|PROCEDURE name (args) .. COST n | ROWS n | [NOT] LEAKPROOF ..`: see
/// [`FN_ATTR_COST`]. Only the attribute words between the argument list and the body (`BEGIN`/`RETURN`)
/// are looked at; `LEAKPROOF` becomes `1` and `NOT LEAKPROOF` `0`.
fn function_cost_attrs(toks: Vec<TokenWithSpan>) -> Vec<TokenWithSpan> {
    let sig: Vec<usize> = (0..toks.len()).filter(|&i| !matches!(toks[i].token, Token::Whitespace(_))).collect();
    let word = |k: usize| sig.get(k).and_then(|&i| bare_word(&toks[i].token));
    if word(0).as_deref() != Some("CREATE") {
        return toks;
    }
    let mut k = 1;
    if word(k).as_deref() == Some("OR") && word(k + 1).as_deref() == Some("REPLACE") {
        k += 2;
    }
    if !matches!(word(k).as_deref(), Some("FUNCTION" | "PROCEDURE")) {
        return toks;
    }
    let Some(open) = (k..sig.len()).find(|&m| matches!(toks[sig[m]].token, Token::LParen)) else { return toks };
    let mut depth = 0usize;
    let mut close = None;
    for m in open..sig.len() {
        match toks[sig[m]].token {
            Token::LParen => depth += 1,
            Token::RParen => {
                depth = depth.saturating_sub(1);
                if depth == 0 {
                    close = Some(m);
                    break;
                }
            }
            _ => {}
        }
    }
    let Some(close) = close else { return toks };
    let mut replace: Vec<(usize, Vec<Token>)> = Vec::new();
    let mut remove: Vec<usize> = Vec::new();
    let mut depth = 0usize;
    let mut m = close + 1;
    while m < sig.len() {
        let t = &toks[sig[m]].token;
        match t {
            Token::LParen => depth += 1,
            Token::RParen => depth = depth.saturating_sub(1),
            _ => {}
        }
        if depth == 0 {
            match word(m).as_deref() {
                Some("BEGIN" | "RETURN") => break,
                Some(w @ ("COST" | "ROWS")) => {
                    if let Some(Token::Number(n, _)) = sig.get(m + 1).map(|&i| &toks[i].token) {
                        let marker = if w == "COST" { FN_ATTR_COST } else { FN_ATTR_ROWS };
                        replace.push((sig[m], marker_set(marker, Token::Number(n.clone(), false))));
                        remove.push(sig[m + 1]);
                        m += 1;
                    }
                }
                Some("LEAKPROOF") => {
                    let negated = m > close + 1 && word(m - 1).as_deref() == Some("NOT");
                    if negated {
                        remove.push(sig[m - 1]);
                    }
                    let flag = Token::Number(if negated { "0" } else { "1" }.to_string(), false);
                    replace.push((sig[m], marker_set(FN_ATTR_LEAKPROOF, flag)));
                }
                _ => {}
            }
        }
        m += 1;
    }
    if replace.is_empty() {
        return toks;
    }
    let mut out = Vec::with_capacity(toks.len() + replace.len() * 3);
    for (i, t) in toks.into_iter().enumerate() {
        if remove.contains(&i) {
            continue;
        }
        match replace.iter().find(|(at, _)| *at == i) {
            Some((_, new)) => out.extend(new.iter().map(|n| TokenWithSpan::new(n.clone(), t.span))),
            None => out.push(t),
        }
    }
    out
}

/// `SET "<marker>" TO <value>` as tokens.
fn marker_set(marker: &str, value: Token) -> Vec<Token> {
    let kw = |s: &str, keyword: Keyword| Token::Word(Word { value: s.to_string(), quote_style: None, keyword });
    vec![
        kw("SET", Keyword::SET),
        Token::Word(Word { value: marker.to_string(), quote_style: Some('"'), keyword: Keyword::NoKeyword }),
        kw("TO", Keyword::TO),
        value,
    ]
}

/// `CREATE OPERATOR op (.., SORT1 = x, SORT2 = x, LTCMP = x, GTCMP = x, ..)`: the obsolete sort options
/// (gram.y accepts any `def_elem` name; `DefineOperator` ignores these four except that they imply
/// `MERGES`) are not in sqlparser 0.63's option list. They are removed from the tokens so the statement
/// parses and reaches the support check like any other `CREATE OPERATOR`.
fn strip_legacy_operator_options(toks: Vec<TokenWithSpan>) -> Vec<TokenWithSpan> {
    let sig: Vec<usize> = (0..toks.len()).filter(|&i| !matches!(toks[i].token, Token::Whitespace(_))).collect();
    let word = |k: usize| sig.get(k).and_then(|&i| bare_word(&toks[i].token));
    if word(0).as_deref() != Some("CREATE") || word(1).as_deref() != Some("OPERATOR") {
        return toks;
    }
    if matches!(word(2).as_deref(), Some("CLASS" | "FAMILY")) {
        return toks;
    }
    let Some(open) = (2..sig.len()).find(|&k| matches!(toks[sig[k]].token, Token::LParen)) else { return toks };
    // Elements of the option list: (first, last) sig positions, split at depth-1 commas.
    let mut elements: Vec<(usize, usize)> = Vec::new();
    let mut depth = 0usize;
    let mut first = open + 1;
    for k in open..sig.len() {
        match toks[sig[k]].token {
            Token::LParen => depth += 1,
            Token::RParen => {
                depth = depth.saturating_sub(1);
                if depth == 0 {
                    if k > first {
                        elements.push((first, k - 1));
                    }
                    break;
                }
            }
            Token::Comma if depth == 1 => {
                if k > first {
                    elements.push((first, k - 1));
                }
                first = k + 1;
            }
            _ => {}
        }
    }
    // Also dropped: any other attribute name. `DefineOperator` only warns `operator attribute "x" not
    // recognized` for those (the statement is 0A000 here anyway), while sqlparser rejects them.
    let strippable = |w: Option<&str>| {
        w.is_some_and(|w| {
            !matches!(
                w,
                "FUNCTION" | "PROCEDURE" | "LEFTARG" | "RIGHTARG" | "COMMUTATOR" | "NEGATOR" | "RESTRICT" | "JOIN" | "HASHES" | "MERGES"
            )
        })
    };
    let mut remove: Vec<usize> = Vec::new();
    let kept = elements
        .iter()
        .filter(|(s, _)| !strippable(word(*s).as_deref()))
        .count();
    if kept == elements.len() || kept == 0 {
        return toks;
    }
    for (n, (s, e)) in elements.iter().enumerate() {
        if strippable(word(*s).as_deref()) {
            remove.extend((*s..=*e).map(|k| sig[k]));
            // The comma before the element when a kept element precedes it, else the one after it.
            let kept_before = elements[..n]
                .iter()
                .any(|(bs, _)| !strippable(word(*bs).as_deref()));
            if kept_before {
                remove.push(sig[*s - 1]);
            } else if n + 1 < elements.len() {
                remove.push(sig[*e + 1]);
            }
        }
    }
    toks.into_iter().enumerate().filter(|(i, _)| !remove.contains(i)).map(|(_, t)| t).collect()
}

/// Prefix of the synthetic type name that stands for a `rel.col%TYPE` reference (gram.y `GenericType` with
/// `'%' TYPE_P`). sqlparser 0.63 cannot read the reference, so [`pct_type_refs`] turns it into one quoted
/// identifier `<PCT_TYPE_MARKER><part>\u{1}<part>...` and `binder::types::resolve_data_type` resolves it.
pub const PCT_TYPE_MARKER: &str = "\0pgarbor pct type ";

/// `CREATE FUNCTION|PROCEDURE` parameter and return types written `name.name[.name]%TYPE`: see
/// [`PCT_TYPE_MARKER`]. Unquoted parts are lower-cased like identifiers; quoted parts keep their text.
fn pct_type_refs(toks: Vec<TokenWithSpan>) -> Vec<TokenWithSpan> {
    let sig: Vec<usize> = (0..toks.len()).filter(|&i| !matches!(toks[i].token, Token::Whitespace(_))).collect();
    let word = |k: usize| sig.get(k).and_then(|&i| bare_word(&toks[i].token));
    if word(0).as_deref() != Some("CREATE") {
        return toks;
    }
    let mut k = 1;
    if word(k).as_deref() == Some("OR") && word(k + 1).as_deref() == Some("REPLACE") {
        k += 2;
    }
    if !matches!(word(k).as_deref(), Some("FUNCTION" | "PROCEDURE")) {
        return toks;
    }
    // (first raw index, last raw index, replacement token)
    let mut repl: Vec<(usize, usize, TokenWithSpan)> = Vec::new();
    for m in 1..sig.len().saturating_sub(1) {
        if !matches!(toks[sig[m]].token, Token::Mod) || word(m + 1).as_deref() != Some("TYPE") {
            continue;
        }
        // Walk back over `word (. word)*`.
        let mut parts: Vec<String> = Vec::new();
        let mut start = m;
        let mut p = m;
        loop {
            let Some(Token::Word(w)) = p.checked_sub(1).and_then(|q| sig.get(q)).map(|&i| &toks[i].token) else { break };
            parts.push(if w.quote_style.is_some() { w.value.clone() } else { w.value.to_ascii_lowercase() });
            start = p - 1;
            p -= 1;
            if p >= 1 && matches!(toks[sig[p - 1]].token, Token::Period) {
                p -= 1;
            } else {
                break;
            }
        }
        if parts.len() < 2 {
            continue;
        }
        parts.reverse();
        let word = Word { value: format!("{PCT_TYPE_MARKER}{}", parts.join("\u{1}")), quote_style: Some('"'), keyword: Keyword::NoKeyword };
        let span = Span::new(toks[sig[start]].span.start, toks[sig[m + 1]].span.end);
        repl.push((sig[start], sig[m + 1], TokenWithSpan::new(Token::Word(word), span)));
    }
    if repl.is_empty() {
        return toks;
    }
    let mut out = Vec::with_capacity(toks.len());
    for (i, t) in toks.into_iter().enumerate() {
        if let Some((_, _, new)) = repl.iter().find(|(a, _, _)| *a == i) {
            out.push(new.clone());
        } else if !repl.iter().any(|(a, b, _)| i > *a && i <= *b) {
            out.push(t);
        }
    }
    out
}

/// `CREATE [OR REPLACE] [TEMP] RECURSIVE VIEW v (cols) [WITH (..)] AS query` is
/// `CREATE VIEW v (cols) AS WITH RECURSIVE v (cols) AS (query) SELECT cols FROM v` (gram.y
/// `makeRecursiveViewSelect`; the CTE and the final `FROM` use the unqualified view name). sqlparser 0.63
/// has no `RECURSIVE VIEW`, so the tokens are rewritten. `toks` ends with EOF (kept last). A chunk that is
/// not a well-formed recursive view comes back unchanged.
fn recursive_view(toks: &[TokenWithSpan]) -> Option<Vec<TokenWithSpan>> {
    let sig: Vec<usize> =
        (0..toks.len()).filter(|&i| !matches!(toks[i].token, Token::Whitespace(_) | Token::EOF)).collect();
    let word = |k: usize| sig.get(k).and_then(|&i| bare_word(&toks[i].token));
    if word(0).as_deref() != Some("CREATE") {
        return None;
    }
    let mut r = 1;
    while matches!(word(r).as_deref(), Some("OR" | "REPLACE" | "TEMP" | "TEMPORARY" | "UNLOGGED" | "LOCAL" | "GLOBAL")) {
        r += 1;
    }
    if word(r).as_deref() != Some("RECURSIVE") || word(r + 1).as_deref() != Some("VIEW") {
        return None;
    }
    // Name: words and periods up to the column list.
    let mut k = r + 2;
    let mut name_last = None;
    while k < sig.len() && matches!(toks[sig[k]].token, Token::Word(_) | Token::Period) {
        if matches!(toks[sig[k]].token, Token::Word(_)) {
            name_last = Some(k);
        }
        k += 1;
    }
    let Some(name_last) = name_last else { return None };
    if !matches!(sig.get(k).map(|&i| &toks[i].token), Some(Token::LParen)) {
        return None;
    }
    let cols_open = k;
    let mut depth = 0usize;
    let mut cols_close = None;
    while k < sig.len() {
        match toks[sig[k]].token {
            Token::LParen => depth += 1,
            Token::RParen => {
                depth = depth.saturating_sub(1);
                if depth == 0 {
                    cols_close = Some(k);
                    break;
                }
            }
            _ => {}
        }
        k += 1;
    }
    let Some(cols_close) = cols_close else { return None };
    // Optional `WITH (reloptions)`, then AS.
    let mut a = cols_close + 1;
    if word(a).as_deref() == Some("WITH") && matches!(sig.get(a + 1).map(|&i| &toks[i].token), Some(Token::LParen)) {
        let mut d = 0usize;
        a += 1;
        while a < sig.len() {
            match toks[sig[a]].token {
                Token::LParen => d += 1,
                Token::RParen => {
                    d = d.saturating_sub(1);
                    if d == 0 {
                        break;
                    }
                }
                _ => {}
            }
            a += 1;
        }
        a += 1;
    }
    if word(a).as_deref() != Some("AS") || a + 1 >= sig.len() {
        return None;
    }
    let span = toks[sig[a]].span;
    let tk = |t: Token| TokenWithSpan::new(t, span);
    let kw = |s: &str, keyword: Keyword| tk(Token::Word(Word { value: s.to_string(), quote_style: None, keyword }));
    let name = toks[sig[name_last]].clone();
    let cols: Vec<TokenWithSpan> = (cols_open..=cols_close).map(|k| toks[sig[k]].clone()).collect();
    let mut out: Vec<TokenWithSpan> = Vec::with_capacity(toks.len() + 2 * cols.len() + 12);
    let as_at = sig[a];
    for (i, t) in toks.iter().enumerate() {
        if i == sig[r] {
            continue;
        }
        if matches!(t.token, Token::EOF) {
            // Close the CTE and add the final SELECT.
            out.push(tk(Token::RParen));
            out.push(kw("SELECT", Keyword::SELECT));
            out.extend(cols[1..cols.len() - 1].iter().cloned());
            out.push(kw("FROM", Keyword::FROM));
            out.push(name.clone());
        }
        out.push(t.clone());
        if i == as_at {
            out.push(kw("WITH", Keyword::WITH));
            out.push(kw("RECURSIVE", Keyword::RECURSIVE));
            out.push(name.clone());
            out.extend(cols.iter().cloned());
            out.push(kw("AS", Keyword::AS));
            out.push(tk(Token::LParen));
        }
    }
    Some(out)
}

/// `COMMENT ON .. IS $$text$$`: gram.y `comment_text` is any `Sconst`, dollar-quoted included, but
/// sqlparser 0.63 reads the text with `parse_literal_string`, which rejects `DollarQuotedString`. The
/// dollar-quoted token right after `IS` becomes a plain string token with the same value.
fn comment_dollar_text(toks: Vec<TokenWithSpan>) -> Vec<TokenWithSpan> {
    let sig: Vec<usize> = (0..toks.len()).filter(|&i| !matches!(toks[i].token, Token::Whitespace(_))).collect();
    if sig.first().and_then(|&i| bare_word(&toks[i].token)).as_deref() != Some("COMMENT") {
        return toks;
    }
    let mut toks = toks;
    for k in 1..sig.len() {
        if bare_word(&toks[sig[k - 1]].token).as_deref() != Some("IS") {
            continue;
        }
        let value = match &toks[sig[k]].token {
            Token::DollarQuotedString(d) => d.value.clone(),
            _ => continue,
        };
        toks[sig[k]].token = Token::SingleQuotedString(value);
    }
    toks
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

/// Name of the call that wraps the lower bound of `x BETWEEN SYMMETRIC low AND high` (PG `a_expr BETWEEN SYMMETRIC
/// b_expr AND a_expr`). sqlparser 0.63 has no SYMMETRIC flag, so [`mark_between_symmetric`] drops the keyword and
/// wraps the lower bound in `"<marker>"(low)`; `expr_ops::bind_between` unwraps it and builds the symmetric
/// expansion. The name holds a NUL, which no PostgreSQL identifier can.
pub const SYMMETRIC_MARKER: &str = "\0pgarbor symmetric";

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

/// `ALTER FUNCTION|PROCEDURE|ROUTINE ... PARALLEL <word>`: the PG grammar takes any identifier and
/// `interpret_func_parallel` rejects it (42601 `parameter "parallel" must be SAFE, RESTRICTED, or UNSAFE`,
/// no position), whereas sqlparser fails with a syntax error at the word.
fn alter_function_parallel_value(toks: &[TokenWithSpan]) -> Result<()> {
    let sig: Vec<&Token> =
        toks.iter().map(|t| &t.token).filter(|t| !matches!(t, Token::Whitespace(_))).collect();
    let is_alter = matches!(
        (sig.first().and_then(|t| bare_word(t)), sig.get(1).and_then(|t| bare_word(t))),
        (Some(a), Some(o)) if a == "ALTER" && matches!(o.as_str(), "FUNCTION" | "PROCEDURE" | "ROUTINE")
    );
    if !is_alter {
        return Ok(());
    }
    // Skip the name and argument list: actions start after the matching close parenthesis.
    let mut depth = 0usize;
    let mut i = 2;
    while i < sig.len() {
        match sig[i] {
            Token::LParen => depth += 1,
            Token::RParen => depth = depth.saturating_sub(1),
            t if depth == 0 && i > 2 && bare_word(t).as_deref() == Some("PARALLEL") => {
                if let Some(Token::Word(w)) = sig.get(i + 1) {
                    if w.quote_style.is_none()
                        && !matches!(w.value.to_ascii_uppercase().as_str(), "SAFE" | "RESTRICTED" | "UNSAFE")
                    {
                        return Err(PgError::new(
                            pgarbor_common::SqlState::SYNTAX_ERROR,
                            "parameter \"parallel\" must be SAFE, RESTRICTED, or UNSAFE",
                        ));
                    }
                }
            }
            _ => {}
        }
        i += 1;
    }
    Ok(())
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

/// `U&"..."` (scan.l `xui`): a delimited identifier with Unicode escapes (`\XXXX`, `\+XXXXXX`, the escape
/// character doubled for itself), optionally followed by `UESCAPE 'c'` to change the escape character. sqlparser
/// tokenizes only the `U&'..'` string form, so the identifier arrives as `U`, `&`, and a quoted word; the run
/// becomes one quoted word holding the decoded name.
fn unicode_escaped_identifiers(toks: Vec<TokenWithSpan>, src: &SourceMap) -> Result<Vec<TokenWithSpan>> {
    let mut out: Vec<TokenWithSpan> = Vec::with_capacity(toks.len());
    let mut i = 0;
    while i + 2 < toks.len() {
        let is_u = matches!(&toks[i].token, Token::Word(w) if w.quote_style.is_none() && w.value.eq_ignore_ascii_case("u"));
        let quoted = match &toks[i + 2].token {
            Token::Word(w) if w.quote_style == Some('"') => Some(w.value.clone()),
            _ => None,
        };
        if !(is_u
            && matches!(toks[i + 1].token, Token::Ampersand)
            && toks[i + 1].span.start == toks[i].span.end
            && toks[i + 2].span.start == toks[i + 1].span.end)
        {
            out.push(toks[i].clone());
            i += 1;
            continue;
        }
        let Some(body) = quoted else {
            out.push(toks[i].clone());
            i += 1;
            continue;
        };
        let pos = src.char_pos(toks[i].span.start);
        let mut last = i + 2;
        let mut escape = '\\';
        if let Some(k) = next_significant(&toks, i + 3) {
            if bare_word(&toks[k].token).as_deref() == Some("UESCAPE") {
                if let Some(q) = next_significant(&toks, k + 1) {
                    if let Token::SingleQuotedString(e) = &toks[q].token {
                        let mut chars = e.chars();
                        match (chars.next(), chars.next()) {
                            (Some(c), None) if !c.is_ascii_hexdigit() && !c.is_whitespace() && !matches!(c, '+' | '\'' | '"') => {
                                escape = c;
                                last = q;
                            }
                            _ => {
                                let at = src.char_pos(toks[q].span.start);
                                return Err(diag::syntax("invalid Unicode escape character", at));
                            }
                        }
                    }
                }
            }
        }
        let name = decode_unicode_identifier(&body, escape).map_err(|msg| {
            let err = diag::syntax(msg, pos);
            err.with_hint("Unicode escapes must be \\XXXX or \\+XXXXXX.")
        })?;
        let word = Word { value: name, quote_style: Some('"'), keyword: Keyword::NoKeyword };
        out.push(TokenWithSpan::new(Token::Word(word), Span::new(toks[i].span.start, toks[last].span.end)));
        i = last + 1;
    }
    out.extend(toks[i..].iter().cloned());
    Ok(out)
}

/// Decode the escapes of a `U&"..."` identifier body (`pg_wchar` surrogate pairs combine into one character).
fn decode_unicode_identifier(body: &str, escape: char) -> std::result::Result<String, &'static str> {
    let chars: Vec<char> = body.chars().collect();
    let mut out = String::with_capacity(body.len());
    let mut pending: Option<u32> = None;
    let hex = |from: usize, n: usize| -> Option<u32> {
        let mut v = 0u32;
        for k in 0..n {
            v = v * 16 + chars.get(from + k)?.to_digit(16)?;
        }
        Some(v)
    };
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        if c != escape {
            if pending.is_some() {
                return Err("invalid Unicode surrogate pair");
            }
            out.push(c);
            i += 1;
            continue;
        }
        if chars.get(i + 1) == Some(&escape) {
            if pending.is_some() {
                return Err("invalid Unicode surrogate pair");
            }
            out.push(escape);
            i += 2;
            continue;
        }
        let (from, n) = if chars.get(i + 1) == Some(&'+') { (i + 2, 6) } else { (i + 1, 4) };
        let cp = hex(from, n).ok_or("invalid Unicode escape")?;
        i = from + n;
        if cp == 0 || cp > 0x10FFFF {
            return Err("invalid Unicode escape value");
        }
        match (pending.take(), cp) {
            (Some(high), 0xDC00..=0xDFFF) => {
                let joined = 0x10000 + ((high - 0xD800) << 10) + (cp - 0xDC00);
                out.push(char::from_u32(joined).ok_or("invalid Unicode escape value")?);
            }
            (Some(_), _) => return Err("invalid Unicode surrogate pair"),
            (None, 0xD800..=0xDBFF) => pending = Some(cp),
            (None, 0xDC00..=0xDFFF) => return Err("invalid Unicode surrogate pair"),
            (None, _) => out.push(char::from_u32(cp).ok_or("invalid Unicode escape value")?),
        }
    }
    if pending.is_some() {
        return Err("invalid Unicode surrogate pair");
    }
    Ok(out)
}

/// `name := value` (gram.y `param_name COLON_EQUALS a_expr`, the pre-9.5 named-argument notation) means
/// `name => value`; sqlparser rejects `:=` in PostgreSQL call arguments.
fn legacy_named_arguments(toks: Vec<TokenWithSpan>) -> Vec<TokenWithSpan> {
    let mut out: Vec<TokenWithSpan> = Vec::with_capacity(toks.len());
    for t in toks {
        if matches!(t.token, Token::Assignment) && matches!(last_significant(&out), Some(Token::Word(_))) {
            out.push(TokenWithSpan::new(Token::RArrow, t.span));
        } else {
            out.push(t);
        }
    }
    out
}

/// Stands one plain prefix token (`-`, spanning the whole spelling) in for a prefix `OPERATOR([schema.]op)`:
/// sqlparser only reads `OPERATOR(..)` as a binary operator. The binder recovers the schema and operator
/// name from the source text (`binder::expr::qualified_prefix_operator`) and resolves it by name.
fn prefix_qualified_operators(toks: Vec<TokenWithSpan>, src: &SourceMap) -> Vec<TokenWithSpan> {
    let mut out: Vec<TokenWithSpan> = Vec::with_capacity(toks.len());
    let mut i = 0;
    while i < toks.len() {
        if bare_word(&toks[i].token).as_deref() == Some("OPERATOR") && !follows_operand(&out) {
            if let Some(span) = prefix_operator_extent(&toks, i, src) {
                out.push(TokenWithSpan::new(Token::Minus, span.0));
                i = span.1 + 1;
                continue;
            }
        }
        out.push(toks[i].clone());
        i += 1;
    }
    out
}

/// For `OPERATOR` at `toks[at]` followed by `( [schema .] op )` and then the start of an operand: the span of
/// the whole spelling and the index of its closing parenthesis.
fn prefix_operator_extent(toks: &[TokenWithSpan], at: usize, src: &SourceMap) -> Option<(Span, usize)> {
    let open = next_significant(toks, at + 1)?;
    if !matches!(toks[open].token, Token::LParen) {
        return None;
    }
    let close = (open + 1..toks.len()).find(|&k| matches!(toks[k].token, Token::RParen))?;
    let after = next_significant(toks, close + 1)?;
    if matches!(toks[after].token, Token::RParen | Token::RBracket | Token::Comma | Token::SemiColon | Token::EOF) {
        return None;
    }
    let (_, body_start) = token_bytes(src, &toks[open].span).ok()?;
    let (body_end, _) = token_bytes(src, &toks[close].span).ok()?;
    let body = src.slice(body_start, body_end).trim();
    let (schema, op) = match body.rfind('.') {
        Some(dot) => (body[..dot].trim(), body[dot + 1..].trim()),
        None => ("", body),
    };
    let op_ok = !op.is_empty() && op.chars().all(|c| "+-*/<>=~!@#%^&|`?".contains(c));
    let schema_ok = schema.chars().all(|c| c.is_alphanumeric() || matches!(c, '_' | '$' | '.' | '"' | ' '));
    if !op_ok || !schema_ok {
        return None;
    }
    Some((Span::new(toks[at].span.start, toks[close].span.end), close))
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

/// `SET LOCAL|SESSION SESSION AUTHORIZATION ..` (gram.y `SET set_scope SESSION AUTHORIZATION`): sqlparser
/// reads `AUTHORIZATION` right after the scope keyword, so the second `SESSION` is dropped.
fn set_scoped_session_authorization(toks: &[TokenWithSpan]) -> Vec<TokenWithSpan> {
    let sig: Vec<usize> = (0..toks.len()).filter(|&i| !matches!(toks[i].token, Token::Whitespace(_))).take(4).collect();
    let word = |k: usize| sig.get(k).and_then(|&i| bare_word(&toks[i].token));
    if word(0).as_deref() == Some("SET")
        && matches!(word(1).as_deref(), Some("LOCAL" | "SESSION"))
        && word(2).as_deref() == Some("SESSION")
        && word(3).as_deref() == Some("AUTHORIZATION")
    {
        let drop_at = sig[2];
        return toks.iter().enumerate().filter(|(i, _)| *i != drop_at).map(|(_, t)| t.clone()).collect();
    }
    toks.to_vec()
}

/// `SET [LOCAL|SESSION] SCHEMA 'x'` and `SET [LOCAL|SESSION] XML OPTION DOCUMENT|CONTENT` (gram.y
/// `set_rest_more`): sqlparser knows neither, so they become `SET search_path TO 'x'` and
/// `SET xmloption TO DOCUMENT|CONTENT`.
fn set_schema_and_xml_option(toks: &[TokenWithSpan]) -> Vec<TokenWithSpan> {
    let sig: Vec<usize> = (0..toks.len()).filter(|&i| !matches!(toks[i].token, Token::Whitespace(_))).take(6).collect();
    let word = |k: usize| sig.get(k).and_then(|&i| bare_word(&toks[i].token));
    if word(0).as_deref() != Some("SET") {
        return toks.to_vec();
    }
    let k = if matches!(word(1).as_deref(), Some("LOCAL" | "SESSION")) { 2 } else { 1 };
    let name_at = |kw: &str| word(k).as_deref() == Some(kw);
    if name_at("SCHEMA") && matches!(sig.get(k + 1).map(|&i| &toks[i].token), Some(Token::SingleQuotedString(_))) {
        let at = sig[k];
        let mut out = Vec::with_capacity(toks.len() + 1);
        for (i, t) in toks.iter().enumerate() {
            if i == at {
                out.push(TokenWithSpan::new(Token::make_word("search_path", None), t.span));
                out.push(TokenWithSpan::new(Token::make_keyword("TO"), Span::new(t.span.end, t.span.end)));
            } else {
                out.push(t.clone());
            }
        }
        return out;
    }
    if name_at("XML")
        && word(k + 1).as_deref() == Some("OPTION")
        && matches!(word(k + 2).as_deref(), Some("DOCUMENT" | "CONTENT"))
    {
        let (xml, option) = (sig[k], sig[k + 1]);
        return toks
            .iter()
            .enumerate()
            .map(|(i, t)| {
                if i == xml {
                    TokenWithSpan::new(Token::make_word("xmloption", None), t.span)
                } else if i == option {
                    TokenWithSpan::new(Token::make_keyword("TO"), t.span)
                } else {
                    t.clone()
                }
            })
            .collect();
    }
    toks.to_vec()
}

/// `NOTIFY channel, $$payload$$`: PG's payload is any `Sconst`, but sqlparser 0.63 `parse_notify` reads the
/// payload with `parse_literal_string`, which rejects dollar-quoted strings. The payload token (right after
/// the comma that follows the channel) becomes a plain single-quoted string with the same contents.
fn notify_dollar_payload(toks: &[TokenWithSpan]) -> Vec<TokenWithSpan> {
    let sig: Vec<usize> = (0..toks.len()).filter(|&i| !matches!(toks[i].token, Token::Whitespace(_))).collect();
    let mut out = toks.to_vec();
    if sig.len() >= 4
        && bare_word(&toks[sig[0]].token).as_deref() == Some("NOTIFY")
        && matches!(toks[sig[2]].token, Token::Comma)
    {
        let at = sig[3];
        if let Token::DollarQuotedString(d) = &toks[at].token {
            out[at] = TokenWithSpan::new(Token::SingleQuotedString(d.value.clone()), toks[at].span);
        }
    }
    out
}

/// Value of a word token used as an identifier (unquoted words fold to lower case).
fn ident_text(t: &Token) -> Option<String> {
    match t {
        Token::Word(w) => Some(if w.quote_style.is_some() { w.value.clone() } else { w.value.to_ascii_lowercase() }),
        _ => None,
    }
}

/// The `USING access_method` and `TABLESPACE name` clauses of `CREATE TABLE [AS]` and `CREATE MATERIALIZED
/// VIEW` (gram.y `OptAccessMethod`, `OptTableSpace`). sqlparser reads neither on these statements. PGArbor
/// has the built-in access methods only (`heap` for tables, the index methods for indexes) and the
/// `pg_default` tablespace, so the clauses are validated here (PG `get_table_am_oid`, `get_tablespace_oid`)
/// and removed when they name the defaults: 55000 `access method "btree" is not of type TABLE`, 42704 for
/// unknown names, and the `pg_global` rejection of `DefineRelation`.
fn table_storage_clauses(toks: &[TokenWithSpan]) -> Result<Vec<TokenWithSpan>> {
    let sig: Vec<usize> = (0..toks.len()).filter(|&i| !matches!(toks[i].token, Token::Whitespace(_))).collect();
    let words: Vec<String> = sig.iter().take(8).map(|&i| bare_word(&toks[i].token).unwrap_or_default()).collect();
    let mut k = 1;
    if words.first().map(String::as_str) != Some("CREATE") {
        return Ok(toks.to_vec());
    }
    while matches!(words.get(k).map(String::as_str), Some("OR" | "REPLACE" | "GLOBAL" | "LOCAL" | "TEMP" | "TEMPORARY" | "UNLOGGED")) {
        k += 1;
    }
    let is_table = words.get(k).map(String::as_str) == Some("TABLE");
    let is_matview = words.get(k).map(String::as_str) == Some("MATERIALIZED") && words.get(k + 1).map(String::as_str) == Some("VIEW");
    if !is_table && !is_matview {
        return Ok(toks.to_vec());
    }
    let mut depth = 0usize;
    let mut remove: Vec<usize> = Vec::new();
    let mut n = 0;
    while n < sig.len() {
        let i = sig[n];
        match &toks[i].token {
            Token::LParen => depth += 1,
            Token::RParen => depth = depth.saturating_sub(1),
            t if depth == 0 => match bare_word(t).as_deref() {
                Some("AS") => break,
                Some(kw @ ("USING" | "TABLESPACE")) => {
                    if let Some(name) = sig.get(n + 1).and_then(|&j| ident_text(&toks[j].token)) {
                        if kw == "USING" {
                            match name.as_str() {
                                "heap" => {}
                                "btree" | "hash" | "gist" | "gin" | "spgist" | "brin" => {
                                    return Err(PgError::new(
                                        pgarbor_common::SqlState::OBJECT_NOT_IN_PREREQUISITE_STATE,
                                        format!("access method \"{name}\" is not of type TABLE"),
                                    ));
                                }
                                _ => {
                                    return Err(PgError::new(
                                        pgarbor_common::SqlState::UNDEFINED_OBJECT,
                                        format!("access method \"{name}\" does not exist"),
                                    ));
                                }
                            }
                        } else {
                            match name.as_str() {
                                "pg_default" => {}
                                "pg_global" => {
                                    return Err(PgError::new(
                                        pgarbor_common::SqlState::INVALID_PARAMETER_VALUE,
                                        "only shared relations can be placed in pg_global tablespace",
                                    ));
                                }
                                _ => {
                                    return Err(PgError::new(
                                        pgarbor_common::SqlState::UNDEFINED_OBJECT,
                                        format!("tablespace \"{name}\" does not exist"),
                                    ));
                                }
                            }
                        }
                        remove.push(i);
                        remove.push(sig[n + 1]);
                        n += 1;
                    }
                }
                _ => {}
            },
            _ => {}
        }
        n += 1;
    }
    if remove.is_empty() {
        return Ok(toks.to_vec());
    }
    Ok(toks.iter().enumerate().filter(|(i, _)| !remove.contains(i)).map(|(_, t)| t.clone()).collect())
}

/// Name of the marker role that stands for the `MAINTAIN` privilege (PG 17+) in a GRANT/REVOKE privilege
/// list: sqlparser 0.63 has no such action, so [`maintain_privilege_marker`] rewrites it into the action
/// `ROLE "<marker>"`, which `binder::ddl_grant::privilege_bits` maps back to the MAINTAIN acl bit.
pub const MAINTAIN_PRIVILEGE_MARKER: &str = "\0pgarbor maintain";

/// Rewrite the unquoted `MAINTAIN` privilege of a GRANT/REVOKE privilege list (a word at depth zero before
/// the `ON` keyword, right after the verb, `FOR` or a comma) into `ROLE "<MAINTAIN_PRIVILEGE_MARKER>"`.
/// Statements without a top-level `ON` (role membership grants) are unchanged, so a role named `maintain`
/// still works there.
fn maintain_privilege_marker(toks: &[TokenWithSpan]) -> Vec<TokenWithSpan> {
    let sig: Vec<usize> = (0..toks.len()).filter(|&i| !matches!(toks[i].token, Token::Whitespace(_))).collect();
    if !matches!(sig.first().and_then(|&i| bare_word(&toks[i].token)).as_deref(), Some("GRANT" | "REVOKE")) {
        return toks.to_vec();
    }
    let mut depth = 0usize;
    let mut on_at = None;
    for (k, &i) in sig.iter().enumerate() {
        match &toks[i].token {
            Token::LParen => depth += 1,
            Token::RParen => depth = depth.saturating_sub(1),
            t if depth == 0 && bare_word(t).as_deref() == Some("ON") => {
                on_at = Some(k);
                break;
            }
            _ => {}
        }
    }
    let Some(on_at) = on_at else { return toks.to_vec() };
    let mut out = toks.to_vec();
    let mut depth = 0usize;
    let mut found = Vec::new();
    for (k, &i) in sig.iter().enumerate().take(on_at) {
        match &toks[i].token {
            Token::LParen => depth += 1,
            Token::RParen => depth = depth.saturating_sub(1),
            t if depth == 0 && k > 0 && matches!(bare_word(t).as_deref(), Some("MAINTAIN" | "TEMP")) => {
                let prev = &toks[sig[k - 1]].token;
                let prev_ok = matches!(prev, Token::Comma) || matches!(bare_word(prev).as_deref(), Some("GRANT" | "REVOKE" | "FOR"));
                let next_ok = sig.get(k + 1).is_some_and(|&j| matches!(toks[j].token, Token::Comma) || bare_word(&toks[j].token).as_deref() == Some("ON"));
                if prev_ok && next_ok {
                    found.push((i, bare_word(t).as_deref() == Some("TEMP")));
                }
            }
            _ => {}
        }
    }
    // Replace from the back so earlier indexes stay valid.
    for &(i, is_temp) in found.iter().rev() {
        let span = toks[i].span;
        if is_temp {
            // sqlparser only knows `TEMPORARY`; PG accepts `TEMP` as a synonym privilege name.
            out[i] = TokenWithSpan::new(Token::make_keyword("TEMPORARY"), span);
            continue;
        }
        out[i] = TokenWithSpan::new(Token::make_keyword("ROLE"), span);
        out.insert(i + 1, marker_word(MAINTAIN_PRIVILEGE_MARKER, span));
    }
    out
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

/// Suffix of the function name `JSON_OBJECT` / `JSON_OBJECTAGG` when the call carried `WITH UNIQUE KEYS`.
/// sqlparser 0.63 cannot parse that clause, so [`mark_json_unique_keys`] removes it and renames the
/// function to a quoted identifier ending in this marker (it holds a NUL, which PostgreSQL identifiers
/// cannot contain); `binder::expr_json` recognizes it.
pub const JSON_UNIQUE_KEYS_MARKER: &str = "\0pgarbor json unique keys";

/// Remove `WITH UNIQUE [KEYS]` / `WITHOUT UNIQUE [KEYS]` from `JSON_OBJECT(..)` and `JSON_OBJECTAGG(..)`
/// calls. `WITH` additionally renames the function with [`JSON_UNIQUE_KEYS_MARKER`]; `WITHOUT` is the
/// default and is simply dropped.
fn mark_json_unique_keys(toks: &[TokenWithSpan]) -> Vec<TokenWithSpan> {
    let sig: Vec<usize> = (0..toks.len()).filter(|&i| !matches!(toks[i].token, Token::Whitespace(_))).collect();
    let word = |k: usize| sig.get(k).and_then(|&i| bare_word(&toks[i].token));
    if !(0..sig.len()).any(|k| word(k).as_deref() == Some("UNIQUE")) {
        return toks.to_vec();
    }
    let mut dropped = vec![false; toks.len()];
    let mut renamed: Vec<usize> = Vec::new();
    // One entry per open parenthesis: the index of the JSON constructor name before it, if any.
    let mut stack: Vec<Option<usize>> = Vec::new();
    for k in 0..sig.len() {
        match &toks[sig[k]].token {
            Token::LParen => {
                let name = match k.checked_sub(1).and_then(|p| word(p)).as_deref() {
                    Some("JSON_OBJECT" | "JSON_OBJECTAGG") => Some(sig[k - 1]),
                    _ => None,
                };
                stack.push(name);
            }
            Token::RParen => {
                stack.pop();
            }
            _ => {
                let Some(Some(name)) = stack.last().copied() else { continue };
                let with = match word(k).as_deref() {
                    Some("WITH") => true,
                    Some("WITHOUT") => false,
                    _ => continue,
                };
                if word(k + 1).as_deref() != Some("UNIQUE") {
                    continue;
                }
                let mut last = k + 1;
                if word(k + 2).as_deref() == Some("KEYS") {
                    last = k + 2;
                }
                for &i in &sig[k..=last] {
                    dropped[i] = true;
                }
                if with {
                    renamed.push(name);
                }
            }
        }
    }
    let mut out = Vec::with_capacity(toks.len());
    for (i, t) in toks.iter().enumerate() {
        if dropped[i] {
            continue;
        }
        match (&t.token, renamed.contains(&i)) {
            (Token::Word(w), true) => {
                let value = format!("{}{}", w.value.to_ascii_lowercase(), JSON_UNIQUE_KEYS_MARKER);
                let word = Word { value, quote_style: Some('"'), keyword: Keyword::NoKeyword };
                out.push(TokenWithSpan::new(Token::Word(word), t.span));
            }
            _ => out.push(t.clone()),
        }
    }
    out
}

/// Parse-analysis errors of SQL/JSON clauses that PG reports before anything runs (parse_expr.c
/// `transformJsonFuncExpr`, parse_jsontable.c `transformJsonTable`): `WITH [CONDITIONAL|UNCONDITIONAL]
/// [ARRAY] WRAPPER OMIT QUOTES` (JSON_QUERY and JSON_TABLE columns), a top-level `JSON_TABLE` `ON ERROR`
/// behavior other than `EMPTY [ARRAY]` / `ERROR`, and a second `FOR ORDINALITY` column in one `COLUMNS`
/// list. All are 42601; sqlparser 0.63 cannot parse these calls, so they are checked on the tokens.
fn check_json_clause_errors(toks: &[TokenWithSpan], src: &SourceMap) -> Result<()> {
    let sig: Vec<usize> = (0..toks.len()).filter(|&i| !matches!(toks[i].token, Token::Whitespace(_))).collect();
    if !sig.iter().any(|&i| matches!(bare_word(&toks[i].token).as_deref(), Some("JSON_QUERY" | "JSON_TABLE"))) {
        return Ok(());
    }
    let word = |k: usize| sig.get(k).and_then(|&i| bare_word(&toks[i].token));
    let tok = |k: usize| sig.get(k).map(|&i| &toks[i].token);
    let at = |e: PgError, k: usize| match src.char_pos(toks[sig[k]].span.start) {
        Some(pos) => e.with_position(pos),
        None => e,
    };
    let close_of = |open: usize| -> Option<usize> {
        let mut depth = 0usize;
        for m in open..sig.len() {
            match tok(m) {
                Some(Token::LParen) => depth += 1,
                Some(Token::RParen) => {
                    depth = depth.saturating_sub(1);
                    if depth == 0 {
                        return Some(m);
                    }
                }
                _ => {}
            }
        }
        None
    };
    // Top-level ON ERROR of JSON_TABLE.
    for k in 1..sig.len() {
        if word(k - 1).as_deref() != Some("JSON_TABLE") || !matches!(tok(k), Some(Token::LParen)) {
            continue;
        }
        let Some(end) = close_of(k) else { continue };
        let mut depth = 0usize;
        let mut cols_close = None;
        let mut m = k;
        while m < end {
            match tok(m) {
                Some(Token::LParen) => depth += 1,
                Some(Token::RParen) => depth = depth.saturating_sub(1),
                _ if depth == 1 && word(m).as_deref() == Some("COLUMNS") && matches!(tok(m + 1), Some(Token::LParen)) => {
                    cols_close = close_of(m + 1);
                    break;
                }
                _ => {}
            }
            m += 1;
        }
        let Some(cc) = cols_close else { continue };
        let first = cc + 1;
        if first < end && matches!(word(first).as_deref(), Some("NULL" | "TRUE" | "FALSE" | "UNKNOWN" | "DEFAULT")) {
            let has_on_error = (first..end).any(|j| word(j).as_deref() == Some("ON") && word(j + 1).as_deref() == Some("ERROR"));
            if has_on_error {
                let e = PgError::syntax("invalid ON ERROR behavior", None)
                    .with_detail("Only EMPTY [ ARRAY ] or ERROR is allowed in the top-level ON ERROR clause.");
                return Err(at(e, first));
            }
        }
    }
    // A second FOR ORDINALITY column in one COLUMNS list.
    for k in 1..sig.len() {
        if word(k - 1).as_deref() != Some("COLUMNS") || !matches!(tok(k), Some(Token::LParen)) {
            continue;
        }
        let Some(end) = close_of(k) else { continue };
        let mut depth = 0usize;
        let mut start = k + 1;
        let mut seen = false;
        for m in k + 1..=end {
            let boundary = match tok(m) {
                Some(Token::LParen) => {
                    depth += 1;
                    false
                }
                Some(Token::RParen) if m < end => {
                    depth = depth.saturating_sub(1);
                    false
                }
                Some(Token::Comma) => depth == 0,
                Some(Token::RParen) => true,
                _ => false,
            };
            if !boundary {
                continue;
            }
            if word(start + 1).as_deref() == Some("FOR") && word(start + 2).as_deref() == Some("ORDINALITY") {
                if seen {
                    return Err(at(PgError::syntax("only one FOR ORDINALITY column is allowed", None), start));
                }
                seen = true;
            }
            start = m + 1;
        }
    }
    // WITH WRAPPER together with OMIT QUOTES.
    for q in 1..sig.len() {
        if word(q).as_deref() != Some("OMIT") || word(q + 1).as_deref() != Some("QUOTES") {
            continue;
        }
        if word(q - 1).as_deref() != Some("WRAPPER") {
            continue;
        }
        let mut w = q - 1;
        if w > 0 && word(w - 1).as_deref() == Some("ARRAY") {
            w -= 1;
        }
        if w > 0 && matches!(word(w - 1).as_deref(), Some("CONDITIONAL" | "UNCONDITIONAL")) {
            w -= 1;
        }
        if w == 0 || word(w - 1).as_deref() != Some("WITH") {
            continue;
        }
        let w = w - 1;
        // Enclosing parenthesis of the clause.
        let mut depth = 0usize;
        let mut open = None;
        for m in (0..w).rev() {
            match tok(m) {
                Some(Token::RParen) => depth += 1,
                Some(Token::LParen) => {
                    if depth == 0 {
                        open = Some(m);
                        break;
                    }
                    depth -= 1;
                }
                _ => {}
            }
        }
        let Some(open) = open else { continue };
        let owner = open.checked_sub(1).and_then(|p| word(p));
        let pos_at = match owner.as_deref() {
            Some("JSON_QUERY") => open - 1,
            Some("COLUMNS") => {
                let mut d = 0usize;
                let mut start = open + 1;
                for m in (open + 1..w).rev() {
                    match tok(m) {
                        Some(Token::RParen) => d += 1,
                        Some(Token::LParen) => d = d.saturating_sub(1),
                        Some(Token::Comma) if d == 0 => {
                            start = m + 1;
                            break;
                        }
                        _ => {}
                    }
                }
                start
            }
            _ => continue,
        };
        let e = PgError::syntax("SQL/JSON QUOTES behavior must not be specified when WITH WRAPPER is used", None);
        return Err(at(e, pos_at));
    }
    Ok(())
}

/// `JSON_VALUE` / `JSON_QUERY` / `JSON_EXISTS` with `PASSING`, `ON ERROR`, `ON EMPTY`, `WRAPPER` or
/// `QUOTES` clauses: sqlparser 0.63 cannot parse them and PGArbor has no SQL/JSON query functions yet,
/// so they are a leased 0A000 instead of a syntax error.
fn reject_json_query_clauses(toks: &[TokenWithSpan], src: &SourceMap) -> Result<()> {
    let sig: Vec<usize> = (0..toks.len()).filter(|&i| !matches!(toks[i].token, Token::Whitespace(_))).collect();
    let word = |k: usize| sig.get(k).and_then(|&i| bare_word(&toks[i].token));
    let mut stack: Vec<Option<usize>> = Vec::new();
    for k in 0..sig.len() {
        match &toks[sig[k]].token {
            Token::LParen => {
                let name = match k.checked_sub(1).and_then(|p| word(p)).as_deref() {
                    Some("JSON_VALUE" | "JSON_QUERY" | "JSON_EXISTS" | "JSON_TABLE") => Some(sig[k - 1]),
                    _ => None,
                };
                stack.push(name);
            }
            Token::RParen => {
                stack.pop();
            }
            _ => {
                // JSON_TABLE clauses also occur inside its (nested) COLUMNS lists.
                let direct = stack.last().copied().flatten();
                let table = stack
                    .iter()
                    .rev()
                    .filter_map(|n| *n)
                    .find(|&n| bare_word(&toks[n].token).as_deref() == Some("JSON_TABLE"));
                let Some(name) = direct.or(table) else { continue };
                let is_table = bare_word(&toks[name].token).as_deref() == Some("JSON_TABLE");
                let prev_ok = k > 0 && !matches!(toks[sig[k - 1]].token, Token::LParen | Token::Comma);
                let clause = match word(k).as_deref() {
                    Some("PASSING") => true,
                    Some("WRAPPER" | "QUOTES") if is_table => prev_ok,
                    Some("ON") if is_table => prev_ok && matches!(word(k + 1).as_deref(), Some("ERROR" | "EMPTY")),
                    _ if is_table => false,
                    Some("ON") if matches!(word(k + 1).as_deref(), Some("ERROR" | "EMPTY")) => true,
                    Some("PASSING" | "WRAPPER" | "QUOTES") => true,
                    _ => false,
                };
                if clause {
                    let fname = bare_word(&toks[name].token).unwrap_or_default();
                    let err = PgError::unimplemented("SQL-10", format!("{fname} with PASSING, WRAPPER, QUOTES, ON ERROR or ON EMPTY clauses"));
                    return Err(match src.char_pos(toks[name].span.start) {
                        Some(pos) => err.with_position(pos),
                        None => err,
                    });
                }
            }
        }
    }
    Ok(())
}

/// `JSON(expr FORMAT JSON [ENCODING e] [WITH|WITHOUT UNIQUE [KEYS]])` (gram.y `json_func_expr`): sqlparser
/// 0.63 cannot parse the `FORMAT` / `UNIQUE` clauses and PGArbor has no `JSON()` constructor yet, so a
/// call that carries one is a leased 0A000 instead of a syntax error.
fn reject_json_constructor_clauses(toks: &[TokenWithSpan], src: &SourceMap) -> Result<()> {
    let sig: Vec<usize> = (0..toks.len()).filter(|&i| !matches!(toks[i].token, Token::Whitespace(_))).collect();
    let word = |k: usize| sig.get(k).and_then(|&i| bare_word(&toks[i].token));
    for k in 0..sig.len() {
        if word(k).as_deref() != Some("JSON") || !matches!(sig.get(k + 1).map(|&i| &toks[i].token), Some(Token::LParen)) {
            continue;
        }
        if k > 0 && matches!(toks[sig[k - 1]].token, Token::Period | Token::DoubleColon) {
            continue;
        }
        let mut depth = 0usize;
        for m in k + 1..sig.len() {
            match &toks[sig[m]].token {
                Token::LParen => depth += 1,
                Token::RParen => {
                    depth = depth.saturating_sub(1);
                    if depth == 0 {
                        break;
                    }
                }
                _ if depth == 1 => {
                    let clause = match word(m).as_deref() {
                        Some("FORMAT") => true,
                        Some("WITH" | "WITHOUT") => word(m + 1).as_deref() == Some("UNIQUE"),
                        _ => false,
                    };
                    if clause {
                        let err = PgError::unimplemented("SQL-10", "JSON() constructor with FORMAT or UNIQUE KEYS clauses");
                        return Err(match src.char_pos(toks[sig[k]].span.start) {
                            Some(pos) => err.with_position(pos),
                            None => err,
                        });
                    }
                }
                _ => {}
            }
        }
    }
    Ok(())
}

/// `JSON_ARRAY(query [RETURNING t])` (gram.y `json_array_constructor` with a `select_no_parens` argument):
/// sqlparser 0.63 parses the call arguments as expressions and fails at the subquery's select list, and
/// PGArbor has no query form of `JSON_ARRAY` yet, so it is a leased 0A000 instead of a syntax error.
fn reject_json_array_subquery(toks: &[TokenWithSpan], src: &SourceMap) -> Result<()> {
    let sig: Vec<usize> = (0..toks.len()).filter(|&i| !matches!(toks[i].token, Token::Whitespace(_))).collect();
    let word = |k: usize| sig.get(k).and_then(|&i| bare_word(&toks[i].token));
    for k in 0..sig.len() {
        if word(k).as_deref() != Some("JSON_ARRAY") || !matches!(sig.get(k + 1).map(|&i| &toks[i].token), Some(Token::LParen)) {
            continue;
        }
        if k > 0 && matches!(toks[sig[k - 1]].token, Token::Period | Token::DoubleColon) {
            continue;
        }
        if matches!(word(k + 2).as_deref(), Some("SELECT" | "WITH" | "VALUES" | "TABLE")) {
            let err = PgError::unimplemented("SQL-10", "JSON_ARRAY with a subquery argument");
            return Err(match src.char_pos(toks[sig[k]].span.start) {
                Some(pos) => err.with_position(pos),
                None => err,
            });
        }
    }
    Ok(())
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

/// `CREATE DOMAIN name [AS] type`: gram.y makes `AS` optional while sqlparser requires it. Inserts `AS` after
/// the (possibly qualified) domain name when it is missing.
fn domain_optional_as(toks: &[TokenWithSpan]) -> Vec<TokenWithSpan> {
    let sig: Vec<usize> = (0..toks.len()).filter(|&i| !matches!(toks[i].token, Token::Whitespace(_))).collect();
    let is = |k: usize, w: &str| sig.get(k).is_some_and(|&i| bare_word(&toks[i].token).as_deref() == Some(w));
    if !(is(0, "CREATE") && is(1, "DOMAIN")) {
        return toks.to_vec();
    }
    // Skip the name: word (. word)*.
    let mut k = 2;
    if !sig.get(k).is_some_and(|&i| matches!(toks[i].token, Token::Word(_))) {
        return toks.to_vec();
    }
    k += 1;
    while sig.get(k).is_some_and(|&i| matches!(toks[i].token, Token::Period))
        && sig.get(k + 1).is_some_and(|&i| matches!(toks[i].token, Token::Word(_)))
    {
        k += 2;
    }
    let Some(&next) = sig.get(k) else {
        return toks.to_vec();
    };
    if is(k, "AS") || matches!(toks[next].token, Token::EOF | Token::SemiColon) {
        return toks.to_vec();
    }
    let span = toks[next].span;
    let mut out = Vec::with_capacity(toks.len() + 2);
    out.extend_from_slice(&toks[..next]);
    out.push(TokenWithSpan::new(Token::make_keyword("AS"), span));
    out.push(TokenWithSpan::new(Token::Whitespace(sqlparser::tokenizer::Whitespace::Space), span));
    out.extend_from_slice(&toks[next..]);
    out
}

/// Marker identifier standing for a domain `NOT NULL` constraint inside `CHECK (marker)` (sqlparser's
/// `CREATE DOMAIN` constraint list is table constraints only). Inserted by [`domain_null_constraints`];
/// the binder (`ddl_objects::bind_create_domain`) maps it back. It contains a NUL, so SQL cannot spell it.
pub const DOMAIN_NOT_NULL_MARKER: &str = "\0pgarbor domain not null";

/// Marker identifier standing for a domain `NULL` constraint (see [`DOMAIN_NOT_NULL_MARKER`]).
pub const DOMAIN_NULL_MARKER: &str = "\0pgarbor domain null";

/// `CREATE DOMAIN` accepts `[CONSTRAINT n] NOT NULL`, `NULL`, and `DEFAULT` after the constraints (gram.y
/// `ColQualList`), while sqlparser parses only `DEFAULT` then table constraints. Rewrites
/// `NOT NULL` / `NULL` into `CHECK (marker)` and moves the `DEFAULT` clause before the first constraint.
fn domain_null_constraints(toks: &[TokenWithSpan]) -> Vec<TokenWithSpan> {
    let sig: Vec<usize> = (0..toks.len()).filter(|&i| !matches!(toks[i].token, Token::Whitespace(_))).collect();
    let is = |k: usize, w: &str| sig.get(k).is_some_and(|&i| bare_word(&toks[i].token).as_deref() == Some(w));
    if !(is(0, "CREATE") && is(1, "DOMAIN")) {
        return toks.to_vec();
    }
    // Pass 1: replace NOT NULL / NULL (depth 0) by CHECK (marker).
    let mut repl: Vec<(usize, usize, &'static str)> = Vec::new(); // (first token idx, last token idx, marker)
    let mut depth = 0usize;
    let mut k = 2;
    while k < sig.len() {
        let i = sig[k];
        match &toks[i].token {
            Token::LParen => depth += 1,
            Token::RParen => depth = depth.saturating_sub(1),
            _ if depth > 0 => {}
            _ => {
                let prev = if k > 0 { Some(&toks[sig[k - 1]].token) } else { None };
                let prev_word = prev.and_then(bare_word);
                if is(k, "NOT") && is(k + 1, "NULL") && prev_word.as_deref() != Some("IS") {
                    repl.push((i, sig[k + 1], DOMAIN_NOT_NULL_MARKER));
                    k += 1;
                } else if is(k, "NULL") {
                    const STOP: &[&str] = &[
                        "DEFAULT", "AND", "OR", "NOT", "IS", "DISTINCT", "FROM", "THEN", "ELSE", "WHEN", "IN", "LIKE",
                        "ILIKE", "BETWEEN", "CASE", "AS", "ESCAPE", "SIMILAR",
                    ];
                    let operand_end = match prev {
                        Some(Token::Word(w)) => w.quote_style.is_some() || !STOP.contains(&w.value.to_ascii_uppercase().as_str()),
                        Some(Token::RParen) | Some(Token::Number(..)) | Some(Token::SingleQuotedString(_)) => true,
                        _ => false,
                    };
                    if operand_end {
                        repl.push((i, i, DOMAIN_NULL_MARKER));
                    }
                }
            }
        }
        k += 1;
    }
    let mut out: Vec<TokenWithSpan> = Vec::with_capacity(toks.len() + repl.len() * 3);
    let mut i = 0;
    while i < toks.len() {
        if let Some(&(first, last, marker)) = repl.iter().find(|r| r.0 == i) {
            let span = toks[first].span;
            let word = Word { value: marker.to_string(), quote_style: Some('"'), keyword: Keyword::NoKeyword };
            out.push(TokenWithSpan::new(Token::make_keyword("CHECK"), span));
            out.push(TokenWithSpan::new(Token::LParen, span));
            out.push(TokenWithSpan::new(Token::Word(word), span));
            out.push(TokenWithSpan::new(Token::RParen, span));
            i = last + 1;
        } else {
            out.push(toks[i].clone());
            i += 1;
        }
    }
    // Pass 2: move a DEFAULT clause that follows a constraint before the first constraint.
    let sig2: Vec<usize> = (0..out.len()).filter(|&i| !matches!(out[i].token, Token::Whitespace(_))).collect();
    let mut depth = 0usize;
    let mut first_constraint: Option<usize> = None;
    let mut default_at: Option<usize> = None;
    let mut default_end: Option<usize> = None;
    for &i in sig2.iter().skip(2) {
        match &out[i].token {
            Token::LParen => depth += 1,
            Token::RParen => depth = depth.saturating_sub(1),
            _ if depth > 0 => {}
            Token::EOF | Token::SemiColon => {
                if default_at.is_some() && default_end.is_none() {
                    default_end = Some(i);
                }
            }
            t => match bare_word(t).as_deref() {
                Some("CONSTRAINT") | Some("CHECK") => {
                    if first_constraint.is_none() {
                        first_constraint = Some(i);
                    }
                    if default_at.is_some() && default_end.is_none() {
                        default_end = Some(i);
                    }
                }
                Some("DEFAULT") if default_at.is_none() => default_at = Some(i),
                _ => {}
            },
        }
    }
    let (Some(fc), Some(ds)) = (first_constraint, default_at) else {
        return out;
    };
    if ds < fc {
        return out;
    }
    let de = default_end.unwrap_or(out.len());
    let mut moved: Vec<TokenWithSpan> = Vec::with_capacity(out.len());
    moved.extend_from_slice(&out[..fc]);
    moved.extend_from_slice(&out[ds..de]);
    moved.extend_from_slice(&out[fc..ds]);
    moved.extend_from_slice(&out[de..]);
    moved
}

/// `CREATE [OR REPLACE] FUNCTION|PROCEDURE name(p OUT type, ..)`: PG `func_arg` also accepts the mode AFTER the
/// parameter name (`param_name arg_class func_type`), while sqlparser only takes it before. Swap the two
/// tokens (spans stay in place) so the parameter reads `OUT p type`.
fn param_name_before_mode(toks: &[TokenWithSpan]) -> Vec<TokenWithSpan> {
    let sig: Vec<usize> = (0..toks.len()).filter(|&i| !matches!(toks[i].token, Token::Whitespace(_))).collect();
    let words: Vec<String> = sig.iter().filter_map(|&i| bare_word(&toks[i].token)).take(4).collect();
    let words: Vec<&str> = words.iter().map(String::as_str).collect();
    if !matches!(words.as_slice(), ["CREATE", "FUNCTION" | "PROCEDURE", ..] | ["CREATE", "OR", "REPLACE", "FUNCTION" | "PROCEDURE", ..]) {
        return toks.to_vec();
    }
    let Some(kind_at) = sig.iter().position(|&i| matches!(bare_word(&toks[i].token).as_deref(), Some("FUNCTION" | "PROCEDURE"))) else {
        return toks.to_vec();
    };
    let mut k = kind_at + 1;
    while k < sig.len() && matches!(toks[sig[k]].token, Token::Word(_) | Token::Period) {
        k += 1;
    }
    if k >= sig.len() || !matches!(toks[sig[k]].token, Token::LParen) {
        return toks.to_vec();
    }
    let is_mode = |i: usize| matches!(bare_word(&toks[i].token).as_deref(), Some("IN" | "OUT" | "INOUT" | "VARIADIC"));
    let mut swaps: Vec<(usize, usize)> = Vec::new();
    let mut item: Vec<usize> = Vec::new();
    let mut depth = 0usize;
    for &i in &sig[k + 1..] {
        let closes_item = match toks[i].token {
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
        if !closes_item {
            item.push(i);
            continue;
        }
        if let [a, b, _, ..] = item.as_slice() {
            if matches!(toks[*a].token, Token::Word(_)) && !is_mode(*a) && is_mode(*b) {
                swaps.push((*a, *b));
            }
        }
        item.clear();
        if matches!(toks[i].token, Token::RParen) {
            break;
        }
    }
    if swaps.is_empty() {
        return toks.to_vec();
    }
    let mut out = toks.to_vec();
    for (a, b) in swaps {
        let ta = out[a].token.clone();
        out[a].token = out[b].token.clone();
        out[b].token = ta;
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

/// Rewrite `BETWEEN SYMMETRIC low AND high` into `BETWEEN "<SYMMETRIC_MARKER>"(low) AND high`. The lower bound
/// (a PG `b_expr`, so it holds no depth-0 `AND`) ends at the first `AND` outside brackets; when none is found
/// the tokens are left as written and fail as a syntax error at `SYMMETRIC`.
fn mark_between_symmetric(toks: &[TokenWithSpan]) -> Vec<TokenWithSpan> {
    let mut out = Vec::with_capacity(toks.len() + 2);
    let mut i = 0;
    while i < toks.len() {
        out.push(toks[i].clone());
        let is_between = bare_word(&toks[i].token).as_deref() == Some("BETWEEN");
        let sym = if is_between { next_significant(toks, i + 1) } else { None };
        let Some(sym) = sym.filter(|&k| bare_word(&toks[k].token).as_deref() == Some("SYMMETRIC")) else {
            i += 1;
            continue;
        };
        let mut depth = 0usize;
        let mut and_at = None;
        for (k, t) in toks.iter().enumerate().skip(sym + 1) {
            match &t.token {
                Token::LParen | Token::LBracket => depth += 1,
                Token::RParen | Token::RBracket => {
                    if depth == 0 {
                        break;
                    }
                    depth -= 1;
                }
                _ if depth == 0 && bare_word(&t.token).as_deref() == Some("AND") => {
                    and_at = Some(k);
                    break;
                }
                _ => {}
            }
        }
        let Some(and_at) = and_at else {
            i += 1;
            continue;
        };
        out.extend(toks[i + 1..sym].iter().cloned());
        out.push(marker_word(SYMMETRIC_MARKER, toks[sym].span));
        out.push(TokenWithSpan::new(Token::LParen, toks[sym].span));
        out.extend(mark_between_symmetric(&toks[sym + 1..and_at]));
        out.push(TokenWithSpan::new(Token::RParen, toks[and_at].span));
        i = and_at;
    }
    out
}

/// Rewrite the `TRIM` argument forms sqlparser 0.63 cannot express into calls of `ltrim`/`rtrim`/`btrim`
/// (gram.y `trim_list: a_expr FROM expr_list | FROM expr_list | expr_list`, where `TRIM LEADING|TRAILING|BOTH
/// trim_list` is a call of that function on the list): `TRIM([where] FROM list)` and `TRIM(where list, with, commas)`.
/// The plain `TRIM(x)`, `TRIM(x FROM y)` and `TRIM(where x FROM y)` forms are left to sqlparser.
fn trim_list_forms(toks: &[TokenWithSpan]) -> Vec<TokenWithSpan> {
    let mut out = Vec::with_capacity(toks.len());
    let mut i = 0;
    while i < toks.len() {
        let open = if bare_word(&toks[i].token).as_deref() == Some("TRIM") {
            next_significant(toks, i + 1).filter(|&p| matches!(toks[p].token, Token::LParen))
        } else {
            None
        };
        let Some(open) = open else {
            out.push(toks[i].clone());
            i += 1;
            continue;
        };
        let mut first = next_significant(toks, open + 1);
        let where_word = first
            .and_then(|k| bare_word(&toks[k].token))
            .filter(|w| matches!(w.as_str(), "BOTH" | "LEADING" | "TRAILING"));
        if where_word.is_some() {
            first = first.and_then(|k| next_significant(toks, k + 1));
        }
        let from_first = first.is_some_and(|k| bare_word(&toks[k].token).as_deref() == Some("FROM"));
        let mut depth = 0usize;
        let (mut close, mut has_comma, mut has_from) = (None, false, false);
        for (k, t) in toks.iter().enumerate().skip(open + 1) {
            match &t.token {
                Token::LParen | Token::LBracket => depth += 1,
                Token::RParen | Token::RBracket => {
                    if depth == 0 {
                        close = Some(k);
                        break;
                    }
                    depth -= 1;
                }
                Token::Comma if depth == 0 => has_comma = true,
                _ if depth == 0 && k > first.unwrap_or(open) && bare_word(&t.token).as_deref() == Some("FROM") => has_from = true,
                _ => {}
            }
        }
        let rewrite = from_first || (where_word.is_some() && has_comma && !has_from);
        let (Some(close), Some(first), true) = (close, first, rewrite) else {
            out.push(toks[i].clone());
            i += 1;
            continue;
        };
        let name = match where_word.as_deref() {
            Some("LEADING") => "ltrim",
            Some("TRAILING") => "rtrim",
            _ => "btrim",
        };
        let args_from = if from_first { first + 1 } else { first };
        out.push(TokenWithSpan::new(Token::make_word(name, None), toks[i].span));
        out.push(toks[open].clone());
        out.extend(trim_list_forms(&toks[args_from..close]));
        out.push(toks[close].clone());
        i = close + 1;
    }
    out
}

/// Name of the function that stands for `ROWS FROM (f(..), g(..))` in a FROM list. sqlparser 0.63 has
/// no `ROWS FROM`, so [`mark_rows_from`] rewrites `ROWS FROM` into a call of this (NUL-bearing, hence
/// impossible in a PostgreSQL identifier) table function whose arguments are the member functions;
/// the binder (`from::bind_rows_from`) expands it into one function scan with several functions.
pub const ROWS_FROM_MARKER: &str = "\0pgarbor rows from";

/// Name of the wrapper call that carries a `ROWS FROM` member's column definition list
/// (`f(..) AS (a int, b text)`): argument 0 is the member call, every further argument is
/// `column => CAST(NULL AS type)`.
pub const ROWS_FROM_COLDEF_MARKER: &str = "\0pgarbor rows from coldef";

/// Name of the identifier that stands for the `*` of `(expr).*` (PG `indirection_el: '.' '*'` after a
/// parenthesized expression). sqlparser 0.63's PostgreSQL dialect rejects the form, so [`mark_paren_star`]
/// rewrites `).*` into `)."<marker>"`; the select-list binder turns that field access back into a star
/// expansion of the expression.
pub const STAR_MARKER: &str = "\0pgarbor star";

/// Column name `COPY ... FORCE_QUOTE *` is rewritten to (`FORCE_QUOTE ("<marker>")`); the COPY binder turns it
/// back into `force_quote_all`.
pub const FORCE_QUOTE_ALL_MARKER: &str = "\0pgarbor force_quote_all";

/// A quoted marker word token carrying `span`.
fn marker_word(name: &str, span: Span) -> TokenWithSpan {
    let word = Word { value: name.to_string(), quote_style: Some('"'), keyword: Keyword::NoKeyword };
    TokenWithSpan::new(Token::Word(word), span)
}

/// Rewrite `).*` (a star after a parenthesized expression, `(c).*`, `(f(x)).*`) into `)."<STAR_MARKER>"`.
fn mark_paren_star(toks: &[TokenWithSpan]) -> Vec<TokenWithSpan> {
    let mut out = Vec::with_capacity(toks.len());
    let mut i = 0;
    while i < toks.len() {
        out.push(toks[i].clone());
        if matches!(toks[i].token, Token::RParen) {
            let period = next_significant(toks, i + 1).filter(|&p| matches!(toks[p].token, Token::Period));
            let star = period
                .and_then(|p| next_significant(toks, p + 1))
                .filter(|&m| matches!(toks[m].token, Token::Mul));
            if let (Some(p), Some(m)) = (period, star) {
                out.extend(toks[i + 1..=p].iter().cloned());
                out.push(marker_word(STAR_MARKER, toks[m].span));
                i = m + 1;
                continue;
            }
        }
        i += 1;
    }
    out
}

/// `ORDER BY c.*` (an ORDER BY item that is a table alias with `.*`) is the whole-row reference `c` in PG
/// (`transformColumnRef` outside a target list), which is also what `ORDER BY c` means. sqlparser rejects
/// the `*`, so the `.*` of such an item is dropped. Only a single-word qualifier directly at an item start of
/// an ORDER BY list (same paren depth as the `ORDER BY`) is rewritten.
fn strip_order_by_whole_row_star(toks: &[TokenWithSpan]) -> Vec<TokenWithSpan> {
    let mut out = Vec::with_capacity(toks.len());
    // Paren depth of every ORDER BY list that is still open.
    let mut regions: Vec<i32> = Vec::new();
    let mut depth: i32 = 0;
    let mut i = 0;
    while i < toks.len() {
        let t = &toks[i];
        match &t.token {
            Token::LParen => depth += 1,
            Token::RParen => {
                depth -= 1;
                regions.retain(|&d| d <= depth);
            }
            Token::SemiColon => regions.clear(),
            _ => {}
        }
        let word = bare_word(&t.token);
        if let Some(w) = word.as_deref() {
            if w == "ORDER" {
                if let Some(by) = next_significant(toks, i + 1).filter(|&j| bare_word(&toks[j].token).as_deref() == Some("BY")) {
                    out.extend(toks[i..=by].iter().cloned());
                    regions.push(depth);
                    i = by + 1;
                    continue;
                }
            } else if matches!(w, "LIMIT" | "OFFSET" | "FETCH" | "FOR" | "UNION" | "INTERSECT" | "EXCEPT") {
                regions.retain(|&d| d != depth);
            }
        }
        let item_start = regions.last() == Some(&depth)
            && matches!(&t.token, Token::Word(_))
            && out
                .iter()
                .rev()
                .find(|x| !matches!(x.token, Token::Whitespace(_)))
                .map(|x| matches!(x.token, Token::Comma) || bare_word(&x.token).as_deref() == Some("BY"))
                .unwrap_or(false);
        if item_start {
            let period = next_significant(toks, i + 1).filter(|&j| matches!(toks[j].token, Token::Period));
            let star = period.and_then(|j| next_significant(toks, j + 1)).filter(|&j| matches!(toks[j].token, Token::Mul));
            if let Some(m) = star {
                out.push(t.clone());
                i = m + 1;
                continue;
            }
        }
        out.push(t.clone());
        i += 1;
    }
    out
}

/// `EXTERNAL SECURITY DEFINER|INVOKER` (gram.y `opt_external`, SQL-standard noise word in function
/// attributes) means `SECURITY DEFINER|INVOKER`; sqlparser only knows the latter, so `EXTERNAL` is dropped
/// when it directly precedes `SECURITY`.
fn drop_external_security(toks: &[TokenWithSpan]) -> Vec<TokenWithSpan> {
    let mut out = Vec::with_capacity(toks.len());
    for (i, t) in toks.iter().enumerate() {
        if bare_word(&t.token).as_deref() == Some("EXTERNAL")
            && next_significant(toks, i + 1).is_some_and(|j| bare_word(&toks[j].token).as_deref() == Some("SECURITY"))
        {
            continue;
        }
        out.push(t.clone());
    }
    out
}

/// Rewrite `FORCE_QUOTE *` (COPY option, PG 12+) into `FORCE_QUOTE ("<FORCE_QUOTE_ALL_MARKER>")`.
fn mark_force_quote_star(toks: &[TokenWithSpan]) -> Vec<TokenWithSpan> {
    let mut out = Vec::with_capacity(toks.len() + 2);
    let mut i = 0;
    while i < toks.len() {
        out.push(toks[i].clone());
        if bare_word(&toks[i].token).as_deref() == Some("FORCE_QUOTE") {
            if let Some(m) = next_significant(toks, i + 1).filter(|&m| matches!(toks[m].token, Token::Mul)) {
                out.extend(toks[i + 1..m].iter().cloned());
                out.push(TokenWithSpan::new(Token::LParen, toks[m].span));
                out.push(marker_word(FORCE_QUOTE_ALL_MARKER, toks[m].span));
                out.push(TokenWithSpan::new(Token::RParen, toks[m].span));
                i = m + 1;
                continue;
            }
        }
        i += 1;
    }
    out
}

/// If `toks[i..]` starts a `ROWS FROM ( func(..) ..` table function, the indexes of its `(` and its
/// matching `)`. A leading `(SELECT`/`(VALUES`/`(t1 JOIN ..` is a column named `rows` followed by FROM.
fn rows_from_at(toks: &[TokenWithSpan], i: usize) -> Option<(usize, usize)> {
    if bare_word(&toks[i].token).as_deref() != Some("ROWS") {
        return None;
    }
    let from = next_significant(toks, i + 1).filter(|&j| bare_word(&toks[j].token).as_deref() == Some("FROM"))?;
    let open = next_significant(toks, from + 1).filter(|&j| matches!(toks[j].token, Token::LParen))?;
    let mut k = next_significant(toks, open + 1)?;
    if !matches!(toks[k].token, Token::Word(_)) {
        return None;
    }
    // `name (` or `schema . name (`: a function call.
    loop {
        k = next_significant(toks, k + 1)?;
        match toks[k].token {
            Token::LParen => break,
            Token::Period => {
                let w = next_significant(toks, k + 1)?;
                if !matches!(toks[w].token, Token::Word(_)) {
                    return None;
                }
                k = w;
            }
            _ => return None,
        }
    }
    let mut depth = 0usize;
    for (j, t) in toks.iter().enumerate().skip(open) {
        match t.token {
            Token::LParen | Token::LBracket => depth += 1,
            Token::RParen | Token::RBracket => {
                depth = depth.checked_sub(1)?;
                if depth == 0 {
                    return Some((open, j));
                }
            }
            _ => {}
        }
    }
    None
}

/// Rewrite `ROWS FROM (f(..) [AS (coldefs)], ..)` into `"<ROWS_FROM_MARKER>"(f(..), ..)`; a member with a
/// column definition list becomes `"<ROWS_FROM_COLDEF_MARKER>"(f(..), col => CAST(NULL AS type), ..)`.
fn mark_rows_from(toks: &[TokenWithSpan]) -> Vec<TokenWithSpan> {
    let mut out = Vec::with_capacity(toks.len() + 8);
    let mut i = 0;
    while i < toks.len() {
        let Some((open, close)) = rows_from_at(toks, i) else {
            out.push(toks[i].clone());
            i += 1;
            continue;
        };
        out.push(marker_word(ROWS_FROM_MARKER, toks[i].span));
        out.push(toks[open].clone());
        let mut j = open + 1;
        while j <= close {
            // One member: up to the next top-level comma or the closing parenthesis.
            let mut depth = 0usize;
            let mut k = j;
            let mut as_at: Option<usize> = None;
            while k < close {
                match toks[k].token {
                    Token::LParen | Token::LBracket => depth += 1,
                    Token::RParen | Token::RBracket => depth = depth.saturating_sub(1),
                    Token::Comma if depth == 0 => break,
                    _ => {}
                }
                if depth == 0 && as_at.is_none() && bare_word(&toks[k].token).as_deref() == Some("AS") {
                    as_at = Some(k);
                }
                k += 1;
            }
            match as_at.and_then(|a| rows_from_coldef(toks, j, a, k)) {
                Some(rewritten) => out.extend(rewritten),
                None => out.extend(toks[j..k].iter().cloned()),
            }
            if k < close {
                out.push(toks[k].clone());
            }
            j = k + 1;
        }
        out.push(toks[close].clone());
        i = close + 1;
    }
    out
}

/// The wrapper call for the `ROWS FROM` member `toks[start..end]` whose `AS` is at `as_at`; `None` when the
/// tokens after `AS` are not a parenthesized `name type, ..` list (the member is then left as written and
/// sqlparser reports the syntax error).
fn rows_from_coldef(toks: &[TokenWithSpan], start: usize, as_at: usize, end: usize) -> Option<Vec<TokenWithSpan>> {
    let open = next_significant(toks, as_at + 1).filter(|&p| p < end && matches!(toks[p].token, Token::LParen))?;
    let mut close = None;
    let mut depth = 0usize;
    for (j, t) in toks.iter().enumerate().take(end).skip(open) {
        match t.token {
            Token::LParen | Token::LBracket => depth += 1,
            Token::RParen | Token::RBracket => {
                depth = depth.checked_sub(1)?;
                if depth == 0 {
                    close = Some(j);
                    break;
                }
            }
            _ => {}
        }
    }
    let close = close?;
    let span = toks[as_at].span;
    let mut out = vec![marker_word(ROWS_FROM_COLDEF_MARKER, span), synth(Token::LParen, span)];
    out.extend(toks[start..as_at].iter().cloned());
    let mut j = open + 1;
    while j < close {
        let mut depth = 0usize;
        let mut k = j;
        while k < close {
            match toks[k].token {
                Token::LParen | Token::LBracket => depth += 1,
                Token::RParen | Token::RBracket => depth = depth.saturating_sub(1),
                Token::Comma if depth == 0 => break,
                _ => {}
            }
            k += 1;
        }
        let name = next_significant(toks, j).filter(|&n| n < k && matches!(toks[n].token, Token::Word(_)))?;
        let ty = next_significant(toks, name + 1).filter(|&t| t < k)?;
        out.push(synth(Token::Comma, toks[name].span));
        out.push(toks[name].clone());
        out.push(synth(Token::RArrow, toks[name].span));
        out.push(synth(Token::make_keyword("CAST"), toks[name].span));
        out.push(synth(Token::LParen, toks[name].span));
        out.push(synth(Token::make_keyword("NULL"), toks[name].span));
        out.push(synth(Token::make_keyword("AS"), toks[name].span));
        out.extend(toks[ty..k].iter().cloned());
        out.push(synth(Token::RParen, toks[name].span));
        j = k + 1;
    }
    out.push(synth(Token::RParen, toks[close].span));
    Some(out)
}

/// A synthetic token carrying `span`.
fn synth(token: Token, span: Span) -> TokenWithSpan {
    TokenWithSpan::new(token, span)
}

/// `CREATE TRIGGER .. EXECUTE FUNCTION|PROCEDURE f('a', 1)` (gram.y `TriggerFuncArgs`): sqlparser reads the
/// argument list as parameter types and rejects literals. CREATE TRIGGER is rejected as unsupported later
/// (`classify`), so the arguments are dropped and the statement reaches that 0A000.
fn trigger_exec_args(toks: Vec<TokenWithSpan>) -> Vec<TokenWithSpan> {
    let head: Vec<String> = toks.iter().filter_map(|t| bare_word(&t.token)).take(4).collect();
    let is_trigger = head.first().map(String::as_str) == Some("CREATE") && head.iter().skip(1).any(|w| w == "TRIGGER");
    if !is_trigger {
        return toks;
    }
    let mut out: Vec<TokenWithSpan> = Vec::with_capacity(toks.len());
    let mut i = 0;
    while i < toks.len() {
        out.push(toks[i].clone());
        if bare_word(&toks[i].token).as_deref() == Some("EXECUTE") {
            let kind = next_significant(&toks, i + 1).filter(|&k| matches!(bare_word(&toks[k].token).as_deref(), Some("FUNCTION" | "PROCEDURE")));
            if let Some(kind) = kind {
                // Function name: words separated by periods, then `(`.
                let mut k = kind + 1;
                let mut open = None;
                while let Some(p) = next_significant(&toks, k) {
                    match &toks[p].token {
                        Token::Word(_) | Token::Period => k = p + 1,
                        Token::LParen => {
                            open = Some(p);
                            break;
                        }
                        _ => break,
                    }
                }
                if let Some(open) = open {
                    let close = (open + 1..toks.len()).find(|&j| matches!(toks[j].token, Token::RParen));
                    if let Some(close) = close {
                        out.extend(toks[i + 1..=open].iter().cloned());
                        out.push(toks[close].clone());
                        i = close + 1;
                        continue;
                    }
                }
            }
        }
        i += 1;
    }
    out
}

/// `a_expr AT LOCAL` (gram.y, PG 18): the session time zone. sqlparser has no such form, so it is rewritten to
/// `AT TIME ZONE current_setting('TimeZone')`, which converts to the same zone.
fn at_local(toks: Vec<TokenWithSpan>) -> Vec<TokenWithSpan> {
    let mut out: Vec<TokenWithSpan> = Vec::with_capacity(toks.len());
    let mut i = 0;
    while i < toks.len() {
        if bare_word(&toks[i].token).as_deref() == Some("AT") {
            if let Some(n) = next_significant(&toks, i + 1).filter(|&n| bare_word(&toks[n].token).as_deref() == Some("LOCAL")) {
                let span = toks[n].span;
                out.push(toks[i].clone());
                out.push(TokenWithSpan::new(Token::make_word("TIME", None), span));
                out.push(TokenWithSpan::new(Token::make_word("ZONE", None), span));
                out.push(TokenWithSpan::new(Token::make_word("current_setting", None), span));
                out.push(TokenWithSpan::new(Token::LParen, span));
                out.push(TokenWithSpan::new(Token::SingleQuotedString("TimeZone".to_string()), span));
                out.push(TokenWithSpan::new(Token::RParen, span));
                i = n + 1;
                continue;
            }
        }
        out.push(toks[i].clone());
        i += 1;
    }
    out
}

/// `TABLESAMPLE method (args) REPEATABLE ( a_expr )` (gram.y `opt_repeatable_clause`): sqlparser only reads a
/// number literal as the seed. A seed expression is replaced by the literal `0`; TABLESAMPLE is always rejected
/// with 0A000 by the binder, so the seed value is never evaluated.
fn tablesample_seed_exprs(toks: Vec<TokenWithSpan>) -> Vec<TokenWithSpan> {
    let mut out: Vec<TokenWithSpan> = Vec::with_capacity(toks.len());
    let mut i = 0;
    while i < toks.len() {
        if bare_word(&toks[i].token).as_deref() == Some("REPEATABLE") && matches!(last_significant(&out), Some(Token::RParen)) {
            if let Some(open) = next_significant(&toks, i + 1).filter(|&p| matches!(toks[p].token, Token::LParen)) {
                let mut depth = 0usize;
                let mut close = None;
                for (k, t) in toks.iter().enumerate().skip(open + 1) {
                    match &t.token {
                        Token::LParen | Token::LBracket => depth += 1,
                        Token::RParen | Token::RBracket => {
                            if depth == 0 {
                                close = Some(k);
                                break;
                            }
                            depth -= 1;
                        }
                        _ => {}
                    }
                }
                if let Some(close) = close {
                    let inner: Vec<&TokenWithSpan> =
                        toks[open + 1..close].iter().filter(|t| !matches!(t.token, Token::Whitespace(_))).collect();
                    let simple = matches!(inner.as_slice(), [t] if matches!(t.token, Token::Number(..)));
                    if !simple && !inner.is_empty() {
                        out.extend(toks[i..=open].iter().cloned());
                        out.push(TokenWithSpan::new(Token::Number("0".to_string(), false), inner[0].span));
                        out.push(toks[close].clone());
                        i = close + 1;
                        continue;
                    }
                }
            }
        }
        out.push(toks[i].clone());
        i += 1;
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

/// `substring(string SIMILAR pattern ESCAPE escape)` (gram.y `substr_list`) is the three-argument call
/// `substring(string, pattern, escape)`: inside a `substring(..)` call, a depth-1 `SIMILAR` (not
/// `SIMILAR TO`) and the `ESCAPE` after it become commas, which sqlparser 0.63 can read.
fn substring_similar_args(toks: &[TokenWithSpan]) -> Vec<TokenWithSpan> {
    let mut out: Vec<TokenWithSpan> = toks.to_vec();
    let mut i = 0usize;
    while i < toks.len() {
        if bare_word(&toks[i].token).as_deref() != Some("SUBSTRING") {
            i += 1;
            continue;
        }
        let Some(open) = next_significant(toks, i + 1) else { break };
        if !matches!(toks[open].token, Token::LParen) {
            i += 1;
            continue;
        }
        let mut depth = 0usize;
        let mut after_similar = false;
        let mut k = open;
        while k < toks.len() {
            match &toks[k].token {
                Token::LParen | Token::LBracket => depth += 1,
                Token::RParen | Token::RBracket => {
                    depth = depth.saturating_sub(1);
                    if depth == 0 {
                        break;
                    }
                }
                Token::Word(_) if depth == 1 => {
                    let w = bare_word(&toks[k].token);
                    if w.as_deref() == Some("SIMILAR") {
                        let next_is_to = next_significant(toks, k + 1)
                            .is_some_and(|n| bare_word(&toks[n].token).as_deref() == Some("TO"));
                        if !next_is_to {
                            out[k].token = Token::Comma;
                            after_similar = true;
                        }
                    } else if after_similar && w.as_deref() == Some("ESCAPE") {
                        out[k].token = Token::Comma;
                        after_similar = false;
                    }
                }
                _ => {}
            }
            k += 1;
        }
        i = open + 1;
    }
    out
}

/// Prefix of the function name that stands for an SQL/XML construct with special syntax
/// (`xmlelement(name n, ..)`, `xmlpi(name n)`, ...). sqlparser 0.63 cannot read those argument lists, and
/// PGArbor has no XML values, so [`mark_xml_constructs`] replaces the whole call with a call of this
/// prefix plus the lowercase construct name, which the binder reports as 0A000 (SQL-10).
pub const XML_UNSUPPORTED_MARKER: &str = "\0pgarbor xml construct ";

/// Replace `xmlelement(..)`, `xmlpi(..)`, `xmlroot(..)`, `xmlparse(..)`, `xmlserialize(..)`,
/// `xmlexists(..)` and `xmlforest(..)` calls (a bare, unqualified name directly followed by `(`) by
/// `<XML_UNSUPPORTED_MARKER><name>()`.
fn mark_xml_constructs(toks: &[TokenWithSpan]) -> Vec<TokenWithSpan> {
    const NAMES: [&str; 8] =
        ["XMLELEMENT", "XMLPI", "XMLROOT", "XMLPARSE", "XMLSERIALIZE", "XMLEXISTS", "XMLFOREST", "XMLCONCAT"];
    let mut out: Vec<TokenWithSpan> = Vec::with_capacity(toks.len());
    let mut i = 0usize;
    while i < toks.len() {
        let name = bare_word(&toks[i].token).filter(|w| NAMES.contains(&w.as_str()));
        let open = name.as_ref().and_then(|_| next_significant(toks, i + 1));
        let after_dot = out.iter().rev().find(|t| !matches!(t.token, Token::Whitespace(_))).is_some_and(|t| matches!(t.token, Token::Period));
        match (name, open) {
            (Some(n), Some(open)) if matches!(toks[open].token, Token::LParen) && !after_dot => {
                let mut depth = 0usize;
                let mut close = None;
                for k in open..toks.len() {
                    match toks[k].token {
                        Token::LParen => depth += 1,
                        Token::RParen => {
                            depth = depth.saturating_sub(1);
                            if depth == 0 {
                                close = Some(k);
                                break;
                            }
                        }
                        _ => {}
                    }
                }
                let Some(close) = close else {
                    out.push(toks[i].clone());
                    i += 1;
                    continue;
                };
                let span = Span::new(toks[i].span.start, toks[close].span.end);
                let word = Word { value: format!("{XML_UNSUPPORTED_MARKER}{}", n.to_ascii_lowercase()), quote_style: Some('"'), keyword: Keyword::NoKeyword };
                out.push(TokenWithSpan::new(Token::Word(word), span));
                out.push(TokenWithSpan::new(Token::LParen, span));
                out.push(TokenWithSpan::new(Token::RParen, span));
                i = close + 1;
            }
            _ => {
                out.push(toks[i].clone());
                i += 1;
            }
        }
    }
    out
}

/// Name prefix of the marker identifier that stands for the `WHERE index_predicate` of an `ON CONFLICT
/// (cols) WHERE pred` inference clause. sqlparser 0.63 has no field for it, so [`mark_conflict_where`]
/// removes the predicate and appends this prefix plus the predicate's source text as one more element
/// of the inference list; `binder::on_conflict` parses and binds it.
pub const CONFLICT_WHERE_MARKER: &str = "\0pgarbor conflict where\0";

/// Move the `WHERE index_predicate` that follows an `ON CONFLICT (..)` inference list into the list as a
/// [`CONFLICT_WHERE_MARKER`] element. Other statements, and clauses without a predicate, are unchanged.
fn mark_conflict_where(toks: &[TokenWithSpan], src: &SourceMap) -> Result<Vec<TokenWithSpan>> {
    let is_insert = next_significant(toks, 0).is_some_and(|h| bare_word(&toks[h].token).as_deref() == Some("INSERT"));
    if !is_insert {
        return Ok(toks.to_vec());
    }
    // (close paren index, last predicate token index, marker token)
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
        if !matches!(toks[open].token, Token::LParen) {
            continue;
        }
        let mut depth = 0usize;
        let mut close = None;
        let mut i = open;
        while i < toks.len() {
            match toks[i].token {
                Token::LParen | Token::LBracket => depth += 1,
                Token::RParen | Token::RBracket => {
                    depth = depth.saturating_sub(1);
                    if depth == 0 {
                        close = Some(i);
                        break;
                    }
                }
                _ => {}
            }
            i += 1;
        }
        let Some(close) = close else { continue };
        let Some(wh) = next_significant(toks, close + 1) else { continue };
        if bare_word(&toks[wh].token).as_deref() != Some("WHERE") {
            continue;
        }
        let Some(first) = next_significant(toks, wh + 1) else { continue };
        // The predicate ends before the depth-0 `DO` that starts the action.
        let mut depth = 0usize;
        let mut last = None;
        let mut end = None;
        let mut k = first;
        while k < toks.len() {
            match &toks[k].token {
                Token::Whitespace(_) => {}
                Token::LParen | Token::LBracket => depth += 1,
                Token::RParen | Token::RBracket => depth = depth.saturating_sub(1),
                Token::Word(_) if depth == 0 && bare_word(&toks[k].token).as_deref() == Some("DO") => {
                    end = Some(k);
                    break;
                }
                _ => {}
            }
            if !matches!(toks[k].token, Token::Whitespace(_)) {
                last = Some(k);
            }
            k += 1;
        }
        let (Some(last), Some(_)) = (last, end) else { continue };
        if last < first {
            continue;
        }
        let (start, _) = token_bytes(src, &toks[first].span)?;
        let (_, stop) = token_bytes(src, &toks[last].span)?;
        let marker = format!("{CONFLICT_WHERE_MARKER}{}", src.slice(start, stop));
        let span = Span::new(toks[first].span.start, toks[last].span.end);
        found.push((close, last, TokenWithSpan::new(Token::make_word(&marker, Some('"')), span)));
    }
    if found.is_empty() {
        return Ok(toks.to_vec());
    }
    found.sort_unstable_by_key(|f| f.0);
    let mut out: Vec<TokenWithSpan> = Vec::with_capacity(toks.len());
    let mut at = 0usize;
    for (close, last, marker) in found {
        out.extend_from_slice(&toks[at..close]);
        let span = marker.span;
        out.push(TokenWithSpan::new(Token::Comma, span));
        out.push(marker);
        out.push(TokenWithSpan::new(Token::RParen, span));
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

/// gram.y `insert_rest`: `INSERT INTO t [AS a] [(cols)] OVERRIDING {SYSTEM|USER} VALUE <select>`. sqlparser 0.63
/// has no such clause, so the three tokens are removed here and returned as an [`Overriding`](crate::ir::stmt::Overriding)
/// for the binder. `DEFAULT VALUES` takes no OVERRIDING clause (syntax error at `DEFAULT`). A chunk that is not an
/// INSERT, or has no clause directly after the target, comes back unchanged.
fn extract_insert_overriding(
    toks: Vec<TokenWithSpan>,
    src: &SourceMap,
) -> Result<(Vec<TokenWithSpan>, Option<crate::ir::stmt::Overriding>)> {
    let Some(head) = next_significant(&toks, 0) else { return Ok((toks, None)) };
    if bare_word(&toks[head].token).as_deref() != Some("INSERT") {
        return Ok((toks, None));
    }
    let Some(into) = next_significant(&toks, head + 1) else { return Ok((toks, None)) };
    if bare_word(&toks[into].token).as_deref() != Some("INTO") {
        return Ok((toks, None));
    }
    // Target name: word (. word)*
    let mut k = next_significant(&toks, into + 1);
    while let Some(i) = k {
        if !matches!(toks[i].token, Token::Word(_)) {
            return Ok((toks, None));
        }
        k = next_significant(&toks, i + 1);
        match k {
            Some(d) if matches!(toks[d].token, Token::Period) => k = next_significant(&toks, d + 1),
            _ => break,
        }
    }
    // Optional `AS alias`.
    if let Some(i) = k {
        if bare_word(&toks[i].token).as_deref() == Some("AS") {
            k = next_significant(&toks, i + 1).and_then(|a| next_significant(&toks, a + 1));
        }
    }
    // Optional column list.
    if let Some(i) = k {
        if matches!(toks[i].token, Token::LParen) {
            let first = next_significant(&toks, i + 1);
            let query_start = first.is_some_and(|f| {
                matches!(bare_word(&toks[f].token).as_deref(), Some("SELECT" | "WITH" | "VALUES" | "TABLE"))
            });
            if query_start {
                return Ok((toks, None));
            }
            let mut depth = 0usize;
            let mut j = i;
            let mut close = None;
            while j < toks.len() {
                match toks[j].token {
                    Token::LParen => depth += 1,
                    Token::RParen => {
                        depth = depth.saturating_sub(1);
                        if depth == 0 {
                            close = Some(j);
                            break;
                        }
                    }
                    _ => {}
                }
                j += 1;
            }
            k = close.and_then(|c| next_significant(&toks, c + 1));
        }
    }
    let Some(ov) = k else { return Ok((toks, None)) };
    if bare_word(&toks[ov].token).as_deref() != Some("OVERRIDING") {
        return Ok((toks, None));
    }
    let kind_at = next_significant(&toks, ov + 1);
    let kind = match kind_at.and_then(|i| bare_word(&toks[i].token)).as_deref() {
        Some("SYSTEM") => crate::ir::stmt::Overriding::System,
        Some("USER") => crate::ir::stmt::Overriding::User,
        _ => return Ok((toks, None)),
    };
    let Some(value_at) = kind_at.and_then(|i| next_significant(&toks, i + 1)) else { return Ok((toks, None)) };
    if bare_word(&toks[value_at].token).as_deref() != Some("VALUE") {
        return Ok((toks, None));
    }
    if let Some(after) = next_significant(&toks, value_at + 1) {
        if bare_word(&toks[after].token).as_deref() == Some("DEFAULT") {
            return Err(syntax_error::token_problem(&toks[after], src));
        }
    }
    let out: Vec<TokenWithSpan> =
        toks[..ov].iter().chain(&toks[value_at + 1..]).cloned().collect();
    Ok((out, Some(kind)))
}

/// A leading `TABLE name` accepts no FROM-clause continuation (gram.y `TABLE relation_expr`): `WHERE`, `GROUP BY`,
/// `HAVING`, `WINDOW`, an alias, a join or a sample after the name is a syntax error at that token. The rewrite to
/// `SELECT * FROM name` would otherwise accept them.
fn reject_table_command_clauses(toks: &[TokenWithSpan], src: &SourceMap) -> Result<()> {
    let Some(j) = next_significant(toks, 0) else { return Ok(()) };
    if bare_word(&toks[j].token).as_deref() != Some("TABLE") {
        return Ok(());
    }
    let mut k = next_significant(toks, j + 1);
    if let Some(i) = k {
        if bare_word(&toks[i].token).as_deref() == Some("ONLY") {
            k = next_significant(toks, i + 1);
        }
    }
    // Qualified name: word (. word)*
    while let Some(i) = k {
        if !matches!(toks[i].token, Token::Word(_)) {
            return Ok(());
        }
        k = next_significant(toks, i + 1);
        match k {
            Some(d) if matches!(toks[d].token, Token::Period) => k = next_significant(toks, d + 1),
            _ => break,
        }
    }
    if let Some(i) = k {
        if matches!(toks[i].token, Token::Mul) {
            k = next_significant(toks, i + 1);
        }
    }
    if let Some(i) = k {
        let bad = matches!(
            bare_word(&toks[i].token).as_deref(),
            Some(
                "WHERE" | "GROUP" | "HAVING" | "WINDOW" | "AS" | "JOIN" | "CROSS" | "NATURAL" | "INNER" | "LEFT" | "RIGHT"
                    | "FULL" | "TABLESAMPLE" | "USING" | "ON"
            )
        );
        if bad || matches!(toks[i].token, Token::Comma) {
            return Err(syntax_error::token_problem(&toks[i], src));
        }
    }
    Ok(())
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

/// Column-level `UNIQUE NULLS [NOT] DISTINCT` (PG gram.y `ColConstraintElem`). sqlparser 0.63 accepts the
/// clause only on table constraints, so the three tokens after a column-level `UNIQUE` are removed and
/// one flag per column-level `UNIQUE` is returned (`Some(true)` = `NOT DISTINCT`, `Some(false)` =
/// `DISTINCT`, `None` = not written); [`restore_column_unique_nulls`] puts them back on the AST.
/// A `UNIQUE` followed by `(`, `INDEX`, `CONCURRENTLY` or `USING` is not column-level.
fn mark_column_unique_nulls(toks: &[TokenWithSpan]) -> (Vec<TokenWithSpan>, Vec<Option<bool>>) {
    let sig: Vec<usize> = (0..toks.len()).filter(|&i| !matches!(toks[i].token, Token::Whitespace(_) | Token::EOF)).collect();
    let word = |k: usize| sig.get(k).and_then(|&i| bare_word(&toks[i].token));
    let mut flags = Vec::new();
    let mut drop = vec![false; toks.len()];
    for k in 0..sig.len() {
        if word(k).as_deref() != Some("UNIQUE") {
            continue;
        }
        let mut j = k + 1;
        let mut written = None;
        if word(j).as_deref() == Some("NULLS") {
            if word(j + 1).as_deref() == Some("NOT") && word(j + 2).as_deref() == Some("DISTINCT") {
                written = Some(true);
                j += 3;
            } else if word(j + 1).as_deref() == Some("DISTINCT") {
                written = Some(false);
                j += 2;
            } else {
                continue;
            }
        }
        let table_level = sig.get(j).is_some_and(|&i| matches!(toks[i].token, Token::LParen))
            || matches!(word(j).as_deref(), Some("INDEX" | "CONCURRENTLY" | "USING"));
        if table_level {
            continue;
        }
        flags.push(written);
        if written.is_some() {
            for &i in &sig[k + 1..j] {
                drop[i] = true;
            }
        }
    }
    if flags.iter().all(Option::is_none) {
        return (toks.to_vec(), Vec::new());
    }
    let kept = toks.iter().enumerate().filter(|(i, _)| !drop[*i]).map(|(_, t)| t.clone()).collect();
    (kept, flags)
}

/// Re-attach the flags of [`mark_column_unique_nulls`] to the column-level `UNIQUE` options, in order.
fn restore_column_unique_nulls(stmt: &mut Statement, flags: &[Option<bool>]) -> Result<()> {
    if flags.is_empty() {
        return Ok(());
    }
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
    let mut uniques = Vec::new();
    for def in defs {
        for opt in def.options.iter_mut() {
            if let ColumnOption::Unique(u) = &mut opt.option {
                uniques.push(u);
            }
        }
    }
    if uniques.len() != flags.len() {
        return Err(PgError::internal("UNIQUE column constraints do not match the statement"));
    }
    for (u, flag) in uniques.into_iter().zip(flags.iter().copied()) {
        match flag {
            Some(true) => u.nulls_distinct = sqlparser::ast::NullsDistinctOption::NotDistinct,
            Some(false) => u.nulls_distinct = sqlparser::ast::NullsDistinctOption::Distinct,
            None => {}
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
/// Markers of `ALTER COLUMN c DROP IDENTITY [IF EXISTS]`, rewritten like the expression subcommands.
pub const DROP_IDENTITY_MARKER: &str = "\0pgarbor drop identity";
pub const DROP_IDENTITY_IF_EXISTS_MARKER: &str = "\0pgarbor drop identity if exists";
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
    if word(verb, "DROP") && (word(object, "EXPRESSION") || word(object, "IDENTITY")) {
        let identity = word(object, "IDENTITY");
        let mut last = object;
        let mut name = if identity { DROP_IDENTITY_MARKER } else { DROP_EXPRESSION_MARKER };
        if let Some(if_kw) = next_significant(toks, object + 1).filter(|&k| word(k, "IF")) {
            if let Some(exists) = next_significant(toks, if_kw + 1).filter(|&k| word(k, "EXISTS")) {
                last = exists;
                name = if identity { DROP_IDENTITY_IF_EXISTS_MARKER } else { DROP_EXPRESSION_IF_EXISTS_MARKER };
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

/// `GRANT` or `REVOKE` of a privilege on a type or domain (`ON TYPE name`, `ON DOMAIN name`). sqlparser has
/// no such object kinds, and PGArbor keeps no `pg_type.typacl` privileges yet (lease SQL-39). A table named
/// `type` or `domain` (`ON type TO role`, `ON domain.t`) does not match: no name follows the keyword there.
fn type_privilege_statement(toks: &[TokenWithSpan]) -> bool {
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
        && words
            .windows(3)
            .any(|w| w[0] == "ON" && matches!(w[1].as_str(), "TYPE" | "DOMAIN") && !w[2].is_empty() && !matches!(w[2].as_str(), "TO" | "FROM"))
}

/// A GRANT/REVOKE privilege that carries a column list although it is not a column privilege
/// (`GRANT DELETE (a) ON t ...`): PG `ExecuteGrantStmt` rejects it with 0LP01
/// `invalid privilege type DELETE for column`. Returns the privilege's PG spelling.
fn non_column_privilege_with_columns(toks: &[TokenWithSpan]) -> Option<&'static str> {
    let sig: Vec<usize> = (0..toks.len()).filter(|&i| !matches!(toks[i].token, Token::Whitespace(_))).collect();
    if !matches!(sig.first().and_then(|&i| bare_word(&toks[i].token)).as_deref(), Some("GRANT" | "REVOKE")) {
        return None;
    }
    let mut depth = 0usize;
    for (k, &i) in sig.iter().enumerate() {
        match &toks[i].token {
            Token::LParen => depth += 1,
            Token::RParen => depth = depth.saturating_sub(1),
            t if depth == 0 => match bare_word(t).as_deref() {
                Some("ON") => return None,
                Some(w @ ("DELETE" | "TRUNCATE" | "TRIGGER" | "USAGE" | "EXECUTE" | "CONNECT" | "TEMP" | "TEMPORARY" | "MAINTAIN")) => {
                    let opens = sig.get(k + 1).is_some_and(|&j| matches!(toks[j].token, Token::LParen));
                    if opens {
                        return Some(match w {
                            "DELETE" => "DELETE",
                            "TRUNCATE" => "TRUNCATE",
                            "TRIGGER" => "TRIGGER",
                            "USAGE" => "USAGE",
                            "EXECUTE" => "EXECUTE",
                            "CONNECT" => "CONNECT",
                            "MAINTAIN" => "MAINTAIN",
                            _ => "TEMPORARY",
                        });
                    }
                }
                _ => {}
            },
            _ => {}
        }
    }
    None
}

/// `GRANT` or `REVOKE` on a language (`ON LANGUAGE name`). Returns the first privilege of the list that
/// languages cannot hold (only `USAGE` and `ALL` exist for them), as PG `ExecGrant_Language` words it,
/// or `None` when the statement is not a language grant or all privileges are valid.
fn language_privilege_statement(toks: &[TokenWithSpan]) -> Option<std::result::Result<(), pgarbor_common::PgError>> {
    let words: Vec<String> = toks
        .iter()
        .filter(|t| !matches!(t.token, Token::Whitespace(_)))
        .map(|t| match &t.token {
            Token::Word(w) if w.quote_style.is_none() => w.value.to_ascii_uppercase(),
            Token::Word(_) => String::from("(quoted)"),
            Token::Comma => String::from(","),
            _ => String::new(),
        })
        .collect();
    if !matches!(words.first().map(String::as_str), Some("GRANT" | "REVOKE")) {
        return None;
    }
    let on_at = words.iter().position(|w| w == "ON")?;
    if words.get(on_at + 1).map(String::as_str) != Some("LANGUAGE") || words.get(on_at + 2).map_or(true, |w| w.is_empty()) {
        return None;
    }
    let mut start = 1;
    if words.get(1).map(String::as_str) == Some("GRANT") && words.get(2).map(String::as_str) == Some("OPTION") {
        start = 4; // REVOKE GRANT OPTION FOR
    }
    for w in &words[start.min(on_at)..on_at] {
        if w.is_empty() || matches!(w.as_str(), "," | "USAGE" | "ALL" | "PRIVILEGES" | "FOR") {
            continue;
        }
        let name = if w == "TEMP" { "TEMPORARY" } else { w.as_str() };
        return Some(Err(pgarbor_common::PgError::new(pgarbor_common::SqlState(*b"0LP01"), format!("invalid privilege type {name} for language"))));
    }
    Some(Ok(()))
}

/// Is `word` the tail of a PostgreSQL octal or binary integer literal after its `0`: `o`/`b` (either case),
/// then digits of that radix with single underscores between digits (scan.l `octinteger`, `bininteger`:
/// `0[oO](_?{octdigit})+`). sqlparser reads `0o273` as the number `0` followed by the word `o273`.
fn is_radix_tail(word: &str) -> bool {
    let mut chars = word.chars();
    let radix = match chars.next() {
        Some('b' | 'B') => 2,
        Some('o' | 'O') => 8,
        // sqlparser tokenizes only a lowercase `0x` prefix itself; `0XFFFF` arrives as `0` + `XFFFF`.
        Some('X') => 16,
        _ => return false,
    };
    let (mut seen_digit, mut prev_underscore) = (false, false);
    for c in chars {
        if c == '_' {
            if prev_underscore {
                return false;
            }
            prev_underscore = true;
        } else if c.to_digit(radix).is_some() {
            seen_digit = true;
            prev_underscore = false;
        } else {
            return false;
        }
    }
    seen_digit && !prev_underscore
}

/// PostgreSQL lexical forms that sqlparser tokenizes differently, normalized before parsing:
/// * `0b101` / `0o17` (binary and octal integers, scan.l) arrive as `0` + a word; they become one
///   `Number` token whose text keeps the prefix (the binder converts it, `literal::number_to_const`);
/// * adjacent string constants separated by whitespace that contains a newline (`'foo'\n'bar'`) are one
///   constant (scan.l `quotecontinue`); a block comment or no newline between them is not continuation;
/// * the postfix operators `ISNULL` and `NOTNULL` after an operand become `IS NULL` / `IS NOT NULL`.
fn pg_lexical_tokens(toks: Vec<TokenWithSpan>) -> Vec<TokenWithSpan> {
    use sqlparser::tokenizer::Whitespace;
    let mut out: Vec<TokenWithSpan> = Vec::with_capacity(toks.len());
    let mut i = 0;
    while i < toks.len() {
        match &toks[i].token {
            Token::Number(n, _) if n.as_str() == "0" => {
                if let Some(next) = toks.get(i + 1) {
                    if let Token::Word(w) = &next.token {
                        if w.quote_style.is_none() && next.span.start == toks[i].span.end && is_radix_tail(&w.value) {
                            let span = Span::new(toks[i].span.start, next.span.end);
                            out.push(TokenWithSpan::new(Token::Number(format!("0{}", w.value), false), span));
                            i += 2;
                            continue;
                        }
                    }
                }
            }
            Token::SingleQuotedString(first) => {
                let mut value = first.clone();
                let mut last = i;
                let mut k = i + 1;
                loop {
                    let mut newline = false;
                    let mut ok = true;
                    let mut j = k;
                    while let Some(Token::Whitespace(ws)) = toks.get(j).map(|t| &t.token) {
                        match ws {
                            Whitespace::Space | Whitespace::Tab => {}
                            Whitespace::Newline | Whitespace::SingleLineComment { .. } => newline = true,
                            Whitespace::MultiLineComment(_) => ok = false,
                        }
                        j += 1;
                    }
                    match toks.get(j).map(|t| &t.token) {
                        Some(Token::SingleQuotedString(more)) if ok && newline => {
                            value.push_str(more);
                            last = j;
                            k = j + 1;
                        }
                        _ => break,
                    }
                }
                if last > i {
                    let span = Span::new(toks[i].span.start, toks[last].span.end);
                    out.push(TokenWithSpan::new(Token::SingleQuotedString(value), span));
                    i = last + 1;
                    continue;
                }
            }
            Token::Word(w) if w.quote_style.is_none() => {
                let negated = w.value.eq_ignore_ascii_case("notnull");
                if (negated || w.value.eq_ignore_ascii_case("isnull"))
                    && follows_operand(&out)
                    && !next_significant(&toks, i + 1).is_some_and(|k| matches!(toks[k].token, Token::LParen))
                {
                    let span = toks[i].span;
                    let mut words = vec!["IS"];
                    if negated {
                        words.push("NOT");
                    }
                    words.push("NULL");
                    for (n, word) in words.into_iter().enumerate() {
                        if n > 0 {
                            out.push(TokenWithSpan::new(Token::Whitespace(Whitespace::Space), span));
                        }
                        out.push(TokenWithSpan::new(Token::make_word(word, None), span));
                    }
                    i += 1;
                    continue;
                }
            }
            _ => {}
        }
        out.push(toks[i].clone());
        i += 1;
    }
    out
}

/// `JOIN ... USING (cols) AS alias` (PG 14 `join_using_alias`): the alias names the merged USING columns.
/// sqlparser's `JoinConstraint::Using` cannot carry it and the binder has no merged-column alias scope yet
/// (lease SQL-18), so it is 0A000 at the `AS` instead of a syntax error. Only a plain column-name list
/// matches, so `MERGE ... USING (SELECT ..) AS s` and `DELETE .. USING` are untouched.
fn reject_using_alias(toks: &[TokenWithSpan], src: &SourceMap) -> Result<()> {
    for i in 0..toks.len() {
        if bare_word(&toks[i].token).as_deref() != Some("USING") {
            continue;
        }
        let Some(open) = next_significant(toks, i + 1) else { continue };
        if !matches!(toks[open].token, Token::LParen) {
            continue;
        }
        let mut k = open + 1;
        let mut names = 0;
        let close = loop {
            let Some(w) = next_significant(toks, k) else { break None };
            if !matches!(toks[w].token, Token::Word(_)) {
                break None;
            }
            if names == 0 && matches!(bare_word(&toks[w].token).as_deref(), Some("SELECT" | "VALUES" | "WITH" | "TABLE")) {
                break None;
            }
            names += 1;
            let Some(sep) = next_significant(toks, w + 1) else { break None };
            match toks[sep].token {
                Token::Comma => k = sep + 1,
                Token::RParen => break Some(sep),
                _ => break None,
            }
        };
        let Some(close) = close else { continue };
        let Some(as_at) = next_significant(toks, close + 1) else { continue };
        if bare_word(&toks[as_at].token).as_deref() != Some("AS") {
            continue;
        }
        let Some(alias) = next_significant(toks, as_at + 1) else { continue };
        if !matches!(toks[alias].token, Token::Word(_)) {
            continue;
        }
        let err = PgError::unimplemented("SQL-18", "JOIN USING with a join alias");
        return Err(match src.char_pos(toks[as_at].span.start) {
            Some(pos) => err.with_position(pos),
            None => err,
        });
    }
    Ok(())
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

/// `RETURNING WITH (OLD [AS] a, NEW [AS] b)` (gram.y `returning_with_clause`) is PostgreSQL 18 grammar that
/// sqlparser 0.63 does not read; the binder has no OLD/NEW row versions yet (lease SQL-28), so it fails here
/// with that 0A000 at the `WITH` instead of a syntax error.
fn reject_returning_with_alias(toks: &[TokenWithSpan], src: &SourceMap) -> Result<()> {
    let word = |k: usize, w: &str| bare_word(&toks[k].token).as_deref() == Some(w);
    for i in 0..toks.len() {
        if !word(i, "RETURNING") {
            continue;
        }
        let Some(with) = next_significant(toks, i + 1) else { continue };
        if !word(with, "WITH") {
            continue;
        }
        let Some(open) = next_significant(toks, with + 1) else { continue };
        if !matches!(toks[open].token, Token::LParen) {
            continue;
        }
        let Some(first) = next_significant(toks, open + 1) else { continue };
        if word(first, "OLD") || word(first, "NEW") {
            let err = PgError::unimplemented("SQL-28", "RETURNING WITH (OLD AS ..., NEW AS ...)");
            return Err(match src.char_pos(toks[with].span.start) {
                Some(pos) => err.with_position(pos),
                None => err,
            });
        }
    }
    Ok(())
}

/// `expr IS [NOT] DOCUMENT` and `XMLPARSE ( DOCUMENT|CONTENT expr [PRESERVE|STRIP WHITESPACE] )` (gram.y
/// `a_expr IS DOCUMENT_P`, `XMLPARSE '(' document_or_content a_expr xml_whitespace_option ')'`). sqlparser has
/// neither form, and the xml type has no implementation yet (TYP-42), so both are 0A000 at the keyword instead of
/// a syntax error. `XMLPARSE` is only matched when a `(` follows, and `DOCUMENT` only after `IS [NOT]`.
fn reject_xml_syntax(toks: &[TokenWithSpan], src: &SourceMap) -> Result<()> {
    let word = |k: usize, w: &str| bare_word(&toks[k].token).as_deref() == Some(w);
    for i in 0..toks.len() {
        let at = if word(i, "XMLPARSE") {
            match next_significant(toks, i + 1) {
                Some(open) if matches!(toks[open].token, Token::LParen) => Some((i, "XMLPARSE")),
                _ => None,
            }
        } else if word(i, "IS") {
            let Some(mut k) = next_significant(toks, i + 1) else { continue };
            if word(k, "NOT") {
                match next_significant(toks, k + 1) {
                    Some(n) => k = n,
                    None => continue,
                }
            }
            if word(k, "DOCUMENT") { Some((k, "IS DOCUMENT")) } else { None }
        } else {
            None
        };
        if let Some((pos_tok, what)) = at {
            let err = PgError::unimplemented("TYP-42", what);
            return Err(match src.char_pos(toks[pos_tok].span.start) {
                Some(pos) => err.with_position(pos),
                None => err,
            });
        }
    }
    Ok(())
}

/// `WITH [RECURSIVE] q AS (...) SEARCH {BREADTH|DEPTH} FIRST BY cols SET col` and `... CYCLE cols SET col
/// [TO v DEFAULT d] [USING path]` (gram.y `opt_search_clause`, `opt_cycle_clause`). sqlparser has neither, and
/// the rewrite into the recursive query is not implemented, so they are 0A000 at the keyword instead of a
/// syntax error. Both only match directly after the `)` that closes the CTE body.
fn reject_cte_search_cycle(toks: &[TokenWithSpan], src: &SourceMap) -> Result<()> {
    let sig: Vec<usize> = (0..toks.len()).filter(|&i| !matches!(toks[i].token, Token::Whitespace(_) | Token::EOF)).collect();
    let word = |k: usize| sig.get(k).and_then(|&i| bare_word(&toks[i].token));
    for k in 1..sig.len() {
        if !matches!(toks[sig[k - 1]].token, Token::RParen) {
            continue;
        }
        let what = match word(k).as_deref() {
            Some("SEARCH") if matches!(word(k + 1).as_deref(), Some("BREADTH" | "DEPTH")) && word(k + 2).as_deref() == Some("FIRST") => {
                "SEARCH clause of WITH"
            }
            Some("CYCLE") if (k + 1..sig.len()).take_while(|&j| matches!(toks[sig[j]].token, Token::Word(_) | Token::Comma)).any(|j| word(j).as_deref() == Some("SET")) => {
                "CYCLE clause of WITH"
            }
            _ => continue,
        };
        let err = PgError::unimplemented("SQL-46", what);
        return Err(match src.char_pos(toks[sig[k]].span.start) {
            Some(pos) => err.with_position(pos),
            None => err,
        });
    }
    Ok(())
}

/// `ALTER TABLE t ALTER CONSTRAINT name { [NOT] ENFORCED | [NOT] DEFERRABLE | INITIALLY .. }` (gram.y
/// `AlterConstraintStmt`): changing a constraint's attributes in place is not implemented, so the form is 0A000
/// at `CONSTRAINT` instead of a syntax error.
fn reject_alter_constraint(toks: &[TokenWithSpan], src: &SourceMap) -> Result<()> {
    let sig: Vec<usize> = (0..toks.len()).filter(|&i| !matches!(toks[i].token, Token::Whitespace(_) | Token::EOF)).collect();
    let word = |k: usize| sig.get(k).and_then(|&i| bare_word(&toks[i].token));
    if word(0).as_deref() != Some("ALTER") || word(1).as_deref() != Some("TABLE") {
        return Ok(());
    }
    for k in 2..sig.len().saturating_sub(1) {
        if word(k).as_deref() == Some("ALTER") && word(k + 1).as_deref() == Some("CONSTRAINT") {
            let err = PgError::unimplemented("SQL-33", "ALTER TABLE ALTER CONSTRAINT");
            return Err(match src.char_pos(toks[sig[k + 1]].span.start) {
                Some(pos) => err.with_position(pos),
                None => err,
            });
        }
    }
    Ok(())
}

/// `CREATE [GLOBAL|LOCAL] [TEMP|TEMPORARY|UNLOGGED] TABLE [IF NOT EXISTS] name OF type ...` (gram.y
/// `CREATE OptTemp TABLE qualified_name OF any_name`): typed tables are not implemented, so the form is 0A000
/// at `OF` instead of a syntax error.
fn reject_typed_table(toks: &[TokenWithSpan], src: &SourceMap) -> Result<()> {
    let sig: Vec<usize> = (0..toks.len()).filter(|&i| !matches!(toks[i].token, Token::Whitespace(_) | Token::EOF)).collect();
    let word = |k: usize| sig.get(k).and_then(|&i| bare_word(&toks[i].token));
    if word(0).as_deref() != Some("CREATE") {
        return Ok(());
    }
    let mut k = 1;
    while matches!(word(k).as_deref(), Some("GLOBAL" | "LOCAL" | "TEMP" | "TEMPORARY" | "UNLOGGED")) {
        k += 1;
    }
    if word(k).as_deref() != Some("TABLE") {
        return Ok(());
    }
    k += 1;
    if word(k).as_deref() == Some("IF") && word(k + 1).as_deref() == Some("NOT") && word(k + 2).as_deref() == Some("EXISTS") {
        k += 3;
    }
    // The table name: `word [. word]*`.
    if !matches!(sig.get(k).map(|&i| &toks[i].token), Some(Token::Word(_))) {
        return Ok(());
    }
    k += 1;
    while matches!(sig.get(k).map(|&i| &toks[i].token), Some(Token::Period))
        && matches!(sig.get(k + 1).map(|&i| &toks[i].token), Some(Token::Word(_)))
    {
        k += 2;
    }
    if word(k).as_deref() == Some("OF") {
        let err = PgError::unimplemented("SQL-30", "CREATE TABLE OF type (typed tables)");
        return Err(match src.char_pos(toks[sig[k]].span.start) {
            Some(pos) => err.with_position(pos),
            None => err,
        });
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
    let typed = typed_literal_as_cast(toks);
    let nested = cast_collate_as_nested(typed.as_deref().unwrap_or(toks));
    let Some(rewritten) = nested.or(typed) else {
        return Err(err);
    };
    parse_ast_tokens(&rewritten, src).or(Err(err))
}

/// Sig index just past the type that follows the `::` at sig index `colons` (gram.y `Typename`: a possibly
/// qualified name, the multi-word spellings, an optional `(modifiers)` and `[]` suffixes). `None` when no type
/// name follows.
fn cast_type_end(toks: &[TokenWithSpan], sig: &[usize], colons: usize) -> Option<usize> {
    let word = |k: usize| sig.get(k).and_then(|&i| bare_word(&toks[i].token));
    let mut k = colons + 1;
    if !matches!(sig.get(k).map(|&i| &toks[i].token), Some(Token::Word(_))) {
        return None;
    }
    k += 1;
    while matches!(sig.get(k).map(|&i| &toks[i].token), Some(Token::Period))
        && matches!(sig.get(k + 1).map(|&i| &toks[i].token), Some(Token::Word(_)))
    {
        k += 2;
    }
    let before = word(k - 1);
    match (before.as_deref(), word(k).as_deref()) {
        (Some("DOUBLE"), Some("PRECISION")) | (Some("CHARACTER" | "CHAR" | "BIT"), Some("VARYING")) => k += 1,
        _ => {}
    }
    let time_zone = |k: usize| {
        matches!(word(k).as_deref(), Some("WITH" | "WITHOUT"))
            && word(k + 1).as_deref() == Some("TIME")
            && word(k + 2).as_deref() == Some("ZONE")
    };
    if time_zone(k) {
        k += 3;
    }
    if matches!(sig.get(k).map(|&i| &toks[i].token), Some(Token::LParen)) {
        let mut depth = 0usize;
        loop {
            match sig.get(k).map(|&i| &toks[i].token) {
                Some(Token::LParen) => depth += 1,
                Some(Token::RParen) => depth = depth.checked_sub(1)?,
                Some(_) => {}
                None => return None,
            }
            k += 1;
            if depth == 0 {
                break;
            }
        }
        if time_zone(k) {
            k += 3;
        }
    }
    while matches!(sig.get(k).map(|&i| &toks[i].token), Some(Token::LBracket)) {
        k += 1;
        if matches!(sig.get(k).map(|&i| &toks[i].token), Some(Token::Number(..))) {
            k += 1;
        }
        if !matches!(sig.get(k).map(|&i| &toks[i].token), Some(Token::RBracket)) {
            return None;
        }
        k += 1;
    }
    Some(k)
}

/// Sig index of the first token of the cast operand that ends at sig index `end`: a literal, a name, a call, a
/// parenthesized group or an `ARRAY[..]` constructor, extended leftwards over `.` paths, subscripts and further
/// `::type` casts. `None` for anything else.
fn cast_operand_start(toks: &[TokenWithSpan], sig: &[usize], end: usize) -> Option<usize> {
    // Not a function name when it precedes a parenthesized group: clause and operator words.
    const NOT_CALL: &[&str] = &[
        "AND", "OR", "NOT", "IN", "ON", "WHERE", "SELECT", "WHEN", "THEN", "ELSE", "BY", "FROM", "AS", "IS", "LIKE", "ILIKE",
        "BETWEEN", "ANY", "ALL", "SOME", "EXISTS", "JOIN", "USING", "HAVING", "LIMIT", "OFFSET", "SET", "RETURNING",
        "VALUES", "DISTINCT", "UNION", "EXCEPT", "INTERSECT", "ESCAPE", "SIMILAR", "OVER", "WITH", "DO", "INTO", "CASE",
    ];
    let tok = |k: usize| &toks[sig[k]].token;
    // Start of the open bracket matching the closing one at `close`.
    let open_of = |close: usize| -> Option<usize> {
        let mut depth = 0usize;
        let mut k = close;
        loop {
            match tok(k) {
                Token::RParen | Token::RBracket | Token::RBrace => depth += 1,
                Token::LParen | Token::LBracket | Token::LBrace => {
                    depth = depth.checked_sub(1)?;
                    if depth == 0 {
                        return Some(k);
                    }
                }
                _ => {}
            }
            k = k.checked_sub(1)?;
        }
    };
    let atom_start = |e: usize| -> Option<usize> {
        match tok(e) {
            Token::Word(_)
            | Token::Number(..)
            | Token::SingleQuotedString(_)
            | Token::EscapedStringLiteral(_)
            | Token::UnicodeStringLiteral(_)
            | Token::DollarQuotedString(_)
            | Token::Placeholder(_) => Some(e),
            Token::RParen => {
                let open = open_of(e)?;
                match open.checked_sub(1).map(|p| tok(p)) {
                    Some(Token::Word(w)) if !NOT_CALL.contains(&w.value.to_ascii_uppercase().as_str()) || w.quote_style.is_some() => {
                        Some(open - 1)
                    }
                    _ => Some(open),
                }
            }
            Token::RBracket => {
                let open = open_of(e)?;
                match open.checked_sub(1).map(|p| tok(p)) {
                    Some(Token::Word(_) | Token::RParen | Token::RBracket) => Some(open - 1),
                    _ => Some(open),
                }
            }
            _ => None,
        }
    };
    let mut s = atom_start(end)?;
    // A call or subscript start returned the name token or the open bracket; walk further left over the
    // operand chain: `a.b`, `a[1]`, `x::t`.
    loop {
        let Some(p) = s.checked_sub(1) else { break };
        match tok(p) {
            Token::Period if p >= 1 => s = atom_start(p - 1)?,
            Token::DoubleColon if p >= 1 => s = atom_start(p - 1)?,
            _ => break,
        }
    }
    Some(s)
}

/// `expr::type COLLATE name` (PG `a_expr COLLATE any_name`, where `expr::type` is the operand): sqlparser reads
/// `COLLATE` only straight after a primary, so after `::type` the keyword is a syntax error. The cast and its
/// operand are wrapped in parentheses, which does not change the grammar's reading. The inserted tokens are
/// zero-width. Used only when the chunk fails to parse as written ([`parse_ast_chunk`]); `None` when no
/// `::type COLLATE` pair is present.
fn cast_collate_as_nested(toks: &[TokenWithSpan]) -> Option<Vec<TokenWithSpan>> {
    let sig: Vec<usize> = (0..toks.len()).filter(|&i| !matches!(toks[i].token, Token::Whitespace(_))).collect();
    let mut opens = vec![0usize; toks.len()];
    let mut closes = vec![0usize; toks.len()];
    let mut changed = false;
    for c in 1..sig.len() {
        if !matches!(toks[sig[c]].token, Token::DoubleColon) {
            continue;
        }
        let Some(end) = cast_type_end(toks, &sig, c) else { continue };
        if sig.get(end).and_then(|&i| bare_word(&toks[i].token)).as_deref() != Some("COLLATE") {
            continue;
        }
        let Some(start) = cast_operand_start(toks, &sig, c - 1) else { continue };
        opens[sig[start]] += 1;
        closes[sig[end - 1]] += 1;
        changed = true;
    }
    if !changed {
        return None;
    }
    let mut out = Vec::with_capacity(toks.len() + 4);
    for (i, t) in toks.iter().enumerate() {
        for _ in 0..opens[i] {
            out.push(TokenWithSpan::new(Token::LParen, Span::new(t.span.start, t.span.start)));
        }
        out.push(t.clone());
        for _ in 0..closes[i] {
            out.push(TokenWithSpan::new(Token::RParen, Span::new(t.span.end, t.span.end)));
        }
    }
    Some(out)
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
    let toks = mark_conflict_where(&toks, src)?;
    let toks = substring_similar_args(&toks);
    let toks = mark_xml_constructs(&toks);
    let toks = mark_variadic_args(&toks);
    let toks = mark_between_symmetric(&toks);
    let toks = trim_list_forms(&toks);
    let toks = param_name_before_mode(&toks);
    let toks = unnamed_procedure_params(&toks);
    let toks = bare_view_options(&toks);
    alter_function_parallel_value(&toks)?;
    let toks = alter_procedure_as_function(&toks);
    let toks = drop_routine_as_function(&toks);
    let toks = routine_grant_as_function(&toks);
    let toks = maintain_privilege_marker(&toks);
    let toks = set_scoped_session_authorization(&toks);
    let toks = set_schema_and_xml_option(&toks);
    let toks = notify_dollar_payload(&toks);
    let toks = table_storage_clauses(&toks)?;
    let toks = collation_for_as_function(&toks);
    let toks = drop_domain_list_as_type(&toks);
    let toks = domain_optional_as(&toks);
    let toks = domain_null_constraints(&toks);
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
    if type_privilege_statement(&toks) {
        return Err(PgError::unimplemented("SQL-39", "GRANT or REVOKE on a type or domain"));
    }
    if let Some(name) = non_column_privilege_with_columns(&toks) {
        return Err(pgarbor_common::PgError::new(
            pgarbor_common::SqlState(*b"0LP01"),
            format!("invalid privilege type {name} for column"),
        ));
    }
    match language_privilege_statement(&toks) {
        Some(Err(e)) => return Err(e),
        Some(Ok(())) => return Err(PgError::unimplemented("SQL-39", "GRANT or REVOKE on a language")),
        None => {}
    }
    let toks = json_table_default_paths(&toks);
    check_json_clause_errors(&toks, src)?;
    reject_json_query_clauses(&toks, src)?;
    reject_json_constructor_clauses(&toks, src)?;
    reject_json_array_subquery(&toks, src)?;
    let toks = mark_json_unique_keys(&toks);
    let toks = mark_rows_from(&toks);
    let toks = mark_paren_star(&toks);
    let toks = mark_force_quote_star(&toks);
    let toks = drop_external_security(&toks);
    let toks = strip_order_by_whole_row_star(&toks);
    let toks = mark_expression_alters(&toks);
    reject_where_current_of(&toks)?;
    reject_returning_with_alias(&toks, src)?;
    reject_using_alias(&toks, src)?;
    reject_xml_syntax(&toks, src)?;
    reject_cte_search_cycle(&toks, src)?;
    reject_typed_table(&toks, src)?;
    reject_alter_constraint(&toks, src)?;
    reject_array_constructor_subscript(&toks, src)?;
    reject_constraint_storage_params(&toks, src)?;
    let toks = values_quantified_operand(&toks);
    let (toks, unique_nulls) = mark_column_unique_nulls(&toks);
    let (toks, virtual_flags) = mark_virtual_generated(&toks);
    reject_table_command_clauses(&toks, src)?;
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
    restore_column_unique_nulls(&mut stmt, &unique_nulls)?;
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
    fn like_option_lists_reach_the_binder() {
        let sql = "create table d (like s including all excluding comments including indexes)";
        let src = SourceMap::new(sql);
        let toks = Tokenizer::new(&PostgreSqlDialect {}, sql).tokenize_with_location().expect("tokenizes");
        let stripped = strip_like_options(toks);
        let words: Vec<String> = stripped.iter().filter_map(|t| bare_word(&t.token)).collect();
        assert!(!words.iter().any(|w| w == "INCLUDING" || w == "EXCLUDING"));
        let _ = src;
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

    /// `expr::type COLLATE name` parses (the cast and its operand are wrapped); a column-level `COLLATE` and a
    /// `COLLATE` after a clause keyword are left alone.
    #[test]
    fn collate_after_cast_parses() {
        assert!(parse_one("SELECT NULL::text COLLATE \"C\" < 'a' COLLATE \"C\" AS c").is_ok());
        assert!(parse_one("SELECT array_sort('{b,a}'::text[] COLLATE \"C\")").is_ok());
        assert!(parse_one("SELECT lower(x)::varchar(5) COLLATE \"C\", (y)::text COLLATE \"C\" FROM t").is_ok());
        assert!(parse_one("SELECT 1::int::text COLLATE \"C\"").is_ok());
        let lex = |sql: &str| {
            Tokenizer::new(&PostgreSqlDialect {}, sql).tokenize_with_location().expect("test SQL tokenizes")
        };
        assert!(cast_collate_as_nested(&lex("CREATE TABLE t (a int, b text COLLATE \"C\")")).is_none());
        assert!(cast_collate_as_nested(&lex("SELECT a::text FROM t ORDER BY b COLLATE \"C\"")).is_none());
    }

    /// Expression elements of an ON CONFLICT inference list parse; trailing index options are 0A000.
    #[test]
    fn conflict_expression_elements_parse() {
        assert!(parse("insert into t values (21, 'QUINCE') on conflict (lower(fruit)) do update set fruit = excluded.fruit").is_ok());
        assert!(parse("insert into t values (1, 2) on conflict ((a + 1), b) do nothing").is_ok());
        let err = parse("insert into t values (1) on conflict (a + 1 desc) do nothing").err().expect("rejected");
        assert_eq!(err.sqlstate, pgarbor_common::SqlState::FEATURE_NOT_SUPPORTED);
    }

    /// GiST `tsvector_ops (siglen = n)`: range errors are 22023, `DEFAULT (..)` is a syntax error.
    #[test]
    fn gist_siglen_validation() {
        let code = |sql: &str| parse(sql).err().map(|e| e.sqlstate.code().to_string());
        assert_eq!(code("CREATE INDEX i ON d USING GIST (tsv tsvector_ops (siglen = 0))").as_deref(), Some("22023"));
        assert_eq!(code("CREATE INDEX i ON d USING GIST (tsv tsvector_ops (siglen = 2025))").as_deref(), Some("22023"));
        assert_eq!(code("CREATE INDEX i ON d USING GIST (tsv tsvector_ops (siglen = 'abc'))").as_deref(), Some("22023"));
        assert_eq!(code("CREATE INDEX i ON d USING GIST (tsv DEFAULT (siglen = 100))").as_deref(), Some("42601"));
        assert!(parse("CREATE INDEX i ON d USING GIST (tsv tsvector_ops (siglen = 2024))").is_ok());
    }

    /// SQL/JSON parse-analysis errors: WITH WRAPPER + OMIT QUOTES, bad top-level ON ERROR, double ORDINALITY.
    #[test]
    fn json_clause_parse_errors() {
        let msg = |sql: &str| parse(sql).err().map(|e| (e.sqlstate.code().to_string(), e.message, e.position));
        let (st, m, pos) = msg("SELECT JSON_QUERY(jsonb '[1]', '$' WITH WRAPPER OMIT QUOTES)").expect("error");
        assert_eq!((st.as_str(), pos), ("42601", Some(8)));
        assert!(m.contains("QUOTES behavior"), "{m}");
        let (_, m, pos) =
            msg("SELECT * FROM JSON_TABLE('[]', '$' COLUMNS (a int PATH '$') NULL ON ERROR) jt").expect("error");
        assert_eq!(m, "invalid ON ERROR behavior");
        assert_eq!(pos, Some(61));
        let (_, m, pos) =
            msg("SELECT * FROM JSON_TABLE('1', '$' COLUMNS (i FOR ORDINALITY, j FOR ORDINALITY)) jt").expect("error");
        assert_eq!(m, "only one FOR ORDINALITY column is allowed");
        assert_eq!(pos, Some(62));
        let (st, _, _) = msg("SELECT * FROM JSON_TABLE('1', '$' COLUMNS (a text PATH '$' KEEP QUOTES)) jt").expect("error");
        assert_eq!(st, "0A000");
        let e = parse("SELECT JSON('{\"a\":1}' FORMAT JSON)").err().expect("0A000");
        assert_eq!((e.sqlstate.code(), e.position), ("0A000", Some(8)));
    }

    /// Nested `GROUPING SETS`, `CUBE` and `ROLLUP` elements inside `GROUPING SETS` parse (flattened).
    #[test]
    fn nested_grouping_sets_parse() {
        for sql in [
            "SELECT sum(c) FROM t GROUP BY GROUPING SETS ((), GROUPING SETS ((), GROUPING SETS (())))",
            "SELECT a FROM t GROUP BY GROUPING SETS (a, GROUPING SETS (a, CUBE (b)))",
            "SELECT a FROM t GROUP BY GROUPING SETS (ROLLUP (a, (b, c)), d)",
        ] {
            assert!(parse(sql).is_ok(), "{sql}");
        }
    }

    /// `ON CONFLICT (cols) WHERE pred`, `substring(x SIMILAR p ESCAPE e)` and the SQL/XML call forms parse.
    #[test]
    fn conflict_where_substring_similar_and_xml_parse() {
        assert!(parse("insert into t values (1, true) on conflict (a) where ok do nothing").is_ok());
        assert!(parse("insert into t values (1, true) on conflict (a) where (ok and a > 0) do update set a = 2").is_ok());
        assert!(parse("select substring('foobar' similar '%#\"o_b#\"%' escape '#')").is_ok());
        assert!(parse("select 'a' similar to 'a' escape '#'").is_ok());
        assert!(parse("select xmlelement(name foo, xmlattributes('x' as bar)), xmlpi(name foo, 'bar')").is_ok());
    }

    #[test]
    fn parse_column_unique_nulls_not_distinct() {
        let sql = "CREATE TABLE t (i int UNIQUE NULLS NOT DISTINCT, j int UNIQUE, k int UNIQUE NULLS DISTINCT)";
        assert!(parse_one(sql).is_ok());
    }

    #[test]
    fn between_symmetric_and_trim_list_forms_parse() {
        assert!(parse("select 2 between symmetric 3 and 1, 2 not between symmetric (1 + 2) and 1").is_ok());
        assert!(parse("select x between symmetric 1 and 2 and y > 0 from t").is_ok());
        assert!(parse("select trim(both from 'ab', 'a'), trim(leading from 'ab'), trim(trailing 'ab', 'b')").is_ok());
        assert!(parse("select trim('a' from 'ab'), trim(both 'a' from 'ab'), trim('ab')").is_ok());
        assert!(parse("select substring('Thomas' for 2)").is_ok());
    }

    #[test]
    fn window_frame_exclusion_parses() {
        assert!(parse("select sum(v) over (order by id rows between 1 preceding and 1 following exclude current row) from f").is_ok());
        assert!(parse("select sum(v) over (order by id groups current row exclude no others) from f").is_ok());
        assert!(parse("select sum(v) over (order by id range unbounded preceding exclude ties) from f").is_ok());
        assert!(parse("select sum(v) over (order by id exclude ties) from f").is_err());
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
    fn xml_is_document_and_xmlparse_are_leased_0a000() {
        for sql in [
            "SELECT '<a/>'::xml IS DOCUMENT",
            "SELECT xml 'abc' IS NOT DOCUMENT",
        ] {
            let err = parse_one(sql).err();
            assert!(err.is_some_and(|e| e.sqlstate == pgarbor_common::SqlState::FEATURE_NOT_SUPPORTED), "{sql}");
        }
        // XMLPARSE is rewritten to a marker call by the XML function pass and rejected (0A000, SQL-10)
        // by the binder, so it parses.
        assert!(parse_one("SELECT XMLPARSE (CONTENT 'x')").is_ok());
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
    fn insert_overriding_clause_is_extracted() {
        use crate::ir::stmt::Overriding;
        let sys = parse("INSERT INTO t (a) OVERRIDING SYSTEM VALUE VALUES (1)").unwrap();
        assert_eq!(sys[0].overriding, Some(Overriding::System));
        let user = parse("INSERT INTO t OVERRIDING USER VALUE SELECT 1").unwrap();
        assert_eq!(user[0].overriding, Some(Overriding::User));
        assert!(parse("INSERT INTO t OVERRIDING SYSTEM VALUE DEFAULT VALUES").is_err());
        assert_eq!(parse("INSERT INTO t VALUES (1)").unwrap()[0].overriding, None);
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
        // An unknown CREATE OPERATOR attribute is only a WARNING in PostgreSQL, so it reaches the support check.
        let e = parse_one("CREATE OPERATOR #@%# (rightarg = int8, procedure = dbl, invalid_att = int8)")
            .err()
            .expect("unsupported");
        assert_eq!(e.sqlstate.code(), "0A000");
        // `SET SCHEMA 'x'` and `SET XML OPTION` are special SET spellings.
        assert!(parse("SET SCHEMA 'x'").is_ok());
        assert!(parse("SET XML OPTION DOCUMENT").is_ok());
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
    fn alter_function_invalid_parallel_value_is_not_a_syntax_error_at_the_word() {
        let err = parse("ALTER FUNCTION f(text) PARALLEL BOGUS").expect_err("invalid PARALLEL value");
        assert_eq!(err.message, "parameter \"parallel\" must be SAFE, RESTRICTED, or UNSAFE");
        assert!(err.position.is_none());
        assert!(parse("ALTER FUNCTION f(text) PARALLEL RESTRICTED").is_ok());
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
    fn external_security_force_quote_star_and_order_by_star_parse() {
        let f = parse("CREATE FUNCTION sdf(int) RETURNS int LANGUAGE sql EXTERNAL SECURITY DEFINER AS 'select 1'").expect("parse");
        assert!(matches!(
            f[0].as_ast(),
            Some(Statement::CreateFunction(cf)) if matches!(cf.security, Some(sqlparser::ast::FunctionSecurity::Definer))
        ));
        let c = parse("COPY (SELECT 1) TO STDOUT (FORMAT csv, FORCE_QUOTE *)").expect("parse");
        assert!(matches!(c[0].as_ast(), Some(Statement::Copy { .. })));
        let o = parse("SELECT * FROM t c ORDER BY c.*, c.a").expect("parse");
        assert!(matches!(o[0].as_ast(), Some(Statement::Query(_))));
    }

    #[test]
    fn function_param_mode_after_name_parses() {
        let create = parse("CREATE FUNCTION dup (f1 anyelement, f2 out anyelement, f3 out anyarray) AS 'select $1, array[$1,$1]' LANGUAGE sql").expect("parse");
        assert!(matches!(create[0].as_ast(), Some(Statement::CreateFunction(_))));
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
    fn json_array_subquery_is_leased_unimplemented() {
        let err = parse("SELECT JSON_ARRAY(SELECT i FROM (VALUES (1)) foo(i) ORDER BY i)").unwrap_err();
        assert_eq!(err.sqlstate.code(), "0A000");
        assert!(parse("SELECT JSON_ARRAY(1, 2)").is_ok());
    }

    #[test]
    fn json_unique_keys_clause_parses() {
        assert!(parse("SELECT JSON_OBJECTAGG(k : v ABSENT ON NULL WITH UNIQUE KEYS) FROM t").is_ok());
        assert!(parse("SELECT JSON_OBJECT('a' VALUE 1 WITHOUT UNIQUE KEYS)").is_ok());
        assert!(parse("CREATE UNIQUE INDEX i ON t (a)").is_ok());
    }

    #[test]
    fn json_table_passing_is_leased_unimplemented() {
        let r = parse("SELECT * FROM JSON_TABLE(jsonb '[1]', '$[*] ? (@ < $x)' PASSING 3 AS x COLUMNS (y int PATH '$')) jt");
        assert!(matches!(r, Err(ref e) if e.is_unimplemented()));
    }

    #[test]
    fn json_exists_passing_is_leased_unimplemented() {
        let r = parse("SELECT JSON_EXISTS(jsonb '{}', '$.a ? (@ > $x)' PASSING 2 AS x)");
        assert!(matches!(r, Err(ref e) if e.is_unimplemented()));
    }

    #[test]
    fn returning_with_old_new_alias_is_leased_unimplemented() {
        let r = parse("DELETE FROM t RETURNING WITH (OLD AS o, NEW AS n) o.a, n.a");
        assert!(matches!(r, Err(ref e) if e.is_unimplemented()));
    }

    #[test]
    fn json_value_on_error_is_leased_unimplemented() {
        let r = parse("SELECT JSON_VALUE(jsonb '1', '$' RETURNING int ERROR ON ERROR)");
        assert!(matches!(r, Err(ref e) if e.is_unimplemented()));
    }

    #[test]
    fn cte_search_and_cycle_clauses_are_leased_unimplemented() {
        for sql in [
            "WITH RECURSIVE s(id) AS (SELECT 1 UNION ALL SELECT id FROM s) SEARCH BREADTH FIRST BY id SET o SELECT * FROM s",
            "WITH RECURSIVE s(id) AS (SELECT 1 UNION ALL SELECT id FROM s) CYCLE id SET is_c USING p SELECT * FROM s",
        ] {
            assert!(matches!(parse(sql), Err(ref e) if e.is_unimplemented()), "{sql}");
        }
    }

    #[test]
    fn create_table_of_type_is_leased_unimplemented() {
        for sql in ["CREATE TABLE tt OF pt", "CREATE TEMP TABLE IF NOT EXISTS s.tt OF pt (a WITH OPTIONS NOT NULL)"] {
            assert!(matches!(parse(sql), Err(ref e) if e.is_unimplemented()), "{sql}");
        }
    }

    #[test]
    fn ctas_with_column_name_list_parses_as_untyped_columns() {
        let stmt = parse("CREATE TEMP TABLE temptest (col) ON COMMIT DELETE ROWS AS SELECT 1").expect("parse");
        assert!(matches!(stmt[0].as_ast(), Some(Statement::CreateTable(ct)) if ct.columns.len() == 1 && ct.query.is_some()));
        assert!(parse("CREATE TABLE renamed (a, b) AS SELECT 1 AS x, 'two' AS y").is_ok());
    }

    #[test]
    fn create_domain_null_constraints_and_late_default() {
        for sql in [
            "CREATE DOMAIN d AS int NOT NULL DEFAULT NULL",
            "CREATE DOMAIN d AS int DEFAULT 1 NOT NULL CHECK (VALUE > 0)",
            "CREATE DOMAIN d AS int CONSTRAINT c NOT NULL NULL",
            "CREATE DOMAIN d varchar(5)",
            "CREATE DOMAIN s.d int NOT NULL",
            "CREATE DOMAIN positiveint int4 CHECK (VALUE > 0)",
        ] {
            let list = parse(sql).expect("parse");
            assert!(matches!(list[0].as_ast(), Some(Statement::CreateDomain(_))), "{sql}");
        }
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
    fn table_command_rejects_where_but_allows_order_by() {
        assert!(parse("TABLE t ORDER BY id LIMIT 2").is_ok());
        assert!(parse("TABLE s.t").is_ok());
        let err = parse("TABLE t WHERE id = 1").err().map(|e| e.message);
        assert_eq!(err.as_deref(), Some("syntax error at or near \"WHERE\""));
    }

    #[test]
    fn invalid_grant_privilege_forms_report_pg_errors() {
        let lang = parse_one("GRANT EXECUTE ON LANGUAGE sql TO r").err().map(|e| (e.sqlstate.code().to_string(), e.message));
        assert_eq!(lang, Some(("0LP01".to_string(), "invalid privilege type EXECUTE for language".to_string())));
        let col = parse_one("GRANT DELETE (a) ON t TO r").err().map(|e| (e.sqlstate.code().to_string(), e.message));
        assert_eq!(col, Some(("0LP01".to_string(), "invalid privilege type DELETE for column".to_string())));
        assert!(parse_one("GRANT TEMP ON SCHEMA s TO r").is_ok());
    }

    #[test]
    fn maintain_privilege_is_marked_only_in_privilege_lists() {
        let toks = |sql: &str| {
            let dialect = PostgreSqlDialect {};
            Tokenizer::new(&dialect, sql).tokenize_with_location().unwrap_or_default()
        };
        let marked = |sql: &str| maintain_privilege_marker(&toks(sql)).len() != toks(sql).len();
        assert!(marked("REVOKE MAINTAIN ON t FROM r"));
        assert!(marked("GRANT SELECT, MAINTAIN ON t TO r"));
        assert!(!marked("GRANT maintain TO r"));
        assert!(!marked("GRANT SELECT (maintain) ON t TO r"));
    }

    #[test]
    fn type_and_domain_grants_are_leased_and_notify_accepts_dollar_payload() {
        for sql in ["GRANT USAGE ON TYPE public.e TO r", "REVOKE USAGE ON DOMAIN public.d FROM PUBLIC"] {
            let e = parse_one(sql).err();
            assert!(e.is_some_and(|e| e.sqlstate == pgarbor_common::SqlState::FEATURE_NOT_SUPPORTED), "{sql}");
        }
        // A table named type is still a table.
        let table = parse_one("GRANT SELECT ON type TO r").err();
        assert!(!table.is_some_and(|e| e.sqlstate == pgarbor_common::SqlState::FEATURE_NOT_SUPPORTED));
        assert!(parse_one("NOTIFY ch, $$dollar 'quoted' payload$$").is_ok());
        assert!(parse_one("NOTIFY ch, U&'\\0061'").is_ok());
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

    #[test]
    fn pg_lexical_forms_are_normalized() {
        let norm = |sql: &str| -> String {
            let toks = Tokenizer::new(&PostgreSqlDialect {}, sql).tokenize_with_location().expect("tokenizes");
            pg_lexical_tokens(toks).iter().map(|t| t.token.to_string()).collect()
        };
        assert_eq!(norm("select 0b_10_01, 0O273"), "select 0b_10_01, 0O273");
        assert_eq!(norm("select 'foo'\n'bar'"), "select 'foobar'");
        assert_eq!(norm("select 'foo' 'bar'"), "select 'foo' 'bar'");
        assert_eq!(norm("select 1 isnull, 2 NOTNULL"), "select 1 IS NULL, 2 IS NOT NULL");
        assert_eq!(norm("select isnull(1)"), "select isnull(1)");
        assert_eq!(norm("select 0b12"), "select 0b12");
    }

    #[test]
    fn doc_lexical_and_call_forms_parse() {
        assert!(parse("SELECT 0XFFFF, 0x42f").is_ok());
        let e = parse("SELECT 1__0").err().expect("junk");
        assert_eq!(e.message, "trailing junk after numeric literal at or near \"1__0\"");
        assert!(parse("SELECT OPERATOR(pg_catalog.@) '-4.5', 1 OPERATOR(pg_catalog.+) 2, OPERATOR(pg_catalog.-) 5").is_ok());
        assert!(parse("SELECT f(a := 1, b := 2)").is_ok());
        assert!(parse("CREATE TABLE u_t (U&\"d\\0061t\\+000061\" int4)").is_ok());
        assert!(parse("SELECT 1 AS U&\"a!!b\" UESCAPE '!'").is_ok());
        let e = parse("SELECT U&\"x\\zz\"").err().expect("bad escape");
        assert_eq!(e.sqlstate.code(), "42601");
        let e = parse("ALTER TABLE fk ALTER CONSTRAINT fk_ne ENFORCED").err().expect("unsupported");
        assert_eq!(e.sqlstate.code(), "0A000");
    }

    #[test]
    fn join_using_alias_is_leased_unsupported() {
        let e = parse("select * from a join b using (i) as x").err().expect("unsupported");
        assert_eq!(e.sqlstate.code(), "0A000");
    }

    #[test]
    fn rows_from_and_paren_star_parse() {
        for sql in [
            "select * from rows from (generate_series(1,3), generate_series(10,11)) with ordinality as g(i,j,o)",
            "select * from rows from (json_to_recordset('[]') as (a integer, b numeric(10,2)), generate_series(1,3)) as x(p,q,r,s)",
            "select (c).* from t c",
            "select (f(x)).* from t",
            "select a, rows from (select 1 as rows) s",
        ] {
            assert_eq!(parse(sql).expect(sql).len(), 1, "{sql}");
        }
    }
    #[test]
    fn pg_only_clauses_reach_the_ast() {
        for sql in [
            "DROP INDEX CONCURRENTLY IF EXISTS i",
            "COMMENT ON TABLE t IS $$a 'b' -- c$$",
            "CREATE FUNCTION f(int) RETURNS int LANGUAGE sql IMMUTABLE COST 7 AS 'select $1'",
            "CREATE FUNCTION f(int) RETURNS SETOF int LANGUAGE sql ROWS 5 NOT LEAKPROOF AS 'select $1'",
            "CREATE PROCEDURE p(x t.id%TYPE, y s.t.label%TYPE) LANGUAGE sql AS 'select 1'",
            "CREATE RECURSIVE VIEW public.v (n) AS VALUES (1) UNION ALL SELECT n+1 FROM v WHERE n < 100",
        ] {
            assert_eq!(parse(sql).expect(sql).len(), 1, "{sql}");
        }
    }

    #[test]
    fn drop_index_concurrently_rejects_lists_and_cascade() {
        for sql in ["DROP INDEX CONCURRENTLY a, b", "DROP INDEX CONCURRENTLY a CASCADE"] {
            let e = parse(sql).err().expect(sql);
            assert_eq!(e.sqlstate, pgarbor_common::SqlState::FEATURE_NOT_SUPPORTED, "{sql}");
        }
    }

    #[test]
    fn legacy_operator_options_are_unsupported_not_syntax_errors() {
        let e = parse("CREATE OPERATOR ## (LEFTARG = int8, PROCEDURE = f, SORT1 = int8lt, LTCMP = int8lt)")
            .err()
            .expect("CREATE OPERATOR is not supported");
        assert_eq!(e.sqlstate, pgarbor_common::SqlState::FEATURE_NOT_SUPPORTED);
    }

    #[test]
    fn recursive_view_with_check_option_is_rejected() {
        let e = parse("CREATE RECURSIVE VIEW v (n) AS VALUES (1) WITH CHECK OPTION").err().expect("rejected");
        assert_eq!(e.message, "WITH CHECK OPTION not supported on recursive views");
    }
}
