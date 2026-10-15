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
use crate::error;
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
pub fn write_overflow(sink: &mut dyn PageSink, value: &[u8]) -> Result<PageId> {
    check_value_len(value.len() as u64)?;
    let ps = sink.page_size();
    let cap = chunk_capacity(ps);
    let n = pages_for(ps, value.len() as u64);
    let ids = alloc_chain(sink, n)?;
    for (k, chunk) in value.chunks(cap).enumerate() {
        let next = ids.get(k + 1).copied().unwrap_or(PageId::NONE);
        write_chunk(sink, ids[k], next, chunk)?;
    }
    Ok(ids[0])
}

// CONTRACT
/// Read a whole chain back. Validation (all `error::invalid_page(page, reason)`, XX001):
/// page kind != Overflow ("expected overflow page, found <kind> page"); payload shorter than 8;
/// chunk length != expected for its position ("overflow chunk has <n> bytes, expected <m>");
/// `next == 0` before `len` bytes were read ("overflow chain ends early"); `next != 0` on the
/// page that completes `len` ("overflow chain longer than value length"). The page-count bound
/// `pages_for(len)` also guarantees termination on cyclic chains.
pub fn read_overflow(src: &dyn PageSource, first: PageId, len: u32) -> Result<Vec<u8>> {
    let mut out = Vec::new();
    walk(src, first, len, |_, _, chunk| {
        out.extend_from_slice(chunk);
        Ok(())
    })?;
    Ok(out)
}

// CONTRACT
/// PageIds of a chain in order, validated like `read_overflow` but without copying data
/// (used by trace/copy/doctor/release).
pub fn chain_pages(src: &dyn PageSource, first: PageId, len: u32) -> Result<Vec<PageId>> {
    chain_ids(src, first, len)
}

// CONTRACT
/// `sink.release_page` every page of the chain (when a value is overwritten/deleted).
/// Uses `chain_pages`.
pub fn release_overflow(sink: &mut dyn PageSink, first: PageId, len: u32) -> Result<()> {
    // The chain is validated completely before any page is released, so a corrupt chain
    // releases nothing.
    let ids = chain_ids(&*sink, first, len)?;
    for id in ids {
        sink.release_page(id)?;
    }
    Ok(())
}

// CONTRACT
/// Copy a chain from `src` into `dst` (fresh ids, same bytes). Used by `gc::copy`.
///
/// The source chain is walked and validated while the copy is written; the destination ids are
/// allocated up front (as `write_overflow` does) and each page is written as its source chunk is
/// read, so no whole value is held in memory. If the source is corrupt, the destination pages
/// already allocated stay private and unreachable, exactly as after any failed write in the
/// same transaction.
pub fn copy_overflow(src: &dyn PageSource, dst: &mut dyn PageSink, first: PageId, len: u32) -> Result<PageId> {
    check_value_len(len as u64)?;
    let ps = src.page_size();
    if dst.page_size() != ps {
        return Err(PgError::internal(format!(
            "overflow chain copy between page sizes {} and {}",
            ps,
            dst.page_size()
        )));
    }
    let n = pages_for(ps, len as u64);
    let ids = alloc_chain(dst, n)?;
    walk(src, first, len, |k, _, chunk| {
        let next = ids.get(k + 1).copied().unwrap_or(PageId::NONE);
        write_chunk(&mut *dst, ids[k], next, chunk)
    })?;
    Ok(ids[0])
}

// ---------------------------------------------------------------------------------------------
// Private helpers
// ---------------------------------------------------------------------------------------------

/// Empty values and values longer than `u32::MAX` bytes are programming errors for this layer.
fn check_value_len(len: u64) -> Result<()> {
    if len == 0 {
        return Err(PgError::internal("overflow value must not be empty"));
    }
    if len > u32::MAX as u64 {
        return Err(PgError::internal(format!("overflow value length {len} exceeds u32::MAX")));
    }
    Ok(())
}

/// Allocate `n` private page ids in ascending allocation order.
fn alloc_chain(sink: &mut dyn PageSink, n: u64) -> Result<Vec<PageId>> {
    let mut ids = Vec::with_capacity(n as usize);
    for _ in 0..n {
        ids.push(sink.alloc_page()?);
    }
    Ok(ids)
}

/// Write one overflow page: `next` pointer followed by the chunk bytes.
fn write_chunk(sink: &mut dyn PageSink, id: PageId, next: PageId, chunk: &[u8]) -> Result<()> {
    let mut payload = Vec::with_capacity(OVERFLOW_HEADER_LEN + chunk.len());
    payload.extend_from_slice(&next.0.to_le_bytes());
    payload.extend_from_slice(chunk);
    sink.write_page(id, PageKind::Overflow, payload)
}

/// Validated walk over a chain. Calls `visit(index, page_id, chunk)` for each page in order.
/// Works for any page source, including a sink (`S = dyn PageSink`) through its supertrait.
fn walk<S: PageSource + ?Sized>(
    src: &S,
    first: PageId,
    len: u32,
    mut visit: impl FnMut(usize, PageId, &[u8]) -> Result<()>,
) -> Result<()> {
    let total = len as usize;
    if total == 0 {
        return Ok(());
    }
    let ps = src.page_size();
    let cap = chunk_capacity(ps);
    let n = pages_for(ps, len as u64) as usize;
    let mut cur = first;
    let mut prev = PageId::NONE;
    for k in 0..n {
        if cur.is_none() {
            return Err(error::invalid_page(prev, "overflow chain ends early"));
        }
        let page = src.read_page(cur)?;
        if page.kind != PageKind::Overflow {
            return Err(error::invalid_page(
                cur,
                format!("expected overflow page, found {} page", page.kind.name()),
            ));
        }
        let payload = page.payload.as_slice();
        if payload.len() < OVERFLOW_HEADER_LEN {
            return Err(error::invalid_page(
                cur,
                format!(
                    "overflow page payload has {} bytes, shorter than the {}-byte header",
                    payload.len(),
                    OVERFLOW_HEADER_LEN
                ),
            ));
        }
        let mut next_bytes = [0u8; OVERFLOW_HEADER_LEN];
        next_bytes.copy_from_slice(&payload[..OVERFLOW_HEADER_LEN]);
        let next = u64::from_le_bytes(next_bytes);
        let chunk = &payload[OVERFLOW_HEADER_LEN..];

        let expected = (total - k * cap).min(cap);
        if chunk.len() != expected {
            return Err(error::invalid_page(
                cur,
                format!("overflow chunk has {} bytes, expected {}", chunk.len(), expected),
            ));
        }
        let last = k + 1 == n;
        if last && next != 0 {
            return Err(error::invalid_page(cur, "overflow chain longer than value length"));
        }
        if !last && next == 0 {
            return Err(error::invalid_page(cur, "overflow chain ends early"));
        }
        visit(k, cur, chunk)?;
        prev = cur;
        cur = PageId(next);
    }
    Ok(())
}

/// Page ids of a validated chain, generic over the page source so sinks can use it too.
fn chain_ids<S: PageSource + ?Sized>(src: &S, first: PageId, len: u32) -> Result<Vec<PageId>> {
    let n = pages_for(src.page_size(), len as u64) as usize;
    let mut ids = Vec::with_capacity(n);
    walk(src, first, len, |_, id, _| {
        ids.push(id);
        Ok(())
    })?;
    Ok(ids)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::format::page::{Page, PageRef};
    use pgarbor_common::CommitSeq;
    use std::collections::HashMap;
    use std::sync::Arc;

    const PS: u32 = 8192;

    /// In-memory page store implementing both traits. Ids are handed out ascending from 1.
    #[derive(Default)]
    struct MemSink {
        ps: u32,
        pages: HashMap<u64, PageRef>,
        next: u64,
        released: Vec<PageId>,
    }

    impl MemSink {
        fn new(ps: u32) -> Self {
            MemSink { ps, ..Default::default() }
        }

        /// Overwrite the `next` pointer of an existing overflow page, keeping its chunk.
        fn set_next(&mut self, id: PageId, next: u64) {
            let page = self.pages.get(&id.0).expect("page exists").clone();
            let mut payload = page.payload.clone();
            payload[..8].copy_from_slice(&next.to_le_bytes());
            self.write_page(id, page.kind, payload).expect("rewrite");
        }
    }

    impl PageSource for MemSink {
        fn page_size(&self) -> u32 {
            self.ps
        }
        fn read_page(&self, id: PageId) -> Result<PageRef> {
            if id.is_none() {
                return Err(error::corrupt("read of null page"));
            }
            self.pages
                .get(&id.0)
                .cloned()
                .ok_or_else(|| error::page_out_of_range(id, self.next))
        }
    }

    impl PageSink for MemSink {
        fn alloc_page(&mut self) -> Result<PageId> {
            self.next += 1;
            Ok(PageId(self.next))
        }
        fn write_page(&mut self, id: PageId, kind: PageKind, payload: Vec<u8>) -> Result<()> {
            if id.is_none() || id.0 > self.next {
                return Err(PgError::internal("write to unallocated page"));
            }
            if payload.len() > payload_capacity(self.ps) {
                return Err(PgError::internal("payload exceeds page capacity"));
            }
            self.pages.insert(id.0, Arc::new(Page { id, kind, commit_seq: CommitSeq(1), payload }));
            Ok(())
        }
        fn is_private(&self, _id: PageId) -> bool {
            true
        }
        fn release_page(&mut self, id: PageId) -> Result<()> {
            self.released.push(id);
            Ok(())
        }
    }

    fn pattern(len: usize) -> Vec<u8> {
        (0..len).map(|i| (i.wrapping_mul(31).wrapping_add(7)) as u8).collect()
    }

    fn detail_of(e: &PgError) -> &str {
        e.detail.as_deref().unwrap_or("")
    }

    fn roundtrip(len: usize) {
        let cap = chunk_capacity(PS);
        let value = pattern(len);
        let mut sink = MemSink::new(PS);
        let first = write_overflow(&mut sink, &value).unwrap();
        assert_eq!(read_overflow(&sink, first, len as u32).unwrap(), value);
        let ids = chain_pages(&sink, first, len as u32).unwrap();
        assert_eq!(ids.len(), len.div_ceil(cap));
        assert_eq!(ids.len() as u64, pages_for(PS, len as u64));
        assert_eq!(ids[0], first);
        for w in ids.windows(2) {
            assert_eq!(w[1].0, w[0].0 + 1, "chain ids are sequential");
        }
    }

    #[test]
    fn roundtrip_len_1() {
        roundtrip(1);
    }

    #[test]
    fn roundtrip_chunk_minus_1() {
        roundtrip(chunk_capacity(PS) - 1);
    }

    #[test]
    fn roundtrip_chunk() {
        roundtrip(chunk_capacity(PS));
    }

    #[test]
    fn roundtrip_chunk_plus_1() {
        roundtrip(chunk_capacity(PS) + 1);
    }

    #[test]
    fn roundtrip_five_chunks_plus_7() {
        roundtrip(5 * chunk_capacity(PS) + 7);
    }

    #[test]
    fn pages_for_boundaries() {
        let cap = chunk_capacity(PS) as u64;
        assert_eq!(pages_for(PS, 1), 1);
        assert_eq!(pages_for(PS, cap), 1);
        assert_eq!(pages_for(PS, cap + 1), 2);
        assert_eq!(pages_for(PS, 2 * cap), 2);
    }

    #[test]
    fn empty_value_is_internal_error() {
        let mut sink = MemSink::new(PS);
        let e = write_overflow(&mut sink, &[]).unwrap_err();
        assert_eq!(e.sqlstate.code(), "XX000");
    }

    #[test]
    fn truncated_chain_detected() {
        let cap = chunk_capacity(PS);
        let len = 5 * cap + 7;
        let mut sink = MemSink::new(PS);
        let first = write_overflow(&mut sink, &pattern(len)).unwrap();
        let ids = chain_pages(&sink, first, len as u32).unwrap();
        sink.set_next(ids[2], 0);
        let e = read_overflow(&sink, first, len as u32).unwrap_err();
        assert_eq!(e.sqlstate.code(), "XX001");
        assert_eq!(detail_of(&e), "overflow chain ends early");
        let e = chain_pages(&sink, first, len as u32).unwrap_err();
        assert_eq!(detail_of(&e), "overflow chain ends early");
    }

    #[test]
    fn over_long_chain_detected() {
        let cap = chunk_capacity(PS);
        let mut sink = MemSink::new(PS);
        // Two full pages; reading only `cap` bytes must notice the chain continues.
        let first = write_overflow(&mut sink, &pattern(2 * cap)).unwrap();
        let e = read_overflow(&sink, first, cap as u32).unwrap_err();
        assert_eq!(e.sqlstate.code(), "XX001");
        assert_eq!(detail_of(&e), "overflow chain longer than value length");
    }

    #[test]
    fn cyclic_chain_terminates_with_xx001() {
        let cap = chunk_capacity(PS);
        let len = 5 * cap + 7;
        let mut sink = MemSink::new(PS);
        let first = write_overflow(&mut sink, &pattern(len)).unwrap();
        let ids = chain_pages(&sink, first, len as u32).unwrap();
        // Last page points back to the head of the chain.
        sink.set_next(ids[5], first.0);
        let e = read_overflow(&sink, first, len as u32).unwrap_err();
        assert_eq!(e.sqlstate.code(), "XX001");
        // Middle page points back to the head: the page-count bound stops the walk.
        let mut sink2 = MemSink::new(PS);
        let first2 = write_overflow(&mut sink2, &pattern(len)).unwrap();
        let ids2 = chain_pages(&sink2, first2, len as u32).unwrap();
        sink2.set_next(ids2[2], first2.0);
        let e = chain_pages(&sink2, first2, len as u32).unwrap_err();
        assert_eq!(e.sqlstate.code(), "XX001");
    }

    #[test]
    fn wrong_kind_detected() {
        let cap = chunk_capacity(PS);
        let len = 2 * cap + 3;
        let mut sink = MemSink::new(PS);
        let first = write_overflow(&mut sink, &pattern(len)).unwrap();
        let ids = chain_pages(&sink, first, len as u32).unwrap();
        sink.write_page(ids[1], PageKind::Leaf, vec![0u8; cap + 8]).unwrap();
        let e = read_overflow(&sink, first, len as u32).unwrap_err();
        assert_eq!(e.sqlstate.code(), "XX001");
        assert_eq!(detail_of(&e), "expected overflow page, found leaf page");
    }

    #[test]
    fn short_payload_detected() {
        let cap = chunk_capacity(PS);
        let len = 2 * cap;
        let mut sink = MemSink::new(PS);
        let first = write_overflow(&mut sink, &pattern(len)).unwrap();
        sink.write_page(first, PageKind::Overflow, vec![0u8; 4]).unwrap();
        let e = read_overflow(&sink, first, len as u32).unwrap_err();
        assert_eq!(e.sqlstate.code(), "XX001");
        assert!(detail_of(&e).starts_with("overflow page payload has 4 bytes"));
    }

    #[test]
    fn wrong_chunk_length_detected() {
        let cap = chunk_capacity(PS);
        let len = 2 * cap + 3;
        let mut sink = MemSink::new(PS);
        let first = write_overflow(&mut sink, &pattern(len)).unwrap();
        let ids = chain_pages(&sink, first, len as u32).unwrap();
        // Middle page loses one chunk byte.
        let mut payload = vec![0u8; OVERFLOW_HEADER_LEN + cap - 1];
        payload[..8].copy_from_slice(&ids[2].0.to_le_bytes());
        sink.write_page(ids[1], PageKind::Overflow, payload).unwrap();
        let e = read_overflow(&sink, first, len as u32).unwrap_err();
        assert_eq!(e.sqlstate.code(), "XX001");
        assert_eq!(detail_of(&e), format!("overflow chunk has {} bytes, expected {}", cap - 1, cap));
    }

    #[test]
    fn release_visits_every_page_once() {
        let cap = chunk_capacity(PS);
        let len = 5 * cap + 7;
        let mut sink = MemSink::new(PS);
        let first = write_overflow(&mut sink, &pattern(len)).unwrap();
        let ids = chain_pages(&sink, first, len as u32).unwrap();
        release_overflow(&mut sink, first, len as u32).unwrap();
        assert_eq!(sink.released, ids);
    }

    #[test]
    fn release_of_corrupt_chain_releases_nothing() {
        let cap = chunk_capacity(PS);
        let len = 5 * cap + 7;
        let mut sink = MemSink::new(PS);
        let first = write_overflow(&mut sink, &pattern(len)).unwrap();
        let ids = chain_pages(&sink, first, len as u32).unwrap();
        sink.set_next(ids[3], 0);
        assert!(release_overflow(&mut sink, first, len as u32).is_err());
        assert!(sink.released.is_empty());
    }

    #[test]
    fn copy_preserves_bytes() {
        let cap = chunk_capacity(PS);
        for len in [1, cap, cap + 1, 5 * cap + 7] {
            let value = pattern(len);
            let mut src = MemSink::new(PS);
            let first = write_overflow(&mut src, &value).unwrap();
            let mut dst = MemSink::new(PS);
            let copied = copy_overflow(&src, &mut dst, first, len as u32).unwrap();
            assert_eq!(read_overflow(&dst, copied, len as u32).unwrap(), value);
            assert_eq!(
                chain_pages(&dst, copied, len as u32).unwrap().len() as u64,
                pages_for(PS, len as u64)
            );
        }
    }

    #[test]
    fn copy_rejects_corrupt_source() {
        let cap = chunk_capacity(PS);
        let len = 3 * cap;
        let mut src = MemSink::new(PS);
        let first = write_overflow(&mut src, &pattern(len)).unwrap();
        let ids = chain_pages(&src, first, len as u32).unwrap();
        src.set_next(ids[1], 0);
        let mut dst = MemSink::new(PS);
        let e = copy_overflow(&src, &mut dst, first, len as u32).unwrap_err();
        assert_eq!(e.sqlstate.code(), "XX001");
    }
}
