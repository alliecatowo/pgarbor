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
use pgarbor_branches::{
    BranchManager, BranchRecord, BranchRef, CreateOptions, CreateSource, DeleteOptions, PurgeFilter, ResetOptions,
    ResetReport, ResetTarget,
};
use pgarbor_common::{BranchId, Oid, PgError, Result, TxnId};
use pgarbor_storage::{CompactOptions, CompactReport, Container, Snapshot, TreeSnapshot};
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
    #[allow(unused_variables)]
    pub fn create(path: &Path, options: &EngineOptions) -> Result<Arc<Engine>> {
        Err(PgError::unimplemented("CORE-01", "engine create"))
    }

    /// Open an existing `.pga`: `Container::open_with(vfs, path, &OpenOptions{read_only, sync,
    /// cache_pages, verify, ..})`, then `BranchManager::open(c)` (initializes an empty directory
    /// left by a crashed create when writable). Errors: missing → 58P01; locked → 55006.
    #[allow(unused_variables)]
    pub fn open(path: &Path, options: &EngineOptions) -> Result<Arc<Engine>> {
        Err(PgError::unimplemented("CORE-01", "engine open"))
    }

    /// Clean shutdown: refuse new sessions (`closed = true`), `sequences.flush_all(self)` (skipped
    /// for read-only engines). Does not wait for sessions; dropping the last `Arc` releases the file.
    #[allow(unused_variables)]
    pub fn close(&self) -> Result<()> {
        Err(PgError::unimplemented("CORE-01", "engine close"))
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
    #[allow(unused_variables)]
    pub fn branch_head(&self, branch: &BranchId, database: Oid) -> Result<BranchHead> {
        Err(PgError::unimplemented("CORE-01", "branch head lookup"))
    }

    /// Bind a new session: closed engine → FATAL 57P01 `terminating connection due to
    /// administrator command`; resolve `req.branch` (active only); v1: `req.database_oid` must have
    /// a root in the branch (`database_not_in_branch`); `sessions.register(..)`.
    #[allow(unused_variables)]
    pub fn attach_session(self: &Arc<Self>, req: &SessionRequest) -> Result<SessionHandle> {
        Err(PgError::unimplemented("CORE-01", "session attach"))
    }

    /// Start a transaction in `session`.
    pub fn begin(self: &Arc<Self>, session: &SessionHandle, opts: &TxnOptions) -> Result<Txn> {
        Txn::begin(Arc::clone(self), Arc::clone(&session.shared), opts)
    }

    /// Create a branch, persisting the source's sequence state first (module docs).
    #[allow(unused_variables)]
    pub fn create_branch(&self, name: &str, source: &CreateSource, opts: &CreateOptions) -> Result<BranchRecord> {
        Err(PgError::unimplemented("CORE-01", "engine branch create"))
    }

    /// Rename (module docs).
    #[allow(unused_variables)]
    pub fn rename_branch(&self, branch: &BranchRef, new_name: &str) -> Result<BranchRecord> {
        Err(PgError::unimplemented("CORE-01", "engine branch rename"))
    }

    /// Delete with session checks (module docs).
    #[allow(unused_variables)]
    pub fn delete_branch(&self, branch: &BranchRef, opts: &DeleteOptions, admin: &BranchAdminOptions) -> Result<BranchRecord> {
        Err(PgError::unimplemented("CORE-01", "engine branch delete"))
    }

    /// Reset with session checks (module docs).
    #[allow(unused_variables)]
    pub fn reset_branch(
        &self,
        branch: &BranchRef,
        target: &ResetTarget,
        opts: &ResetOptions,
        admin: &BranchAdminOptions,
    ) -> Result<ResetReport> {
        Err(PgError::unimplemented("CORE-01", "engine branch reset"))
    }

    /// Restore a retained deleted branch, then forget caches of that id.
    #[allow(unused_variables)]
    pub fn restore_branch(&self, id: &BranchId, new_name: Option<&str>) -> Result<BranchRecord> {
        Err(PgError::unimplemented("CORE-01", "engine branch restore"))
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
    #[allow(unused_variables)]
    pub fn cancel_backend(&self, pid: SessionId) -> Result<bool> {
        Err(PgError::unimplemented("CORE-01", "pg_cancel_backend"))
    }

    /// `pg_terminate_backend(pid)`: request terminate + wake; false when no such session.
    #[allow(unused_variables)]
    pub fn terminate_backend(&self, pid: SessionId) -> Result<bool> {
        Err(PgError::unimplemented("CORE-01", "pg_terminate_backend"))
    }

    /// Wire CancelRequest (pid + secret): `sessions.cancel_with_key`, then `locks.wake` on success.
    #[allow(unused_variables)]
    pub fn cancel_request(&self, pid: i32, secret: &[u8]) -> Result<bool> {
        Err(PgError::unimplemented("CORE-01", "wire cancel request"))
    }
}
