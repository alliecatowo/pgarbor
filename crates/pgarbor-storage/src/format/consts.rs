//! Format constants and address arithmetic. CONTRACT. Fully implemented.
//!
//! File layout (ARCHITECTURE §5):
//! ```text
//! offset 0     : superblock slot 0 (4096 bytes)
//! offset 4096  : superblock slot 1 (4096 bytes)
//! offset 8192  : page 1, then page 2 at 8192 + page_size, ...
//! PageId n (n >= 1) lives at 8192 + (n - 1) * page_size. PageId 0 = "no page".
//! ```
//! `page_count` in the superblock = highest allocated PageId = number of pages in use; the valid
//! file length is `file_len_for(page_size, page_count)`. Bytes past it are uncommitted garbage.
use pgarbor_common::PageId;

// CONTRACT
pub const SUPERBLOCK_MAGIC: [u8; 8] = *b"PGARBOR\0";
// CONTRACT
/// The only format version this build reads and writes.
pub const FORMAT_VERSION: u32 = 1;
// CONTRACT
pub const SUPERBLOCK_SLOT_SIZE: usize = 4096;
// CONTRACT
pub const SUPERBLOCK_SLOTS: usize = 2;
// CONTRACT
/// Byte offset of page 1 (size of the superblock area).
pub const PAGES_OFFSET: u64 = 8192;
// CONTRACT
/// Number of meaningful bytes in a superblock slot (fields + crc); the rest of the slot is zero.
pub const SUPERBLOCK_ENCODED_LEN: usize = 92;
// CONTRACT
pub const DEFAULT_PAGE_SIZE: u32 = 8192;
// CONTRACT
pub const MIN_PAGE_SIZE: u32 = 4096;
// CONTRACT
pub const MAX_PAGE_SIZE: u32 = 65536;
// CONTRACT
/// Page header magic: the bytes `b"PGAp"` read as a little-endian u32.
pub const PAGE_MAGIC: u32 = u32::from_le_bytes(*b"PGAp");
// CONTRACT
pub const PAGE_HEADER_LEN: usize = 32;
// CONTRACT
/// Feature flag bits 32..63 are "required": a reader that does not know a set required bit must
/// refuse to open the file. Bits 0..31 are "compatible" and ignored when unknown.
pub const FEATURE_REQUIRED_MASK: u64 = 0xFFFF_FFFF_0000_0000;
// CONTRACT
/// Feature bits this build understands (none defined in format v1).
pub const KNOWN_FEATURES: u64 = 0;
// CONTRACT
/// Largest value length representable in a leaf cell (`vlen` is u32).
pub const MAX_VALUE_LEN: u64 = u32::MAX as u64;

// CONTRACT
/// Kind byte of a page header.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, serde::Serialize, serde::Deserialize)]
#[repr(u8)]
pub enum PageKind {
    /// B+tree leaf node (level 0).
    Leaf = 1,
    /// B+tree internal node (level >= 1).
    Internal = 2,
    /// One link of an overflow-value chain.
    Overflow = 3,
    /// Reserved for a future free-list (format v1 never writes it).
    FreeList = 4,
}

impl PageKind {
    pub fn from_u8(b: u8) -> Option<PageKind> {
        match b {
            1 => Some(PageKind::Leaf),
            2 => Some(PageKind::Internal),
            3 => Some(PageKind::Overflow),
            4 => Some(PageKind::FreeList),
            _ => None,
        }
    }
    pub fn as_u8(self) -> u8 {
        self as u8
    }
    pub fn name(self) -> &'static str {
        match self {
            PageKind::Leaf => "leaf",
            PageKind::Internal => "internal",
            PageKind::Overflow => "overflow",
            PageKind::FreeList => "freelist",
        }
    }
}

// CONTRACT
/// Power of two within [MIN_PAGE_SIZE, MAX_PAGE_SIZE].
pub fn is_valid_page_size(page_size: u32) -> bool {
    page_size.is_power_of_two() && (MIN_PAGE_SIZE..=MAX_PAGE_SIZE).contains(&page_size)
}

// CONTRACT
/// Byte offset of page `id` (`id.0 >= 1`). Caller guarantees `id` is not `PageId::NONE`.
pub fn page_offset(page_size: u32, id: PageId) -> u64 {
    debug_assert!(id.0 >= 1);
    PAGES_OFFSET + (id.0.saturating_sub(1)) * page_size as u64
}

// CONTRACT
/// Byte offset of superblock slot `slot` (0 or 1).
pub fn slot_offset(slot: usize) -> u64 {
    (slot * SUPERBLOCK_SLOT_SIZE) as u64
}

// CONTRACT
/// Exact committed file length for `page_count` pages.
pub fn file_len_for(page_size: u32, page_count: u64) -> u64 {
    PAGES_OFFSET + page_count * page_size as u64
}

// CONTRACT
/// Maximum payload bytes in one page.
pub fn payload_capacity(page_size: u32) -> usize {
    page_size as usize - PAGE_HEADER_LEN
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn offsets() {
        assert_eq!(page_offset(8192, PageId(1)), 8192);
        assert_eq!(page_offset(8192, PageId(3)), 8192 + 2 * 8192);
        assert_eq!(page_offset(4096, PageId(2)), 8192 + 4096);
        assert_eq!(file_len_for(8192, 0), 8192);
        assert!(is_valid_page_size(8192) && !is_valid_page_size(8000) && !is_valid_page_size(2048));
        assert_eq!(&PAGE_MAGIC.to_le_bytes(), b"PGAp");
    }
}
