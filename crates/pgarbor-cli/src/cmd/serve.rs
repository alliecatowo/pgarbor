//! `pgarbor serve` (lease CLI-07; CLI-02, CLI-11, harness-02 item 2). A foreground server for ONE branch.
//!
//! ## Sequence ([`run_serve`])
//! 1. Resolve target (`paths::resolve_target`), `signals::install()`.
//! 2. `open::open_database(path, OpenSpec{read_only: opts.read_only, ..existing()})`; errors map to
//!    exit 5 (missing), 3 (corrupt; stderr must contain `corrupt`), 4 (locked).
//! 3. `branch = paths::bind_branch(&db, &target)`.
//! 4. Build `pgarbor_wire::ServerConfig`: `ServerConfig::loopback(&branch)` then
//!    `listen = [Tcp{host, port}]` (+ `Unix{socket_dir, port}` when `--socket-dir`; with `--port 0` the
//!    Unix port label is the real TCP port, bind TCP first then compute), `auth` from `--auth`
//!    (`AuthPolicy::parse`), `max_connections`, `allow_nonloopback`, `default_gucs =
//!    [("statement_timeout", v)]` when given, `transcript = JsonlFileSink` (`transcript_file.rs`).
//!    `config.validate()` failures are exit 7 (non-loopback/trust) or 8 (TLS).
//! 5. `pgarbor_wire::Server::start(db.clone(), cfg)`. Bind failure whose message contains `address in
//!    use` -> exit 2 with stderr text `address in use: <host>:<port>` (the wire message already contains
//!    the phrase; harness-02 needs the exact lowercase `address in use`).
//! 6. **Announce** (after the sockets are listening, BEFORE anything optional):
//!    * human: exactly one stdout line `pgarbor: listening on <host>:<port>` (host as passed in
//!      `--host`; port the REAL port; flushed immediately). The connection URL goes to stderr as
//!      `pgarbor: url: postgres://postgres@127.0.0.1:41873/postgres?sslmode=disable` (suppressed by
//!      `--quiet`). Unix-only listeners (no TCP): `pgarbor: listening on <socket path>`.
//!    * `--json`: one line [`crate::reports::ListeningEvent`] on stdout instead of the human line.
//! 7. Unless `--no-registry` (or non-unix): write the discovery file (`serve_registry::write_info`) with
//!    [`crate::reports::ServeInfo`] and start `control::ControlServer`; failures here are warnings on
//!    stderr, never fatal.
//! 8. Block: `server.wait_for(&|| signals::shutdown_requested())` (polls every 50 ms).
//! 9. **Shutdown** (SIGTERM/SIGINT/SIGHUP): stop control server, `Server::shutdown` (stops accepting,
//!    FATAL 57P01 to sessions, waits `shutdown_grace`), remove the discovery file, `db.close()`
//!    (flushes sequence state). Must finish within 10 s and exit 0 (harness-02). `--json`: emit
//!    [`crate::reports::StoppedEvent`]. A second signal during shutdown forces immediate exit 130/143.
//! 10. Never kill another process; never write to the `.pga` except through the engine.
//!
//! Rust panics keep the default hook (`panicked at ...` on stderr, exit 101).
use pgarbor_common::PgError;
use crate::cli::{ServeArgs, ServeOpts};
use crate::ctx::{CliError, CliResult, Ctx};
use crate::exit::Exit;
use crate::paths::Target;

/// Parameters of a serve invocation after target resolution (shared with `branch serve`).
// CONTRACT
#[derive(Clone, Debug)]
pub struct ServeParams {
    pub target: Target,
    pub opts: ServeOpts,
}

#[allow(unused_variables)]
pub fn run(ctx: &mut Ctx, args: &ServeArgs) -> CliResult<Exit> {
    Err(CliError::from(PgError::unimplemented("CLI-07", "serve (cmd::serve::run)")))
}

/// The shared implementation (module docs).
#[allow(unused_variables)]
pub fn run_serve(ctx: &mut Ctx, params: ServeParams) -> CliResult<Exit> {
    Err(CliError::from(PgError::unimplemented("CLI-07", "serve (cmd::serve::run_serve)")))
}
