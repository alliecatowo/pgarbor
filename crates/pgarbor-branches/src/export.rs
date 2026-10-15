//! Standalone compact export of one branch into a NEW `.pga` (BRANCH-09, RFC 001 §4.3).
//! Lease BR-10 (sonnet).
//!
//! `pgarbor branch export X --out X.pga` = `detach` with `DetachStrategy::Copy`. The result has a
//! fresh `DatabaseUuid`, exactly one branch (named `opts.target_name`, fresh `BranchId`, no
//! parent, made the default), only the pages reachable from that branch's roots, and a `Morigin`
//! provenance entry. The source keeps committing during the export (it reads one immutable
//! `Snapshot`).
//!
//! # Steps of `export_branch` (each failure removes the temp file best-effort; `dest` untouched)
//! 1. `snap = container.snapshot()`; `rec = directory::resolve(&snap.directory(), branch)?`;
//!    v1: `rec.extra_databases` must be empty (else `PgError::unimplemented("BR-10", "exporting
//!    branches with several databases")`).
//! 2. `opts.target_name` validated (42602). `dest` exists and `!opts.overwrite` →
//!    `error::destination_exists(dest)` (check with `vfs.exists`).
//! 3. `tmp = sibling_path(dest, DETACH_TEMP_SUFFIX)`; if it exists, `vfs.remove_file(tmp)` (stale
//!    leftover of a crashed export; storage recovery only cleans temps of the file it opens).
//! 4. `ExportBuilder::create(vfs, &tmp, &ExportOptions { page_size: opts.page_size.unwrap_or(
//!    snap page size), db_uuid: Some(new_uuid), sync: opts.sync, overwrite: false, ..default })`
//!    where `new_uuid = opts.new_db_uuid.unwrap_or_else(random_db_uuid)`.
//! 5. `data = b.copy_tree(&snap, rec.data_root)`; if `opts.include_snapshots`, copy each snapshot
//!    root (the builder memoizes shared pages) and keep them with kind `Inherited`.
//! 6. New record: `BranchRecord::new_active(new_id, target_name, None, data, CommitSeq(1), now)`
//!    (+ snapshots). `entries = standalone_directory_entries(&new_rec, &origin)?`;
//!    `dir = b.build_tree(&mut entries.into_iter().map(Ok))?`; `b.finish(dir)?` (writes `tmp`).
//! 7. `verify_and_publish(vfs, &tmp, dest, opts, &new_rec)` → report fields.
//!
//! `ExportOrigin { source_db_uuid: container.db_uuid(), source_branch_id: rec.id, source_branch_name:
//! rec.name, source_commit_seq: snap.commit_seq(), exported_unix_ms: now, strategy: "copy" }`.
use crate::detach::DetachReport;
use crate::meta::ExportOrigin;
use crate::options::{BranchRef, DetachOptions};
use crate::record::BranchRecord;
use pgarbor_common::{DatabaseUuid, PgError, Result};
use pgarbor_storage::{Container, Vfs};
use std::path::Path;

/// Suffix of the intermediate file export/detach build and verify before renaming to `dest`.
pub const DETACH_TEMP_SUFFIX: &str = ".detach-tmp";

/// Copy-strategy export (module docs). Returns a report with `strategy_used = Copy`.
#[allow(unused_variables)]
pub fn export_branch(container: &Container, branch: &BranchRef, dest: &Path, opts: &DetachOptions) -> Result<DetachReport> {
    Err(PgError::unimplemented("BR-10", "branch export"))
}

/// Directory entries of a standalone file holding exactly `rec`, in ascending key order:
/// `B`+id → `rec.encode()`, `Mdefault_branch` → id, `Mformat` → 1, `Morigin` → origin JSON,
/// `N`+name → id. (Bytes: `B`=0x42 < `M`=0x4D < `N`=0x4E; `default_branch` < `format` < `origin`.)
#[allow(unused_variables)]
pub fn standalone_directory_entries(rec: &BranchRecord, origin: &ExportOrigin) -> Result<Vec<(Vec<u8>, Vec<u8>)>> {
    Err(PgError::unimplemented("BR-10", "standalone directory construction"))
}

// CONTRACT
/// Facts about a verified, published file.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PublishedFile {
    pub db_uuid: DatabaseUuid,
    pub page_count: u64,
    pub file_len: u64,
    /// Committed pages not reachable from any live root (non-zero after a reflink detach).
    pub garbage_pages: u64,
}

/// Reopen-before-success (RFC 001 §9 "Detach ... checked for integrity and reopened before success
/// is reported"), then publish:
/// 1. `c = Container::open_with(vfs.clone(), tmp, &OpenOptions { verify: opts.verify, sync:
///    opts.sync, ..default })`; `report = c.verify(&BranchRootResolver, opts.verify)`; any
///    Error-severity issue → `error::export_verify_failed(dest, first error message)`.
/// 2. Directory check on `c.snapshot().directory()`: `check_format == Ready`, the default branch
///    is `expected.id` with name `expected.name` and the same `data_root` mapping (resolve by id),
///    exactly one `B` record. Mismatch → `export_verify_failed`.
/// 3. Collect `PublishedFile` (`space_report` for garbage pages), then drop `c` (releases the lock).
/// 4. Re-check `dest` existence vs `opts.overwrite` (race) → `destination_exists`.
/// 5. `vfs.rename(tmp, dest)` (`pgarbor_storage::error::rename_failed`); if `opts.sync == Full`,
///    `vfs.sync_dir(parent_dir(dest))` (`fsync_failed`).
/// On any error before the rename: remove `tmp` best-effort and return the error.
#[allow(unused_variables)]
pub fn verify_and_publish(
    vfs: &std::sync::Arc<dyn Vfs>,
    tmp: &Path,
    dest: &Path,
    opts: &DetachOptions,
    expected: &BranchRecord,
) -> Result<PublishedFile> {
    Err(PgError::unimplemented("BR-10", "export verification and publish"))
}
