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
use pgarbor_common::{PgError, Result};

use crate::config::AuthPolicy;
use crate::conn_io::ConnIo;
use crate::error::{WireError, WireResult};
use crate::shared::Shared;
use crate::startup_flow::StartupInfo;
use crate::transport::PeerInfo;

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
#[allow(unused_variables)]
pub fn choose_method(
    policy: AuthPolicy,
    role: Option<&RoleAuth>,
    peer: &PeerInfo,
    user: &str,
    database: &str,
) -> Result<AuthMethod> {
    Err(PgError::unimplemented("WIR-10", "choose the authentication method (auth::choose_method)"))
}

/// `"md5" + hex(md5(password || user))` - the legacy stored form.
#[allow(unused_variables)]
pub fn md5_verifier(password: &str, user: &str) -> Result<String> {
    Err(PgError::unimplemented("WIR-10", "legacy md5 verifier (auth::md5_verifier)"))
}

/// The md5 response a client must send: `"md5" + hex(md5(hex_inner || salt))`, where `hex_inner` is the
/// 32 hex characters after the `md5` prefix of the stored verifier.
#[allow(unused_variables)]
pub fn md5_response(stored_verifier: &str, salt: &[u8; 4]) -> Result<String> {
    Err(PgError::unimplemented("WIR-10", "md5 challenge response (auth::md5_response)"))
}

/// Verify a plaintext password against a stored verifier (md5 or SCRAM) for `AuthMethod::Cleartext`.
#[allow(unused_variables)]
pub fn verify_plain_password(stored_verifier: &str, user: &str, password: &str) -> Result<bool> {
    Err(PgError::unimplemented("WIR-10", "plain password check (auth::verify_plain_password)"))
}

/// Run the exchange (module docs). `deadline` bounds every password read.
#[allow(unused_variables)]
pub fn authenticate(
    io: &mut ConnIo,
    shared: &Shared,
    startup: &StartupInfo,
    deadline: std::time::Instant,
) -> WireResult<()> {
    Err(WireError::Pg(PgError::unimplemented("WIR-10", "authentication exchange (auth::authenticate)")))
}
