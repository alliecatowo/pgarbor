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
#[allow(unused_variables)]
pub fn read_slots(file: &dyn FileIo, path: &Path) -> Result<[SlotStatus; 2]> {
    Err(PgError::unimplemented("STO-11", "superblock slot reading"))
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
#[allow(unused_variables)]
pub(crate) fn initialize(vfs: &dyn Vfs, path: &Path, opts: &CreateOptions) -> Result<Recovered> {
    Err(PgError::unimplemented("STO-11", "container file initialization"))
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
#[allow(unused_variables)]
pub(crate) fn recover(vfs: &dyn Vfs, path: &Path, opts: &OpenOptions) -> Result<Recovered> {
    Err(PgError::unimplemented("STO-11", "container recovery"))
}
