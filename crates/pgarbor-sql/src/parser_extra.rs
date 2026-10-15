//! Hand parser for PostgreSQL statements `sqlparser` lacks (SQL-02). See `extra_stmt` for the AST.
//!
//! Called by `parser::parse` with the token vector of ONE statement chunk (already terminated by
//! `Token::EOF`). The functions use `sqlparser::parser::Parser` as a token cursor:
//! `Parser::new(&PostgreSqlDialect{}).with_tokens_with_locations(tokens)`, then the public helpers
//! `parse_keyword`, `parse_keywords`, `parse_one_of_keywords`, `expect_keyword`, `consume_token`,
//! `parse_object_name(false)`, `parse_identifier()`, `parse_literal_string()`, `parse_expr()`,
//! `parse_data_type()`, `parse_comma_separated(..)`, `peek_token()`, `next_token()`.
use pgarbor_common::{PgError, Result};
use sqlparser::tokenizer::TokenWithSpan;

use crate::extra_stmt::ExtraStatement;
use crate::source::SourceMap;

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
#[allow(unused_variables)]
pub fn is_extra_statement(tokens: &[TokenWithSpan]) -> Result<bool> {
    Err(PgError::unimplemented("SQL-02", "extra statement detection (parser_extra::is_extra_statement)"))
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
#[allow(unused_variables)]
pub fn parse_extra(tokens: Vec<TokenWithSpan>, src: &SourceMap) -> Result<ExtraStatement> {
    Err(PgError::unimplemented("SQL-02", "hand-parsed PostgreSQL statements (parser_extra::parse_extra)"))
}
