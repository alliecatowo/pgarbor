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
use crate::container::options::SyncMode;
use crate::container::recovery::EXPORT_TEMP_SUFFIX;
use crate::error;
use crate::format::consts::{
    file_len_for, is_valid_page_size, page_offset, payload_capacity, slot_offset, PageKind, DEFAULT_PAGE_SIZE,
    PAGES_OFFSET, SUPERBLOCK_SLOT_SIZE,
};
use crate::format::page::{decode_page, encode_page, Page, PageRef};
use crate::format::superblock::{now_unix_ms, random_db_uuid, Superblock};
use crate::gc::copy::TreeCopier;
use crate::io::{parent_dir, FileIo, LockMode, OpenMode, Vfs};
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
    pub(crate) fn create_internal(
        vfs: Arc<dyn Vfs>,
        dest: &Path,
        tmp: PathBuf,
        opts: &ExportOptions,
    ) -> Result<ExportBuilder> {
        if !is_valid_page_size(opts.page_size) {
            return Err(PgError::new(
                pgarbor_common::SqlState::INVALID_PARAMETER_VALUE,
                format!("invalid page size {}", opts.page_size),
            )
            .with_detail("Page size must be a power of two between 4096 and 65536."));
        }
        if opts.commit_seq.0 == 0 {
            return Err(PgError::internal("export commit sequence must be at least 1"));
        }
        if !opts.overwrite {
            let exists = vfs.exists(dest).map_err(|e| error::open_failed(dest, &e))?;
            if exists {
                return Err(error::open_failed(dest, &std::io::Error::from(std::io::ErrorKind::AlreadyExists)));
            }
        }
        match vfs.remove_file(&tmp) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(error::write_failed(&tmp, &e)),
        }
        let file = vfs.open(&tmp, OpenMode::CreateTruncate).map_err(|e| error::open_failed(&tmp, &e))?;
        // From here on, a failure must not leave the temp file behind.
        let setup = match file.try_lock(LockMode::Exclusive) {
            Ok(()) => file.set_len(PAGES_OFFSET).map_err(|e| error::truncate_failed(&tmp, PAGES_OFFSET, &e)),
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => Err(error::locked(&tmp)),
            Err(e) => Err(error::open_failed(&tmp, &e)),
        };
        if let Err(e) = setup {
            drop(file);
            let _ = vfs.remove_file(&tmp);
            return Err(e);
        }
        let mut sb = Superblock::new_empty(
            opts.page_size,
            opts.db_uuid.unwrap_or_else(random_db_uuid),
            opts.created_unix_ms.unwrap_or_else(now_unix_ms),
        );
        sb.commit_seq = opts.commit_seq;
        Ok(ExportBuilder {
            vfs,
            final_path: dest.to_path_buf(),
            tmp_path: tmp,
            file: Some(file),
            opts: opts.clone(),
            sb,
            next_page: 1,
            pending: BTreeMap::new(),
            pending_bytes: 0,
            flushed: HashSet::new(),
            free: Vec::new(),
            copier: TreeCopier::new(),
            finished: false,
        })
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
    pub fn copy_tree(&mut self, src: &dyn PageSource, root: PageId) -> Result<PageId> {
        let mut copier = std::mem::take(&mut self.copier);
        let r = copier.copy_tree(src, self, root);
        self.copier = copier;
        r
    }

    /// Copy a directory tree, renumbering embedded roots via `resolver` (see
    /// `TreeCopier::copy_directory`).
    pub fn copy_directory(&mut self, src: &dyn PageSource, dir_root: PageId, resolver: &dyn RootResolver) -> Result<PageId> {
        let mut copier = std::mem::take(&mut self.copier);
        let r = copier.copy_directory(src, self, dir_root, resolver);
        self.copier = copier;
        r
    }

    /// Bulk-build a new tree in the destination (`btree::bulk::build_from_sorted`).
    pub fn build_tree(&mut self, items: &mut dyn Iterator<Item = Result<(Vec<u8>, Vec<u8>)>>) -> Result<PageId> {
        crate::btree::bulk::build_from_sorted(self, items)
    }

    /// Insert into a destination tree (`btree::insert::put`); returns the new root.
    pub fn put(&mut self, root: PageId, key: &[u8], value: &[u8]) -> Result<PageId> {
        crate::btree::insert::put(self, root, key, value).map(|r| r.root)
    }

    /// Flush `pending` (encode with `sb.commit_seq`, write in id order via `write_all_at` at
    /// `page_offset`), mark ids flushed. io errors → `error::write_failed(tmp, e)`.
    pub(crate) fn flush(&mut self) -> Result<()> {
        if self.pending.is_empty() {
            self.pending_bytes = 0;
            return Ok(());
        }
        let ps = self.sb.page_size;
        let pending = std::mem::take(&mut self.pending);
        self.pending_bytes = 0;
        let file = match self.file.as_ref() {
            Some(f) => f,
            None => return Err(PgError::internal("export builder used after finish")),
        };
        // Coalesce runs of consecutive ids into single writes.
        let mut run: Vec<u8> = Vec::new();
        let mut run_start: u64 = 0;
        let mut next_id: u64 = 0;
        for (id, page) in pending.iter() {
            if !run.is_empty() && *id != next_id {
                file.write_all_at(page_offset(ps, PageId(run_start)), &run)
                    .map_err(|e| error::write_failed(&self.tmp_path, &e))?;
                run.clear();
            }
            if run.is_empty() {
                run_start = *id;
            }
            run.extend_from_slice(&encode_page(ps, page)?);
            next_id = *id + 1;
        }
        if !run.is_empty() {
            file.write_all_at(page_offset(ps, PageId(run_start)), &run)
                .map_err(|e| error::write_failed(&self.tmp_path, &e))?;
        }
        for id in pending.keys() {
            self.flushed.insert(*id);
        }
        Ok(())
    }

    /// Complete WITHOUT renaming (compaction): flush; `page_count = next_page - 1` minus trailing
    /// free ids; ensure file len == `file_len_for(ps, page_count)` (`set_len`); slot 0 = sb with
    /// `directory_root`/`page_count`, slot 1 = zeros; `sync_all` (Full). Returns the still-locked
    /// handle and the superblock. Marks finished (Drop no longer removes the temp).
    pub(crate) fn finish_in_place(mut self, directory_root: PageId) -> Result<(Box<dyn FileIo>, Superblock)> {
        self.flush()?;
        let ps = self.sb.page_size;
        let mut page_count = self.next_page - 1;
        while page_count > 0 && self.free.contains(&PageId(page_count)) {
            page_count -= 1;
        }
        if directory_root.0 > page_count {
            return Err(PgError::internal(format!(
                "export directory root {} is beyond page count {}",
                directory_root.0, page_count
            )));
        }
        let sync = self.opts.sync == SyncMode::Full;
        let len = file_len_for(ps, page_count);
        let tmp = self.tmp_path.clone();
        let file = match self.file.take() {
            Some(f) => f,
            None => return Err(PgError::internal("export builder used after finish")),
        };
        // On any error below, dropping `self` (finished == false) removes the temp file.
        let cur = file.len().map_err(|e| error::read_failed(&tmp, &e))?;
        if cur != len {
            file.set_len(len).map_err(|e| error::truncate_failed(&tmp, len, &e))?;
        }
        if sync {
            file.sync_data().map_err(|e| error::fsync_failed(&tmp, &e))?;
        }
        let mut sb = self.sb.clone();
        sb.directory_root = directory_root;
        sb.page_count = page_count;
        let slot0 = sb.encode()?;
        file.write_all_at(slot_offset(0), &slot0).map_err(|e| error::write_failed(&tmp, &e))?;
        let zeros = vec![0u8; SUPERBLOCK_SLOT_SIZE];
        file.write_all_at(slot_offset(1), &zeros).map_err(|e| error::write_failed(&tmp, &e))?;
        if sync {
            file.sync_all().map_err(|e| error::fsync_failed(&tmp, &e))?;
        }
        self.finished = true;
        Ok((file, sb))
    }

    /// Complete the export: `finish_in_place`, then `vfs.rename(tmp, dest)`
    /// (`error::rename_failed`), then `sync_dir(parent_dir(dest))` (Full), drop the handle
    /// (unlock). On any failure the temp file is removed best-effort and `dest` is untouched
    /// (unless the failure is the final dir sync, after which dest is complete but its name may
    /// not be durable — reported as the error).
    pub fn finish(self, directory_root: PageId) -> Result<ExportReport> {
        let vfs = Arc::clone(&self.vfs);
        let tmp = self.tmp_path.clone();
        let dest = self.final_path.clone();
        let sync = self.opts.sync == SyncMode::Full;
        let pages_copied = self.copier.pages_copied;
        let (file, sb) = self.finish_in_place(directory_root)?;
        if let Err(e) = vfs.rename(&tmp, &dest) {
            drop(file);
            let _ = vfs.remove_file(&tmp);
            return Err(error::rename_failed(&tmp, &dest, &e));
        }
        drop(file);
        if sync {
            let dir = parent_dir(&dest);
            vfs.sync_dir(&dir).map_err(|e| error::fsync_failed(&dir, &e))?;
        }
        Ok(ExportReport {
            path: dest,
            db_uuid: sb.db_uuid,
            commit_seq: sb.commit_seq,
            page_count: sb.page_count,
            file_len: file_len_for(sb.page_size, sb.page_count),
            pages_copied,
        })
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
    fn read_page(&self, id: PageId) -> Result<PageRef> {
        if let Some(p) = self.pending.get(&id.0) {
            return Ok(Arc::clone(p));
        }
        if self.flushed.contains(&id.0) {
            let file = match self.file.as_ref() {
                Some(f) => f,
                None => return Err(PgError::internal("export builder used after finish")),
            };
            let ps = self.sb.page_size;
            let mut buf = vec![0u8; ps as usize];
            file.read_exact_at(page_offset(ps, id), &mut buf).map_err(|e| error::read_failed(&self.tmp_path, &e))?;
            return Ok(Arc::new(decode_page(ps, id, &buf)?));
        }
        Err(error::page_out_of_range(id, self.next_page - 1))
    }
}

impl PageSink for ExportBuilder {
    /// Pop `free`, else `next_page++`.
    fn alloc_page(&mut self) -> Result<PageId> {
        if let Some(id) = self.free.pop() {
            return Ok(id);
        }
        let id = PageId(self.next_page);
        self.next_page += 1;
        Ok(id)
    }
    /// Insert into `pending` (commit_seq = `sb.commit_seq`), flush when over `buffer_bytes`.
    fn write_page(&mut self, id: PageId, kind: PageKind, payload: Vec<u8>) -> Result<()> {
        if !self.is_private(id) {
            return Err(PgError::internal(format!("export write to non-private page {}", id.0)));
        }
        let cap = payload_capacity(self.sb.page_size);
        if payload.len() > cap {
            return Err(PgError::internal(format!(
                "page payload of {} bytes exceeds capacity {}",
                payload.len(),
                cap
            )));
        }
        let page = Arc::new(Page { id, kind, commit_seq: self.sb.commit_seq, payload });
        if self.pending.insert(id.0, page).is_none() {
            self.pending_bytes += self.sb.page_size as usize;
        }
        if self.pending_bytes > self.opts.buffer_bytes {
            self.flush()?;
        }
        Ok(())
    }
    /// Every allocated, unreleased id is private.
    fn is_private(&self, id: PageId) -> bool {
        id.0 >= 1 && id.0 < self.next_page && !self.free.contains(&id)
    }
    /// Remove from pending/flushed, push onto `free`.
    fn release_page(&mut self, id: PageId) -> Result<()> {
        if !self.is_private(id) {
            return Err(PgError::internal(format!("export release of non-private page {}", id.0)));
        }
        if self.pending.remove(&id.0).is_some() {
            self.pending_bytes = self.pending_bytes.saturating_sub(self.sb.page_size as usize);
        }
        self.flushed.remove(&id.0);
        self.free.push(id);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::container::handle::Container;
    use crate::container::options::OpenOptions;
    use crate::io::MemVfs;

    fn items(n: usize) -> Vec<(Vec<u8>, Vec<u8>)> {
        (0..n)
            .map(|i| {
                let k = format!("key{i:08}").into_bytes();
                let v = if i % 1000 == 7 { vec![(i % 251) as u8; 50_000] } else { format!("value-{i}").into_bytes() };
                (k, v)
            })
            .collect()
    }

    fn opts() -> ExportOptions {
        ExportOptions { sync: SyncMode::Off, ..ExportOptions::default() }
    }

    #[test]
    fn export_roundtrip_opens_and_matches() {
        let vfs = Arc::new(MemVfs::new());
        let dest = Path::new("/x/out.pga");
        let all = items(10_000);
        let o = ExportOptions { buffer_bytes: 64 * 1024, ..opts() };
        let mut b = ExportBuilder::create(vfs.clone(), dest, &o).unwrap();
        let root = b.build_tree(&mut all.clone().into_iter().map(Ok)).unwrap();
        let uuid = b.db_uuid();
        let rep = b.finish(root).unwrap();
        assert_eq!(rep.db_uuid, uuid);
        assert_eq!(rep.file_len, file_len_for(DEFAULT_PAGE_SIZE, rep.page_count));
        assert!(!vfs.paths().iter().any(|p| p.to_string_lossy().ends_with(EXPORT_TEMP_SUFFIX)));
        let oo = OpenOptions { sync: SyncMode::Off, ..OpenOptions::default() };
        let c = Container::open_with(vfs.clone(), dest, &oo).unwrap();
        assert_eq!(c.db_uuid(), uuid);
        let snap = c.snapshot();
        assert_eq!(snap.directory_root(), root);
        for (k, v) in all.iter().step_by(97) {
            assert_eq!(crate::btree::read::get(&snap, root, k).unwrap().as_deref(), Some(v.as_slice()));
        }
        let big = &all[7];
        assert_eq!(crate::btree::read::get(&snap, root, &big.0).unwrap().unwrap(), big.1);
    }

    #[test]
    fn dest_exists_is_58p02() {
        let vfs = Arc::new(MemVfs::new());
        let dest = Path::new("/x/out.pga");
        vfs.write_bytes(dest, vec![1, 2, 3]);
        let e = ExportBuilder::create(vfs.clone(), dest, &opts()).unwrap_err();
        assert_eq!(e.sqlstate.0, *b"58P02");
        let ok = ExportBuilder::create(vfs, dest, &ExportOptions { overwrite: true, ..opts() });
        assert!(ok.is_ok());
    }

    #[test]
    fn bad_page_size_is_22023() {
        let vfs = Arc::new(MemVfs::new());
        let e = ExportBuilder::create(vfs, Path::new("/x/o.pga"), &ExportOptions { page_size: 1000, ..opts() })
            .unwrap_err();
        assert_eq!(e.sqlstate.0, *b"22023");
    }

    #[test]
    fn abort_and_drop_remove_temp() {
        let vfs = Arc::new(MemVfs::new());
        let dest = Path::new("/x/out.pga");
        let tmp = crate::io::sibling_path(dest, EXPORT_TEMP_SUFFIX);
        let b = ExportBuilder::create(vfs.clone(), dest, &opts()).unwrap();
        assert!(vfs.exists(&tmp).unwrap());
        b.abort().unwrap();
        assert!(!vfs.exists(&tmp).unwrap());
        let b = ExportBuilder::create(vfs.clone(), dest, &opts()).unwrap();
        assert!(vfs.exists(&tmp).unwrap());
        drop(b);
        assert!(!vfs.exists(&tmp).unwrap());
        assert!(!vfs.exists(dest).unwrap());
    }

    #[test]
    fn released_pages_are_reused() {
        let vfs = Arc::new(MemVfs::new());
        let mut b = ExportBuilder::create(vfs, Path::new("/x/o.pga"), &opts()).unwrap();
        let a = b.alloc_page().unwrap();
        let c = b.alloc_page().unwrap();
        assert!(b.is_private(a) && b.is_private(c));
        b.release_page(c).unwrap();
        assert!(!b.is_private(c));
        assert_eq!(b.alloc_page().unwrap(), c);
        assert!(b.write_page(PageId(99), PageKind::Leaf, vec![]).is_err());
    }
}
