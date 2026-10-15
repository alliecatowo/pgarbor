//! SCRAM-SHA-256 (RFC 5802 / RFC 7677 / PG `libpq/auth-scram.c`, `common/scram-common.c`) (lease WIR-09).
//! Pure computation: no sockets. `auth.rs` moves the strings through SASL messages.
//!
//! ## Verifier (what the catalog stores in `RoleAuth::password_verifier`)
//! `SCRAM-SHA-256$<iterations>:<salt_b64>$<StoredKey_b64>:<ServerKey_b64>` (standard base64 with
//! padding), where, for password `P` (SASLprep'd; PGArbor applies it only for ASCII, i.e. verbatim),
//! ```text
//! SaltedPassword = PBKDF2-HMAC-SHA256(P, salt, iterations, 32)
//! ClientKey      = HMAC-SHA256(SaltedPassword, "Client Key")
//! StoredKey      = SHA256(ClientKey)
//! ServerKey      = HMAC-SHA256(SaltedPassword, "Server Key")
//! ```
//! Default iterations 4096 (`scram_iterations`), salt 16 random bytes.
//!
//! ## Server exchange ([`ScramServer`])
//! 1. client-first-message (from SASLInitialResponse): `n,,n=<ignored>,r=<client nonce>` (gs2 header
//!    `n,,` or `y,,`; `p=...` channel binding is refused: `28000`-class failure below).
//!    PG ignores the username in `n=` (uses the startup user). Optional extensions `m=` are
//!    refused. Parse errors: `malformed SCRAM message` (SQLSTATE `08P01`) with DETAIL variants:
//!    `The message is empty.`, `Invalid SCRAM message prefix... ` - use the PG texts in
//!    `read_attr`/`read_client_first_message` of `auth-scram.c`.
//! 2. server-first-message: `r=<client nonce><server nonce>,s=<salt_b64>,i=<iterations>`; server nonce =
//!    18 random bytes base64-encoded (24 chars).
//! 3. client-final-message: `c=<b64 of gs2 header ("biws" for "n,,")>,r=<full nonce>,p=<ClientProof_b64>`.
//!    Verify: nonce equals the full nonce; channel binding attr equals the base64 of the gs2 header;
//!    `AuthMessage = client-first-bare + "," + server-first + "," + client-final-without-proof`;
//!    `ClientSignature = HMAC(StoredKey, AuthMessage)`; `ClientKey = ClientProof XOR ClientSignature`;
//!    accept iff `SHA256(ClientKey) == StoredKey` (constant-time compare).
//! 4. server-final-message: `v=<b64 ServerSignature>` with `ServerSignature = HMAC(ServerKey, AuthMessage)`.
//!
//! ## Mock exchange
//! For an unknown role or a role without a SCRAM verifier PG runs a mock exchange so timing and
//! messages do not reveal whether the role exists: [`mock_verifier`] derives a deterministic fake
//! verifier from `(mock_secret, user)` (salt = first 16 bytes of `SHA256(mock_secret || user)`,
//! iterations 4096, random-looking keys); the exchange proceeds and ALWAYS fails at client-final
//! ([`ScramOutcome::Failed`]).
//!
//! ## Client side ([`ScramClient`])
//! Needed by `client.rs` and tests: builds client-first, consumes server-first, produces client-final,
//! checks server-final.
use pgarbor_common::{PgError, Result};

/// Default PBKDF2 iteration count.
pub const DEFAULT_ITERATIONS: u32 = 4096;

/// Parsed `SCRAM-SHA-256$...` verifier.
// CONTRACT
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ScramVerifier {
    pub iterations: u32,
    pub salt: Vec<u8>,
    pub stored_key: [u8; 32],
    pub server_key: [u8; 32],
}

/// Parse a stored verifier string (module docs). `None` when the string is not a well-formed
/// SCRAM-SHA-256 verifier (wrong prefix, missing parts, bad base64, keys not 32 bytes,
/// iterations 0 or > 10_000_000).
#[allow(unused_variables)]
pub fn parse_verifier(s: &str) -> Result<Option<ScramVerifier>> {
    Err(PgError::unimplemented("WIR-09", "parse a SCRAM verifier (scram::parse_verifier)"))
}

/// Build a verifier string for `password` (used by tests, the client helper and `CREATE ROLE ...
/// PASSWORD` tooling): module docs formulas with the given salt and iteration count.
#[allow(unused_variables)]
pub fn make_verifier(password: &str, salt: &[u8], iterations: u32) -> Result<String> {
    Err(PgError::unimplemented("WIR-09", "build a SCRAM verifier (scram::make_verifier)"))
}

/// Deterministic fake verifier for the mock exchange (module docs).
#[allow(unused_variables)]
pub fn mock_verifier(mock_secret: &[u8; 32], user: &str) -> Result<ScramVerifier> {
    Err(PgError::unimplemented("WIR-09", "mock SCRAM verifier (scram::mock_verifier)"))
}

/// Result of the final step.
// CONTRACT
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ScramOutcome {
    /// Proof valid; send `server_final` (`v=...`) in AuthenticationSASLFinal.
    Authenticated { server_final: String },
    /// Proof invalid (or mock exchange): fail with `28P01 password authentication failed for user`.
    Failed,
}

/// Server side state machine (module docs). One instance per authentication attempt.
#[derive(Debug)]
pub struct ScramServer {
    pub(crate) verifier: ScramVerifier,
    pub(crate) mock: bool,
    pub(crate) client_first_bare: String,
    pub(crate) server_first: String,
    pub(crate) nonce: String,
    pub(crate) gs2_header: String,
}

impl ScramServer {
    /// `mock = true` forces `Failed` at the end.
    pub fn new(verifier: ScramVerifier, mock: bool) -> ScramServer {
        ScramServer {
            verifier,
            mock,
            client_first_bare: String::new(),
            server_first: String::new(),
            nonce: String::new(),
            gs2_header: String::new(),
        }
    }

    /// Consume the client-first-message, return the server-first-message (module docs step 1-2).
    /// Errors: `08P01 malformed SCRAM message` (+ DETAIL texts from `auth-scram.c`); a gs2 header
    /// starting with `p=` (channel binding requested; PGArbor offers no -PLUS mechanism) is
    /// `08P01 SCRAM channel binding is not supported`.
    #[allow(unused_variables)]
    pub fn client_first(&mut self, message: &str) -> Result<String> {
        Err(PgError::unimplemented("WIR-09", "SCRAM client-first (ScramServer::client_first)"))
    }

    /// Consume the client-final-message, verify, return the outcome (module docs step 3-4).
    /// Protocol errors (bad nonce, missing attributes) are `08P01 malformed SCRAM message`; a wrong
    /// proof is `Ok(ScramOutcome::Failed)` (NOT an error) so the caller reports 28P01 uniformly.
    #[allow(unused_variables)]
    pub fn client_final(&mut self, message: &str) -> Result<ScramOutcome> {
        Err(PgError::unimplemented("WIR-09", "SCRAM client-final (ScramServer::client_final)"))
    }
}

/// Client side state machine (module docs).
#[derive(Debug)]
pub struct ScramClient {
    pub(crate) password: String,
    pub(crate) client_nonce: String,
    pub(crate) client_first_bare: String,
    pub(crate) auth_message: String,
    pub(crate) server_key_sig: Vec<u8>,
}

impl ScramClient {
    pub fn new(password: &str) -> ScramClient {
        ScramClient {
            password: password.to_string(),
            client_nonce: String::new(),
            client_first_bare: String::new(),
            auth_message: String::new(),
            server_key_sig: Vec::new(),
        }
    }
    /// `n,,n=,r=<18 random bytes base64>` (empty user name, as libpq sends).
    pub fn client_first(&mut self) -> Result<String> {
        Err(PgError::unimplemented("WIR-09", "SCRAM client first message (ScramClient::client_first)"))
    }
    /// Consume server-first, return client-final (with proof).
    #[allow(unused_variables)]
    pub fn client_final(&mut self, server_first: &str) -> Result<String> {
        Err(PgError::unimplemented("WIR-09", "SCRAM client final message (ScramClient::client_final)"))
    }
    /// Check server-final `v=...`. `Ok(())` when the signature matches.
    #[allow(unused_variables)]
    pub fn verify_server_final(&self, server_final: &str) -> Result<()> {
        Err(PgError::unimplemented("WIR-09", "SCRAM server signature check (ScramClient::verify_server_final)"))
    }
}
