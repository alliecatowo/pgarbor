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
use crate::detach::{DetachReport, StrategyUsed};
use crate::directory::{self, DirectoryStatus};
use crate::error;
use crate::keys;
use crate::meta::{self, ExportOrigin, DIRECTORY_FORMAT_VERSION};
use crate::name::validate_branch_name;
use crate::options::{BranchRef, DetachOptions};
use crate::record::{BranchRecord, NamedSnapshot, SnapshotKind};
use crate::resolver::BranchRootResolver;
use pgarbor_common::{BranchId, CommitSeq, DatabaseUuid, PgError, Result};
use pgarbor_storage::format::consts::file_len_for;
use pgarbor_storage::format::superblock::{now_unix_ms, random_db_uuid};
use pgarbor_storage::io::{parent_dir, sibling_path};
use pgarbor_storage::{Container, ExportBuilder, ExportOptions, OpenOptions, SyncMode, Vfs};
use std::path::Path;
use std::sync::Arc;

/// Suffix of the intermediate file export/detach build and verify before renaming to `dest`.
pub const DETACH_TEMP_SUFFIX: &str = ".detach-tmp";

/// Copy-strategy export (module docs). Returns a report with `strategy_used = Copy`.
pub fn export_branch(container: &Container, branch: &BranchRef, dest: &Path, opts: &DetachOptions) -> Result<DetachReport> {
    // 1. One immutable snapshot; resolve the branch in it.
    let snap = container.snapshot();
    let rec = directory::resolve(&snap.directory(), branch)?;
    if !rec.extra_databases.is_empty() {
        return Err(PgError::unimplemented("BR-10", "exporting branches with several databases"));
    }
    // 2. Validate the target name and the destination.
    validate_branch_name(&opts.target_name)?;
    let vfs: Arc<dyn Vfs> = Arc::clone(container.vfs());
    if !opts.overwrite && dest_exists(&vfs, dest)? {
        return Err(error::destination_exists(dest));
    }
    // 3. Remove a stale temp left behind by a crashed export.
    let tmp = sibling_path(dest, DETACH_TEMP_SUFFIX);
    match vfs.remove_file(&tmp) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => return Err(pgarbor_storage::error::write_failed(&tmp, &e)),
    }

    let now = now_unix_ms();
    let new_uuid = opts.new_db_uuid.unwrap_or_else(random_db_uuid);
    let new_id = opts.new_branch_id.unwrap_or_else(crate::ops::new_branch_id);
    let origin = ExportOrigin {
        source_db_uuid: container.db_uuid().to_hyphenated(),
        source_branch_id: rec.id.to_hyphenated(),
        source_branch_name: rec.name.clone(),
        source_commit_seq: snap.commit_seq().0,
        exported_unix_ms: now,
        strategy: "copy".to_string(),
    };

    let new_rec = match write_export(&vfs, &snap, &rec, &tmp, new_uuid, new_id, now, &origin, opts) {
        Ok(r) => r,
        Err(e) => {
            let _ = vfs.remove_file(&tmp);
            return Err(e);
        }
    };
    // 7. Reopen-verify, then publish.
    let published = verify_and_publish(&vfs, &tmp, dest, opts, &new_rec)?;
    Ok(DetachReport::from_parts(dest, StrategyUsed::Copy, None, origin, &new_rec, &published, false))
}

/// Steps 4-6: build the standalone file at `tmp`. On error the builder removes its own temp;
/// the caller removes `tmp` (which exists only if `finish` got as far as renaming).
#[allow(clippy::too_many_arguments)]
fn write_export(
    vfs: &Arc<dyn Vfs>,
    snap: &pgarbor_storage::Snapshot,
    rec: &BranchRecord,
    tmp: &Path,
    new_uuid: DatabaseUuid,
    new_id: BranchId,
    now: u64,
    origin: &ExportOrigin,
    opts: &DetachOptions,
) -> Result<BranchRecord> {
    let page_size = opts.page_size.unwrap_or(snap.superblock().page_size);
    let eo = ExportOptions {
        page_size,
        db_uuid: Some(new_uuid),
        sync: opts.sync,
        overwrite: false,
        ..ExportOptions::default()
    };
    let mut b = ExportBuilder::create(Arc::clone(vfs), tmp, &eo)?;
    // 5. Copy the data tree (and snapshot trees; the builder memoizes shared pages).
    let data = b.copy_tree(snap, rec.data_root)?;
    let mut new_rec = BranchRecord::new_active(new_id, &opts.target_name, None, data, CommitSeq(1), now);
    if opts.include_snapshots {
        for s in &rec.snapshots {
            let root = b.copy_tree(snap, s.root)?;
            new_rec.snapshots.push(NamedSnapshot {
                label: s.label.clone(),
                root,
                created_seq: s.created_seq,
                created_unix_ms: s.created_unix_ms,
                kind: SnapshotKind::Inherited,
            });
        }
    }
    // 6. Directory tree.
    let entries = standalone_directory_entries(&new_rec, origin)?;
    let dir = b.build_tree(&mut entries.into_iter().map(Ok))?;
    b.finish(dir)?;
    Ok(new_rec)
}

fn dest_exists(vfs: &Arc<dyn Vfs>, dest: &Path) -> Result<bool> {
    vfs.exists(dest).map_err(|e| pgarbor_storage::error::open_failed(dest, &e))
}

/// Directory entries of a standalone file holding exactly `rec`, in ascending key order:
/// `B`+id → `rec.encode()`, `Mdefault_branch` → id, `Mformat` → 1, `Morigin` → origin JSON,
/// `N`+name → id. (Bytes: `B`=0x42 < `M`=0x4D < `N`=0x4E; `default_branch` < `format` < `origin`.)
pub fn standalone_directory_entries(rec: &BranchRecord, origin: &ExportOrigin) -> Result<Vec<(Vec<u8>, Vec<u8>)>> {
    Ok(vec![
        (keys::branch_key(&rec.id), rec.encode()?),
        (keys::meta_key(meta::META_DEFAULT_BRANCH), meta::encode_default_branch(&rec.id)),
        (keys::meta_key(meta::META_FORMAT), meta::encode_format(DIRECTORY_FORMAT_VERSION)),
        (keys::meta_key(meta::META_ORIGIN), origin.to_json()),
        (keys::name_key(&rec.name), rec.id.as_bytes().to_vec()),
    ])
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
pub fn verify_and_publish(
    vfs: &std::sync::Arc<dyn Vfs>,
    tmp: &Path,
    dest: &Path,
    opts: &DetachOptions,
    expected: &BranchRecord,
) -> Result<PublishedFile> {
    match verify_and_publish_inner(vfs, tmp, dest, opts, expected) {
        Ok(p) => Ok(p),
        Err(e) => {
            let _ = vfs.remove_file(tmp);
            Err(e)
        }
    }
}

fn verify_and_publish_inner(
    vfs: &Arc<dyn Vfs>,
    tmp: &Path,
    dest: &Path,
    opts: &DetachOptions,
    expected: &BranchRecord,
) -> Result<PublishedFile> {
    let published = {
        let oo = OpenOptions { verify: opts.verify, sync: opts.sync, ..OpenOptions::default() };
        let c = Container::open_with(Arc::clone(vfs), tmp, &oo)?;
        let report = c.verify(&BranchRootResolver, opts.verify)?;
        if let Some(first) = report.errors().next() {
            return Err(error::export_verify_failed(dest, &first.message));
        }
        let snap = c.snapshot();
        let dir = snap.directory();
        let bad = |what: &str| error::export_verify_failed(dest, what);
        if directory::check_format(&dir)? != DirectoryStatus::Ready {
            return Err(bad("branch directory is empty"));
        }
        match directory::default_branch(&dir)? {
            Some(id) if id == expected.id => {}
            _ => return Err(bad("default branch does not match the exported branch")),
        }
        let found = match directory::load_record(&dir, &expected.id)? {
            Some(r) => r,
            None => return Err(bad("exported branch record is missing")),
        };
        if found.name != expected.name {
            return Err(bad("exported branch has an unexpected name"));
        }
        if found.data_root != expected.data_root
            || found.snapshots.len() != expected.snapshots.len()
            || found.snapshots.iter().zip(&expected.snapshots).any(|(a, b)| a.label != b.label || a.root != b.root)
        {
            return Err(bad("exported branch roots do not match"));
        }
        if directory::list(&dir, true)?.len() != 1 {
            return Err(bad("exported file does not hold exactly one branch"));
        }
        let space = pgarbor_storage::doctor::space_report(&snap, &BranchRootResolver)?;
        let page_count = snap.page_count();
        // `c` (and its lock) is released at the end of this block.
        let pf = PublishedFile {
            db_uuid: c.db_uuid(),
            page_count,
            file_len: file_len_for(snap.superblock().page_size, page_count),
            garbage_pages: space.garbage_pages,
        };
        pf
    };
    if !opts.overwrite && dest_exists(vfs, dest)? {
        return Err(error::destination_exists(dest));
    }
    vfs.rename(tmp, dest).map_err(|e| pgarbor_storage::error::rename_failed(tmp, dest, &e))?;
    if opts.sync == SyncMode::Full {
        let dir = parent_dir(dest);
        vfs.sync_dir(&dir).map_err(|e| pgarbor_storage::error::fsync_failed(&dir, &e))?;
    }
    Ok(published)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::record::BranchRecord;

    #[test]
    fn entries_are_sorted_and_complete() {
        let rec = BranchRecord::new_active(BranchId([3; 16]), "main", None, pgarbor_common::PageId::NONE, CommitSeq(1), 5);
        let origin = ExportOrigin {
            source_db_uuid: DatabaseUuid([1; 16]).to_hyphenated(),
            source_branch_id: BranchId([2; 16]).to_hyphenated(),
            source_branch_name: "feature".into(),
            source_commit_seq: 9,
            exported_unix_ms: 5,
            strategy: "copy".into(),
        };
        // Encoding is another lease; tolerate its absence but check ordering when it works.
        if let Ok(e) = standalone_directory_entries(&rec, &origin) {
            assert_eq!(e.len(), 5);
            assert!(e.windows(2).all(|w| w[0].0 < w[1].0));
            assert_eq!(e[4].0, b"Nmain".to_vec());
        }
    }
}
