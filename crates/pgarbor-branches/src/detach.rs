//! `pgarbor branch detach` — make an independent `.pga` for one branch, preferring a filesystem
//! reflink clone (RFC 001 §4.2). Lease BR-11 (sonnet).
//!
//! # `detach_branch` strategy selection
//! - `Copy` → `export::export_branch` (always compact).
//! - `ReflinkRequired` → `reflink_detach`; `Unsupported{reason}` (or `!opts.os_filesystem`, or
//!   `!reflink::platform_has_reflink()`) → `error::reflink_unsupported(container.path(), reason)`.
//! - `Auto` → `reflink_detach`; on `Unsupported{reason}` fall back to `export_branch` and set
//!   `fallback_reason = Some(reason)` (the CLI prints that the copy incurs data-proportional I/O).
//!
//! # `reflink_detach` steps (temp = `sibling_path(dest, DETACH_TEMP_SUFFIX)`)
//! 1. Validate `opts.target_name`; dest exists && !overwrite → `destination_exists`; remove stale temp.
//! 2. BARRIER: unless `container.is_read_only()`, `let w = container.begin_write()?` and keep it
//!    alive across steps 3-4 (no commit or spill can be mid-write, so the file's committed state
//!    equals the latest snapshot). Then `snap = container.snapshot()`, `rec = directory::resolve`.
//! 3. `reflink::clone_file(container.path(), &temp)`; Unsupported → drop the barrier, return the
//!    reason to the caller (no temp left behind).
//! 4. Drop the barrier (`w.abort()`).
//! 5. `reidentify_file(vfs, &temp, new_uuid, opts.sync)`.
//! 6. Open temp (`Container::open_with`, writable, `VerifyLevel::Roots`), `prune_to_branch(..)`.
//! 7. If `opts.compact_after_reflink` → `c.compact(&BranchRootResolver, &CompactOptions::default())`.
//! 8. Drop the container; `export::verify_and_publish(vfs, &temp, dest, opts, &new_rec)`.
//! Any error after step 3 removes the temp best-effort.
//!
//! The clone initially shares extents with the source; pages of other branches remain as
//! unreachable garbage inside it (`DetachReport::garbage_pages`) until `pgarbor compact`.
use crate::export::PublishedFile;
use crate::meta::ExportOrigin;
use crate::options::{BranchRef, DetachOptions};
use crate::record::BranchRecord;
use pgarbor_common::{BranchId, CommitSeq, DatabaseUuid, PgError, Result};
use pgarbor_storage::{Container, SyncMode, Vfs};
use std::path::{Path, PathBuf};

// CONTRACT
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum StrategyUsed {
    Copy,
    Reflink,
}

// CONTRACT
/// Result of export/detach (also the `--json` output of the CLI).
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize)]
pub struct DetachReport {
    pub path: PathBuf,
    pub strategy_used: StrategyUsed,
    /// Why `Auto` fell back to a copy (None when it did not).
    pub fallback_reason: Option<String>,
    pub origin: ExportOrigin,
    pub new_db_uuid: DatabaseUuid,
    pub new_branch_id: BranchId,
    pub new_branch_name: String,
    pub page_count: u64,
    pub file_len: u64,
    /// Unreachable pages left in the new file (0 for copy; other branches' pages after reflink).
    pub garbage_pages: u64,
    pub compacted: bool,
    /// Always true on `Ok` (the file was reopened and verified before success).
    pub verified: bool,
}

impl DetachReport {
    /// Assemble a report from its parts (shared by export and detach).
    pub fn from_parts(
        dest: &Path,
        strategy_used: StrategyUsed,
        fallback_reason: Option<String>,
        origin: ExportOrigin,
        rec: &BranchRecord,
        published: &PublishedFile,
        compacted: bool,
    ) -> DetachReport {
        DetachReport {
            path: dest.to_path_buf(),
            strategy_used,
            fallback_reason,
            origin,
            new_db_uuid: published.db_uuid,
            new_branch_id: rec.id,
            new_branch_name: rec.name.clone(),
            page_count: published.page_count,
            file_len: published.file_len,
            garbage_pages: published.garbage_pages,
            compacted,
            verified: true,
        }
    }
}

/// Detach per `opts.strategy` (module docs).
#[allow(unused_variables)]
pub fn detach_branch(container: &Container, branch: &BranchRef, dest: &Path, opts: &DetachOptions) -> Result<DetachReport> {
    Err(PgError::unimplemented("BR-11", "branch detach"))
}

/// Reflink path; `Ok(Err(reason))` = unsupported, nothing left behind.
#[allow(unused_variables)]
pub fn reflink_detach(
    container: &Container,
    branch: &BranchRef,
    dest: &Path,
    opts: &DetachOptions,
) -> Result<std::result::Result<DetachReport, String>> {
    Err(PgError::unimplemented("BR-11", "reflink detach"))
}

/// Give a CLOSED, not-yet-published database file a new identity. Only safe on a private temp file
/// (a crash mid-way leaves an unopenable temp, which is fine because it was never published).
/// Steps: `f = vfs.open(path, ReadWrite)`; `try_lock(Exclusive)` (WouldBlock →
/// `pgarbor_storage::error::locked`); `slots = pgarbor_storage::container::read_slots(&*f, path)`;
/// `choice = pgarbor_storage::format::choose_slot(path, &slots)`; `sb = choice.superblock` with
/// `db_uuid = new_uuid`; write `sb.encode()` at slot offset `choice.slot * 4096` and 4096 zero bytes
/// at the OTHER slot (two valid slots with different UUIDs are refused by `choose_slot`, so the
/// older slot must be erased); `sync_data` when `sync == Full`.
/// See amendment `pgarbor-branches-02` (proposes moving this into storage as `reidentify`).
#[allow(unused_variables)]
pub fn reidentify_file(vfs: &dyn Vfs, path: &Path, new_uuid: DatabaseUuid, sync: SyncMode) -> Result<()> {
    Err(PgError::unimplemented("BR-11", "database file re-identification"))
}

/// Rewrite the (cloned) container's directory so it holds exactly one branch: `keep`'s roots
/// under a new record `new_active(new_id, new_name, None, keep.data_root, w.commit_seq(), now)`
/// (+ `keep.snapshots` as `Inherited` when `include_snapshots`), built with
/// `w.build_tree(export::standalone_directory_entries(..))` into a FRESH directory tree,
/// `set_directory_root`, `commit`. Returns the new record and the commit seq.
#[allow(unused_variables)]
pub fn prune_to_branch(
    c: &Container,
    keep: &BranchRecord,
    new_id: BranchId,
    new_name: &str,
    include_snapshots: bool,
    origin: &ExportOrigin,
) -> Result<(BranchRecord, CommitSeq)> {
    Err(PgError::unimplemented("BR-11", "post-clone prune"))
}
