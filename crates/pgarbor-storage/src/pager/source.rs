//! Page source/sink traits. CONTRACT. Fully specified; no lease.
//!
//! The B+tree code (`crate::btree`) is written only against these traits, so the same algorithms
//! serve committed snapshots ([`crate::Snapshot`]), the single writer ([`crate::WriteTxn`]) and
//! export/compaction destinations ([`crate::ExportBuilder`]).
use crate::format::consts::PageKind;
use crate::format::page::PageRef;
use pgarbor_common::{PageId, Result};

// CONTRACT
/// Read access to verified pages.
pub trait PageSource {
    /// Page size of the underlying file.
    fn page_size(&self) -> u32;
    /// Read page `id` (verified: checksum, magic, id, bounds). `PageId::NONE` → corruption error.
    /// Implementations bound-check `id` against what they may see (e.g. a snapshot's page_count)
    /// with `error::page_out_of_range`, and reject a page whose header `commit_seq` is newer than
    /// the reader's commit with `error::invalid_page(id, "page written by commit <x>, newer than
    /// snapshot commit <y>")`.
    fn read_page(&self, id: PageId) -> Result<PageRef>;
}

// CONTRACT
/// Write access for building/mutating trees. Pages written through a sink become visible to the
/// sink's own `read_page` immediately; they become durable/visible to others only through the
/// owner's commit/finish protocol.
///
/// Copy-on-write rule: a page is *private* if it was allocated by this sink (not reachable from any
/// committed superblock). Private pages may be overwritten in place with `write_page` and released
/// for reuse; shared (committed) pages are never written — the tree code allocates a replacement
/// and calls `release_page(old)` (which, for a shared page, only updates space accounting).
pub trait PageSink: PageSource {
    /// Allocate a fresh private PageId (reusing released private ids first is allowed).
    fn alloc_page(&mut self) -> Result<PageId>;
    /// Write (or overwrite) private page `id`. Errors: `PgError::internal` if `id` is not private
    /// or `payload.len() > payload_capacity(page_size)`.
    fn write_page(&mut self, id: PageId, kind: PageKind, payload: Vec<u8>) -> Result<()>;
    /// Whether `id` is private to this sink.
    fn is_private(&self, id: PageId) -> bool;
    /// The tree no longer references `id` (after a CoW replacement, merge, or value overwrite).
    /// Private → may be reused by a later `alloc_page`; shared → accounting only.
    fn release_page(&mut self, id: PageId) -> Result<()>;

    /// Allocate and write a new page in one step. Fully implemented.
    fn write_new(&mut self, kind: PageKind, payload: Vec<u8>) -> Result<PageId> {
        let id = self.alloc_page()?;
        self.write_page(id, kind, payload)?;
        Ok(id)
    }

    /// Copy-on-write replace: if `old` is private, overwrite it in place and return `old`;
    /// otherwise write a new page, release `old` (when not NONE) and return the new id.
    /// Fully implemented; the B+tree uses this for every node rewrite.
    fn cow_write(&mut self, old: PageId, kind: PageKind, payload: Vec<u8>) -> Result<PageId> {
        if !old.is_none() && self.is_private(old) {
            self.write_page(old, kind, payload)?;
            Ok(old)
        } else {
            let id = self.write_new(kind, payload)?;
            if !old.is_none() {
                self.release_page(old)?;
            }
            Ok(id)
        }
    }
}
