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
use pgarbor_common::{PgError, Result};

use crate::database::Database;
use crate::session::Session;
use crate::types::{RoleAuth, SessionOptions};

/// Bootstrap the catalog of `branch` (module docs). Returns true if it did work.
#[allow(unused_variables)]
pub(crate) fn bootstrap_branch(db: &Database, branch: &str) -> Result<bool> {
    Err(PgError::unimplemented("API-05", "catalog bootstrap (bootstrap::bootstrap_branch)"))
}

/// Build a connected session (module docs).
#[allow(unused_variables)]
pub(crate) fn connect_session(db: &Database, branch: &str, opts: SessionOptions) -> Result<Session> {
    Err(PgError::unimplemented("API-05", "session connection (bootstrap::connect_session)"))
}

/// Credentials lookup (module docs).
#[allow(unused_variables)]
pub(crate) fn lookup_role_auth(db: &Database, branch: &str, user: &str) -> Result<Option<RoleAuth>> {
    Err(PgError::unimplemented("API-05", "role credentials (bootstrap::lookup_role_auth)"))
}

impl Session {
    /// Bootstrap the catalog inside the running transaction when needed (module docs).
    pub(crate) fn ensure_bootstrapped(&mut self) -> Result<()> {
        Err(PgError::unimplemented("API-05", "lazy catalog bootstrap (Session::ensure_bootstrapped)"))
    }
}
