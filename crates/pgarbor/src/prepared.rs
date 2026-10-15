//! Prepared statements and the plan cache (lease API-11). PG: `commands/prepare.c`,
//! `utils/cache/plancache.c`.
//!
//! ## Store
//! [`PreparedStore`] holds named statements plus the unnamed one (name `""`). Protocol-level `Parse` and
//! SQL `PREPARE` share the namespace (`from_sql` distinguishes them for `pg_prepared_statements`).
//! SQL `PREPARE name [(types)] AS stmt` creates a named statement (`prepare_sql`); DEALLOCATE removes
//! (`DEALLOCATE ALL`, `DISCARD ALL`/`PLANS` clear). Error texts: `prepared statement "x" already exists`
//! (42P05), `prepared statement "x" does not exist` (26000).
//!
//! ## Parse-time work (`prepare_statement`)
//! `parse(text)`: exactly one statement or none; more than one -> 42601 `cannot insert multiple commands
//! into a prepared statement`. Bind it NOW with `declared` types (client `paramTypes`, 0 = infer) inside
//! the current (implicit) transaction so errors surface at Parse time (42P01, 42703, 42P18...). Store:
//! final `param_types`, result `columns` (`BoundStatement::result_columns()` via `column_infos`), the
//! bound statement and the catalog version it was bound against.
//!
//! ## Revalidation and plans (`ensure_current`)
//! Called before every execution: refresh the env (`refresh_env`); if the snapshot differs from the one
//! the statement was bound against AND `catalog::invalidation::plan_is_stale(deps, &inv, old, new)` (deps =
//! the cached plan's `Plan::deps`, or all-true if no plan) re-bind from `raw` (same declared types) and drop
//! the cached plan. If the new result columns differ in count or type from the old ones:
//! 0A000 `cached plan must not change result type` (only if the client already received a
//! RowDescription, i.e. always for protocol statements; SQL PREPARE statements re-describe silently).
//! Plan choice: the first 5 executions use CUSTOM plans (`param_values = Some(values)`), then the GENERIC
//! plan (`param_values = None`, cached in `generic_plan`, rebuilt on invalidation) - simplification of
//! PG's cost comparison (`plan_cache_mode = auto`); `force_custom_plan`/`force_generic_plan` honor the GUC
//! `plan_cache_mode`. Counters feed `pg_prepared_statements.generic_plans/custom_plans`.
//!
//! ## SQL EXECUTE (`exec_sql_execute`)
//! `BoundExecute{name, args}`: look up the statement, coerce `args` to its parameter types
//! (`pgarbor_sql::binder::utility::coerce_execute_args`, wrong count: 42601 `wrong number of parameters
//! for prepared statement "x"` DETAIL `Expected 2 parameters but got 1.`), evaluate them (constant
//! expressions via `pgarbor_exec::eval::eval_standalone` in a `with_exec` context), then run through
//! the common path (`run_prepared`) and return its `StatementResult` with the INNER statement's tag
//! (`EXECUTE` itself has no tag of its own: PG returns the inner command's tag, e.g. `SELECT 1`).
use std::collections::HashMap;
use std::rc::Rc;

use pgarbor_catalog::CatalogVersion;
use pgarbor_common::{Oid, PgError, Result};
use pgarbor_plan::plan::Plan;
use pgarbor_sql::ir::{BoundExecute, BoundPrepare, BoundStatement};
use pgarbor_sql::RawStatement;
use pgarbor_types::{Datum, TypeId};

use crate::run_stmt::ParamValues;
use crate::session::Session;
use crate::types::{ColumnInfo, StatementResult};

/// One prepared statement (module docs).
// CONTRACT
#[derive(Clone, Debug)]
pub struct PreparedStatement {
    pub name: String,
    /// Statement text (what `pg_prepared_statements.statement` shows).
    pub text: String,
    /// `None` for an empty query.
    pub raw: Option<RawStatement>,
    /// Parameter types declared by the client (`unknown` / OID 0 entries are inferred).
    pub declared: Vec<TypeId>,
    /// Final parameter types.
    pub param_types: Vec<TypeId>,
    /// Result description, `None` = NoData.
    pub columns: Option<Vec<ColumnInfo>>,
    pub bound: Option<BoundStatement>,
    pub catalog_version: CatalogVersion,
    pub generic_plan: Option<Rc<Plan>>,
    pub from_sql: bool,
    /// timestamptz us when prepared.
    pub prepare_time: i64,
    pub generic_plans: i64,
    pub custom_plans: i64,
}

/// Named statements + the unnamed one.
#[derive(Debug, Default)]
pub struct PreparedStore {
    pub(crate) named: HashMap<String, PreparedStatement>,
    pub(crate) unnamed: Option<PreparedStatement>,
    /// The catalog snapshot changed since the statements were last validated (set by
    /// `ctx_build::refresh_env`, cleared by `ensure_current` after revalidation).
    pub(crate) invalidated: bool,
}

impl PreparedStore {
    pub fn get(&self, name: &str) -> Option<&PreparedStatement> {
        if name.is_empty() {
            self.unnamed.as_ref()
        } else {
            self.named.get(name)
        }
    }
    pub fn get_mut(&mut self, name: &str) -> Option<&mut PreparedStatement> {
        if name.is_empty() {
            self.unnamed.as_mut()
        } else {
            self.named.get_mut(name)
        }
    }
    /// Insert/replace; named duplicates must be rejected by the caller (42P05).
    pub fn put(&mut self, ps: PreparedStatement) {
        if ps.name.is_empty() {
            self.unnamed = Some(ps);
        } else {
            self.named.insert(ps.name.clone(), ps);
        }
    }
    pub fn remove(&mut self, name: &str) -> Option<PreparedStatement> {
        if name.is_empty() {
            self.unnamed.take()
        } else {
            self.named.remove(name)
        }
    }
    pub fn clear(&mut self) {
        self.named.clear();
        self.unnamed = None;
    }
    /// Names of named statements, sorted.
    pub fn names(&self) -> Vec<String> {
        let mut v: Vec<String> = self.named.keys().cloned().collect();
        v.sort();
        v
    }
    /// Remember that the catalog snapshot changed (`refresh_env` calls this; `ensure_current` then
    /// revalidates each statement with `plan_is_stale`).
    pub fn note_invalidation(&mut self) {
        self.invalidated = true;
    }
    /// True if a catalog change has not been revalidated yet.
    pub fn is_invalidated(&self) -> bool {
        self.invalidated
    }
    /// Drop cached generic plans (DISCARD PLANS, DDL invalidation).
    pub fn drop_plans(&mut self) {
        for ps in self.named.values_mut() {
            ps.generic_plan = None;
        }
        if let Some(u) = self.unnamed.as_mut() {
            u.generic_plan = None;
        }
    }
}

impl Session {
    /// Parse, bind and store a statement under `name` (module docs, "Parse-time work"). Replaces the
    /// unnamed statement silently; a duplicate NAMED statement is 42P05.
    #[allow(unused_variables)]
    pub(crate) fn prepare_statement(&mut self, name: &str, text: &str, declared: &[TypeId], from_sql: bool) -> Result<()> {
        Err(PgError::unimplemented("API-11", "statement preparation (Session::prepare_statement)"))
    }

    /// Revalidate/re-bind `name` after catalog changes and return a clone of the current statement
    /// (module docs, "Revalidation").
    #[allow(unused_variables)]
    pub(crate) fn ensure_current(&mut self, name: &str) -> Result<PreparedStatement> {
        Err(PgError::unimplemented("API-11", "prepared statement revalidation (Session::ensure_current)"))
    }

    /// The plan to run `ps` with `values` (custom vs generic, module docs); `None` for statements that
    /// are not plannable.
    #[allow(unused_variables)]
    pub(crate) fn plan_for(&mut self, ps: &PreparedStatement, values: &[Datum]) -> Result<Option<Rc<Plan>>> {
        Err(PgError::unimplemented("API-11", "plan selection (Session::plan_for)"))
    }

    /// SQL `PREPARE`.
    #[allow(unused_variables)]
    pub(crate) fn exec_sql_prepare(&mut self, p: &BoundPrepare) -> Result<StatementResult> {
        Err(PgError::unimplemented("API-11", "PREPARE (Session::exec_sql_prepare)"))
    }

    /// SQL `EXECUTE name (args)`.
    #[allow(unused_variables)]
    pub(crate) fn exec_sql_execute(&mut self, e: &BoundExecute) -> Result<StatementResult> {
        Err(PgError::unimplemented("API-11", "EXECUTE (Session::exec_sql_execute)"))
    }

    /// SQL `DEALLOCATE [ALL | name]` (tag `DEALLOCATE` / `DEALLOCATE ALL`).
    #[allow(unused_variables)]
    pub(crate) fn exec_sql_deallocate(&mut self, name: Option<&str>) -> Result<StatementResult> {
        Err(PgError::unimplemented("API-11", "DEALLOCATE (Session::exec_sql_deallocate)"))
    }

    /// Run a prepared statement once with final parameter values and return the materialized result
    /// (used by SQL EXECUTE and the embedded `execute_prepared`).
    #[allow(unused_variables)]
    pub(crate) fn run_prepared(&mut self, name: &str, params: ParamValues) -> Result<StatementResult> {
        Err(PgError::unimplemented("API-11", "prepared statement run (Session::run_prepared)"))
    }

    /// Embedded convenience: `PREPARE` over the extended path, returning the description.
    #[allow(unused_variables)]
    pub fn prepare(&mut self, name: &str, sql: &str, param_types: &[Oid]) -> Result<crate::types::StatementDescription> {
        Err(PgError::unimplemented("API-11", "prepare (Session::prepare)"))
    }

    /// Embedded convenience: execute a prepared statement with typed values.
    #[allow(unused_variables)]
    pub fn execute_prepared(&mut self, name: &str, params: &[Datum]) -> Result<StatementResult> {
        Err(PgError::unimplemented("API-11", "execute_prepared (Session::execute_prepared)"))
    }
}
