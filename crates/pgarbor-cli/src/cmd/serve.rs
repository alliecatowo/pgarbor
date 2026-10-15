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
    let target = crate::paths::resolve_target(args.file.as_deref(), &args.target)?;
    run_serve(ctx, ServeParams { target, opts: args.opts.clone() })
}

/// The shared implementation (module docs).
#[allow(unused_variables)]
pub fn run_serve(ctx: &mut Ctx, params: ServeParams) -> CliResult<Exit> {
    crate::signals::install()?;
    let opts = &params.opts;
    let db = crate::open::open_database(&params.target.db_path, &crate::open::OpenSpec { read_only: opts.read_only, ..crate::open::OpenSpec::existing() })?;
    let branch = crate::paths::bind_branch(&db, &params.target)?;
    let mut cfg = pgarbor_wire::ServerConfig::loopback(&branch);
    cfg.listen = vec![pgarbor_wire::ListenAddr::Tcp { host: opts.host.clone(), port: opts.port }];
    if let Some(dir) = &opts.socket_dir { cfg.listen.push(pgarbor_wire::ListenAddr::Unix { dir: dir.clone(), port: opts.port }); }
    cfg.auth = match opts.auth {
        crate::cli::AuthArg::Auto => pgarbor_wire::AuthPolicy::Auto,
        crate::cli::AuthArg::Trust => pgarbor_wire::AuthPolicy::Trust,
        crate::cli::AuthArg::Password => pgarbor_wire::AuthPolicy::Password,
        crate::cli::AuthArg::Md5 => pgarbor_wire::AuthPolicy::Md5,
        crate::cli::AuthArg::ScramSha256 => pgarbor_wire::AuthPolicy::ScramSha256,
    };
    cfg.max_connections = opts.max_connections;
    cfg.allow_nonloopback = opts.allow_nonloopback;
    if let Some(value) = &opts.statement_timeout { cfg.default_gucs.push(("statement_timeout".into(), value.clone())); }
    if let Some(path) = &opts.transcript { cfg.transcript = Some(pgarbor_wire::transcript::TranscriptHandle(std::sync::Arc::new(crate::transcript_file::JsonlFileSink::open(path)?))); }
    cfg.validate().map_err(|e| if e.is_unimplemented() { CliError::from(e) } else { CliError::refused(e.message) })?;
    let server = pgarbor_wire::Server::start(db.clone(), cfg.clone())?;
    let addr = server.tcp_addr().ok_or_else(|| CliError::internal("server has no TCP listener"))?;
    let url = server.url("postgres", "postgres");
    if ctx.json {
        ctx.emit_json_line(&crate::reports::ListeningEvent { event: "listening", host: opts.host.clone(), port: addr.port(), url: url.clone(), socket_dir: opts.socket_dir.as_ref().map(|p| p.display().to_string()), branch: branch.clone(), pid: std::process::id(), db_path: params.target.db_path.display().to_string() })?;
    } else {
        ctx.out_line(&format!("pgarbor: listening on {}:{}", opts.host, addr.port()));
        ctx.info(&format!("pgarbor: url: {url}"));
    }
    let mut control = None;
    if !opts.no_registry {
        let info = crate::reports::ServeInfo { format: crate::serve_registry::INFO_FORMAT.into(), pid: std::process::id(), db_path: std::fs::canonicalize(&params.target.db_path)?.display().to_string(), db_uuid: db.engine().container().db_uuid().to_hyphenated(), started_unix_ms: std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_millis() as u64).unwrap_or(0), listeners: vec![crate::reports::ListenerInfo { branch: branch.clone(), host: opts.host.clone(), port: addr.port(), socket_dir: opts.socket_dir.as_ref().map(|p| p.display().to_string()), on_demand: false }], control: None };
        if let Err(e) = crate::serve_registry::write_info(&params.target.db_path, &info) { ctx.info(&format!("pgarbor: warning: {e}")); }
        match crate::control::ControlServer::start(db.clone(), crate::control::ControlConfig { db_path: params.target.db_path.clone(), primary_branch: branch, primary_host: opts.host.clone(), primary_port: addr.port(), template: cfg }) {
            Ok(c) => control = Some(c), Err(e) => ctx.info(&format!("pgarbor: warning: {e}")),
        }
    }
    let closing = std::cell::Cell::new(0);
    let waited = server.wait_for(&|| {
        let stop = crate::signals::shutdown_requested();
        if stop { closing.set(server.connection_count()); }
        stop
    });
    if let Some(c) = control { c.stop()?; }
    server.shutdown()?;
    if !opts.no_registry { crate::serve_registry::remove_info(&params.target.db_path)?; }
    db.close()?;
    waited?;
    if ctx.json { ctx.emit_json_line(&crate::reports::StoppedEvent { event: "stopped", signal: match crate::signals::last_signal() { 0 => None, n => Some(n) }, connections_closed: closing.get().saturating_sub(server.connection_count()) })?; }
    Ok(Exit::OK)
}
