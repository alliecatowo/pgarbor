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
use std::collections::HashMap;
use pgarbor_common::time::{SystemTime, UNIX_EPOCH};

use pgarbor_catalog::{invalidation, CatalogEnv};
use pgarbor_common::{PgError, Result};
use pgarbor_exec::{ExecContext, SessionEnv};
use pgarbor_plan::plan::ColumnDesc;
use pgarbor_plan::PlanContext;
use pgarbor_sql::{BindContext, BindSession};
use pgarbor_types::registry::base_type_and_typmod;
use pgarbor_types::{Datum, TypeEnv, TypeId};

use crate::engine_info::SessionServices;
use crate::guc_derive;
use crate::session::Session;
use crate::types::ColumnInfo;

/// Microseconds from 1970-01-01 to 2000-01-01 (the timestamptz epoch offset).
const PG_EPOCH_OFFSET_US: i64 = 946_684_800_000_000;

/// What `with_exec` hands back besides the closure's value.
// CONTRACT
#[derive(Debug, Default)]
pub struct ExecSideEffects {
    pub notices: Vec<PgError>,
    pub catalog_changed: bool,
}

/// Wall clock as timestamptz microseconds since 2000-01-01 UTC (`statement_timestamp()` source).
fn now_pg_us() -> Result<i64> {
    let since_unix = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|_| PgError::internal("system clock is set before 1970-01-01"))?;
    let us = i64::try_from(since_unix.as_micros()).map_err(|_| PgError::internal("system clock out of range"))?;
    us.checked_sub(PG_EPOCH_OFFSET_US).ok_or_else(|| PgError::internal("system clock out of range"))
}

/// `ColumnDesc`s -> `ColumnInfo`s (module docs, [`Session::column_infos`]). The type length comes from
/// the type registry of `env`; an unknown type OID is an internal error (PG: `cache lookup failed for
/// type %u`, XX000).
fn columns_to_infos(env: &CatalogEnv, cols: &[ColumnDesc]) -> Result<Vec<ColumnInfo>> {
    let mut out = Vec::with_capacity(cols.len());
    for c in cols {
        // PG `SendRowDescriptionMessage`: a domain-typed column is described by its base type and typmod.
        let (oid, type_mod) = base_type_and_typmod(env, c.type_id.oid, c.type_id.typmod);
        let info = env
            .type_info(oid)
            .ok_or_else(|| PgError::internal(format!("cache lookup failed for type {oid}")))?;
        out.push(ColumnInfo {
            name: c.name.clone(),
            type_oid: oid,
            type_mod,
            type_len: info.len,
            table_oid: c.table_oid,
            column_attnum: c.column_attnum,
        });
    }
    Ok(out)
}

impl Session {
    /// Statement-start catalog protocol (module docs). Returns a clone of the current env.
    pub(crate) fn refresh_env(&mut self) -> Result<CatalogEnv> {
        if self.txn.is_none() {
            // Step 1: no live transaction snapshot; reuse the last env.
            return match &self.env {
                Some(env) => Ok(env.clone()),
                None => Err(PgError::internal("catalog environment requested without a transaction")),
            };
        }

        // Utility / transaction-control statements take no snapshot (PG leaves FirstSnapshotSet
        // alone, so `SET TRANSACTION` stays legal): reuse the last env instead of reading the catalog.
        if self.txn.as_ref().is_some_and(|t| !t.has_snapshot()) {
            if let Some(env) = &self.env {
                return Ok(env.clone());
            }
            // First statement of a fresh or reopened session: no cached env yet. Build one from a
            // short read-only look at the branch head (no transaction snapshot is taken).
            let (_, snap) = crate::bootstrap::read_catalog(&self.db, &self.branch_name())?;
            let sp = guc_derive::search_path(&self.guc)?;
            let env = CatalogEnv::new(snap, &sp, self.user_oid)?
                .with_session(self.session_user_oid, self.database_oid)
                .with_row_security(guc_derive::row_security(&self.guc)?);
            self.env = Some(env.clone());
            return Ok(env);
        }

        // Step 2: bootstrap a branch created before its catalog existed, then read the snapshot
        // through the statement's transaction view.
        self.ensure_bootstrapped()?;
        let snap = {
            let txn = self
                .txn
                .as_mut()
                .ok_or_else(|| PgError::internal("catalog environment requested without a transaction"))?;
            let kv = txn.kv("SELECT");
            self.db.inner.catalog_cache.snapshot(&kv)?
        };

        let sp = guc_derive::search_path(&self.guc)?;
        let row_security = guc_derive::row_security(&self.guc)?;
        let user = self.user_oid;
        let session_user = self.session_user_oid;
        let database = self.database_oid;

        // Step 3a: keep the current env when nothing it depends on changed. The search_path GUC
        // lives in the GUC table (not in the env), so the path is re-resolved to detect a SET
        // search_path; the resolution is a few catalog lookups.
        if let Some(old) = &self.env {
            if old.snapshot().ptr_eq(&snap)
                && old.current_user() == user
                && old.session_user() == session_user
                && old.database() == database
                && old.row_security() == row_security
            {
                let candidate = CatalogEnv::new(snap.clone(), &sp, user)?;
                if candidate.search_path() == old.search_path()
                    && candidate.creation_namespace().ok() == old.creation_namespace().ok()
                {
                    return Ok(old.clone());
                }
            }
        }

        // Step 3b: rebuild. A changed snapshot marks the prepared statements for revalidation.
        if let Some(old) = &self.env {
            if !old.snapshot().ptr_eq(&snap) {
                let inv = invalidation::diff(old.snapshot(), &snap);
                if !inv.is_empty() {
                    self.prepared.note_invalidation();
                }
            }
        }
        let env = CatalogEnv::new(snap, &sp, user)?.with_session(session_user, database).with_row_security(row_security);
        self.env = Some(env.clone());
        Ok(env)
    }

    /// Transaction start time as timestamptz us since 2000-01-01 (module docs).
    pub(crate) fn txn_timestamp(&self) -> i64 {
        // Derived from the activity record; 0 when no transaction.
        self.handle
            .shared()
            .activity()
            .xact_start_ms
            .map(|ms| (ms as i64) * 1000 - PG_EPOCH_OFFSET_US)
            .unwrap_or(0)
    }

    /// `statement_timestamp()`.
    pub(crate) fn statement_timestamp(&self) -> Result<i64> {
        now_pg_us()
    }

    /// `BindContext` over `env` (module docs). The statement timestamp is the transaction start, so
    /// `now()` and the special date literals are stable inside a transaction.
    pub(crate) fn bind_context<'a>(&'a self, env: &'a CatalogEnv, declared: Vec<TypeId>) -> Result<BindContext<'a>> {
        Ok(BindContext {
            env,
            session: BindSession { database: self.database_name().to_string() },
            io_config: guc_derive::io_config(&self.guc)?,
            statement_timestamp: self.txn_timestamp(),
            declared_params: declared,
        })
    }

    /// `PlanContext` over `env` (module docs). `table_rows` stays empty (no row-count source yet,
    /// amendment plan-01); `for_explain` is passed through to the planner options.
    pub(crate) fn plan_context<'a>(
        &'a self,
        env: &'a CatalogEnv,
        param_types: Vec<TypeId>,
        param_values: Option<Vec<Datum>>,
        for_explain: bool,
    ) -> Result<PlanContext<'a>> {
        let mut options = guc_derive::plan_options(&self.guc)?;
        options.for_explain = for_explain;
        Ok(PlanContext { env, param_types, param_values, options, table_rows: HashMap::new() })
    }

    /// Run `f` with an `ExecContext` over the live transaction (module docs). `command` names the
    /// statement for 25006 errors (`INSERT`, `CREATE TABLE`, ...).
    pub(crate) fn with_exec<R>(
        &mut self,
        command: &'static str,
        env: &CatalogEnv,
        params: Vec<Datum>,
        param_types: Vec<TypeId>,
        f: impl FnOnce(&mut ExecContext) -> Result<R>,
    ) -> Result<(R, ExecSideEffects)> {
        // Everything that needs `&self` methods is computed before the split borrows below.
        let io = guc_derive::io_config(&self.guc)?;
        let statement_ts = self.statement_timestamp()?;
        let txn_ts = self.txn_timestamp();
        let database_name = self.database_name().to_string();

        let (result, notices, catalog_changed, new_env) = {
            let Session { txn, guc, rng, prepared, db, handle, listen, pending_notices, .. } = self;
            let txn = txn
                .as_mut()
                .ok_or_else(|| PgError::internal("statement execution requested without a transaction"))?;
            let mut kv = txn.kv(command);
            kv.set_no_key_update(false);

            let services = SessionServices {
                db: &*db,
                shared: handle.shared(),
                prepared: &*prepared,
                listen: &*listen,
                database_name,
            };
            let mut se = SessionEnv::new(env.clone(), io, &*guc, &services, &*rng, statement_ts, txn_ts);
            se.params = params;
            se.param_types = param_types;

            let mut cx = ExecContext::new(&mut kv, se);
            let result = match f(&mut cx) {
                Ok(r) => r,
                Err(e) => {
                    // Notices raised before the error (e.g. RAISE NOTICE in a function) are still sent.
                    pending_notices.extend(cx.se.take_notices());
                    return Err(e);
                }
            };
            let notices = cx.se.take_notices();
            let catalog_changed = cx.stmt.catalog_changed;
            let new_env = if catalog_changed { Some(cx.se.env.clone()) } else { None };
            (result, notices, catalog_changed, new_env)
        };

        if let Some(env) = new_env {
            self.env = Some(env);
        }
        Ok((result, ExecSideEffects { notices, catalog_changed }))
    }

    /// `ColumnInfo`s for result descriptors (module docs).
    pub(crate) fn column_infos(&self, env: &CatalogEnv, cols: &[ColumnDesc]) -> Result<Vec<ColumnInfo>> {
        columns_to_infos(env, cols)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::database::Database;
    use crate::types::SessionOptions;

    fn col(name: &str, oid: u32, table_oid: u32, attnum: i16) -> ColumnDesc {
        ColumnDesc {
            name: name.to_string(),
            type_id: TypeId { oid, typmod: -1 },
            table_oid,
            column_attnum: attnum,
        }
    }

    #[test]
    fn column_infos_carry_type_len_and_table_attnum() {
        let (_, env) = pgarbor_catalog::testing::bootstrapped().expect("catalog bootstrap");
        let cols = vec![col("id", 23, 16384, 1), col("name", 25, 16384, 2), col("expr", 23, 0, 0)];
        let infos = columns_to_infos(&env, &cols).expect("column infos");
        assert_eq!(infos.len(), 3);
        assert_eq!(infos[0].name, "id");
        assert_eq!(infos[0].type_oid, 23);
        assert_eq!(infos[0].type_len, 4);
        assert_eq!(infos[0].table_oid, 16384);
        assert_eq!(infos[0].column_attnum, 1);
        assert_eq!(infos[1].type_len, -1);
        assert_eq!(infos[1].column_attnum, 2);
        assert_eq!(infos[2].table_oid, 0);
        assert_eq!(infos[2].column_attnum, 0);
        assert_eq!(infos[2].type_mod, -1);
    }

    #[test]
    fn domain_columns_are_described_by_base_type_and_typmod() {
        // information_schema.sql_identifier (13312) is a domain over name; yes_or_no (13320) is a
        // domain over varchar(3), so its typmod is 3 + VARHDRSZ.
        let (_, env) = pgarbor_catalog::testing::bootstrapped().expect("catalog bootstrap");
        let cols = vec![col("schema_name", 13312, 13467, 2), col("is_nullable", 13320, 13381, 7)];
        let infos = columns_to_infos(&env, &cols).expect("column infos");
        assert_eq!(infos[0].type_oid, 19);
        assert_eq!(infos[0].type_mod, -1);
        assert_eq!(infos[1].type_oid, 1043);
        assert_eq!(infos[1].type_mod, 7);
    }

    #[test]
    fn unknown_type_oid_is_internal_error() {
        let (_, env) = pgarbor_catalog::testing::bootstrapped().expect("catalog bootstrap");
        let err = columns_to_infos(&env, &[col("x", 4_000_000_000, 0, 0)]).unwrap_err();
        assert_eq!(err.sqlstate.code(), "XX000");
    }

    #[test]
    fn statement_clock_is_after_2020_in_pg_epoch() {
        // 2020-01-01 in timestamptz microseconds since 2000-01-01.
        let now = now_pg_us().expect("clock");
        assert!(now > 631_152_000_000_000);
    }

    #[test]
    #[ignore = "needs API-04, API-05"]
    fn refresh_env_without_transaction_fails_xx000() {
        let db = Database::in_memory().expect("in-memory database");
        let mut s = db.session("main", SessionOptions::default()).expect("session");
        let err = s.refresh_env().unwrap_err();
        assert_eq!(err.sqlstate.code(), "XX000");
    }
}
