//! `pgarbor shell` (lease CLI-08): a small psql-like REPL over the EMBEDDED engine (no network).
//!
//! ## Flow
//! 1. Resolve target, open the database (read-only with `--read-only`), bind branch, create a session
//!    `db.session(&branch, SessionOptions{user, database, application_name: "pgarbor shell", ..})`.
//! 2. `-c SQL`: run it like `exec -c` (same code path via `render`), exit status 0/1. `-f FILE`: run
//!    the script (all items, stop at first error unless the script uses `\set ON_ERROR_STOP off` - not
//!    supported, so always stop at first error and exit 1).
//! 3. Interactive loop: read stdin line by line (no line-editing library is available; documented
//!    gap). Prompt, written to stderr only when stdin is a terminal (`std::io::IsTerminal`):
//!    `<database>[<branch>]=# ` (`=>` instead of `=#` when `session.setting("is_superuser")` is `off`);
//!    continuation prompt `<database>[<branch>]-# ` (statement open), `...'# ` (open quote), `...$# `
//!    (dollar quote), `...*# ` (block comment), `...(# ` (open paren) following
//!    `ScriptSplitter::state()`. Statements are completed by `;` via `sql_script::ScriptSplitter`.
//! 4. Each `ScriptItem::Statement` runs `session.simple_query_raw(text)` and prints through
//!    `render::print_result` / `print_error` (format table unless `\pset`-like `\x`/`\timing` toggled).
//!    The prompt shows transaction state like psql: `*` after the database when `txn_status()` is
//!    `InTransaction` and `!` when `Failed` (`postgres[main]*=# `).
//! 5. `ItemKind::Meta` lines go to `shell_meta::run_meta`; it returns `MetaOutcome::Quit` for `\q`.
//! 6. EOF (Ctrl-D) exits 0 after printing a newline when interactive. Ctrl-C: the default handler
//!    terminates the process (documented gap: no query cancel in the embedded shell).
//! 7. COPY FROM stdin inside the shell reads the following lines from the same input (the splitter's
//!    data mode); blocked on amendment pgarbor-wire-01 (`MemChannel`): until then error `COPY FROM
//!    stdin is not supported in the shell yet` (exit 8 for -c/-f).
//! 8. On exit: `session.close()`, `db.close()`.
//! `--json` is rejected for the interactive shell (usage error 64) but works with -c/-f
//! (same ExecReport as `exec`).
use pgarbor_common::PgError;
use crate::cli::ShellArgs;
use crate::ctx::{CliError, CliResult, Ctx};
use crate::exit::Exit;

#[allow(unused_variables)]
pub fn run(ctx: &mut Ctx, args: &ShellArgs) -> CliResult<Exit> {
    Err(CliError::from(PgError::unimplemented("CLI-08", "shell (cmd::shell::run)")))
}
