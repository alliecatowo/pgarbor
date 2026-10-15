//! PGArbor catalog: the PostgreSQL-compatible system catalog of one branch.
//!
//! CONTRACT: everything marked `// CONTRACT` is frozen at `contracts-v1`; see
//! `contracts/crates/pgarbor-catalog.md` for the design, data formats and downstream usage and
//! `contracts/leases/pgarbor-catalog.json` for the implementation leases (CAT-01..CAT-49).
//!
//! ## Module map
//! | area | modules | role |
//! |---|---|---|
//! | object model | [`model`] | plain serde structs: `Namespace`, `Relation`+`Column`, `Index`, `Constraint`, `Sequence`, `TypeObj`, `Function`, `Role`, `Database`, `ObjectAddress`, `StoredExpr`, `Acl` |
//! | persistence | [`persist`], [`data`] | byte-exact keys in KV space 1, versioned JSON rows, OID sources, catalog version in space 3, bootstrap |
//! | snapshots | [`snapshot`], [`cache`], [`invalidation`], [`env`] | immutable indexed `CatalogSnapshot`, version-keyed cache, change detection, `CatalogEnv` (snapshot + search_path + user) |
//! | name resolution | [`search_path`], [`resolve`], [`typeenv`], [`naming`], [`describe`] | PG `namespace.c` / `regproc.c` semantics, `pgarbor_types::TypeEnv`, automatic object names, object descriptions |
//! | builtin objects | [`builtin`] | generated PG 18.6 system relations (exact column lists), schemas, roles, collations, descriptions |
//! | DDL | [`txn`], [`ddl`], [`drop`], [`depend`], [`acl`], [`deparse`] | `CatalogTxn` writer, one file per statement family, dependency graph + DROP planner, privileges, definition text |
//! | virtual relations | [`vrel`], [`session`] | `pg_catalog` / `information_schema` row producers over snapshot + `SessionInfo` |
//! | SQL functions | [`sysfuncs`] | catalog-backed `ImplModule::External` functions |
//! | tools | [`diff`], [`errors`] | schema diff of two snapshots; PG-exact error constructors |
//!
//! ## Typical flow (see the contract doc for the full protocol)
//! ```ignore
//! // statement start (any statement):
//! let snap = cache.snapshot(&txn_kv)?;                       // one point read of cat:version
//! let env = CatalogEnv::new(snap, &search_path, user_oid)?;  // naming context
//! // binder: env.lookup_class / lookup_type / function_candidates; env implements TypeEnv.
//! // DDL statement:
//! txn.lock_key(&ddl_lock_key(), ...)?;                       // serialize catalog writers
//! let mut ddl = CatalogTxn::begin(&mut store, &env)?;    // store: KvWrite + OidSource over the txn
//! ddl.create_table(&spec)?;
//! let res = ddl.finish_statement(stamp)?;                    // bumps cat:version once
//! // executor performs res.outcome.follow_ups, session swaps res.snapshot
//! // catalog scans: vrel::scan(&ScanContext{env, session, io_config}, rel_oid, &ScanRequest::all())
//! ```
#![allow(clippy::too_many_arguments)]

pub mod acl;
pub mod builtin;
pub mod cache;
pub mod data;
pub mod ddl;
pub mod depend;
pub mod deparse;
pub mod describe;
pub mod diff;
pub mod drop;
pub mod env;
pub mod errors;
pub mod invalidation;
pub mod model;
pub mod naming;
pub mod persist;
pub mod resolve;
pub mod search_path;
pub mod session;
pub mod snapshot;
pub mod sysfuncs;
#[doc(hidden)]
pub mod testing;
pub mod txn;
pub mod typeenv;
pub mod vrel;

pub use cache::CatalogCache;
pub use data::CatalogData;
pub use env::CatalogEnv;
pub use model::{
    Column, Constraint, ConstraintKind, Database, DescriptionKey, DropBehavior, Function, Index, Namespace, ObjClass,
    ObjectAddress, Relation, RelKind, Role, Sequence, StoredExpr, TypeObj,
};
pub use persist::{CatalogStore, CatalogVersion, MemoryOids, OidSource, WithOids};
pub use search_path::{ResolvedSearchPath, SearchPath};
pub use session::{NullSessionInfo, SessionInfo};
pub use snapshot::CatalogSnapshot;
pub use txn::{ddl_lock_key, CatalogTxn, DdlOutcome, DdlResult, FollowUp};
