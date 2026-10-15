//! Control socket of a serving process (lease CLI-24). Lets `pgarbor branch exec` obtain a listener
//! for ANY branch of a database that a long-running `pgarbor serve` has locked, and tells diagnostics
//! what is being served. Unix only (`#[cfg(unix)]`; on other targets `start` returns `0A000`).
//!
//! ## Transport and protocol
//! A Unix-domain stream socket at `serve_registry::control_socket_path(db)`, mode `0o600`.
//! Newline-delimited JSON, one request line -> one response line, connections are handled by one
//! thread each. Requests (`"op"`):
//! * `{"op":"ping"}` -> `{"ok":true,"pid":N}`;
//! * `{"op":"status"}` -> `{"ok":true,"info":<ServeInfo>}`;
//! * `{"op":"listen","branch":"feature"}` -> ensure a listener bound to that branch exists and return
//!   `{"ok":true,"host":"127.0.0.1","port":41873,"on_demand":true}`. If the primary listener already
//!   serves that branch, return it with `"on_demand":false`. Otherwise start a new
//!   `pgarbor_wire::Server` (`ServerConfig::loopback(branch)` with the same auth policy / limits as the
//!   primary, port 0, loopback ONLY regardless of the primary's host), remember it with a reference
//!   count and rewrite the info file. Errors -> `{"ok":false,"error":"<message>","exit":<code>}`
//!   (unknown branch: exit 5).
//! After a successful `listen` the connection STAYS OPEN: it is the lease. When the client closes it
//! (child finished, client crashed) the server decrements the branch's reference count and, at zero,
//! shuts the on-demand listener down (`Server::shutdown`) and rewrites the info file. The primary
//! listener is never stopped by leases.
//! * unknown op / malformed JSON -> `{"ok":false,"error":"bad request"}`; a request line over 64 KiB is
//!   rejected and the connection closed.
//!
//! ## Lifecycle
//! `ControlServer::start` binds the socket (removing a stale socket file whose connect fails), spawns
//! the accept thread (non-blocking accept, 50 ms poll of a stop flag) and returns. `stop()` sets the
//! flag, joins, shuts down all on-demand listeners and removes the socket file. Dropping without
//! `stop` must not leave threads running (`Drop` calls the same teardown).
use pgarbor_common::PgError;
use std::path::PathBuf;

use pgarbor::Database;
use pgarbor_wire::ServerConfig;

use crate::ctx::CliError;

/// Everything the control server needs to start on-demand listeners that match the primary.
// CONTRACT
#[derive(Clone, Debug)]
pub struct ControlConfig {
    pub db_path: PathBuf,
    /// Branch served by the primary listener.
    pub primary_branch: String,
    pub primary_host: String,
    pub primary_port: u16,
    /// Template for on-demand listeners (auth policy, limits, default GUCs); `branch`/`listen` are
    /// overwritten per request.
    pub template: ServerConfig,
}

/// The running control server (fields private to the implementation).
pub struct ControlServer {
    pub(crate) _private: (),
}

impl ControlServer {
    /// Bind the socket and start serving (module docs).
    #[allow(unused_variables)]
    pub fn start(db: Database, cfg: ControlConfig) -> Result<ControlServer, CliError> {
        Err(CliError::from(PgError::unimplemented("CLI-24", "start the control server (ControlServer::start)")))
    }

    /// Stop everything started by this control server (module docs). Idempotent.
    pub fn stop(&self) -> Result<(), CliError> {
        Err(CliError::from(PgError::unimplemented("CLI-24", "stop the control server (ControlServer::stop)")))
    }

    /// Current `ServeInfo.listeners` (primary + on-demand) for rewriting the info file.
    pub fn listeners(&self) -> Vec<crate::reports::ListenerInfo> {
        Vec::new()
    }
}
