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
//! 1. Validate target/destination; acquire the stable publication guard and clean only
//!    exclusively locked, identity-checked stale temporaries.
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
use crate::directory;
use crate::error;
use crate::export::{self, PublishedFile, DETACH_TEMP_SUFFIX};
use crate::meta::ExportOrigin;
use crate::name;
use crate::ops;
use crate::options::{BranchRef, DetachOptions, DetachStrategy};
use crate::record::{BranchRecord, NamedSnapshot, SnapshotKind};
use crate::reflink::{self, CloneOutcome};
use crate::resolver::BranchRootResolver;
use pgarbor_common::{BranchId, CommitSeq, DatabaseUuid, PgError, Result};
use pgarbor_storage::format::superblock::now_unix_ms;
use pgarbor_storage::format::random_db_uuid;
use pgarbor_storage::io::sibling_path;
use pgarbor_storage::{CompactOptions, Container, OpenOptions, SyncMode, VerifyLevel, Vfs};
use std::path::{Path, PathBuf};
use std::sync::Arc;

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

fn io_err(what: &str, path: &Path, e: &std::io::Error) -> PgError {
    PgError::io(&format!("could not {what} \"{}\"", path.display()), e)
}

/// Detach per `opts.strategy` (module docs).
pub fn detach_branch(container: &Container, branch: &BranchRef, dest: &Path, opts: &DetachOptions) -> Result<DetachReport> {
    if opts.strategy == DetachStrategy::Copy {
        return export::export_branch(container, branch, dest, opts);
    }
    let pre = if !opts.os_filesystem {
        Some("the database is not on the operating system file system".to_string())
    } else if !reflink::platform_has_reflink() {
        Some("reflink cloning is not available on this platform".to_string())
    } else {
        None
    };
    let unsupported = match pre {
        Some(reason) => reason,
        None => match reflink_detach(container, branch, dest, opts)? {
            Ok(report) => return Ok(report),
            Err(reason) => reason,
        },
    };
    match opts.strategy {
        DetachStrategy::ReflinkRequired => Err(error::reflink_unsupported(container.path(), &unsupported)),
        _ => {
            let mut report = export::export_branch(container, branch, dest, opts)?;
            report.strategy_used = StrategyUsed::Copy;
            report.fallback_reason = Some(unsupported);
            Ok(report)
        }
    }
}

/// Reflink path; `Ok(Err(reason))` = unsupported, nothing left behind.
pub fn reflink_detach(
    container: &Container,
    branch: &BranchRef,
    dest: &Path,
    opts: &DetachOptions,
) -> Result<std::result::Result<DetachReport, String>> {
    if !opts.os_filesystem {
        return Ok(Err("the database is not on the operating system file system".to_string()));
    }
    if !reflink::platform_has_reflink() {
        return Ok(Err("reflink cloning is not available on this platform".to_string()));
    }
    // 1. Validate and prepare the destination.
    name::validate_branch_name(&opts.target_name)?;
    let vfs: Arc<dyn Vfs> = container.vfs().clone();
    let temp = sibling_path(dest, DETACH_TEMP_SUFFIX);
    if !opts.overwrite && vfs.exists(dest).map_err(|e| io_err("access file", dest, &e))? {
        return Err(error::destination_exists(dest));
    }
    let _publication = export::PublicationGuard::acquire(&vfs, dest)?;
    export::remove_stale_temp(&vfs, &temp)?;

    // 2. Barrier: the writer lock keeps commits and spills out while the clone is taken.
    let barrier = if container.is_read_only() { None } else { Some(container.begin_write()?) };
    let snap = container.snapshot();
    let rec = directory::resolve(&snap.directory(), branch)?;

    // 3-4. Clone, then release the barrier.
    let outcome = reflink::clone_file(container.path(), &temp);
    if let Some(w) = barrier {
        w.abort();
    }
    match outcome? {
        CloneOutcome::Unsupported { reason } => return Ok(Err(reason)),
        CloneOutcome::Cloned => {}
    }

    let new_uuid = opts.new_db_uuid.unwrap_or_else(random_db_uuid);
    let new_id = opts.new_branch_id.unwrap_or_else(ops::new_branch_id);
    let origin = ExportOrigin {
        source_db_uuid: container.db_uuid().to_hyphenated(),
        source_branch_id: rec.id.to_hyphenated(),
        source_branch_name: rec.name.clone(),
        source_commit_seq: snap.commit_seq().0,
        exported_unix_ms: now_unix_ms(),
        strategy: "reflink".to_string(),
    };
    let result = finish_clone(&vfs, &temp, dest, opts, &rec, new_uuid, new_id, &origin);
    if result.is_err() {
        let _ = export::remove_stale_temp(&vfs, &temp);
    }
    result.map(Ok)
}

/// Steps 5-8 of `reflink_detach`, on the private temp clone.
#[allow(clippy::too_many_arguments)]
fn finish_clone(
    vfs: &Arc<dyn Vfs>,
    temp: &Path,
    dest: &Path,
    opts: &DetachOptions,
    rec: &BranchRecord,
    new_uuid: DatabaseUuid,
    new_id: BranchId,
    origin: &ExportOrigin,
) -> Result<DetachReport> {
    reidentify_file(&**vfs, temp, new_uuid, opts.sync)?;
    let open_opts = OpenOptions { sync: opts.sync, verify: VerifyLevel::Roots, ..OpenOptions::default() };
    let c = Container::open_with(vfs.clone(), temp, &open_opts)?;
    let (mut new_rec, _seq) = prune_to_branch(&c, rec, new_id, &opts.target_name, opts.include_snapshots, origin)?;
    let mut compacted = false;
    if opts.compact_after_reflink {
        c.compact(&BranchRootResolver, &CompactOptions::default())?;
        // Compaction renumbers pages: re-read the record to compare against the real roots.
        new_rec = directory::resolve(&c.snapshot().directory(), &BranchRef::Id(new_id))?;
        compacted = true;
    }
    c.close()?;
    let published = export::verify_and_publish_owned(vfs, temp, dest, opts, &new_rec)?;
    Ok(DetachReport::from_parts(
        dest,
        StrategyUsed::Reflink,
        None,
        origin.clone(),
        &new_rec,
        &published,
        compacted,
    ))
}

/// Give a CLOSED, not-yet-published database file a new identity. Only safe on a private temp file
/// (a crash mid-way leaves an unopenable temp, which is fine because it was never published).
/// Steps: `f = vfs.open(path, ReadWrite)`; `try_lock(Exclusive)` (WouldBlock →
/// `pgarbor_storage::error::locked`); `slots = pgarbor_storage::container::read_slots(&*f, path)`;
/// `choice = pgarbor_storage::format::choose_slot(path, &slots)`; `sb = choice.superblock` with
/// `db_uuid = new_uuid`; write `sb.encode()` at slot offset `choice.slot * 4096` and 4096 zero bytes
/// at the OTHER slot (two valid slots with different UUIDs are refused by `choose_slot`, so the
/// older slot must be erased); `sync_data` when `sync == Full`.
/// Compatibility wrapper for storage-owned `recovery::reidentify` (branches-02 accepted).
pub fn reidentify_file(vfs: &dyn Vfs, path: &Path, new_uuid: DatabaseUuid, sync: SyncMode) -> Result<()> {
    pgarbor_storage::container::recovery::reidentify(vfs, path, new_uuid, sync)?;
    Ok(())
}

/// Rewrite the (cloned) container's directory so it holds exactly one branch: `keep`'s roots
/// under a new record `new_active(new_id, new_name, None, keep.data_root, w.commit_seq(), now)`
/// (+ `keep.snapshots` as `Inherited` when `include_snapshots`), built with
/// `w.build_tree(export::standalone_directory_entries(..))` into a FRESH directory tree,
/// `set_directory_root`, `commit`. Returns the new record and the commit seq.
pub fn prune_to_branch(
    c: &Container,
    keep: &BranchRecord,
    new_id: BranchId,
    new_name: &str,
    include_snapshots: bool,
    origin: &ExportOrigin,
) -> Result<(BranchRecord, CommitSeq)> {
    name::validate_branch_name(new_name)?;
    let mut w = c.begin_write()?;
    let mut new_rec =
        BranchRecord::new_active(new_id, new_name, None, keep.data_root, w.commit_seq(), now_unix_ms());
    new_rec.extra_databases = keep.extra_databases.clone();
    if include_snapshots {
        new_rec.snapshots = keep
            .snapshots
            .iter()
            .map(|s| NamedSnapshot { kind: SnapshotKind::Inherited, ..s.clone() })
            .collect();
    }
    let entries = export::standalone_directory_entries(&new_rec, origin)?;
    let dir = w.build_tree(&mut entries.into_iter().map(Ok::<(Vec<u8>, Vec<u8>), PgError>))?;
    w.set_directory_root(dir);
    let seq = w.commit()?;
    Ok((new_rec, seq))
}
