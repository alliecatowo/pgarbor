//! Authentication (lease WIR-10). PG: `libpq/auth.c` (`ClientAuthentication`, `CheckPWChallengeAuth`,
//! `CheckPasswordAuth`, `CheckMD5Auth`), `libpq/auth-scram.c`, `libpq/crypt.c`
//! (`get_role_password`, `md5_crypt_verify`, `plain_crypt_verify`).
//!
//! [`authenticate`] runs the whole authentication exchange for one connection and, on success,
//! sends `AuthenticationOk` (flushed). Nothing else is sent: ParameterStatus/BackendKeyData/ReadyForQuery
//! come later from `connection.rs`.
//!
//! ## Method selection ([`choose_method`])
//! Looks up `Database::role_auth(branch, user)` (`Ok(None)` = unknown role; an `Err` is sent as FATAL).
//! With `peer.is_local()` = Unix socket or loopback TCP:
//! | policy | unknown role | role without verifier | role with verifier |
//! |---|---|---|---|
//! | `Trust` | `Trust` | `Trust` | `Trust` |
//! | `Auto` | `Trust` | `Trust` | by verifier type: `SCRAM-SHA-256$` -> `Scram`, `md5` + 32 hex -> `Md5` |
//! | `Password` | `Cleartext` | `Cleartext` | `Cleartext` |
//! | `Md5` | `Md5` | `Md5` | SCRAM verifier -> `Scram`, else `Md5` |
//! | `ScramSha256` | `Scram` | `Scram` | `Scram` (md5 verifier -> challenge runs, then fails 28P01) |
//! `Trust` for an unknown role does not fail here: the session connect reports `28000 role "x" does
//! not exist` (PG does the same under trust). Non-local peers: `Trust`/`Auto`-without-verifier are
//! refused with FATAL `28000` `no pg_hba.conf entry for host "<ip>", user "<user>", database "<db>",
//! no encryption`.
//!
//! ## Exchanges
//! * `Trust`: send AuthenticationOk.
//! * `Cleartext`: send `AuthenticationCleartextPassword`, flush, read ONE `p` frame (`io.poll_raw`,
//!   timeout = remaining startup time), `frontend::parse_password`. Compare with the stored verifier:
//!   md5 verifier -> `md5_verifier(password, user) == stored`; SCRAM verifier -> recompute
//!   StoredKey via PBKDF2 and compare in constant time (`scram::make_verifier` with the stored salt and
//!   iterations, compare the `StoredKey`/`ServerKey` parts). No verifier -> failure.
//! * `Md5`: random 4-byte salt (`rand::fill`), send `AuthenticationMD5Password{salt}`, read the `p`
//!   frame (`md5` + 32 hex + NUL), expected = `"md5" + hex(md5(hex_part_of_stored_md5_verifier + salt))`
//!   where the stored verifier is `"md5" + hex(md5(password + user))`. Constant-time compare.
//! * `Scram`: send `AuthenticationSasl{["SCRAM-SHA-256"]}`, read the `p` frame as SASLInitialResponse
//!   (`frontend::parse_sasl_initial`): unknown mechanism -> FATAL `28000` `client selected an invalid
//!   SASL authentication mechanism`; `ScramServer::client_first` -> send
//!   `AuthenticationSaslContinue{server_first}`; read the next `p` frame (SASLResponse = raw body, must
//!   be UTF-8) -> `client_final`; `Authenticated{server_final}` -> send
//!   `AuthenticationSaslFinal{server_final}` then `AuthenticationOk`. For unknown roles or roles without a
//!   SCRAM verifier use `scram::mock_verifier(&shared.mock_secret, user)` with `mock = true`.
//! * every challenge is flushed before the blocking read.
//!
//! ## Failure
//! Wrong password / mock exchange / md5 verifier under SCRAM / `valid_until` in the past /
//! no verifier: FATAL `28P01` `password authentication failed for user "<user>"` (no DETAIL to the
//! client; PG logs the detail server-side only). Wrong message type where a password response is
//! expected: FATAL `08P01` `expected password response, got message type <tag as decimal>`. EOF
//! during auth: `WireError::Closed`. After success the caller (`connection.rs`) creates the session;
//! role-level checks (`can_login`, unknown role under trust) are the session's job (28000).
use pgarbor::RoleAuth;
use pgarbor_common::{PgError, Result, Severity, SqlState};

use crate::config::AuthPolicy;
use crate::backend::Backend;
use crate::conn_io::ConnIo;
use crate::error::{WireError, WireResult};
use crate::framing::{Frame, Polled};
use crate::shared::Shared;
use crate::startup_flow::StartupInfo;
use crate::transport::PeerInfo;
use md5::{Digest, Md5};

/// The challenge actually used for this connection.
// CONTRACT
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AuthMethod {
    Trust,
    Cleartext,
    Md5,
    Scram,
}

/// Pick the method (module docs table) or refuse the connection (`28000` pg_hba-style message).
pub fn choose_method(
    policy: AuthPolicy,
    role: Option<&RoleAuth>,
    peer: &PeerInfo,
    user: &str,
    database: &str,
) -> Result<AuthMethod> {
    let verifier = role.and_then(|r| r.password_verifier.as_deref());
    let scram = verifier.is_some_and(|v| v.starts_with("SCRAM-SHA-256$"));
    let md5 = verifier.is_some_and(valid_md5_verifier);
    let method = match policy {
        AuthPolicy::Trust => AuthMethod::Trust,
        AuthPolicy::Password => AuthMethod::Cleartext,
        AuthPolicy::Md5 => if scram { AuthMethod::Scram } else { AuthMethod::Md5 },
        AuthPolicy::ScramSha256 => AuthMethod::Scram,
        AuthPolicy::Auto => match (peer.is_local(), verifier) {
            (true, None) => AuthMethod::Trust,
            (true, Some(v)) if v.is_empty() => AuthMethod::Trust,
            (true, Some(_)) if scram => AuthMethod::Scram,
            (true, Some(_)) if md5 => AuthMethod::Md5,
            (true, Some(_)) => AuthMethod::Trust,
            (false, Some(v)) if scram => AuthMethod::Scram,
            (false, Some(_)) if md5 => AuthMethod::Md5,
            (false, _) => return Err(pg_hba_error(peer, user, database)),
        },
    };
    if !peer.is_local() && method == AuthMethod::Trust {
        return Err(pg_hba_error(peer, user, database));
    }
    Ok(method)
}

/// `"md5" + hex(md5(password || user))` - the legacy stored form.
pub fn md5_verifier(password: &str, user: &str) -> Result<String> {
    let mut h = Md5::new();
    h.update(password.as_bytes());
    h.update(user.as_bytes());
    Ok(format!("md5{}", hex(&h.finalize())))
}

/// The md5 response a client must send: `"md5" + hex(md5(hex_inner || salt))`, where `hex_inner` is the
/// 32 hex characters after the `md5` prefix of the stored verifier.
pub fn md5_response(stored_verifier: &str, salt: &[u8; 4]) -> Result<String> {
    if !valid_md5_verifier(stored_verifier) {
        return Err(PgError::new(SqlState::INVALID_PASSWORD, "invalid MD5 password verifier"));
    }
    let mut h = Md5::new();
    h.update(&stored_verifier.as_bytes()[3..]);
    h.update(salt);
    Ok(format!("md5{}", hex(&h.finalize())))
}

/// Verify a plaintext password against a stored verifier (md5 or SCRAM) for `AuthMethod::Cleartext`.
pub fn verify_plain_password(stored_verifier: &str, user: &str, password: &str) -> Result<bool> {
    if valid_md5_verifier(stored_verifier) {
        let candidate = md5_verifier(password, user)?;
        return Ok(constant_time_eq(candidate.as_bytes(), stored_verifier.as_bytes()));
    }
    if let Some(v) = crate::scram::parse_verifier(stored_verifier)? {
        let candidate = crate::scram::make_verifier(password, &v.salt, v.iterations)?;
        if let Some(c) = crate::scram::parse_verifier(&candidate)? {
            return Ok(constant_time_eq(&c.stored_key, &v.stored_key)
                & constant_time_eq(&c.server_key, &v.server_key));
        }
    }
    Ok(false)
}

/// Run the exchange (module docs). `deadline` bounds every password read.
pub fn authenticate(
    io: &mut ConnIo,
    shared: &Shared,
    startup: &StartupInfo,
    deadline: std::time::Instant,
) -> WireResult<()> {
    let role = match shared.db.role_auth(&shared.config.branch, &startup.user) {
        Ok(r) => r,
        Err(e) => return send_fatal(io, e),
    };
    let method = match choose_method(shared.config.auth, role.as_ref(), io.peer(), &startup.user, &startup.database) {
        Ok(m) => m,
        Err(e) => return send_fatal(io, e),
    };
    let verifier = role.as_ref().and_then(|r| r.password_verifier.as_deref());
    let expired = role.as_ref().and_then(|r| r.valid_until).is_some_and(|v| {
        const PG_EPOCH_UNIX_MICROS: i64 = 946_684_800_000_000;
        let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH)
            .map(|d| i64::try_from(d.as_micros()).unwrap_or(i64::MAX))
            .unwrap_or(0);
        v < now.saturating_sub(PG_EPOCH_UNIX_MICROS)
    });
    let mut authenticated = false;
    let result: WireResult<bool> = (|| {
        match method {
            AuthMethod::Trust => authenticated = !expired,
            AuthMethod::Cleartext => {
                io.send_flush(&Backend::AuthenticationCleartextPassword)?;
                let body = read_password(io, deadline)?;
                let password = crate::frontend::parse_password(&body).map_err(WireError::Pg)?;
                authenticated = !expired && match verifier {
                    Some(v) => verify_plain_password(v, &startup.user, &password).map_err(WireError::Pg)?,
                    None => false,
                };
            }
            AuthMethod::Md5 => {
                let mut salt = [0u8; 4];
                rand::fill(&mut salt);
                io.send_flush(&Backend::AuthenticationMd5Password { salt })?;
                let body = read_password(io, deadline)?;
                let response = crate::frontend::parse_password(&body).map_err(WireError::Pg)?;
                let expected = match verifier.filter(|v| valid_md5_verifier(v)) {
                    Some(v) => Some(md5_response(v, &salt).map_err(WireError::Pg)?),
                    None => None,
                };
                authenticated = !expired && expected.as_ref().is_some_and(|e| constant_time_eq(e.as_bytes(), response.as_bytes()));
            }
            AuthMethod::Scram => {
                io.send_flush(&Backend::AuthenticationSasl { mechanisms: vec!["SCRAM-SHA-256".to_string()] })?;
                let initial = read_password(io, deadline)?;
                let (mechanism, first) = crate::frontend::parse_sasl_initial(&initial).map_err(WireError::Pg)?;
                if mechanism != "SCRAM-SHA-256" {
                    return Err(WireError::fatal(PgError::new(SqlState::INVALID_AUTHORIZATION_SPECIFICATION, "client selected an invalid SASL authentication mechanism")));
                }
                let mock = verifier.and_then(|v| crate::scram::parse_verifier(v).ok().flatten()).is_none();
                let parsed = if mock { None } else { verifier.and_then(|v| crate::scram::parse_verifier(v).ok().flatten()) };
                let scram_verifier = match parsed {
                    Some(v) => v,
                    None => crate::scram::mock_verifier(&shared.mock_secret, &startup.user).map_err(WireError::Pg)?,
                };
                let mut server = crate::scram::ScramServer::new(scram_verifier, mock || expired);
                let first = first.ok_or_else(|| WireError::fatal(PgError::new(SqlState::PROTOCOL_VIOLATION, "malformed SCRAM message")))?;
                let first = std::str::from_utf8(&first).map_err(|_| WireError::fatal(PgError::new(SqlState::PROTOCOL_VIOLATION, "malformed SCRAM message")))?;
                let server_first = server.client_first(first).map_err(WireError::fatal)?;
                io.send_flush(&Backend::AuthenticationSaslContinue { data: server_first.into_bytes() })?;
                let final_body = read_password(io, deadline)?;
                let final_msg = std::str::from_utf8(&final_body).map_err(|_| WireError::fatal(PgError::new(SqlState::PROTOCOL_VIOLATION, "malformed SCRAM message")))?;
                match server.client_final(final_msg).map_err(WireError::fatal)? {
                    crate::scram::ScramOutcome::Authenticated { server_final } => {
                        io.send_flush(&Backend::AuthenticationSaslFinal { data: server_final.into_bytes() })?;
                        authenticated = true;
                    }
                    crate::scram::ScramOutcome::Failed => authenticated = false,
                }
            }
        }
        Ok(authenticated)
    })();
    match result {
        Err(WireError::Pg(e)) => send_fatal(io, e),
        Err(e) => Err(e),
        Ok(true) => io.send_flush(&Backend::AuthenticationOk),
        Ok(false) => send_fatal(io, password_failure(&startup.user)),
    }
}

fn valid_md5_verifier(s: &str) -> bool {
    let b = s.as_bytes();
    b.len() == 35 && &b[..3] == b"md5" && b[3..].iter().all(u8::is_ascii_hexdigit)
}

fn hex(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(bytes.len() * 2);
    for &b in bytes { out.push(HEX[(b >> 4) as usize] as char); out.push(HEX[(b & 15) as usize] as char); }
    out
}

fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    let mut diff = a.len() ^ b.len();
    let n = a.len().max(b.len());
    for i in 0..n { diff |= usize::from(*a.get(i).unwrap_or(&0) ^ *b.get(i).unwrap_or(&0)); }
    diff == 0
}

fn pg_hba_error(peer: &PeerInfo, user: &str, database: &str) -> PgError {
    let host = peer.addr.map(|a| a.ip().to_string()).unwrap_or_else(|| "local".to_string());
    PgError::new(SqlState::INVALID_AUTHORIZATION_SPECIFICATION,
        format!("no pg_hba.conf entry for host \"{host}\", user \"{user}\", database \"{database}\", no encryption"))
        .with_severity(Severity::Fatal)
}

fn password_failure(user: &str) -> PgError {
    PgError::new(SqlState::INVALID_PASSWORD, format!("password authentication failed for user \"{user}\""))
        .with_severity(Severity::Fatal)
}

fn send_fatal<T>(io: &mut ConnIo, err: PgError) -> WireResult<T> {
    let err = err.with_severity(Severity::Fatal);
    io.send_flush(&crate::error_fields::error_message(&err))?;
    Err(WireError::Pg(err))
}

fn read_password(io: &mut ConnIo, deadline: std::time::Instant) -> WireResult<Vec<u8>> {
    let timeout = deadline.checked_duration_since(std::time::Instant::now()).unwrap_or_default();
    match io.poll_raw(Some(timeout))? {
        Polled::TimedOut => Err(WireError::fatal(PgError::new(SqlState::QUERY_CANCELED, "canceling authentication due to timeout"))),
        Polled::Eof => Err(WireError::Closed),
        Polled::Ready(Frame { tag: b'p', body }) => Ok(body),
        Polled::Ready(Frame { tag, .. }) => Err(WireError::fatal(PgError::new(SqlState::PROTOCOL_VIOLATION,
            format!("expected password response, got message type {}", tag as u32)))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn md5_verifier_and_challenge_follow_postgres_formula() {
        assert_eq!(md5_verifier("secret", "postgres").unwrap(), "md5e8a48653851e28c69d0506508fb27fc5");
        let stored = md5_verifier("secret", "postgres").unwrap();
        let salt = [1, 2, 3, 4];
        let response = md5_response(&stored, &salt).unwrap();
        let mut inner = Md5::new();
        inner.update(b"secretpostgres");
        let inner = hex(&inner.finalize());
        let mut outer = Md5::new();
        outer.update(inner.as_bytes());
        outer.update(salt);
        assert_eq!(response, format!("md5{}", hex(&outer.finalize())));
        assert!(verify_plain_password(&stored, "postgres", "secret").unwrap());
        assert!(!verify_plain_password(&stored, "postgres", "wrong").unwrap());
    }

    #[test]
    fn trust_method_and_nonlocal_auto_refusal() {
        let local = PeerInfo { addr: Some("127.0.0.1:5432".parse().unwrap()), is_unix: false };
        assert_eq!(choose_method(AuthPolicy::Auto, None, &local, "u", "d").unwrap(), AuthMethod::Trust);
        let remote = PeerInfo { addr: Some("192.0.2.1:5432".parse().unwrap()), is_unix: false };
        let e = choose_method(AuthPolicy::Auto, None, &remote, "u", "d").unwrap_err();
        assert_eq!(e.sqlstate, SqlState::INVALID_AUTHORIZATION_SPECIFICATION);
        assert!(e.message.contains("no pg_hba.conf entry"));
    }
}
