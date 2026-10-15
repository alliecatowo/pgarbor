//! Storage error constructors. CONTRACT: every storage error is built through these helpers
//! so SQLSTATEs and message wording stay uniform. All are fully implemented (no lease).
//!
//! Conventions (PostgreSQL 18 wording where PG has an equivalent, cited per helper):
//! - Checksum / structural damage of file contents → `XX001 data_corrupted`.
//! - OS I/O failure → `PgError::io` (`58030 io_error`, or `53100 disk_full` for ENOSPC = os error 28),
//!   with PG's `could not <verb> file "<path>"` prefix (src/backend/storage/file/fd.c, smgr/md.c).
//! - File locked by another process → `55006 object_in_use`.
//! - Writer poisoned after a failed commit/fsync → `58030 io_error` (PG PANICs; we refuse writes).
//! - Key larger than [`crate::btree::node::max_key_len`] → `54000 program_limit_exceeded`.
use pgarbor_common::{PageId, PgError, SqlState};
use std::io;
use std::path::Path;

// CONTRACT
/// `could not read file "<path>": <os error>` (PG fd.c / md.c wording).
pub fn read_failed(path: &Path, err: &io::Error) -> PgError {
    PgError::io(&format!("could not read file \"{}\"", path.display()), err)
}

// CONTRACT
/// `could not write to file "<path>": <os error>`. ENOSPC → 53100 with hint `Check free disk space.`
/// (PG md.c `mdextend` hint).
pub fn write_failed(path: &Path, err: &io::Error) -> PgError {
    let e = PgError::io(&format!("could not write to file \"{}\"", path.display()), err);
    if e.sqlstate == SqlState::DISK_FULL { e.with_hint("Check free disk space.") } else { e }
}

// CONTRACT
/// `could not fsync file "<path>": <os error>` (PG md.c:1469 wording).
pub fn fsync_failed(path: &Path, err: &io::Error) -> PgError {
    PgError::io(&format!("could not fsync file \"{}\"", path.display()), err)
}

// CONTRACT
/// `could not open file "<path>": <os error>`. NotFound maps to `58P01 undefined_file`
/// (PG `errcode_for_file_access()` maps ENOENT to ERRCODE_UNDEFINED_FILE).
pub fn open_failed(path: &Path, err: &io::Error) -> PgError {
    let mut e = PgError::io(&format!("could not open file \"{}\"", path.display()), err);
    if err.kind() == io::ErrorKind::NotFound {
        e.sqlstate = SqlState::UNDEFINED_FILE;
    }
    if err.kind() == io::ErrorKind::AlreadyExists {
        e.sqlstate = SqlState(*b"58P02"); // duplicate_file
    }
    e
}

// CONTRACT
/// `could not rename file "<from>" to "<to>": <os error>` (PG fd.c durable_rename wording).
pub fn rename_failed(from: &Path, to: &Path, err: &io::Error) -> PgError {
    PgError::io(
        &format!("could not rename file \"{}\" to \"{}\"", from.display(), to.display()),
        err,
    )
}

// CONTRACT
/// `could not truncate file "<path>" to <len> bytes: <os error>`.
pub fn truncate_failed(path: &Path, len: u64, err: &io::Error) -> PgError {
    PgError::io(&format!("could not truncate file \"{}\" to {} bytes", path.display(), len), err)
}

// CONTRACT
/// Lock contention: `could not lock database file "<path>": it is already open in another process`,
/// SQLSTATE 55006, hint `Close the other PGArbor process, or connect to it through its server.`
pub fn locked(path: &Path) -> PgError {
    PgError::new(
        SqlState::OBJECT_IN_USE,
        format!("could not lock database file \"{}\": it is already open in another process", path.display()),
    )
    .with_hint("Close the other PGArbor process, or connect to it through its server.")
}

// CONTRACT
/// Generic corruption: XX001 with the given message.
pub fn corrupt(message: impl Into<String>) -> PgError {
    PgError::corruption(message)
}

// CONTRACT
/// Page checksum mismatch, PG bufpage.c:154 wording:
/// `page verification failed, calculated checksum <calc> but expected <stored>`
/// (decimal u32 values, like PG) with detail `page <id> of database file`.
pub fn checksum_mismatch(id: PageId, calculated: u32, expected: u32) -> PgError {
    PgError::corruption(format!(
        "page verification failed, calculated checksum {calculated} but expected {expected}"
    ))
    .with_detail(format!("page {} of database file", id.0))
}

// CONTRACT
/// Structurally invalid page, PG bufmgr.c:7361 wording adapted:
/// `invalid page in block <id> of database file` with `detail` = specific reason.
pub fn invalid_page(id: PageId, reason: impl Into<String>) -> PgError {
    PgError::corruption(format!("invalid page in block {} of database file", id.0)).with_detail(reason)
}

// CONTRACT
/// Reference to a page outside the committed range of the snapshot/txn reading it.
/// Message: `page reference <id> is beyond the end of committed data (page_count <n>)`.
pub fn page_out_of_range(id: PageId, page_count: u64) -> PgError {
    PgError::corruption(format!(
        "page reference {} is beyond the end of committed data (page_count {})",
        id.0, page_count
    ))
}

// CONTRACT
/// Writes refused after an unrecoverable I/O failure during commit/compaction (STORE-18).
/// Message: `database file "<path>" is unavailable for writing after an earlier I/O failure`,
/// SQLSTATE 58030, detail = the original failure, hint `Reopen the database to run recovery.`
pub fn poisoned(path: &Path, reason: &str) -> PgError {
    PgError::new(
        SqlState::IO_ERROR,
        format!("database file \"{}\" is unavailable for writing after an earlier I/O failure", path.display()),
    )
    .with_detail(reason.to_string())
    .with_hint("Reopen the database to run recovery.")
}

// CONTRACT
/// Write attempted on a read-only container: `database file "<path>" is opened read-only`, 25006.
pub fn read_only(path: &Path) -> PgError {
    PgError::new(
        SqlState::READ_ONLY_SQL_TRANSACTION,
        format!("database file \"{}\" is opened read-only", path.display()),
    )
}

// CONTRACT
/// Key too large for a tree node: `key size <len> exceeds maximum <max> for storage tree`, 54000.
/// Upper layers (index build) should check first and raise PG's own index-row-size message.
pub fn key_too_large(len: usize, max: usize) -> PgError {
    PgError::new(
        SqlState::PROGRAM_LIMIT_EXCEEDED,
        format!("key size {len} exceeds maximum {max} for storage tree"),
    )
}

// CONTRACT
/// Value too large (> u32::MAX bytes): `value size <len> exceeds maximum <max> for storage tree`, 54000.
pub fn value_too_large(len: usize, max: u64) -> PgError {
    PgError::new(
        SqlState::PROGRAM_LIMIT_EXCEEDED,
        format!("value size {len} exceeds maximum {max} for storage tree"),
    )
}

// CONTRACT
/// Not a PGArbor file: `file "<path>" is not a PGArbor database`, XX001, fail closed.
pub fn not_a_database(path: &Path, reason: &str) -> PgError {
    PgError::corruption(format!("file \"{}\" is not a PGArbor database", path.display()))
        .with_detail(reason.to_string())
}

// CONTRACT
/// Unsupported format: `database file "<path>" has unsupported format version <v>`,
/// XX001, detail `This build of PGArbor supports format version <FORMAT_VERSION>.`
pub fn unsupported_version(path: &Path, version: u32, supported: u32) -> PgError {
    PgError::corruption(format!(
        "database file \"{}\" has unsupported format version {}",
        path.display(),
        version
    ))
    .with_detail(format!("This build of PGArbor supports format version {supported}."))
}

// CONTRACT
/// Unknown required feature flags: `database file "<path>" requires unsupported features 0x<hex16>`, XX001.
pub fn unsupported_features(path: &Path, flags: u64) -> PgError {
    PgError::corruption(format!(
        "database file \"{}\" requires unsupported features 0x{:016x}",
        path.display(),
        flags
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn enospc_is_disk_full_with_hint() {
        let e = write_failed(Path::new("a.pga"), &io::Error::from_raw_os_error(28));
        assert_eq!(e.sqlstate, SqlState::DISK_FULL);
        assert_eq!(e.hint.as_deref(), Some("Check free disk space."));
        assert!(e.message.starts_with("could not write to file \"a.pga\""));
    }
    #[test]
    fn checksum_message_matches_pg() {
        let e = checksum_mismatch(PageId(7), 1, 2);
        assert_eq!(e.message, "page verification failed, calculated checksum 1 but expected 2");
        assert_eq!(e.sqlstate.code(), "XX001");
    }
}
