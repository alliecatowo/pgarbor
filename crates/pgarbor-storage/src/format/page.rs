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

/// Copy `N` bytes starting at `off` out of `bytes`. Out-of-range access is an internal error,
/// never a panic.
fn read_array<const N: usize>(bytes: &[u8], off: usize) -> Result<[u8; N]> {
    bytes
        .get(off..off.saturating_add(N))
        .and_then(|s| <[u8; N]>::try_from(s).ok())
        .ok_or_else(|| PgError::internal(format!("page read of {N} bytes at offset {off} is out of bounds")))
}

impl PageHeader {
    /// Parse the first 32 bytes without any validation. `bytes.len() >= 32` required
    /// (else `PgError::internal`).
    pub fn parse(bytes: &[u8]) -> Result<PageHeader> {
        if bytes.len() < PAGE_HEADER_LEN {
            return Err(PgError::internal(format!(
                "page header needs {PAGE_HEADER_LEN} bytes, got {}",
                bytes.len()
            )));
        }
        Ok(PageHeader {
            magic: u32::from_le_bytes(read_array(bytes, 0)?),
            kind: bytes[4],
            flags: bytes[5],
            reserved: u16::from_le_bytes(read_array(bytes, 6)?),
            page_id: PageId(u64::from_le_bytes(read_array(bytes, 8)?)),
            commit_seq: CommitSeq(u64::from_le_bytes(read_array(bytes, 16)?)),
            crc32c: u32::from_le_bytes(read_array(bytes, 24)?),
            payload_len: u32::from_le_bytes(read_array(bytes, 28)?),
        })
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
pub fn encode_page(page_size: u32, page: &Page) -> Result<Vec<u8>> {
    if !is_valid_page_size(page_size) {
        return Err(PgError::internal(format!("invalid page size {page_size}")));
    }
    if page.id.is_none() {
        return Err(PgError::internal("cannot encode a page with PageId::NONE"));
    }
    let cap = payload_capacity(page_size);
    let payload_len = page.payload.len();
    if payload_len > cap {
        return Err(PgError::internal(format!(
            "payload length {payload_len} exceeds page capacity {cap}"
        )));
    }

    let mut out = vec![0u8; page_size as usize];
    out[0..4].copy_from_slice(&PAGE_MAGIC.to_le_bytes());
    out[4] = page.kind.as_u8();
    // bytes 5..8 (flags, reserved) stay zero for format v1
    out[8..16].copy_from_slice(&page.id.0.to_le_bytes());
    out[16..24].copy_from_slice(&page.commit_seq.0.to_le_bytes());
    // bytes 24..28 (crc) are filled in below
    out[28..32].copy_from_slice(&(payload_len as u32).to_le_bytes());
    out[PAGE_HEADER_LEN..PAGE_HEADER_LEN + payload_len].copy_from_slice(&page.payload);

    let header = read_array::<PAGE_HEADER_LEN>(&out, 0)?;
    let crc = page_checksum(&header, &page.payload);
    out[24..28].copy_from_slice(&crc.to_le_bytes());
    Ok(out)
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
pub fn decode_page(page_size: u32, expected: PageId, bytes: &[u8]) -> Result<Page> {
    if !is_valid_page_size(page_size) {
        return Err(PgError::internal(format!("invalid page size {page_size}")));
    }
    if bytes.len() != page_size as usize {
        return Err(PgError::internal(format!(
            "page buffer is {} bytes, expected page size {page_size}",
            bytes.len()
        )));
    }

    // 1. never written or torn to nothing
    if bytes.iter().all(|b| *b == 0) {
        return Err(error::invalid_page(expected, "page is all zeroes"));
    }

    // Header fields (length already checked against page_size >= 4096).
    let header = PageHeader::parse(bytes)?;

    // 2. magic
    if header.magic != PAGE_MAGIC {
        return Err(error::invalid_page(expected, format!("bad page magic 0x{:08x}", header.magic)));
    }

    // 3. payload length against capacity, before any slicing
    let cap = payload_capacity(page_size);
    let payload_len = header.payload_len as usize;
    if payload_len > cap {
        return Err(error::invalid_page(
            expected,
            format!("payload length {payload_len} exceeds page capacity {cap}"),
        ));
    }
    let payload = bytes.get(PAGE_HEADER_LEN..PAGE_HEADER_LEN + payload_len).ok_or_else(|| {
        PgError::internal("validated payload range is out of bounds")
    })?;

    // 4. checksum over header (crc field zeroed) and payload; padding is not covered
    let raw_header = read_array::<PAGE_HEADER_LEN>(bytes, 0)?;
    let computed = page_checksum(&raw_header, payload);
    if computed != header.crc32c {
        return Err(error::checksum_mismatch(expected, computed, header.crc32c));
    }

    // 5. kind
    let kind = PageKind::from_u8(header.kind)
        .ok_or_else(|| error::invalid_page(expected, format!("unknown page kind {}", header.kind)))?;

    // 6. flags and reserved must be zero in format v1
    if header.flags != 0 || header.reserved != 0 {
        return Err(error::invalid_page(
            expected,
            format!("unsupported page flags 0x{:02x} reserved 0x{:04x}", header.flags, header.reserved),
        ));
    }

    // 7. misdirected or stale write
    if header.page_id != expected {
        return Err(error::invalid_page(expected, format!("page header claims page {}", header.page_id.0)));
    }

    // 8. verified
    Ok(Page {
        id: expected,
        kind,
        commit_seq: header.commit_seq,
        payload: payload.to_vec(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use pgarbor_common::SqlState;

    #[test]
    fn checksum_ignores_crc_field() {
        let mut h = [7u8; PAGE_HEADER_LEN];
        let a = page_checksum(&h, b"xyz");
        h[24] = 0xEE;
        assert_eq!(a, page_checksum(&h, b"xyz"));
    }

    fn mk(kind: PageKind, id: u64, seq: u64, payload: Vec<u8>) -> Page {
        Page { id: PageId(id), kind, commit_seq: CommitSeq(seq), payload }
    }

    fn pattern(len: usize) -> Vec<u8> {
        (0..len).map(|i| (i.wrapping_mul(31) % 251) as u8).collect()
    }

    /// Recompute the stored crc after a deliberate edit, so only the edited field is wrong.
    fn fix_crc(buf: &mut [u8]) {
        let payload_len = u32::from_le_bytes(buf[28..32].try_into().unwrap()) as usize;
        let hdr: [u8; PAGE_HEADER_LEN] = buf[0..PAGE_HEADER_LEN].try_into().unwrap();
        let crc = page_checksum(&hdr, &buf[PAGE_HEADER_LEN..PAGE_HEADER_LEN + payload_len]);
        buf[24..28].copy_from_slice(&crc.to_le_bytes());
    }

    #[test]
    fn roundtrip_every_kind_and_size() {
        let kinds = [PageKind::Leaf, PageKind::Internal, PageKind::Overflow, PageKind::FreeList];
        for &ps in &[4096u32, 8192] {
            let cap = payload_capacity(ps);
            let lens = (0..=cap).step_by(37).chain(std::iter::once(cap));
            for kind in kinds {
                for len in lens.clone() {
                    let p = mk(kind, 42, 7, pattern(len));
                    let enc = encode_page(ps, &p).unwrap();
                    assert_eq!(enc.len(), ps as usize);
                    let dec = decode_page(ps, PageId(42), &enc).unwrap();
                    assert_eq!(dec, p, "ps {ps} kind {kind:?} len {len}");
                }
            }
        }
    }

    #[test]
    fn encode_layout_is_exact() {
        let p = mk(PageKind::Overflow, 5, 9, vec![1, 2, 3]);
        let enc = encode_page(4096, &p).unwrap();
        assert_eq!(&enc[0..4], b"PGAp");
        assert_eq!(enc[4], 3);
        assert_eq!(&enc[5..8], &[0, 0, 0]);
        assert_eq!(&enc[8..16], &5u64.to_le_bytes());
        assert_eq!(&enc[16..24], &9u64.to_le_bytes());
        assert_eq!(&enc[28..32], &3u32.to_le_bytes());
        assert_eq!(&enc[32..35], &[1, 2, 3]);
        assert!(enc[35..].iter().all(|b| *b == 0));
        let h = PageHeader::parse(&enc).unwrap();
        assert_eq!(h.magic, PAGE_MAGIC);
        assert_eq!(h.kind, 3);
        assert_eq!(h.page_id, PageId(5));
        assert_eq!(h.commit_seq, CommitSeq(9));
        assert_eq!(h.payload_len, 3);
    }

    #[test]
    fn parse_rejects_short_input() {
        let e = PageHeader::parse(&[0u8; 31]).unwrap_err();
        assert_eq!(e.sqlstate, SqlState::INTERNAL_ERROR);
    }

    #[test]
    fn encode_rejects_bad_inputs() {
        let none = mk(PageKind::Leaf, 0, 1, vec![]);
        assert_eq!(encode_page(4096, &none).unwrap_err().sqlstate, SqlState::INTERNAL_ERROR);
        let big = mk(PageKind::Leaf, 1, 1, vec![0; payload_capacity(4096) + 1]);
        assert_eq!(encode_page(4096, &big).unwrap_err().sqlstate, SqlState::INTERNAL_ERROR);
        let ok = mk(PageKind::Leaf, 1, 1, vec![]);
        assert_eq!(encode_page(1000, &ok).unwrap_err().sqlstate, SqlState::INTERNAL_ERROR);
    }

    #[test]
    fn decode_rejects_wrong_buffer_length() {
        let enc = encode_page(4096, &mk(PageKind::Leaf, 1, 1, vec![1])).unwrap();
        let e = decode_page(8192, PageId(1), &enc).unwrap_err();
        assert_eq!(e.sqlstate, SqlState::INTERNAL_ERROR);
    }

    #[test]
    fn zero_page_is_rejected() {
        let e = decode_page(4096, PageId(3), &vec![0u8; 4096]).unwrap_err();
        assert_eq!(e.sqlstate, SqlState::DATA_CORRUPTED);
        assert_eq!(e.message, "invalid page in block 3 of database file");
        assert_eq!(e.detail.as_deref(), Some("page is all zeroes"));
    }

    #[test]
    fn bad_magic_is_rejected() {
        let mut b = encode_page(4096, &mk(PageKind::Leaf, 1, 1, vec![1, 2])).unwrap();
        b[0] = b'X';
        let e = decode_page(4096, PageId(1), &b).unwrap_err();
        assert_eq!(e.sqlstate, SqlState::DATA_CORRUPTED);
        assert!(e.detail.unwrap().starts_with("bad page magic 0x"));
    }

    #[test]
    fn oversized_payload_len_is_rejected_before_slicing() {
        let mut b = encode_page(4096, &mk(PageKind::Leaf, 1, 1, vec![1, 2])).unwrap();
        let too_big = (payload_capacity(4096) + 1) as u32;
        b[28..32].copy_from_slice(&too_big.to_le_bytes());
        fix_crc(&mut b);
        let e = decode_page(4096, PageId(1), &b).unwrap_err();
        assert_eq!(e.detail.as_deref(), Some("payload length 4065 exceeds page capacity 4064"));
    }

    #[test]
    fn corrupted_payload_is_detected_as_checksum_mismatch() {
        let mut b = encode_page(4096, &mk(PageKind::Leaf, 2, 1, pattern(50))).unwrap();
        b[40] ^= 0x01;
        let e = decode_page(4096, PageId(2), &b).unwrap_err();
        assert_eq!(e.sqlstate, SqlState::DATA_CORRUPTED);
        assert!(e.message.starts_with("page verification failed, calculated checksum "));
    }

    #[test]
    fn unknown_kind_is_rejected_with_valid_crc() {
        let mut b = encode_page(4096, &mk(PageKind::Leaf, 1, 1, vec![1])).unwrap();
        b[4] = 9;
        fix_crc(&mut b);
        let e = decode_page(4096, PageId(1), &b).unwrap_err();
        assert_eq!(e.detail.as_deref(), Some("unknown page kind 9"));
    }

    #[test]
    fn nonzero_flags_are_rejected_with_valid_crc() {
        let mut b = encode_page(4096, &mk(PageKind::Leaf, 1, 1, vec![1])).unwrap();
        b[5] = 1;
        fix_crc(&mut b);
        let e = decode_page(4096, PageId(1), &b).unwrap_err();
        assert_eq!(e.detail.as_deref(), Some("unsupported page flags 0x01 reserved 0x0000"));
    }

    #[test]
    fn misdirected_page_is_rejected() {
        let b = encode_page(4096, &mk(PageKind::Internal, 5, 1, vec![1, 2, 3])).unwrap();
        let e = decode_page(4096, PageId(6), &b).unwrap_err();
        assert_eq!(e.sqlstate, SqlState::DATA_CORRUPTED);
        assert_eq!(e.message, "invalid page in block 6 of database file");
        assert_eq!(e.detail.as_deref(), Some("page header claims page 5"));
    }

    #[test]
    fn every_single_bit_flip_in_header_or_payload_is_detected() {
        let ps = 4096u32;
        let good = encode_page(ps, &mk(PageKind::Leaf, 3, 9, pattern(100))).unwrap();
        for i in (0..PAGE_HEADER_LEN).chain(PAGE_HEADER_LEN..PAGE_HEADER_LEN + 100) {
            for bit in 0..8 {
                let mut b = good.clone();
                b[i] ^= 1 << bit;
                let r = decode_page(ps, PageId(3), &b);
                assert!(r.is_err(), "flip of byte {i} bit {bit} went undetected");
                assert_eq!(r.unwrap_err().sqlstate, SqlState::DATA_CORRUPTED);
            }
        }
    }

    #[test]
    fn flipping_padding_is_not_detected() {
        let p = mk(PageKind::Leaf, 3, 9, pattern(10));
        let mut b = encode_page(4096, &p).unwrap();
        b[4000] ^= 0xFF;
        b[PAGE_HEADER_LEN + 10] = 0xAB;
        assert_eq!(decode_page(4096, PageId(3), &b).unwrap(), p);
    }
}
