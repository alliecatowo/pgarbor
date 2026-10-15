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
use pgarbor_branches::{
    error as branch_error, BranchRecord, BranchRef, BranchState, CompareOptions, CompareReport, CreateOptions,
    CreateSource, DeleteOptions, DetachOptions, DetachReport, PurgeFilter, ResetOptions, ResetTarget,
};
use pgarbor_common::{BranchId, PgError, Result};
use pgarbor_core::BranchAdminOptions;
use pgarbor_storage::CompactOptions;

use crate::database::Database;
use crate::types::BranchInfo;

/// Directory state read once per operation: every branch record (tombstones included) and the id of
/// the default branch, if one is set and active. Used to build [`BranchInfo`] values.
struct DirView {
    all: Vec<BranchRecord>,
    default_id: Option<BranchId>,
}

impl DirView {
    /// Read the directory of `db` (two snapshots of the same container; a concurrent branch
    /// operation can only make the view newer, never inconsistent within one record).
    fn read(db: &Database) -> Result<DirView> {
        let mgr = db.inner.engine.branches();
        let all = mgr.list(true)?;
        let default_id = mgr.default_branch()?.map(|r| r.id);
        Ok(DirView { all, default_id })
    }

    /// Map a record to its public form. The parent name is looked up by id in `all` (a deleted
    /// parent keeps its name, so it is still reported); `is_default` compares ids.
    fn info(&self, rec: &BranchRecord) -> BranchInfo {
        let parent = match rec.parent {
            Some(pid) => self.all.iter().find(|r| r.id == pid).map(|r| r.name.clone()),
            None => None,
        };
        BranchInfo {
            name: rec.name.clone(),
            id: rec.id.to_hyphenated(),
            parent,
            is_default: self.default_id == Some(rec.id),
            deleted: rec.state == BranchState::Deleted,
            created_unix_ms: rec.created_unix_ms,
            updated_unix_ms: rec.updated_unix_ms,
            head_seq: rec.head_seq.0,
            snapshots: rec.snapshots.iter().map(|s| s.label.clone()).collect(),
        }
    }
}

/// `BranchInfo` of one record, read against the directory as it is now (after a mutation).
fn info_of(db: &Database, rec: &BranchRecord) -> Result<BranchInfo> {
    Ok(DirView::read(db)?.info(rec))
}

/// Admin options for a destructive call made from the Rust API (no calling session).
fn admin(force: bool) -> BranchAdminOptions {
    BranchAdminOptions { force, caller: None }
}

/// Id of the tombstone to restore for a name. When several tombstones share the name, the most
/// recently deleted one (highest `deleted_at_seq`) wins. An active branch of that name without a
/// tombstone is 55000 `branch "x" is not deleted`; no branch of that name at all is 42704.
fn tombstone_id_by_name(db: &Database, name: &str) -> Result<BranchId> {
    let all = db.inner.engine.branches().list(true)?;
    let mut best: Option<&BranchRecord> = None;
    for r in all.iter().filter(|r| r.name == name && r.state == BranchState::Deleted) {
        let better = match best {
            None => true,
            Some(b) => r.deleted_at_seq > b.deleted_at_seq,
        };
        if better {
            best = Some(r);
        }
    }
    if let Some(r) = best {
        return Ok(r.id);
    }
    if all.iter().any(|r| r.name == name && r.is_active()) {
        return Err(branch_error::branch_not_deleted(name));
    }
    Err(branch_error::branch_not_found(name))
}

impl Database {
    /// `CREATE DATABASE`-style fork of `from`'s current head into a new branch `name` (O(1)).
    pub fn create_branch(&self, name: &str, from: &str) -> Result<BranchInfo> {
        let rec = self.inner.engine.create_branch(name, &CreateSource::branch(from), &CreateOptions::default())?;
        info_of(self, &rec)
    }

    /// Fork a new branch `name` from a named snapshot `label` of branch `branch` (O(1)).
    pub fn create_branch_from_snapshot(&self, name: &str, branch: &str, label: &str) -> Result<BranchInfo> {
        let source = CreateSource::Snapshot { branch: BranchRef::name(branch), label: label.to_string() };
        let rec = self.inner.engine.create_branch(name, &source, &CreateOptions::default())?;
        info_of(self, &rec)
    }

    /// All branches in name order; tombstones only when `include_deleted`.
    pub fn list_branches(&self, include_deleted: bool) -> Result<Vec<BranchInfo>> {
        let view = DirView::read(self)?;
        Ok(view.all.iter().filter(|r| include_deleted || r.is_active()).map(|r| view.info(r)).collect())
    }

    /// Delete branch `name` (retained tombstone, restorable). 55006 while other sessions use it unless
    /// `force`, which terminates them. The default branch cannot be deleted.
    pub fn delete_branch(&self, name: &str, force: bool) -> Result<BranchInfo> {
        let rec = self.inner.engine.delete_branch(&BranchRef::name(name), &DeleteOptions::default(), &admin(force))?;
        info_of(self, &rec)
    }

    /// Rename active branch `old` to `new`.
    pub fn rename_branch(&self, old: &str, new: &str) -> Result<BranchInfo> {
        let rec = self.inner.engine.rename_branch(&BranchRef::name(old), new)?;
        info_of(self, &rec)
    }

    /// Replace the data of branch `name` with `target`'s (BRANCH-06). 55006 while other sessions use
    /// it unless `force`, which terminates them.
    pub fn reset_branch(&self, name: &str, target: ResetTarget, force: bool) -> Result<BranchInfo> {
        let report = self.inner.engine.reset_branch(
            &BranchRef::name(name),
            &target,
            &ResetOptions::default(),
            &admin(force),
        )?;
        info_of(self, &report.branch)
    }

    /// Restore a retained tombstone, given its id or the name of a deleted branch, optionally under
    /// `new_name`.
    pub fn restore_branch(&self, name_or_id: &str, new_name: Option<&str>) -> Result<BranchInfo> {
        let id = match BranchId::parse(name_or_id) {
            Some(id) => id,
            None => tombstone_id_by_name(self, name_or_id)?,
        };
        let rec = self.inner.engine.restore_branch(&id, new_name)?;
        info_of(self, &rec)
    }

    /// Remove every tombstone (all deleted branches); returns how many were removed.
    pub fn purge_deleted(&self) -> Result<usize> {
        let removed = self.inner.engine.purge_deleted(&PurgeFilter::All)?;
        Ok(removed.len())
    }

    /// Add a named snapshot `label` of the current head of `branch`.
    pub fn create_snapshot(&self, branch: &str, label: &str) -> Result<()> {
        self.inner.engine.branches().create_snapshot(&BranchRef::name(branch), label).map(|_| ())
    }

    /// Remove the named snapshot `label` of `branch`.
    pub fn drop_snapshot(&self, branch: &str, label: &str) -> Result<()> {
        self.inner.engine.branches().drop_snapshot(&BranchRef::name(branch), label).map(|_| ())
    }

    /// Make `name` the default branch (used when a session names no branch).
    pub fn set_default_branch(&self, name: &str) -> Result<BranchInfo> {
        let rec = self.inner.engine.branches().set_default(&BranchRef::name(name))?;
        info_of(self, &rec)
    }

    /// Compare the logical content of branches `a` and `b` (key-level differences per space).
    pub fn compare_branches(&self, a: &str, b: &str) -> Result<CompareReport> {
        self.inner.engine.branches().compare(
            &BranchRef::name(a),
            &BranchRef::name(b),
            &CompareOptions::default(),
        )
    }

    /// Write branch `name` as a standalone `.pga` file at `dest` (copy export, fresh database uuid).
    pub fn export_branch(&self, name: &str, dest: &std::path::Path, opts: &DetachOptions) -> Result<DetachReport> {
        self.inner.engine.branches().export(&BranchRef::name(name), dest, opts)
    }

    /// Detach branch `name` into a standalone `.pga` file at `dest` (reflink or copy, see `opts`).
    pub fn detach_branch(&self, name: &str, dest: &std::path::Path, opts: &DetachOptions) -> Result<DetachReport> {
        self.inner.engine.branches().detach(&BranchRef::name(name), dest, opts)
    }

    /// Rewrite the file without unreachable pages (keeps every live branch and snapshot).
    pub fn compact(&self) -> Result<pgarbor_storage::CompactReport> {
        self.inner.engine.compact(&CompactOptions::default())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pgarbor_common::{CommitSeq, PageId};

    fn rec(id: u8, name: &str, parent: Option<u8>, state: BranchState) -> BranchRecord {
        let mut r = BranchRecord::new_active(
            BranchId([id; 16]),
            name,
            parent.map(|p| BranchId([p; 16])),
            PageId::NONE,
            CommitSeq(7),
            1_000,
        );
        r.state = state;
        r.head_seq = CommitSeq(9);
        r
    }

    #[test]
    fn info_resolves_parent_name_default_and_deleted() {
        let main = rec(1, "main", None, BranchState::Active);
        let feature = rec(2, "feature", Some(1), BranchState::Active);
        let old = rec(3, "old", Some(2), BranchState::Deleted);
        let view = DirView { all: vec![main.clone(), feature.clone(), old.clone()], default_id: Some(main.id) };

        let fi = view.info(&feature);
        assert_eq!(fi.name, "feature");
        assert_eq!(fi.parent.as_deref(), Some("main"));
        assert!(!fi.is_default);
        assert!(!fi.deleted);
        assert_eq!(fi.head_seq, 9);
        assert_eq!(fi.id, BranchId([2; 16]).to_hyphenated());

        let mi = view.info(&main);
        assert!(mi.is_default);
        assert_eq!(mi.parent, None);

        let oi = view.info(&old);
        assert!(oi.deleted);
        assert_eq!(oi.parent.as_deref(), Some("feature"));
    }

    #[test]
    fn info_parent_missing_from_listing_is_none() {
        let feature = rec(2, "feature", Some(9), BranchState::Active);
        let view = DirView { all: vec![feature.clone()], default_id: None };
        assert_eq!(view.info(&feature).parent, None);
        assert!(!view.info(&feature).is_default);
    }

    #[test]
    fn tombstone_lookup_prefers_latest_deletion() {
        let mut a = rec(4, "scratch", None, BranchState::Deleted);
        a.deleted_at_seq = CommitSeq(5);
        let mut b = rec(5, "scratch", None, BranchState::Deleted);
        b.deleted_at_seq = CommitSeq(8);
        let all = vec![a.clone(), b.clone()];
        let mut best: Option<&BranchRecord> = None;
        for r in all.iter().filter(|r| r.name == "scratch" && r.state == BranchState::Deleted) {
            let better = match best {
                None => true,
                Some(x) => r.deleted_at_seq > x.deleted_at_seq,
            };
            if better {
                best = Some(r);
            }
        }
        assert_eq!(best.map(|r| r.id), Some(b.id));
    }

    #[test]
    fn admin_options_carry_force_without_caller() {
        assert_eq!(admin(true), BranchAdminOptions { force: true, caller: None });
        assert_eq!(admin(false), BranchAdminOptions { force: false, caller: None });
    }

    #[test]
    #[ignore = "needs API-04"]
    fn create_and_list_feature_branch() {
        let db = Database::in_memory().unwrap();
        let created = db.create_branch("feature", "main").unwrap();
        assert_eq!(created.parent.as_deref(), Some("main"));
        let names: Vec<String> = db.list_branches(false).unwrap().into_iter().map(|b| b.name).collect();
        assert!(names.contains(&"feature".to_string()));
        assert!(names.contains(&"main".to_string()));
    }

    #[test]
    #[ignore = "needs API-04"]
    fn deleting_default_branch_fails() {
        let db = Database::in_memory().unwrap();
        assert!(db.delete_branch("main", false).is_err());
    }
}
