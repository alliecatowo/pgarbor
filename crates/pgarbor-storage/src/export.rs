//! Snapshot-consistent streaming export into a fresh standalone container (STORE-17).
//! Lease STO-16 (sonnet). `Drop` and accessors are implemented.
//!
//! `ExportBuilder` writes a NEW `.pga` (fresh UUID by default) through a temp file
//! `<dest><EXPORT_TEMP_SUFFIX>`, then atomically renames it to `dest`. Sources are `Snapshot`s (or
//! any `PageSource`), which are immutable, so exports run while the source container keeps
//! committing. Memory is O(copier memo + write buffer), not O(database).
//!
//! Typical use by pgarbor-branches (`branch export`/detach):
//! ```ignore
//! let snap = container.snapshot();
//! let mut b = ExportBuilder::create(container.vfs().clone(), Path::new("feature.pga"), &ExportOptions::default())?;
//! let data = b.copy_tree(&snap, branch.data_root)?;                 // shares nothing with source
//! let dir = b.build_tree(&mut new_directory_entries(data).into_iter().map(Ok))?;
//! let report = b.finish(dir)?;                                       // synced + renamed
//! ```
//!
//! The builder is itself a `PageSink`: every page it allocates is private (the destination has no
//! committed pages until `finish`).
#[allow(unused_imports)]
use crate::container::options::SyncMode;
#[allow(unused_imports)]
use crate::container::recovery::EXPORT_TEMP_SUFFIX;
#[allow(unused_imports)]
use crate::error;
use crate::format::consts::{PageKind, DEFAULT_PAGE_SIZE};
#[allow(unused_imports)]
use crate::format::page::{encode_page, Page, PageRef};
use crate::format::superblock::Superblock;
use crate::gc::copy::TreeCopier;
use crate::io::{FileIo, Vfs};
use crate::pager::{PageSink, PageSource};
use crate::roots::RootResolver;
use pgarbor_common::{CommitSeq, DatabaseUuid, PageId, PgError, Result};
use std::collections::{BTreeMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;

// CONTRACT
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ExportOptions {
    pub page_size: u32,
    /// `None` → fresh `random_db_uuid()` (an export is a new database identity, ARCHITECTURE §2).
    pub db_uuid: Option<DatabaseUuid>,
    /// `None` → now.
    pub created_unix_ms: Option<u64>,
    /// Commit sequence stamped into the new file's pages and superblock (default 1; compaction
    /// uses old + 1).
    pub commit_seq: CommitSeq,
    pub sync: SyncMode,
    /// Replace an existing destination (rename over it). Default false → existing dest is an
    /// error (`error::open_failed` with AlreadyExists → 58P02).
    pub overwrite: bool,
    /// Write buffer flushed when it exceeds this many bytes (default 8 MiB).
    pub buffer_bytes: usize,
}

impl Default for ExportOptions {
    fn default() -> Self {
        ExportOptions {
            page_size: DEFAULT_PAGE_SIZE,
            db_uuid: None,
            created_unix_ms: None,
            commit_seq: CommitSeq(1),
            sync: SyncMode::Full,
            overwrite: false,
            buffer_bytes: 8 << 20,
        }
    }
}

// CONTRACT
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize)]
pub struct ExportReport {
    pub path: PathBuf,
    pub db_uuid: DatabaseUuid,
    pub commit_seq: CommitSeq,
    pub page_count: u64,
    pub file_len: u64,
    pub pages_copied: u64,
}

// CONTRACT
pub struct ExportBuilder {
    pub(crate) vfs: Arc<dyn Vfs>,
    pub(crate) final_path: PathBuf,
    pub(crate) tmp_path: PathBuf,
    /// `None` after `finish*` handed it out.
    pub(crate) file: Option<Box<dyn FileIo>>,
    pub(crate) opts: ExportOptions,
    /// Superblock template (uuid/created/page_size/commit_seq fixed at create).
    pub(crate) sb: Superblock,
    pub(crate) next_page: u64,
    /// Unflushed pages (id → page); flushed in id order when over `buffer_bytes`.
    pub(crate) pending: BTreeMap<u64, PageRef>,
    pub(crate) pending_bytes: usize,
    /// Ids written to the temp file already.
    pub(crate) flushed: HashSet<u64>,
    pub(crate) free: Vec<PageId>,
    pub(crate) copier: TreeCopier,
    pub(crate) finished: bool,
}

impl std::fmt::Debug for ExportBuilder {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ExportBuilder")
            .field("final_path", &self.final_path)
            .field("tmp_path", &self.tmp_path)
            .field("next_page", &self.next_page)
            .finish()
    }
}

impl ExportBuilder {
    /// Start an export to `dest` using temp `sibling_path(dest, EXPORT_TEMP_SUFFIX)`.
    /// Errors: invalid page size → 22023 (as `Container::create`); `dest` exists and
    /// `!opts.overwrite` → `error::open_failed(dest, AlreadyExists)` (58P02).
    pub fn create(vfs: Arc<dyn Vfs>, dest: &Path, opts: &ExportOptions) -> Result<ExportBuilder> {
        let tmp = crate::io::sibling_path(dest, EXPORT_TEMP_SUFFIX);
        ExportBuilder::create_internal(vfs, dest, tmp, opts)
    }

    /// Shared constructor (compaction passes its own temp path and `overwrite = true`). Steps:
    /// validate page size; existence check of `dest` per `overwrite`; remove a stale `tmp` if it
    /// exists; `vfs.open(tmp, CreateTruncate)`; `try_lock(Exclusive)` (WouldBlock →
    /// `error::locked(tmp)`); `set_len(PAGES_OFFSET)` (both slots zero = not yet a valid
    /// database, so a crash leaves an unopenable temp, never a half-valid file); build `sb` =
    /// `Superblock::new_empty(..)` with `commit_seq = opts.commit_seq`; `next_page = 1`.
    #[allow(unused_variables)]
    pub(crate) fn create_internal(
        vfs: Arc<dyn Vfs>,
        dest: &Path,
        tmp: PathBuf,
        opts: &ExportOptions,
    ) -> Result<ExportBuilder> {
        Err(PgError::unimplemented("STO-16", "export builder creation"))
    }

    pub fn dest(&self) -> &Path {
        &self.final_path
    }
    pub fn db_uuid(&self) -> DatabaseUuid {
        self.sb.db_uuid
    }
    pub fn pages_written(&self) -> u64 {
        self.next_page - 1
    }

    /// Copy one tree from `src` (sharing-preserving across calls via the internal memo).
    /// Implementation note: temporarily `std::mem::take(&mut self.copier)` so `self` can be the
    /// sink, then put it back (also on error).
    #[allow(unused_variables)]
    pub fn copy_tree(&mut self, src: &dyn PageSource, root: PageId) -> Result<PageId> {
        Err(PgError::unimplemented("STO-16", "export tree copy"))
    }

    /// Copy a directory tree, renumbering embedded roots via `resolver` (see
    /// `TreeCopier::copy_directory`).
    #[allow(unused_variables)]
    pub fn copy_directory(&mut self, src: &dyn PageSource, dir_root: PageId, resolver: &dyn RootResolver) -> Result<PageId> {
        Err(PgError::unimplemented("STO-16", "export directory copy"))
    }

    /// Bulk-build a new tree in the destination (`btree::bulk::build_from_sorted`).
    #[allow(unused_variables)]
    pub fn build_tree(&mut self, items: &mut dyn Iterator<Item = Result<(Vec<u8>, Vec<u8>)>>) -> Result<PageId> {
        Err(PgError::unimplemented("STO-16", "export tree build"))
    }

    /// Insert into a destination tree (`btree::insert::put`); returns the new root.
    #[allow(unused_variables)]
    pub fn put(&mut self, root: PageId, key: &[u8], value: &[u8]) -> Result<PageId> {
        Err(PgError::unimplemented("STO-16", "export tree insert"))
    }

    /// Flush `pending` (encode with `sb.commit_seq`, write in id order via `write_all_at` at
    /// `page_offset`), mark ids flushed. io errors → `error::write_failed(tmp, e)`.
    pub(crate) fn flush(&mut self) -> Result<()> {
        Err(PgError::unimplemented("STO-16", "export buffer flush"))
    }

    /// Complete WITHOUT renaming (compaction): flush; `page_count = next_page - 1` minus trailing
    /// free ids; ensure file len == `file_len_for(ps, page_count)` (`set_len`); slot 0 = sb with
    /// `directory_root`/`page_count`, slot 1 = zeros; `sync_all` (Full). Returns the still-locked
    /// handle and the superblock. Marks finished (Drop no longer removes the temp).
    #[allow(unused_variables)]
    pub(crate) fn finish_in_place(mut self, directory_root: PageId) -> Result<(Box<dyn FileIo>, Superblock)> {
        Err(PgError::unimplemented("STO-16", "export finish in place"))
    }

    /// Complete the export: `finish_in_place`, then `vfs.rename(tmp, dest)`
    /// (`error::rename_failed`), then `sync_dir(parent_dir(dest))` (Full), drop the handle
    /// (unlock). On any failure the temp file is removed best-effort and `dest` is untouched
    /// (unless the failure is the final dir sync, after which dest is complete but its name may
    /// not be durable — reported as the error).
    #[allow(unused_variables)]
    pub fn finish(self, directory_root: PageId) -> Result<ExportReport> {
        Err(PgError::unimplemented("STO-16", "export finish"))
    }

    /// Abandon: drop the handle and remove the temp file.
    pub fn abort(mut self) -> Result<()> {
        self.finished = true;
        self.file = None;
        match self.vfs.remove_file(&self.tmp_path) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(crate::error::write_failed(&self.tmp_path, &e)),
        }
    }
}

impl Drop for ExportBuilder {
    fn drop(&mut self) {
        if !self.finished {
            self.file = None;
            let _ = self.vfs.remove_file(&self.tmp_path);
        }
    }
}

impl PageSource for ExportBuilder {
    fn page_size(&self) -> u32 {
        self.sb.page_size
    }
    /// `pending` → clone Arc; `flushed` → read `page_size` bytes from the temp file and
    /// `decode_page` (uncached); else `error::page_out_of_range(id, next_page - 1)`.
    #[allow(unused_variables)]
    fn read_page(&self, id: PageId) -> Result<PageRef> {
        Err(PgError::unimplemented("STO-16", "export page read"))
    }
}

impl PageSink for ExportBuilder {
    /// Pop `free`, else `next_page++`.
    fn alloc_page(&mut self) -> Result<PageId> {
        Err(PgError::unimplemented("STO-16", "export page allocation"))
    }
    /// Insert into `pending` (commit_seq = `sb.commit_seq`), flush when over `buffer_bytes`.
    #[allow(unused_variables)]
    fn write_page(&mut self, id: PageId, kind: PageKind, payload: Vec<u8>) -> Result<()> {
        Err(PgError::unimplemented("STO-16", "export page write"))
    }
    /// Every allocated, unreleased id is private.
    fn is_private(&self, id: PageId) -> bool {
        id.0 >= 1 && id.0 < self.next_page && !self.free.contains(&id)
    }
    /// Remove from pending/flushed, push onto `free`.
    #[allow(unused_variables)]
    fn release_page(&mut self, id: PageId) -> Result<()> {
        Err(PgError::unimplemented("STO-16", "export page release"))
    }
}
