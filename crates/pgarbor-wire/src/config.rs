//! Server configuration (frozen, v1). Fully implemented.
//!
//! ## Authentication policy ([`AuthPolicy`])
//! | policy | behavior |
//! |---|---|
//! | `Auto` (default) | per connection: Unix-socket and loopback peers whose role has NO password verifier are trusted; a role WITH a verifier is challenged using the verifier's type (`SCRAM-SHA-256$..` -> SCRAM, `md5..` -> MD5); a non-loopback peer without a verifier is refused (`28000`, no trust over the network) |
//! | `Trust` | no password check for anyone who can reach the listener; `validate` refuses non-loopback listeners unless `allow_nonloopback` |
//! | `Password` | AuthenticationCleartextPassword for every role (verifier may be plain `md5..` or SCRAM; the password is checked against whatever is stored) |
//! | `Md5` | md5 challenge; roles with a SCRAM verifier are challenged with SCRAM instead (PG behavior) |
//! | `ScramSha256` | SCRAM-SHA-256 for every role; a role with an md5 verifier fails with `28P01` |
//!
//! ## TLS
//! TLS is explicitly NOT implemented yet (WIRE-13): an SSLRequest is answered with the single byte
//! `N` (clients with `sslmode=prefer` then continue in plaintext, `sslmode=require` fails
//! client-side). `TlsPolicy::Disabled` is the only usable policy; `TlsPolicy::Required` makes
//! [`ServerConfig::validate`] fail with `0A000 TLS is not supported yet`. Because there is no TLS,
//! non-loopback binds need the explicit `allow_nonloopback` opt-in and a password policy.
use std::path::PathBuf;
use std::time::Duration;

use pgarbor_common::{PgError, Result, SqlState};

use crate::consts::{DEFAULT_IDLE_POLL_MS, DEFAULT_MAX_FRAME_BYTES};
use crate::transcript::TranscriptHandle;

/// Where to listen.
// CONTRACT
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ListenAddr {
    /// `host:port`; `port == 0` = pick a free port (real port via `Server::tcp_addr`).
    Tcp { host: String, port: u16 },
    /// Unix-domain socket file `<dir>/.s.PGSQL.<port>` (libpq convention, so `psql -h <dir> -p <port>` works).
    Unix { dir: PathBuf, port: u16 },
}

/// Authentication policy (module docs).
// CONTRACT
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum AuthPolicy {
    #[default]
    Auto,
    Trust,
    Password,
    Md5,
    ScramSha256,
}

impl AuthPolicy {
    /// Parse the CLI spelling: `auto`, `trust`, `password`, `md5`, `scram-sha-256` (also `scram`).
    pub fn parse(s: &str) -> Option<AuthPolicy> {
        match s.to_ascii_lowercase().as_str() {
            "auto" => Some(AuthPolicy::Auto),
            "trust" => Some(AuthPolicy::Trust),
            "password" | "cleartext" => Some(AuthPolicy::Password),
            "md5" => Some(AuthPolicy::Md5),
            "scram-sha-256" | "scram" => Some(AuthPolicy::ScramSha256),
            _ => None,
        }
    }
    pub fn as_str(self) -> &'static str {
        match self {
            AuthPolicy::Auto => "auto",
            AuthPolicy::Trust => "trust",
            AuthPolicy::Password => "password",
            AuthPolicy::Md5 => "md5",
            AuthPolicy::ScramSha256 => "scram-sha-256",
        }
    }
}

/// TLS policy (module docs): only `Disabled` works today.
// CONTRACT
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum TlsPolicy {
    /// Answer SSLRequest/GSSENCRequest with `N`.
    #[default]
    Disabled,
    /// Reserved for WIRE-13: rejected by `validate`.
    Required,
}

/// Everything a [`crate::server::Server`] needs.
// CONTRACT
#[derive(Clone, Debug)]
pub struct ServerConfig {
    /// The branch every connection of this listener is bound to (RFC 001 section 7).
    pub branch: String,
    pub listen: Vec<ListenAddr>,
    pub auth: AuthPolicy,
    pub tls: TlsPolicy,
    /// Maximum simultaneously established sessions (PG `max_connections`). Further connections get
    /// FATAL `53300 sorry, too many clients already`. Default 100.
    pub max_connections: usize,
    /// Time allowed for startup + authentication (PG `authentication_timeout`, 60 s): on expiry the
    /// connection is closed after FATAL `57014 canceling authentication due to timeout`.
    pub startup_timeout: Duration,
    /// Idle poll interval of connection threads (stop flag, NOTIFY delivery).
    pub idle_poll: Duration,
    /// Cap for one frontend message body.
    pub max_frame_bytes: usize,
    /// Mode bits of created Unix socket files (default 0o700: owner only).
    pub unix_socket_mode: u32,
    /// GUCs applied to every session as startup defaults BEFORE the client's own `-c` options
    /// (e.g. `statement_timeout`).
    pub default_gucs: Vec<(String, String)>,
    /// Tee of all messages for WIRE-12 diagnostics.
    pub transcript: Option<TranscriptHandle>,
    /// How long `shutdown` waits for sessions to end before closing their sockets.
    pub shutdown_grace: Duration,
    /// Allow binding non-loopback TCP addresses (opt-in, needs a password policy).
    pub allow_nonloopback: bool,
    pub tcp_nodelay: bool,
}

impl ServerConfig {
    /// Loopback TCP on an automatically chosen port, default policies.
    pub fn loopback(branch: &str) -> ServerConfig {
        ServerConfig {
            branch: branch.to_string(),
            listen: vec![ListenAddr::Tcp { host: "127.0.0.1".to_string(), port: 0 }],
            auth: AuthPolicy::Auto,
            tls: TlsPolicy::Disabled,
            max_connections: 100,
            startup_timeout: Duration::from_secs(60),
            idle_poll: Duration::from_millis(DEFAULT_IDLE_POLL_MS),
            max_frame_bytes: DEFAULT_MAX_FRAME_BYTES,
            unix_socket_mode: 0o700,
            default_gucs: Vec::new(),
            transcript: None,
            shutdown_grace: Duration::from_secs(5),
            allow_nonloopback: false,
            tcp_nodelay: true,
        }
    }

    /// Replace the listener list with one TCP listener.
    pub fn with_tcp(mut self, host: &str, port: u16) -> ServerConfig {
        self.listen = vec![ListenAddr::Tcp { host: host.to_string(), port }];
        self
    }

    /// Add a Unix-socket listener `<dir>/.s.PGSQL.<port>`.
    pub fn with_unix(mut self, dir: impl Into<PathBuf>, port: u16) -> ServerConfig {
        self.listen.push(ListenAddr::Unix { dir: dir.into(), port });
        self
    }

    /// Reject unsafe or unsupported combinations before binding anything.
    /// * no listeners: `22023 no listen address configured`;
    /// * `max_connections == 0`: `22023 max_connections must be positive`;
    /// * `TlsPolicy::Required`: `0A000 TLS is not supported yet` (HINT: use a loopback or Unix
    ///   socket listener);
    /// * non-loopback TCP host without `allow_nonloopback`: `42501 refusing to listen on non-loopback
    ///   address "H"` (HINT: pass allow_nonloopback with a password auth policy);
    /// * non-loopback TCP host with `allow_nonloopback` and `AuthPolicy::Trust`: `42501 refusing
    ///   trust authentication on a non-loopback address`.
    pub fn validate(&self) -> Result<()> {
        if self.listen.is_empty() {
            return Err(PgError::new(SqlState::INVALID_PARAMETER_VALUE, "no listen address configured"));
        }
        if self.max_connections == 0 {
            return Err(PgError::new(SqlState::INVALID_PARAMETER_VALUE, "max_connections must be positive"));
        }
        if self.tls == TlsPolicy::Required {
            return Err(PgError::new(SqlState::FEATURE_NOT_SUPPORTED, "TLS is not supported yet")
                .with_hint("use a loopback TCP or Unix-domain socket listener"));
        }
        for l in &self.listen {
            if let ListenAddr::Tcp { host, .. } = l {
                if !is_loopback_host(host) {
                    if !self.allow_nonloopback {
                        return Err(PgError::new(
                            SqlState::INSUFFICIENT_PRIVILEGE,
                            format!("refusing to listen on non-loopback address \"{host}\""),
                        )
                        .with_hint("pass allow_nonloopback together with a password authentication policy"));
                    }
                    if self.auth == AuthPolicy::Trust {
                        return Err(PgError::new(
                            SqlState::INSUFFICIENT_PRIVILEGE,
                            "refusing trust authentication on a non-loopback address",
                        ));
                    }
                }
            }
        }
        Ok(())
    }
}

/// `localhost`, `127.0.0.0/8`, `::1` are loopback; everything else (including `0.0.0.0`, `::`) is not.
pub fn is_loopback_host(host: &str) -> bool {
    let h = host.trim_start_matches('[').trim_end_matches(']');
    if h.eq_ignore_ascii_case("localhost") {
        return true;
    }
    match h.parse::<std::net::IpAddr>() {
        Ok(ip) => ip.is_loopback(),
        Err(_) => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_validate_and_nonloopback_is_refused() {
        assert!(ServerConfig::loopback("main").validate().is_ok());
        let c = ServerConfig::loopback("main").with_tcp("0.0.0.0", 5432);
        assert_eq!(c.validate().unwrap_err().sqlstate.code(), "42501");
        let mut c = ServerConfig::loopback("main").with_tcp("0.0.0.0", 5432);
        c.allow_nonloopback = true;
        c.auth = AuthPolicy::Trust;
        assert!(c.validate().is_err());
        c.auth = AuthPolicy::ScramSha256;
        assert!(c.validate().is_ok());
    }

    #[test]
    fn auth_policy_parse() {
        assert_eq!(AuthPolicy::parse("SCRAM-SHA-256"), Some(AuthPolicy::ScramSha256));
        assert_eq!(AuthPolicy::parse("nope"), None);
    }
}
