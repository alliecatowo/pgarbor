//! `pgarbor dump` (lease CLI-26): pg_dump-like PLAIN SQL text of one branch, produced entirely
//! through SQL against the catalog (embedded session), so it also serves as a catalog test.
//! Data export is delegated to `dump_data` (lease CLI-27).
//!
//! ## Output order (mirrors `pg_dump` plain format)
//! ```text
//! --
//! -- PGArbor database dump (branch "main", PostgreSQL 18.6 compatible)
//! --
//! SET client_encoding = 'UTF8';
//! SET standard_conforming_strings = on;
//! SELECT pg_catalog.set_config('search_path', '', false);
//! ```
//! then, unless `--data-only`: user schemas (`CREATE SCHEMA x;` for non-system schemas other than
//! `public`), extensions are NOT supported (error 8 if any exist), types in dependency order
//! (enums `CREATE TYPE s.t AS ENUM ('a','b');`, composites, domains with constraints), sequences
//! (`CREATE SEQUENCE ... START WITH .. INCREMENT BY .. MINVALUE .. MAXVALUE .. CACHE .. [CYCLE];`),
//! tables (`CREATE TABLE s.t (col type [COLLATE] [DEFAULT expr] [NOT NULL] [GENERATED ...], ...);`
//! with `format_type(atttypid, atttypmod)` and `pg_get_expr(adbin, adrelid)`), functions/procedures
//! (`pg_get_functiondef(oid)`), views/materialized views (`CREATE VIEW ... AS ` + `pg_get_viewdef`),
//! then the data (`dump_data::dump_table_data` for every table in dependency order: referenced tables
//! first; cycles fall back to catalog order and rely on the constraints being added afterwards), then
//! post-data: sequence values (`SELECT pg_catalog.setval('s.seq', N, true|false);` from the sequence
//! relation), constraints (`ALTER TABLE ONLY s.t ADD CONSTRAINT n <pg_get_constraintdef>;` primary
//! key / unique first, then check, then foreign keys), indexes (`pg_get_indexdef`), triggers
//! (`pg_get_triggerdef`), `ALTER TABLE ... OWNER` is NOT emitted (single-owner dev databases; `--no-owner`
//! is the implicit behavior), comments (`COMMENT ON ... IS '...'` from `pg_description`).
//! `--schema-only` omits the data section and the setval lines; `--data-only` emits only data.
//!
//! ## Rules
//! * Identifiers are quoted only when needed (`quote_ident` semantics: lowercase letters, digits,
//!   underscore, not starting with a digit and not a reserved keyword stay bare).
//! * Everything is read in ONE `REPEATABLE READ READ ONLY` transaction so the dump is consistent
//!   (`BEGIN ISOLATION LEVEL REPEATABLE READ READ ONLY` ... `COMMIT`).
//! * Output goes to `--output FILE` (written to `<file>.tmp` then renamed) or stdout. Statement
//!   terminators are `;` at line end; no psql meta-commands are emitted except `COPY ... \.` blocks
//!   with `--copy`.
//! * Unsupported object kinds found in the catalog (e.g. extensions, event triggers, publications) ->
//!   exit 8 listing them; the dump is not silently incomplete.
//! * Catalog functions the engine lacks (`pg_get_viewdef`, ...) surface their 0A000/42883 error: exit 8
//!   `dump needs <function>, which is not implemented yet`.
//! * `--json` is not meaningful for dump (the output is SQL): `--json` prints a summary
//!   `{"branch","tables","rows","bytes"}` to stdout ONLY when `--output` is given, else usage error 64.
use pgarbor_common::PgError;
use crate::cli::DumpArgs;
use crate::ctx::{CliError, CliResult, Ctx};
use crate::exit::Exit;

#[allow(unused_variables)]
pub fn run(ctx: &mut Ctx, args: &DumpArgs) -> CliResult<Exit> {
    Err(CliError::from(PgError::unimplemented("CLI-26", "dump (cmd::dump::run)")))
}

/// PostgreSQL `quote_ident`: returns `s` unchanged if it matches `^[a-z_][a-z0-9_]*$` and is not a
/// reserved word (use the keyword list in `contracts/reference/pg18/pg_keywords.csv` for the
/// `reserved`/`reserved (can be function or type)`/`reserved (cannot be function or type)` categories
/// - embed the reserved words as a const array), otherwise wraps in `"` and doubles inner `"`.
#[allow(unused_variables)]
pub fn quote_ident(s: &str) -> Result<String, CliError> {
    Err(CliError::from(PgError::unimplemented("CLI-26", "identifier quoting (dump::quote_ident)")))
}

/// PostgreSQL `quote_literal` under `standard_conforming_strings = on`: `'` doubled, wrapped in `'`;
/// a value containing a backslash is emitted as `E'...'` with `\` doubled (matches `pg_dump`).
#[allow(unused_variables)]
pub fn quote_literal(s: &str) -> Result<String, CliError> {
    Err(CliError::from(PgError::unimplemented("CLI-26", "literal quoting (dump::quote_literal)")))
}
