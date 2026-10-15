//! `.pgabranch` handle files (BRANCH-07, ARCHITECTURE §6, RFC 001 §4.1). Lease BR-08.
//!
//! A handle is a tiny JSON pointer INTO a master `.pga`; it is never a standalone database and
//! must never be advertised as portable on its own. Deleting a handle never deletes branch data.
//!
//! # File format `pgabranch/1` (CONTRACT)
//!
//! UTF-8 JSON object, written with `serde_json::to_string_pretty` + `"\n"`, members in this order:
//!
//! ```json
//! {
//!   "format": "pgabranch/1",
//!   "db_uuid": "0f1e2d3c-4b5a-6978-8796-a5b4c3d2e1f0",
//!   "branch_id": "11111111-2222-4333-8444-555555555555",
//!   "branch_name_hint": "feature/auth",
//!   "db_path_hint": "../dev.pga",
//!   "created_unix_ms": 1760400000000,
//!   "crc32c": "1a2b3c4d"
//! }
//! ```
//!
//! - UUIDs: lowercase hyphenated (`to_hyphenated`). `db_path_hint`: relative to the directory
//!   containing the handle (lexical relative path, `/` separators) unless
//!   `HandleOptions::absolute_db_path` or no relative path exists (different root/drive) → absolute.
//! - `crc32c`: 8 lowercase hex digits of `crc32c::crc32c(body)` where `body` is
//!   `serde_json::to_vec(&HandleBody{..})` — the COMPACT serialization of the first six members in
//!   the order above (no whitespace). It detects truncation/hand edits, not tampering.
//! - Readers: reject unknown `format` (`handle_invalid(path, "unsupported format \"x\"")`), missing
//!   or extra members (`deny_unknown_fields`), bad UUIDs, crc mismatch
//!   (`"checksum mismatch"`), non-JSON (`"not valid JSON: <serde error>"`), files over 64 KiB.
//!
//! # Resolution
//!
//! `resolve_db_path` finds the master file: explicit path (if given) → `db_path_hint` joined to the
//! handle's directory (if relative) → `db_path_hint` as-is. First existing candidate wins; none →
//! `error::handle_db_not_found(handle, tried)`. After opening that container the caller runs
//! `HandleFile::check(db_uuid, dir)`: uuid mismatch → `error::handle_db_mismatch` (loud error, never
//! silently use another database); branch id deleted → `branch_deleted`; missing → 42704
//! `branch_id_not_found`. A renamed branch resolves fine (the name is only a hint).
use crate::options::HandleOptions;
use crate::record::BranchRecord;
use pgarbor_common::{BranchId, DatabaseUuid, KvRead, PgError, Result};
use pgarbor_storage::Vfs;
use std::path::{Path, PathBuf};

/// The `format` member value.
pub const HANDLE_FORMAT: &str = "pgabranch/1";
/// Conventional extension (without dot).
pub const HANDLE_EXTENSION: &str = "pgabranch";
/// Largest handle file accepted by the reader.
pub const MAX_HANDLE_BYTES: u64 = 64 * 1024;

// CONTRACT
/// Parsed handle (typed form of the JSON).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HandleFile {
    pub db_uuid: DatabaseUuid,
    pub branch_id: BranchId,
    pub branch_name_hint: String,
    pub db_path_hint: String,
    pub created_unix_ms: u64,
}

// CONTRACT
/// Handle plus the master database path it resolved to.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ResolvedHandle {
    pub handle: HandleFile,
    pub handle_path: PathBuf,
    pub db_path: PathBuf,
}

/// The six checksummed members, in file order (serde field order = JSON member order).
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HandleBody {
    pub format: String,
    pub db_uuid: String,
    pub branch_id: String,
    pub branch_name_hint: String,
    pub db_path_hint: String,
    pub created_unix_ms: u64,
}

/// Full file object (body members + `crc32c`), used for (de)serialization.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HandleJson {
    pub format: String,
    pub db_uuid: String,
    pub branch_id: String,
    pub branch_name_hint: String,
    pub db_path_hint: String,
    pub created_unix_ms: u64,
    pub crc32c: String,
}

impl HandleFile {
    /// Serialize to the exact file bytes (pretty JSON + trailing newline, crc filled in).
    #[allow(unused_variables)]
    pub fn to_bytes(&self) -> Result<Vec<u8>> {
        Err(PgError::unimplemented("BR-08", "branch handle serialization"))
    }

    /// Parse and validate file bytes (`path` only for messages). See module docs for errors.
    #[allow(unused_variables)]
    pub fn parse(path: &Path, bytes: &[u8]) -> Result<HandleFile> {
        Err(PgError::unimplemented("BR-08", "branch handle parsing"))
    }

    /// Verify against an opened master: `db_uuid` must equal `actual_uuid`
    /// (else `error::handle_db_mismatch(handle_path, &self.db_uuid, db_path, &actual_uuid)`), then
    /// `directory::resolve(dir, &BranchRef::Id(self.branch_id))` (deleted/missing errors as in the
    /// module docs). Returns the active record.
    #[allow(unused_variables)]
    pub fn check(
        &self,
        handle_path: &Path,
        db_path: &Path,
        actual_uuid: DatabaseUuid,
        dir: &dyn KvRead,
    ) -> Result<BranchRecord> {
        Err(PgError::unimplemented("BR-08", "branch handle verification"))
    }
}

/// Write a handle file for `record` of the master at `db_path` (database `db_uuid`) to
/// `handle_path` atomically: temp sibling `<handle_path>.tmp` (`vfs.open(.., CreateTruncate)`),
/// `write_all_at(0, bytes)`, `sync_all`, `vfs.rename(tmp, handle_path)`, `vfs.sync_dir(parent)`.
/// Existing `handle_path` and `!opts.overwrite` → `error::destination_exists`. io errors →
/// `pgarbor_storage::error::{write_failed, fsync_failed, rename_failed}`. Returns the handle.
#[allow(unused_variables)]
pub fn write_handle(
    vfs: &dyn Vfs,
    handle_path: &Path,
    db_path: &Path,
    db_uuid: DatabaseUuid,
    record: &BranchRecord,
    created_unix_ms: u64,
    opts: &HandleOptions,
) -> Result<HandleFile> {
    Err(PgError::unimplemented("BR-08", "branch handle write"))
}

/// Read + parse a handle file (`vfs.open(ReadOnly)`, length check against `MAX_HANDLE_BYTES`,
/// `read_exact_at`). Missing file → `pgarbor_storage::error::open_failed` (58P01).
#[allow(unused_variables)]
pub fn read_handle(vfs: &dyn Vfs, handle_path: &Path) -> Result<HandleFile> {
    Err(PgError::unimplemented("BR-08", "branch handle read"))
}

/// Find the master database file (module docs, "Resolution").
#[allow(unused_variables)]
pub fn resolve_db_path(vfs: &dyn Vfs, handle_path: &Path, explicit_db: Option<&Path>) -> Result<ResolvedHandle> {
    Err(PgError::unimplemented("BR-08", "branch handle database lookup"))
}

/// Lexical relative path from directory `from_dir` to `target` (both made absolute against the
/// current directory first via `std::env::current_dir`; no symlink resolution). `Ok(None)` when
/// they share no common root.
/// Example: from `/w/branches`, target `/w/dev.pga` → `../dev.pga`.
#[allow(unused_variables)]
pub fn relative_path(from_dir: &Path, target: &Path) -> Result<Option<PathBuf>> {
    Err(PgError::unimplemented("BR-08", "relative path computation"))
}
