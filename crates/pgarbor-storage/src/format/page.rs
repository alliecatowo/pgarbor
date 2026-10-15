//! Page header + checksum codec (STORE-04). Lease STO-02.
//!
//! # Page layout (CONTRACT, little-endian, total = page_size bytes)
//! ```text
//! off len field
//!   0   4 magic        = PAGE_MAGIC (bytes "PGAp")
//!   4   1 kind         = PageKind as u8 (1 leaf, 2 internal, 3 overflow, 4 freelist)
//!   5   1 flags        = 0 (format v1)
//!   6   2 reserved     = 0
//!   8   8 page_id      = this page's own PageId (detects misdirected writes)
//!  16   8 commit_seq   = CommitSeq of the commit that wrote the page
//!  24   4 crc32c       = crc32c(header[0..32] with bytes 24..28 zeroed ++ payload[0..payload_len])
//!  28   4 payload_len  (<= page_size - 32)
//!  32   . payload (payload_len bytes), then zero padding to page_size (padding not checksummed)
//! ```
#[allow(unused_imports)]
use crate::error;
use crate::format::consts::*;
use pgarbor_common::{CommitSeq, PageId, PgError, Result};
use std::sync::Arc;

// CONTRACT
/// A verified, decoded page. `payload.len()` == header payload_len.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Page {
    pub id: PageId,
    pub kind: PageKind,
    pub commit_seq: CommitSeq,
    pub payload: Vec<u8>,
}

// CONTRACT
/// Shared immutable page handle (what the cache stores and readers hold).
pub type PageRef = Arc<Page>;

// CONTRACT
/// Raw header fields (for doctor/inspect of possibly-damaged pages).
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize)]
pub struct PageHeader {
    pub magic: u32,
    pub kind: u8,
    pub flags: u8,
    pub reserved: u16,
    pub page_id: PageId,
    pub commit_seq: CommitSeq,
    pub crc32c: u32,
    pub payload_len: u32,
}

impl PageHeader {
    /// Parse the first 32 bytes without any validation. `bytes.len() >= 32` required
    /// (else `PgError::internal`).
    #[allow(unused_variables)]
    pub fn parse(bytes: &[u8]) -> Result<PageHeader> {
        Err(PgError::unimplemented("STO-02", "page header parsing"))
    }
}

// CONTRACT
/// Checksum over a 32-byte header (bytes 24..28 are treated as zero regardless of content) and
/// the payload. Fully implemented.
pub fn page_checksum(header: &[u8; PAGE_HEADER_LEN], payload: &[u8]) -> u32 {
    let mut h = *header;
    h[24..28].copy_from_slice(&[0, 0, 0, 0]);
    let c = crc32c::crc32c(&h);
    crc32c::crc32c_append(c, payload)
}

// CONTRACT
/// Encode `page` into exactly `page_size` bytes (header, payload, zero padding), computing crc.
///
/// Errors (`PgError::internal`, these are programming errors): `page.id` is NONE;
/// `payload.len() > payload_capacity(page_size)`.
#[allow(unused_variables)]
pub fn encode_page(page_size: u32, page: &Page) -> Result<Vec<u8>> {
    Err(PgError::unimplemented("STO-02", "page encoding"))
}

// CONTRACT
/// Decode and verify a page read from slot `expected`. `bytes.len()` must equal `page_size`
/// (else `PgError::internal`). Validation order and errors (all XX001 via crate::error):
/// 1. all bytes zero → `error::invalid_page(expected, "page is all zeroes")` (never written / torn)
/// 2. magic != PAGE_MAGIC → `invalid_page(expected, format!("bad page magic 0x{magic:08x}"))`
/// 3. payload_len > payload_capacity → `invalid_page(expected, "payload length <n> exceeds page capacity <c>")`
/// 4. crc mismatch → `error::checksum_mismatch(expected, computed, stored)`
/// 5. unknown kind byte → `invalid_page(expected, "unknown page kind <k>")`
/// 6. flags != 0 or reserved != 0 → `invalid_page(expected, "unsupported page flags 0x..")`
/// 7. header page_id != expected → `invalid_page(expected, "page header claims page <x>")`
///    (misdirected/stale write)
/// 8. Ok(Page { id: expected, kind, commit_seq, payload: bytes[32..32+payload_len].to_vec() })
#[allow(unused_variables)]
pub fn decode_page(page_size: u32, expected: PageId, bytes: &[u8]) -> Result<Page> {
    Err(PgError::unimplemented("STO-02", "page decoding"))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn checksum_ignores_crc_field() {
        let mut h = [7u8; PAGE_HEADER_LEN];
        let a = page_checksum(&h, b"xyz");
        h[24] = 0xEE;
        assert_eq!(a, page_checksum(&h, b"xyz"));
    }
    // STO-02 adds: roundtrip for each kind; flipping any byte in header[0..24], header[28..32] or
    // payload gives an error; flipping a padding byte does not; misdirected page id detected.
}
