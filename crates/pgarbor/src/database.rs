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

use pgarbor_branches::{BranchRef, POSTGRES_DATABASE_OID};
use pgarbor_catalog::persist::is_bootstrapped;
use pgarbor_catalog::CatalogCache;
use pgarbor_common::{PgError, Result};
use pgarbor_core::Engine;

use crate::bootstrap;
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
        let now_us = pgarbor_common::time::SystemTime::now()
            .duration_since(pgarbor_common::time::UNIX_EPOCH)
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

    /// Create a new `.pga` file and bootstrap the catalog of the initial branch (module docs).
    /// Existing file: 58P02 (from the storage layer).
    pub fn create(path: impl AsRef<Path>, opts: OpenOptions) -> Result<Database> {
        let path: PathBuf = path.as_ref().to_path_buf();
        let engine = Engine::create(&path, &opts.to_engine_options())?;
        let db = Database::from_engine(engine, opts, path);
        let initial = db.inner.opts.initial_branch.clone();
        bootstrap::bootstrap_branch(&db, &initial)?;
        Ok(db)
    }

    /// Open an existing `.pga` file (module docs). Missing file: 58P01. If the initial branch's
    /// catalog is not bootstrapped (crash between create and bootstrap), bootstrap it now unless
    /// the database is read-only.
    pub fn open(path: impl AsRef<Path>, opts: OpenOptions) -> Result<Database> {
        let path: PathBuf = path.as_ref().to_path_buf();
        let engine = Engine::open(&path, &opts.to_engine_options())?;
        let db = Database::from_engine(engine, opts, path);
        if !db.inner.opts.read_only {
            bootstrap_initial_branch_if_needed(&db)?;
        }
        Ok(db)
    }

    /// Open if the file exists, else create it (module docs). Existence is checked through
    /// `opts.vfs` when set, otherwise through the OS file system.
    pub fn open_or_create(path: impl AsRef<Path>, opts: OpenOptions) -> Result<Database> {
        let p: &Path = path.as_ref();
        let exists = match &opts.vfs {
            Some(vfs) => vfs.exists(p).map_err(|e| PgError::io("open_or_create", &e))?,
            None => match std::fs::metadata(p) {
                Ok(_) => true,
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => false,
                Err(e) => return Err(PgError::io("open_or_create", &e)),
            },
        };
        if exists {
            Database::open(p, opts)
        } else {
            Database::create(p, opts)
        }
    }

    /// A throw-away database: `create("memory.pga")` on a fresh `MemVfs` with `SyncMode::Off`.
    pub fn in_memory() -> Result<Database> {
        Database::create("memory.pga", OpenOptions::in_memory())
    }

    /// New session bound to `branch` (module docs).
    pub fn session(&self, branch: &str, opts: SessionOptions) -> Result<Session> {
        bootstrap::connect_session(self, branch, opts)
    }

    /// Like [`Database::session`] with the default branch when `branch` is `None`. The default
    /// branch is the one set with `set_default_branch`; without one, the initial branch name of the
    /// open options is used.
    pub fn connect(&self, branch: Option<&str>, opts: SessionOptions) -> Result<Session> {
        match branch {
            Some(name) => self.session(name, opts),
            None => {
                let name = match self.inner.engine.branches().default_branch()? {
                    Some(rec) => rec.name,
                    None => self.inner.opts.initial_branch.clone(),
                };
                self.session(&name, opts)
            }
        }
    }

    /// Credentials of `user` as stored in the catalog of `branch` (module docs).
    pub fn role_auth(&self, branch: &str, user: &str) -> Result<Option<RoleAuth>> {
        bootstrap::lookup_role_auth(self, branch, user)
    }

    /// Wire CancelRequest: `Engine::cancel_request(pid, secret)`; false for an unknown pid or a
    /// wrong secret.
    pub fn cancel_request(&self, pid: i32, secret: &[u8]) -> Result<bool> {
        self.inner.engine.cancel_request(pid, secret)
    }

    /// Clean shutdown (module docs). Idempotent in the engine.
    pub fn close(&self) -> Result<()> {
        self.inner.engine.close()
    }
}

/// Bootstrap the catalog of the initial branch when a crash left it unbootstrapped (module docs).
///
/// If the initial branch has no record (for example the file was created with another initial
/// branch name), there is nothing to bootstrap here; the first session on a missing branch reports
/// `branch "x" does not exist` through the normal path.
fn bootstrap_initial_branch_if_needed(db: &Database) -> Result<()> {
    let name = db.inner.opts.initial_branch.clone();
    let rec = match db.inner.engine.branches().get(&BranchRef::name(name.as_str())) {
        Ok(rec) => rec,
        // Only "branch does not exist" (undefined_object, branches::error::branch_not_found) means
        // there is nothing to bootstrap; any other failure (I/O, corruption) must surface.
        Err(e) if e.sqlstate.code() == "42704" => return Ok(()),
        Err(e) => return Err(e),
    };
    let head = db.inner.engine.branch_head(&rec.id, POSTGRES_DATABASE_OID)?;
    if is_bootstrapped(&head.tree)? {
        return Ok(());
    }
    bootstrap::bootstrap_branch(db, &name)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{MemVfs, SyncMode, Vfs};

    /// A database built straight on the engine, without the catalog bootstrap (which lives in
    /// API-05), so cancel and close paths can run on their own.
    fn raw_db() -> Database {
        let opts = OpenOptions { vfs: Some(Arc::new(MemVfs::new())), sync: SyncMode::Off, ..OpenOptions::default() };
        let path = PathBuf::from("/t/raw.pga");
        let engine = Engine::create(&path, &opts.to_engine_options()).unwrap();
        Database::from_engine(engine, opts, path)
    }

    #[test]
    fn open_missing_file_is_58p01() {
        let opts = OpenOptions { vfs: Some(Arc::new(MemVfs::new())), sync: SyncMode::Off, ..OpenOptions::default() };
        let err = Database::open("/t/missing.pga", opts).unwrap_err();
        assert_eq!(err.sqlstate.code(), "58P01");
    }

    #[test]
    fn cancel_with_wrong_secret_returns_false() {
        let db = raw_db();
        assert!(!db.cancel_request(424242, &[1, 2, 3, 4]).unwrap());
    }

    #[test]
    fn close_is_idempotent() {
        let db = raw_db();
        db.close().unwrap();
        db.close().unwrap();
        assert!(db.engine().is_closed());
    }

    #[test]
    #[ignore = "needs API-05"]
    fn in_memory_session_main() {
        let db = Database::in_memory().unwrap();
        let _s = db.session("main", SessionOptions::default()).unwrap();
    }

    #[test]
    #[ignore = "needs API-05"]
    fn create_over_existing_file_is_58p02() {
        let vfs: Arc<dyn Vfs> = Arc::new(MemVfs::new());
        let opts = OpenOptions { vfs: Some(vfs.clone()), sync: SyncMode::Off, ..OpenOptions::default() };
        Database::create("/t/dup.pga", opts.clone()).unwrap();
        let err = Database::create("/t/dup.pga", opts).unwrap_err();
        assert_eq!(err.sqlstate.code(), "58P02");
    }

    #[test]
    #[ignore = "needs API-05"]
    fn open_after_create_and_close_sees_catalog() {
        let vfs: Arc<dyn Vfs> = Arc::new(MemVfs::new());
        let opts = OpenOptions { vfs: Some(vfs.clone()), sync: SyncMode::Off, ..OpenOptions::default() };
        let db = Database::create("/t/rt.pga", opts.clone()).unwrap();
        db.close().unwrap();
        drop(db);
        let db2 = Database::open("/t/rt.pga", opts).unwrap();
        let mut s = db2.session("main", SessionOptions::default()).unwrap();
        s.simple_query("SELECT 1").unwrap();
    }
}
