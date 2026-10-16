//! [`CatalogTxn`]: the DDL writer. One `CatalogTxn` lives for exactly ONE DDL statement; it holds
//! a working copy of the catalog data, writes every change THROUGH to the `&mut dyn KvWrite` it was
//! given (the transaction's overlay view), and bumps `cat:version` once in `finish_statement`.
//! The DDL operations themselves are `impl CatalogTxn` blocks in `crate::ddl::*` and
//! `crate::drop`. Core mechanics only here (fully implemented, frozen v1).
//!
//! ## Transaction / concurrency protocol (what pgarbor-exec / the pgarbor crate MUST do)
//! 1. Before the FIRST catalog write of a statement take the catalog write lock:
//!    `txn.lock_key(&ddl_lock_key(), ForUpdate, Block)` (pgarbor-core). DDL is therefore SERIALIZED
//!    per branch (PG locks per object and mostly does not block; this is a documented difference:
//!    two sessions running DDL concurrently queue, and can deadlock-report `40P01` in cases PG would
//!    not). Without this lock two sessions creating `public.t` concurrently would get different OIDs,
//!    touch different keys and both commit -> duplicate names (the snapshot builder then reports
//!    XX001 corruption).
//! 2. If the lock result is `Changed` (someone committed catalog changes after this statement's
//!    snapshot): READ COMMITTED -> `begin_statement()` again, reload the catalog snapshot
//!    (`CatalogCache::snapshot`), re-bind the DDL statement; REPEATABLE READ/SERIALIZABLE -> 40001.
//! 3. `CatalogTxn::begin(store, &env)` re-verifies `read_version(kv) == env.snapshot().version()`
//!    and fails with 40001 (`errors::concurrent_catalog_change`) otherwise (defense in depth).
//! 4. Run the operations; each returns its own result and ALSO records notices and
//!    [`FollowUp`]s in the shared outcome.
//! 5. `finish_statement(stamp)` bumps the version (only if something changed) and returns
//!    [`DdlResult`] (outcome + new version + built snapshot). `stamp` must be unique per call
//!    (pgarbor-core passes `random_u64() ^ txn_id`). Dropping a `CatalogTxn` without finishing
//!    leaves rows written but the version unbumped: the CALLER must roll the statement back to its
//!    statement savepoint on any `Err` (statement atomicity is the transaction layer's job).
//! 6. The executor then performs the `follow_ups` (index builds, validations, rewrites, data
//!    removal, sequence state) inside the same transaction.
//!
//! OIDs come from the same object that writes the rows (a [`CatalogStore`] = `KvWrite + OidSource`),
//! see `persist::meta` for why and for the ready-made stores (`MemKv`, `WithOids`).
use crate::data::CatalogData;
use crate::env::CatalogEnv;
use crate::model::*;
use crate::persist::{self, CatalogStore, CatalogVersion};
use crate::snapshot::CatalogSnapshot;
use pgarbor_common::{Oid, PgError, Result};

pub use crate::ddl::spec::FollowUp;

/// Everything a DDL statement reports back besides its direct return value.
// CONTRACT
#[derive(Clone, Debug, Default, PartialEq)]
pub struct DdlOutcome {
    /// NOTICEs raised (IF [NOT] EXISTS skips, `drop cascades to ...`, truncation notices, ...), in
    /// emission order. The session layer sends them to the client.
    pub notices: Vec<PgError>,
    /// Data-level work for the executor, in the order it must run.
    pub follow_ups: Vec<FollowUp>,
    /// Objects created by the statement.
    pub created: Vec<ObjectAddress>,
    /// Objects dropped by the statement (including cascades), in deletion order.
    pub dropped: Vec<ObjectAddress>,
}

/// Result of [`CatalogTxn::finish_statement`].
// CONTRACT
#[derive(Clone, Debug)]
pub struct DdlResult {
    pub outcome: DdlOutcome,
    /// Version after the statement (unchanged when the statement wrote nothing).
    pub version: CatalogVersion,
    /// The catalog after the statement (includes the txn's own uncommitted changes): swap the
    /// session's snapshot to this and `CatalogCache::insert` it.
    pub snapshot: CatalogSnapshot,
    /// Whether anything was written.
    pub changed: bool,
}

/// The key every DDL statement must lock (module docs, step 1): `cat:version` in space 3.
// CONTRACT
pub fn ddl_lock_key() -> Vec<u8> {
    persist::keys::version_key()
}

/// DDL writer for one statement (module docs).
pub struct CatalogTxn<'a> {
    pub(crate) store: &'a mut dyn CatalogStore,
    /// Session naming context (search_path, current user); its snapshot is refreshed lazily.
    pub(crate) env: CatalogEnv,
    /// Working copy of the persisted objects (always current).
    pub(crate) data: CatalogData,
    /// `env.snapshot()` no longer reflects `data`.
    pub(crate) dirty: bool,
    pub(crate) base_version: CatalogVersion,
    pub(crate) changed: bool,
    pub(crate) outcome: DdlOutcome,
}

impl<'a> CatalogTxn<'a> {
    /// Start a DDL statement over the catalog described by `env` (module docs, step 3).
    pub fn begin(store: &'a mut dyn CatalogStore, env: &CatalogEnv) -> Result<CatalogTxn<'a>> {
        let cur = persist::read_version(&*store)?;
        if cur != env.snapshot().version() {
            return Err(crate::errors::concurrent_catalog_change());
        }
        Ok(CatalogTxn {
            store,
            env: env.clone(),
            data: env.snapshot().data().clone(),
            dirty: false,
            base_version: cur,
            changed: false,
            outcome: DdlOutcome::default(),
        })
    }

    /// The statement's current naming context, with a snapshot that includes this statement's own
    /// earlier changes (rebuilt on demand: O(catalog), fine for DDL). Returned BY VALUE (an `Arc`
    /// clone) so callers can keep it across later `&mut self` calls (`put`, `new_oid`, ...).
    pub fn env(&mut self) -> Result<CatalogEnv> {
        self.refresh()?;
        Ok(self.env.clone())
    }

    /// Current snapshot (see [`CatalogTxn::env`]).
    pub fn snapshot(&mut self) -> Result<CatalogSnapshot> {
        self.refresh()?;
        Ok(self.env.snapshot().clone())
    }

    /// Rebuild the snapshot if `data` changed since the last build.
    pub fn refresh(&mut self) -> Result<()> {
        if self.dirty {
            let snap = CatalogSnapshot::build(self.base_version, self.data.clone())?;
            self.env = self.env.with_snapshot(snap);
            self.dirty = false;
        }
        Ok(())
    }

    /// Role executing the statement.
    pub fn current_user(&self) -> Oid {
        self.env.current_user()
    }

    /// The working data (read-only).
    pub fn data(&self) -> &CatalogData {
        &self.data
    }

    /// Allocate a fresh OID that is not used by any object of the working data (including OIDs embedded
    /// in values). The store guarantees uniqueness in production; the re-check means a misbehaving
    /// store can never corrupt the catalog (up to 8 attempts, then XX000).
    pub fn new_oid(&mut self) -> Result<Oid> {
        for _ in 0..8 {
            let o = self.store.next_oid()?;
            if o >= pgarbor_common::FIRST_NORMAL_OID && !self.data.oid_in_use(o) {
                return Ok(o);
            }
        }
        Err(PgError::internal("OID source returned only in-use OIDs"))
    }

    /// Insert or replace an object (write-through).
    pub fn put(&mut self, obj: CatalogObject) -> Result<()> {
        persist::put_object(&mut *self.store, &obj)?;
        self.data.insert(obj);
        self.dirty = true;
        self.changed = true;
        Ok(())
    }

    /// Delete an object (write-through); error `cache lookup failed` if it did not exist.
    pub fn remove(&mut self, kind: ObjKind, oid: Oid) -> Result<()> {
        if !self.data.remove(kind, oid) {
            return Err(crate::errors::cache_lookup_failed(&format!("{kind:?}").to_lowercase(), oid));
        }
        persist::delete_object(&mut *self.store, kind, oid)?;
        self.dirty = true;
        self.changed = true;
        Ok(())
    }

    /// Set (or with `None` / empty text delete) a description row.
    pub fn set_description(&mut self, key: DescriptionKey, text: Option<&str>) -> Result<()> {
        match text {
            Some(t) if !t.is_empty() => {
                persist::put_description(&mut *self.store, &key, t)?;
                self.data.descriptions.insert(key, t.to_string());
            }
            _ => {
                persist::delete_description(&mut *self.store, &key)?;
                self.data.descriptions.remove(&key);
            }
        }
        self.dirty = true;
        self.changed = true;
        Ok(())
    }

    /// Delete every description attached to `(classoid, objoid)` (all sub-ids).
    pub fn clear_descriptions(&mut self, classoid: Oid, objoid: Oid) -> Result<()> {
        let keys: Vec<DescriptionKey> =
            self.data.descriptions.keys().filter(|k| k.classoid == classoid && k.objoid == objoid).copied().collect();
        for k in keys {
            self.set_description(k, None)?;
        }
        Ok(())
    }

    /// Modify a persisted relation in place (clone, mutate, write back).
    pub fn update_relation(&mut self, oid: Oid, f: impl FnOnce(&mut Relation) -> Result<()>) -> Result<()> {
        let mut r = self
            .data
            .relations
            .get(&oid)
            .cloned()
            .ok_or_else(|| crate::errors::cache_lookup_failed("relation", oid))?;
        f(&mut r)?;
        self.put(CatalogObject::Relation(r))
    }

    /// Modify a persisted index.
    pub fn update_index(&mut self, oid: Oid, f: impl FnOnce(&mut Index) -> Result<()>) -> Result<()> {
        let mut r = self.data.indexes.get(&oid).cloned().ok_or_else(|| crate::errors::cache_lookup_failed("index", oid))?;
        f(&mut r)?;
        self.put(CatalogObject::Index(r))
    }

    /// Modify a persisted sequence.
    pub fn update_sequence(&mut self, oid: Oid, f: impl FnOnce(&mut Sequence) -> Result<()>) -> Result<()> {
        let mut r =
            self.data.sequences.get(&oid).cloned().ok_or_else(|| crate::errors::cache_lookup_failed("sequence", oid))?;
        f(&mut r)?;
        self.put(CatalogObject::Sequence(r))
    }

    /// Modify a persisted user type.
    pub fn update_type(&mut self, oid: Oid, f: impl FnOnce(&mut TypeObj) -> Result<()>) -> Result<()> {
        let mut r = self.data.types.get(&oid).cloned().ok_or_else(|| crate::errors::cache_lookup_failed("type", oid))?;
        f(&mut r)?;
        self.put(CatalogObject::Type(r))
    }

    /// Modify a persisted constraint.
    pub fn update_constraint(&mut self, oid: Oid, f: impl FnOnce(&mut Constraint) -> Result<()>) -> Result<()> {
        let mut r =
            self.data.constraints.get(&oid).cloned().ok_or_else(|| crate::errors::cache_lookup_failed("constraint", oid))?;
        f(&mut r)?;
        self.put(CatalogObject::Constraint(r))
    }

    /// Modify a persisted schema.
    pub fn update_namespace(&mut self, oid: Oid, f: impl FnOnce(&mut Namespace) -> Result<()>) -> Result<()> {
        let mut r =
            self.data.namespaces.get(&oid).cloned().ok_or_else(|| crate::errors::cache_lookup_failed("schema", oid))?;
        f(&mut r)?;
        self.put(CatalogObject::Namespace(r))
    }

    /// Modify a persisted role.
    pub fn update_role(&mut self, oid: Oid, f: impl FnOnce(&mut Role) -> Result<()>) -> Result<()> {
        let mut r = self.data.roles.get(&oid).cloned().ok_or_else(|| crate::errors::cache_lookup_failed("role", oid))?;
        f(&mut r)?;
        self.put(CatalogObject::Role(r))
    }

    /// Modify a persisted function.
    pub fn update_function(&mut self, oid: Oid, f: impl FnOnce(&mut Function) -> Result<()>) -> Result<()> {
        let mut r =
            self.data.functions.get(&oid).cloned().ok_or_else(|| crate::errors::cache_lookup_failed("function", oid))?;
        f(&mut r)?;
        self.put(CatalogObject::Function(r))
    }

    /// Modify a persisted database row.
    pub fn update_database(&mut self, oid: Oid, f: impl FnOnce(&mut Database) -> Result<()>) -> Result<()> {
        let mut r =
            self.data.databases.get(&oid).cloned().ok_or_else(|| crate::errors::cache_lookup_failed("database", oid))?;
        f(&mut r)?;
        self.put(CatalogObject::Database(r))
    }

    /// Queue a NOTICE for the client.
    /// Take the NOTICEs queued so far (the DDL driver forwards them even when the statement fails,
    /// like PG which sends each NOTICE as it is raised).
    pub fn take_notices(&mut self) -> Vec<PgError> {
        std::mem::take(&mut self.outcome.notices)
    }
    pub fn notice(&mut self, n: PgError) {
        self.outcome.notices.push(n);
    }

    /// Queue executor work.
    pub fn follow_up(&mut self, f: FollowUp) {
        self.outcome.follow_ups.push(f);
    }

    /// Record a created object.
    pub fn record_created(&mut self, a: ObjectAddress) {
        self.outcome.created.push(a);
    }

    /// Record a dropped object.
    pub fn record_dropped(&mut self, a: ObjectAddress) {
        self.outcome.dropped.push(a);
    }

    /// Whether the statement has written anything yet.
    pub fn has_changes(&self) -> bool {
        self.changed
    }

    /// Finish the statement (module docs, step 5).
    pub fn finish_statement(self, stamp: u64) -> Result<DdlResult> {
        let CatalogTxn { store, data, outcome, changed, base_version, .. } = self;
        let version = if changed { persist::bump_version(&mut *store, stamp)? } else { base_version };
        let snapshot = CatalogSnapshot::build(version, data)?;
        Ok(DdlResult { outcome, version, snapshot, changed })
    }
}
