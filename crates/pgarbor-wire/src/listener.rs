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

use pgarbor_common::{PgError, Result};

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
#[allow(unused_variables)]
pub fn bind_all(cfg: &ServerConfig) -> Result<Vec<BoundListener>> {
    Err(PgError::unimplemented("WIR-18", "bind listeners (listener::bind_all)"))
}

/// Run the accept loop for one listener on the current thread (module docs). Returns when the
/// server is stopping.
#[allow(unused_variables)]
pub fn accept_loop(shared: Arc<Shared>, listener: BoundListener) -> Result<()> {
    Err(PgError::unimplemented("WIR-18", "accept loop (listener::accept_loop)"))
}
