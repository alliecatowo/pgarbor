//! `BranchManager`: one storage commit per branch operation. CONTRACT. Fully implemented glue
//! (no lease): every method delegates to `directory`/`ops`/`history`/`compare`/`space`/`handle`/
//! `export`/`detach`, whose bodies are leased.
//!
//! Concurrency: each mutating method holds the container's single writer (`begin_write`) for the
//! duration of one small directory update, so branch operations serialize with pgarbor-core's
//! data commits. Readers never block. pgarbor-core wraps the destructive operations (delete,
//! reset, purge) with active-session checks; calling them directly bypasses those checks (CLI
//! offline mode owns the file exclusively, so there are no sessions).
use crate::compare::{self, CompareReport};
use crate::detach::{self, DetachReport};
use crate::directory::{self, DirectoryStatus};
use crate::export;
use crate::handle::{self, HandleFile};
use crate::history::{self, ResetReport};
use crate::ops::{self, OpCtx};
use crate::options::*;
use crate::record::{BranchRecord, NamedSnapshot};
use crate::resolver::BranchRootResolver;
use crate::space::{self, BranchSpace};
use pgarbor_common::{BranchId, CommitSeq, KvWrite, PageId, Result};
use pgarbor_storage::format::superblock::now_unix_ms;
use pgarbor_storage::{CompactOptions, CompactReport, Container, DoctorReport, Snapshot, SpaceReport, VerifyLevel};
use std::path::Path;

/// Name of the first branch of a new database.
pub const DEFAULT_BRANCH_NAME: &str = "main";

// CONTRACT
#[derive(Clone, Debug)]
pub struct BranchManager {
    container: Container,
}

impl BranchManager {
    /// Wrap a container without checking the directory (tests, tools).
    pub fn new(container: Container) -> BranchManager {
        BranchManager { container }
    }

    /// Wrap and validate: `check_format` → `Ready` is fine; `Empty` on a writable container is
    /// initialized with branch "main" (this also repairs a create that crashed before its first
    /// directory commit); `Empty` on a read-only container → `error::directory_missing`.
    pub fn open(container: Container) -> Result<BranchManager> {
        let m = BranchManager::new(container);
        let snap = m.container.snapshot();
        match directory::check_format(&snap.directory())? {
            DirectoryStatus::Ready => {}
            DirectoryStatus::Empty => {
                if m.container.is_read_only() {
                    return Err(crate::error::directory_missing(m.container.path()));
                }
                m.initialize(DEFAULT_BRANCH_NAME)?;
            }
        }
        Ok(m)
    }

    pub fn container(&self) -> &Container {
        &self.container
    }

    pub fn snapshot(&self) -> Snapshot {
        self.container.snapshot()
    }

    /// Run `f` against the directory inside one storage write transaction and commit it.
    /// `f` gets the directory as `KvWrite` and the commit context. Errors from `f` abort (nothing
    /// becomes durable). Returns `f`'s value and the durable commit sequence.
    pub fn write<T>(&self, f: impl FnOnce(&mut dyn KvWrite, &OpCtx) -> Result<T>) -> Result<(T, CommitSeq)> {
        let mut w = self.container.begin_write()?;
        let ctx = OpCtx { commit_seq: w.commit_seq(), now_unix_ms: now_unix_ms() };
        let dir = w.directory_root();
        let (value, new_dir) = {
            let mut t = w.tree(dir);
            let value = f(&mut t, &ctx)?;
            (value, t.root())
        };
        w.set_directory_root(new_dir);
        let seq = w.commit()?;
        Ok((value, seq))
    }

    /// Initialize an empty directory with its first branch (empty data tree).
    pub fn initialize(&self, name: &str) -> Result<BranchRecord> {
        self.write(|dir, ctx| ops::initialize(dir, ctx, name, PageId::NONE, None)).map(|(r, _)| r)
    }

    pub fn list(&self, include_deleted: bool) -> Result<Vec<BranchRecord>> {
        directory::list(&self.snapshot().directory(), include_deleted)
    }

    /// Active branch by name or id.
    pub fn get(&self, branch: &BranchRef) -> Result<BranchRecord> {
        directory::resolve(&self.snapshot().directory(), branch)
    }

    /// The default branch (`Mdefault_branch`), if set and active.
    pub fn default_branch(&self) -> Result<Option<BranchRecord>> {
        let snap = self.snapshot();
        let dir = snap.directory();
        match directory::default_branch(&dir)? {
            Some(id) => directory::resolve(&dir, &BranchRef::Id(id)).map(Some),
            None => Ok(None),
        }
    }

    pub fn create(&self, name: &str, source: &CreateSource, opts: &CreateOptions) -> Result<BranchRecord> {
        self.write(|dir, ctx| ops::create(dir, ctx, name, source, opts)).map(|(r, _)| r)
    }

    pub fn rename(&self, branch: &BranchRef, new_name: &str) -> Result<BranchRecord> {
        self.write(|dir, ctx| ops::rename(dir, ctx, branch, new_name)).map(|(r, _)| r)
    }

    pub fn delete(&self, branch: &BranchRef, opts: &DeleteOptions) -> Result<BranchRecord> {
        self.write(|dir, ctx| ops::delete(dir, ctx, branch, opts)).map(|(r, _)| r)
    }

    pub fn restore(&self, id: &BranchId, new_name: Option<&str>) -> Result<BranchRecord> {
        self.write(|dir, ctx| ops::restore(dir, ctx, id, new_name)).map(|(r, _)| r)
    }

    pub fn purge_deleted(&self, filter: &PurgeFilter) -> Result<Vec<BranchId>> {
        self.write(|dir, ctx| ops::purge_deleted(dir, ctx, filter)).map(|(r, _)| r)
    }

    pub fn set_default(&self, branch: &BranchRef) -> Result<BranchRecord> {
        self.write(|dir, ctx| ops::set_default(dir, ctx, branch)).map(|(r, _)| r)
    }

    pub fn create_snapshot(&self, branch: &BranchRef, label: &str) -> Result<NamedSnapshot> {
        self.write(|dir, ctx| history::create_snapshot(dir, ctx, branch, label)).map(|(r, _)| r)
    }

    pub fn drop_snapshot(&self, branch: &BranchRef, label: &str) -> Result<NamedSnapshot> {
        self.write(|dir, ctx| history::drop_snapshot(dir, ctx, branch, label)).map(|(r, _)| r)
    }

    pub fn reset(&self, branch: &BranchRef, target: &ResetTarget, opts: &ResetOptions) -> Result<ResetReport> {
        self.write(|dir, ctx| history::reset(dir, ctx, branch, target, opts)).map(|(r, _)| r)
    }

    pub fn compare(&self, a: &BranchRef, b: &BranchRef, opts: &CompareOptions) -> Result<CompareReport> {
        compare::compare(&self.snapshot(), a, b, opts)
    }

    pub fn space(&self) -> Result<(Vec<BranchSpace>, SpaceReport)> {
        space::branch_space(&self.container)
    }

    /// Compact with the branch resolver (keeps every live branch/snapshot root, BRANCH-12).
    pub fn compact(&self, opts: &CompactOptions) -> Result<CompactReport> {
        self.container.compact(&BranchRootResolver, opts)
    }

    pub fn verify(&self, level: VerifyLevel) -> Result<DoctorReport> {
        self.container.verify(&BranchRootResolver, level)
    }

    /// Write a `.pgabranch` handle for an active branch of this container.
    pub fn write_handle(&self, branch: &BranchRef, handle_path: &Path, opts: &HandleOptions) -> Result<HandleFile> {
        let rec = self.get(branch)?;
        handle::write_handle(
            self.container.vfs().as_ref(),
            handle_path,
            self.container.path(),
            self.container.db_uuid(),
            &rec,
            now_unix_ms(),
            opts,
        )
    }

    pub fn detach(&self, branch: &BranchRef, dest: &Path, opts: &DetachOptions) -> Result<DetachReport> {
        detach::detach_branch(&self.container, branch, dest, opts)
    }

    pub fn export(&self, branch: &BranchRef, dest: &Path, opts: &DetachOptions) -> Result<DetachReport> {
        export::export_branch(&self.container, branch, dest, opts)
    }
}
