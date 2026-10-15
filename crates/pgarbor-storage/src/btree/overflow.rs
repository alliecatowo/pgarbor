//! Overflow page chains for large values. Lease STO-06.
//!
//! # Overflow page payload (CONTRACT; page kind = Overflow)
//! ```text
//! 0  8  next PageId (u64 LE; 0 = last page of the chain)
//! 8  .  chunk bytes (payload_len - 8 bytes)
//! ```
//! A value of `len` bytes uses `n = ceil(len / chunk_capacity)` pages; every chunk is exactly
//! `chunk_capacity(ps)` bytes except the last (`len - (n-1)*cap`, >= 1). Values are only ever
//! stored in overflow chains when `len > max_inline_value(ps)`, so `n >= 1` and len >= 1.
//! Chains are immutable and may be shared between trees (copy-on-write of a leaf copies only the
//! 8-byte pointer). Ids are allocated in ascending order before writing so the chain is laid out
//! sequentially: page k has `next` = id of page k+1.
#[allow(unused_imports)]
use crate::format::consts::{payload_capacity, PageKind};
use crate::pager::{PageSink, PageSource};
use pgarbor_common::{PageId, PgError, Result};

// CONTRACT
pub const OVERFLOW_HEADER_LEN: usize = 8;

// CONTRACT
/// Data bytes per overflow page (8192 → 8152).
pub fn chunk_capacity(page_size: u32) -> usize {
    payload_capacity(page_size) - OVERFLOW_HEADER_LEN
}

// CONTRACT
/// Number of pages needed for a value of `len` bytes.
pub fn pages_for(page_size: u32, len: u64) -> u64 {
    len.div_ceil(chunk_capacity(page_size) as u64)
}

// CONTRACT
/// Write `value` as a new chain; returns the first PageId. Errors: empty value or
/// `len > u32::MAX` → `PgError::internal`; sink errors propagate.
#[allow(unused_variables)]
pub fn write_overflow(sink: &mut dyn PageSink, value: &[u8]) -> Result<PageId> {
    Err(PgError::unimplemented("STO-06", "overflow chain write"))
}

// CONTRACT
/// Read a whole chain back. Validation (all `error::invalid_page(page, reason)`, XX001):
/// page kind != Overflow ("expected overflow page, found <kind> page"); payload shorter than 8;
/// chunk length != expected for its position ("overflow chunk has <n> bytes, expected <m>");
/// `next == 0` before `len` bytes were read ("overflow chain ends early"); `next != 0` on the
/// page that completes `len` ("overflow chain longer than value length"). The page-count bound
/// `pages_for(len)` also guarantees termination on cyclic chains.
#[allow(unused_variables)]
pub fn read_overflow(src: &dyn PageSource, first: PageId, len: u32) -> Result<Vec<u8>> {
    Err(PgError::unimplemented("STO-06", "overflow chain read"))
}

// CONTRACT
/// PageIds of a chain in order, validated like `read_overflow` but without copying data
/// (used by trace/copy/doctor/release).
#[allow(unused_variables)]
pub fn chain_pages(src: &dyn PageSource, first: PageId, len: u32) -> Result<Vec<PageId>> {
    Err(PgError::unimplemented("STO-06", "overflow chain walk"))
}

// CONTRACT
/// `sink.release_page` every page of the chain (when a value is overwritten/deleted).
/// Uses `chain_pages`.
#[allow(unused_variables)]
pub fn release_overflow(sink: &mut dyn PageSink, first: PageId, len: u32) -> Result<()> {
    Err(PgError::unimplemented("STO-06", "overflow chain release"))
}

// CONTRACT
/// Copy a chain from `src` into `dst` (fresh ids, same bytes). Used by `gc::copy`.
#[allow(unused_variables)]
pub fn copy_overflow(src: &dyn PageSource, dst: &mut dyn PageSink, first: PageId, len: u32) -> Result<PageId> {
    Err(PgError::unimplemented("STO-06", "overflow chain copy"))
}
