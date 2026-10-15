//! Catalog bootstrap and session connection internals (lease API-05, north-star).
//!
//! * [`bootstrap_branch`]: make `branch` a PostgreSQL-compatible database: attach a throw-away
//!   internal session (`SessionRequest::embedded(branch)`), `begin` an explicit transaction, `begin_statement`,
//!   `pgarbor_catalog::persist::bootstrap(&mut txn.kv("BOOTSTRAP"))` (idempotent: returns false when
//!   already bootstrapped), commit. New branches inherit the catalog by copy-on-write, so only the
//!   initial branch needs this.
//! * [`connect_session`]: everything between "startup packet" and "ReadyForQuery":
//!   1. resolve the branch (`engine.branches().get(&BranchRef::name(..))`; missing/deleted:
//!      3D000-class error with PG-like wording `database "branch" does not exist`? no: use
//!      the branches error unchanged `branch "x" does not exist`);
//!   2. read the catalog of the branch head WITHOUT a session: `engine.branch_head(&id, 5)?` then
//!      `db.catalog_cache.snapshot(&head.tree)`; if the branch is not bootstrapped and the engine is
//!      writable, `bootstrap_branch` first;
//!   3. role: `snapshot.role_by_name(user)` - missing: FATAL 28000 `role "x" does not exist`; not
//!      `can_login`: FATAL 28000 `role "x" is not permitted to log in`; `valid_until` in the past: FATAL
//!      28000 `role "x" password has expired`? (only when a password was used; skip in trust mode);
//!   4. database: `snapshot.database_by_name(name)` - missing: FATAL 3D000 `database "x" does not exist`;
//!      oid != 5: 0A000 (core amendment 02); `allow_conn == false`: FATAL 42501 `database "x" is not
//!      currently accepting connections`;
//!   5. `engine.attach_session(&SessionRequest{branch, database_oid: 5, database_name, user,
//!      application_name, client_addr, client_port, cancel_key_len})`; 53300 FATAL when full;
//!   6. `NotifyHub::register(pid, None)`; GUC table `GucTable::new(role.superuser)`;
//!      startup assignments (`GucSource::Client`): `application_name`, then each `startup_gucs` entry in
//!      order (errors fail the connection with the SET error as FATAL), then role/database settings
//!      (`Role::config` / database config entries `name=value`, source `Database`/`User`), then
//!      `session_authorization = user`, `is_superuser = on/off` via `force_value`;
//!   7. PRNG seed from `pgarbor_core::session::random_bytes(8)`;
//!   8. build the `Session` (block `Idle`, no txn, empty stores, `env: None`).
//! * [`lookup_role_auth`]: the catalog `Role` -> `RoleAuth` (`password` field = verifier).
//! * [`Session::ensure_bootstrapped`]: inside a running transaction, bootstrap if
//!   `persist::is_bootstrapped(&kv)` is false and the transaction is writable (used by `refresh_env` for
//!   branches created before bootstrap).
use std::cell::{Cell, RefCell};

use pgarbor_branches::{BranchRef, POSTGRES_DATABASE_OID};
use pgarbor_catalog::{persist, CatalogSnapshot};
use pgarbor_catalog::{Database as CatDatabase, Role as CatRole};
use pgarbor_common::{BranchId, PgError, Result, Severity, SqlState};
use pgarbor_core::session::random_bytes;
use pgarbor_core::{SessionHandle, SessionRequest, TxnKind, TxnOptions};

use crate::database::Database;
use crate::guc::{GucSource, GucTable};
use crate::notify::ListenState;
use crate::portal::PortalStore;
use crate::prepared::PreparedStore;
use crate::session::Session;
use crate::txn_block::BlockState;
use crate::types::{RoleAuth, SessionOptions};

/// Bootstrap the catalog of `branch` (module docs). Returns true if it did work.
pub(crate) fn bootstrap_branch(db: &Database, branch: &str) -> Result<bool> {
    let engine = &db.inner.engine;
    // Throw-away internal session; a missing or deleted branch fails here with the branches error.
    let handle = engine.attach_session(&SessionRequest::embedded(branch))?;
    let mut txn = engine.begin(&handle, &TxnOptions { kind: TxnKind::Explicit, ..TxnOptions::default() })?;
    txn.begin_statement()?;
    let created = persist::bootstrap(&mut txn.kv("BOOTSTRAP"))?;
    txn.commit()?;
    Ok(created)
}

/// Build a connected session (module docs).
pub(crate) fn connect_session(db: &Database, branch: &str, opts: SessionOptions) -> Result<Session> {
    let engine = &db.inner.engine;
    // 1 and 2: branch resolution and catalog snapshot (bootstrapping a writable, fresh branch).
    let (branch_id, snapshot) = read_catalog(db, branch)?;

    // 3: role.
    let role: CatRole = match snapshot.role_by_name(&opts.user) {
        Some(r) => r.clone(),
        None => {
            return Err(fatal_err(
                SqlState::INVALID_AUTHORIZATION_SPECIFICATION,
                format!("role \"{}\" does not exist", opts.user),
            ))
        }
    };
    if !role.can_login {
        return Err(fatal_err(
            SqlState::INVALID_AUTHORIZATION_SPECIFICATION,
            format!("role \"{}\" is not permitted to log in", opts.user),
        ));
    }

    // 4: database.
    let database: CatDatabase = match snapshot.database_by_name(&opts.database) {
        Some(d) => d.clone(),
        None => {
            return Err(fatal_err(
                SqlState::INVALID_CATALOG_NAME,
                format!("database \"{}\" does not exist", opts.database),
            ))
        }
    };
    if database.oid != POSTGRES_DATABASE_OID {
        return Err(fatal_err(
            SqlState::FEATURE_NOT_SUPPORTED,
            format!("database \"{}\" is not supported: only database \"postgres\" exists", database.name),
        ));
    }
    // PG InitPostgres: superusers may connect to a database that does not allow connections.
    if !database.allow_conn && !role.superuser {
        return Err(fatal_err(
            SqlState::INSUFFICIENT_PRIVILEGE,
            format!("database \"{}\" is not currently accepting connections", database.name),
        ));
    }

    // Connection limits (PG counts existing backends; superusers are exempt).
    let open_sessions = engine.sessions().all();
    if !role.superuser && role.conn_limit >= 0 {
        let open = open_sessions.iter().filter(|s| s.user == role.name).count();
        if open as i64 >= i64::from(role.conn_limit) {
            return Err(fatal_err(
                SqlState::TOO_MANY_CONNECTIONS,
                format!("too many connections for role \"{}\"", role.name),
            ));
        }
    }
    if !role.superuser && database.conn_limit >= 0 {
        let open = open_sessions.iter().filter(|s| s.database_oid == database.oid).count();
        if open as i64 >= i64::from(database.conn_limit) {
            return Err(fatal_err(
                SqlState::TOO_MANY_CONNECTIONS,
                format!("too many connections for database \"{}\"", database.name),
            ));
        }
    }
    drop(open_sessions);

    // 5: register the backend with the engine (53300 when full).
    let request = SessionRequest {
        branch: BranchRef::id(branch_id),
        database_oid: database.oid,
        database_name: database.name.clone(),
        user: opts.user.clone(),
        application_name: opts.application_name.clone(),
        client_addr: opts.client_addr.clone(),
        client_port: opts.client_port,
        cancel_key_len: opts.cancel_key_len,
    };
    let handle = engine.attach_session(&request).map_err(fatal)?;

    // 6: notification mailbox; undone if the rest of the connection setup fails.
    let pid = handle.id().0;
    db.inner.notify.register(pid, None);
    match build_session(db, handle, opts, &role, &database) {
        Ok(session) => Ok(session),
        Err(e) => {
            db.inner.notify.unregister(pid);
            Err(e)
        }
    }
}

/// Startup assignments, PRNG seed and the `Session` value (steps 6-8 of the module docs).
fn build_session(
    db: &Database,
    handle: SessionHandle,
    opts: SessionOptions,
    role: &CatRole,
    database: &CatDatabase,
) -> Result<Session> {
    let guc = GucTable::new(role.superuser);
    apply_startup_gucs(&guc, &opts, role, database).map_err(fatal)?;
    let seed = rng_seed()?;
    Ok(Session {
        db: db.clone(),
        handle,
        opts,
        user_oid: role.oid,
        session_user_oid: role.oid,
        database_oid: database.oid,
        txn: None,
        block: BlockState::Idle,
        guc,
        env: None,
        prepared: PreparedStore::default(),
        portals: PortalStore::default(),
        rng: Cell::new(seed),
        pending_notices: Vec::new(),
        listen: RefCell::new(ListenState::default()),
        error_since_sync: false,
        pipelining: false,
        closed: false,
        copy_channel: None,
    })
}

/// Startup GUC assignments in PG order: `application_name`, `startup_gucs`, database settings, role
/// settings (PG `process_settings`: database-only before role-only, so the role wins), then
/// `session_authorization` and `is_superuser` forced from the validated role.
fn apply_startup_gucs(guc: &GucTable, opts: &SessionOptions, role: &CatRole, database: &CatDatabase) -> Result<()> {
    guc.apply_startup("application_name", &opts.application_name, GucSource::Client)?;
    for (name, value) in &opts.startup_gucs {
        guc.apply_startup(name, value, GucSource::Client)?;
    }
    for entry in &database.config {
        let (name, value) = split_setting(entry)?;
        guc.apply_startup(name, value, GucSource::Database)?;
    }
    for entry in &role.config {
        let (name, value) = split_setting(entry)?;
        guc.apply_startup(name, value, GucSource::User)?;
    }
    guc.force_value("session_authorization", &opts.user)?;
    guc.force_value("is_superuser", if role.superuser { "on" } else { "off" })?;
    Ok(())
}

/// Split a stored `name=value` setting at the first `=`.
fn split_setting(entry: &str) -> Result<(&str, &str)> {
    match entry.split_once('=') {
        Some((name, value)) if !name.is_empty() => Ok((name, value)),
        _ => Err(PgError::corruption(format!("malformed stored setting \"{entry}\""))),
    }
}

/// Seed for the session PRNG from OS-seeded bytes; never zero.
fn rng_seed() -> Result<u64> {
    let bytes = random_bytes(8)?;
    let mut seed: u64 = 0;
    for (i, b) in bytes.iter().take(8).enumerate() {
        seed |= u64::from(*b) << (8 * i);
    }
    Ok(if seed == 0 { 0x9E37_79B9_7F4A_7C15 } else { seed })
}

fn fatal(e: PgError) -> PgError {
    e.with_severity(Severity::Fatal)
}

fn fatal_err(code: SqlState, message: String) -> PgError {
    PgError::new(code, message).with_severity(Severity::Fatal)
}

/// Resolve `branch` and return its id with the catalog snapshot of its head. A branch whose catalog is
/// not bootstrapped is bootstrapped first when the engine is writable; read-only engines fail with
/// XX001 `database is not initialized`.
fn read_catalog(db: &Database, branch: &str) -> Result<(BranchId, CatalogSnapshot)> {
    let engine = &db.inner.engine;
    let rec = engine.branches().get(&BranchRef::name(branch))?;
    let id = rec.id;
    let head = engine.branch_head(&id, POSTGRES_DATABASE_OID)?;
    if persist::is_bootstrapped(&head.tree)? {
        let snapshot = db.inner.catalog_cache.snapshot(&head.tree)?;
        return Ok((id, snapshot));
    }
    if engine.is_read_only() {
        return Err(fatal_err(SqlState(*b"XX001"), "database is not initialized".to_string()));
    }
    bootstrap_branch(db, branch)?;
    let head = engine.branch_head(&id, POSTGRES_DATABASE_OID)?;
    let snapshot = db.inner.catalog_cache.snapshot(&head.tree)?;
    Ok((id, snapshot))
}

/// Credentials lookup (module docs).
pub(crate) fn lookup_role_auth(db: &Database, branch: &str, user: &str) -> Result<Option<RoleAuth>> {
    let (_, snapshot) = read_catalog(db, branch)?;
    Ok(snapshot.role_by_name(user).map(|r| RoleAuth {
        name: r.name.clone(),
        oid: r.oid,
        can_login: r.can_login,
        superuser: r.superuser,
        password_verifier: r.password.clone(),
        valid_until: r.valid_until,
        conn_limit: r.conn_limit,
    }))
}

impl Session {
    /// Bootstrap the catalog inside the running transaction when needed (module docs).
    pub(crate) fn ensure_bootstrapped(&mut self) -> Result<()> {
        let txn = match self.txn.as_mut() {
            Some(t) => t,
            None => return Err(PgError::internal("catalog bootstrap requested outside a transaction")),
        };
        let bootstrapped = {
            let kv = txn.kv("BOOTSTRAP");
            persist::is_bootstrapped(&kv)?
        };
        if bootstrapped || txn.is_read_only() {
            return Ok(());
        }
        persist::bootstrap(&mut txn.kv("BOOTSTRAP"))?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stored_settings_split_on_first_equals() {
        let (name, value) = split_setting("search_path=a=b").expect("split");
        assert_eq!(name, "search_path");
        assert_eq!(value, "a=b");
        assert!(split_setting("novalue").is_err());
        assert!(split_setting("=x").is_err());
    }

    #[test]
    fn fatal_errors_have_fatal_severity() {
        let e = fatal_err(SqlState::INVALID_AUTHORIZATION_SPECIFICATION, "role \"x\" does not exist".to_string());
        assert_eq!(e.severity, Severity::Fatal);
        assert_eq!(e.sqlstate.code(), "28000");
        assert_eq!(e.message, "role \"x\" does not exist");
    }

    #[test]
    fn rng_seed_is_never_zero() {
        for _ in 0..16 {
            assert_ne!(rng_seed().expect("seed"), 0);
        }
    }

    #[test]
    #[ignore = "needs API-04"]
    fn in_memory_database_is_already_bootstrapped() {
        let db = Database::in_memory().expect("in-memory database");
        assert!(!bootstrap_branch(&db, "main").expect("bootstrap"));
    }

    #[test]
    #[ignore = "needs API-04"]
    fn connect_resolves_role_and_database() {
        let db = Database::in_memory().expect("in-memory database");
        let mut unknown = SessionOptions::default();
        unknown.user = "x".to_string();
        match connect_session(&db, "main", unknown) {
            Ok(_) => panic!("unknown role must not connect"),
            Err(e) => {
                assert_eq!(e.sqlstate.code(), "28000");
                assert_eq!(e.message, "role \"x\" does not exist");
            }
        }
        let mut no_db = SessionOptions::default();
        no_db.database = "nope".to_string();
        match connect_session(&db, "main", no_db) {
            Ok(_) => panic!("unknown database must not connect"),
            Err(e) => assert_eq!(e.sqlstate.code(), "3D000"),
        }
        let mut named = SessionOptions::default();
        named.application_name = "tests".to_string();
        let s = connect_session(&db, "main", named).expect("connect as postgres");
        assert_eq!(s.setting("application_name").expect("show"), "tests");
    }
}
