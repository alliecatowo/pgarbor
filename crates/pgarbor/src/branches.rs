//! Branch administration on [`Database`] (lease API-16). Delegates to `pgarbor_core::Engine` (session
//! aware: refuses destructive operations while other sessions use the branch, 55006) and
//! `pgarbor_branches::BranchManager` (directory, snapshots, compare, export).
//!
//! | method | call |
//! |---|---|
//! | `create_branch(name, from)` | `engine.create_branch(name, &CreateSource::branch(from), &CreateOptions::default())` (O(1) fork; sequence state flushed first; the catalog is inherited) |
//! | `create_branch_from_snapshot(name, branch, label)` | `CreateSource::Snapshot{..}` |
//! | `list_branches(include_deleted)` | `engine.branches().list(..)` -> `BranchInfo` (parent NAME resolved from the id, `is_default` from `default_branch`) |
//! | `delete_branch(name, force)` | `engine.delete_branch(&BranchRef::name(name), &DeleteOptions::default(), &BranchAdminOptions{force, caller: None})` (the default branch cannot be deleted) |
//! | `rename_branch(old, new)` | `engine.rename_branch` |
//! | `reset_branch(name, target, force)` | `engine.reset_branch(.., &ResetTarget, &ResetOptions, &BranchAdminOptions)`; also drops the catalog cache entries nobody can reach? (not needed: cache is content-addressed) |
//! | `restore_branch(name_or_id, new_name)` / `purge_deleted()` | `engine.restore_branch`, `engine.purge_deleted(&PurgeFilter::All)` |
//! | `create_snapshot(branch, label)` / `drop_snapshot` | `engine.branches().create_snapshot/drop_snapshot` |
//! | `set_default_branch(name)` | `branches().set_default` |
//! | `compare_branches(a, b)` | `branches().compare(&a, &b, &CompareOptions::default())` (`CompareReport`) |
//! | `export_branch(name, dest, opts)` / `detach_branch` | `branches().export / detach` with `DetachOptions` (standalone file, fresh db uuid) |
//! | `compact()` | `engine.compact(&CompactOptions::default())` |
//! | `verify()` | `branches().verify(VerifyLevel::Full)` -> `DoctorReport` |
//! Errors are those of the lower layers (branch-not-found, name syntax `invalid branch name`, in-use 55006,
//! ...), unchanged.
use pgarbor_branches::{CompareReport, DetachOptions, DetachReport, ResetTarget};
use pgarbor_common::{PgError, Result};

use crate::database::Database;
use crate::types::BranchInfo;

impl Database {
    #[allow(unused_variables)]
    pub fn create_branch(&self, name: &str, from: &str) -> Result<BranchInfo> {
        Err(PgError::unimplemented("API-16", "create branch (Database::create_branch)"))
    }

    #[allow(unused_variables)]
    pub fn create_branch_from_snapshot(&self, name: &str, branch: &str, label: &str) -> Result<BranchInfo> {
        Err(PgError::unimplemented("API-16", "create branch from snapshot (Database::create_branch_from_snapshot)"))
    }

    #[allow(unused_variables)]
    pub fn list_branches(&self, include_deleted: bool) -> Result<Vec<BranchInfo>> {
        Err(PgError::unimplemented("API-16", "list branches (Database::list_branches)"))
    }

    #[allow(unused_variables)]
    pub fn delete_branch(&self, name: &str, force: bool) -> Result<BranchInfo> {
        Err(PgError::unimplemented("API-16", "delete branch (Database::delete_branch)"))
    }

    #[allow(unused_variables)]
    pub fn rename_branch(&self, old: &str, new: &str) -> Result<BranchInfo> {
        Err(PgError::unimplemented("API-16", "rename branch (Database::rename_branch)"))
    }

    #[allow(unused_variables)]
    pub fn reset_branch(&self, name: &str, target: ResetTarget, force: bool) -> Result<BranchInfo> {
        Err(PgError::unimplemented("API-16", "reset branch (Database::reset_branch)"))
    }

    #[allow(unused_variables)]
    pub fn restore_branch(&self, name_or_id: &str, new_name: Option<&str>) -> Result<BranchInfo> {
        Err(PgError::unimplemented("API-16", "restore branch (Database::restore_branch)"))
    }

    pub fn purge_deleted(&self) -> Result<usize> {
        Err(PgError::unimplemented("API-16", "purge deleted branches (Database::purge_deleted)"))
    }

    #[allow(unused_variables)]
    pub fn create_snapshot(&self, branch: &str, label: &str) -> Result<()> {
        Err(PgError::unimplemented("API-16", "create snapshot (Database::create_snapshot)"))
    }

    #[allow(unused_variables)]
    pub fn drop_snapshot(&self, branch: &str, label: &str) -> Result<()> {
        Err(PgError::unimplemented("API-16", "drop snapshot (Database::drop_snapshot)"))
    }

    #[allow(unused_variables)]
    pub fn set_default_branch(&self, name: &str) -> Result<BranchInfo> {
        Err(PgError::unimplemented("API-16", "set default branch (Database::set_default_branch)"))
    }

    #[allow(unused_variables)]
    pub fn compare_branches(&self, a: &str, b: &str) -> Result<CompareReport> {
        Err(PgError::unimplemented("API-16", "compare branches (Database::compare_branches)"))
    }

    #[allow(unused_variables)]
    pub fn export_branch(&self, name: &str, dest: &std::path::Path, opts: &DetachOptions) -> Result<DetachReport> {
        Err(PgError::unimplemented("API-16", "export branch (Database::export_branch)"))
    }

    #[allow(unused_variables)]
    pub fn detach_branch(&self, name: &str, dest: &std::path::Path, opts: &DetachOptions) -> Result<DetachReport> {
        Err(PgError::unimplemented("API-16", "detach branch (Database::detach_branch)"))
    }

    pub fn compact(&self) -> Result<pgarbor_storage::CompactReport> {
        Err(PgError::unimplemented("API-16", "compact (Database::compact)"))
    }
}
