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
use std::time::{Duration, Instant};

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
    done: Mutex<bool>,
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
    pub fn start(db: Database, cfg: ServerConfig) -> Result<Server> {
        cfg.validate()?;
        let shared = Shared::new(db, cfg);
        let listeners = crate::listener::bind_all(&shared.config)?;
        let mut tcp_addrs = Vec::new();
        let mut unix_paths = Vec::new();
        for listener in &listeners {
            if let Some(addr) = listener.tcp_addr() { tcp_addrs.push(addr); }
            if let Some(path) = listener.unix_path() { unix_paths.push(path); }
        }
        let mut accept_threads = Vec::with_capacity(listeners.len());
        for (index, listener) in listeners.into_iter().enumerate() {
            let thread_shared = Arc::clone(&shared);
            let name = format!("pgarbor-accept-{}", index + 1);
            match std::thread::Builder::new().name(name).spawn(move || {
                if let Err(error) = crate::listener::accept_loop(Arc::clone(&thread_shared), listener) {
                    eprintln!("pgarbor-wire: accept loop failed: {error}");
                    thread_shared.request_stop();
                }
            }) {
                Ok(handle) => accept_threads.push(handle),
                Err(error) => {
                    shared.request_stop();
                    for handle in accept_threads { let _ = handle.join(); }
                    for path in &unix_paths { let _ = std::fs::remove_file(path); }
                    return Err(PgError::new(pgarbor_common::SqlState::INTERNAL_ERROR,
                        format!("could not start listener thread: {error}")));
                }
            }
        }
        Ok(Server { shared, tcp_addrs, unix_paths, accept_threads: Mutex::new(accept_threads), done: Mutex::new(false) })
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
    pub fn url(&self, user: &str, database: &str) -> String {
        let user = percent_encode(user);
        let database = percent_encode(database);
        match self.tcp_addrs.first() {
            Some(a) => {
                let host = if a.ip().is_unspecified() { "127.0.0.1".to_string() } else { a.ip().to_string() };
                let host = if host.contains(':') { format!("[{host}]") } else { host };
                format!("postgres://{user}@{host}:{}/{database}?sslmode=disable", a.port())
            }
            None => {
                let path = match self.unix_paths.first() { Some(p) => p, None => return format!("postgres://{user}@/{database}?sslmode=disable") };
                let dir = path.parent().unwrap_or_else(|| std::path::Path::new(""));
                let port = path.file_name().and_then(|n| n.to_str()).and_then(|n| n.strip_prefix(".s.PGSQL."))
                    .unwrap_or("5432");
                format!("postgres://{user}@/{database}?host={}&port={port}&sslmode=disable", percent_encode(&dir.to_string_lossy()))
            }
        }
    }

    /// Stop everything (module docs).
    pub fn shutdown(&self) -> Result<()> {
        let mut done = self.done.lock().unwrap_or_else(|e| e.into_inner());
        if *done { return Ok(()); }
        self.shared.request_stop();
        self.shared.registry.terminate_all();
        let handles = {
            let mut threads = self.accept_threads.lock().unwrap_or_else(|e| e.into_inner());
            std::mem::take(&mut *threads)
        };
        for handle in handles { let _ = handle.join(); }
        let deadline = Instant::now() + self.shared.config.shutdown_grace;
        while !self.shared.registry.is_empty() && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(20));
        }
        if !self.shared.registry.is_empty() {
            self.shared.registry.close_all_sockets();
            let close_deadline = Instant::now() + Duration::from_secs(2);
            while !self.shared.registry.is_empty() && Instant::now() < close_deadline {
                std::thread::sleep(Duration::from_millis(20));
            }
        }
        *done = true;
        Ok(())
    }

    /// Block until `should_stop()` returns true (polled every 50 ms) or `ServerHandle::stop` was
    /// called, then run `shutdown()`.
    pub fn wait_for(&self, should_stop: &dyn Fn() -> bool) -> Result<()> {
        while !self.shared.is_stopping() {
            if should_stop() { break; }
            std::thread::sleep(Duration::from_millis(50));
        }
        self.shutdown()
    }
}

fn percent_encode(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for byte in value.bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'.' | b'_' | b'~') {
            out.push(byte as char);
        } else {
            use std::fmt::Write;
            let _ = write!(out, "%{byte:02X}");
        }
    }
    out
}

impl Drop for Server {
    fn drop(&mut self) {
        // Best effort: never leave listener threads running when the owner forgets `shutdown`.
        self.shared.request_stop();
    }
}

#[cfg(test)]
mod tests {
    use super::percent_encode;

    #[test]
    fn url_component_encoding_uses_utf8_bytes_and_unreserved_set() {
        assert_eq!(percent_encode("azAZ09-._~"), "azAZ09-._~");
        assert_eq!(percent_encode("a b/@"), "a%20b%2F%40");
        assert_eq!(percent_encode("é"), "%C3%A9");
    }
}
