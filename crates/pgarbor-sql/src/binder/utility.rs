//! PREPARE / EXECUTE / DEALLOCATE, EXPLAIN, LISTEN/NOTIFY/UNLISTEN, LOCK, VACUUM/ANALYZE/REINDEX/
//! CLUSTER/CHECKPOINT (SQL-41). PG references: `prepare.c` (`PrepareQuery`, `ExecuteQuery`,
//! `EvaluateParams`), `explain.c` (`ExplainQuery`, option parsing `ExplainQuery` switch), `async.c`,
//! `lockcmds.c` (`LockTableCommand`), `vacuum.c`, `gram.y`.
//!
//! ## PREPARE (`Statement::Prepare { name, data_types, statement }`)
//! `data_types` -> declared parameter types (`types::resolve_data_type`); the inner statement is bound
//! with a FRESH parameter table seeded with those types (`BindContext::declared_params` semantic: build a
//! nested `ParamTable::new(&declared)` and swap it into the Binder while binding the inner statement via
//! `dispatch::bind_inner_statement(.., top_level = false)`; only `SELECT`, `INSERT`, `UPDATE`, `DELETE`,
//! `MERGE`, `VALUES` allowed: 42601 `syntax error at or near ...`-style `PREPARE` of other statements
//! is a grammar error). Result `BoundPrepare { name (folded), param_types (final, after inference;
//! 42P18 if unresolved), statement, text }`; `text` = the PREPARE statement's own source text
//! (what `pg_prepared_statements.statement` shows). Duplicate prepared-statement names (42P05
//! `prepared statement "x" already exists`) are the SESSION's check.
//! ## EXECUTE (`Statement::Execute { name, parameters, using, has_parentheses, immediate, into, output, default }`)
//! `parameters` bound with `ExprCtx::execute_args()` (no columns; sub-selects allowed; `$n` not allowed:
//! 42P02) -> `BoundExecute { name, args }`; coercion to the prepared statement's parameter types happens in
//! the session with [`coerce_execute_args`] (42804 `parameter $1 of type integer cannot be coerced to the
//! expected type text`, 42601? `wrong number of parameters for prepared statement "x"` 42601 DETAIL
//! `Expected 2 parameters but got 1.`; 26000 `prepared statement "x" does not exist`). `EXECUTE IMMEDIATE`,
//! `INTO`, `OUTPUT`, `DEFAULT`: 42601.
//! ## DEALLOCATE (`Statement::Deallocate { name, prepare }`) -> `Deallocate(Some(name))`; `DEALLOCATE ALL`
//! (name `ALL`) -> `Deallocate(None)`.
//!
//! ## EXPLAIN (`Statement::Explain { describe_alias, analyze, verbose, query_plan, estimate, statement,
//! format, options }`)
//! `describe_alias` DESCRIBE/DESC (MySQL) -> 42601. Options in the parenthesized list `(ANALYZE [bool],
//! VERBOSE, COSTS, SETTINGS, GENERIC_PLAN, BUFFERS, SERIALIZE, WAL, TIMING, SUMMARY, MEMORY, FORMAT
//! TEXT|XML|JSON|YAML)` and the legacy `EXPLAIN [ANALYZE] [VERBOSE] stmt`; unknown option: 42601
//! `unrecognized EXPLAIN option "x"`; `FORMAT bad`: 22023 `unrecognized value for EXPLAIN option
//! "format": "bad"` (both positioned at the option; oracle). Defaults in `ExplainOptions::default()` (`COSTS` on; `ANALYZE` implies
//! `TIMING`/`SUMMARY`/`BUFFERS` off here). The inner statement is bound with `dispatch::bind_inner_statement`
//! and must be plannable (`SELECT`, `INSERT`, `UPDATE`, `DELETE`, `VALUES`, `CREATE TABLE AS`, `EXECUTE`,
//! `DECLARE`, `REFRESH MATERIALIZED VIEW`); anything else is a grammar error 42601. Result
//! `Explain(Box<BoundExplain{options, statement}>)`.
//!
//! ## LISTEN / UNLISTEN / NOTIFY
//! `LISTEN ch` -> `Listen(ch)` (channel is an identifier, folded, max 63 bytes); `UNLISTEN ch|*` ->
//! `Unlisten`; `NOTIFY ch [, 'payload']` -> `Notify { channel, payload }` (payload must be a string literal;
//! > 7999 bytes: 22023 `payload string too long`; channel empty: 22023 `channel name cannot be empty`).
//! ## LOCK (`Statement::Lock(LockTableStmt { tables, lock_mode, nowait, only, .. })`)
//! `LOCK [TABLE] [ONLY] t,.. [IN mode MODE] [NOWAIT]` -> `Lock(BoundLock{tables, mode, nowait})`; default
//! mode ACCESS EXCLUSIVE; tables resolved (42P01), must be tables/views.
//! ## Maintenance
//! `VACUUM [(opts)|FULL|FREEZE|ANALYZE|VERBOSE] [t,..]`, `ANALYZE [t]` (`Statement::Analyze`),
//! `REINDEX ...`, `CLUSTER`, `CHECKPOINT` -> `Maintenance { kind, targets: Vec<Oid> }` (accepted, no
//! physical effect; targets resolved so missing tables still error 42P01). `VACUUM` cannot run inside
//! a transaction block (25001, session).
use pgarbor_common::{PgError, Result};
use pgarbor_types::TypeId;
use sqlparser::ast::Statement;

use super::context::Binder;
use crate::extra_stmt::ExtraStatement;
use crate::ir::{BoundExpr, BoundStatement};

/// PREPARE.
#[allow(unused_variables)]
pub fn bind_prepare(b: &mut Binder, stmt: &Statement, stmt_text: &str) -> Result<BoundStatement> {
    Err(PgError::unimplemented("SQL-41", "PREPARE binding (utility::bind_prepare)"))
}

/// EXECUTE.
#[allow(unused_variables)]
pub fn bind_execute(b: &mut Binder, stmt: &Statement) -> Result<BoundStatement> {
    Err(PgError::unimplemented("SQL-41", "EXECUTE binding (utility::bind_execute)"))
}

/// DEALLOCATE.
#[allow(unused_variables)]
pub fn bind_deallocate(b: &mut Binder, stmt: &Statement) -> Result<BoundStatement> {
    Err(PgError::unimplemented("SQL-41", "DEALLOCATE binding (utility::bind_deallocate)"))
}

/// EXPLAIN.
#[allow(unused_variables)]
pub fn bind_explain(b: &mut Binder, stmt: &Statement) -> Result<BoundStatement> {
    Err(PgError::unimplemented("SQL-41", "EXPLAIN binding (utility::bind_explain)"))
}

/// LISTEN / UNLISTEN / NOTIFY.
#[allow(unused_variables)]
pub fn bind_notify(b: &mut Binder, stmt: &Statement) -> Result<BoundStatement> {
    Err(PgError::unimplemented("SQL-41", "LISTEN/NOTIFY binding (utility::bind_notify)"))
}

/// LOCK TABLE.
#[allow(unused_variables)]
pub fn bind_lock(b: &mut Binder, stmt: &Statement) -> Result<BoundStatement> {
    Err(PgError::unimplemented("SQL-41", "LOCK binding (utility::bind_lock)"))
}

/// VACUUM / ANALYZE and the extras REINDEX / CLUSTER / CHECKPOINT.
#[allow(unused_variables)]
pub fn bind_maintenance(b: &mut Binder, stmt: Option<&Statement>, extra: Option<&ExtraStatement>) -> Result<BoundStatement> {
    Err(PgError::unimplemented("SQL-41", "maintenance statement binding (utility::bind_maintenance)"))
}

/// Coerce EXECUTE arguments to the prepared statement's parameter types (used by the session, which
/// owns the prepared statement table). Each arg: assignment-context coercion (PG `EvaluateParams` uses
/// `coerce_to_target_type` with `COERCION_ASSIGNMENT`); wrong count -> 42601 `wrong number of parameters
/// for prepared statement "name"` DETAIL `Expected N parameters but got M.`; type failure -> 42804
/// `parameter $1 of type integer cannot be coerced to the expected type text` HINT `You will need to
/// rewrite or cast the expression.` Pure over `BoundExpr` + the catalog env; no Binder state: it builds
/// a throwaway `Binder`.
#[allow(unused_variables)]
pub fn coerce_execute_args(
    ctx: &super::context::BindContext,
    src: &crate::source::SourceMap,
    name: &str,
    args: Vec<BoundExpr>,
    param_types: &[TypeId],
) -> Result<Vec<BoundExpr>> {
    Err(PgError::unimplemented("SQL-41", "EXECUTE argument coercion (utility::coerce_execute_args)"))
}
