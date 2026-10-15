//! Hand parser for PostgreSQL statements `sqlparser` lacks (SQL-02). See `extra_stmt` for the AST.
//!
//! Called by `parser::parse` with the token vector of ONE statement chunk (already terminated by
//! `Token::EOF`). The functions use `sqlparser::parser::Parser` as a token cursor:
//! `Parser::new(&PostgreSqlDialect{}).with_tokens_with_locations(tokens)`, then the public helpers
//! `parse_keyword`, `parse_keywords`, `parse_one_of_keywords`, `expect_keyword`, `consume_token`,
//! `parse_object_name(false)`, `parse_identifier()`, `parse_literal_string()`, `parse_expr()`,
//! `parse_data_type()`, `parse_comma_separated(..)`, `peek_token()`, `next_token()`.
//!
//! Keywords are matched as unquoted words, case-insensitively, through local helpers. Several
//! PostgreSQL keywords used here (`CONCURRENTLY`, `ALLOW_CONNECTIONS`, `IS_TEMPLATE`, `NONE`, ...)
//! are not in sqlparser's keyword list, so `Parser::parse_keyword` cannot match them.
//!
//! Grammar sources: `src/backend/parser/gram.y` (PostgreSQL 18.6). Productions are cited per
//! function; the code is written from the grammar, not copied from it.
use pgarbor_common::{PgError, Result, SqlState};
use sqlparser::ast::{DataType, Expr, Ident, ObjectName};
use sqlparser::dialect::PostgreSqlDialect;
use sqlparser::keywords::Keyword;
use sqlparser::parser::{Parser, ParserError};
use sqlparser::tokenizer::{Token, TokenWithSpan};

use crate::diag;
use crate::extra_stmt::{
    AlterDatabaseAction, AlterDomainAction, AlterMatViewAction, AlterSequenceAction, ExtraStatement, ReindexKind,
    RoleOptionAst, RoleOptionItem, RoleStmtKind, SequenceOptionAst,
};
use crate::parser::RECURSION_LIMIT;
use crate::source::SourceMap;
use crate::syntax_error::location_from_message;

/// Does this chunk start with a keyword sequence that belongs to [`ExtraStatement`]?
///
/// Decision table over the first non-whitespace tokens (case-insensitive keywords):
/// * `REFRESH MATERIALIZED VIEW`, `REINDEX`, `CLUSTER`, `CHECKPOINT`, `SET CONSTRAINTS`,
///   `DO`, `DROP OWNED`, `REASSIGN OWNED`, `ALTER SEQUENCE`, `ALTER DOMAIN`, `ALTER DATABASE`,
///   `ALTER MATERIALIZED VIEW` -> true;
/// * recognized-unsupported PostgreSQL commands (the list in `extra_stmt` docs; also
///   `CREATE {RULE|CAST|TABLESPACE|EVENT TRIGGER|FOREIGN TABLE|FOREIGN DATA WRAPPER|PUBLICATION|
///   SUBSCRIPTION|STATISTICS|LANGUAGE|ACCESS METHOD|TRANSFORM|AGGREGATE|CONVERSION|USER MAPPING|
///   [TRUSTED] [PROCEDURAL] LANGUAGE|TEMP...}`, `ALTER {SYSTEM|DEFAULT PRIVILEGES|RULE|PUBLICATION|
///   SUBSCRIPTION|EXTENSION|TABLESPACE|FOREIGN TABLE|...}`, `SECURITY LABEL`, `IMPORT FOREIGN
///   SCHEMA`, `PREPARE TRANSACTION`, `COMMIT PREPARED`, `ROLLBACK PREPARED`, `DROP {RULE|CAST|
///   TABLESPACE|EVENT TRIGGER|FOREIGN TABLE|...}`) -> true (parsed to `Unsupported`);
/// * everything else -> false (handled by `sqlparser`).
/// `CREATE EXTENSION`, `CREATE TRIGGER`, `CREATE POLICY`, `MERGE`, cursors are parsed by sqlparser
/// and rejected by `classify`, not here.
pub fn is_extra_statement(tokens: &[TokenWithSpan]) -> Result<bool> {
    Ok(route(&head_words(tokens)).is_some())
}

/// Parse the chunk (tokens end with EOF) into an [`ExtraStatement`].
///
/// Grammar references: `src/backend/parser/gram.y` productions `RefreshMatViewStmt`, `ReindexStmt`,
/// `ClusterStmt`, `CheckPointStmt`, `ConstraintsSetStmt`, `DoStmt`, `AlterSeqStmt` (+ `SeqOptElem`),
/// `AlterDomainStmt`, `AlterDatabaseStmt`/`AlterDatabaseSetStmt`, `DropOwnedStmt`,
/// `ReassignOwnedStmt`, `AlterTableStmt` (ALTER MATERIALIZED VIEW forms).
/// Syntax errors are returned through `syntax_error::translate_parser_error`-compatible PgErrors
/// (`syntax error at or near "tok"` with `src.pos_of_span(tok.span)`); trailing tokens before EOF are
/// a syntax error at the first extra token. `DO`: the code is the single string literal (any form
/// `Parser::parse_literal_string` accepts, incl. `$$..$$`), optional `LANGUAGE ident` before or after.
/// Numbers in `SequenceOptionAst` keep sign and digits as text (`- 5` with the unary minus folded in).
pub fn parse_extra(tokens: Vec<TokenWithSpan>, src: &SourceMap) -> Result<ExtraStatement> {
    let head = head_words(&tokens);
    let route = route(&head);
    let dialect = PostgreSqlDialect {};
    let mut cx = Cx {
        p: Parser::new(&dialect).with_recursion_limit(RECURSION_LIMIT).with_tokens_with_locations(tokens.clone()),
        tokens,
        src,
    };
    match route {
        None => Err(cx.fail_here()),
        Some(Route::Unsupported(feature)) => Ok(unsupported(feature)),
        Some(Route::Known(k)) => match k {
            Known::Refresh => parse_refresh(&mut cx),
            Known::Reindex => parse_reindex(&mut cx),
            Known::Cluster => parse_cluster(&mut cx),
            Known::Checkpoint => parse_checkpoint(&mut cx),
            Known::SetConstraints => parse_set_constraints(&mut cx),
            Known::Do => parse_do(&mut cx),
            Known::DropOwned => parse_drop_owned(&mut cx),
            Known::ReassignOwned => parse_reassign_owned(&mut cx),
            Known::AlterSequence => parse_alter_sequence(&mut cx),
            Known::CreateSequence => parse_create_sequence(&mut cx),
            Known::CreateRole => parse_create_role(&mut cx),
            Known::AlterRole => parse_alter_role(&mut cx),
            Known::AlterDomain => parse_alter_domain(&mut cx),
            Known::AlterDatabase => parse_alter_database(&mut cx),
            Known::AlterMaterializedView => parse_alter_matview(&mut cx),
        },
    }
}

// ---------------------------------------------------------------------------------------------
// Statement head routing (no parsing; works on the token slice before the cursor exists)
// ---------------------------------------------------------------------------------------------

/// Statement kinds this module parses fully.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Known {
    Refresh,
    Reindex,
    Cluster,
    Checkpoint,
    SetConstraints,
    Do,
    DropOwned,
    ReassignOwned,
    AlterSequence,
    AlterDomain,
    AlterDatabase,
    AlterMaterializedView,
    /// `CREATE [GLOBAL|LOCAL] [TEMP|TEMPORARY|UNLOGGED] SEQUENCE ...` (options in any order).
    CreateSequence,
    /// `CREATE {ROLE|USER|GROUP} ...` (options in any order, signed numbers).
    CreateRole,
    /// `ALTER {ROLE|USER} name [WITH] options` (not RENAME / SET / RESET / IN DATABASE).
    AlterRole,
}

/// Where a chunk goes: a fully parsed statement, or a recognized-but-unsupported feature name.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Route {
    Known(Known),
    Unsupported(&'static str),
}

/// Leading keyword sequences of the fully parsed statements (checked first).
const KNOWN_HEADS: &[(&[&str], Known)] = &[
    (&["REFRESH", "MATERIALIZED", "VIEW"], Known::Refresh),
    (&["REINDEX"], Known::Reindex),
    (&["CLUSTER"], Known::Cluster),
    (&["CHECKPOINT"], Known::Checkpoint),
    (&["SET", "CONSTRAINTS"], Known::SetConstraints),
    (&["DO"], Known::Do),
    (&["DROP", "OWNED"], Known::DropOwned),
    (&["REASSIGN", "OWNED"], Known::ReassignOwned),
    (&["ALTER", "SEQUENCE"], Known::AlterSequence),
    (&["ALTER", "DOMAIN"], Known::AlterDomain),
    (&["ALTER", "DATABASE"], Known::AlterDatabase),
    (&["ALTER", "MATERIALIZED", "VIEW"], Known::AlterMaterializedView),
];

/// Recognized PostgreSQL commands PGArbor does not implement; the feature text is the 0A000 subject.
const UNSUPPORTED_HEADS: &[(&[&str], &str)] = &[
    (&["CREATE", "RULE"], "CREATE RULE"),
    (&["CREATE", "CAST"], "CREATE CAST"),
    (&["CREATE", "TABLESPACE"], "CREATE TABLESPACE"),
    (&["CREATE", "EVENT", "TRIGGER"], "CREATE EVENT TRIGGER"),
    (&["CREATE", "FOREIGN", "TABLE"], "CREATE FOREIGN TABLE"),
    (&["CREATE", "FOREIGN", "DATA", "WRAPPER"], "CREATE FOREIGN DATA WRAPPER"),
    (&["CREATE", "SERVER"], "CREATE SERVER"),
    (&["CREATE", "PUBLICATION"], "CREATE PUBLICATION"),
    (&["CREATE", "SUBSCRIPTION"], "CREATE SUBSCRIPTION"),
    (&["CREATE", "STATISTICS"], "CREATE STATISTICS"),
    (&["CREATE", "LANGUAGE"], "CREATE LANGUAGE"),
    (&["CREATE", "ACCESS", "METHOD"], "CREATE ACCESS METHOD"),
    (&["CREATE", "TRANSFORM"], "CREATE TRANSFORM"),
    (&["CREATE", "AGGREGATE"], "CREATE AGGREGATE"),
    (&["CREATE", "CONVERSION"], "CREATE CONVERSION"),
    (&["CREATE", "USER", "MAPPING"], "CREATE USER MAPPING"),
    (&["ALTER", "SYSTEM"], "ALTER SYSTEM"),
    (&["ALTER", "DEFAULT", "PRIVILEGES"], "ALTER DEFAULT PRIVILEGES"),
    (&["ALTER", "RULE"], "ALTER RULE"),
    (&["ALTER", "PUBLICATION"], "ALTER PUBLICATION"),
    (&["ALTER", "SUBSCRIPTION"], "ALTER SUBSCRIPTION"),
    (&["ALTER", "EXTENSION"], "ALTER EXTENSION"),
    (&["ALTER", "TABLESPACE"], "ALTER TABLESPACE"),
    (&["ALTER", "FOREIGN", "TABLE"], "ALTER FOREIGN TABLE"),
    (&["ALTER", "FOREIGN", "DATA", "WRAPPER"], "ALTER FOREIGN DATA WRAPPER"),
    (&["ALTER", "SERVER"], "ALTER SERVER"),
    (&["ALTER", "EVENT", "TRIGGER"], "ALTER EVENT TRIGGER"),
    (&["ALTER", "STATISTICS"], "ALTER STATISTICS"),
    (&["ALTER", "LANGUAGE"], "ALTER LANGUAGE"),
    (&["ALTER", "ACCESS", "METHOD"], "ALTER ACCESS METHOD"),
    (&["ALTER", "CONVERSION"], "ALTER CONVERSION"),
    (&["ALTER", "USER", "MAPPING"], "ALTER USER MAPPING"),
    (&["DROP", "RULE"], "DROP RULE"),
    (&["DROP", "CAST"], "DROP CAST"),
    (&["DROP", "TABLESPACE"], "DROP TABLESPACE"),
    (&["DROP", "EVENT", "TRIGGER"], "DROP EVENT TRIGGER"),
    (&["DROP", "FOREIGN", "TABLE"], "DROP FOREIGN TABLE"),
    (&["DROP", "FOREIGN", "DATA", "WRAPPER"], "DROP FOREIGN DATA WRAPPER"),
    (&["DROP", "SERVER"], "DROP SERVER"),
    (&["DROP", "PUBLICATION"], "DROP PUBLICATION"),
    (&["DROP", "SUBSCRIPTION"], "DROP SUBSCRIPTION"),
    (&["DROP", "STATISTICS"], "DROP STATISTICS"),
    (&["DROP", "LANGUAGE"], "DROP LANGUAGE"),
    (&["DROP", "ACCESS", "METHOD"], "DROP ACCESS METHOD"),
    (&["DROP", "TRANSFORM"], "DROP TRANSFORM"),
    (&["DROP", "CONVERSION"], "DROP CONVERSION"),
    (&["DROP", "USER", "MAPPING"], "DROP USER MAPPING"),
    (&["SECURITY", "LABEL"], "SECURITY LABEL"),
    (&["IMPORT", "FOREIGN", "SCHEMA"], "IMPORT FOREIGN SCHEMA"),
    (&["PREPARE", "TRANSACTION"], "PREPARE TRANSACTION"),
    (&["COMMIT", "PREPARED"], "COMMIT PREPARED"),
    (&["ROLLBACK", "PREPARED"], "ROLLBACK PREPARED"),
    (&["LOAD"], "LOAD"),
];

/// Upper-cased unquoted words of the chunk's first significant tokens (`""` for any other token),
/// normalized so that `CREATE OR REPLACE` and `CREATE [TRUSTED] [PROCEDURAL] LANGUAGE` match the
/// plain patterns. Whitespace and comments are skipped.
fn head_words(tokens: &[TokenWithSpan]) -> Vec<String> {
    let mut head: Vec<String> = tokens
        .iter()
        .filter(|t| !matches!(t.token, Token::Whitespace(_)))
        .take(6)
        .map(|t| match &t.token {
            Token::Word(w) if w.quote_style.is_none() => w.value.to_ascii_uppercase(),
            _ => String::new(),
        })
        .collect();
    if head.len() >= 3 && head[0] == "CREATE" && head[1] == "OR" && head[2] == "REPLACE" {
        head.drain(1..3);
    }
    if at(&head, 0, "CREATE") {
        while at(&head, 1, "TRUSTED") || at(&head, 1, "PROCEDURAL") {
            head.remove(1);
        }
    }
    head
}

fn at(head: &[String], i: usize, kw: &str) -> bool {
    head.get(i).is_some_and(|h| h.as_str() == kw)
}

fn starts_with(head: &[String], pat: &[&str]) -> bool {
    head.len() >= pat.len() && pat.iter().zip(head.iter()).all(|(p, h)| h.as_str() == *p)
}

/// Heads that need more than a fixed prefix: CREATE SEQUENCE with its optional persistence words,
/// CREATE ROLE|USER|GROUP, and the option-list form of ALTER ROLE|USER.
fn route_special(head: &[String]) -> Option<Known> {
    if at(head, 0, "CREATE") {
        let mut i = 1;
        while ["GLOBAL", "LOCAL", "TEMP", "TEMPORARY", "UNLOGGED"].iter().any(|w| at(head, i, w)) {
            i += 1;
        }
        if at(head, i, "SEQUENCE") {
            return Some(Known::CreateSequence);
        }
        if (at(head, 1, "ROLE") || at(head, 1, "GROUP") || at(head, 1, "USER")) && !user_mapping(head) {
            return Some(Known::CreateRole);
        }
    }
    if at(head, 0, "ALTER") && (at(head, 1, "ROLE") || at(head, 1, "USER")) && !user_mapping(head) {
        let other_form = ["RENAME", "SET", "RESET", "IN"].iter().any(|w| at(head, 3, w));
        if head.len() >= 3 && !other_form {
            return Some(Known::AlterRole);
        }
    }
    None
}

/// `{CREATE|ALTER|DROP} USER MAPPING FOR|IF ...` is a foreign-data command, not a role.
fn user_mapping(head: &[String]) -> bool {
    at(head, 1, "USER") && at(head, 2, "MAPPING") && (at(head, 3, "FOR") || at(head, 3, "IF"))
}

fn route(head: &[String]) -> Option<Route> {
    if let Some(k) = route_special(head) {
        return Some(Route::Known(k));
    }
    for (pat, k) in KNOWN_HEADS {
        if starts_with(head, pat) {
            return Some(Route::Known(*k));
        }
    }
    for (pat, feature) in UNSUPPORTED_HEADS {
        if starts_with(head, pat) {
            return Some(Route::Unsupported(*feature));
        }
    }
    None
}

fn unsupported(feature: &str) -> ExtraStatement {
    ExtraStatement::Unsupported { feature: feature.to_string() }
}

/// Result of a sub-grammar that may turn out to be a recognized-but-unsupported form.
enum Parsed<T> {
    Value(T),
    Unsupported(&'static str),
}

// ---------------------------------------------------------------------------------------------
// Cursor and error helpers
// ---------------------------------------------------------------------------------------------

/// PostgreSQL's hint for `RecursionLimitExceeded` (same text as `syntax_error`).
const RECURSION_HINT: &str = "Increase the configuration parameter \"max_stack_depth\" (currently 2048kB), after ensuring the platform's stack depth limit is adequate.";

/// Token cursor over one statement chunk. `tokens` is a copy of the chunk, used to find the token
/// named by a sqlparser error message.
struct Cx<'a> {
    p: Parser<'a>,
    tokens: Vec<TokenWithSpan>,
    src: &'a SourceMap,
}

fn is_word(t: &TokenWithSpan, kw: &str) -> bool {
    match &t.token {
        Token::Word(w) => w.quote_style.is_none() && w.value.eq_ignore_ascii_case(kw),
        _ => false,
    }
}

fn is_eof(t: &TokenWithSpan) -> bool {
    matches!(t.token, Token::EOF)
}

/// Syntax error at a token: `syntax error at or near "<source text>"`, or end-of-input for EOF.
fn fail_at_token(t: &TokenWithSpan, src: &SourceMap) -> PgError {
    if is_eof(t) {
        return diag::syntax_error_eof(src.end_pos());
    }
    let pos = src.pos_of_span(t.span.clone());
    match (src.byte_offset(t.span.start), src.byte_offset(t.span.end)) {
        (Some(s), Some(e)) => diag::syntax_error_near(src.slice(s, e), pos),
        _ => diag::syntax("syntax error", pos),
    }
}

/// 42601 `conflicting or redundant options` at a repeated option (PG `errorConflictingDefElem`).
fn conflicting(cx: &Cx<'_>, t: &TokenWithSpan) -> PgError {
    diag::syntax("conflicting or redundant options", cx.src.pos_of_span(t.span.clone()))
}

impl<'a> Cx<'a> {
    fn peek(&self) -> TokenWithSpan {
        self.p.peek_token()
    }

    fn fail_at(&self, t: &TokenWithSpan) -> PgError {
        fail_at_token(t, self.src)
    }

    fn fail_here(&self) -> PgError {
        self.fail_at(&self.peek())
    }

    /// Consume the next token if it is the unquoted keyword `kw`.
    fn word(&mut self, kw: &str) -> bool {
        if is_word(&self.p.peek_token(), kw) {
            self.p.next_token();
            true
        } else {
            false
        }
    }

    fn expect_word(&mut self, kw: &str) -> Result<()> {
        if self.word(kw) { Ok(()) } else { Err(self.fail_here()) }
    }

    fn token(&mut self, t: &Token) -> bool {
        self.p.consume_token(t)
    }

    fn expect_token(&mut self, t: &Token) -> Result<()> {
        if self.token(t) { Ok(()) } else { Err(self.fail_here()) }
    }

    /// Translate a sqlparser error. The offending token is the one named by the message location
    /// (`Expected: ..., found: X at Line: L, Column: C`); otherwise the cursor's next token.
    fn parser_error(&self, e: ParserError) -> PgError {
        match e {
            ParserError::RecursionLimitExceeded => {
                PgError::new(SqlState::STATEMENT_TOO_COMPLEX, "stack depth limit exceeded").with_hint(RECURSION_HINT)
            }
            ParserError::TokenizerError(msg) | ParserError::ParserError(msg) => {
                if let Some(loc) = location_from_message(&msg) {
                    if let Some(t) = self
                        .tokens
                        .iter()
                        .find(|t| t.span.start.line == loc.line && t.span.start.column == loc.column)
                    {
                        return self.fail_at(t);
                    }
                }
                self.fail_here()
            }
        }
    }

    fn lift<T>(&self, r: std::result::Result<T, ParserError>) -> Result<T> {
        r.map_err(|e| self.parser_error(e))
    }

    fn object_name(&mut self) -> Result<ObjectName> {
        let r = self.p.parse_object_name(false);
        self.lift(r)
    }

    fn ident(&mut self) -> Result<Ident> {
        let r = self.p.parse_identifier();
        self.lift(r)
    }

    fn expr(&mut self) -> Result<Expr> {
        let r = self.p.parse_expr();
        self.lift(r)
    }

    fn data_type(&mut self) -> Result<DataType> {
        let r = self.p.parse_data_type();
        self.lift(r)
    }

    /// Require end of statement (the next token must be EOF).
    fn finish(&self) -> Result<()> {
        if is_eof(&self.peek()) { Ok(()) } else { Err(self.fail_here()) }
    }

    /// `item (, item)*`.
    fn comma_list<T, F>(&mut self, mut f: F) -> Result<Vec<T>>
    where
        F: FnMut(&mut Self) -> Result<T>,
    {
        let mut out = vec![f(self)?];
        while self.token(&Token::Comma) {
            out.push(f(self)?);
        }
        Ok(out)
    }

    /// Consume a standard, escape, unicode or dollar-quoted string constant (PG `Sconst`), returning
    /// its contents. Consumes nothing and returns `None` for any other token.
    fn string_const(&mut self) -> Option<String> {
        let t = self.peek();
        let s = match &t.token {
            Token::SingleQuotedString(s) | Token::EscapedStringLiteral(s) | Token::UnicodeStringLiteral(s) => s.clone(),
            Token::DollarQuotedString(d) => d.value.clone(),
            _ => return None,
        };
        self.p.next_token();
        Some(s)
    }

    /// PG `NumericOnly`: optional sign, then a number literal. The text keeps the sign.
    fn numeric(&mut self) -> Result<String> {
        let sign = if self.token(&Token::Plus) {
            "+"
        } else if self.token(&Token::Minus) {
            "-"
        } else {
            ""
        };
        let t = self.peek();
        match &t.token {
            Token::Number(digits, _) => {
                let text = format!("{sign}{digits}");
                self.p.next_token();
                Ok(text)
            }
            _ => Err(self.fail_at(&t)),
        }
    }

    /// PG `SignedIconst` as an `i32` (ALTER DATABASE CONNECTION LIMIT).
    fn signed_int(&mut self) -> Result<i32> {
        let t = self.peek();
        let text = self.numeric()?;
        let digits_only = text.bytes().all(|b| b.is_ascii_digit() || b == b'-' || b == b'+');
        if !digits_only {
            return Err(self.fail_at(&t));
        }
        text.parse::<i32>().map_err(|_| {
            PgError::new(SqlState::NUMERIC_VALUE_OUT_OF_RANGE, format!("value \"{text}\" is out of range for type integer"))
        })
    }

    /// PG `defGetBoolean` input: TRUE/FALSE/ON/OFF, a string, or 0/1.
    fn bool_value(&mut self, opt: &str) -> Result<bool> {
        let t = self.peek();
        let text = match &t.token {
            Token::Word(w) => {
                let v = w.value.to_ascii_lowercase();
                self.p.next_token();
                v
            }
            Token::Number(digits, _) => {
                let v = digits.clone();
                self.p.next_token();
                v
            }
            _ => match self.string_const() {
                Some(s) => s.to_ascii_lowercase(),
                None => return Err(self.fail_at(&t)),
            },
        };
        match text.as_str() {
            "true" | "t" | "yes" | "y" | "on" | "1" => Ok(true),
            "false" | "f" | "no" | "n" | "off" | "0" => Ok(false),
            _ => Err(PgError::syntax(format!("{opt} requires a Boolean value"), None)),
        }
    }
}

/// `IF EXISTS` (returns whether it was present).
fn parse_if_exists(cx: &mut Cx<'_>) -> Result<bool> {
    if cx.word("IF") {
        cx.expect_word("EXISTS")?;
        Ok(true)
    } else {
        Ok(false)
    }
}

/// Optional `CASCADE` / `RESTRICT` (PG `opt_drop_behavior`); returns true for CASCADE.
fn parse_drop_behavior(cx: &mut Cx<'_>) -> bool {
    if cx.word("CASCADE") {
        true
    } else {
        cx.word("RESTRICT");
        false
    }
}

/// PG `RoleSpec` as used by OWNER TO, DROP OWNED, REASSIGN OWNED: `CURRENT_USER`, `SESSION_USER`,
/// `CURRENT_ROLE` (kept as idents with their upper-case keyword text) or a role name.
fn role_spec(cx: &mut Cx<'_>) -> Result<Ident> {
    const ROLE_KEYWORDS: [&str; 3] = ["CURRENT_USER", "SESSION_USER", "CURRENT_ROLE"];
    let t = cx.peek();
    for kw in ROLE_KEYWORDS.iter() {
        if is_word(&t, kw) {
            cx.p.next_token();
            return Ok(Ident { value: (*kw).to_string(), quote_style: None, span: t.span.clone() });
        }
    }
    cx.ident()
}

/// Case folding for GUC names and values: unquoted words are lower-cased (as PG's lexer does),
/// quoted ones are kept.
fn guc_part(id: &Ident) -> String {
    if id.quote_style.is_none() { id.value.to_ascii_lowercase() } else { id.value.clone() }
}

/// `a.b.c` GUC name (dotted parts joined with `.`).
fn dotted_name(cx: &mut Cx<'_>) -> Result<String> {
    let mut parts = vec![guc_part(&cx.ident()?)];
    while cx.token(&Token::Period) {
        parts.push(guc_part(&cx.ident()?));
    }
    Ok(parts.join("."))
}

/// PG `var_value`: string constant, word (case-folded when unquoted) or number.
fn var_value(cx: &mut Cx<'_>) -> Result<String> {
    if let Some(s) = cx.string_const() {
        return Ok(s);
    }
    let t = cx.peek();
    match &t.token {
        Token::Word(w) => {
            let v = if w.quote_style.is_none() { w.value.to_ascii_lowercase() } else { w.value.clone() };
            cx.p.next_token();
            Ok(v)
        }
        Token::Number(..) | Token::Plus | Token::Minus => cx.numeric(),
        _ => Err(cx.fail_at(&t)),
    }
}

// ---------------------------------------------------------------------------------------------
// Statements
// ---------------------------------------------------------------------------------------------

/// PG `RefreshMatViewStmt`: `REFRESH MATERIALIZED VIEW opt_concurrently qualified_name opt_with_data`
/// where `opt_with_data` is `WITH DATA | WITH NO DATA | empty` (default DATA).
fn parse_refresh(cx: &mut Cx<'_>) -> Result<ExtraStatement> {
    cx.expect_word("REFRESH")?;
    cx.expect_word("MATERIALIZED")?;
    cx.expect_word("VIEW")?;
    let concurrently = cx.word("CONCURRENTLY");
    let name = cx.object_name()?;
    let mut with_data = true;
    if cx.word("WITH") {
        if cx.word("NO") {
            with_data = false;
        }
        cx.expect_word("DATA")?;
    }
    cx.finish()?;
    Ok(ExtraStatement::RefreshMaterializedView { name, concurrently, with_data })
}

/// PG `ReindexStmt`: `REINDEX opt_reindex_option_list reindex_target opt_concurrently name`.
/// Option lists (`(VERBOSE, ...)`) are not representable in the AST, so they are reported as
/// unsupported rather than silently ignored.
fn parse_reindex(cx: &mut Cx<'_>) -> Result<ExtraStatement> {
    cx.expect_word("REINDEX")?;
    if matches!(cx.peek().token, Token::LParen) {
        return Ok(unsupported("REINDEX with options"));
    }
    let kind = if cx.word("INDEX") {
        ReindexKind::Index
    } else if cx.word("TABLE") {
        ReindexKind::Table
    } else if cx.word("SCHEMA") {
        ReindexKind::Schema
    } else if cx.word("DATABASE") {
        ReindexKind::Database
    } else if cx.word("SYSTEM") {
        ReindexKind::System
    } else {
        return Err(cx.fail_here());
    };
    let concurrently = cx.word("CONCURRENTLY");
    let name = match kind {
        ReindexKind::Index | ReindexKind::Table => Some(cx.object_name()?),
        ReindexKind::Schema => {
            let id = cx.ident()?;
            Some(ObjectName::from(vec![id]))
        }
        // `opt_single_name`: the name is optional for DATABASE and SYSTEM.
        ReindexKind::Database | ReindexKind::System => {
            if is_eof(&cx.peek()) {
                None
            } else {
                let id = cx.ident()?;
                Some(ObjectName::from(vec![id]))
            }
        }
    };
    cx.finish()?;
    Ok(ExtraStatement::Reindex { kind, name, concurrently })
}

/// PG `ClusterStmt`: `CLUSTER [VERBOSE] [qualified_name [USING name]]` and the parenthesized
/// option form. VERBOSE and options change the output, which the AST cannot carry, so they are
/// reported as unsupported.
fn parse_cluster(cx: &mut Cx<'_>) -> Result<ExtraStatement> {
    cx.expect_word("CLUSTER")?;
    if matches!(cx.peek().token, Token::LParen) {
        return Ok(unsupported("CLUSTER with options"));
    }
    if cx.word("VERBOSE") {
        return Ok(unsupported("CLUSTER VERBOSE"));
    }
    if is_eof(&cx.peek()) {
        return Ok(ExtraStatement::Cluster { table: None, index: None });
    }
    let table = cx.object_name()?;
    let index = if cx.word("USING") { Some(cx.ident()?) } else { None };
    cx.finish()?;
    Ok(ExtraStatement::Cluster { table: Some(table), index })
}

/// PG `CheckPointStmt`: `CHECKPOINT` with no arguments.
fn parse_checkpoint(cx: &mut Cx<'_>) -> Result<ExtraStatement> {
    cx.expect_word("CHECKPOINT")?;
    cx.finish()?;
    Ok(ExtraStatement::Checkpoint)
}

/// PG `ConstraintsSetStmt`: `SET CONSTRAINTS (ALL | qualified_name_list) (DEFERRED | IMMEDIATE)`.
fn parse_set_constraints(cx: &mut Cx<'_>) -> Result<ExtraStatement> {
    cx.expect_word("SET")?;
    cx.expect_word("CONSTRAINTS")?;
    let names = if cx.word("ALL") { None } else { Some(cx.comma_list(|c| c.object_name())?) };
    let deferred = if cx.word("DEFERRED") {
        true
    } else if cx.word("IMMEDIATE") {
        false
    } else {
        return Err(cx.fail_here());
    };
    cx.finish()?;
    Ok(ExtraStatement::SetConstraints { names, deferred })
}

/// PG `DoStmt`: `DO dostmt_opt_list` where each item is a `Sconst` (the code) or
/// `LANGUAGE NonReservedWord_or_Sconst`, in any order. A repeated item is 42601
/// `conflicting or redundant options`, a missing code is 42601 `no inline code specified`.
fn parse_do(cx: &mut Cx<'_>) -> Result<ExtraStatement> {
    cx.expect_word("DO")?;
    let mut language: Option<Ident> = None;
    let mut code: Option<String> = None;
    let mut items = 0usize;
    loop {
        let t = cx.peek();
        if is_eof(&t) {
            break;
        }
        items += 1;
        if cx.word("LANGUAGE") {
            if language.is_some() {
                return Err(conflicting(cx, &t));
            }
            language = Some(language_name(cx)?);
        } else if let Some(s) = cx.string_const() {
            if code.is_some() {
                return Err(conflicting(cx, &t));
            }
            code = Some(s);
        } else {
            return Err(cx.fail_at(&t));
        }
    }
    if items == 0 {
        return Err(cx.fail_here());
    }
    let code = match code {
        Some(c) => c,
        None => return Err(PgError::syntax("no inline code specified", None)),
    };
    Ok(ExtraStatement::Do { language, code })
}

/// `NonReservedWord_or_Sconst` of a LANGUAGE item. A string language name is marked as quoted so
/// the binder keeps its case as PG does.
fn language_name(cx: &mut Cx<'_>) -> Result<Ident> {
    let t = cx.peek();
    if let Some(s) = cx.string_const() {
        return Ok(Ident { value: s, quote_style: Some('\''), span: t.span.clone() });
    }
    cx.ident()
}

/// PG `DropOwnedStmt`: `DROP OWNED BY role_list opt_drop_behavior`.
fn parse_drop_owned(cx: &mut Cx<'_>) -> Result<ExtraStatement> {
    cx.expect_word("DROP")?;
    cx.expect_word("OWNED")?;
    cx.expect_word("BY")?;
    let roles = cx.comma_list(role_spec)?;
    let cascade = parse_drop_behavior(cx);
    cx.finish()?;
    Ok(ExtraStatement::DropOwned { roles, cascade })
}

/// PG `ReassignOwnedStmt`: `REASSIGN OWNED BY role_list TO RoleSpec`.
fn parse_reassign_owned(cx: &mut Cx<'_>) -> Result<ExtraStatement> {
    cx.expect_word("REASSIGN")?;
    cx.expect_word("OWNED")?;
    cx.expect_word("BY")?;
    let old_roles = cx.comma_list(role_spec)?;
    cx.expect_word("TO")?;
    let new_role = role_spec(cx)?;
    cx.finish()?;
    Ok(ExtraStatement::ReassignOwned { old_roles, new_role })
}

/// PG `AlterSeqStmt` (`ALTER SEQUENCE [IF EXISTS] qualified_name SeqOptList`) plus the sequence
/// forms of `AlterTableStmt` (RENAME TO, OWNER TO, SET SCHEMA, SET LOGGED/UNLOGGED).
fn parse_alter_sequence(cx: &mut Cx<'_>) -> Result<ExtraStatement> {
    cx.expect_word("ALTER")?;
    cx.expect_word("SEQUENCE")?;
    let if_exists = parse_if_exists(cx)?;
    let name = cx.object_name()?;
    let action = if cx.word("RENAME") {
        cx.expect_word("TO")?;
        AlterSequenceAction::RenameTo(cx.ident()?)
    } else if cx.word("OWNER") {
        cx.expect_word("TO")?;
        AlterSequenceAction::OwnerTo(role_spec(cx)?)
    } else if cx.word("SET") {
        if cx.word("SCHEMA") {
            AlterSequenceAction::SetSchema(cx.ident()?)
        } else if cx.word("LOGGED") {
            AlterSequenceAction::SetLogged(true)
        } else if cx.word("UNLOGGED") {
            AlterSequenceAction::SetLogged(false)
        } else {
            return Err(cx.fail_here());
        }
    } else {
        match seq_options(cx, false)? {
            Parsed::Value(opts) => {
                if opts.is_empty() {
                    return Err(cx.fail_here());
                }
                AlterSequenceAction::Options(opts)
            }
            Parsed::Unsupported(feature) => return Ok(unsupported(feature)),
        }
    };
    cx.finish()?;
    Ok(ExtraStatement::AlterSequence { if_exists, name, action })
}

/// PG `CreateSeqStmt`: `CREATE OptTemp SEQUENCE [IF NOT EXISTS] qualified_name OptSeqOptList`. The
/// options come in any order (`SeqOptElem`); repeats are a binder error (42601).
fn parse_create_sequence(cx: &mut Cx<'_>) -> Result<ExtraStatement> {
    cx.expect_word("CREATE")?;
    let (mut temporary, mut unlogged) = (false, false);
    loop {
        if cx.word("GLOBAL") || cx.word("LOCAL") {
            continue;
        } else if cx.word("TEMP") || cx.word("TEMPORARY") {
            temporary = true;
        } else if cx.word("UNLOGGED") {
            unlogged = true;
        } else {
            break;
        }
    }
    cx.expect_word("SEQUENCE")?;
    let if_not_exists = if cx.word("IF") {
        cx.expect_word("NOT")?;
        cx.expect_word("EXISTS")?;
        true
    } else {
        false
    };
    let name = cx.object_name()?;
    let options = match seq_options(cx, true)? {
        Parsed::Value(o) => o,
        Parsed::Unsupported(feature) => return Ok(unsupported(feature)),
    };
    cx.finish()?;
    Ok(ExtraStatement::CreateSequence { temporary, unlogged, if_not_exists, name, options })
}

/// PG `CreateRoleStmt` / `CreateUserStmt` / `CreateGroupStmt`:
/// `CREATE {ROLE|USER|GROUP} RoleId opt_with OptRoleList`.
fn parse_create_role(cx: &mut Cx<'_>) -> Result<ExtraStatement> {
    cx.expect_word("CREATE")?;
    let kind = if cx.word("ROLE") {
        RoleStmtKind::Role
    } else if cx.word("USER") {
        RoleStmtKind::User
    } else {
        cx.expect_word("GROUP")?;
        RoleStmtKind::Group
    };
    let name = cx.ident()?;
    cx.word("WITH");
    let options = role_options(cx, true)?;
    cx.finish()?;
    Ok(ExtraStatement::CreateRole { kind, name, options })
}

/// PG `AlterRoleStmt`: `ALTER {ROLE|USER} RoleSpec opt_with AlterOptRoleList`.
fn parse_alter_role(cx: &mut Cx<'_>) -> Result<ExtraStatement> {
    cx.expect_word("ALTER")?;
    if !cx.word("ROLE") {
        cx.expect_word("USER")?;
    }
    let name = role_spec(cx)?;
    cx.word("WITH");
    let options = role_options(cx, false)?;
    cx.finish()?;
    Ok(ExtraStatement::AlterRole { name, options })
}

/// PG `SignedIconst` as `i32`: optional sign, then an integer token that fits `int` (a larger one
/// is lexed as a numeric constant, hence a syntax error at the number).
fn signed_iconst(cx: &mut Cx<'_>) -> Result<i32> {
    let neg = if cx.token(&Token::Minus) {
        true
    } else {
        cx.token(&Token::Plus);
        false
    };
    let t = cx.peek();
    match &t.token {
        Token::Number(digits, _) if digits.bytes().all(|b| b.is_ascii_digit()) => match digits.parse::<i32>() {
            Ok(v) => {
                cx.p.next_token();
                Ok(if neg { -v } else { v })
            }
            Err(_) => Err(cx.fail_at(&t)),
        },
        _ => Err(cx.fail_at(&t)),
    }
}

/// `role_list`.
fn role_list(cx: &mut Cx<'_>) -> Result<Vec<Ident>> {
    cx.comma_list(role_spec)
}

/// PG `OptRoleList` (`create = true`, `CreateOptRoleElem`) or `AlterOptRoleList` (`AlterOptRoleElem`):
/// options in any order up to the end of the statement.
fn role_options(cx: &mut Cx<'_>, create: bool) -> Result<Vec<RoleOptionItem>> {
    let mut out = Vec::new();
    loop {
        let t = cx.peek();
        if is_eof(&t) {
            break;
        }
        let pos = cx.src.pos_of_span(t.span.clone());
        let option = if cx.word("PASSWORD") {
            if cx.word("NULL") {
                RoleOptionAst::Password(None)
            } else {
                match cx.string_const() {
                    Some(s) => RoleOptionAst::Password(Some(s)),
                    None => return Err(cx.fail_here()),
                }
            }
        } else if cx.word("ENCRYPTED") {
            cx.expect_word("PASSWORD")?;
            match cx.string_const() {
                Some(s) => RoleOptionAst::Password(Some(s)),
                None => return Err(cx.fail_here()),
            }
        } else if cx.word("UNENCRYPTED") {
            cx.expect_word("PASSWORD")?;
            if cx.string_const().is_none() {
                return Err(cx.fail_here());
            }
            let e = PgError::new(SqlState::FEATURE_NOT_SUPPORTED, "UNENCRYPTED PASSWORD is no longer supported")
                .with_hint("Remove UNENCRYPTED to store the password in encrypted form instead.");
            return Err(diag::with_position(e, pos));
        } else if cx.word("INHERIT") {
            RoleOptionAst::Inherit(true)
        } else if cx.word("CONNECTION") {
            cx.expect_word("LIMIT")?;
            RoleOptionAst::ConnectionLimit(signed_iconst(cx)?)
        } else if cx.word("VALID") {
            cx.expect_word("UNTIL")?;
            match cx.string_const() {
                Some(s) => RoleOptionAst::ValidUntil(s),
                None => return Err(cx.fail_here()),
            }
        } else if cx.word("USER") {
            RoleOptionAst::RoleMembers(role_list(cx)?)
        } else if create && cx.word("SYSID") {
            let n = cx.peek();
            match &n.token {
                Token::Number(d, _) if d.bytes().all(|b| b.is_ascii_digit()) && d.parse::<i32>().is_ok() => {
                    cx.p.next_token();
                    RoleOptionAst::Sysid
                }
                _ => return Err(cx.fail_at(&n)),
            }
        } else if create && cx.word("ADMIN") {
            RoleOptionAst::AdminMembers(role_list(cx)?)
        } else if create && cx.word("ROLE") {
            RoleOptionAst::RoleMembers(role_list(cx)?)
        } else if create && cx.word("IN") {
            if !(cx.word("ROLE") || cx.word("GROUP")) {
                return Err(cx.fail_here());
            }
            RoleOptionAst::AddRoleTo(role_list(cx)?)
        } else if let Token::Word(w) = &t.token {
            // The IDENT production: a plain identifier naming a boolean attribute.
            let word = w.value.to_ascii_lowercase();
            let (which, on) = match word.as_str() {
                "superuser" => (0, true),
                "nosuperuser" => (0, false),
                "createrole" => (1, true),
                "nocreaterole" => (1, false),
                "replication" => (2, true),
                "noreplication" => (2, false),
                "createdb" => (3, true),
                "nocreatedb" => (3, false),
                "login" => (4, true),
                "nologin" => (4, false),
                "bypassrls" => (5, true),
                "nobypassrls" => (5, false),
                "noinherit" => (6, false),
                _ => {
                    // Any identifier that is not a keyword reaches the IDENT action and is reported
                    // there; a keyword in this position is an ordinary syntax error.
                    // SYSID is a PostgreSQL keyword that sqlparser does not list.
                    let pg_keyword = word == "sysid";
                    if w.quote_style.is_some() || (w.keyword == Keyword::NoKeyword && !pg_keyword) {
                        let e = PgError::syntax(format!("unrecognized role option \"{}\"", w.value), None);
                        return Err(diag::with_position(e, pos));
                    }
                    return Err(cx.fail_at(&t));
                }
            };
            cx.p.next_token();
            match which {
                0 => RoleOptionAst::Superuser(on),
                1 => RoleOptionAst::CreateRole(on),
                2 => RoleOptionAst::Replication(on),
                3 => RoleOptionAst::CreateDb(on),
                4 => RoleOptionAst::Login(on),
                5 => RoleOptionAst::BypassRls(on),
                _ => RoleOptionAst::Inherit(on),
            }
        } else {
            return Err(cx.fail_at(&t));
        };
        out.push(RoleOptionItem { option, pos });
    }
    Ok(out)
}

/// Is the next token the start of a `NumericOnly`?
fn starts_numeric(cx: &Cx<'_>) -> bool {
    matches!(cx.peek().token, Token::Number(..) | Token::Plus | Token::Minus)
}

/// PG `SeqOptList` items, in written order, until a token that is not a sequence option (the
/// caller's `finish` reports it). `LOGGED`, `UNLOGGED` and `SEQUENCE NAME` are grammar items PG
/// accepts in this list but PGArbor does not implement.
fn seq_options(cx: &mut Cx<'_>, for_create: bool) -> Result<Parsed<Vec<SequenceOptionAst>>> {
    let mut out = Vec::new();
    loop {
        if is_eof(&cx.peek()) {
            break;
        }
        if cx.word("AS") {
            out.push(SequenceOptionAst::As(cx.data_type()?));
        } else if cx.word("CACHE") {
            out.push(SequenceOptionAst::Cache(cx.numeric()?));
        } else if cx.word("CYCLE") {
            out.push(SequenceOptionAst::Cycle(true));
        } else if cx.word("NO") {
            if cx.word("CYCLE") {
                out.push(SequenceOptionAst::Cycle(false));
            } else if cx.word("MAXVALUE") {
                out.push(SequenceOptionAst::MaxValue(None));
            } else if cx.word("MINVALUE") {
                out.push(SequenceOptionAst::MinValue(None));
            } else {
                return Err(cx.fail_here());
            }
        } else if cx.word("INCREMENT") {
            cx.word("BY");
            out.push(SequenceOptionAst::IncrementBy(cx.numeric()?));
        } else if cx.word("MAXVALUE") {
            out.push(SequenceOptionAst::MaxValue(Some(cx.numeric()?)));
        } else if cx.word("MINVALUE") {
            out.push(SequenceOptionAst::MinValue(Some(cx.numeric()?)));
        } else if cx.word("OWNED") {
            cx.expect_word("BY")?;
            // `OWNED BY NONE` (a single unqualified word NONE, sequence.c) detaches the sequence.
            let none = is_word(&cx.peek(), "NONE") && !matches!(cx.p.peek_nth_token(1).token, Token::Period);
            if none {
                cx.p.next_token();
                out.push(SequenceOptionAst::OwnedBy(None));
            } else {
                out.push(SequenceOptionAst::OwnedBy(Some(cx.object_name()?)));
            }
        } else if cx.word("START") {
            cx.word("WITH");
            out.push(SequenceOptionAst::StartWith(cx.numeric()?));
        } else if cx.word("RESTART") {
            if cx.word("WITH") {
                out.push(SequenceOptionAst::Restart(Some(cx.numeric()?)));
            } else if starts_numeric(cx) {
                out.push(SequenceOptionAst::Restart(Some(cx.numeric()?)));
            } else {
                out.push(SequenceOptionAst::Restart(None));
            }
        } else if cx.word("LOGGED") {
            if !for_create {
                return Ok(Parsed::Unsupported("ALTER SEQUENCE ... LOGGED"));
            }
            out.push(SequenceOptionAst::Logged(true));
        } else if cx.word("UNLOGGED") {
            if !for_create {
                return Ok(Parsed::Unsupported("ALTER SEQUENCE ... UNLOGGED"));
            }
            out.push(SequenceOptionAst::Logged(false));
        } else if cx.word("SEQUENCE") {
            if !for_create {
                return Ok(Parsed::Unsupported("ALTER SEQUENCE ... SEQUENCE NAME"));
            }
            cx.expect_word("NAME")?;
            cx.object_name()?;
            out.push(SequenceOptionAst::SequenceName);
        } else {
            break;
        }
    }
    Ok(Parsed::Value(out))
}

/// PG `AlterDomainStmt`: SET/DROP DEFAULT, SET/DROP NOT NULL, ADD [CONSTRAINT n] CHECK (expr)
/// [NOT VALID], DROP CONSTRAINT [IF EXISTS], RENAME CONSTRAINT, VALIDATE CONSTRAINT, RENAME TO,
/// OWNER TO, SET SCHEMA.
fn parse_alter_domain(cx: &mut Cx<'_>) -> Result<ExtraStatement> {
    cx.expect_word("ALTER")?;
    cx.expect_word("DOMAIN")?;
    let name = cx.object_name()?;
    let action = if cx.word("SET") {
        if cx.word("DEFAULT") {
            AlterDomainAction::SetDefault(cx.expr()?)
        } else if cx.word("NOT") {
            cx.expect_word("NULL")?;
            AlterDomainAction::SetNotNull
        } else if cx.word("SCHEMA") {
            AlterDomainAction::SetSchema(cx.ident()?)
        } else {
            return Err(cx.fail_here());
        }
    } else if cx.word("DROP") {
        if cx.word("DEFAULT") {
            AlterDomainAction::DropDefault
        } else if cx.word("NOT") {
            cx.expect_word("NULL")?;
            AlterDomainAction::DropNotNull
        } else if cx.word("CONSTRAINT") {
            let if_exists = parse_if_exists(cx)?;
            let cname = cx.ident()?;
            let cascade = parse_drop_behavior(cx);
            AlterDomainAction::DropConstraint { name: cname, if_exists, cascade }
        } else {
            return Err(cx.fail_here());
        }
    } else if cx.word("ADD") {
        let cname = if cx.word("CONSTRAINT") { Some(cx.ident()?) } else { None };
        if !cx.word("CHECK") {
            // Other domain constraint forms exist in the grammar; PGArbor implements CHECK only.
            if cname.is_some() {
                return Ok(unsupported("ALTER DOMAIN ... ADD constraint other than CHECK"));
            }
            return Err(cx.fail_here());
        }
        cx.expect_token(&Token::LParen)?;
        let expr = cx.expr()?;
        cx.expect_token(&Token::RParen)?;
        let not_valid = if cx.word("NOT") {
            cx.expect_word("VALID")?;
            true
        } else {
            false
        };
        AlterDomainAction::AddCheck { name: cname, expr, not_valid }
    } else if cx.word("RENAME") {
        if cx.word("CONSTRAINT") {
            let old = cx.ident()?;
            cx.expect_word("TO")?;
            let new = cx.ident()?;
            AlterDomainAction::RenameConstraint { old, new }
        } else {
            cx.expect_word("TO")?;
            AlterDomainAction::RenameTo(cx.ident()?)
        }
    } else if cx.word("VALIDATE") {
        cx.expect_word("CONSTRAINT")?;
        AlterDomainAction::ValidateConstraint(cx.ident()?)
    } else if cx.word("OWNER") {
        cx.expect_word("TO")?;
        AlterDomainAction::OwnerTo(role_spec(cx)?)
    } else {
        return Err(cx.fail_here());
    };
    cx.finish()?;
    Ok(ExtraStatement::AlterDomain { name, action })
}

/// Session settings PG accepts after ALTER DATABASE SET/RESET that PGArbor does not implement.
const DATABASE_SESSION_WORDS: &[(&str, &str)] = &[
    ("SCHEMA", "ALTER DATABASE SET SCHEMA"),
    ("NAMES", "ALTER DATABASE SET NAMES"),
    ("ROLE", "ALTER DATABASE SET ROLE"),
    ("SESSION", "ALTER DATABASE SET SESSION"),
    ("TRANSACTION", "ALTER DATABASE SET TRANSACTION"),
    ("XML", "ALTER DATABASE SET XML OPTION"),
    ("CATALOG", "ALTER DATABASE SET CATALOG"),
];

/// Unsupported session-setting word after SET or RESET in ALTER DATABASE, if the next token is one.
fn database_session_word(cx: &Cx<'_>) -> Option<&'static str> {
    let t = cx.peek();
    for (word, feature) in DATABASE_SESSION_WORDS.iter() {
        if is_word(&t, word) {
            return Some(*feature);
        }
    }
    None
}

/// PG `AlterDatabaseStmt` (RENAME TO is `RenameStmt`, OWNER TO is `AlterOwnerStmt`), the
/// `alterdb_opt_item` list, and `AlterDatabaseSetStmt` (SET / RESET).
fn parse_alter_database(cx: &mut Cx<'_>) -> Result<ExtraStatement> {
    cx.expect_word("ALTER")?;
    cx.expect_word("DATABASE")?;
    let name = cx.ident()?;
    let action = if cx.word("RENAME") {
        cx.expect_word("TO")?;
        AlterDatabaseAction::RenameTo(cx.ident()?)
    } else if cx.word("OWNER") {
        cx.expect_word("TO")?;
        AlterDatabaseAction::OwnerTo(role_spec(cx)?)
    } else if cx.word("SET") {
        match alterdb_set(cx)? {
            Parsed::Value(a) => a,
            Parsed::Unsupported(feature) => return Ok(unsupported(feature)),
        }
    } else if cx.word("RESET") {
        if let Some(feature) = database_session_word(cx) {
            return Ok(unsupported(feature));
        }
        let guc = if cx.word("ALL") {
            "all".to_string()
        } else if cx.word("TIME") {
            cx.expect_word("ZONE")?;
            "timezone".to_string()
        } else {
            dotted_name(cx)?
        };
        AlterDatabaseAction::Set { name: guc, value: None, reset: true }
    } else if cx.word("REFRESH") {
        return Ok(unsupported("ALTER DATABASE REFRESH COLLATION VERSION"));
    } else {
        match alterdb_options(cx)? {
            Parsed::Value(a) => a,
            Parsed::Unsupported(feature) => return Ok(unsupported(feature)),
        }
    };
    cx.finish()?;
    Ok(ExtraStatement::AlterDatabase { name, action })
}

/// `SET` part of ALTER DATABASE: `TIME ZONE value`, `name {TO|=} DEFAULT`, `name {TO|=} values`,
/// `name FROM CURRENT`.
fn alterdb_set(cx: &mut Cx<'_>) -> Result<Parsed<AlterDatabaseAction>> {
    if cx.word("TABLESPACE") {
        return Ok(Parsed::Unsupported("ALTER DATABASE SET TABLESPACE"));
    }
    if let Some(feature) = database_session_word(cx) {
        return Ok(Parsed::Unsupported(feature));
    }
    let time_zone = cx.word("TIME");
    let guc = if time_zone {
        cx.expect_word("ZONE")?;
        "timezone".to_string()
    } else {
        dotted_name(cx)?
    };
    if !time_zone {
        if cx.word("FROM") {
            cx.expect_word("CURRENT")?;
            return Ok(Parsed::Value(AlterDatabaseAction::Set { name: guc, value: None, reset: false }));
        }
        if !(cx.word("TO") || cx.token(&Token::Eq)) {
            return Err(cx.fail_here());
        }
    }
    if cx.word("DEFAULT") {
        return Ok(Parsed::Value(AlterDatabaseAction::Set { name: guc, value: None, reset: true }));
    }
    let values = if time_zone { vec![var_value(cx)?] } else { cx.comma_list(var_value)? };
    Ok(Parsed::Value(AlterDatabaseAction::Set { name: guc, value: Some(values), reset: false }))
}

/// `[WITH] alterdb_opt_item+`: `CONNECTION LIMIT n`, `ALLOW_CONNECTIONS b`, `IS_TEMPLATE b`, each at
/// most once, in any order.
fn alterdb_options(cx: &mut Cx<'_>) -> Result<Parsed<AlterDatabaseAction>> {
    cx.word("WITH");
    let mut allow_connections: Option<bool> = None;
    let mut connection_limit: Option<i32> = None;
    let mut is_template: Option<bool> = None;
    let mut count = 0usize;
    loop {
        let t = cx.peek();
        if cx.word("CONNECTION") {
            cx.expect_word("LIMIT")?;
            if connection_limit.is_some() {
                return Err(conflicting(cx, &t));
            }
            cx.token(&Token::Eq);
            if is_word(&cx.peek(), "DEFAULT") {
                return Err(cx.fail_here());
            }
            connection_limit = Some(cx.signed_int()?);
        } else if cx.word("ALLOW_CONNECTIONS") {
            if allow_connections.is_some() {
                return Err(conflicting(cx, &t));
            }
            cx.token(&Token::Eq);
            allow_connections = Some(cx.bool_value("allow_connections")?);
        } else if cx.word("IS_TEMPLATE") {
            if is_template.is_some() {
                return Err(conflicting(cx, &t));
            }
            cx.token(&Token::Eq);
            is_template = Some(cx.bool_value("is_template")?);
        } else {
            break;
        }
        count += 1;
    }
    if count == 0 {
        return Err(cx.fail_here());
    }
    Ok(Parsed::Value(AlterDatabaseAction::Options { allow_connections, connection_limit, is_template }))
}

/// `ALTER MATERIALIZED VIEW [IF EXISTS] name`: RENAME [COLUMN] / RENAME TO / SET SCHEMA / OWNER TO.
/// Other actions are reported as unsupported.
fn parse_alter_matview(cx: &mut Cx<'_>) -> Result<ExtraStatement> {
    cx.expect_word("ALTER")?;
    cx.expect_word("MATERIALIZED")?;
    cx.expect_word("VIEW")?;
    let if_exists = parse_if_exists(cx)?;
    let name = cx.object_name()?;
    let action = if cx.word("RENAME") {
        if cx.word("TO") {
            AlterMatViewAction::RenameTo(cx.ident()?)
        } else {
            cx.word("COLUMN");
            let old = cx.ident()?;
            cx.expect_word("TO")?;
            let new = cx.ident()?;
            AlterMatViewAction::RenameColumn { old, new }
        }
    } else if cx.word("SET") {
        if cx.word("SCHEMA") {
            AlterMatViewAction::SetSchema(cx.ident()?)
        } else {
            return Ok(unsupported("ALTER MATERIALIZED VIEW ... SET"));
        }
    } else if cx.word("OWNER") {
        cx.expect_word("TO")?;
        AlterMatViewAction::OwnerTo(role_spec(cx)?)
    } else {
        return Ok(unsupported("ALTER MATERIALIZED VIEW"));
    };
    cx.finish()?;
    Ok(ExtraStatement::AlterMaterializedView { if_exists, name, action })
}

// ---------------------------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use sqlparser::tokenizer::{Location, Span, Tokenizer};

    /// Tokenize a chunk and terminate it with EOF, as `parser::parse` does.
    fn lex(sql: &str) -> Vec<TokenWithSpan> {
        let dialect = PostgreSqlDialect {};
        let mut toks = Tokenizer::new(&dialect, sql).tokenize_with_location().expect("tokenize");
        let end = toks.last().map(|t| t.span.end).unwrap_or(Location { line: 1, column: 1 });
        toks.push(TokenWithSpan::new(Token::EOF, Span::new(end, end)));
        toks
    }

    fn run(sql: &str) -> Result<ExtraStatement> {
        let src = SourceMap::new(sql);
        parse_extra(lex(sql), &src)
    }

    fn ok(sql: &str) -> ExtraStatement {
        match run(sql) {
            Ok(s) => s,
            Err(e) => panic!("{sql}: unexpected error {}", e.message),
        }
    }

    fn err(sql: &str) -> PgError {
        match run(sql) {
            Ok(s) => panic!("{sql}: expected an error, got {s:?}"),
            Err(e) => e,
        }
    }

    fn name(parts: &[&str]) -> ObjectName {
        ObjectName::from(parts.iter().map(|p| Ident::new(*p)).collect::<Vec<_>>())
    }

    fn ident(s: &str) -> Ident {
        Ident::new(s)
    }

    fn is_extra(sql: &str) -> bool {
        is_extra_statement(&lex(sql)).expect("is_extra_statement")
    }

    #[test]
    fn detection_table() {
        assert!(is_extra("refresh materialized view v"));
        assert!(is_extra("  -- note\n REINDEX TABLE t"));
        assert!(is_extra("CREATE OR REPLACE RULE r AS ON INSERT TO t DO INSTEAD NOTHING"));
        assert!(is_extra("create trusted procedural language x"));
        assert!(is_extra("DROP FOREIGN DATA WRAPPER w"));
        assert!(is_extra("ALTER DEFAULT PRIVILEGES GRANT ALL ON TABLES TO r"));
        assert!(is_extra("COMMIT PREPARED 'x'"));
        assert!(!is_extra("refresh view v"));
        assert!(!is_extra("select 1"));
        assert!(!is_extra("create table t (a int)"));
        assert!(!is_extra("alter table t add column a int"));
        assert!(!is_extra("drop table t"));
        assert!(!is_extra("commit"));
        assert!(!is_extra("create extension hstore"));
        assert!(!is_extra("set search_path to a"));
    }

    #[test]
    fn refresh_matview_forms() {
        assert_eq!(
            ok("REFRESH MATERIALIZED VIEW CONCURRENTLY s.v WITH NO DATA"),
            ExtraStatement::RefreshMaterializedView { name: name(&["s", "v"]), concurrently: true, with_data: false }
        );
        assert_eq!(
            ok("refresh materialized view v"),
            ExtraStatement::RefreshMaterializedView { name: name(&["v"]), concurrently: false, with_data: true }
        );
        assert_eq!(
            ok("REFRESH MATERIALIZED VIEW v WITH DATA"),
            ExtraStatement::RefreshMaterializedView { name: name(&["v"]), concurrently: false, with_data: true }
        );
        // "REFRESH MATERIALIZED VIEW v WITH NO" is 35 characters; end of input is position 36.
        let e = err("REFRESH MATERIALIZED VIEW v WITH NO");
        assert_eq!(e.message, "syntax error at end of input");
        assert_eq!(e.position, Some(36));
    }

    #[test]
    fn reindex_forms() {
        assert_eq!(
            ok("REINDEX DATABASE CONCURRENTLY"),
            ExtraStatement::Reindex { kind: ReindexKind::Database, name: None, concurrently: true }
        );
        assert_eq!(
            ok("REINDEX TABLE s.t"),
            ExtraStatement::Reindex { kind: ReindexKind::Table, name: Some(name(&["s", "t"])), concurrently: false }
        );
        assert_eq!(
            ok("REINDEX SCHEMA x"),
            ExtraStatement::Reindex { kind: ReindexKind::Schema, name: Some(name(&["x"])), concurrently: false }
        );
        assert_eq!(
            ok("REINDEX SYSTEM"),
            ExtraStatement::Reindex { kind: ReindexKind::System, name: None, concurrently: false }
        );
        let e = err("REINDEX SCHEMA");
        assert_eq!(e.message, "syntax error at end of input");
        assert_eq!(e.position, Some(15));
        let e = err("REINDEX TABLE t extra");
        assert_eq!(e.message, "syntax error at or near \"extra\"");
        assert_eq!(e.position, Some(17));
    }

    #[test]
    fn cluster_forms() {
        assert_eq!(ok("CLUSTER"), ExtraStatement::Cluster { table: None, index: None });
        assert_eq!(
            ok("CLUSTER t USING i"),
            ExtraStatement::Cluster { table: Some(name(&["t"])), index: Some(ident("i")) }
        );
        assert_eq!(ok("CLUSTER t"), ExtraStatement::Cluster { table: Some(name(&["t"])), index: None });
        assert_eq!(ok("CLUSTER VERBOSE"), ExtraStatement::Unsupported { feature: "CLUSTER VERBOSE".into() });
    }

    #[test]
    fn checkpoint_and_trailing_tokens() {
        assert_eq!(ok("CHECKPOINT"), ExtraStatement::Checkpoint);
        let e = err("CHECKPOINT x");
        assert_eq!(e.message, "syntax error at or near \"x\"");
        assert_eq!(e.position, Some(12));
        assert_eq!(e.sqlstate, SqlState::SYNTAX_ERROR);
    }

    #[test]
    fn set_constraints_forms() {
        assert_eq!(
            ok("SET CONSTRAINTS ALL DEFERRED"),
            ExtraStatement::SetConstraints { names: None, deferred: true }
        );
        assert_eq!(
            ok("SET CONSTRAINTS a.b, c IMMEDIATE"),
            ExtraStatement::SetConstraints { names: Some(vec![name(&["a", "b"]), name(&["c"])]), deferred: false }
        );
        let e = err("SET CONSTRAINTS a");
        assert_eq!(e.message, "syntax error at end of input");
        assert_eq!(e.position, Some(18));
    }

    #[test]
    fn do_forms() {
        assert_eq!(
            ok("DO $$ begin end $$ LANGUAGE plpgsql"),
            ExtraStatement::Do { language: Some(ident("plpgsql")), code: " begin end ".into() }
        );
        assert_eq!(
            ok("DO LANGUAGE plpgsql 'x'"),
            ExtraStatement::Do { language: Some(ident("plpgsql")), code: "x".into() }
        );
        assert_eq!(ok("DO 'x'"), ExtraStatement::Do { language: None, code: "x".into() });
        let e = err("DO LANGUAGE plpgsql");
        assert_eq!(e.message, "no inline code specified");
        assert_eq!(e.position, None);
        let e = err("DO 'a' 'b'");
        assert_eq!(e.message, "conflicting or redundant options");
        assert_eq!(e.position, Some(8));
        let e = err("DO");
        assert_eq!(e.message, "syntax error at end of input");
        assert_eq!(e.position, Some(3));
    }

    #[test]
    fn drop_and_reassign_owned() {
        assert_eq!(
            ok("DROP OWNED BY a, CURRENT_USER CASCADE"),
            ExtraStatement::DropOwned { roles: vec![ident("a"), ident("CURRENT_USER")], cascade: true }
        );
        assert_eq!(
            ok("DROP OWNED BY a"),
            ExtraStatement::DropOwned { roles: vec![ident("a")], cascade: false }
        );
        assert_eq!(
            ok("REASSIGN OWNED BY a TO b"),
            ExtraStatement::ReassignOwned { old_roles: vec![ident("a")], new_role: ident("b") }
        );
    }

    #[test]
    fn alter_sequence_options_in_order() {
        assert_eq!(
            ok("ALTER SEQUENCE IF EXISTS s RESTART WITH 5 OWNED BY t.c"),
            ExtraStatement::AlterSequence {
                if_exists: true,
                name: name(&["s"]),
                action: AlterSequenceAction::Options(vec![
                    SequenceOptionAst::Restart(Some("5".into())),
                    SequenceOptionAst::OwnedBy(Some(name(&["t", "c"]))),
                ]),
            }
        );
        assert_eq!(
            ok("ALTER SEQUENCE s INCREMENT BY -5 MINVALUE - 100 NO MAXVALUE NO CYCLE CACHE 3"),
            ExtraStatement::AlterSequence {
                if_exists: false,
                name: name(&["s"]),
                action: AlterSequenceAction::Options(vec![
                    SequenceOptionAst::IncrementBy("-5".into()),
                    SequenceOptionAst::MinValue(Some("-100".into())),
                    SequenceOptionAst::MaxValue(None),
                    SequenceOptionAst::Cycle(false),
                    SequenceOptionAst::Cache("3".into()),
                ]),
            }
        );
        assert_eq!(
            ok("ALTER SEQUENCE s START 7 RESTART"),
            ExtraStatement::AlterSequence {
                if_exists: false,
                name: name(&["s"]),
                action: AlterSequenceAction::Options(vec![
                    SequenceOptionAst::StartWith("7".into()),
                    SequenceOptionAst::Restart(None),
                ]),
            }
        );
        assert_eq!(
            ok("ALTER SEQUENCE s OWNED BY NONE"),
            ExtraStatement::AlterSequence {
                if_exists: false,
                name: name(&["s"]),
                action: AlterSequenceAction::Options(vec![SequenceOptionAst::OwnedBy(None)]),
            }
        );
        assert_eq!(
            ok("ALTER SEQUENCE s SET UNLOGGED"),
            ExtraStatement::AlterSequence {
                if_exists: false,
                name: name(&["s"]),
                action: AlterSequenceAction::SetLogged(false),
            }
        );
        assert_eq!(
            ok("ALTER SEQUENCE s RENAME TO t"),
            ExtraStatement::AlterSequence {
                if_exists: false,
                name: name(&["s"]),
                action: AlterSequenceAction::RenameTo(ident("t")),
            }
        );
        // "ALTER SEQUENCE s" is 16 characters; end of input is position 17.
        let e = err("ALTER SEQUENCE s");
        assert_eq!(e.message, "syntax error at end of input");
        assert_eq!(e.position, Some(17));
    }

    #[test]
    fn alter_domain_forms() {
        match ok("ALTER DOMAIN d ADD CONSTRAINT c CHECK (VALUE > 0) NOT VALID") {
            ExtraStatement::AlterDomain {
                name: n,
                action: AlterDomainAction::AddCheck { name: Some(c), expr: Expr::BinaryOp { .. }, not_valid: true },
            } => {
                assert_eq!(n, name(&["d"]));
                assert_eq!(c, ident("c"));
            }
            other => panic!("unexpected {other:?}"),
        }
        assert_eq!(
            ok("ALTER DOMAIN d DROP CONSTRAINT IF EXISTS c CASCADE"),
            ExtraStatement::AlterDomain {
                name: name(&["d"]),
                action: AlterDomainAction::DropConstraint { name: ident("c"), if_exists: true, cascade: true },
            }
        );
        assert_eq!(
            ok("ALTER DOMAIN d SET NOT NULL"),
            ExtraStatement::AlterDomain { name: name(&["d"]), action: AlterDomainAction::SetNotNull }
        );
        assert_eq!(
            ok("ALTER DOMAIN d RENAME CONSTRAINT a TO b"),
            ExtraStatement::AlterDomain {
                name: name(&["d"]),
                action: AlterDomainAction::RenameConstraint { old: ident("a"), new: ident("b") },
            }
        );
    }

    #[test]
    fn alter_database_forms() {
        assert_eq!(
            ok("ALTER DATABASE d WITH CONNECTION LIMIT -1 IS_TEMPLATE true"),
            ExtraStatement::AlterDatabase {
                name: ident("d"),
                action: AlterDatabaseAction::Options {
                    allow_connections: None,
                    connection_limit: Some(-1),
                    is_template: Some(true),
                },
            }
        );
        assert_eq!(
            ok("ALTER DATABASE d ALLOW_CONNECTIONS off"),
            ExtraStatement::AlterDatabase {
                name: ident("d"),
                action: AlterDatabaseAction::Options {
                    allow_connections: Some(false),
                    connection_limit: None,
                    is_template: None,
                },
            }
        );
        assert_eq!(
            ok("ALTER DATABASE d SET search_path TO a, B"),
            ExtraStatement::AlterDatabase {
                name: ident("d"),
                action: AlterDatabaseAction::Set {
                    name: "search_path".into(),
                    value: Some(vec!["a".into(), "b".into()]),
                    reset: false,
                },
            }
        );
        assert_eq!(
            ok("ALTER DATABASE d RESET work_mem"),
            ExtraStatement::AlterDatabase {
                name: ident("d"),
                action: AlterDatabaseAction::Set { name: "work_mem".into(), value: None, reset: true },
            }
        );
        assert_eq!(
            ok("ALTER DATABASE d SET timezone FROM CURRENT"),
            ExtraStatement::AlterDatabase {
                name: ident("d"),
                action: AlterDatabaseAction::Set { name: "timezone".into(), value: None, reset: false },
            }
        );
        assert_eq!(
            ok("ALTER DATABASE d RENAME TO e"),
            ExtraStatement::AlterDatabase { name: ident("d"), action: AlterDatabaseAction::RenameTo(ident("e")) }
        );
        let e = err("ALTER DATABASE d WITH");
        assert_eq!(e.message, "syntax error at end of input");
    }

    #[test]
    fn alter_materialized_view_forms() {
        assert_eq!(
            ok("ALTER MATERIALIZED VIEW IF EXISTS v RENAME COLUMN a TO b"),
            ExtraStatement::AlterMaterializedView {
                if_exists: true,
                name: name(&["v"]),
                action: AlterMatViewAction::RenameColumn { old: ident("a"), new: ident("b") },
            }
        );
        assert_eq!(
            ok("ALTER MATERIALIZED VIEW v OWNER TO CURRENT_USER"),
            ExtraStatement::AlterMaterializedView {
                if_exists: false,
                name: name(&["v"]),
                action: AlterMatViewAction::OwnerTo(ident("CURRENT_USER")),
            }
        );
        assert_eq!(
            ok("ALTER MATERIALIZED VIEW v SET (fillfactor = 70)"),
            ExtraStatement::Unsupported { feature: "ALTER MATERIALIZED VIEW ... SET".into() }
        );
    }

    #[test]
    fn unsupported_commands_keep_feature_text() {
        assert_eq!(
            ok("CREATE RULE r AS ON INSERT TO t DO INSTEAD NOTHING"),
            ExtraStatement::Unsupported { feature: "CREATE RULE".into() }
        );
        assert_eq!(
            ok("create or replace rule r as on insert to t do instead nothing"),
            ExtraStatement::Unsupported { feature: "CREATE RULE".into() }
        );
        assert_eq!(
            ok("ALTER DEFAULT PRIVILEGES GRANT ALL ON TABLES TO r"),
            ExtraStatement::Unsupported { feature: "ALTER DEFAULT PRIVILEGES".into() }
        );
        assert_eq!(
            ok("SECURITY LABEL ON TABLE t IS 'x'"),
            ExtraStatement::Unsupported { feature: "SECURITY LABEL".into() }
        );
    }

    #[test]
    fn unrouted_chunk_is_syntax_error_at_first_token() {
        let sql = "reindex_nothing x";
        let e = err(sql);
        assert_eq!(e.message, "syntax error at or near \"reindex_nothing\"");
        assert_eq!(e.position, Some(1));
    }

    #[test]
    fn sequence_and_role_routing() {
        assert!(is_extra("create sequence s"));
        assert!(is_extra("create temp sequence s"));
        assert!(is_extra("create unlogged sequence s"));
        assert!(is_extra("create role r"));
        assert!(is_extra("create user u"));
        assert!(is_extra("create group g"));
        assert!(is_extra("alter role r login"));
        assert!(is_extra("alter user r with password 'x'"));
        assert!(is_extra("alter role r"));
        // Forms sqlparser still owns.
        assert!(!is_extra("alter role r rename to s"));
        assert!(!is_extra("alter role r set work_mem = '1MB'"));
        assert!(!is_extra("alter role r reset all"));
        assert!(!is_extra("alter role all in database d set a = 1"));
        assert!(!is_extra("alter role r in database d set a = 1"));
        assert!(!is_extra("create table t (a int)"));
        assert!(!is_extra("create unlogged table t (a int)"));
        // Foreign-data commands keep their unsupported route.
        assert_eq!(
            ok("create user mapping for u server s"),
            ExtraStatement::Unsupported { feature: "CREATE USER MAPPING".into() }
        );
    }

    #[test]
    fn create_sequence_options_any_order() {
        let ExtraStatement::CreateSequence { temporary, unlogged, if_not_exists, name: n, options } =
            ok("create sequence if not exists s.q cycle increment by -2 as int start 5 no minvalue owned by none restart")
        else {
            panic!("not a CREATE SEQUENCE");
        };
        assert!(!temporary && !unlogged && if_not_exists);
        assert_eq!(n, name(&["s", "q"]));
        assert_eq!(options.len(), 7);
        assert_eq!(options[0], SequenceOptionAst::Cycle(true));
        assert_eq!(options[1], SequenceOptionAst::IncrementBy("-2".into()));
        assert_eq!(options[3], SequenceOptionAst::StartWith("5".into()));
        assert_eq!(options[5], SequenceOptionAst::OwnedBy(None));
        assert_eq!(options[6], SequenceOptionAst::Restart(None));
        let e = err("create sequence s increment");
        assert_eq!(e.message, "syntax error at end of input");
        let e = err("create sequence s foo");
        assert_eq!(e.message, "syntax error at or near \"foo\"");
    }

    #[test]
    fn role_options_parse_signed_and_unordered() {
        let ExtraStatement::CreateRole { kind, name: n, options } =
            ok("create user u with in role a, b connection limit -2 noinherit admin c password null sysid 3")
        else {
            panic!("not a CREATE ROLE");
        };
        assert_eq!(kind, RoleStmtKind::User);
        assert_eq!(n, ident("u"));
        let opts: Vec<RoleOptionAst> = options.into_iter().map(|i| i.option).collect();
        assert_eq!(
            opts,
            vec![
                RoleOptionAst::AddRoleTo(vec![ident("a"), ident("b")]),
                RoleOptionAst::ConnectionLimit(-2),
                RoleOptionAst::Inherit(false),
                RoleOptionAst::AdminMembers(vec![ident("c")]),
                RoleOptionAst::Password(None),
                RoleOptionAst::Sysid,
            ]
        );
        let ExtraStatement::AlterRole { name: n, options } = ok("alter role r connection limit +7 nologin") else {
            panic!("not an ALTER ROLE");
        };
        assert_eq!(n, ident("r"));
        assert_eq!(options[0].option, RoleOptionAst::ConnectionLimit(7));
        assert_eq!(options[0].pos, Some(14));
        assert_eq!(options[1].option, RoleOptionAst::Login(false));
        // CREATE-only options are syntax errors in ALTER ROLE.
        assert_eq!(err("alter role r sysid 3").message, "syntax error at or near \"sysid\"");
        let e = err("alter role r connection limit 2147483648");
        assert_eq!(e.message, "syntax error at or near \"2147483648\"");
        let e = err("create role r bogus");
        assert_eq!(e.message, "unrecognized role option \"bogus\"");
        assert_eq!(e.position, Some(15));
    }
}
