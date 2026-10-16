//! Role password hashing for CREATE/ALTER ROLE (PG `encrypt_password` with `password_encryption =
//! scram-sha-256`): plaintext becomes a `SCRAM-SHA-256$<iter>:<salt>$<StoredKey>:<ServerKey>` verifier,
//! already-hashed verifiers are stored as given, the empty string stays empty (the catalog clears it).
use base64::engine::general_purpose::STANDARD;
use base64::Engine;
use hmac::{Hmac, Mac};
use sha2::{Digest, Sha256};

const ITERATIONS: u32 = 4096;
const SALT_LEN: usize = 16;

fn hmac_sha256(key: &[u8], data: &[u8]) -> [u8; 32] {
    match Hmac::<Sha256>::new_from_slice(key) {
        Ok(mut mac) => {
            mac.update(data);
            mac.finalize().into_bytes().into()
        }
        Err(_) => [0; 32],
    }
}

fn is_verifier(p: &str) -> bool {
    p.starts_with("SCRAM-SHA-256$")
        || (p.len() == 35 && p.starts_with("md5") && p.as_bytes()[3..].iter().all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(b)))
}

/// Build a SCRAM-SHA-256 verifier for `password` with the given salt.
fn scram_verifier(password: &str, salt: &[u8]) -> String {
    let mut salted = [0u8; 32];
    pbkdf2::pbkdf2_hmac::<Sha256>(password.as_bytes(), salt, ITERATIONS, &mut salted);
    let client_key = hmac_sha256(&salted, b"Client Key");
    let stored_key: [u8; 32] = Sha256::digest(client_key).into();
    let server_key = hmac_sha256(&salted, b"Server Key");
    format!("SCRAM-SHA-256${}:{}${}:{}", ITERATIONS, STANDARD.encode(salt), STANDARD.encode(stored_key), STANDARD.encode(server_key))
}

/// Hash a plaintext password for storage; verifiers and the empty string pass through unchanged.
pub fn encrypt_password(password: &str) -> String {
    if password.is_empty() || is_verifier(password) {
        return password.to_string();
    }
    let mut salt = [0u8; SALT_LEN];
    rand::fill(&mut salt);
    scram_verifier(password, &salt)
}

/// Hash a plaintext password for `role` honoring `password_encryption` (`md5` gives
/// `md5` + md5(password || role) and queues PG's deprecation WARNING; anything else is SCRAM).
pub fn encrypt_password_for(se: &crate::context::SessionEnv<'_>, password: &str, role: &str) -> String {
    if password.is_empty() || is_verifier(password) {
        return password.to_string();
    }
    let md5_mode = se.guc.get("password_encryption").is_some_and(|v| v.eq_ignore_ascii_case("md5"));
    if md5_mode {
        let mut data = password.as_bytes().to_vec();
        data.extend_from_slice(role.as_bytes());
        if let Ok(digest) = pgarbor_types::crypto::md5(&data) {
            se.notice(
                pgarbor_common::PgError::new(pgarbor_common::SqlState(*b"01P01"), "setting an MD5-encrypted password")
                    .with_severity(pgarbor_common::Severity::Warning)
                    .with_detail("MD5 password support is deprecated and will be removed in a future release of PostgreSQL.")
                    .with_hint("Refer to the PostgreSQL documentation for details about migrating to another password type."),
            );
            let hex: String = digest.iter().map(|b| format!("{b:02x}")).collect();
            return format!("md5{hex}");
        }
    }
    encrypt_password(password)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plaintext_becomes_scram_and_verifiers_pass_through() {
        let h = encrypt_password("hu8jmn3");
        assert!(h.starts_with("SCRAM-SHA-256$4096:"));
        assert_eq!(encrypt_password(&h), h);
        assert_eq!(encrypt_password(""), "");
        let md5 = format!("md5{}", "a".repeat(32));
        assert_eq!(encrypt_password(&md5), md5);
    }
}
