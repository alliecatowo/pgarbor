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
use std::io::Write;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use pgarbor::Database;
use pgarbor_wire::{Server, ServerConfig};
use crate::cli::BranchExecArgs;
use crate::ctx::{CliError, CliResult, Ctx};
use crate::exit::Exit;
use crate::{control_client::ListenLease, open, paths, reports::ExecExitEvent, serve_registry, signals};

struct Cleanup {
    server: Option<Server>,
    db: Option<Database>,
    lease: Option<ListenLease>,
}

impl Drop for Cleanup {
    fn drop(&mut self) {
        self.lease.take();
        if let Some(server) = self.server.take() {
            let _ = server.shutdown();
        }
        if let Some(db) = self.db.take() {
            let _ = db.close();
        }
    }
}

pub fn run(ctx: &mut Ctx, args: &BranchExecArgs) -> CliResult<Exit> {
    let target = paths::resolve_branch_target(args.branch.as_deref(), &args.target)?;
    if std::env::var_os(&args.env_name).is_some_and(|v| !v.is_empty()) && !args.force_env {
        return Err(CliError::refused("DATABASE_URL is already set; refusing to overwrite it")
            .with_hint("pass --force-env to replace it"));
    }

    let mut cleanup = Cleanup { server: None, db: None, lease: None };
    let branch;
    let host;
    let port;
    match open::open_database(&target.db_path, &open::OpenSpec::existing()) {
        Ok(db) => {
            cleanup.db = Some(db.clone());
            branch = paths::bind_branch(&db, &target)?;
            let server = Server::start(db.clone(), ServerConfig::loopback(&branch)).map_err(CliError::from)?;
            let addr = server.tcp_addr().ok_or_else(|| CliError::internal("wire server started without a TCP listener"))?;
            host = addr.ip().to_string();
            port = addr.port();
            cleanup.server = Some(server);
        }
        Err(open_err) if open_err.exit == Exit::LOCKED && !args.no_reuse => {
            let info = match serve_registry::read_info(&target.db_path) {
                Ok(Some(info)) => info,
                Ok(None) | Err(_) => return Err(open_err),
            };
            branch = target.branch.clone().or_else(|| target.from_handle.as_ref().map(|h| h.branch_name_hint.clone()))
                .ok_or_else(|| CliError::usage("a branch name is required when reusing a served database"))?;
            let lease = crate::control_client::request_listener(&info, &branch)?;
            host = lease.host.clone();
            port = lease.port;
            cleanup.lease = Some(lease);
        }
        Err(open_err) => return Err(open_err),
    }

    let url = url_for(&args.user, host.as_str(), port, &args.database);
    if ctx.verbose > 0 { ctx.debug(&format!("running command with branch {branch} at {url}")); }
    let program = args.command.first().ok_or_else(|| CliError::usage("branch exec requires a command"))?;
    let mut command = Command::new(program);
    command.args(args.command.iter().skip(1));
    command.stdin(Stdio::inherit()).stdout(Stdio::inherit()).stderr(Stdio::inherit());
    command.env(&args.env_name, &url).env("PGARBOR_BRANCH", &branch).env("PGARBOR_URL", &url);
    signals::install().map_err(|e| CliError::failure(format!("could not install signal handlers: {e}")))?;
    let mut child = match command.spawn() {
        Ok(child) => child,
        Err(e) => {
            let name = program.to_string_lossy();
            let err = match e.kind() {
                std::io::ErrorKind::NotFound => CliError::new(Exit::CMD_NOT_FOUND, format!("command not found: {name}")),
                std::io::ErrorKind::PermissionDenied => CliError::new(Exit::CANNOT_EXEC, format!("permission denied: {name}")),
                _ => CliError::failure(format!("could not execute {name}: {e}")),
            };
            drop(cleanup);
            return Err(err);
        }
    };
    let status = match wait_child(&mut child) {
        Ok(status) => status,
        Err(e) => { drop(cleanup); return Err(CliError::failure(format!("waiting for child process failed: {e}"))); }
    };
    #[cfg(unix)]
    let (code, signal) = {
        use std::os::unix::process::ExitStatusExt;
        match status.code() {
            Some(code) => (code.clamp(0, 255) as u8, None),
            None => { let sig = status.signal().unwrap_or(0); (Exit::from_signal(sig).code(), Some(sig)) }
        }
    };
    #[cfg(not(unix))]
    let (code, signal) = (status.code().unwrap_or(1).clamp(0, 255) as u8, None);

    let owned_listener = cleanup.server.is_some();
    drop(cleanup);
    if ctx.json {
        let event = ExecExitEvent { event: "exited", branch, exit_code: code, signal, owned_listener };
        let line = serde_json::to_string(&event).map_err(|e| CliError::internal(format!("json serialization failed: {e}")))?;
        let _ = writeln!(ctx.err, "{line}");
    }
    Ok(Exit(code))
}

fn wait_child(child: &mut Child) -> std::io::Result<std::process::ExitStatus> {
    let mut forwarded = false;
    let mut kill_at = None;
    loop {
        if let Some(status) = child.try_wait()? { return Ok(status); }
        if !forwarded && signals::shutdown_requested() {
            let sig = signals::last_signal();
            if sig != 0 { signals::send_signal(child.id(), sig); }
            forwarded = true;
            kill_at = Some(Instant::now() + Duration::from_secs(10));
        }
        if kill_at.is_some_and(|deadline| Instant::now() >= deadline) { child.kill()?; kill_at = None; }
        std::thread::sleep(Duration::from_millis(20));
    }
}

fn url_for(user: &str, host: &str, port: u16, database: &str) -> String {
    fn encode(s: &str) -> String {
        let mut out = String::with_capacity(s.len());
        for b in s.bytes() {
            if b.is_ascii_alphanumeric() || b"-._~".contains(&b) { out.push(char::from(b)); }
            else { out.push_str(&format!("%{b:02X}")); }
        }
        out
    }
    let host = if host.contains(':') && !host.starts_with('[') { format!("[{host}]") } else { host.to_string() };
    format!("postgres://{}@{}:{port}/{}?sslmode=disable", encode(user), host, encode(database))
}

#[cfg(test)]
mod tests {
    use super::url_for;
    #[test]
    fn url_components_are_percent_encoded() {
        assert_eq!(url_for("a b", "127.0.0.1", 5432, "d/b"), "postgres://a%20b@127.0.0.1:5432/d%2Fb?sslmode=disable");
        assert_eq!(url_for("u", "::1", 1, "db"), "postgres://u@[::1]:1/db?sslmode=disable");
    }
}
