//! Listening sockets and accept loops (lease WIR-18). PG: `libpq/pqcomm.c` (`StreamServerPort`,
//! `StreamConnection`), `postmaster/postmaster.c` (`ServerLoop`).
//!
//! ## Binding ([`bind_all`])
//! For each `ListenAddr` in `ServerConfig::listen`:
//! * `Tcp{host, port}`: `std::net::TcpListener::bind((host, port))` (resolve `localhost` to
//!   `127.0.0.1`; IPv6 literal hosts may be bracketed). `port == 0` -> OS picks; the real address is
//!   `listener.local_addr()`. Set non-blocking. Failure: `PgError` with SQLSTATE `58030` and message
//!   `could not bind {host}:{port}: {os error text}`; when `io::ErrorKind::AddrInUse` the message
//!   is exactly `could not bind {host}:{port}: address in use` (the CLI maps this substring to exit
//!   code 2, see harness-02).
//! * `Unix{dir, port}` (cfg(unix)): path `<dir>/.s.PGSQL.<port>`. Path longer than 107 bytes:
//!   `58030` `Unix-domain socket path "<path>" is too long (maximum 107 bytes)`. If the file exists:
//!   try `UnixStream::connect`; success -> `58030` `could not bind {path}: address in use`; failure
//!   (stale) -> remove it. Bind, `set_permissions(mode = unix_socket_mode)`, non-blocking.
//!   On non-unix targets: `0A000` `Unix-domain sockets are not supported on this platform`.
//! * `port == 0` for a Unix listener means "use the first TCP listener's actual port, or 5432".
//!
//! ## Accept loop ([`accept_loop`])
//! One thread per listener. Loop until `shared.is_stopping()`:
//! * non-blocking `accept()`; `WouldBlock` -> `sleep(50 ms)` (signals and `shutdown` are observed at
//!   this granularity; a blocking accept is not used because SA_RESTART signal handlers would never
//!   interrupt it); `Interrupted` -> retry; other errors (e.g. EMFILE) -> sleep 100 ms and continue.
//! * on success: set the accepted socket BLOCKING again (`set_nonblocking(false)`), build a
//!   `Stream`, apply `tcp_nodelay`, take a `shared.threads` permit - none available: drop the socket
//!   immediately (flood protection) - else `thread::Builder::new().name("pgarbor-conn-N")
//!   .spawn(move || connection::serve_connection(..))`. A spawn failure drops the stream.
//! On exit remove the Unix socket file (only the one this listener created).
use std::net::{SocketAddr, TcpListener};
use std::path::PathBuf;
use std::sync::Arc;
use std::thread;
use std::time::Duration;

use pgarbor_common::{PgError, Result, SqlState};

use crate::config::{ListenAddr, ServerConfig};
use crate::shared::Shared;

/// A bound, non-blocking listener plus what it resolved to.
pub struct BoundListener {
    pub tcp: Option<(TcpListener, SocketAddr)>,
    #[cfg(unix)]
    pub unix: Option<(std::os::unix::net::UnixListener, PathBuf)>,
    /// The configured address this listener came from.
    pub spec: ListenAddr,
}

impl BoundListener {
    /// Real TCP address (port resolved).
    pub fn tcp_addr(&self) -> Option<SocketAddr> {
        self.tcp.as_ref().map(|(_, a)| *a)
    }
    /// Socket file path of a Unix listener.
    pub fn unix_path(&self) -> Option<PathBuf> {
        #[cfg(unix)]
        {
            self.unix.as_ref().map(|(_, p)| p.clone())
        }
        #[cfg(not(unix))]
        {
            None
        }
    }
}

/// Bind every configured address (module docs). All-or-nothing: on failure already created
/// listeners/socket files are cleaned up.
pub fn bind_all(cfg: &ServerConfig) -> Result<Vec<BoundListener>> {
    cfg.validate()?;
    let mut bound: Vec<BoundListener> = Vec::with_capacity(cfg.listen.len());
    for spec in &cfg.listen {
        let result = match spec {
            ListenAddr::Tcp { host, port } => {
                let bind_host = if host.eq_ignore_ascii_case("localhost") { "127.0.0.1" } else { host.as_str() };
                let bind_host = bind_host.trim_start_matches('[').trim_end_matches(']');
                TcpListener::bind((bind_host, *port)).and_then(|listener| {
                    let addr = listener.local_addr()?;
                    listener.set_nonblocking(true)?;
                    Ok(BoundListener { tcp: Some((listener, addr)), #[cfg(unix)] unix: None, spec: spec.clone() })
                }).map_err(|e| {
                    let msg = if e.kind() == std::io::ErrorKind::AddrInUse {
                        format!("could not bind {host}:{port}: address in use")
                    } else {
                        format!("could not bind {host}:{port}: {e}")
                    };
                    PgError::new(SqlState::IO_ERROR, msg)
                })
            }
            ListenAddr::Unix { dir, port } => bind_unix(dir, if *port == 0 {
                bound.iter().find_map(BoundListener::tcp_addr).map(|a| a.port()).unwrap_or(5432)
            } else { *port }, cfg.unix_socket_mode, spec),
        };
        match result {
            Ok(listener) => bound.push(listener),
            Err(error) => {
                cleanup_unix_files(&bound);
                return Err(error);
            }
        }
    }
    Ok(bound)
}

/// Run the accept loop for one listener on the current thread (module docs). Returns when the
/// server is stopping.
pub fn accept_loop(shared: Arc<Shared>, listener: BoundListener) -> Result<()> {
    let created_unix_path = listener.unix_path();
    while !shared.is_stopping() {
        let accepted = if let Some((tcp, _)) = listener.tcp.as_ref() {
            match tcp.accept() {
                Ok((stream, _)) => Some(crate::transport::Stream::Tcp(stream)),
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                    thread::sleep(Duration::from_millis(50));
                    None
                }
                Err(e) if e.kind() == std::io::ErrorKind::Interrupted => None,
                Err(_) => { thread::sleep(Duration::from_millis(100)); None }
            }
        } else {
            #[cfg(unix)]
            {
                match listener.unix.as_ref().map(|(sock, _)| sock.accept()) {
                    Some(Ok((stream, _))) => Some(crate::transport::Stream::Unix(stream)),
                    Some(Err(e)) if e.kind() == std::io::ErrorKind::WouldBlock => {
                        thread::sleep(Duration::from_millis(50)); None
                    }
                    Some(Err(e)) if e.kind() == std::io::ErrorKind::Interrupted => None,
                    Some(Err(_)) => { thread::sleep(Duration::from_millis(100)); None }
                    None => None,
                }
            }
            #[cfg(not(unix))]
            { None }
        };
        let Some(stream) = accepted else { continue };
        let nonblocking_result = match &stream {
            crate::transport::Stream::Tcp(s) => s.set_nonblocking(false),
            #[cfg(unix)]
            crate::transport::Stream::Unix(s) => s.set_nonblocking(false),
        };
        if nonblocking_result.is_err() || stream.set_nodelay(shared.config.tcp_nodelay).is_err() {
            continue;
        }
        let Some(permit) = shared.threads.try_acquire() else { continue };
        let id = shared.next_conn_id();
        let worker_shared = Arc::clone(&shared);
        let _ = thread::Builder::new().name(format!("pgarbor-conn-{id}"))
            .spawn(move || crate::connection::serve_connection(worker_shared, stream, permit));
    }
    if let Some(path) = created_unix_path {
        let _ = std::fs::remove_file(path);
    }
    Ok(())
}

fn io_error(message: String) -> PgError { PgError::new(SqlState::IO_ERROR, message) }

#[cfg(unix)]
fn bind_unix(dir: &std::path::Path, port: u16, mode: u32, spec: &ListenAddr) -> Result<BoundListener> {
    use std::os::unix::fs::PermissionsExt;
    use std::os::unix::net::{UnixListener, UnixStream};
    let path = dir.join(format!(".s.PGSQL.{port}"));
    use std::os::unix::ffi::OsStrExt;
    if path.as_os_str().as_bytes().len() > 107 {
        return Err(io_error(format!("Unix-domain socket path \"{}\" is too long (maximum 107 bytes)", path.display())));
    }
    if path.exists() {
        if UnixStream::connect(&path).is_ok() {
            return Err(io_error(format!("could not bind {}: address in use", path.display())));
        }
        std::fs::remove_file(&path).map_err(|e| io_error(format!("could not remove stale socket {}: {e}", path.display())))?;
    }
    let listener = UnixListener::bind(&path).map_err(|e| io_error(format!("could not bind {}: {e}", path.display())))?;
    let setup = (|| -> std::io::Result<()> {
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(mode))?;
        listener.set_nonblocking(true)
    })();
    if let Err(e) = setup {
        drop(listener);
        let _ = std::fs::remove_file(&path);
        return Err(io_error(format!("could not configure {}: {e}", path.display())));
    }
    Ok(BoundListener { tcp: None, unix: Some((listener, path)), spec: spec.clone() })
}

#[cfg(not(unix))]
fn bind_unix(_dir: &std::path::Path, _port: u16, _mode: u32, _spec: &ListenAddr) -> Result<BoundListener> {
    Err(PgError::new(SqlState::FEATURE_NOT_SUPPORTED, "Unix-domain sockets are not supported on this platform"))
}

fn cleanup_unix_files(listeners: &[BoundListener]) {
    #[cfg(unix)]
    for listener in listeners {
        if let Some((_, path)) = listener.unix.as_ref() { let _ = std::fs::remove_file(path); }
    }
    #[cfg(not(unix))]
    let _ = listeners;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ephemeral_tcp_ports_are_resolved_and_distinct() {
        let cfg = ServerConfig::loopback("main").with_tcp("127.0.0.1", 0);
        let a = bind_all(&cfg).expect("first ephemeral bind");
        let b = bind_all(&cfg).expect("second ephemeral bind");
        let port_a = a[0].tcp_addr().map(|a| a.port());
        let port_b = b[0].tcp_addr().map(|a| a.port());
        assert!(port_a.is_some() && port_b.is_some() && port_a != port_b);
    }

    #[test]
    fn occupied_tcp_port_reports_address_in_use() {
        let socket = TcpListener::bind(("127.0.0.1", 0)).expect("reserve local port");
        let port = socket.local_addr().expect("reserved address").port();
        let cfg = ServerConfig::loopback("main").with_tcp("127.0.0.1", port);
        let error = bind_all(&cfg).err().expect("occupied port must fail");
        assert!(error.message.contains("address in use"));
    }

    #[cfg(unix)]
    #[test]
    fn unix_path_limit_is_reported_before_bind() {
        let dir = PathBuf::from(format!("/{}", "x".repeat(110)));
        let cfg = ServerConfig::loopback("main").with_unix(dir, 5432);
        let error = bind_all(&cfg).err().expect("long Unix path must fail");
        assert_eq!(error.sqlstate.code(), "58030");
        assert!(error.message.contains("maximum 107 bytes"));
    }

    #[cfg(unix)]
    #[test]
    fn stale_unix_socket_is_removed_before_binding() {
        use std::os::unix::net::UnixListener;
        let dir = std::env::temp_dir().join(format!("pgarbor-wir18-{}-{}", std::process::id(), std::thread::current().name().unwrap_or("test")));
        std::fs::create_dir_all(&dir).expect("create test directory");
        let path = dir.join(".s.PGSQL.65431");
        let stale = UnixListener::bind(&path).expect("create stale socket");
        drop(stale);
        let cfg = ServerConfig::loopback("main").with_unix(&dir, 65431);
        let listeners = bind_all(&cfg).expect("stale socket is replaced");
        assert!(path.exists());
        drop(listeners);
        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_dir(&dir);
    }
}
