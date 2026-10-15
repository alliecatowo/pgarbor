//! Typed reads/writes of the branch directory over the `KvRead`/`KvWrite` traits.
//! Lease BR-02. Trivial items are implemented.
//!
//! `dir` is the directory tree as a key/value view: `Snapshot::directory()` (read),
//! `WriteTxn::tree(w.directory_root())` (write, a `TreeWriter`), or `MemKv` in tests.
//! Callers that need a consistent view across several reads must use ONE view (one storage
//! snapshot or one write txn); these helpers never open snapshots themselves.
//!
//! Consistency checks performed by readers (all XX001 via `error::directory_corrupt`):
//! - an `N` value that is not 16 bytes → "name entry \"<name>\" has a malformed id";
//! - an `N` entry whose `B` record is missing → "name \"<name>\" points to missing branch <id>";
//! - an `N` entry whose record is Deleted or whose `record.name != name` →
//!   "name \"<name>\" points to branch <id> named \"<record name>\"" / "... to deleted branch <id>";
//! - a `B` key whose record's `id` differs from the key → "branch record <key id> has id <id>".
use crate::error;
use crate::keys::{self, DirKey};
use crate::meta::{self, ExportOrigin, DIRECTORY_FORMAT_VERSION};
use crate::options::BranchRef;
use crate::record::BranchRecord;
use pgarbor_common::{BranchId, KvRead, KvWrite, PgError, Result};

// CONTRACT
/// State of a directory as found on open.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DirectoryStatus {
    /// No keys at all: a freshly created container (or a create interrupted before the first
    /// branch commit). Writable opens initialize it.
    Empty,
    /// `Mformat` present and equal to `DIRECTORY_FORMAT_VERSION`.
    Ready,
}

/// Inspect the directory format.
/// - no keys at all → `Empty`;
/// - `Mformat` missing but other keys present → XX001 "missing format marker";
/// - `Mformat` != `DIRECTORY_FORMAT_VERSION` → `error::unsupported_directory_format(v)`;
/// - else `Ready`.
#[allow(unused_variables)]
pub fn check_format(dir: &dyn KvRead) -> Result<DirectoryStatus> {
    Err(PgError::unimplemented("BR-02", "branch directory format check"))
}

/// Record stored under `B`+id (any state), decoded with `BranchRecord::decode`; `Ok(None)` when absent.
/// Also enforces the key/id consistency check.
#[allow(unused_variables)]
pub fn load_record(dir: &dyn KvRead, id: &BranchId) -> Result<Option<BranchRecord>> {
    Err(PgError::unimplemented("BR-02", "branch record lookup"))
}

/// Id stored under `N`+name; `Ok(None)` when absent. Malformed value → XX001.
#[allow(unused_variables)]
pub fn lookup_name(dir: &dyn KvRead, name: &str) -> Result<Option<BranchId>> {
    Err(PgError::unimplemented("BR-02", "branch name lookup"))
}

/// Resolve to an ACTIVE branch record.
/// - `Name(n)`: `N` lookup (absent → `error::branch_not_found(n)`), then `load_record` with the
///   consistency checks above.
/// - `Id(id)`: `load_record` (absent → `error::branch_id_not_found(id)`; Deleted →
///   `error::branch_deleted(record.name, id)`).
#[allow(unused_variables)]
pub fn resolve(dir: &dyn KvRead, branch: &BranchRef) -> Result<BranchRecord> {
    Err(PgError::unimplemented("BR-02", "branch resolution"))
}

/// Like [`resolve`] but also returns Deleted records for `Id` refs (used by restore/purge/handles).
/// `Name` refs still only see active branches.
#[allow(unused_variables)]
pub fn resolve_any_state(dir: &dyn KvRead, branch: &BranchRef) -> Result<BranchRecord> {
    Err(PgError::unimplemented("BR-02", "branch resolution including deleted"))
}

/// All branch records (scan of the `B` prefix), sorted by `(name, id)` ascending (bytewise name).
/// `include_deleted = false` filters to Active. Every `B` value is decoded (corruption surfaces).
#[allow(unused_variables)]
pub fn list(dir: &dyn KvRead, include_deleted: bool) -> Result<Vec<BranchRecord>> {
    Err(PgError::unimplemented("BR-02", "branch listing"))
}

/// Id under `Mdefault_branch`, if set (`meta::decode_default_branch`).
#[allow(unused_variables)]
pub fn default_branch(dir: &dyn KvRead) -> Result<Option<BranchId>> {
    Err(PgError::unimplemented("BR-02", "default branch lookup"))
}

/// `Morigin` decoded, if present.
#[allow(unused_variables)]
pub fn origin(dir: &dyn KvRead) -> Result<Option<ExportOrigin>> {
    Err(PgError::unimplemented("BR-02", "export origin lookup"))
}

/// Write `B`+id → `rec.encode()` (does NOT touch `N`; callers maintain the name index).
#[allow(unused_variables)]
pub fn store_record(dir: &mut dyn KvWrite, rec: &BranchRecord) -> Result<()> {
    Err(PgError::unimplemented("BR-02", "branch record store"))
}

/// Write `N`+name → id bytes.
pub fn store_name(dir: &mut dyn KvWrite, name: &str, id: &BranchId) -> Result<()> {
    dir.put(&keys::name_key(name), id.as_bytes())
}

/// Delete `N`+name; returns whether it existed.
pub fn remove_name(dir: &mut dyn KvWrite, name: &str) -> Result<bool> {
    dir.delete(&keys::name_key(name))
}

/// Write `Mformat` = `DIRECTORY_FORMAT_VERSION`.
pub fn store_format(dir: &mut dyn KvWrite) -> Result<()> {
    dir.put(&keys::meta_key(meta::META_FORMAT), &meta::encode_format(DIRECTORY_FORMAT_VERSION))
}

/// Write `Mdefault_branch` (or delete it when `id` is None).
pub fn store_default_branch(dir: &mut dyn KvWrite, id: Option<&BranchId>) -> Result<()> {
    let k = keys::meta_key(meta::META_DEFAULT_BRANCH);
    match id {
        Some(id) => dir.put(&k, &meta::encode_default_branch(id)),
        None => dir.delete(&k).map(|_| ()),
    }
}

/// Write `Morigin`.
pub fn store_origin(dir: &mut dyn KvWrite, origin: &ExportOrigin) -> Result<()> {
    dir.put(&keys::meta_key(meta::META_ORIGIN), &origin.to_json())
}

/// Classify a key (re-export for doctor tooling).
pub fn classify_key(key: &[u8]) -> DirKey {
    keys::classify(key)
}

/// Helper for error paths: `error::branch_not_found` for names, `branch_id_not_found` for ids.
pub fn not_found(branch: &BranchRef) -> PgError {
    match branch {
        BranchRef::Name(n) => error::branch_not_found(n),
        BranchRef::Id(id) => error::branch_id_not_found(id),
    }
}
