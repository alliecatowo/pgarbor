//! Backslash commands of the shell (lease CLI-09). Text-level reimplementation of the useful part of
//! `psql` (`psql/command.c`, `psql/describe.c`); all catalog access is plain SQL through the session
//! (`pg_catalog` views), so these commands also test the catalog layer.
//!
//! | command | behavior |
//! |---|---|
//! | `\q` | quit (`MetaOutcome::Quit`) |
//! | `\?` | print this table (one line per command) |
//! | `\conninfo` | `You are connected to database "postgres" as user "postgres" on branch "main" (file dev.pga).` |
//! | `\branch` | current branch name and head sequence; `\branch list` = same as `pgarbor branch list` table |
//! | `\l` | databases: `SELECT datname, pg_catalog.pg_get_userbyid(datdba) AS owner, pg_catalog.pg_encoding_to_char(encoding) AS encoding FROM pg_catalog.pg_database ORDER BY 1` |
//! | `\dn` | schemas: `pg_namespace` excluding `pg_*` and `information_schema` (psql default), owner via `pg_get_userbyid` |
//! | `\dt [pattern]`, `\di`, `\dv`, `\ds`, `\d` | relations by `relkind` (`r`, `i`, `v`, `S`; `\d` all), columns `Schema | Name | Type | Owner` with psql's type words (`table`, `index`, `view`, `sequence`), `pattern` supports `*` and `?` (converted to a regex on `relname`, optionally `schema.name`), only schemas visible via `pg_table_is_visible` unless the pattern names a schema |
//! | `\d NAME` | describe one relation: columns (`attname`, `format_type(atttypid, atttypmod)`, collation, `not null`, default via `pg_get_expr(adbin, adrelid)`), then `Indexes:` (`pg_get_indexdef`), `Check constraints:`, `Foreign-key constraints:` (`pg_get_constraintdef`) |
//! | `\timing` | toggle printing `Time: N.NNN ms` after each statement |
//! | `\x` | toggle expanded output (`table::render_expanded`) |
//! | `\echo TEXT` | print TEXT |
//! | `\i FILE` | run the script file through the same loop (relative to cwd) |
//! | `\set` / `\unset` | not supported: `invalid command \set` style error (see below) |
//! Unknown command: stderr `invalid command \foo` + `Try \? for help.`; the shell continues.
//! Commands that need a catalog function the engine lacks print the engine error and continue.
use pgarbor_common::PgError;
use pgarbor::Session;

use crate::ctx::{CliError, Ctx};

/// What the shell loop should do next.
// CONTRACT
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum MetaOutcome {
    Continue,
    Quit,
}

/// Mutable toggles owned by the shell loop.
// CONTRACT
#[derive(Clone, Debug, Default)]
pub struct ShellSettings {
    pub timing: bool,
    pub expanded: bool,
}

/// Run one backslash line (`line` starts with `\`, no trailing newline).
#[allow(unused_variables)]
pub fn run_meta(
    ctx: &mut Ctx,
    session: &mut Session,
    settings: &mut ShellSettings,
    branch: &str,
    line: &str,
) -> Result<MetaOutcome, CliError> {
    Err(CliError::from(PgError::unimplemented("CLI-09", "shell meta commands (shell_meta::run_meta)")))
}
