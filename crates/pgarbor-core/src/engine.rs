//! `Engine`: the one in-process owner of an open `.pga` (RFC 001 §8, ARCHITECTURE §7).
//! Lease CORE-01 (sonnet) for `create`, `open`, `close`, `branch_head`, `attach_session` and the
//! session-aware branch administration; accessors and simple delegations are implemented.
//!
//! An `Engine` is shared by all sessions as `Arc<Engine>` (`Send + Sync`). It owns the storage
//! `Container` (which holds the exclusive OS file lock), the `BranchManager`, the lock manager,
//! the session registry, the sequence cache, the id allocators and the SSI tracker.
//!
//! # Branch administration with live sessions (RFC 001 §8 "If a branch is reset/deleted,
//! existing sessions get documented safe behavior")
//! - `delete_branch` / `reset_branch` / `purge`-of-retained: sessions bound to the branch other
//!   than `caller` and not already terminating (`interrupts.is_terminating()`) → without `force`:
//!   `error::branch_in_use(name, n)` (55006); with `force`: for each such session
//!   `interrupts.request_terminate()` + `locks.wake(id)` and proceed IMMEDIATELY (no waiting:
//!   embedded sessions have no thread driving them to disconnect, and waiting is unnecessary for
//!   safety because `ops::advance_head` rejects Deleted records and stale generations, so no
//!   commit of a terminated session can land afterwards). Their next statement or lock wait fails
//!   with FATAL 57P01; an in-flight commit fails the generation guard (55000).
//! - After a successful delete/reset/restore: `sequences.forget_branch`, `alloc.forget_branch`.
//! - `create_branch`: ONE storage write, respecting the lock order (sequence cache before the
//!   container writer): `let mut seqs = sequences.lock_entries()` → `begin_write` → load the source
//!   record → `sequences.flush_branch(&mut seqs, &mut w, &mut src)` (persist visible sequence state into the source's
//!   trees; if it wrote anything, `ops::advance_head` for the source) → `ops::create(...)` on the
//!   directory (the child copies the source's possibly just-updated roots) → commit.
//!   For `CreateSource::Snapshot` no flush is needed.
//! - `rename_branch`: no session check; afterwards refresh `SessionShared.branch_name` of
//!   sessions on that branch.
//! - `compact`: `branches.compact` (storage holds the writer for the duration; open transactions
//!   keep reading their pre-compaction snapshots; commits after it apply to the new head).
//!
//! # branch_head (snapshot pairing rule)
//! `branch_head(branch, db)`: `snap = container.snapshot()`; `rec = directory::load_record(
//! &snap.directory(), &branch)` (absent → `branch_id_not_found`; Deleted → `branch_deleted`);
//! `root = rec.database_root(db)` (None → `error::database_not_in_branch`); `tree = snap.tree(root)`.
//! The three values ALWAYS come from the same snapshot.
use crate::alloc::IdAllocator;
use crate::locks::LockManager;
use crate::options::{EngineOptions, SessionRequest};
use crate::sequence::SequenceStore;
use crate::session::{SessionHandle, SessionRegistry, SessionShared};
use crate::ssi::SsiTracker;
use crate::txn::Txn;
use crate::types::{SessionId, TxnOptions};
use crate::error;
use pgarbor_branches::{
    directory, ops, BranchManager, BranchRecord, BranchRef, CreateOptions, CreateSource, DeleteOptions, OpCtx,
    PurgeFilter, ResetOptions, ResetReport, ResetTarget,
};
use pgarbor_common::{BranchId, Oid, PageId, Result, TxnId};
use pgarbor_storage::format::superblock::now_unix_ms;
#[cfg(not(target_arch = "wasm32"))]
use pgarbor_storage::OsVfs;
#[cfg(target_arch = "wasm32")]
use pgarbor_storage::MemVfs;
use pgarbor_storage::{CompactOptions, CompactReport, Container, Snapshot, TreeSnapshot, Vfs};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;

/// First transaction id handed out (PG FirstNormalTransactionId = 3; we start at 3 too so
/// `txid_current()` values look familiar). Not persisted (ARCHITECTURE §2).
pub const FIRST_TXN_ID: u64 = 3;

// CONTRACT
/// A matched (storage snapshot, branch record, database tree) triple — see module docs.
#[derive(Clone, Debug)]
pub struct BranchHead {
    pub snapshot: Snapshot,
    pub record: BranchRecord,
    pub tree: TreeSnapshot,
}

// CONTRACT
/// Options for destructive branch administration through the engine.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct BranchAdminOptions {
    /// Terminate other sessions on the branch instead of failing with 55006.
    pub force: bool,
    /// The calling session (excluded from the in-use count), if the request came over SQL.
    pub caller: Option<SessionId>,
}

// CONTRACT
#[derive(Debug)]
pub struct Engine {
    pub(crate) container: Container,
    pub(crate) branches: BranchManager,
    pub(crate) options: EngineOptions,
    pub(crate) path: PathBuf,
    pub(crate) locks: LockManager,
    pub(crate) sessions: SessionRegistry,
    pub(crate) sequences: SequenceStore,
    pub(crate) alloc: IdAllocator,
    pub(crate) ssi: SsiTracker,
    pub(crate) next_txn: AtomicU64,
    pub(crate) closed: AtomicBool,
}

// CONTRACT: compile-time proof of the sharing model.
const _: fn() = || {
    fn assert_send_sync<T: Send + Sync>() {}
    assert_send_sync::<Engine>();
    assert_send_sync::<SessionShared>();
};

impl Engine {
    /// Assemble an engine around an already opened container + manager (used by create/open and
    /// by tests).
    pub fn from_parts(container: Container, branches: BranchManager, options: EngineOptions) -> Engine {
        let path = container.path().to_path_buf();
        Engine {
            container,
            branches,
            options,
            path,
            locks: LockManager::new(),
            sessions: SessionRegistry::new(),
            sequences: SequenceStore::new(),
            alloc: IdAllocator::new(),
            ssi: SsiTracker::new(),
            next_txn: AtomicU64::new(FIRST_TXN_ID),
            closed: AtomicBool::new(false),
        }
    }

    /// Create a new `.pga`: `Container::create_with(vfs, path, &CreateOptions{page_size, db_uuid,
    /// sync, cache_pages, ..})` (vfs = `options.vfs` or `OsVfs`), then
    /// `BranchManager::new(c).initialize(&options.initial_branch)`. Errors: exists → 58P02.
    pub fn create(path: &Path, options: &EngineOptions) -> Result<Arc<Engine>> {
        let vfs = Self::vfs_of(options);
        let copts = pgarbor_storage::CreateOptions {
            page_size: options.page_size,
            db_uuid: options.db_uuid,
            sync: options.sync,
            cache_pages: options.cache_pages,
            ..Default::default()
        };
        let container = Container::create_with(vfs, path, &copts)?;
        let branches = BranchManager::new(container.clone());
        branches.initialize(&options.initial_branch)?;
        Ok(Arc::new(Engine::from_parts(container, branches, options.clone())))
    }

    fn vfs_of(options: &EngineOptions) -> Arc<dyn Vfs> {
        options.vfs.clone().unwrap_or_else(|| {
            #[cfg(not(target_arch = "wasm32"))]
            {
                Arc::new(OsVfs)
            }
            // wasm32 has no OS file system: the default VFS is memory-only.
            #[cfg(target_arch = "wasm32")]
            {
                Arc::new(MemVfs::new())
            }
        })
    }

    /// Open an existing `.pga`: `Container::open_with(vfs, path, &OpenOptions{read_only, sync,
    /// cache_pages, verify, ..})`, then `BranchManager::open(c)` (initializes an empty directory
    /// left by a crashed create when writable). Errors: missing → 58P01; locked → 55006.
    pub fn open(path: &Path, options: &EngineOptions) -> Result<Arc<Engine>> {
        let vfs = Self::vfs_of(options);
        let oopts = pgarbor_storage::OpenOptions {
            read_only: options.read_only,
            sync: options.sync,
            cache_pages: options.cache_pages,
            verify: options.verify,
            ..Default::default()
        };
        let container = Container::open_with(vfs, path, &oopts)?;
        let branches = BranchManager::open(container.clone())?;
        Ok(Arc::new(Engine::from_parts(container, branches, options.clone())))
    }

    /// Clean shutdown: refuse new sessions (`closed = true`), `sequences.flush_all(self)` (skipped
    /// for read-only engines). Does not wait for sessions; dropping the last `Arc` releases the file.
    pub fn close(&self) -> Result<()> {
        // Idempotent: only the first call flushes.
        if self.closed.swap(true, Ordering::SeqCst) {
            return Ok(());
        }
        if self.is_read_only() {
            return Ok(());
        }
        self.sequences.flush_all(self)
    }

    pub fn path(&self) -> &Path {
        &self.path
    }
    pub fn options(&self) -> &EngineOptions {
        &self.options
    }
    pub fn container(&self) -> &Container {
        &self.container
    }
    pub fn branches(&self) -> &BranchManager {
        &self.branches
    }
    pub fn locks(&self) -> &LockManager {
        &self.locks
    }
    pub fn sessions(&self) -> &SessionRegistry {
        &self.sessions
    }
    pub fn sequences(&self) -> &SequenceStore {
        &self.sequences
    }
    pub fn ids(&self) -> &IdAllocator {
        &self.alloc
    }
    pub fn ssi(&self) -> &SsiTracker {
        &self.ssi
    }
    pub fn is_read_only(&self) -> bool {
        self.options.read_only || self.container.is_read_only()
    }
    pub fn is_closed(&self) -> bool {
        self.closed.load(Ordering::SeqCst)
    }
    /// Next in-process transaction id.
    pub fn next_txn_id(&self) -> TxnId {
        TxnId(self.next_txn.fetch_add(1, Ordering::SeqCst))
    }

    /// Latest committed head of one database of a branch (module docs).
    pub fn branch_head(&self, branch: &BranchId, database: Oid) -> Result<BranchHead> {
        // ONE snapshot for the directory record and the data tree (pairing rule).
        let snapshot = self.container.snapshot();
        let dir = snapshot.directory();
        let record = match directory::load_record(&dir, branch)? {
            Some(r) => r,
            None => return Err(pgarbor_branches::error::branch_id_not_found(branch)),
        };
        if !record.is_active() {
            return Err(pgarbor_branches::error::branch_deleted(&record.name, &record.id));
        }
        let root = record
            .database_root(database)
            .ok_or_else(|| error::database_not_in_branch(database, &record.name))?;
        let tree = snapshot.tree(root)?;
        Ok(BranchHead { snapshot, record, tree })
    }

    /// Bind a new session: closed engine → FATAL 57P01 `terminating connection due to
    /// administrator command`; resolve `req.branch` (active only); v1: `req.database_oid` must have
    /// a root in the branch (`database_not_in_branch`); `sessions.register(..)`.
    pub fn attach_session(self: &Arc<Self>, req: &SessionRequest) -> Result<SessionHandle> {
        if self.is_closed() {
            return Err(error::terminated_by_administrator());
        }
        let rec = self.branches.get(&req.branch)?;
        if rec.database_root(req.database_oid).is_none() {
            return Err(error::database_not_in_branch(req.database_oid, &rec.name));
        }
        let shared = self.sessions.register(rec.id, &rec.name, req, self.options.max_sessions, now_unix_ms())?;
        Ok(SessionHandle { engine: Arc::clone(self), shared })
    }

    /// Start a transaction in `session`.
    pub fn begin(self: &Arc<Self>, session: &SessionHandle, opts: &TxnOptions) -> Result<Txn> {
        Txn::begin(Arc::clone(self), Arc::clone(&session.shared), opts)
    }

    /// Create a branch, persisting the source's sequence state first (module docs).
    pub fn create_branch(&self, name: &str, source: &CreateSource, opts: &CreateOptions) -> Result<BranchRecord> {
        let source_ref: &BranchRef = match source {
            CreateSource::Branch(b) => b,
            CreateSource::Snapshot { branch, .. } => branch,
        };
        // Lock order: sequence cache BEFORE the container writer.
        let mut seqs = self.sequences.lock_entries();
        let mut w = self.container.begin_write()?;
        let ctx = OpCtx { commit_seq: w.commit_seq(), now_unix_ms: now_unix_ms() };

        if matches!(source, CreateSource::Branch(_)) {
            let dir_root = w.directory_root();
            let mut src = {
                let t = w.tree(dir_root);
                directory::resolve(&t, source_ref)?
            };
            // Roots before the flush, to find which databases it changed.
            let mut before: Vec<(Oid, PageId)> = vec![(pgarbor_branches::POSTGRES_DATABASE_OID, src.data_root)];
            before.extend(src.extra_databases.iter().map(|d| (d.db_oid, d.root)));
            let wrote = self.sequences.flush_branch(&mut seqs, &mut w, &mut src)?;
            if wrote > 0 {
                for (oid, old_root) in before {
                    let new_root = match src.database_root(oid) {
                        Some(r) => r,
                        None => continue,
                    };
                    if new_root == old_root {
                        continue;
                    }
                    let dir_root = w.directory_root();
                    let new_dir = {
                        let mut t = w.tree(dir_root);
                        ops::advance_head(&mut t, &ctx, &src.id, src.generation, oid, new_root)?;
                        t.root()
                    };
                    w.set_directory_root(new_dir);
                }
            }
        }

        let dir_root = w.directory_root();
        let (rec, new_dir) = {
            let mut t = w.tree(dir_root);
            let rec = ops::create(&mut t, &ctx, name, source, opts)?;
            (rec, t.root())
        };
        w.set_directory_root(new_dir);
        w.commit()?;
        drop(seqs);
        Ok(rec)
    }

    /// Other sessions on `rec`'s branch that are not already terminating.
    fn live_sessions_on(&self, rec: &BranchRecord, admin: &BranchAdminOptions) -> Vec<Arc<SessionShared>> {
        self.sessions
            .on_branch(&rec.id, admin.caller)
            .into_iter()
            .filter(|s| !s.interrupts.is_terminating())
            .collect()
    }

    /// Non-forced: fail with 55006 when other live sessions exist.
    fn check_branch_idle(&self, rec: &BranchRecord, admin: &BranchAdminOptions) -> Result<Vec<Arc<SessionShared>>> {
        let others = self.live_sessions_on(rec, admin);
        if !others.is_empty() && !admin.force {
            return Err(error::branch_in_use(&rec.name, others.len()));
        }
        Ok(others)
    }

    /// Forced: terminate the sessions and wake their lock waits. Nothing waits for them.
    fn terminate_sessions(&self, sessions: &[Arc<SessionShared>]) {
        for s in sessions {
            s.interrupts.request_terminate();
            // A failed wake only delays the waiter noticing the flag until its next poll.
            let _ = self.locks.wake(s.id);
        }
    }

    fn forget_branch_caches(&self, id: &BranchId) {
        self.sequences.forget_branch(id);
        self.alloc.forget_branch(id);
    }

    /// Rename (module docs).
    pub fn rename_branch(&self, branch: &BranchRef, new_name: &str) -> Result<BranchRecord> {
        let rec = self.branches.rename(branch, new_name)?;
        for s in self.sessions.on_branch(&rec.id, None) {
            *s.branch_name.lock().unwrap_or_else(|e| e.into_inner()) = rec.name.clone();
        }
        Ok(rec)
    }

    /// Delete with session checks (module docs).
    pub fn delete_branch(&self, branch: &BranchRef, opts: &DeleteOptions, admin: &BranchAdminOptions) -> Result<BranchRecord> {
        let rec = self.branches.get(branch)?;
        let others = self.check_branch_idle(&rec, admin)?;
        // Terminate only after the delete succeeded, so a refused delete (e.g. the default
        // branch) does not kill anyone; commits of those sessions fail the generation guard.
        let tomb = self.branches.delete(&BranchRef::Id(rec.id), opts)?;
        self.terminate_sessions(&others);
        self.forget_branch_caches(&rec.id);
        Ok(tomb)
    }

    /// Reset with session checks (module docs).
    pub fn reset_branch(
        &self,
        branch: &BranchRef,
        target: &ResetTarget,
        opts: &ResetOptions,
        admin: &BranchAdminOptions,
    ) -> Result<ResetReport> {
        let rec = self.branches.get(branch)?;
        let others = self.check_branch_idle(&rec, admin)?;
        let report = self.branches.reset(&BranchRef::Id(rec.id), target, opts)?;
        self.terminate_sessions(&others);
        self.forget_branch_caches(&rec.id);
        Ok(report)
    }

    /// Restore a retained deleted branch, then forget caches of that id.
    pub fn restore_branch(&self, id: &BranchId, new_name: Option<&str>) -> Result<BranchRecord> {
        let rec = self.branches.restore(id, new_name)?;
        self.forget_branch_caches(id);
        Ok(rec)
    }

    /// Purge tombstones (no session can be bound to a deleted branch, so no check is needed).
    pub fn purge_deleted(&self, filter: &PurgeFilter) -> Result<Vec<BranchId>> {
        self.branches.purge_deleted(filter)
    }

    /// Compact the file (module docs).
    pub fn compact(&self, opts: &CompactOptions) -> Result<CompactReport> {
        self.branches.compact(opts)
    }

    /// `pg_cancel_backend(pid)`: request cancel + wake; false when no such session.
    pub fn cancel_backend(&self, pid: SessionId) -> Result<bool> {
        match self.sessions.get(pid) {
            Some(s) => {
                s.interrupts.request_cancel();
                let _ = self.locks.wake(pid);
                Ok(true)
            }
            None => Ok(false),
        }
    }

    /// `pg_terminate_backend(pid)`: request terminate + wake; false when no such session.
    pub fn terminate_backend(&self, pid: SessionId) -> Result<bool> {
        match self.sessions.get(pid) {
            Some(s) => {
                s.interrupts.request_terminate();
                let _ = self.locks.wake(pid);
                Ok(true)
            }
            None => Ok(false),
        }
    }

    /// Wire CancelRequest (pid + secret): `sessions.cancel_with_key`, then `locks.wake` on success.
    pub fn cancel_request(&self, pid: i32, secret: &[u8]) -> Result<bool> {
        let ok = self.sessions.cancel_with_key(pid, secret)?;
        if ok {
            let _ = self.locks.wake(SessionId(pid));
        }
        Ok(ok)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pgarbor_storage::{MemVfs, SyncMode};

    fn opts() -> EngineOptions {
        EngineOptions { vfs: Some(Arc::new(MemVfs::new())), sync: SyncMode::Off, ..Default::default() }
    }

    fn engine() -> Arc<Engine> {
        Engine::create(Path::new("/t/test.pga"), &opts()).unwrap()
    }

    const DB: Oid = pgarbor_branches::POSTGRES_DATABASE_OID;

    #[test]
    fn create_then_open_same_vfs() {
        let o = opts();
        let p = Path::new("/t/a.pga");
        let e = Engine::create(p, &o).unwrap();
        e.create_branch("feature", &CreateSource::branch("main"), &CreateOptions::default()).unwrap();
        e.close().unwrap();
        drop(e);
        let e2 = Engine::open(p, &o).unwrap();
        assert!(e2.branches().get(&BranchRef::name("main")).is_ok());
        assert!(e2.branches().get(&BranchRef::name("feature")).is_ok());
    }

    #[test]
    fn attach_unknown_branch() {
        let e = engine();
        let err = e.attach_session(&SessionRequest::embedded("nope")).unwrap_err();
        assert_eq!(err.sqlstate.code(), "42704");
    }

    #[test]
    fn attach_closed_engine() {
        let e = engine();
        e.close().unwrap();
        let err = e.attach_session(&SessionRequest::embedded("main")).unwrap_err();
        assert_eq!(err.sqlstate.code(), "57P01");
    }

    #[test]
    fn delete_with_other_session_is_55006() {
        let e = engine();
        e.create_branch("b", &CreateSource::branch("main"), &CreateOptions::default()).unwrap();
        let _s = e.attach_session(&SessionRequest::embedded("b")).unwrap();
        let err = e
            .delete_branch(&BranchRef::name("b"), &DeleteOptions::default(), &BranchAdminOptions::default())
            .unwrap_err();
        assert_eq!(err.sqlstate.code(), "55006");
        assert_eq!(err.detail.as_deref(), Some("There is 1 other session using the branch."));
    }

    #[test]
    fn caller_is_excluded_from_count() {
        let e = engine();
        e.create_branch("b", &CreateSource::branch("main"), &CreateOptions::default()).unwrap();
        let s = e.attach_session(&SessionRequest::embedded("b")).unwrap();
        let admin = BranchAdminOptions { force: false, caller: Some(s.id()) };
        e.delete_branch(&BranchRef::name("b"), &DeleteOptions::default(), &admin).unwrap();
    }

    #[test]
    fn force_delete_terminates_session() {
        let e = engine();
        e.create_branch("b", &CreateSource::branch("main"), &CreateOptions::default()).unwrap();
        let s = e.attach_session(&SessionRequest::embedded("b")).unwrap();
        let admin = BranchAdminOptions { force: true, caller: None };
        e.delete_branch(&BranchRef::name("b"), &DeleteOptions::default(), &admin).unwrap();
        let err = s.interrupts().check().unwrap_err();
        assert_eq!(err.sqlstate.code(), "57P01");
        assert_eq!(err.severity, pgarbor_common::Severity::Fatal);
    }

    #[test]
    fn terminating_sessions_are_ignored() {
        let e = engine();
        e.create_branch("c", &CreateSource::branch("main"), &CreateOptions::default()).unwrap();
        let s = e.attach_session(&SessionRequest::embedded("c")).unwrap();
        s.interrupts().request_terminate();
        e.delete_branch(&BranchRef::name("c"), &DeleteOptions::default(), &BranchAdminOptions::default()).unwrap();
    }

    #[test]
    fn fresh_branch_head_equals_parent_root() {
        let e = engine();
        let main = e.branches().get(&BranchRef::name("main")).unwrap();
        let child = e.create_branch("f", &CreateSource::branch("main"), &CreateOptions::default()).unwrap();
        let h_main = e.branch_head(&main.id, DB).unwrap();
        let h_child = e.branch_head(&child.id, DB).unwrap();
        assert_eq!(h_main.tree.root(), h_child.tree.root());
        assert_eq!(h_child.record.id, child.id);
        assert_eq!(h_child.snapshot.commit_seq(), h_child.tree.snapshot().commit_seq());
    }

    #[test]
    fn branch_head_errors() {
        let e = engine();
        let err = e.branch_head(&BranchId([9; 16]), DB).unwrap_err();
        assert_eq!(err.sqlstate.code(), "42704");
        let main = e.branches().get(&BranchRef::name("main")).unwrap();
        let err = e.branch_head(&main.id, 12345).unwrap_err();
        assert_eq!(err.sqlstate.code(), "3D000");
    }

    #[test]
    fn rename_refreshes_session_branch_name() {
        let e = engine();
        e.create_branch("b", &CreateSource::branch("main"), &CreateOptions::default()).unwrap();
        let s = e.attach_session(&SessionRequest::embedded("b")).unwrap();
        e.rename_branch(&BranchRef::name("b"), "b2").unwrap();
        assert_eq!(s.shared().branch_name(), "b2");
    }

    #[test]
    fn cancel_unknown_backend_is_false() {
        let e = engine();
        assert!(!e.cancel_backend(SessionId(424242)).unwrap());
        assert!(!e.terminate_backend(SessionId(424242)).unwrap());
        assert!(!e.cancel_request(424242, &[1, 2, 3, 4]).unwrap());
    }

    #[test]
    fn cancel_and_terminate_backend() {
        let e = engine();
        let s = e.attach_session(&SessionRequest::embedded("main")).unwrap();
        assert!(e.cancel_backend(s.id()).unwrap());
        assert_eq!(s.interrupts().check().unwrap_err().sqlstate.code(), "57014");
        assert!(e.terminate_backend(s.id()).unwrap());
        assert_eq!(s.interrupts().check().unwrap_err().sqlstate.code(), "57P01");
    }
}
