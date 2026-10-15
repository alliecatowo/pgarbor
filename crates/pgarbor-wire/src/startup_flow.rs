//! Connection startup negotiation (lease WIR-08). PG: `postmaster/postmaster.c`
//! `ProcessStartupPacket`, `BackendInitialize`; `protocol.sgml` "Start-up", "Protocol Versions".
//!
//! [`negotiate`] drives the untagged phase on a fresh [`ConnIo`] until the connection either
//! proceeds to authentication, is a finished CancelRequest, or must be dropped.
//!
//! ## Algorithm
//! Loop (at most 3 iterations: SSLRequest, GSSENCRequest, then the real packet):
//! 1. `io.poll_startup(Some(remaining))` where `remaining` = time left until `deadline`
//!    (`ServerConfig::startup_timeout` after accept). `TimedOut` -> FATAL `57014`
//!    `canceling authentication due to timeout`; `Eof` -> `Ok(Outcome::Dropped)` (port scanners,
//!    health checks; silent).
//! 2. `parse_startup_packet`. Errors are FATAL `08P01` and are sent before closing.
//! 3. `SslRequest` / `GssEncRequest`: `io.write_ssl_answer(tls::encryption_answer(policy))` (always
//!    `N`), then loop for the next packet. At most one SSLRequest and one GSSENCRequest are accepted;
//!    a repeat of either -> FATAL `08P01` `invalid startup packet layout`.
//! 4. `Cancel{pid, secret}`: `cancel::handle(&shared.db, pid, &secret)`, return
//!    `Outcome::CancelHandled` (caller closes, sends nothing).
//! 5. `Startup{major, minor, params}`:
//!    * `major != 3`: FATAL `0A000` `unsupported frontend protocol {major}.{minor}: server supports 3.0
//!      to 3.2`;
//!    * effective minor: `minor >= 2 -> 2`, else `0` (3.1 was never used; treat as 3.0);
//!    * unrecognized protocol options: every param named `_pq_.*` (we support none);
//!    * if `minor > 2` or `minor == 1` or any `_pq_.` option: send
//!      `NegotiateProtocolVersion{ newest_minor: effective_minor, unrecognized: [names] }` and flush
//!      (names exactly as received, in order);
//!    * `user`: last occurrence wins; missing/empty -> FATAL `28000` `no PostgreSQL user name specified
//!      in startup packet`; truncated to 63 bytes at a char boundary (NAMEDATALEN - 1);
//!    * `database`: last occurrence wins; missing/empty -> same as `user`; truncated to 63 bytes;
//!    * `application_name` (last wins; the full value is kept, the GUC layer validates it) ->
//!      `StartupInfo::application_name`;
//!    * `replication` present and not one of `false`, `off`, `no`, `0` (case-insensitive) -> FATAL
//!      `0A000` `replication connections are not supported`;
//!    * `options` (last wins) + remaining params + `ServerConfig::default_gucs` ->
//!      `options_parse::build_startup_gucs`;
//!    * `cancel_key_len = 4` when effective minor is 0 else `32` (`consts::CANCEL_KEY_LEN_*`).
//!
//! The returned [`StartupInfo`] feeds `auth` and `SessionOptions`. No ReadyForQuery is sent here.
use std::time::Instant;

use pgarbor_common::PgError;

use crate::conn_io::ConnIo;
use crate::error::{WireError, WireResult};
use crate::shared::Shared;

/// Everything learned from the startup packet.
// CONTRACT
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StartupInfo {
    pub user: String,
    pub database: String,
    pub application_name: String,
    /// Effective protocol minor (0 or 2).
    pub minor: u16,
    /// `SessionOptions::startup_gucs` (module docs of `options_parse`).
    pub startup_gucs: Vec<(String, String)>,
    /// 4 (protocol 3.0) or 32 (protocol 3.2).
    pub cancel_key_len: usize,
}

/// What `negotiate` decided.
// CONTRACT
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Outcome {
    /// Continue with authentication.
    Proceed(StartupInfo),
    /// A CancelRequest was processed; close without a reply.
    CancelHandled,
    /// The peer disconnected before sending a startup message; close silently.
    Dropped,
}

/// Run the startup phase (module docs).
#[allow(unused_variables)]
pub fn negotiate(io: &mut ConnIo, shared: &Shared, deadline: Instant) -> WireResult<Outcome> {
    Err(WireError::Pg(PgError::unimplemented("WIR-08", "startup negotiation (startup_flow::negotiate)")))
}
