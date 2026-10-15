//! Assembly of the per-statement contexts of the lower layers (lease API-09, north-star). Pure
//! plumbing over `Session` fields; this is where the borrow-splitting happens.
//!
//! ## `refresh_env` - statement start protocol (catalog contract, "Transactions, concurrency and
//! invalidation")
//! 1. If the session has no live transaction snapshot (utility statement before `begin_statement`)
//!    reuse `self.env` when present.
//! 2. Else `snap = db.catalog_cache.snapshot(&txn.kv(..))` (one point read of `cat:version`; an
//!    unbootstrapped branch is bootstrapped first: `Session::ensure_bootstrapped` (bootstrap.rs)).
//! 3. If `self.env` exists and `snap.ptr_eq(env.snapshot())` and the search_path/user did not change:
//!    keep it. Otherwise rebuild: `CatalogEnv::new(snap, &guc_derive::search_path(&guc)?, user_oid)` then
//!    `.with_session(session_user_oid, database_oid)`; if the snapshot changed, run
//!    `catalog::invalidation::diff(old, new)` and mark prepared statements for revalidation
//!    (`prepared::PreparedStore::note_invalidation`).
//!
//! ## Context builders
//! * [`Session::bind_context`]: `BindContext{env, session: BindSession{database}, io_config, statement_timestamp
//!   = current transaction start (timestamptz us, see `txn_timestamp`), declared_params}`.
//! * [`Session::plan_context`]: `PlanContext{env, param_types, param_values, options: guc_derive::plan_options,
//!   table_rows: empty (amendment plan-01: no row-count source yet)}`.
//! * [`Session::with_exec`]: builds `ExecContext` and runs the closure. Inside: `let Session { txn, guc,
//!   rng, prepared, db, .. } = self;` (split borrows), `let mut kv = txn.kv(command)`,
//!   `kv.set_no_key_update(false)`, `SessionEnv::new(env, io_config, guc, &services, rng, statement_ts, txn_ts)`
//!   with `services = engine_info::SessionServices::new(..)`, `se.params/param_types` set, then
//!   `ExecContext::new(&mut kv, se)`; after the closure copies `cx.se.take_notices()` into the
//!   returned `Executed.notices` and, if `cx.stmt.catalog_changed`, replaces `self.env` with `cx.se.env`.
//! * [`Session::column_infos`]: `ColumnDesc`/`OutputColumn` -> `ColumnInfo` (adds `type_len` from
//!   `env`'s `TypeInfo`: `CatalogEnv::type_info(oid)`; typmod from the column type; table/attnum from the
//!   descriptor).
//! * [`Session::txn_timestamp`]/[`Session::statement_timestamp`]: timestamptz microseconds since
//!   2000-01-01 UTC: transaction start = captured when the core transaction starts (store it in the
//!   `Session`... derive from `Txn`'s start time `started_unix_ms` via activity `xact_start_ms`);
//!   statement = now at statement start. (`now()` must be stable inside a transaction.)
use pgarbor_catalog::CatalogEnv;
use pgarbor_common::{PgError, Result};
use pgarbor_exec::ExecContext;
use pgarbor_plan::plan::ColumnDesc;
use pgarbor_plan::PlanContext;
use pgarbor_sql::BindContext;
use pgarbor_types::{Datum, TypeId};

use crate::session::Session;
use crate::types::ColumnInfo;

/// What `with_exec` hands back besides the closure's value.
// CONTRACT
#[derive(Debug, Default)]
pub struct ExecSideEffects {
    pub notices: Vec<PgError>,
    pub catalog_changed: bool,
}

impl Session {
    /// Statement-start catalog protocol (module docs). Returns a clone of the current env.
    pub(crate) fn refresh_env(&mut self) -> Result<CatalogEnv> {
        Err(PgError::unimplemented("API-09", "catalog env refresh (Session::refresh_env)"))
    }

    /// Transaction start time as timestamptz us since 2000-01-01 (module docs).
    pub(crate) fn txn_timestamp(&self) -> i64 {
        // Derived from the activity record; 0 when no transaction.
        self.handle
            .shared()
            .activity()
            .xact_start_ms
            .map(|ms| (ms as i64) * 1000 - 946_684_800_000_000)
            .unwrap_or(0)
    }

    /// `statement_timestamp()`.
    #[allow(unused_variables)]
    pub(crate) fn statement_timestamp(&self) -> Result<i64> {
        Err(PgError::unimplemented("API-09", "statement timestamp (Session::statement_timestamp)"))
    }

    #[allow(unused_variables)]
    pub(crate) fn bind_context<'a>(&'a self, env: &'a CatalogEnv, declared: Vec<TypeId>) -> Result<BindContext<'a>> {
        Err(PgError::unimplemented("API-09", "BindContext (Session::bind_context)"))
    }

    #[allow(unused_variables)]
    pub(crate) fn plan_context<'a>(
        &'a self,
        env: &'a CatalogEnv,
        param_types: Vec<TypeId>,
        param_values: Option<Vec<Datum>>,
        for_explain: bool,
    ) -> Result<PlanContext<'a>> {
        Err(PgError::unimplemented("API-09", "PlanContext (Session::plan_context)"))
    }

    /// Run `f` with an `ExecContext` over the live transaction (module docs). `command` names the
    /// statement for 25006 errors (`INSERT`, `CREATE TABLE`, ...).
    #[allow(unused_variables)]
    pub(crate) fn with_exec<R>(
        &mut self,
        command: &'static str,
        env: &CatalogEnv,
        params: Vec<Datum>,
        param_types: Vec<TypeId>,
        f: impl FnOnce(&mut ExecContext) -> Result<R>,
    ) -> Result<(R, ExecSideEffects)> {
        Err(PgError::unimplemented("API-09", "ExecContext assembly (Session::with_exec)"))
    }

    /// `ColumnInfo`s for result descriptors (module docs).
    #[allow(unused_variables)]
    pub(crate) fn column_infos(&self, env: &CatalogEnv, cols: &[ColumnDesc]) -> Result<Vec<ColumnInfo>> {
        Err(PgError::unimplemented("API-09", "ColumnInfo conversion (Session::column_infos)"))
    }
}
