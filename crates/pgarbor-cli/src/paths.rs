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
use pgarbor_common::PgError;
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
    Err(CliError::from(PgError::unimplemented("CLI-02", "resolve the database path (paths::resolve_db)")))
}

/// Database + branch intent for commands with `[FILE] --db --branch --handle`.
#[allow(unused_variables)]
pub fn resolve_target(file: Option<&Path>, flags: &TargetFlags) -> CliResult<Target> {
    Err(CliError::from(PgError::unimplemented("CLI-02", "resolve the target (paths::resolve_target)")))
}

/// Database + branch intent for commands whose branch is a positional argument (`branch exec`, ...).
#[allow(unused_variables)]
pub fn resolve_branch_target(branch: Option<&str>, t: &BranchTarget) -> CliResult<Target> {
    Err(CliError::from(PgError::unimplemented("CLI-02", "resolve the branch target (paths::resolve_branch_target)")))
}

/// Nearest handle file at or above `start` (module docs). `Ok(None)` = none found.
#[allow(unused_variables)]
pub fn find_handle_upwards(start: &Path) -> CliResult<Option<PathBuf>> {
    Err(CliError::from(PgError::unimplemented("CLI-02", "handle discovery (paths::find_handle_upwards)")))
}

/// Decide the branch NAME a command binds to (module docs, "Branch binding").
#[allow(unused_variables)]
pub fn bind_branch(db: &Database, target: &Target) -> CliResult<String> {
    Err(CliError::from(PgError::unimplemented("CLI-02", "bind the branch (paths::bind_branch)")))
}
