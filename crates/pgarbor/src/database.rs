//! [`Database`]: an open `.pga` container (ARCHITECTURE §11, lease API-04). Cheap to clone
//! (`Arc` inside), `Send + Sync`; sessions are created from it.
//!
//! ```ignore
//! let db = pgarbor::Database::open("dev.pga", OpenOptions::default())?;      // or ::create / ::in_memory
//! let mut s = db.session("main", SessionOptions::default())?;
//! s.simple_query("CREATE TABLE t(id int primary key); INSERT INTO t VALUES (1)")?;
//! db.create_branch("feature", "main")?;
//! ```
//! ## Operations
//! * [`Database::create`]: `Engine::create(path, &opts.to_engine_options())` then bootstrap the PG catalog
//!   of the initial branch (`bootstrap::bootstrap_branch`). Errors: file exists 58P02.
//! * [`Database::open`]: `Engine::open`; if the initial branch's catalog is not bootstrapped (a crash
//!   between create and bootstrap) bootstrap it now unless read-only (then the first session fails with
//!   XX001 `database is not initialized`). Errors: missing 58P01, locked by another process 55006.
//! * [`Database::open_or_create`]: open if the file exists, else create.
//! * [`Database::in_memory`]: `create("memory.pga")` on a fresh `MemVfs` with `SyncMode::Off`.
//! * [`Database::session`]/[`Database::connect`]: `bootstrap::connect_session` (identity resolution, GUC
//!   startup, registration). Branch must exist and be active (3D000-style error from branches:
//!   `branch "x" does not exist`).
//! * [`Database::role_auth`]: what a wire server needs to authenticate (`RoleAuth`); `Ok(None)` for an
//!   unknown role (the server then fails authentication like PG: 28P01 for password methods, 28000
//!   `role "x" does not exist` for trust).
//! * [`Database::cancel_request`]: `Engine::cancel_request(pid, secret)`.
//! * [`Database::close`]: `Engine::close` (flush sequence state); sessions fail afterwards with 57P01.
//! Branch administration is in `branches.rs` (same type, more `impl` blocks).
use std::path::{Path, PathBuf};
use std::sync::Arc;

use pgarbor_catalog::CatalogCache;
use pgarbor_common::{PgError, Result};
use pgarbor_core::Engine;

use crate::notify::NotifyHub;
use crate::session::Session;
use crate::types::{OpenOptions, RoleAuth, SessionOptions};

/// Shared state of an open database.
pub(crate) struct DbInner {
    pub(crate) engine: Arc<Engine>,
    pub(crate) catalog_cache: CatalogCache,
    pub(crate) notify: NotifyHub,
    pub(crate) opts: OpenOptions,
    pub(crate) path: PathBuf,
    /// Open time as timestamptz us since 2000-01-01 (`pg_postmaster_start_time()`).
    pub(crate) start_time_us: i64,
}

/// An open database (module docs).
#[derive(Clone)]
pub struct Database {
    pub(crate) inner: Arc<DbInner>,
}

impl std::fmt::Debug for Database {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Database").field("path", &self.inner.path).finish()
    }
}

impl Database {
    /// Wrap an engine (shared by the constructors and by tests).
    pub(crate) fn from_engine(engine: Arc<Engine>, opts: OpenOptions, path: PathBuf) -> Database {
        const PG_EPOCH_OFFSET_US: i64 = 946_684_800_000_000;
        let now_us = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_micros() as i64)
            .unwrap_or(0);
        Database {
            inner: Arc::new(DbInner {
                engine,
                catalog_cache: CatalogCache::default(),
                notify: NotifyHub::new(),
                opts,
                path,
                start_time_us: now_us - PG_EPOCH_OFFSET_US,
            }),
        }
    }

    /// The core engine (advanced use: wire server, tooling).
    pub fn engine(&self) -> &Arc<Engine> {
        &self.inner.engine
    }

    /// File path the database was opened with.
    pub fn path(&self) -> &Path {
        &self.inner.path
    }

    /// Options the database was opened with.
    pub fn options(&self) -> &OpenOptions {
        &self.inner.opts
    }

    /// The notification hub (the wire layer registers wake-up callbacks per session pid).
    pub fn notify_hub(&self) -> &NotifyHub {
        &self.inner.notify
    }

    #[allow(unused_variables)]
    pub fn create(path: impl AsRef<Path>, opts: OpenOptions) -> Result<Database> {
        Err(PgError::unimplemented("API-04", "create database (Database::create)"))
    }

    #[allow(unused_variables)]
    pub fn open(path: impl AsRef<Path>, opts: OpenOptions) -> Result<Database> {
        Err(PgError::unimplemented("API-04", "open database (Database::open)"))
    }

    #[allow(unused_variables)]
    pub fn open_or_create(path: impl AsRef<Path>, opts: OpenOptions) -> Result<Database> {
        Err(PgError::unimplemented("API-04", "open or create database (Database::open_or_create)"))
    }

    pub fn in_memory() -> Result<Database> {
        Err(PgError::unimplemented("API-04", "in-memory database (Database::in_memory)"))
    }

    /// New session bound to `branch` (module docs).
    #[allow(unused_variables)]
    pub fn session(&self, branch: &str, opts: SessionOptions) -> Result<Session> {
        Err(PgError::unimplemented("API-04", "session creation (Database::session)"))
    }

    /// Like [`Database::session`] with the default branch when `branch` is `None`.
    #[allow(unused_variables)]
    pub fn connect(&self, branch: Option<&str>, opts: SessionOptions) -> Result<Session> {
        Err(PgError::unimplemented("API-04", "connect (Database::connect)"))
    }

    /// Credentials of `user` as stored in the catalog of `branch`.
    #[allow(unused_variables)]
    pub fn role_auth(&self, branch: &str, user: &str) -> Result<Option<RoleAuth>> {
        Err(PgError::unimplemented("API-04", "role lookup (Database::role_auth)"))
    }

    /// Wire CancelRequest.
    #[allow(unused_variables)]
    pub fn cancel_request(&self, pid: i32, secret: &[u8]) -> Result<bool> {
        Err(PgError::unimplemented("API-04", "cancel request (Database::cancel_request)"))
    }

    /// Clean shutdown (module docs).
    pub fn close(&self) -> Result<()> {
        Err(PgError::unimplemented("API-04", "close database (Database::close)"))
    }
}
