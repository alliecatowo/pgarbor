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

use pgarbor_common::{PgError, Severity, SqlState};

use crate::backend::Backend;
use crate::cancel;
use crate::conn_io::ConnIo;
use crate::consts::{CANCEL_KEY_LEN_3_0, CANCEL_KEY_LEN_3_2, NEWEST_MINOR};
use crate::error::{WireError, WireResult};
use crate::error_fields;
use crate::framing::Polled;
use crate::options_parse;
use crate::shared::Shared;
use crate::startup_packet::{parse_startup_packet, StartupPacket};
use crate::tls;

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
pub fn negotiate(io: &mut ConnIo, shared: &Shared, deadline: Instant) -> WireResult<Outcome> {
    let mut saw_ssl = false;
    let mut saw_gss = false;

    for _ in 0..3 {
        let remaining = deadline.saturating_duration_since(Instant::now());
        let body = match io.poll_startup(Some(remaining))? {
            Polled::Ready(body) => body,
            Polled::Eof => return Ok(Outcome::Dropped),
            Polled::TimedOut => {
                return send_fatal(io, PgError::new(SqlState::QUERY_CANCELED, "canceling authentication due to timeout"));
            }
        };
        let packet = match parse_startup_packet(&body) {
            Ok(packet) => packet,
            Err(err) => return send_fatal(io, err),
        };

        match packet {
            StartupPacket::SslRequest => {
                if saw_ssl {
                    return send_fatal(io, layout_error());
                }
                saw_ssl = true;
                io.write_ssl_answer(tls::encryption_answer(shared.config.tls))?;
            }
            StartupPacket::GssEncRequest => {
                if saw_gss {
                    return send_fatal(io, layout_error());
                }
                saw_gss = true;
                io.write_ssl_answer(tls::encryption_answer(shared.config.tls))?;
            }
            StartupPacket::Cancel { pid, secret } => {
                cancel::handle(&shared.db, pid, &secret);
                return Ok(Outcome::CancelHandled);
            }
            StartupPacket::Startup { major, minor, params } => {
                if major != 3 {
                    return send_fatal(io, PgError::new(
                        SqlState::FEATURE_NOT_SUPPORTED,
                        format!("unsupported frontend protocol {major}.{minor}: server supports 3.0 to 3.2"),
                    ));
                }

                let effective_minor = if minor >= 2 { 2 } else { 0 };
                let unrecognized: Vec<String> = params.iter()
                    .filter(|(name, _)| name.starts_with("_pq_."))
                    .map(|(name, _)| name.clone())
                    .collect();
                if minor > NEWEST_MINOR as u16 || minor == 1 || !unrecognized.is_empty() {
                    io.send_flush(&Backend::NegotiateProtocolVersion {
                        newest_minor: effective_minor as u32,
                        unrecognized,
                    })?;
                }

                let last = |key: &str| params.iter().rev().find(|(name, _)| name == key).map(|(_, value)| value.as_str());
                let user = match last("user") {
                    Some(s) if !s.is_empty() => truncate_name(s),
                    _ => return send_fatal(io, PgError::new(SqlState::INVALID_AUTHORIZATION_SPECIFICATION, "no PostgreSQL user name specified in startup packet")),
                };
                let database = match last("database") {
                    Some(s) if !s.is_empty() => truncate_name(s),
                    _ => return send_fatal(io, PgError::new(SqlState::INVALID_AUTHORIZATION_SPECIFICATION, "no PostgreSQL user name specified in startup packet")),
                };
                if last("replication").is_some_and(|value| !is_false(value)) {
                    return send_fatal(io, PgError::new(SqlState::FEATURE_NOT_SUPPORTED, "replication connections are not supported"));
                }
                let options = last("options").unwrap_or("");
                let startup_gucs = match options_parse::build_startup_gucs(&shared.config.default_gucs, &params, options) {
                    Ok(gucs) => gucs,
                    Err(err) => return send_fatal(io, err),
                };
                return Ok(Outcome::Proceed(StartupInfo {
                    user,
                    database,
                    application_name: last("application_name").unwrap_or("").to_owned(),
                    minor: effective_minor,
                    startup_gucs,
                    cancel_key_len: if effective_minor == 0 { CANCEL_KEY_LEN_3_0 } else { CANCEL_KEY_LEN_3_2 },
                }));
            }
        }
    }
    // Three request slots are sufficient for SSL, GSS, and the StartupMessage. Reaching this
    // indicates a repeated negotiation request before the actual startup packet.
    send_fatal(io, layout_error())
}

fn send_fatal<T>(io: &mut ConnIo, err: PgError) -> WireResult<T> {
    let fatal = err.with_severity(Severity::Fatal);
    io.send_flush(&error_fields::error_message(&fatal))?;
    Err(WireError::fatal(fatal))
}

fn layout_error() -> PgError {
    PgError::new(SqlState::PROTOCOL_VIOLATION, "invalid startup packet layout")
}

fn truncate_name(value: &str) -> String {
    let mut end = value.len().min(63);
    while !value.is_char_boundary(end) {
        end -= 1;
    }
    value[..end].to_owned()
}

fn is_false(value: &str) -> bool {
    ["false", "off", "no", "0"].iter().any(|false_value| value.eq_ignore_ascii_case(false_value))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn name_limit_is_bytes_and_utf8_safe() {
        let value = format!("{}é", "a".repeat(62));
        assert_eq!(truncate_name(&value), "a".repeat(62));
        assert_eq!(truncate_name(&"x".repeat(80)).len(), 63);
    }

    #[test]
    fn replication_false_spellings() {
        for value in ["false", "OFF", "no", "0"] { assert!(is_false(value)); }
        assert!(!is_false("true"));
    }
}
