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
use crate::directory;
use crate::error;
use crate::options::{BranchRef, HandleOptions};
use crate::record::BranchRecord;
use pgarbor_common::{BranchId, DatabaseUuid, KvRead, PgError, Result, SqlState};
use pgarbor_storage::error as storage_error;
use pgarbor_storage::io::{parent_dir, sibling_path};
use pgarbor_storage::{FileIo, OpenMode, Vfs};
use std::path::{Component, Path, PathBuf};

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
    pub fn to_bytes(&self) -> Result<Vec<u8>> {
        let body = HandleBody {
            format: HANDLE_FORMAT.to_string(),
            db_uuid: self.db_uuid.to_hyphenated(),
            branch_id: self.branch_id.to_hyphenated(),
            branch_name_hint: self.branch_name_hint.clone(),
            db_path_hint: self.db_path_hint.clone(),
            created_unix_ms: self.created_unix_ms,
        };
        let compact = serde_json::to_vec(&body)
            .map_err(|e| PgError::internal(format!("could not encode branch handle: {e}")))?;
        let crc = format!("{:08x}", crc32c::crc32c(&compact));
        let file = HandleJson {
            format: body.format,
            db_uuid: body.db_uuid,
            branch_id: body.branch_id,
            branch_name_hint: body.branch_name_hint,
            db_path_hint: body.db_path_hint,
            created_unix_ms: body.created_unix_ms,
            crc32c: crc,
        };
        let mut text = serde_json::to_string_pretty(&file)
            .map_err(|e| PgError::internal(format!("could not encode branch handle: {e}")))?;
        text.push('\n');
        Ok(text.into_bytes())
    }

    /// Parse and validate file bytes (`path` only for messages). See module docs for errors.
    pub fn parse(path: &Path, bytes: &[u8]) -> Result<HandleFile> {
        if bytes.len() as u64 > MAX_HANDLE_BYTES {
            return Err(oversize_error(path));
        }
        let json: HandleJson = serde_json::from_slice(bytes)
            .map_err(|e| error::handle_invalid(path, format!("not valid JSON: {e}")))?;
        if json.format.as_str() != HANDLE_FORMAT {
            return Err(error::handle_invalid(path, format!("unsupported format \"{}\"", json.format)));
        }
        let db_uuid = match DatabaseUuid::parse(&json.db_uuid) {
            Some(u) if u.to_hyphenated() == json.db_uuid => u,
            _ => return Err(error::handle_invalid(path, "invalid db_uuid")),
        };
        let branch_id = match BranchId::parse(&json.branch_id) {
            Some(id) if id.to_hyphenated() == json.branch_id => id,
            _ => return Err(error::handle_invalid(path, "invalid branch_id")),
        };
        let HandleJson { format, db_uuid: _, branch_id: _, branch_name_hint, db_path_hint, created_unix_ms, crc32c: stored_crc } =
            json;
        // Recompute the checksum over the compact body exactly as `to_bytes` produced it.
        let body = HandleBody {
            format,
            db_uuid: db_uuid.to_hyphenated(),
            branch_id: branch_id.to_hyphenated(),
            branch_name_hint,
            db_path_hint,
            created_unix_ms,
        };
        let compact = serde_json::to_vec(&body)
            .map_err(|e| error::handle_invalid(path, format!("not valid JSON: {e}")))?;
        if format!("{:08x}", crc32c::crc32c(&compact)) != stored_crc {
            return Err(error::handle_invalid(path, "checksum mismatch"));
        }
        Ok(HandleFile {
            db_uuid,
            branch_id,
            branch_name_hint: body.branch_name_hint,
            db_path_hint: body.db_path_hint,
            created_unix_ms: body.created_unix_ms,
        })
    }

    /// Verify against an opened master: `db_uuid` must equal `actual_uuid`
    /// (else `error::handle_db_mismatch(handle_path, &self.db_uuid, db_path, &actual_uuid)`), then
    /// `directory::resolve(dir, &BranchRef::Id(self.branch_id))` (deleted/missing errors as in the
    /// module docs). Returns the active record.
    pub fn check(
        &self,
        handle_path: &Path,
        db_path: &Path,
        actual_uuid: DatabaseUuid,
        dir: &dyn KvRead,
    ) -> Result<BranchRecord> {
        if self.db_uuid != actual_uuid {
            return Err(error::handle_db_mismatch(handle_path, &self.db_uuid, db_path, &actual_uuid));
        }
        directory::resolve(dir, &BranchRef::Id(self.branch_id))
    }
}

/// Write a handle file for `record` of the master at `db_path` (database `db_uuid`) to
/// `handle_path` atomically: temp sibling `<handle_path>.tmp` (`vfs.open(.., CreateTruncate)`),
/// `write_all_at(0, bytes)`, `sync_all`, `vfs.rename(tmp, handle_path)`, `vfs.sync_dir(parent)`.
/// Existing `handle_path` and `!opts.overwrite` → `error::destination_exists`. io errors →
/// `pgarbor_storage::error::{write_failed, fsync_failed, rename_failed}`. Returns the handle.
pub fn write_handle(
    vfs: &dyn Vfs,
    handle_path: &Path,
    db_path: &Path,
    db_uuid: DatabaseUuid,
    record: &BranchRecord,
    created_unix_ms: u64,
    opts: &HandleOptions,
) -> Result<HandleFile> {
    if !opts.overwrite && vfs.exists(handle_path).map_err(|e| storage_error::read_failed(handle_path, &e))? {
        return Err(error::destination_exists(handle_path));
    }
    let handle = HandleFile {
        db_uuid,
        branch_id: record.id,
        branch_name_hint: record.name.clone(),
        db_path_hint: db_path_hint(handle_path, db_path, opts.absolute_db_path)?,
        created_unix_ms,
    };
    let bytes = handle.to_bytes()?;
    let handle_dir = parent_dir(handle_path);
    let tmp = sibling_path(handle_path, ".tmp");
    {
        let file = vfs
            .open(&tmp, OpenMode::CreateTruncate)
            .map_err(|e| storage_error::open_failed(&tmp, &e))?;
        file.write_all_at(0, &bytes).map_err(|e| storage_error::write_failed(&tmp, &e))?;
        file.sync_all().map_err(|e| storage_error::fsync_failed(&tmp, &e))?;
    }
    vfs.rename(&tmp, handle_path).map_err(|e| storage_error::rename_failed(&tmp, handle_path, &e))?;
    vfs.sync_dir(&handle_dir).map_err(|e| storage_error::fsync_failed(&handle_dir, &e))?;
    Ok(handle)
}

/// Read + parse a handle file (`vfs.open(ReadOnly)`, length check against `MAX_HANDLE_BYTES`,
/// `read_exact_at`). Missing file → `pgarbor_storage::error::open_failed` (58P01).
pub fn read_handle(vfs: &dyn Vfs, handle_path: &Path) -> Result<HandleFile> {
    let file = vfs
        .open(handle_path, OpenMode::ReadOnly)
        .map_err(|e| storage_error::open_failed(handle_path, &e))?;
    let len = file.len().map_err(|e| storage_error::read_failed(handle_path, &e))?;
    if len > MAX_HANDLE_BYTES {
        return Err(oversize_error(handle_path));
    }
    let mut buf = vec![0u8; len as usize];
    file.read_exact_at(0, &mut buf).map_err(|e| storage_error::read_failed(handle_path, &e))?;
    HandleFile::parse(handle_path, &buf)
}

/// Find the master database file (module docs, "Resolution").
pub fn resolve_db_path(vfs: &dyn Vfs, handle_path: &Path, explicit_db: Option<&Path>) -> Result<ResolvedHandle> {
    let handle = read_handle(vfs, handle_path)?;
    let mut candidates: Vec<PathBuf> = Vec::with_capacity(3);
    if let Some(explicit) = explicit_db {
        candidates.push(explicit.to_path_buf());
    }
    let hint = Path::new(&handle.db_path_hint);
    if hint.is_absolute() {
        push_unique(&mut candidates, lexical_normalize(hint));
    } else {
        push_unique(&mut candidates, lexical_normalize(&parent_dir(handle_path).join(hint)));
        push_unique(&mut candidates, lexical_normalize(hint));
    }
    for candidate in &candidates {
        let exists = vfs.exists(candidate).map_err(|e| storage_error::read_failed(candidate, &e))?;
        if exists {
            return Ok(ResolvedHandle {
                handle,
                handle_path: handle_path.to_path_buf(),
                db_path: candidate.clone(),
            });
        }
    }
    Err(error::handle_db_not_found(handle_path, &candidates))
}

// ----- private helpers -------------------------------------------------------------------------

fn oversize_error(path: &Path) -> PgError {
    error::handle_invalid(path, format!("file is larger than {MAX_HANDLE_BYTES} bytes"))
}

fn push_unique(list: &mut Vec<PathBuf>, p: PathBuf) {
    if !list.iter().any(|q| *q == p) {
        list.push(p);
    }
}

/// Lexical normalization: drops `.` components and folds `..` into the preceding normal
/// component (a leading `..` on a relative path is kept). No filesystem access.
fn lexical_normalize(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for comp in path.components() {
        match comp {
            Component::CurDir => {}
            Component::ParentDir => {
                let last = match out.components().next_back() {
                    Some(Component::Normal(_)) => 0u8,
                    Some(Component::RootDir) | Some(Component::Prefix(_)) => 1u8,
                    _ => 2u8,
                };
                match last {
                    0 => {
                        out.pop();
                    }
                    1 => {}
                    _ => out.push(".."),
                }
            }
            other => out.push(other.as_os_str()),
        }
    }
    if out.as_os_str().is_empty() {
        out.push(".");
    }
    out
}

/// Make `path` absolute against the current directory, then normalize it lexically.
fn absolutize(path: &Path) -> Result<PathBuf> {
    if path.is_absolute() {
        return Ok(lexical_normalize(path));
    }
    let cwd = std::env::current_dir().map_err(|e| PgError::io("could not determine the current directory", &e))?;
    Ok(lexical_normalize(&cwd.join(path)))
}

/// Render a relative path with `/` separators (the `db_path_hint` form).
fn hint_from_relative(rel: &Path) -> Result<String> {
    let mut parts: Vec<&str> = Vec::new();
    for comp in rel.components() {
        match comp {
            Component::CurDir => {}
            Component::ParentDir => parts.push(".."),
            Component::Normal(s) => parts.push(s.to_str().ok_or_else(|| non_utf8_error(rel))?),
            _ => return Err(non_utf8_error(rel)),
        }
    }
    if parts.is_empty() {
        return Ok(".".to_string());
    }
    Ok(parts.join("/"))
}

fn non_utf8_error(path: &Path) -> PgError {
    PgError::new(
        SqlState::INVALID_PARAMETER_VALUE,
        format!("database path \"{}\" cannot be stored in a branch handle", path.display()),
    )
    .with_detail("Branch handles store paths as UTF-8 text.")
}

/// The `db_path_hint` for a handle at `handle_path` pointing at `db_path`.
fn db_path_hint(handle_path: &Path, db_path: &Path, absolute: bool) -> Result<String> {
    if !absolute {
        if let Some(rel) = relative_path(&parent_dir(handle_path), db_path)? {
            return hint_from_relative(&rel);
        }
    }
    let abs = absolutize(db_path)?;
    match abs.to_str() {
        Some(s) => Ok(s.to_string()),
        None => Err(non_utf8_error(&abs)),
    }
}

/// Lexical relative path from directory `from_dir` to `target` (both made absolute against the
/// current directory first via `std::env::current_dir`; no symlink resolution). `Ok(None)` when
/// they share no common root.
/// Example: from `/w/branches`, target `/w/dev.pga` → `../dev.pga`.
pub fn relative_path(from_dir: &Path, target: &Path) -> Result<Option<PathBuf>> {
    let from = absolutize(from_dir)?;
    let to = absolutize(target)?;
    let fc: Vec<Component<'_>> = from.components().collect();
    let tc: Vec<Component<'_>> = to.components().collect();
    // Different roots (drive letters / UNC prefixes on Windows) share no common ancestor.
    if fc.first() != tc.first() {
        return Ok(None);
    }
    let common = fc.iter().zip(tc.iter()).take_while(|(a, b)| a == b).count();
    let mut rel = PathBuf::new();
    for _ in common..fc.len() {
        rel.push("..");
    }
    for comp in &tc[common..] {
        rel.push(comp.as_os_str());
    }
    if rel.as_os_str().is_empty() {
        rel.push(".");
    }
    Ok(Some(rel))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::record::BranchState;
    use pgarbor_common::{CommitSeq, MemKv, PageId};
    use pgarbor_storage::MemVfs;

    const DB_UUID: &str = "0f1e2d3c-4b5a-6978-8796-a5b4c3d2e1f0";
    const BRANCH_ID: &str = "11111111-2222-4333-8444-555555555555";

    fn db_uuid() -> DatabaseUuid {
        DatabaseUuid::parse(DB_UUID).unwrap()
    }

    fn branch_id() -> BranchId {
        BranchId::parse(BRANCH_ID).unwrap()
    }

    fn sample() -> HandleFile {
        HandleFile {
            db_uuid: db_uuid(),
            branch_id: branch_id(),
            branch_name_hint: "feature/auth".to_string(),
            db_path_hint: "../dev.pga".to_string(),
            created_unix_ms: 1_760_400_000_000,
        }
    }

    const GOLDEN: &str = "{\n  \"format\": \"pgabranch/1\",\n  \"db_uuid\": \"0f1e2d3c-4b5a-6978-8796-a5b4c3d2e1f0\",\n  \"branch_id\": \"11111111-2222-4333-8444-555555555555\",\n  \"branch_name_hint\": \"feature/auth\",\n  \"db_path_hint\": \"../dev.pga\",\n  \"created_unix_ms\": 1760400000000,\n  \"crc32c\": \"5102720c\"\n}\n";

    fn record(name: &str, id: BranchId) -> BranchRecord {
        BranchRecord {
            id,
            name: name.to_string(),
            state: BranchState::Active,
            parent: None,
            data_root: PageId::NONE,
            created_at_seq: CommitSeq(1),
            head_seq: CommitSeq(1),
            created_unix_ms: 1,
            updated_unix_ms: 1,
            generation: 1,
            deleted_at_seq: CommitSeq(0),
            deleted_unix_ms: 0,
            roots_retained: false,
            snapshots: Vec::new(),
            extra_databases: Vec::new(),
        }
    }

    fn err_msg(r: Result<HandleFile>) -> PgError {
        match r {
            Ok(h) => panic!("expected error, got {h:?}"),
            Err(e) => e,
        }
    }

    #[test]
    fn roundtrip() {
        let h = sample();
        let bytes = h.to_bytes().unwrap();
        assert_eq!(HandleFile::parse(Path::new("x.pgabranch"), &bytes).unwrap(), h);
    }

    #[test]
    fn golden_bytes_are_stable() {
        assert_eq!(sample().to_bytes().unwrap(), GOLDEN.as_bytes());
    }

    #[test]
    fn parses_golden_text() {
        let h = HandleFile::parse(Path::new("x.pgabranch"), GOLDEN.as_bytes()).unwrap();
        assert_eq!(h, sample());
    }

    #[test]
    fn flipped_name_hint_is_checksum_mismatch() {
        let text = GOLDEN.replace("feature/auth", "feature/autH");
        let e = err_msg(HandleFile::parse(Path::new("x.pgabranch"), text.as_bytes()));
        assert_eq!(e.sqlstate.code(), "22023");
        assert_eq!(e.message, "invalid branch handle file \"x.pgabranch\": checksum mismatch");
    }

    #[test]
    fn extra_member_is_rejected() {
        let text = GOLDEN.replace("\"crc32c\"", "\"extra\": 1,\n  \"crc32c\"");
        let e = err_msg(HandleFile::parse(Path::new("x.pgabranch"), text.as_bytes()));
        assert_eq!(e.sqlstate.code(), "22023");
        assert!(e.message.starts_with("invalid branch handle file \"x.pgabranch\": not valid JSON: "));
    }

    #[test]
    fn wrong_format_is_rejected() {
        let text = GOLDEN.replace("pgabranch/1", "pgabranch/2");
        let e = err_msg(HandleFile::parse(Path::new("x.pgabranch"), text.as_bytes()));
        assert_eq!(e.message, "invalid branch handle file \"x.pgabranch\": unsupported format \"pgabranch/2\"");
    }

    #[test]
    fn bad_uuid_is_rejected() {
        let text = GOLDEN.replace(DB_UUID, "0f1e2d3c-4b5a-6978-8796-a5b4c3d2e1f");
        let e = err_msg(HandleFile::parse(Path::new("x.pgabranch"), text.as_bytes()));
        assert_eq!(e.message, "invalid branch handle file \"x.pgabranch\": invalid db_uuid");
    }

    #[test]
    fn oversize_and_non_json_are_rejected() {
        let big = vec![b' '; (MAX_HANDLE_BYTES as usize) + 1];
        assert!(HandleFile::parse(Path::new("x"), &big).is_err());
        let e = err_msg(HandleFile::parse(Path::new("x"), b"{not json"));
        assert!(e.message.contains("not valid JSON"));
    }

    #[test]
    fn relative_path_examples() {
        assert_eq!(
            relative_path(Path::new("/w/branches"), Path::new("/w/dev.pga")).unwrap(),
            Some(PathBuf::from("../dev.pga"))
        );
        assert_eq!(
            relative_path(Path::new("/w/a/b"), Path::new("/w/dev.pga")).unwrap(),
            Some(PathBuf::from("../../dev.pga"))
        );
        assert_eq!(
            relative_path(Path::new("/w"), Path::new("/w/sub/dev.pga")).unwrap(),
            Some(PathBuf::from("sub/dev.pga"))
        );
    }

    #[test]
    fn lexical_normalize_folds_parent_components() {
        assert_eq!(lexical_normalize(Path::new("/w/branches/../dev.pga")), PathBuf::from("/w/dev.pga"));
        assert_eq!(lexical_normalize(Path::new("./../dev.pga")), PathBuf::from("../dev.pga"));
        assert_eq!(lexical_normalize(Path::new("/../x")), PathBuf::from("/x"));
    }

    fn write_sample(vfs: &MemVfs, handle_path: &Path, db_path: &Path, opts: &HandleOptions) -> Result<HandleFile> {
        let rec = record("feature/auth", branch_id());
        write_handle(vfs, handle_path, db_path, db_uuid(), &rec, 1_760_400_000_000, opts)
    }

    #[test]
    fn write_then_read_roundtrip_with_relative_hint() {
        let vfs = MemVfs::new();
        vfs.write_bytes(Path::new("/w/dev.pga"), vec![0u8; 8]);
        let hp = Path::new("/w/branches/feature.pgabranch");
        let written = write_sample(&vfs, hp, Path::new("/w/dev.pga"), &HandleOptions::default()).unwrap();
        assert_eq!(written.db_path_hint, "../dev.pga");
        assert_eq!(read_handle(&vfs, hp).unwrap(), written);
        assert!(!vfs.exists(Path::new("/w/branches/feature.pgabranch.tmp")).unwrap());
    }

    #[test]
    fn write_absolute_hint_when_requested() {
        let vfs = MemVfs::new();
        let hp = Path::new("/w/branches/feature.pgabranch");
        let opts = HandleOptions { overwrite: false, absolute_db_path: true };
        let written = write_sample(&vfs, hp, Path::new("/w/dev.pga"), &opts).unwrap();
        assert_eq!(written.db_path_hint, "/w/dev.pga");
    }

    #[test]
    fn existing_handle_needs_overwrite() {
        let vfs = MemVfs::new();
        let hp = Path::new("/w/branches/feature.pgabranch");
        write_sample(&vfs, hp, Path::new("/w/dev.pga"), &HandleOptions::default()).unwrap();
        let e = write_sample(&vfs, hp, Path::new("/w/dev.pga"), &HandleOptions::default()).unwrap_err();
        assert_eq!(e.sqlstate.code(), "58P02");
        assert_eq!(e.message, "file \"/w/branches/feature.pgabranch\" already exists");
        let opts = HandleOptions { overwrite: true, absolute_db_path: false };
        assert!(write_sample(&vfs, hp, Path::new("/w/dev.pga"), &opts).is_ok());
    }

    #[test]
    fn missing_handle_is_58p01() {
        let vfs = MemVfs::new();
        let e = read_handle(&vfs, Path::new("/w/none.pgabranch")).unwrap_err();
        assert_eq!(e.sqlstate.code(), "58P01");
    }

    #[test]
    fn resolve_prefers_explicit_then_hint() {
        let vfs = MemVfs::new();
        vfs.write_bytes(Path::new("/w/dev.pga"), vec![0u8; 8]);
        vfs.write_bytes(Path::new("/elsewhere/other.pga"), vec![0u8; 8]);
        let hp = Path::new("/w/branches/feature.pgabranch");
        write_sample(&vfs, hp, Path::new("/w/dev.pga"), &HandleOptions::default()).unwrap();

        let via_hint = resolve_db_path(&vfs, hp, None).unwrap();
        assert_eq!(via_hint.db_path, PathBuf::from("/w/dev.pga"));
        assert_eq!(via_hint.handle_path, hp.to_path_buf());

        let explicit = resolve_db_path(&vfs, hp, Some(Path::new("/elsewhere/other.pga"))).unwrap();
        assert_eq!(explicit.db_path, PathBuf::from("/elsewhere/other.pga"));

        // A missing explicit path falls through to the hint.
        let fallback = resolve_db_path(&vfs, hp, Some(Path::new("/nope.pga"))).unwrap();
        assert_eq!(fallback.db_path, PathBuf::from("/w/dev.pga"));
    }

    #[test]
    fn resolve_reports_every_tried_path() {
        let vfs = MemVfs::new();
        let hp = Path::new("/w/branches/feature.pgabranch");
        write_sample(&vfs, hp, Path::new("/w/dev.pga"), &HandleOptions::default()).unwrap();
        let e = resolve_db_path(&vfs, hp, Some(Path::new("/nope.pga"))).unwrap_err();
        assert_eq!(e.sqlstate.code(), "58P01");
        assert_eq!(
            e.detail.as_deref(),
            Some("Tried: \"/nope.pga\", \"/w/dev.pga\", \"../dev.pga\".")
        );
    }

    #[test]
    fn check_accepts_renamed_branch_and_rejects_other_database() {
        let h = sample();
        let mut kv = MemKv::default();
        // Branch was renamed after the handle was written: the name is only a hint.
        directory::store_record(&mut kv, &record("renamed", branch_id())).unwrap();
        let hp = Path::new("/w/branches/feature.pgabranch");
        let dp = Path::new("/w/dev.pga");
        let rec = h.check(hp, dp, db_uuid(), &kv).unwrap();
        assert_eq!(rec.name, "renamed");

        let other = DatabaseUuid::parse("aaaaaaaa-bbbb-4ccc-8ddd-eeeeeeeeeeee").unwrap();
        let e = h.check(hp, dp, other, &kv).unwrap_err();
        assert_eq!(e.sqlstate.code(), "55000");
        assert!(e.message.starts_with("branch handle \"/w/branches/feature.pgabranch\" belongs to database "));
    }

    #[test]
    fn check_reports_missing_branch() {
        let h = sample();
        let kv = MemKv::default();
        let e = h
            .check(Path::new("/w/x.pgabranch"), Path::new("/w/dev.pga"), db_uuid(), &kv)
            .unwrap_err();
        assert_eq!(e.sqlstate.code(), "42704");
    }
}
