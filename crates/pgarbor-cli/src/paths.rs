//! Locating the database, the branch and `.pgabranch` handles (lease CLI-02). RFC 001 sections 3, 4.1.
//!
//! ## Resolution order for the database file ([`resolve_db`])
//! 1. positional FILE argument (if the command has one);
//! 2. `--db FILE`;
//! 3. environment `PGARBOR_DB` (non-empty);
//! 4. the nearest handle file: [`find_handle_upwards`] from the current directory (see below) ->
//!    `branches::handle::resolve_db_path(&OsVfs, handle, None)`;
//! 5. otherwise `CliError::usage("no database specified")` with hint
//!    `pass FILE, --db FILE, set PGARBOR_DB, or run inside a directory with a .pgabranch handle`.
//! FILE and `--db` both given and different: `CliError::usage("both FILE and --db were given and differ")`.
//! A path that does not exist is NOT an error here (commands decide: `init` creates it, others fail with
//! exit 5 via `open.rs`).
//!
//! ## Handles
//! `--handle H` names a `.pgabranch` file explicitly (error 5 `handle file "H" does not exist` if
//! missing). Discovery ([`find_handle_upwards`]): for `dir` in `start`, `start/..`, ... up to the root:
//! a file named exactly `.pgabranch` in `dir`, else the only `*.pgabranch` file in `dir`. Several
//! `*.pgabranch` files and no exact `.pgabranch` in one directory is ambiguous: stop and return
//! `CliError::usage("several .pgabranch files in <dir>: pass --handle")`.
//! The master database path comes from `pgarbor_branches::handle::resolve_db_path(&OsVfs::default(),
//! handle, explicit_db)` (searches the hint, then relative variants; error 5 with the tried paths
//! when not found, 65 `DATA_ERR` for a malformed handle). The handle also supplies the branch IDENTITY
//! (`branch_id`) and a name hint.
//!
//! ## Branch binding ([`bind_branch`])
//! Once the database is open: if the target came from a handle, find the active branch with that id
//! (`Database::list_branches(false)`), verify `handle.db_uuid` equals the container's uuid
//! (`HandleFile::check`, mismatch is a loud exit 65 error), and use the branch's CURRENT name (rename
//! safe). If the branch id is gone: exit 5 `branch "<hint>" (id <uuid>) no longer exists in "<db>"`. Else
//! `--branch NAME`, else `$PGARBOR_BRANCH`, else the default branch (`is_default`), else the only
//! active branch, else exit 64 `database has no default branch: pass --branch`.
use std::path::{Path, PathBuf};

use pgarbor::Database;
use pgarbor_common::BranchId;

use crate::cli::{BranchTarget, TargetFlags};
use crate::ctx::{CliError, CliResult};

pub const ENV_DB: &str = "PGARBOR_DB";
pub const ENV_BRANCH: &str = "PGARBOR_BRANCH";
/// Conventional handle file name looked up by discovery.
pub const HANDLE_FILE_NAME: &str = ".pgabranch";

/// A resolved database + branch intent.
// CONTRACT
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Target {
    pub db_path: PathBuf,
    /// Explicit branch name (flag, positional or env), if any.
    pub branch: Option<String>,
    /// Set when the target came from a handle: the branch identity and the handle path.
    pub from_handle: Option<HandleOrigin>,
}

/// What a handle contributed.
// CONTRACT
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HandleOrigin {
    pub handle_path: PathBuf,
    pub db_uuid: String,
    pub branch_id: BranchId,
    pub branch_name_hint: String,
}

/// Database path only (module docs order 1-5).
#[allow(unused_variables)]
pub fn resolve_db(file: Option<&Path>, db: Option<&Path>) -> CliResult<PathBuf> {
    if let (Some(a), Some(b)) = (file, db) {
        if a != b { return Err(CliError::usage("both FILE and --db were given and differ")); }
    }
    if let Some(path) = file.or(db) { return Ok(path.to_path_buf()); }
    if let Some(path) = std::env::var_os(ENV_DB).filter(|v| !v.is_empty()) { return Ok(path.into()); }
    let cwd = std::env::current_dir().map_err(CliError::from)?;
    if let Some(handle) = find_handle_upwards(&cwd)? {
        return Ok(pgarbor_branches::handle::resolve_db_path(&pgarbor_storage::OsVfs, &handle, None)?.db_path);
    }
    Err(CliError::usage("no database specified").with_hint("pass FILE, --db FILE, set PGARBOR_DB, or run inside a directory with a .pgabranch handle"))
}

/// Database + branch intent for commands with `[FILE] --db --branch --handle`.
#[allow(unused_variables)]
pub fn resolve_target(file: Option<&Path>, flags: &TargetFlags) -> CliResult<Target> {
    let explicit = file.or(flags.db.as_deref());
    if file.is_some() && flags.db.is_some() { resolve_db(file, flags.db.as_deref())?; }
    let env_db = std::env::var_os(ENV_DB).filter(|v| !v.is_empty()).map(PathBuf::from);
    let selected_db = explicit.or(env_db.as_deref());
    let handle = match &flags.handle {
        Some(path) => Some(path.clone()),
        None if selected_db.is_none() => find_handle_upwards(&std::env::current_dir()?)?,
        None => None,
    };
    let (db_path, from_handle) = if let Some(path) = handle {
        if !path.exists() { return Err(CliError::not_found(format!("handle file \"{}\" does not exist", path.display()))); }
        let r = pgarbor_branches::handle::resolve_db_path(&pgarbor_storage::OsVfs, &path, selected_db)?;
        let h = r.handle;
        (r.db_path, Some(HandleOrigin { handle_path: path, db_uuid: h.db_uuid.to_hyphenated(), branch_id: h.branch_id, branch_name_hint: h.branch_name_hint }))
    } else { (resolve_db(file, flags.db.as_deref())?, None) };
    let branch = flags.branch.clone().or_else(|| std::env::var(ENV_BRANCH).ok().filter(|s| !s.is_empty()));
    Ok(Target { db_path, branch, from_handle })
}

/// Database + branch intent for commands whose branch is a positional argument (`branch exec`, ...).
#[allow(unused_variables)]
pub fn resolve_branch_target(branch: Option<&str>, t: &BranchTarget) -> CliResult<Target> {
    resolve_target(None, &TargetFlags { db: t.db.clone(), branch: branch.map(str::to_owned), handle: t.handle.clone() })
}

/// Nearest handle file at or above `start` (module docs). `Ok(None)` = none found.
#[allow(unused_variables)]
pub fn find_handle_upwards(start: &Path) -> CliResult<Option<PathBuf>> {
    for dir in start.ancestors() {
        let exact = dir.join(HANDLE_FILE_NAME);
        if exact.is_file() { return Ok(Some(exact)); }
        let mut found = Vec::new();
        for entry in std::fs::read_dir(dir)? {
            let path = entry?.path();
            if path.extension().is_some_and(|e| e == "pgabranch") && path.is_file() { found.push(path); }
        }
        match found.len() {
            0 => {}, 1 => return Ok(found.pop()),
            _ => return Err(CliError::usage(format!("several .pgabranch files in {}: pass --handle", dir.display()))),
        }
    }
    Ok(None)
}

/// Decide the branch NAME a command binds to (module docs, "Branch binding").
#[allow(unused_variables)]
pub fn bind_branch(db: &Database, target: &Target) -> CliResult<String> {
    let branches = db.list_branches(false)?;
    if let Some(origin) = &target.from_handle {
        if origin.db_uuid != db.engine().container().db_uuid().to_hyphenated() {
            return Err(CliError::data("branch handle database UUID does not match"));
        }
        // An explicit branch flag takes precedence, but handle identity is always verified.
        let current = branches.iter().find(|b| b.id == origin.branch_id.to_hyphenated())
            .ok_or_else(|| CliError::not_found(format!("branch \"{}\" no longer exists", origin.branch_name_hint)))?;
        if let Some(name) = &target.branch { return Ok(name.clone()); }
        return Ok(current.name.clone());
    }
    if let Some(name) = &target.branch { return Ok(name.clone()); }
    if let Some(b) = branches.iter().find(|b| b.is_default).or_else(|| if branches.len() == 1 { branches.first() } else { None }) {
        return Ok(b.name.clone());
    }
    Err(CliError::usage("database has no default branch: pass --branch"))
}
