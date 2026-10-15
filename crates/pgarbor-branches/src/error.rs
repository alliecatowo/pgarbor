//! Error constructors for branch operations. CONTRACT. Fully implemented (no lease).
//!
//! Branch errors are PGArbor product errors (PostgreSQL has no branches), so the wording is ours,
//! but SQLSTATE classes follow PostgreSQL conventions for the analogous object errors
//! (`42704 undefined_object`, `42710 duplicate_object`, `42602 invalid_name`, `55006 object_in_use`,
//! `55000 object_not_in_prerequisite_state`, `XX001 data_corrupted`). Every operation in this crate
//! MUST use these helpers so messages are identical across CLI, embedded API and SQL functions.
use pgarbor_common::{BranchId, DatabaseUuid, PgError, SqlState};
use std::path::{Path, PathBuf};

/// SQLSTATE 58P02 `duplicate_file` (not in pgarbor-common's named list).
pub const DUPLICATE_FILE: SqlState = SqlState(*b"58P02");

/// 42704 `branch "<name>" does not exist`.
pub fn branch_not_found(name: &str) -> PgError {
    PgError::new(SqlState::UNDEFINED_OBJECT, format!("branch \"{name}\" does not exist"))
}

/// 42704 `branch with id <uuid> does not exist`.
pub fn branch_id_not_found(id: &BranchId) -> PgError {
    PgError::new(SqlState::UNDEFINED_OBJECT, format!("branch with id {id} does not exist"))
}

/// 42704 `branch "<name>" has been deleted` with detail `Branch id: <uuid>.`
pub fn branch_deleted(name: &str, id: &BranchId) -> PgError {
    PgError::new(SqlState::UNDEFINED_OBJECT, format!("branch \"{name}\" has been deleted"))
        .with_detail(format!("Branch id: {id}."))
}

/// 42710 `branch "<name>" already exists`.
pub fn branch_exists(name: &str) -> PgError {
    PgError::new(SqlState::DUPLICATE_OBJECT, format!("branch \"{name}\" already exists"))
}

/// 42602 `invalid branch name "<name>"` with `reason` as detail.
pub fn invalid_branch_name(name: &str, reason: &str) -> PgError {
    PgError::new(SqlState::INVALID_NAME, format!("invalid branch name \"{name}\"")).with_detail(reason.to_string())
}

/// 42602 `invalid snapshot name "<label>"` with `reason` as detail.
pub fn invalid_snapshot_label(label: &str, reason: &str) -> PgError {
    PgError::new(SqlState::INVALID_NAME, format!("invalid snapshot name \"{label}\"")).with_detail(reason.to_string())
}

/// 42704 `snapshot "<label>" of branch "<branch>" does not exist`.
pub fn snapshot_not_found(branch: &str, label: &str) -> PgError {
    PgError::new(SqlState::UNDEFINED_OBJECT, format!("snapshot \"{label}\" of branch \"{branch}\" does not exist"))
}

/// 42710 `snapshot "<label>" already exists on branch "<branch>"`.
pub fn snapshot_exists(branch: &str, label: &str) -> PgError {
    PgError::new(SqlState::DUPLICATE_OBJECT, format!("snapshot \"{label}\" already exists on branch \"{branch}\""))
}

/// 54000 `branch "<branch>" has too many snapshots` detail `The limit is <max>.`
pub fn too_many_snapshots(branch: &str, max: usize) -> PgError {
    PgError::new(SqlState::PROGRAM_LIMIT_EXCEEDED, format!("branch \"{branch}\" has too many snapshots"))
        .with_detail(format!("The limit is {max}."))
}

/// 55000 `cannot delete the default branch "<name>"` hint `Make another branch the default first.`
pub fn cannot_delete_default(name: &str) -> PgError {
    PgError::new(SqlState::OBJECT_NOT_IN_PREREQUISITE_STATE, format!("cannot delete the default branch \"{name}\""))
        .with_hint("Make another branch the default first.")
}

/// 55000 `branch "<name>" is not deleted` (restore of an active branch).
pub fn branch_not_deleted(name: &str) -> PgError {
    PgError::new(SqlState::OBJECT_NOT_IN_PREREQUISITE_STATE, format!("branch \"{name}\" is not deleted"))
}

/// 55000 `deleted branch "<name>" was purged and cannot be restored`.
pub fn branch_purged(name: &str) -> PgError {
    PgError::new(
        SqlState::OBJECT_NOT_IN_PREREQUISITE_STATE,
        format!("deleted branch \"{name}\" was purged and cannot be restored"),
    )
}

/// 55000 `branch "<name>" changed concurrently` — the record's generation is not the expected one
/// (it was reset or deleted after the caller read it).
pub fn branch_generation_mismatch(name: &str) -> PgError {
    PgError::new(SqlState::OBJECT_NOT_IN_PREREQUISITE_STATE, format!("branch \"{name}\" was reset or deleted concurrently"))
}

/// XX001 `database file "<path>" has no branch directory`.
pub fn directory_missing(path: &Path) -> PgError {
    PgError::corruption(format!("database file \"{}\" has no branch directory", path.display()))
}

/// XX001 `branch directory is corrupt: <what>`.
pub fn directory_corrupt(what: impl AsRef<str>) -> PgError {
    PgError::corruption(format!("branch directory is corrupt: {}", what.as_ref()))
}

/// XX001 `unsupported branch directory format version <v>` hint `This file was written by a newer PGArbor.`
pub fn unsupported_directory_format(version: u32) -> PgError {
    PgError::corruption(format!("unsupported branch directory format version {version}"))
        .with_hint("This file was written by a newer PGArbor.")
}

/// XX001 `unsupported branch record format version <v>`.
pub fn unsupported_record_format(version: u8) -> PgError {
    PgError::corruption(format!("unsupported branch record format version {version}"))
}

/// 22023 `invalid branch handle file "<path>": <reason>`.
pub fn handle_invalid(path: &Path, reason: impl AsRef<str>) -> PgError {
    PgError::new(
        SqlState::INVALID_PARAMETER_VALUE,
        format!("invalid branch handle file \"{}\": {}", path.display(), reason.as_ref()),
    )
}

/// 55000 `branch handle "<handle>" belongs to database <h>, but "<db>" is database <d>`.
pub fn handle_db_mismatch(handle: &Path, handle_uuid: &DatabaseUuid, db: &Path, db_uuid: &DatabaseUuid) -> PgError {
    PgError::new(
        SqlState::OBJECT_NOT_IN_PREREQUISITE_STATE,
        format!(
            "branch handle \"{}\" belongs to database {}, but \"{}\" is database {}",
            handle.display(),
            handle_uuid,
            db.display(),
            db_uuid
        ),
    )
    .with_hint("A .pgabranch handle only works with the database file it was created from.")
}

/// 58P01 `could not find the database file for branch handle "<handle>"`, detail lists every
/// tried path (`Tried: "a", "b".`), hint `Pass the database path explicitly with --db.`
pub fn handle_db_not_found(handle: &Path, tried: &[PathBuf]) -> PgError {
    let list: Vec<String> = tried.iter().map(|p| format!("\"{}\"", p.display())).collect();
    PgError::new(
        SqlState::UNDEFINED_FILE,
        format!("could not find the database file for branch handle \"{}\"", handle.display()),
    )
    .with_detail(format!("Tried: {}.", list.join(", ")))
    .with_hint("Pass the database path explicitly with --db.")
}

/// 58P02 `file "<path>" already exists` hint `Choose another output path or pass --overwrite.`
pub fn destination_exists(path: &Path) -> PgError {
    PgError::new(DUPLICATE_FILE, format!("file \"{}\" already exists", path.display()))
        .with_hint("Choose another output path or pass --overwrite.")
}

/// 0A000 (NOT an "unimplemented" marker) `reflink cloning is not supported for "<path>"`,
/// detail = reason, hint `Use --strategy auto or --strategy copy.`
pub fn reflink_unsupported(path: &Path, reason: &str) -> PgError {
    PgError::new(
        SqlState::FEATURE_NOT_SUPPORTED,
        format!("reflink cloning is not supported for \"{}\"", path.display()),
    )
    .with_detail(reason.to_string())
    .with_hint("Use --strategy auto or --strategy copy.")
}

/// XX001 `exported database "<path>" failed verification: <msg>`.
pub fn export_verify_failed(path: &Path, msg: &str) -> PgError {
    PgError::corruption(format!("exported database \"{}\" failed verification: {}", path.display(), msg))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn codes() {
        assert_eq!(branch_not_found("x").sqlstate.code(), "42704");
        assert_eq!(branch_exists("x").message, "branch \"x\" already exists");
        assert_eq!(destination_exists(Path::new("a.pga")).sqlstate.code(), "58P02");
        assert!(!reflink_unsupported(Path::new("a"), "EXDEV").is_unimplemented());
    }
}
