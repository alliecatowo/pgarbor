//! [`CatalogEnv`]: a snapshot plus the session's resolved search_path and roles. This is the
//! object the binder, planner, executor and virtual relations hold: it implements
//! `pgarbor_types::TypeEnv` (`crate::typeenv`, CAT-05) and the name-resolution API
//! (`crate::resolve`, CAT-04). Cheap to clone (Arc'd parts). Fully implemented.
use crate::search_path::{resolve_search_path, ResolvedSearchPath, SearchPath};
use crate::snapshot::CatalogSnapshot;
use pgarbor_common::{Oid, Result};
use std::sync::Arc;

/// Snapshot + session naming context.
// CONTRACT
#[derive(Clone, Debug)]
pub struct CatalogEnv {
    snapshot: CatalogSnapshot,
    path: Arc<ResolvedSearchPath>,
    current_user: Oid,
    session_user: Oid,
    database: Oid,
}

impl CatalogEnv {
    /// Resolve `search_path` for `current_user` (its name looked up in the snapshot; an unknown
    /// role resolves `$user` to nothing) and build the env. Database = `postgres` (5).
    pub fn new(snapshot: CatalogSnapshot, search_path: &SearchPath, current_user: Oid) -> Result<CatalogEnv> {
        let user_name = snapshot.role_name(current_user).unwrap_or("").to_string();
        let resolved = resolve_search_path(&snapshot, search_path, &user_name, None)?;
        Ok(CatalogEnv::with_resolved(snapshot, resolved, current_user))
    }

    /// Build from an already resolved path (session_user = current_user, database = 5).
    pub fn with_resolved(snapshot: CatalogSnapshot, path: ResolvedSearchPath, current_user: Oid) -> CatalogEnv {
        CatalogEnv {
            snapshot,
            path: Arc::new(path),
            current_user,
            session_user: current_user,
            database: crate::builtin::POSTGRES_DATABASE,
        }
    }

    /// Builtin default path ([pg_catalog, public]) as the bootstrap superuser. For tests/bootstrap.
    pub fn with_default_path(snapshot: CatalogSnapshot) -> CatalogEnv {
        CatalogEnv::with_resolved(snapshot, ResolvedSearchPath::builtin_default(), crate::builtin::BOOTSTRAP_SUPERUSER)
    }

    /// Same session context over a different snapshot (after DDL / invalidation). The search path is
    /// NOT re-resolved; callers that changed schemas must call `new` again (PG recomputes the path
    /// on every namespace invalidation; pgarbor does it per statement in the session layer).
    pub fn with_snapshot(&self, snapshot: CatalogSnapshot) -> CatalogEnv {
        CatalogEnv { snapshot, ..self.clone() }
    }

    /// Same env with explicit session user / database.
    pub fn with_session(mut self, session_user: Oid, database: Oid) -> CatalogEnv {
        self.session_user = session_user;
        self.database = database;
        self
    }

    pub fn snapshot(&self) -> &CatalogSnapshot {
        &self.snapshot
    }
    pub fn path(&self) -> &ResolvedSearchPath {
        &self.path
    }
    /// Effective search order for types/functions/operators.
    pub fn search_path(&self) -> &[Oid] {
        &self.path.effective
    }
    pub fn current_user(&self) -> Oid {
        self.current_user
    }
    pub fn session_user(&self) -> Oid {
        self.session_user
    }
    pub fn database(&self) -> Oid {
        self.database
    }
    /// Unqualified-CREATE target, or 3F000 `no schema has been selected to create in`.
    pub fn creation_namespace(&self) -> Result<Oid> {
        self.path.creation_namespace.ok_or_else(crate::errors::no_schema_selected)
    }
}
