//! [`Session`]: one client connection bound to ONE branch and ONE logical database (RFC 001 §8,
//! ARCHITECTURE §11). The struct and its accessors are fully implemented and frozen (v1); behavior
//! lives in `impl Session` blocks of the sibling modules:
//!
//! | module | `impl Session` content |
//! |---|---|
//! | `simple_query` | `simple_query`, `simple_query_raw`, `query` (simple Query message semantics) |
//! | `extended` | `parse`, `bind`, `describe_statement`, `describe_portal`, `execute`, `close_statement`, `close_portal`, `sync` (extended protocol primitives) |
//! | `prepared` | `prepare`, `execute_prepared`, SQL `PREPARE`/`EXECUTE`/`DEALLOCATE`, plan cache |
//! | `portal` | portals, row limits and suspension, row encoding (`encode_row`) |
//! | `run_stmt` | parse -> bind -> plan -> execute of one statement, DDL retry |
//! | `ctx_build` | `BindContext` / `PlanContext` / `ExecContext` assembly, `ColumnInfo` building |
//! | `txn_block` | transaction block state machine, BEGIN/COMMIT/ROLLBACK/SAVEPOINT/SET TRANSACTION |
//! | `session_utility` | SET / RESET / SHOW / DISCARD / LISTEN / NOTIFY / SET ROLE |
//! | `bootstrap` | `Session::connect` internals: identity resolution, startup GUCs |
//! | `engine_info` | `EngineServices` implementation (activity, locks, prepared statements) |
//!
//! ## Threading
//! A `Session` is single-threaded: it is `!Send` (open portals own operator trees with `Rc`). Create
//! and use it on one thread (thread-per-connection in the wire server). Other threads interact
//! only through [`CancelToken`] and `Database::cancel_request`.
//!
//! ## Lifecycle
//! `Database::session` / `Database::connect` -> `Session` -> statements -> drop (or [`Session::close`]):
//! rolls back any open transaction, releases session-level locks, unregisters from the engine.
use std::cell::{Cell, RefCell};

use pgarbor_catalog::CatalogEnv;
use pgarbor_common::{Oid, PgError, Result};
use pgarbor_core::{SessionHandle, Txn};

use crate::database::Database;
use crate::guc::GucTable;
use crate::notify::ListenState;
use crate::portal::PortalStore;
use crate::prepared::PreparedStore;
use crate::txn_block::BlockState;
use crate::types::{CancelToken, Notification, SessionOptions, TxnStatus};

/// A client session (module docs).
pub struct Session {
    pub(crate) db: Database,
    pub(crate) handle: SessionHandle,
    pub(crate) opts: SessionOptions,
    /// Current role (changes with `SET ROLE`).
    pub(crate) user_oid: Oid,
    /// Authenticated role (changes with `SET SESSION AUTHORIZATION`).
    pub(crate) session_user_oid: Oid,
    pub(crate) database_oid: Oid,
    /// The core transaction, present exactly when `block != Idle`.
    pub(crate) txn: Option<Txn>,
    pub(crate) block: BlockState,
    pub(crate) guc: GucTable,
    /// Naming context of the last statement start (snapshot + resolved search_path); reused for
    /// statements that need no catalog (transaction control in a failed block).
    pub(crate) env: Option<CatalogEnv>,
    pub(crate) prepared: PreparedStore,
    pub(crate) portals: PortalStore,
    /// Session PRNG state for `random()` (seeded from OS entropy at startup; `setseed` rewrites it).
    pub(crate) rng: Cell<u64>,
    /// Notices raised outside statement execution (parse, transaction control) awaiting attachment
    /// to the next output.
    pub(crate) pending_notices: Vec<PgError>,
    pub(crate) listen: RefCell<ListenState>,
    /// Extended protocol: an error happened and messages must be skipped until Sync (informational;
    /// the wire layer implements the skipping).
    pub(crate) error_since_sync: bool,
    /// Extended protocol: statements after the first Execute run in an implicit block (PG pipelining).
    pub(crate) pipelining: bool,
    pub(crate) closed: bool,
    /// COPY endpoint installed by the wire layer / embedder for `COPY ... STDIN/STDOUT` statements
    /// (see [`Session::set_copy_channel`]); `None` = `pgarbor_exec::execute::NoCopyChannel` (0A000 on use).
    pub(crate) copy_channel: Option<Box<dyn pgarbor_exec::copy::CopyChannel>>,
}

impl Session {
    /// Backend process id (`pg_backend_pid()`, BackendKeyData).
    pub fn pid(&self) -> i32 {
        self.handle.id().0
    }

    /// BackendKeyData `(pid, secret)`.
    pub fn backend_key(&self) -> (i32, Vec<u8>) {
        (self.handle.id().0, self.handle.shared().cancel_key.clone())
    }

    /// A cancel handle usable from other threads.
    pub fn cancel_token(&self) -> CancelToken {
        CancelToken { shared: self.handle.shared().clone() }
    }

    /// Status byte for ReadyForQuery.
    pub fn txn_status(&self) -> TxnStatus {
        match self.block {
            BlockState::Idle | BlockState::Started | BlockState::ImplicitBlock => TxnStatus::Idle,
            BlockState::InBlock => TxnStatus::InTransaction,
            BlockState::Failed => TxnStatus::Failed,
        }
    }

    /// The `Database` this session belongs to.
    pub fn database(&self) -> &Database {
        &self.db
    }

    /// Name of the branch this session is bound to.
    pub fn branch_name(&self) -> String {
        self.handle.shared().branch_name()
    }

    /// Logical database name (v1: `postgres`).
    pub fn database_name(&self) -> &str {
        &self.opts.database
    }

    /// Session user name as given at connect time.
    pub fn user_name(&self) -> &str {
        &self.opts.user
    }

    /// OID of the current role.
    pub fn current_user_oid(&self) -> Oid {
        self.user_oid
    }

    /// Drain notices that were raised outside statement output (rare; statement notices travel in
    /// `StatementOutput::notices`).
    pub fn take_notices(&mut self) -> Vec<PgError> {
        std::mem::take(&mut self.pending_notices)
    }

    /// Drain asynchronous notifications queued for this session (LISTEN/NOTIFY). The wire layer calls
    /// this when idle and after each ReadyForQuery.
    pub fn take_notifications(&mut self) -> Vec<Notification> {
        self.db.inner.notify.take(self.pid())
    }

    /// The ParameterStatus set to send right after authentication (module `guc`, `STARTUP_REPORT_ORDER`).
    pub fn startup_param_status(&self) -> Result<Vec<(String, String)>> {
        self.guc.startup_report()
    }

    /// Current value of a GUC in SHOW form (embedded convenience; `SHOW` statements use the same path).
    pub fn setting(&self, name: &str) -> Result<String> {
        self.guc.show(name)
    }

    /// Install the channel `COPY FROM STDIN` reads from / `COPY TO STDOUT` writes to for the next
    /// statements (the wire layer wraps CopyData messages; embedders may use
    /// `pgarbor_exec::copy::MemChannel`). `None` removes it.
    pub fn set_copy_channel(&mut self, chan: Option<Box<dyn pgarbor_exec::copy::CopyChannel>>) {
        self.copy_channel = chan;
    }

    /// True once `close` ran.
    pub fn is_closed(&self) -> bool {
        self.closed
    }

    /// Clean shutdown: drop portals and prepared statements, roll back any open transaction,
    /// unregister from the notification hub. The core `SessionHandle` (dropped with the session)
    /// releases session-level locks and unregisters the backend.
    pub fn close(mut self) {
        self.shutdown();
    }

    pub(crate) fn shutdown(&mut self) {
        if self.closed {
            return;
        }
        self.closed = true;
        self.portals.clear();
        if let Some(t) = self.txn.take() {
            let _ = t.rollback();
        }
        self.block = BlockState::Idle;
        self.db.inner.notify.unregister(self.pid());
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        self.shutdown();
    }
}
