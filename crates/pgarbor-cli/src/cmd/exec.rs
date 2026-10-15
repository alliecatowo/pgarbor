//! `pgarbor exec` (lease CLI-10): run SQL non-interactively over the embedded engine.
//!
//! ## SQL source
//! Exactly one of `-c SQL`, `-f FILE` (`-f -` = stdin) or piped stdin (when stdin is not a terminal and
//! neither flag is given). Terminal stdin with no flag: exit 64 `no SQL given: use -c, -f or pipe SQL
//! on stdin`. Both -c and -f: exit 64.
//!
//! ## Execution
//! 1. Resolve target, open (read-only with `--read-only`), bind branch, `db.session(&branch,
//!    SessionOptions{user, database, application_name: "pgarbor exec", ..})`.
//! 2. Split the text with `sql_script::split_script` (splitter error -> exit 65, nothing executed).
//! 3. `--single-transaction`: run `BEGIN` first; on success `COMMIT` at the end; on error `ROLLBACK` and
//!    stop (even with --continue-on-error).
//! 4. For each item: `Statement` -> `session.simple_query_raw(text)` (so psql-like semantics: each
//!    statement is its own implicit transaction unless the script uses BEGIN); `Meta` items: `\.`
//!    ignored, others are skipped with a warning on stderr (`ignoring psql meta-command ...`);
//!    `CopyFromStdin{data}` -> blocked on amendment pgarbor-wire-01 (`pgarbor::MemChannel`): exit 8
//!    `COPY FROM stdin blocks are not supported yet`.
//! 5. Output via `render::print_result` (format from `--format`; `--json` forces
//!    [`crate::reports::ExecReport`] on stdout with ALL statements, including the failing one).
//!    An error prints via `render::print_error` and stops, unless `--continue-on-error` (then continue;
//!    final exit is 1 if any statement failed).
//! 6. Exit 0 when all statements succeeded, 1 otherwise. `session.close()`, `db.close()` before exit.
//!
//! The command never starts a listener. If the database is locked by `pgarbor serve` the open fails
//! with exit 4 and the hint from `open.rs`.
use pgarbor_common::PgError;
use crate::cli::ExecArgs;
use crate::ctx::{CliError, CliResult, Ctx};
use crate::exit::Exit;

#[allow(unused_variables)]
pub fn run(ctx: &mut Ctx, args: &ExecArgs) -> CliResult<Exit> {
    Err(CliError::from(PgError::unimplemented("CLI-10", "exec (cmd::exec::run)")))
}
