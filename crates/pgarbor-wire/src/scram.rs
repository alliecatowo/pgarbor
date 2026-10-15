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
use base64::engine::general_purpose::STANDARD;
use base64::Engine;
use hmac::{Hmac, Mac};
use sha2::{Digest, Sha256};
type HmacSha256 = Hmac<Sha256>;

fn malformed(detail: &str) -> PgError {
    PgError::new(pgarbor_common::SqlState::PROTOCOL_VIOLATION, "malformed SCRAM message").with_detail(detail)
}
fn hmac(key: &[u8], data: &[u8]) -> [u8; 32] {
    // HMAC accepts keys of every length; the error branch is unreachable for this algorithm.
    let mut mac = match HmacSha256::new_from_slice(key) { Ok(m) => m, Err(_) => return [0; 32] };
    mac.update(data); mac.finalize().into_bytes().into()
}
fn salted(password: &str, salt: &[u8], iterations: u32) -> [u8; 32] {
    let mut out = [0u8; 32]; pbkdf2::pbkdf2_hmac::<Sha256>(password.as_bytes(), salt, iterations, &mut out); out
}
fn verifier_for(password: &str, salt: &[u8], iterations: u32) -> ScramVerifier {
    let sp = salted(password, salt, iterations); let ck = hmac(&sp, b"Client Key");
    ScramVerifier { iterations, salt: salt.to_vec(), stored_key: Sha256::digest(ck).into(), server_key: hmac(&sp, b"Server Key") }
}
fn attrs(s: &str) -> std::result::Result<Vec<(char, &str)>, ()> {
    // Authentication messages are already framed by the wire layer; keep the pure helper
    // bounded too, so callers cannot make attribute parsing allocate without limit.
    if s.is_empty() || s.len() > 1024 * 1024 { return Err(()); }
    let mut out = Vec::new();
    for part in s.split(',') {
        let b = part.as_bytes(); if b.len() < 2 || b[1] != b'=' || !b[0].is_ascii() { return Err(()); }
        let key = b[0] as char; if out.iter().any(|(k, _)| *k == key) { return Err(()); }
        out.push((key, &part[2..]));
    }
    Ok(out)
}
fn attr<'a>(items: &'a [(char, &'a str)], key: char) -> std::result::Result<&'a str, ()> {
    items.iter().find(|(k, _)| *k == key).map(|(_, v)| *v).ok_or(())
}
fn equal(a: &[u8], b: &[u8]) -> bool { a.len() == b.len() && a.iter().zip(b).fold(0u8, |v, (x,y)| v | (x ^ y)) == 0 }


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
pub fn parse_verifier(s: &str) -> Result<Option<ScramVerifier>> {
    if s.len() > 1024 * 1024 { return Ok(None); }
    let Some(rest) = s.strip_prefix("SCRAM-SHA-256$") else { return Ok(None) };
    let Some((params, keys)) = rest.split_once('$') else { return Ok(None) };
    let Some((iterations, salt)) = params.split_once(':') else { return Ok(None) };
    let Some((stored, server)) = keys.split_once(':') else { return Ok(None) };
    let Ok(iterations) = iterations.parse::<u32>() else { return Ok(None) };
    if iterations == 0 || iterations > 10_000_000 { return Ok(None); }
    let (Ok(salt), Ok(stored), Ok(server)) = (STANDARD.decode(salt), STANDARD.decode(stored), STANDARD.decode(server)) else { return Ok(None) };
    if salt.is_empty() || salt.len() > 1024 || stored.len() != 32 || server.len() != 32 { return Ok(None); }
    let mut stored_key = [0;32]; let mut server_key = [0;32]; stored_key.copy_from_slice(&stored); server_key.copy_from_slice(&server);
    Ok(Some(ScramVerifier { iterations, salt, stored_key, server_key }))
}

/// Build a verifier string for `password` (used by tests, the client helper and `CREATE ROLE ...
/// PASSWORD` tooling): module docs formulas with the given salt and iteration count.
pub fn make_verifier(password: &str, salt: &[u8], iterations: u32) -> Result<String> {
    if iterations == 0 || iterations > 10_000_000 { return Err(PgError::new(pgarbor_common::SqlState::INVALID_PARAMETER_VALUE, "SCRAM iteration count must be between 1 and 10000000")); }
    let v = verifier_for(password, salt, iterations);
    Ok(format!("SCRAM-SHA-256${}:{}${}:{}", iterations, STANDARD.encode(&v.salt), STANDARD.encode(v.stored_key), STANDARD.encode(v.server_key)))
}

/// Deterministic fake verifier for the mock exchange (module docs).
pub fn mock_verifier(mock_secret: &[u8; 32], user: &str) -> Result<ScramVerifier> {
    let mut hash = Sha256::new(); hash.update(mock_secret); hash.update(user.as_bytes()); let digest = hash.finalize();
    let mut salt = [0;16]; salt.copy_from_slice(&digest[..16]);
    Ok(ScramVerifier { iterations: DEFAULT_ITERATIONS, salt: salt.to_vec(), stored_key: hmac(mock_secret, user.as_bytes()), server_key: hmac(mock_secret, b"mock SCRAM server key") })
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
    pub fn client_first(&mut self, message: &str) -> Result<String> {
        let (header, bare) = if let Some(v) = message.strip_prefix("n,,") { ("n,,", v) } else if let Some(v) = message.strip_prefix("y,,") { ("y,,", v) } else if message.starts_with("p=") { return Err(PgError::new(pgarbor_common::SqlState::PROTOCOL_VIOLATION, "SCRAM channel binding is not supported")); } else { return Err(malformed("Invalid SCRAM message prefix.")); };
        let a = attrs(bare).map_err(|_| malformed("Invalid SCRAM message."))?;
        if a.first().map(|x| x.0) == Some('m') { return Err(malformed("SCRAM messages with extensions are not supported.")); }
        let _username = attr(&a, 'n').map_err(|_| malformed("Attribute \"n\" expected."))?;
        let nonce = attr(&a, 'r').map_err(|_| malformed("Attribute \"r\" expected."))?;
        if nonce.is_empty() || nonce.contains(',') { return Err(malformed("Invalid SCRAM nonce.")); }
        let mut random = [0u8;18]; rand::fill(&mut random); let server_nonce = STANDARD.encode(random);
        self.gs2_header = header.to_string(); self.client_first_bare = bare.to_string(); self.nonce = format!("{nonce}{server_nonce}");
        self.server_first = format!("r={},s={},i={}", self.nonce, STANDARD.encode(&self.verifier.salt), self.verifier.iterations);
        Ok(self.server_first.clone())
    }

    /// Consume the client-final-message, verify, return the outcome (module docs step 3-4).
    /// Protocol errors (bad nonce, missing attributes) are `08P01 malformed SCRAM message`; a wrong
    /// proof is `Ok(ScramOutcome::Failed)` (NOT an error) so the caller reports 28P01 uniformly.
    pub fn client_final(&mut self, message: &str) -> Result<ScramOutcome> {
        let a = attrs(message).map_err(|_| malformed("Invalid SCRAM message."))?;
        if a.last().map(|x| x.0) != Some('p') { return Err(malformed("Attribute \"p\" expected.")); }
        let c = attr(&a,'c').map_err(|_| malformed("Attribute \"c\" expected."))?;
        let bind = STANDARD.decode(c).map_err(|_| malformed("Invalid channel-binding data."))?;
        if bind != self.gs2_header.as_bytes() { return Err(malformed("Channel binding mismatch.")); }
        let nonce = attr(&a,'r').map_err(|_| malformed("Attribute \"r\" expected."))?;
        if nonce != self.nonce { return Err(malformed("Nonce does not match.")); }
        let proof = STANDARD.decode(attr(&a,'p').map_err(|_| malformed("Attribute \"p\" expected."))?).map_err(|_| malformed("Invalid proof."))?;
        if proof.len() != 32 { return Err(malformed("Invalid proof.")); }
        let without = message.rsplit_once(",p=").map(|v| v.0).ok_or_else(|| malformed("Attribute \"p\" expected."))?;
        let auth = format!("{},{},{}", self.client_first_bare, self.server_first, without);
        let sig = hmac(&self.verifier.stored_key, auth.as_bytes()); let mut key = [0;32];
        for i in 0..32 { key[i] = proof[i] ^ sig[i]; }
        if self.mock || !equal(&Sha256::digest(key), &self.verifier.stored_key) { return Ok(ScramOutcome::Failed); }
        Ok(ScramOutcome::Authenticated { server_final: format!("v={}", STANDARD.encode(hmac(&self.verifier.server_key, auth.as_bytes()))) })
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
        let mut random = [0u8;18]; rand::fill(&mut random); self.client_nonce = STANDARD.encode(random);
        self.client_first_bare = format!("n=,r={}", self.client_nonce); Ok(format!("n,,{}", self.client_first_bare))
    }
    /// Consume server-first, return client-final (with proof).
    pub fn client_final(&mut self, server_first: &str) -> Result<String> {
        let a = attrs(server_first).map_err(|_| malformed("Invalid SCRAM message."))?;
        let nonce = attr(&a,'r').map_err(|_| malformed("Attribute \"r\" expected."))?;
        if !nonce.starts_with(&self.client_nonce) || nonce.len() <= self.client_nonce.len() { return Err(malformed("Nonce does not match.")); }
        let salt = STANDARD.decode(attr(&a,'s').map_err(|_| malformed("Attribute \"s\" expected."))?).map_err(|_| malformed("Invalid salt."))?;
        let iterations = attr(&a,'i').map_err(|_| malformed("Attribute \"i\" expected."))?.parse::<u32>().map_err(|_| malformed("Invalid iteration count."))?;
        if iterations == 0 || iterations > 10_000_000 { return Err(malformed("Invalid iteration count.")); }
        let sp = salted(&self.password, &salt, iterations); let ck = hmac(&sp,b"Client Key");
        let stored: [u8;32] = Sha256::digest(ck).into(); let without = format!("c={},r={}", STANDARD.encode(b"n,,"), nonce);
        self.auth_message = format!("{},{},{}", self.client_first_bare, server_first, without);
        self.server_key_sig = hmac(&hmac(&sp,b"Server Key"), self.auth_message.as_bytes()).to_vec();
        let sig = hmac(&stored,self.auth_message.as_bytes()); let mut proof = [0;32];
        for i in 0..32 { proof[i] = ck[i] ^ sig[i]; }
        Ok(format!("{},p={}", without, STANDARD.encode(proof)))
    }
    /// Check server-final `v=...`. `Ok(())` when the signature matches.
    pub fn verify_server_final(&self, server_final: &str) -> Result<()> {
        let a = attrs(server_final).map_err(|_| malformed("Invalid SCRAM message."))?;
        let sig = STANDARD.decode(attr(&a,'v').map_err(|_| malformed("Attribute \"v\" expected."))?).map_err(|_| malformed("Invalid server signature."))?;
        if !equal(&sig, &self.server_key_sig) { return Err(malformed("Invalid server signature.")); } Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn verifier_round_trip_and_bad_inputs() {
        let salt = STANDARD.decode("W22ZaJ0SNY7soEsUEjb6gQ==").expect("RFC salt");
        let text = make_verifier("pencil", &salt, 4096).expect("valid verifier input");
        let parsed = parse_verifier(&text).expect("parse result").expect("verifier");
        assert_eq!(parsed.iterations, 4096);
        assert_eq!(parsed.salt, salt);
        assert!(parse_verifier("plain").expect("parse result").is_none());
        assert!(parse_verifier("SCRAM-SHA-256$0:YQ==$YQ==:YQ==").expect("parse result").is_none());
    }

    #[test]
    fn rfc_7677_client_vector() {
        // RFC 7677 section 3, using its fixed client/server nonces and salt.
        let mut client = ScramClient::new("pencil");
        client.client_nonce = "rOprNGfwEbeRWgbNEkqO".to_string();
        client.client_first_bare = "n=user,r=rOprNGfwEbeRWgbNEkqO".to_string();
        let final_msg = client.client_final(
            "r=rOprNGfwEbeRWgbNEkqO%hvYDpWUa2RaTCAfuxFIlj)hNlF$k0,s=W22ZaJ0SNY7soEsUEjb6gQ==,i=4096",
        ).expect("RFC client final");
        assert_eq!(final_msg,
            "c=biws,r=rOprNGfwEbeRWgbNEkqO%hvYDpWUa2RaTCAfuxFIlj)hNlF$k0,p=dHzbZapWIk4jUhN+Ute9ytag9zjfMHgsqmmiz7AndVQ=");
        client.verify_server_final("v=6rriTRBi23WpRR/wtup+mMhUZUn/dB5nLTJRsjl95G4=")
            .expect("RFC server signature");
    }

    #[test]
    fn client_server_exchange_and_wrong_password() {
        let verifier = verifier_for("pencil", b"salt", 4096);
        let mut client = ScramClient::new("pencil");
        let first = client.client_first().expect("first");
        let mut server = ScramServer::new(verifier.clone(), false);
        let challenge = server.client_first(&first).expect("challenge");
        let final_msg = client.client_final(&challenge).expect("client final");
        let outcome = server.client_final(&final_msg).expect("server final");
        let ScramOutcome::Authenticated { server_final } = outcome else { panic!("valid proof rejected") };
        client.verify_server_final(&server_final).expect("signature");

        let mut wrong = ScramClient::new("wrong");
        let first = wrong.client_first().expect("first");
        let mut server = ScramServer::new(verifier, false);
        let challenge = server.client_first(&first).expect("challenge");
        let final_msg = wrong.client_final(&challenge).expect("client final");
        assert_eq!(server.client_final(&final_msg).expect("exchange"), ScramOutcome::Failed);
    }

    #[test]
    fn mock_is_deterministic_and_doomed() {
        let secret = [7u8;32];
        let one = mock_verifier(&secret, "unknown").expect("mock");
        assert_eq!(one, mock_verifier(&secret, "unknown").expect("mock"));
        let mut client = ScramClient::new("anything");
        let first = client.client_first().expect("first");
        let mut server = ScramServer::new(one, true);
        let challenge = server.client_first(&first).expect("challenge");
        let final_msg = client.client_final(&challenge).expect("client final");
        assert_eq!(server.client_final(&final_msg).expect("exchange"), ScramOutcome::Failed);
    }
}
