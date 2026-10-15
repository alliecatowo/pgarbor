//! CREATE FUNCTION / PROCEDURE and CALL / DO (SQL-37). PG references: `functioncmds.c`
//! (`CreateFunction`, `compute_function_attributes`, `interpret_function_parameter_list`,
//! `ExecuteCallStmt`, `ExecuteDoStmt`), `gram.y` (`CreateFunctionStmt`, `func_args`, `createfunc_opt_item`).
//!
//! PGArbor stores function definitions in the catalog (v1) but cannot execute user-defined
//! function bodies yet (later work: SQL-language and PL/pgSQL). The binder still validates and
//! records them so ORMs/migration tools that create functions succeed and `\df`/`pg_proc` work.
//!
//! `bind_create_function(b, stmt)` (`Statement::CreateFunction(CreateFunction { or_replace, name, args,
//! return_type, function_body, behavior, called_on_null, parallel, language, determinism_specifier, ..
//! })` and `Statement::CreateProcedure { or_alter, name, params, body, .. }`) ->
//! `BoundStatement::CreateFunction(BoundCreateFunction { function: Function{oid: 0, ..}, or_replace })`:
//! * Parameters: mode (`IN` default, `OUT`, `INOUT`, `VARIADIC`), optional name, type, `DEFAULT expr`
//!   (bound with `ExprCtx::column_default()` and stored as canonical text); `RETURNS type`,
//!   `RETURNS TABLE(..)`, `RETURNS SETOF type`, `RETURNS void`; OUT parameters determine the return type
//!   (one OUT -> its type; several -> `record`). 42P13 `function result type must be X because of OUT
//!   parameters`, `only the last parameter can be variadic`, `input parameters after one with a default
//!   value must also have defaults`, 42P13 `procedures cannot have OUT arguments`? (PG 14+ allows OUT for
//!   procedures; INOUT too).
//! * Body: `AS 'text'` / `AS $$text$$` -> `body`; `LANGUAGE sql|plpgsql|c|internal`: `sql` (language OID
//!   14) and `plpgsql` (13647?? use `builtin::LANGUAGES` lookup by name) accepted; others 42704 `language
//!   "x" does not exist` (or 0A000 for `c`/`internal` user functions); `BEGIN ATOMIC ... END` bodies and
//!   `RETURN expr` -> `sql_body: Some(StoredExpr)` (deparsed) when binding succeeds.
//!   SQL-language bodies are syntax-checked lazily (`check_function_bodies` is `on` in PG: a SQL body is
//!   parsed/analyzed at creation; PGArbor v1 parses it with `parser::parse` and reports 42601 syntax errors,
//!   but does not bind it).
//! * Attributes: `IMMUTABLE|STABLE|VOLATILE` (default volatile), `[NOT] LEAKPROOF`, `STRICT|RETURNS NULL
//!   ON NULL INPUT|CALLED ON NULL INPUT`, `SECURITY DEFINER|INVOKER`, `PARALLEL UNSAFE|RESTRICTED|SAFE`,
//!   `COST n`, `ROWS n` (SRFs only), `SET guc = v` -> `config`, `WINDOW`, `TRANSFORM`: 0A000.
//! * `kind`: `FuncKind::Function` or `Procedure` (CREATE PROCEDURE; no RETURNS).
//! * Namespace and owner like tables. Dependencies: argument/return types.
//!
//! `bind_call(b, stmt)` (`Statement::Call(FunctionCall)`): resolve a PROCEDURE by name + argument types
//! (`resolve_function` with kind Procedure; 42883 `procedure p(integer) does not exist`; a function
//! called with CALL: 42809 `f(integer) is not a procedure` HINT `To call a function, use SELECT.`) ->
//! `BoundStatement::Call(BoundCall{procedure, name, args})`, args coerced; OUT args as NULL placeholders.
//! `bind_do(b, ..)`: `DO [LANGUAGE l] 'code'` -> `BoundStatement::Do(BoundDo{language, code})`
//! (`language` defaults to `plpgsql`; unknown language 42704).
use pgarbor_common::{PgError, Result};
use sqlparser::ast::Statement;

use super::context::Binder;
use crate::ir::BoundStatement;

/// CREATE FUNCTION / PROCEDURE.
#[allow(unused_variables)]
pub fn bind_create_function(b: &mut Binder, stmt: &Statement) -> Result<BoundStatement> {
    Err(PgError::unimplemented("SQL-37", "CREATE FUNCTION binding (ddl_function::bind_create_function)"))
}

/// CALL.
#[allow(unused_variables)]
pub fn bind_call(b: &mut Binder, stmt: &Statement) -> Result<BoundStatement> {
    Err(PgError::unimplemented("SQL-37", "CALL binding (ddl_function::bind_call)"))
}

/// DO (from `ExtraStatement::Do`).
#[allow(unused_variables)]
pub fn bind_do(b: &mut Binder, language: Option<&sqlparser::ast::Ident>, code: &str) -> Result<BoundStatement> {
    Err(PgError::unimplemented("SQL-37", "DO binding (ddl_function::bind_do)"))
}
