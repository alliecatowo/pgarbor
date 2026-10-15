//! Minimal PG v3 CLIENT for tests, diagnostics and the harness-independent smoke checks of the CLI
//! (lease WIR-21). It is NOT libpq: it implements exactly what is needed to drive the server in
//! integration tests without external drivers.
//!
//! ```ignore
//! let mut c = WireClient::connect_tcp(addr, "postgres", "postgres", None)?;
//! let r = c.simple_query("select 1 as x; select 'a'")?;
//! assert_eq!(r.results[0].rows[0][0].as_deref(), Some(&b"1"[..]));
//! let r = c.extended_query("select $1::int + 1", &[Some("41")])?;
//! ```
//!
//! ## Behavior
//! * `connect_*`: send StartupMessage 3.0 (`user`, `database`, `client_encoding=UTF8`), handle
//!   `AuthenticationOk`, cleartext, md5 (needs `password`) and SCRAM-SHA-256 (via `scram::ScramClient`),
//!   `NegotiateProtocolVersion` (recorded), collect ParameterStatus, BackendKeyData, stop at the first
//!   ReadyForQuery. ErrorResponse before that -> `Err(PgError)` rebuilt from the fields (code, message,
//!   severity).
//! * `simple_query`: send `Q`; read until ReadyForQuery; build a [`QueryResponse`].
//! * `extended_query`: unnamed Parse (no param oids), Bind with text parameters (`None` = NULL, all
//!   result columns text), Describe portal, Execute (0 rows), Sync; same response shape. Errors skip to
//!   ReadyForQuery.
//! * `send_raw` / `read_message` for byte-level protocol tests.
//! * `cancel()` opens a second connection and sends a CancelRequest with this client's key.
//! * Reads use a 10 s socket timeout by default (`set_timeout`).
use std::net::SocketAddr;
use std::path::Path;
use std::time::Duration;

use pgarbor_common::{PgError, Result};

use crate::error_fields::ErrorFields;
use crate::transport::Stream;

/// One statement's result inside a [`QueryResponse`].
// CONTRACT
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ResultSet {
    /// `(name, type_oid, format)` per column from RowDescription.
    pub columns: Vec<(String, u32, i16)>,
    /// Raw DataRow bytes (`None` = NULL).
    pub rows: Vec<Vec<Option<Vec<u8>>>>,
    /// CommandComplete tag (`None` for EmptyQueryResponse).
    pub tag: Option<String>,
}

/// Everything received until ReadyForQuery.
// CONTRACT
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct QueryResponse {
    pub results: Vec<ResultSet>,
    pub notices: Vec<ErrorFields>,
    pub error: Option<ErrorFields>,
    pub notifications: Vec<(i32, String, String)>,
    pub parameter_status: Vec<(String, String)>,
    /// Status byte of the final ReadyForQuery (`b'I'`, `b'T'`, `b'E'`).
    pub status: u8,
}

/// The test client (module docs).
pub struct WireClient {
    pub(crate) stream: Stream,
    pub(crate) buf: Vec<u8>,
    pub(crate) params: Vec<(String, String)>,
    pub(crate) key: Option<(i32, Vec<u8>)>,
    pub(crate) status: u8,
    pub(crate) peer: Option<SocketAddr>,
}

impl WireClient {
    /// Connect over TCP and complete the startup (module docs).
    #[allow(unused_variables)]
    pub fn connect_tcp(addr: SocketAddr, user: &str, database: &str, password: Option<&str>) -> Result<WireClient> {
        Err(PgError::unimplemented("WIR-21", "TCP client connect (WireClient::connect_tcp)"))
    }

    /// Connect to a Unix socket file and complete the startup.
    #[allow(unused_variables)]
    pub fn connect_unix(path: &Path, user: &str, database: &str, password: Option<&str>) -> Result<WireClient> {
        Err(PgError::unimplemented("WIR-21", "Unix client connect (WireClient::connect_unix)"))
    }

    #[allow(unused_variables)]
    pub fn simple_query(&mut self, sql: &str) -> Result<QueryResponse> {
        Err(PgError::unimplemented("WIR-21", "client simple query (WireClient::simple_query)"))
    }

    #[allow(unused_variables)]
    pub fn extended_query(&mut self, sql: &str, params: &[Option<&str>]) -> Result<QueryResponse> {
        Err(PgError::unimplemented("WIR-21", "client extended query (WireClient::extended_query)"))
    }

    /// Write raw bytes (already framed) to the socket.
    #[allow(unused_variables)]
    pub fn send_raw(&mut self, bytes: &[u8]) -> Result<()> {
        Err(PgError::unimplemented("WIR-21", "client raw send (WireClient::send_raw)"))
    }

    /// Read one tagged backend message `(tag, body)`; `Ok(None)` on EOF.
    pub fn read_message(&mut self) -> Result<Option<(u8, Vec<u8>)>> {
        Err(PgError::unimplemented("WIR-21", "client raw read (WireClient::read_message)"))
    }

    /// Send CancelRequest for this connection's key on a NEW connection to the same address.
    pub fn cancel(&self) -> Result<()> {
        Err(PgError::unimplemented("WIR-21", "client cancel (WireClient::cancel)"))
    }

    #[allow(unused_variables)]
    pub fn set_timeout(&self, d: Option<Duration>) -> Result<()> {
        Err(PgError::unimplemented("WIR-21", "client timeout (WireClient::set_timeout)"))
    }

    /// Value of a ParameterStatus seen so far.
    pub fn parameter_status(&self, name: &str) -> Option<&str> {
        self.params.iter().rev().find(|(n, _)| n == name).map(|(_, v)| v.as_str())
    }
    /// `(pid, secret)` from BackendKeyData.
    pub fn backend_key(&self) -> Option<(i32, &[u8])> {
        self.key.as_ref().map(|(p, k)| (*p, k.as_slice()))
    }
    /// Last ReadyForQuery status byte.
    pub fn txn_status(&self) -> u8 {
        self.status
    }

    /// Send Terminate and close.
    pub fn terminate(self) -> Result<()> {
        Err(PgError::unimplemented("WIR-21", "client terminate (WireClient::terminate)"))
    }
}
