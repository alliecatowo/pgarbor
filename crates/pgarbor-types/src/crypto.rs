//! Hash digests and checksums (TYPE-07, TYPE-13). Lease TYP-19.
//!
//! The crate has no crypto dependency (deps frozen), so this lease implements MD5 (RFC 1321),
//! SHA-224/256 and SHA-384/512 (FIPS 180-4), CRC-32 (IEEE, reflected, poly 0xEDB88320) and CRC-32C
//! (Castagnoli, reflected, poly 0x82F63B78) in plain Rust. Correctness is checked against the RFC/FIPS
//! test vectors in unit tests (e.g. md5("") = d41d8cd98f00b204e9800998ecf8427e,
//! sha256("abc") = ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad).
//!
//! Owned procs (ImplModule::Crypto): `md5_text` (md5(text) → lowercase hex text), `md5_bytea`,
//! `sha224_bytea sha256_bytea sha384_bytea sha512_bytea` (→ bytea digest), `crc32_bytea`,
//! `crc32c_bytea` (PG18: → int8, the unsigned 32-bit CRC). PG: `crc32('The quick brown fox jumps over
//! the lazy dog'::bytea)` = 1095738169.
use crate::catalog_data::BuiltinProc;
use crate::func::FuncImpl;
use pgarbor_common::{PgError, Result};

/// MD5 digest (16 bytes).
// CONTRACT
#[allow(unused_variables)]
pub fn md5(data: &[u8]) -> Result<[u8; 16]> {
    Err(PgError::unimplemented("TYP-19", "md5"))
}

/// SHA-256 digest.
// CONTRACT
#[allow(unused_variables)]
pub fn sha256(data: &[u8]) -> Result<[u8; 32]> {
    Err(PgError::unimplemented("TYP-19", "sha256"))
}

/// SHA-224 digest.
// CONTRACT
#[allow(unused_variables)]
pub fn sha224(data: &[u8]) -> Result<[u8; 28]> {
    Err(PgError::unimplemented("TYP-19", "sha224"))
}

/// SHA-512 digest.
// CONTRACT
#[allow(unused_variables)]
pub fn sha512(data: &[u8]) -> Result<[u8; 64]> {
    Err(PgError::unimplemented("TYP-19", "sha512"))
}

/// SHA-384 digest.
// CONTRACT
#[allow(unused_variables)]
pub fn sha384(data: &[u8]) -> Result<[u8; 48]> {
    Err(PgError::unimplemented("TYP-19", "sha384"))
}

/// Function registry hook (see `crate::func`).
#[allow(unused_variables)]
pub(crate) fn lookup(proc: &BuiltinProc) -> Option<FuncImpl> {
    None
}

/// 0A000 for a Crypto-module proc without implementation.
pub(crate) fn unsupported(proc: &BuiltinProc) -> PgError {
    PgError::unimplemented("TYP-19", format!("function {}", proc.name))
}
