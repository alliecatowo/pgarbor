//! File initialization and crash recovery (STORE-01, STORE-02, STORE-11, STORE-12).
//! Lease STO-11 (sonnet).
//!
//! Recovery is deliberately trivial because the commit protocol never overwrites reachable pages:
//! the newest valid superblock IS the recovered state; everything past its `page_count` is an
//! unacknowledged tail. No log replay exists.
#[allow(unused_imports)]
use crate::container::options::{CreateOptions, OpenOptions, RecoveryPolicy, RecoveryReport, SyncMode};
#[allow(unused_imports)]
use crate::error;
#[allow(unused_imports)]
use crate::format::consts::*;
#[allow(unused_imports)]
use crate::format::superblock::{choose_slot, now_unix_ms, random_db_uuid, SlotChoice, SlotStatus, Superblock};
#[allow(unused_imports)]
use crate::io::{parent_dir, sibling_path, FileIo, LockMode, OpenMode, Vfs};
use pgarbor_common::{PgError, Result};
use std::io;
use std::path::Path;

// CONTRACT
/// Suffix of the temporary file compaction writes next to the database before renaming it over.
pub const COMPACT_TEMP_SUFFIX: &str = ".compact-tmp";
// CONTRACT
/// Suffix of the temporary file `ExportBuilder` writes before renaming to the destination.
pub const EXPORT_TEMP_SUFFIX: &str = ".export-tmp";

/// Output of initialize/recover: a locked file handle and the adopted superblock.
#[derive(Debug)]
pub(crate) struct Recovered {
    pub(crate) file: Box<dyn FileIo>,
    pub(crate) choice: SlotChoice,
    pub(crate) report: RecoveryReport,
}

// CONTRACT
/// Read both superblock slots straight from a file (page size unknown yet): 4096 bytes at offsets
/// 0 and 4096, zero-filling anything beyond EOF, then `Superblock::decode` each. io errors →
/// `error::read_failed(path, e)`. Public for doctor (works on files `open` refuses).
pub fn read_slots(file: &dyn FileIo, path: &Path) -> Result<[SlotStatus; 2]> {
    let a = read_slot(file, path, 0)?;
    let b = read_slot(file, path, 1)?;
    Ok([a, b])
}

/// Read one 4096-byte slot, zero-filling whatever lies beyond EOF.
fn read_slot(file: &dyn FileIo, path: &Path, slot: usize) -> Result<SlotStatus> {
    let mut buf = vec![0u8; SUPERBLOCK_SLOT_SIZE];
    let base = slot_offset(slot);
    let mut done = 0usize;
    while done < buf.len() {
        match file.read_at(base + done as u64, &mut buf[done..]) {
            Ok(0) => break,
            Ok(n) => done += n,
            Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
            Err(e) => return Err(error::read_failed(path, &e)),
        }
    }
    Superblock::decode(&buf)
}

/// Take a lock, mapping contention to the PG-style "locked" error.
fn lock(file: &dyn FileIo, path: &Path, mode: LockMode) -> Result<()> {
    match file.try_lock(mode) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == io::ErrorKind::WouldBlock => Err(error::locked(path)),
        Err(e) => Err(PgError::io(&format!("could not lock file \"{}\"", path.display()), &e)),
    }
}

/// Create a brand-new container file. Steps (each failure → remove the partial file best-effort
/// with `vfs.remove_file` and return the error):
/// 1. `is_valid_page_size(opts.page_size)` else 22023 `invalid page size <n>` with detail
///    `Page size must be a power of two between 4096 and 65536.` (before touching the disk).
/// 2. `vfs.open(path, CreateNew)` (`error::open_failed`; exists → 58P02).
/// 3. `try_lock(Exclusive)` (WouldBlock → `error::locked`).
/// 4. sb = `Superblock::new_empty(page_size, opts.db_uuid or random_db_uuid(),
///    opts.created_unix_ms or now_unix_ms())`; write slot 0 = `sb.encode()`, slot 1 = 4096 zero
///    bytes (file length becomes exactly 8192).
/// 5. If `opts.sync == Full`: `sync_all` (`error::fsync_failed`) then
///    `vfs.sync_dir(parent_dir(path))` (`error::fsync_failed` with the dir path).
/// 6. Return `Recovered { choice: {slot 0, sb, other_valid false}, report: {chosen_slot 0,
///    commit_seq 1, slots [Valid(sb), Empty], file_len 8192, committed_len 8192, created: true,
///    ..zero/empty} }`.
pub(crate) fn initialize(vfs: &dyn Vfs, path: &Path, opts: &CreateOptions) -> Result<Recovered> {
    if !is_valid_page_size(opts.page_size) {
        return Err(PgError::new(
            pgarbor_common::SqlState::INVALID_PARAMETER_VALUE,
            format!("invalid page size {}", opts.page_size),
        )
        .with_detail("Page size must be a power of two between 4096 and 65536."));
    }
    let file = vfs.open(path, OpenMode::CreateNew).map_err(|e| error::open_failed(path, &e))?;
    match initialize_inner(vfs, path, opts, file) {
        Ok(r) => Ok(r),
        Err(e) => {
            let _ = vfs.remove_file(path);
            Err(e)
        }
    }
}

fn initialize_inner(
    vfs: &dyn Vfs,
    path: &Path,
    opts: &CreateOptions,
    file: Box<dyn FileIo>,
) -> Result<Recovered> {
    lock(file.as_ref(), path, LockMode::Exclusive)?;
    let sb = Superblock::new_empty(
        opts.page_size,
        opts.db_uuid.unwrap_or_else(random_db_uuid),
        opts.created_unix_ms.unwrap_or_else(now_unix_ms),
    );
    let mut image = sb.encode()?;
    image.resize(2 * SUPERBLOCK_SLOT_SIZE, 0);
    file.write_all_at(0, &image).map_err(|e| error::write_failed(path, &e))?;
    if opts.sync == SyncMode::Full {
        file.sync_all().map_err(|e| error::fsync_failed(path, &e))?;
        let dir = parent_dir(path);
        vfs.sync_dir(&dir).map_err(|e| error::fsync_failed(&dir, &e))?;
    }
    let len = 2 * SUPERBLOCK_SLOT_SIZE as u64;
    let report = RecoveryReport {
        chosen_slot: 0,
        commit_seq: sb.commit_seq,
        slots: [SlotStatus::Valid(sb.clone()), SlotStatus::Empty],
        file_len: len,
        committed_len: len,
        truncated_tail_bytes: 0,
        removed_temp_files: Vec::new(),
        fell_back_to_older_slot: false,
        created: true,
    };
    Ok(Recovered { file, choice: SlotChoice { slot: 0, superblock: sb, other_valid: false }, report })
}

/// Open + recover an existing container. Steps:
/// 1. `vfs.open(path, ReadOnly|ReadWrite)` per `opts.read_only` (`error::open_failed`).
/// 2. `try_lock(Shared)` for read-only, `Exclusive` otherwise; WouldBlock → `error::locked(path)`.
/// 3. `read_slots`, `choose_slot(path, &slots)`.
/// 4. `file_len < sb.committed_len()`: Strict → XX001 `database file "<path>" is truncated:
///    commit <seq> needs <need> bytes but the file has <len>`; FallbackToOlderSlot → if the other
///    slot is Valid and fits, adopt it (`fell_back_to_older_slot = true`), else the same error.
/// 5. Writable only: if `file_len > committed_len` → `set_len(committed_len)` and (sync Full)
///    `sync_data`; record `truncated_tail_bytes`. Remove `<path>.compact-tmp` and
///    `<path>.export-tmp` if they exist (`vfs.remove_file`), record them in
///    `removed_temp_files`, and (sync Full) `sync_dir(parent)`. Read-only opens change nothing.
/// 6. Return the locked handle, the choice and the report.
///
/// Recovery invariant (verified by pgarbor-fault): after any crash, the adopted superblock is
/// either the last acknowledged commit or the single in-flight one; it never references a page
/// that was not durable before its own superblock write.
pub(crate) fn recover(vfs: &dyn Vfs, path: &Path, opts: &OpenOptions) -> Result<Recovered> {
    let mode = if opts.read_only { OpenMode::ReadOnly } else { OpenMode::ReadWrite };
    let mut attempts = 0;
    let file = loop {
        let candidate = vfs.open(path, mode).map_err(|e| error::open_failed(path, &e))?;
        let locked = lock(candidate.as_ref(), path, if opts.read_only { LockMode::Shared } else { LockMode::Exclusive });
        let current = vfs.is_current_file(path, candidate.as_ref()).map_err(|e| error::open_failed(path, &e))?;
        if current {
            locked?;
            break candidate;
        }
        // An opener paused before flock can hold a retired inode after compaction.
        // Drop it and retry the pathname; never read or truncate the retired file.
        attempts += 1;
        if attempts >= 16 { return Err(error::locked(path)); }
    };

    let slots = read_slots(file.as_ref(), path)?;
    let mut choice = choose_slot(path, &slots)?;
    let file_len = file.len().map_err(|e| error::read_failed(path, &e))?;

    let mut fell_back = false;
    if file_len < choice.superblock.committed_len() {
        let err = error::corrupt(format!(
            "database file \"{}\" is truncated: commit {} needs {} bytes but the file has {}",
            path.display(),
            choice.superblock.commit_seq.0,
            choice.superblock.committed_len(),
            file_len
        ));
        if opts.recovery != RecoveryPolicy::FallbackToOlderSlot {
            return Err(err);
        }
        let other = 1 - choice.slot;
        match slots[other].valid() {
            Some(sb) if sb.committed_len() <= file_len => {
                choice = SlotChoice { slot: other, superblock: sb.clone(), other_valid: false };
                fell_back = true;
            }
            _ => return Err(err),
        }
    }

    let committed_len = choice.superblock.committed_len();
    let mut truncated_tail_bytes = 0u64;
    let mut removed_temp_files = Vec::new();
    if !opts.read_only {
        if file_len > committed_len {
            file.set_len(committed_len).map_err(|e| error::truncate_failed(path, committed_len, &e))?;
            if opts.sync == SyncMode::Full {
                file.sync_data().map_err(|e| error::fsync_failed(path, &e))?;
            }
            truncated_tail_bytes = file_len - committed_len;
        }
        for suffix in [COMPACT_TEMP_SUFFIX, EXPORT_TEMP_SUFFIX] {
            let tmp = sibling_path(path, suffix);
            let present = vfs
                .exists(&tmp)
                .map_err(|e| PgError::io(&format!("could not stat file \"{}\"", tmp.display()), &e))?;
            if !present {
                continue;
            }
            let temporary = match vfs.open(&tmp, OpenMode::ReadWrite) {
                Ok(f) => f,
                Err(e) if e.kind() == io::ErrorKind::NotFound => continue,
                Err(e) => return Err(error::open_failed(&tmp, &e)),
            };
            match temporary.try_lock(LockMode::Exclusive) {
                Ok(()) => {},
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => continue,
                Err(e) => return Err(error::open_failed(&tmp, &e)),
            }
            if !vfs.is_current_file(&tmp, temporary.as_ref()).map_err(|e| error::open_failed(&tmp, &e))? {
                continue;
            }
            match vfs.remove_file(&tmp) {
                Ok(()) => removed_temp_files.push(tmp),
                Err(e) if e.kind() == io::ErrorKind::NotFound => {}
                Err(e) => {
                    return Err(PgError::io(&format!("could not remove file \"{}\"", tmp.display()), &e));
                }
            }
        }
        if !removed_temp_files.is_empty() && opts.sync == SyncMode::Full {
            let dir = parent_dir(path);
            vfs.sync_dir(&dir).map_err(|e| error::fsync_failed(&dir, &e))?;
        }
    }

    let report = RecoveryReport {
        chosen_slot: choice.slot,
        commit_seq: choice.superblock.commit_seq,
        slots,
        file_len,
        committed_len,
        truncated_tail_bytes,
        removed_temp_files,
        fell_back_to_older_slot: fell_back,
        created: false,
    };
    Ok(Recovered { file, choice, report })
}

/// Assign a fresh identity to a closed, unpublished private file. A crash during
/// this operation may invalidate the private temporary; callers must not expose it.
pub fn reidentify(
    vfs: &dyn Vfs, path: &Path, new_uuid: pgarbor_common::DatabaseUuid, sync: SyncMode,
) -> Result<Superblock> {
    let file = vfs.open(path, OpenMode::ReadWrite).map_err(|e| error::open_failed(path, &e))?;
    lock(file.as_ref(), path, LockMode::Exclusive)?;
    if !vfs.is_current_file(path, file.as_ref()).map_err(|e| error::open_failed(path, &e))? {
        return Err(error::locked(path));
    }
    let choice = choose_slot(path, &read_slots(file.as_ref(), path)?)?;
    let mut sb = choice.superblock;
    sb.db_uuid = new_uuid;
    file.write_all_at(slot_offset(choice.slot), &sb.encode()?).map_err(|e| error::write_failed(path, &e))?;
    file.write_all_at(slot_offset(1-choice.slot), &[0; SUPERBLOCK_SLOT_SIZE])
        .map_err(|e| error::write_failed(path, &e))?;
    if sync == SyncMode::Full { file.sync_data().map_err(|e| error::fsync_failed(path, &e))?; }
    Ok(sb)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::io::MemVfs;
    use pgarbor_common::CommitSeq;

    fn copts() -> CreateOptions {
        CreateOptions { sync: SyncMode::Off, ..CreateOptions::default() }
    }
    fn oopts() -> OpenOptions {
        OpenOptions { sync: SyncMode::Off, ..OpenOptions::default() }
    }
    fn p() -> &'static Path {
        Path::new("t.pga")
    }

    #[test]
    fn init_then_recover() {
        let vfs = MemVfs::new();
        let r = initialize(&vfs, p(), &copts()).unwrap();
        assert!(r.report.created);
        drop(r);
        let bytes = vfs.read_bytes(p()).unwrap();
        assert_eq!(bytes.len(), 8192);
        assert!(bytes[4096..].iter().all(|b| *b == 0));
        let r = recover(&vfs, p(), &oopts()).unwrap();
        assert_eq!(r.report.commit_seq, CommitSeq(1));
        assert_eq!(r.report.chosen_slot, 0);
        assert!(!r.report.created);
    }

    #[test]
    fn invalid_page_size_and_existing() {
        let vfs = MemVfs::new();
        let o = CreateOptions { page_size: 1000, ..copts() };
        let e = initialize(&vfs, p(), &o).unwrap_err();
        assert_eq!(e.sqlstate.code(), "22023");
        assert!(vfs.paths().is_empty());
        drop(initialize(&vfs, p(), &copts()).unwrap());
        let e = initialize(&vfs, p(), &copts()).unwrap_err();
        assert_eq!(e.sqlstate.code(), "58P02");
    }

    #[test]
    fn garbage_file_rejected() {
        let vfs = MemVfs::new();
        vfs.write_bytes(p(), (0..10000u32).map(|i| (i * 7 + 3) as u8).collect());
        let e = recover(&vfs, p(), &oopts()).unwrap_err();
        assert_eq!(e.sqlstate.code(), "XX001");
        assert!(e.message.contains("is not a PGArbor database"));
        vfs.write_bytes(p(), vec![1, 2, 3]);
        assert!(recover(&vfs, p(), &oopts()).is_err());
    }

    #[test]
    fn second_exclusive_open_is_locked() {
        let vfs = MemVfs::new();
        let _held = initialize(&vfs, p(), &copts()).unwrap();
        let e = recover(&vfs, p(), &oopts()).unwrap_err();
        assert_eq!(e.sqlstate.code(), "55006");
    }

    #[test]
    fn tail_truncated_and_temp_removed() {
        let vfs = MemVfs::new();
        drop(initialize(&vfs, p(), &copts()).unwrap());
        let mut bytes = vfs.read_bytes(p()).unwrap();
        bytes.extend_from_slice(&[0xAB; 100]);
        vfs.write_bytes(p(), bytes.clone());
        let tmp = sibling_path(p(), COMPACT_TEMP_SUFFIX);
        vfs.write_bytes(&tmp, vec![1]);

        let ro = OpenOptions { read_only: true, ..oopts() };
        let r = recover(&vfs, p(), &ro).unwrap();
        assert_eq!(r.report.truncated_tail_bytes, 0);
        assert_eq!(vfs.read_bytes(p()).unwrap(), bytes);
        assert!(vfs.exists(&tmp).unwrap());
        drop(r);

        let r = recover(&vfs, p(), &oopts()).unwrap();
        assert_eq!(r.report.truncated_tail_bytes, 100);
        assert_eq!(r.report.file_len, 8292);
        assert_eq!(r.report.removed_temp_files, vec![tmp.clone()]);
        assert_eq!(vfs.read_bytes(p()).unwrap().len(), 8192);
        assert!(!vfs.exists(&tmp).unwrap());
    }

    fn with_newer_slot1(vfs: &MemVfs) {
        drop(initialize(vfs, p(), &copts()).unwrap());
        let mut bytes = vfs.read_bytes(p()).unwrap();
        let mut sb = Superblock::decode(&bytes[..4096]).unwrap().valid().unwrap().clone();
        sb.commit_seq = CommitSeq(2);
        sb.page_count = 3;
        bytes[4096..8192].copy_from_slice(&sb.encode().unwrap());
        vfs.write_bytes(p(), bytes);
    }

    #[test]
    fn truncated_strict_vs_fallback() {
        let vfs = MemVfs::new();
        with_newer_slot1(&vfs);
        let e = recover(&vfs, p(), &oopts()).unwrap_err();
        assert_eq!(e.sqlstate.code(), "XX001");
        assert!(e.message.contains("is truncated"));
        let o = OpenOptions { recovery: RecoveryPolicy::FallbackToOlderSlot, ..oopts() };
        let r = recover(&vfs, p(), &o).unwrap();
        assert_eq!(r.report.chosen_slot, 0);
        assert_eq!(r.report.commit_seq, CommitSeq(1));
        assert!(r.report.fell_back_to_older_slot);
    }
}

