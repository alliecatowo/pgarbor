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
use std::ops::Bound;

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
pub fn check_format(dir: &dyn KvRead) -> Result<DirectoryStatus> {
    {
        let mut it = dir.scan(Bound::Unbounded, Bound::Unbounded)?;
        match it.next() {
            None => return Ok(DirectoryStatus::Empty),
            Some(Err(e)) => return Err(e),
            Some(Ok(_)) => {}
        }
    }
    let value = dir
        .get(&keys::meta_key(meta::META_FORMAT))?
        .ok_or_else(|| error::directory_corrupt("missing format marker"))?;
    let version = meta::decode_format(&value)?;
    if version != DIRECTORY_FORMAT_VERSION {
        return Err(error::unsupported_directory_format(version));
    }
    Ok(DirectoryStatus::Ready)
}

/// Record stored under `B`+id (any state), decoded with `BranchRecord::decode`; `Ok(None)` when absent.
/// Also enforces the key/id consistency check.
pub fn load_record(dir: &dyn KvRead, id: &BranchId) -> Result<Option<BranchRecord>> {
    let Some(bytes) = dir.get(&keys::branch_key(id))? else {
        return Ok(None);
    };
    let rec = BranchRecord::decode(&bytes)?;
    if rec.id != *id {
        return Err(error::directory_corrupt(format!("branch record {id} has id {}", rec.id)));
    }
    Ok(Some(rec))
}

/// Id stored under `N`+name; `Ok(None)` when absent. Malformed value → XX001.
pub fn lookup_name(dir: &dyn KvRead, name: &str) -> Result<Option<BranchId>> {
    let Some(value) = dir.get(&keys::name_key(name))? else {
        return Ok(None);
    };
    match keys::decode_id_value(&value) {
        Some(id) => Ok(Some(id)),
        None => Err(error::directory_corrupt(format!("name entry \"{name}\" has a malformed id"))),
    }
}

/// Resolve a name through the `N` index to an active record, applying the consistency checks.
fn resolve_name(dir: &dyn KvRead, name: &str) -> Result<BranchRecord> {
    let id = lookup_name(dir, name)?.ok_or_else(|| error::branch_not_found(name))?;
    let rec = load_record(dir, &id)?.ok_or_else(|| {
        error::directory_corrupt(format!("name \"{name}\" points to missing branch {id}"))
    })?;
    if !rec.is_active() {
        return Err(error::directory_corrupt(format!("name \"{name}\" points to deleted branch {id}")));
    }
    if rec.name != name {
        return Err(error::directory_corrupt(format!(
            "name \"{name}\" points to branch {id} named \"{}\"",
            rec.name
        )));
    }
    Ok(rec)
}

/// Resolve to an ACTIVE branch record.
/// - `Name(n)`: `N` lookup (absent → `error::branch_not_found(n)`), then `load_record` with the
///   consistency checks above.
/// - `Id(id)`: `load_record` (absent → `error::branch_id_not_found(id)`; Deleted →
///   `error::branch_deleted(record.name, id)`).
pub fn resolve(dir: &dyn KvRead, branch: &BranchRef) -> Result<BranchRecord> {
    match branch {
        BranchRef::Name(n) => resolve_name(dir, n),
        BranchRef::Id(id) => {
            let rec = load_record(dir, id)?.ok_or_else(|| error::branch_id_not_found(id))?;
            if !rec.is_active() {
                return Err(error::branch_deleted(&rec.name, id));
            }
            Ok(rec)
        }
    }
}

/// Like [`resolve`] but also returns Deleted records for `Id` refs (used by restore/purge/handles).
/// `Name` refs still only see active branches.
pub fn resolve_any_state(dir: &dyn KvRead, branch: &BranchRef) -> Result<BranchRecord> {
    match branch {
        BranchRef::Name(n) => resolve_name(dir, n),
        BranchRef::Id(id) => load_record(dir, id)?.ok_or_else(|| error::branch_id_not_found(id)),
    }
}

/// All branch records (scan of the `B` prefix), sorted by `(name, id)` ascending (bytewise name).
/// `include_deleted = false` filters to Active. Every `B` value is decoded (corruption surfaces).
pub fn list(dir: &dyn KvRead, include_deleted: bool) -> Result<Vec<BranchRecord>> {
    let mut out = Vec::new();
    for item in dir.scan_prefix(&[keys::BRANCH_PREFIX])? {
        let (key, value) = item?;
        let key_id = match keys::classify(&key) {
            DirKey::Branch(id) => id,
            _ => return Err(error::directory_corrupt("malformed branch record key")),
        };
        let rec = BranchRecord::decode(&value)?;
        if rec.id != key_id {
            return Err(error::directory_corrupt(format!("branch record {key_id} has id {}", rec.id)));
        }
        if include_deleted || rec.is_active() {
            out.push(rec);
        }
    }
    out.sort_by(|a, b| a.name.as_bytes().cmp(b.name.as_bytes()).then_with(|| a.id.cmp(&b.id)));
    Ok(out)
}

/// Id under `Mdefault_branch`, if set (`meta::decode_default_branch`).
pub fn default_branch(dir: &dyn KvRead) -> Result<Option<BranchId>> {
    match dir.get(&keys::meta_key(meta::META_DEFAULT_BRANCH))? {
        Some(value) => meta::decode_default_branch(&value).map(Some),
        None => Ok(None),
    }
}

/// `Morigin` decoded, if present.
pub fn origin(dir: &dyn KvRead) -> Result<Option<ExportOrigin>> {
    match dir.get(&keys::meta_key(meta::META_ORIGIN))? {
        Some(value) => ExportOrigin::from_json(&value).map(Some),
        None => Ok(None),
    }
}

/// Write `B`+id → `rec.encode()` (does NOT touch `N`; callers maintain the name index).
pub fn store_record(dir: &mut dyn KvWrite, rec: &BranchRecord) -> Result<()> {
    let bytes = rec.encode()?;
    dir.put(&keys::branch_key(&rec.id), &bytes)
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

#[cfg(test)]
mod tests {
    use super::*;
    use pgarbor_common::{CommitSeq, DatabaseUuid, MemKv, PageId};

    fn active(id: u8, name: &str) -> BranchRecord {
        BranchRecord::new_active(BranchId([id; 16]), name, None, PageId(0), CommitSeq(1), 1000)
    }

    fn deleted(id: u8, name: &str) -> BranchRecord {
        let mut r = active(id, name);
        r.state = crate::record::BranchState::Deleted;
        r.deleted_at_seq = CommitSeq(2);
        r.deleted_unix_ms = 2000;
        r
    }

    fn err_of<T: std::fmt::Debug>(r: Result<T>) -> PgError {
        match r {
            Ok(v) => panic!("expected an error, got {v:?}"),
            Err(e) => e,
        }
    }

    fn put_record(kv: &mut MemKv, rec: &BranchRecord) {
        store_record(kv, rec).unwrap();
    }

    #[test]
    fn empty_directory_is_empty() {
        let kv = MemKv::default();
        assert_eq!(check_format(&kv).unwrap(), DirectoryStatus::Empty);
    }

    #[test]
    fn format_marker_is_ready() {
        let mut kv = MemKv::default();
        store_format(&mut kv).unwrap();
        assert_eq!(check_format(&kv).unwrap(), DirectoryStatus::Ready);
    }

    #[test]
    fn unsupported_format_version_is_xx001() {
        let mut kv = MemKv::default();
        kv.put(&keys::meta_key(meta::META_FORMAT), &2u32.to_le_bytes()).unwrap();
        let e = err_of(check_format(&kv));
        assert_eq!(e.sqlstate.code(), "XX001");
        assert_eq!(e.message, "unsupported branch directory format version 2");
    }

    #[test]
    fn missing_marker_with_keys_is_xx001() {
        let mut kv = MemKv::default();
        put_record(&mut kv, &active(1, "main"));
        let e = err_of(check_format(&kv));
        assert_eq!(e.sqlstate.code(), "XX001");
        assert_eq!(e.message, "branch directory is corrupt: missing format marker");
    }

    #[test]
    fn list_sorts_by_name_then_id_and_filters_deleted() {
        let mut kv = MemKv::default();
        store_format(&mut kv).unwrap();
        put_record(&mut kv, &active(0x01, "beta"));
        put_record(&mut kv, &active(0x10, "alpha"));
        put_record(&mut kv, &deleted(0x05, "alpha"));

        let all = list(&kv, true).unwrap();
        let ids: Vec<u8> = all.iter().map(|r| r.id.0[0]).collect();
        assert_eq!(ids, vec![0x05, 0x10, 0x01]);

        let live = list(&kv, false).unwrap();
        let ids: Vec<u8> = live.iter().map(|r| r.id.0[0]).collect();
        assert_eq!(ids, vec![0x10, 0x01]);
    }

    #[test]
    fn resolve_by_name_and_id() {
        let mut kv = MemKv::default();
        store_format(&mut kv).unwrap();
        put_record(&mut kv, &active(0x10, "alpha"));
        store_name(&mut kv, "alpha", &BranchId([0x10; 16])).unwrap();

        let by_name = resolve(&kv, &BranchRef::name("alpha")).unwrap();
        assert_eq!(by_name.id, BranchId([0x10; 16]));
        let by_id = resolve(&kv, &BranchRef::Id(BranchId([0x10; 16]))).unwrap();
        assert_eq!(by_id.name, "alpha");

        let e = err_of(resolve(&kv, &BranchRef::name("nope")));
        assert_eq!(e.sqlstate.code(), "42704");
        assert_eq!(e.message, "branch \"nope\" does not exist");
    }

    #[test]
    fn resolve_deleted_id_is_deleted_but_any_state_returns_it() {
        let mut kv = MemKv::default();
        put_record(&mut kv, &deleted(0x05, "alpha"));
        let id = BranchRef::Id(BranchId([0x05; 16]));

        let e = err_of(resolve(&kv, &id));
        assert_eq!(e.sqlstate.code(), "42704");
        assert_eq!(e.message, "branch \"alpha\" has been deleted");
        assert!(e.detail.as_deref().unwrap_or("").starts_with("Branch id: "));

        let rec = resolve_any_state(&kv, &id).unwrap();
        assert_eq!(rec.state, crate::record::BranchState::Deleted);

        // Names never see deleted branches, even through resolve_any_state.
        store_name(&mut kv, "alpha", &BranchId([0x05; 16])).unwrap();
        let e = err_of(resolve_any_state(&kv, &BranchRef::name("alpha")));
        assert_eq!(e.sqlstate.code(), "XX001");
        assert!(e.message.contains("name \"alpha\" points to deleted branch"), "{}", e.message);
    }

    #[test]
    fn dangling_name_entry_is_xx001() {
        let mut kv = MemKv::default();
        store_name(&mut kv, "ghost", &BranchId([0x77; 16])).unwrap();
        let e = err_of(resolve(&kv, &BranchRef::name("ghost")));
        assert_eq!(e.sqlstate.code(), "XX001");
        assert!(e.message.starts_with("branch directory is corrupt: name \"ghost\" points to missing branch "));
    }

    #[test]
    fn name_entry_pointing_at_deleted_record_is_xx001() {
        let mut kv = MemKv::default();
        put_record(&mut kv, &deleted(0x05, "alpha"));
        store_name(&mut kv, "alpha", &BranchId([0x05; 16])).unwrap();
        let e = err_of(resolve(&kv, &BranchRef::name("alpha")));
        assert_eq!(e.sqlstate.code(), "XX001");
        assert!(e.message.contains("points to deleted branch"), "{}", e.message);
    }

    #[test]
    fn name_entry_pointing_at_other_name_is_xx001() {
        let mut kv = MemKv::default();
        put_record(&mut kv, &active(0x10, "alpha"));
        store_name(&mut kv, "beta", &BranchId([0x10; 16])).unwrap();
        let e = err_of(resolve(&kv, &BranchRef::name("beta")));
        assert_eq!(e.sqlstate.code(), "XX001");
        assert!(e.message.ends_with("named \"alpha\""), "{}", e.message);
    }

    #[test]
    fn malformed_name_value_is_xx001() {
        let mut kv = MemKv::default();
        kv.put(&keys::name_key("bad"), b"short").unwrap();
        let e = err_of(lookup_name(&kv, "bad"));
        assert_eq!(e.sqlstate.code(), "XX001");
        assert_eq!(e.message, "branch directory is corrupt: name entry \"bad\" has a malformed id");
    }

    #[test]
    fn record_key_id_mismatch_is_xx001() {
        let mut kv = MemKv::default();
        let rec = active(0x02, "alpha");
        kv.put(&keys::branch_key(&BranchId([0x01; 16])), &rec.encode().unwrap()).unwrap();
        let e = err_of(load_record(&kv, &BranchId([0x01; 16])));
        assert_eq!(e.sqlstate.code(), "XX001");
        assert!(e.message.starts_with("branch directory is corrupt: branch record "), "{}", e.message);
        assert!(e.message.contains(" has id "), "{}", e.message);
    }

    #[test]
    fn default_branch_and_origin_roundtrip() {
        let mut kv = MemKv::default();
        assert_eq!(default_branch(&kv).unwrap(), None);
        assert_eq!(origin(&kv).unwrap(), None);

        let id = BranchId([0x33; 16]);
        store_default_branch(&mut kv, Some(&id)).unwrap();
        assert_eq!(default_branch(&kv).unwrap(), Some(id));
        store_default_branch(&mut kv, None).unwrap();
        assert_eq!(default_branch(&kv).unwrap(), None);

        let o = ExportOrigin {
            source_db_uuid: DatabaseUuid([1; 16]).to_hyphenated(),
            source_branch_id: BranchId([2; 16]).to_hyphenated(),
            source_branch_name: "feature".into(),
            source_commit_seq: 9,
            exported_unix_ms: 5,
            strategy: "copy".into(),
        };
        store_origin(&mut kv, &o).unwrap();
        assert_eq!(origin(&kv).unwrap(), Some(o));
    }

    #[test]
    fn remove_name_reports_presence() {
        let mut kv = MemKv::default();
        store_name(&mut kv, "x", &BranchId([9; 16])).unwrap();
        assert!(remove_name(&mut kv, "x").unwrap());
        assert!(!remove_name(&mut kv, "x").unwrap());
    }
}
