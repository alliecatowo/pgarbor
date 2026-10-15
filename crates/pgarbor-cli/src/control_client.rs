//! Client side of the control socket (lease CLI-25), used by `branch exec` when the database is
//! locked by a running `pgarbor serve`.
//!
//! [`request_listener`] connects to `info.control.unix_path`, sends
//! `{"op":"listen","branch":"<branch>"}\n`, reads one response line (timeout 10 s) and returns a
//! [`ListenLease`] that owns the open connection. Dropping the lease closes the connection, which
//! releases the on-demand listener on the server side (protocol in `control.rs`).
//!
//! Errors: no control endpoint in `info` (older serve / `--no-registry`): exit 4 `database file
//! "<db>" is in use by pgarbor serve (pid N) which does not accept control connections`; connect
//! failure: exit 4 with the OS error; `{"ok":false,...}` response: `CliError::new(Exit(exit), error)`
//! (unknown branch -> exit 5); malformed response: exit 1 `invalid response from serve control socket`.
//! Non-unix targets: exit 8 unsupported.
use pgarbor_common::PgError;
use crate::ctx::CliError;
use crate::reports::ServeInfo;

/// A granted listener; the TCP endpoint to put into the URL.
pub struct ListenLease {
    pub host: String,
    pub port: u16,
    pub branch: String,
    /// The server had to start a new listener for this branch (false = reused the primary).
    pub on_demand: bool,
    /// Keeps the lease alive (a `UnixStream` on unix).
    pub(crate) _conn: Box<dyn std::any::Any>,
}

/// Ask the serving process for a listener on `branch` (module docs).
#[allow(unused_variables)]
pub fn request_listener(info: &ServeInfo, branch: &str) -> Result<ListenLease, CliError> {
    Err(CliError::from(PgError::unimplemented("CLI-25", "request a listener from the serving process (control_client::request_listener)")))
}
