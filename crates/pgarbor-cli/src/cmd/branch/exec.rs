//! `pgarbor branch exec` (lease CLI-22; CLI-07 in the seed DAG, RFC 001 section 3): run a command with a
//! `DATABASE_URL` that reaches an isolated branch.
//!
//! ```text
//! pgarbor branch exec feature/auth --db ./dev.pga -- npm test
//! ```
//!
//! ## Sequence
//! 1. Resolve `paths::resolve_branch_target(args.branch, &args.target)`. The command vector
//!    (`args.command`, after `--`) is never empty (clap `required`).
//! 2. **Environment guard**: if the environment already has a non-empty variable named `args.env_name`
//!    and `!args.force_env` -> exit 7 `DATABASE_URL is already set; refusing to overwrite it` with
//!    hint `pass --force-env to replace it`. Checked BEFORE opening or starting anything.
//! 3. **Get a listener** for the branch (`GotListener { host, port, owned_server: Option<Server>,
//!    lease: Option<ListenLease>, db: Option<Database> }`):
//!    a. `open::open_database(path, writable)`. On success: `branch = paths::bind_branch(&db, &target)`,
//!       start an OWNED `pgarbor_wire::Server::start(db.clone(), ServerConfig::loopback(&branch))`
//!       (loopback, port 0 = the OS picks a free port atomically; no reserve-then-rebind race),
//!       auth `Auto`, `unix_socket` none.
//!    b. If the open fails with exit 4 (locked) and `!args.no_reuse`: `serve_registry::read_info(path)`;
//!       `Some(info)` -> `control_client::request_listener(&info, &branch)` (branch name given
//!       explicitly or resolved from the handle by reading the branch list read-only is NOT possible while
//!       locked: with a handle and a locked database use the handle's `branch_name_hint`). The returned
//!       lease keeps the on-demand listener alive. A pre-existing listener is NEVER stopped by this
//!       command.
//!    c. Locked and no usable serve info (or `--no-reuse`): exit 4 with the open error's hint.
//! 4. URL: `postgres://{user}@{host}:{port}/{database}?sslmode=disable` (`Server::url` for owned
//!    listeners; the same format built by hand for leases; `--user`/`--database` percent-encoded). All
//!    major drivers (libpq, node-pg, sqlx, Prisma) accept `sslmode=disable`; the server has no TLS.
//! 5. **Spawn**: `std::process::Command::new(&command[0]).args(&command[1..])`, inherit stdin/stdout/
//!    stderr, `env(env_name, url)`, plus `PGARBOR_BRANCH=<branch>` and `PGARBOR_URL=<url>`. Spawn error:
//!    `NotFound` -> exit 127 `command not found: <cmd>`; `PermissionDenied` -> exit 126; others exit 1.
//!    (Cleanup of step 3 still runs.)
//! 6. **Wait** with `try_wait` every 20 ms. `signals::install()` first: when `signals::
//!    shutdown_requested()` becomes true, forward the signal once with `signals::send_signal(child_pid,
//!    last_signal)` and keep waiting up to 10 s, then `Child::kill`.
//! 7. **Cleanup in all paths** (also on spawn failure and on forwarding): drop the lease; if the listener
//!    is owned: `Server::shutdown()` then `db.close()`. Always run cleanup before returning.
//! 8. **Exit status**: the child's `status.code()` (0-255) verbatim; killed by signal N -> `128 + N`
//!    (`std::os::unix::process::ExitStatusExt::signal`). `--json`: after the child exits write
//!    [`crate::reports::ExecExitEvent`] as one line to STDERR (stdout belongs to the child).
//! 10. The command never prints to stdout itself except in `--verbose` mode (stderr).
use pgarbor_common::PgError;
use crate::cli::BranchExecArgs;
use crate::ctx::{CliError, CliResult, Ctx};
use crate::exit::Exit;

#[allow(unused_variables)]
pub fn run(ctx: &mut Ctx, args: &BranchExecArgs) -> CliResult<Exit> {
    Err(CliError::from(PgError::unimplemented("CLI-22", "branch exec (branch::exec::run)")))
}
