//! `pgarbor import` (lease CLI-28): run a SQL script / `pg_dump` plain dump / `pgarbor dump` into a branch.
//!
//! ## Flow
//! 1. Input: `FILE` or `-` (stdin). Missing file: exit 5. Read incrementally line by line into
//!    `sql_script::ScriptSplitter` (dumps can be large; do not slurp the file).
//! 2. Target: `paths::resolve_target`; with `--create` and a missing database file the file is created
//!    first (`OpenSpec.create = true`, branch `main` unless `--branch` names another initial branch);
//!    without `--create` a missing file is exit 5. Branch via `paths::bind_branch` (error if the named
//!    branch does not exist: exit 5; `import` never creates branches implicitly).
//! 3. Session as in `exec` (`application_name = "pgarbor import"`).
//! 4. `--single-transaction`: `BEGIN` ... `COMMIT` around everything; any error -> `ROLLBACK`, report,
//!    exit 1, `committed = false`.
//! 5. Items:
//!    * `Statement` -> `session.simple_query_raw`; success counts `statements_run`; failure records a
//!      [`crate::reports::ScriptErrorJson`] (statement number, line, SQLSTATE, message) and stops unless
//!      `--continue-on-error` (then exit is still 1 at the end);
//!    * `CopyFromStdin{data}` -> install a memory channel with `data` as input and run the COPY
//!      statement; `copy_rows` += rows from the `COPY n` tag. Blocked on amendment pgarbor-wire-01
//!      (`pgarbor::MemChannel`); until then exit 8 for dumps that contain COPY blocks (INSERT dumps
//!      from `pgarbor dump` work);
//!    * `Meta` lines: `\connect`, `\c`, `\restrict`, `\unrestrict`, `\.` are ignored silently; any other
//!      meta line produces a warning on stderr and is skipped.
//! 6. `pg_dump` compatibility notes: `SET` statements are executed normally (unknown GUCs fail the
//!    statement, which is how incompatibilities show up); `CREATE EXTENSION`, `ALTER ... OWNER TO`
//!    run as written (the engine decides).
//! 7. Output: human summary `imported 1204 statements, 8800 rows copied into branch "main"` (stdout,
//!    suppressed by `--quiet`); errors as `render::print_error` with the script line: `ERROR:  ... (statement N, line L)`.
//!    `--json`: [`crate::reports::ImportReport`].
//! 8. Exit: 0 success; 1 statement error(s); 65 when the splitter fails (nothing after the failure
//!    point is executed; statements before it were already executed, or rolled back with
//!    `--single-transaction`); 8 when unsupported constructs are hit.
//! 9. Progress: for inputs over 1 MiB print `imported N statements...` to stderr every 5 seconds
//!    (not with `--quiet`/`--json`).
use pgarbor_common::PgError;
use crate::cli::ImportArgs;
use crate::ctx::{CliError, CliResult, Ctx};
use crate::exit::Exit;

#[allow(unused_variables)]
pub fn run(ctx: &mut Ctx, args: &ImportArgs) -> CliResult<Exit> {
    Err(CliError::from(PgError::unimplemented("CLI-28", "import (cmd::import::run)")))
}
