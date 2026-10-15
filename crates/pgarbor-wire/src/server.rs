//! Server lifecycle (lease WIR-19): start listeners, expose addresses, graceful shutdown.
//!
//! ```ignore
//! let server = Server::start(db.clone(), ServerConfig::loopback("main"))?;
//! let url = server.url("postgres", "postgres");       // postgres://postgres@127.0.0.1:41873/postgres?sslmode=disable
//! let handle = server.handle();                        // clone into a signal-watching thread
//! server.wait_for(&|| signals_requested());            // blocks; on stop runs shutdown()
//! ```
//!
//! ## `Server::start`
//! 1. `cfg.validate()?`; `Shared::new(db, cfg)`;
//! 2. `listener::bind_all(&shared.config)?` (all-or-nothing);
//! 3. one thread per listener running `listener::accept_loop` (names `pgarbor-accept-N`); an
//!    accept-loop error is printed to stderr and requests stop of the whole server;
//! 4. return `Server` holding the join handles, resolved TCP addresses and Unix socket paths.
//!
//! ## `Server::shutdown` (idempotent, blocking; safe to call from any thread)
//! 1. `shared.request_stop()` (accept loops exit within ~50 ms; idle connection loops notice at their
//!    next poll and send FATAL `57P01 terminating connection due to administrator command`);
//! 2. `shared.registry.terminate_all()` (interrupts running statements, FATAL 57P01);
//! 3. join the accept threads (this also removes Unix socket files);
//! 4. wait up to `shutdown_grace` for `registry` to become empty (poll 20 ms);
//! 5. `registry.close_all_sockets()`, then wait up to 2 s more; connection threads still running after
//!    that are detached (never block forever).
//! Does NOT close the `Database` (the CLI owns that: `db.close()`, see `serve` in the CLI).
//!
//! ## Addresses and URLs
//! `url(user, database)`: first TCP listener: `postgres://{user}@{host}:{port}/{database}?sslmode=disable`
//! (IPv6 hosts bracketed; `0.0.0.0` is printed as `127.0.0.1`); Unix-only servers:
//! `postgres://{user}@/{database}?host={dir}&port={port}&sslmode=disable`. User and database are
//! percent-encoded (everything outside `A-Za-z0-9-._~`).
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;

use pgarbor::Database;
use pgarbor_common::{PgError, Result};

use crate::config::ServerConfig;
use crate::shared::Shared;

/// A running server (module docs).
pub struct Server {
    pub(crate) shared: Arc<Shared>,
    pub(crate) tcp_addrs: Vec<SocketAddr>,
    pub(crate) unix_paths: Vec<PathBuf>,
    pub(crate) accept_threads: Mutex<Vec<JoinHandle<()>>>,
}

/// Cheap clonable control handle (`Send + Sync`) for signal watchers and `branch exec`.
#[derive(Clone)]
pub struct ServerHandle {
    pub(crate) shared: Arc<Shared>,
    pub(crate) tcp_addrs: Vec<SocketAddr>,
}

impl ServerHandle {
    /// Request shutdown without blocking (sets the stop flag only). `Server::shutdown`/`wait_for`
    /// perform the actual teardown.
    pub fn stop(&self) {
        self.shared.request_stop();
    }
    pub fn is_stopping(&self) -> bool {
        self.shared.is_stopping()
    }
    /// First TCP address (port resolved).
    pub fn local_addr(&self) -> Option<SocketAddr> {
        self.tcp_addrs.first().copied()
    }
    /// Established sessions right now.
    pub fn connection_count(&self) -> usize {
        self.shared.registry.len()
    }
}

impl Server {
    /// Start listening (module docs).
    #[allow(unused_variables)]
    pub fn start(db: Database, cfg: ServerConfig) -> Result<Server> {
        Err(PgError::unimplemented("WIR-19", "start the server (Server::start)"))
    }

    pub fn handle(&self) -> ServerHandle {
        ServerHandle { shared: Arc::clone(&self.shared), tcp_addrs: self.tcp_addrs.clone() }
    }
    /// First TCP address.
    pub fn tcp_addr(&self) -> Option<SocketAddr> {
        self.tcp_addrs.first().copied()
    }
    pub fn tcp_addrs(&self) -> &[SocketAddr] {
        &self.tcp_addrs
    }
    pub fn unix_paths(&self) -> &[PathBuf] {
        &self.unix_paths
    }
    pub fn config(&self) -> &ServerConfig {
        &self.shared.config
    }
    pub fn database(&self) -> &Database {
        &self.shared.db
    }
    pub fn connection_count(&self) -> usize {
        self.shared.registry.len()
    }

    /// Connection URL (module docs).
    #[allow(unused_variables)]
    pub fn url(&self, user: &str, database: &str) -> String {
        // Placeholder shape; the worker implements percent-encoding and the Unix form.
        match self.tcp_addrs.first() {
            Some(a) => format!("postgres://{user}@{a}/{database}?sslmode=disable"),
            None => format!("postgres://{user}@/{database}?sslmode=disable"),
        }
    }

    /// Stop everything (module docs).
    pub fn shutdown(&self) -> Result<()> {
        Err(PgError::unimplemented("WIR-19", "graceful shutdown (Server::shutdown)"))
    }

    /// Block until `should_stop()` returns true (polled every 50 ms) or `ServerHandle::stop` was
    /// called, then run `shutdown()`.
    #[allow(unused_variables)]
    pub fn wait_for(&self, should_stop: &dyn Fn() -> bool) -> Result<()> {
        Err(PgError::unimplemented("WIR-19", "wait for stop request (Server::wait_for)"))
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        // Best effort: never leave listener threads running when the owner forgets `shutdown`.
        self.shared.request_stop();
    }
}
