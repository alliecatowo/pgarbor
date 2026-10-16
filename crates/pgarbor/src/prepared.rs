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

use pgarbor_catalog::{invalidation, CatalogEnv, CatalogSnapshot, CatalogVersion};
use pgarbor_common::{Oid, PgError, Result, SqlState};
use pgarbor_exec::execute::NoCopyChannel;
use pgarbor_exec::ExecResult;
use pgarbor_plan::plan::{ColumnDesc, Plan};
use pgarbor_sql::ir::{BoundExecute, BoundPrepare, BoundStatement};
use pgarbor_sql::{RawStatement, SourceMap};
use pgarbor_types::{Datum, TypeId};

use crate::run_stmt::ParamValues;
use crate::session::Session;
use crate::txn_block::BlockState;
use crate::types::{ColumnInfo, StatementDescription, StatementResult};

/// PG `plancache.c` `choose_custom_plan`: auto mode builds custom plans for the first 5 executions.
const CUSTOM_PLAN_LIMIT: i64 = 5;

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
    /// Catalog snapshot the statement was last bound against: the old side of
    /// `invalidation::diff` in `ensure_current`. Maintained only by this module.
    pub(crate) bound_snapshot: Option<CatalogSnapshot>,
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

/// 26000 `prepared statement "x" does not exist`.
fn missing_statement(name: &str) -> PgError {
    PgError::new(SqlState::INVALID_SQL_STATEMENT_NAME, format!("prepared statement \"{name}\" does not exist"))
}

/// 42P05 `prepared statement "x" already exists`.
fn duplicate_statement(name: &str) -> PgError {
    PgError::new(SqlState::DUPLICATE_PREPARED_STATEMENT, format!("prepared statement \"{name}\" already exists"))
}

/// Parse the text of a statement to prepare: none = empty query (`None`), one = `Some`, more = 42601.
fn parse_prepared_text(text: &str) -> Result<Option<RawStatement>> {
    let mut stmts = pgarbor_sql::parse(text)?;
    if stmts.len() > 1 {
        return Err(PgError::new(SqlState::SYNTAX_ERROR, "cannot insert multiple commands into a prepared statement"));
    }
    Ok(stmts.pop())
}

/// Plan choice (module docs): `plan_cache_mode` `force_*` override; in auto mode a statement without
/// parameters always gets the generic plan (PG `choose_custom_plan`), otherwise custom plans are built
/// until `CUSTOM_PLAN_LIMIT` of them exist.
pub(crate) fn wants_generic(mode: &str, custom_plans: i64, param_count: usize) -> bool {
    match mode {
        "force_generic_plan" => true,
        "force_custom_plan" => false,
        _ => param_count == 0 || custom_plans >= CUSTOM_PLAN_LIMIT,
    }
}

/// True when a client that already received a RowDescription would see a different row shape
/// (PG `equalRowTypes`: column count, type and typmod).
fn row_type_changed(old: &[ColumnInfo], new: &[ColumnInfo]) -> bool {
    old.len() != new.len()
        || old.iter().zip(new.iter()).any(|(o, n)| o.type_oid != n.type_oid || o.type_mod != n.type_mod)
}

/// True when `ps` must be re-bound against the new snapshot. Without a bound snapshot, or without a cached
/// generic plan, every catalog change counts (the all-true dependency set); otherwise `plan_is_stale`.
fn needs_rebind(ps: &PreparedStatement, new: &CatalogSnapshot) -> bool {
    let old = match ps.bound_snapshot.as_ref() {
        Some(old) => old,
        None => return true,
    };
    let inv = invalidation::diff(old, new);
    if inv.is_empty() {
        return false;
    }
    match ps.generic_plan.as_deref() {
        Some(plan) => invalidation::plan_is_stale(&plan.deps, &inv, old, new),
        None => true,
    }
}

/// The static command name used in 25006 messages for an executed statement.
fn command_name(stmt: &BoundStatement) -> &'static str {
    match stmt {
        BoundStatement::Query(_) => "SELECT",
        BoundStatement::Insert(_) => "INSERT",
        BoundStatement::Update(_) => "UPDATE",
        BoundStatement::Delete(_) => "DELETE",
        BoundStatement::Merge(_) => "MERGE",
        BoundStatement::CreateTableAs(_) => "CREATE TABLE AS",
        _ => "EXECUTE",
    }
}

/// Outcome of re-binding a statement after a catalog change.
struct Rebound {
    bound: BoundStatement,
    param_types: Vec<TypeId>,
    columns: Option<Vec<ColumnInfo>>,
}

impl Session {
    /// Parse, bind and store a statement under `name` (module docs, "Parse-time work"). Replaces the
    /// unnamed statement silently; a duplicate NAMED statement is 42P05.
    pub(crate) fn prepare_statement(&mut self, name: &str, text: &str, declared: &[TypeId], from_sql: bool) -> Result<()> {
        if !name.is_empty() && self.prepared.get(name).is_some() {
            return Err(duplicate_statement(name));
        }
        let raw = parse_prepared_text(text)?;
        if let Some(txn) = self.txn.as_mut() {
            txn.begin_statement()?;
        }
        let env = self.refresh_env()?;
        let (bound, param_types, columns, notices) = match raw.as_ref() {
            Some(r) => {
                let out = {
                    let ctx = self.bind_context(&env, declared.to_vec())?;
                    pgarbor_sql::bind(r, &ctx)?
                };
                let columns = self.describe_columns(&env, &out.statement)?;
                (Some(out.statement), out.param_types, columns, out.notices)
            }
            None => (None, Vec::new(), None, Vec::new()),
        };
        self.pending_notices.extend(notices);
        let catalog_version = env.snapshot().version();
        let bound_snapshot = Some(env.snapshot().clone());
        let prepare_time = self.statement_timestamp()?;
        self.prepared.put(PreparedStatement {
            name: name.to_string(),
            text: text.to_string(),
            raw,
            declared: declared.to_vec(),
            param_types,
            columns,
            bound,
            catalog_version,
            generic_plan: None,
            from_sql,
            prepare_time,
            generic_plans: 0,
            custom_plans: 0,
            bound_snapshot,
        });
        Ok(())
    }

    /// Revalidate/re-bind `name` after catalog changes and return a clone of the current statement
    /// (module docs, "Revalidation").
    pub(crate) fn ensure_current(&mut self, name: &str) -> Result<PreparedStatement> {
        let env = self.refresh_env()?;
        let snap = env.snapshot();
        let new_version = snap.version();
        let rebound: Option<Rebound> = {
            let ps = self.prepared.get(name).ok_or_else(|| missing_statement(name))?;
            let version_changed = ps.catalog_version != new_version;
            match ps.raw.as_ref() {
                Some(raw) if version_changed && needs_rebind(ps, snap) => {
                    let mut out = {
                        let ctx = self.bind_context(&env, ps.declared.clone())?;
                        pgarbor_sql::bind(raw, &ctx)?
                    };
                    // SQL PREPARE keeps the whole `PREPARE ... AS` text as `raw`: unwrap the inner statement.
                    if ps.from_sql {
                        if let BoundStatement::Prepare(p) = out.statement {
                            out.param_types = p.param_types;
                            out.statement = *p.statement;
                        }
                    }
                    let columns = self.describe_columns(&env, &out.statement)?;
                    // PG: both PREPARE and protocol Parse create plan sources with fixed_result = true.
                    if row_type_changed(
                        ps.columns.as_deref().unwrap_or(&[]),
                        columns.as_deref().unwrap_or(&[]),
                    ) {
                        return Err(PgError::new(
                            SqlState::FEATURE_NOT_SUPPORTED,
                            "cached plan must not change result type",
                        ));
                    }
                    Some(Rebound { bound: out.statement, param_types: out.param_types, columns })
                }
                _ => None,
            }
        };
        let ps = self.prepared.get_mut(name).ok_or_else(|| missing_statement(name))?;
        ps.catalog_version = new_version;
        ps.bound_snapshot = Some(snap.clone());
        if let Some(r) = rebound {
            ps.bound = Some(r.bound);
            ps.param_types = r.param_types;
            ps.columns = r.columns;
            ps.generic_plan = None;
        }
        let current = ps.clone();
        self.prepared.invalidated = false;
        Ok(current)
    }

    /// The plan to run `ps` with `values` (custom vs generic, module docs); `None` for statements that
    /// are not plannable. Counts the plan kind used and caches the generic plan in the store.
    pub(crate) fn plan_for(&mut self, ps: &PreparedStatement, values: &[Datum]) -> Result<Option<Rc<Plan>>> {
        let stmt = match ps.bound.as_ref() {
            Some(s) if s.is_plannable() => s,
            _ => return Ok(None),
        };
        let mode = self.guc.show("plan_cache_mode")?;
        let generic = wants_generic(&mode, ps.custom_plans, ps.param_types.len());
        let plan = if generic {
            match ps.generic_plan.as_ref() {
                Some(cached) => Rc::clone(cached),
                None => {
                    let built = self.build_plan(stmt, ps.param_types.clone(), None)?;
                    if let Some(slot) = self.prepared.get_mut(&ps.name) {
                        slot.generic_plan = Some(Rc::clone(&built));
                    }
                    built
                }
            }
        } else {
            let param_values = if values.is_empty() { None } else { Some(values.to_vec()) };
            self.build_plan(stmt, ps.param_types.clone(), param_values)?
        };
        if let Some(slot) = self.prepared.get_mut(&ps.name) {
            if generic {
                slot.generic_plans += 1;
            } else {
                slot.custom_plans += 1;
            }
        }
        Ok(Some(plan))
    }

    /// Plan one bound statement against the current env (`None` values = generic plan).
    fn build_plan(
        &mut self,
        stmt: &BoundStatement,
        param_types: Vec<TypeId>,
        param_values: Option<Vec<Datum>>,
    ) -> Result<Rc<Plan>> {
        let env = self.refresh_env()?;
        let pc = self.plan_context(&env, param_types, param_values, false)?;
        let plan = pgarbor_plan::plan(stmt, &pc)?;
        Ok(Rc::new(plan))
    }

    /// `ColumnInfo`s of a bound statement's result (`None` = NoData).
    fn describe_columns(&self, env: &CatalogEnv, stmt: &BoundStatement) -> Result<Option<Vec<ColumnInfo>>> {
        let cols = match stmt.result_columns() {
            Some(cols) => cols,
            None => return Ok(None),
        };
        let descs: Vec<ColumnDesc> = cols
            .into_iter()
            .map(|c| {
                let (table_oid, column_attnum) = match c.origin {
                    Some(o) => (o.table, o.attnum),
                    None => (0, 0),
                };
                ColumnDesc { name: c.name, type_id: c.ty, table_oid, column_attnum }
            })
            .collect();
        Ok(Some(self.column_infos(env, &descs)?))
    }

    /// Run `f` as one top-level command: start the implicit transaction, commit it when `f` succeeds,
    /// abort it when `f` fails (embedded `prepare` / `execute_prepared`).
    fn run_in_implicit_txn<T>(&mut self, f: impl FnOnce(&mut Session) -> Result<T>) -> Result<T> {
        if self.block == BlockState::Failed {
            return Err(pgarbor_core::error::in_failed_transaction());
        }
        self.ensure_txn()?;
        match f(&mut *self) {
            Ok(value) => {
                self.finish_command()?;
                Ok(value)
            }
            Err(err) => {
                let _ = self.abort_after_error(&err);
                Err(err)
            }
        }
    }

    /// SQL `PREPARE`.
    pub(crate) fn exec_sql_prepare(&mut self, p: &BoundPrepare) -> Result<StatementResult> {
        if self.prepared.get(&p.name).is_some() {
            return Err(duplicate_statement(&p.name));
        }
        let raw = parse_prepared_text(&p.text)?;
        let env = self.refresh_env()?;
        // The binder already bound the inner statement with the final parameter types: reuse it.
        let columns = self.describe_columns(&env, &p.statement)?;
        let catalog_version = env.snapshot().version();
        let bound_snapshot = Some(env.snapshot().clone());
        let prepare_time = self.statement_timestamp()?;
        self.prepared.put(PreparedStatement {
            name: p.name.clone(),
            text: p.text.clone(),
            raw,
            declared: p.param_types.clone(),
            param_types: p.param_types.clone(),
            columns,
            bound: Some((*p.statement).clone()),
            catalog_version,
            generic_plan: None,
            from_sql: true,
            prepare_time,
            generic_plans: 0,
            custom_plans: 0,
            bound_snapshot,
        });
        Ok(StatementResult::Command { tag: "PREPARE".to_string() })
    }

    /// SQL `EXECUTE name (args)`.
    pub(crate) fn exec_sql_execute(&mut self, e: &BoundExecute) -> Result<StatementResult> {
        let ps = self.ensure_current(&e.name)?;
        let env = self.refresh_env()?;
        let src = SourceMap::new("");
        let args = {
            let ctx = self.bind_context(&env, Vec::new())?;
            pgarbor_sql::binder::utility::coerce_execute_args(
                &ctx,
                &src,
                &e.name,
                e.args.clone(),
                &ps.param_types,
            )?
        };
        let (values, effects) = self.with_exec("EXECUTE", &env, Vec::new(), ps.param_types.clone(), |cx| {
            args.iter()
                .enumerate()
                .map(|(i, arg)| {
                    pgarbor_exec::eval::eval_standalone(cx, arg, &[]).map_err(|mut err| {
                        // A failing argument (e.g. 22P02 on `'abc'` -> int) points at its source text.
                        if err.position.is_none() {
                            err.position = e.arg_positions.get(i).copied().flatten();
                        }
                        err
                    })
                })
                .collect::<Result<Vec<Datum>>>()
        })?;
        self.pending_notices.extend(effects.notices);
        self.run_prepared(&e.name, ParamValues { types: ps.param_types.clone(), values })
    }

    /// SQL `DEALLOCATE [ALL | name]` (tag `DEALLOCATE` / `DEALLOCATE ALL`).
    pub(crate) fn exec_sql_deallocate(&mut self, name: Option<&str>) -> Result<StatementResult> {
        match name {
            None => {
                // DEALLOCATE ALL drops named statements only; the unnamed one is not in PG's table.
                self.prepared.named.clear();
                Ok(StatementResult::Command { tag: "DEALLOCATE ALL".to_string() })
            }
            Some(n) => {
                if self.prepared.remove(n).is_none() {
                    return Err(missing_statement(n));
                }
                Ok(StatementResult::Command { tag: "DEALLOCATE".to_string() })
            }
        }
    }

    /// Run a prepared statement once with final parameter values and return the materialized result
    /// (used by SQL EXECUTE and the embedded `execute_prepared`).
    pub(crate) fn run_prepared(&mut self, name: &str, params: ParamValues) -> Result<StatementResult> {
        let ps = self.ensure_current(name)?;
        if params.values.len() != ps.param_types.len() {
            return Err(PgError::new(
                SqlState::PROTOCOL_VIOLATION,
                format!(
                    "bind message supplies {} parameters, but prepared statement \"{}\" requires {}",
                    params.values.len(),
                    name,
                    ps.param_types.len()
                ),
            ));
        }
        let stmt = match ps.bound.as_ref() {
            Some(s) => s,
            None => return Ok(StatementResult::Empty),
        };
        if let Some(result) = self.exec_session_statement(stmt)? {
            return Ok(result);
        }
        let plan = self.plan_for(&ps, &params.values)?;
        let env = self.refresh_env()?;
        let command = command_name(stmt);
        let mut chan = self.copy_channel.take();
        let run = self.with_exec(command, &env, params.values.clone(), ps.param_types.clone(), |cx| {
            match chan.as_deref_mut() {
                Some(c) => pgarbor_exec::execute_with_channel(stmt, plan.as_deref(), cx, c),
                None => pgarbor_exec::execute_with_channel(stmt, plan.as_deref(), cx, &mut NoCopyChannel),
            }
        });
        self.copy_channel = chan;
        let (exec_result, effects) = run?;
        self.pending_notices.extend(effects.notices);
        match exec_result {
            ExecResult::Empty => Ok(StatementResult::Empty),
            ExecResult::Command { tag } => Ok(StatementResult::Command { tag }),
            ExecResult::Rows { desc, rows, tag } => {
                let columns = self.column_infos(&env, &desc)?;
                Ok(StatementResult::Rows { columns, rows, tag })
            }
        }
    }

    /// Embedded convenience: `PREPARE` over the extended path, returning the description.
    pub fn prepare(&mut self, name: &str, sql: &str, param_types: &[Oid]) -> Result<StatementDescription> {
        let declared: Vec<TypeId> = param_types.iter().map(|oid| TypeId::of(*oid)).collect();
        self.run_in_implicit_txn(|s| {
            s.prepare_statement(name, sql, &declared, false)?;
            let ps = s.prepared.get(name).ok_or_else(|| missing_statement(name))?;
            Ok(StatementDescription {
                param_oids: ps.param_types.iter().map(|t| t.oid).collect(),
                columns: ps.columns.clone(),
            })
        })
    }

    /// Embedded convenience: execute a prepared statement with typed values.
    pub fn execute_prepared(&mut self, name: &str, params: &[Datum]) -> Result<StatementResult> {
        let values = params.to_vec();
        self.run_in_implicit_txn(|s| s.run_prepared(name, ParamValues { types: Vec::new(), values }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::database::Database;
    use crate::types::SessionOptions;

    fn stub(name: &str) -> PreparedStatement {
        PreparedStatement {
            name: name.to_string(),
            text: format!("SELECT {name}"),
            raw: None,
            declared: Vec::new(),
            param_types: Vec::new(),
            columns: None,
            bound: None,
            catalog_version: CatalogVersion::default(),
            generic_plan: None,
            from_sql: false,
            prepare_time: 0,
            generic_plans: 0,
            custom_plans: 0,
            bound_snapshot: None,
        }
    }

    fn col(type_oid: Oid, type_mod: i32) -> ColumnInfo {
        ColumnInfo {
            name: "a".to_string(),
            type_oid,
            type_mod,
            type_len: 4,
            table_oid: 0,
            column_attnum: 0,
        }
    }

    #[test]
    fn store_put_get_remove_names_clear() {
        let mut store = PreparedStore::default();
        store.put(stub("b"));
        store.put(stub("a"));
        store.put(stub(""));
        assert!(store.get("a").is_some());
        assert!(store.get("").is_some());
        assert!(store.get("zz").is_none());
        assert_eq!(store.names(), vec!["a".to_string(), "b".to_string()]);
        assert!(store.remove("a").is_some());
        assert!(store.remove("a").is_none());
        assert_eq!(store.names(), vec!["b".to_string()]);
        assert!(store.remove("").is_some());
        store.clear();
        assert!(store.names().is_empty());
        assert!(store.get("").is_none());
    }

    #[test]
    fn store_invalidation_flag() {
        let mut store = PreparedStore::default();
        assert!(!store.is_invalidated());
        store.note_invalidation();
        assert!(store.is_invalidated());
    }

    #[test]
    fn statement_errors_carry_pg_sqlstates() {
        let missing = missing_statement("p");
        assert_eq!(missing.sqlstate.code(), "26000");
        assert_eq!(missing.message, "prepared statement \"p\" does not exist");
        let dup = duplicate_statement("p");
        assert_eq!(dup.sqlstate.code(), "42P05");
        assert_eq!(dup.message, "prepared statement \"p\" already exists");
    }

    #[test]
    fn plan_choice_follows_auto_rules_and_force_settings() {
        assert!(wants_generic("auto", 0, 0));
        assert!(!wants_generic("auto", 0, 1));
        assert!(!wants_generic("auto", 4, 1));
        assert!(wants_generic("auto", 5, 1));
        assert!(!wants_generic("force_custom_plan", 9, 1));
        assert!(wants_generic("force_generic_plan", 0, 1));
    }

    #[test]
    fn row_shape_changes_are_detected() {
        let same = vec![col(23, -1)];
        assert!(!row_type_changed(&same, &[col(23, -1)]));
        assert!(row_type_changed(&same, &[]));
        assert!(row_type_changed(&same, &[col(25, -1)]));
        assert!(row_type_changed(&same, &[col(23, 4)]));
        assert!(!row_type_changed(&[], &[]));
    }

    #[test]
    fn command_names_for_read_only_messages() {
        assert_eq!(command_name(&BoundStatement::Empty), "EXECUTE");
    }

    #[test]
    #[ignore = "needs SQL-01 (parser)"]
    fn parse_prepared_text_counts_statements() {
        assert!(parse_prepared_text("").unwrap().is_none());
        assert!(parse_prepared_text("SELECT 1").unwrap().is_some());
        let err = parse_prepared_text("SELECT 1; SELECT 2").unwrap_err();
        assert_eq!(err.sqlstate.code(), "42601");
        assert_eq!(err.message, "cannot insert multiple commands into a prepared statement");
    }

    #[test]
    #[ignore = "needs SQL-01, API-06, API-08, API-09, SQL-41"]
    fn sql_prepare_execute_returns_inner_tag() {
        let db = Database::in_memory().unwrap();
        let mut s = db.session("main", SessionOptions::default()).unwrap();
        s.simple_query("PREPARE p AS SELECT $1::int").unwrap();
        let out = s.simple_query("EXECUTE p(5)").unwrap();
        let last = out.last().unwrap();
        assert_eq!(last.result.tag(), "SELECT 1");
        assert_eq!(last.result.rows().len(), 1);
    }

    #[test]
    #[ignore = "needs SQL-01, API-06, API-08, API-09, SQL-41"]
    fn duplicate_sql_prepare_is_42p05() {
        let db = Database::in_memory().unwrap();
        let mut s = db.session("main", SessionOptions::default()).unwrap();
        s.simple_query("PREPARE p AS SELECT 1").unwrap();
        let err = s.simple_query("PREPARE p AS SELECT 1").unwrap_err();
        assert_eq!(err.sqlstate.code(), "42P05");
        let err = s.simple_query("EXECUTE nope").unwrap_err();
        assert_eq!(err.sqlstate.code(), "26000");
    }
}
