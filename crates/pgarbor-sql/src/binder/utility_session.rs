//! Transaction control, SET / RESET / SHOW, DISCARD (SQL-40). PG references: `gram.y`
//! (`TransactionStmt`, `VariableSetStmt`, `VariableShowStmt`, `VariableResetStmt`, `DiscardStmt`),
//! `utility.c`, `guc.c` (`ExtractSetVariableArgs`, `flatten_set_variable_args`), `xact.c`.
//!
//! These statements NEVER touch the catalog (they must also work in an aborted transaction).
//!
//! ## Transactions (`TransactionStmt`)
//! * `BEGIN [WORK|TRANSACTION] [modes]`, `START TRANSACTION [modes]` -> `Begin(TxnModes)`; modes:
//!   `ISOLATION LEVEL {READ UNCOMMITTED|READ COMMITTED|REPEATABLE READ|SERIALIZABLE}`, `READ ONLY|READ
//!   WRITE`, `[NOT] DEFERRABLE`, comma separated; sqlparser: `Statement::StartTransaction { modes:
//!   Vec<TransactionMode>, begin, transaction, modifier, statements, exception, has_end_keyword }`
//!   (non-PG modifiers `DEFERRED/IMMEDIATE/EXCLUSIVE`, `TRY` -> 42601).
//! * `COMMIT [WORK|TRANSACTION] [AND [NO] CHAIN]`, `END` -> `Commit{chain}`; `ROLLBACK/ABORT [AND [NO]
//!   CHAIN]` -> `Rollback{chain}`; `ROLLBACK [WORK] TO [SAVEPOINT] n` -> `RollbackToSavepoint(n)`;
//!   `SAVEPOINT n` -> `Savepoint(n)`; `RELEASE [SAVEPOINT] n` -> `ReleaseSavepoint(n)`. Savepoint names
//!   folded as identifiers. `COMMIT PREPARED`/`PREPARE TRANSACTION` are `ExtraStatement::Unsupported`.
//! * `SET TRANSACTION modes` -> `SetTransaction(TxnModes)`; `SET TRANSACTION SNAPSHOT 'id'` 0A000;
//!   `SET SESSION CHARACTERISTICS AS TRANSACTION modes` -> `SetSessionCharacteristics`.
//!
//! ## SET (`Statement::Set(Set::...)`)
//! * `SET [SESSION|LOCAL] name {TO|=} v1[, v2...]` (`Set::SingleAssignment { scope, variable, values }`)
//!   -> `Set(SetStmt { scope, name, value })`. `name` = qualified GUC name (`app.setting`) lower-cased;
//!   each value rendered per `flatten_set_variable_args`: string literal -> its contents; identifier ->
//!   as written folded? (PG keeps the identifier text with case as written for `SET x = Foo`? -> it stores
//!   `Foo`; quoted identifiers keep case, unquoted are folded by the lexer), numeric literal -> the literal
//!   text, `TRUE|FALSE|ON|OFF` -> `on`/`off`/`true`/`false` words as written, `DEFAULT` -> `SetValue::Default`,
//!   `SET x FROM CURRENT` -> `FromCurrent`. Multiple values form a list (`SET search_path = a, "B"` ->
//!   `["a","B"]`). `SET x TO` with no value: 42601.
//! * Special forms normalized: `SET TIME ZONE v` (`Set::SetTimeZone { local, value }`) -> `timezone`
//!   (`LOCAL`/`DEFAULT` -> Default; `INTERVAL '1 hour'`, numbers -> text), `SET NAMES 'x'` ->
//!   `client_encoding`, `SET SCHEMA 'x'` -> `search_path`, `SET SESSION AUTHORIZATION u|DEFAULT` ->
//!   `SetSessionAuthorization`, `SET [LOCAL|SESSION] ROLE r|NONE` -> `SetRole`, `RESET ROLE` ->
//!   `SetRole{None}`, `RESET SESSION AUTHORIZATION` -> `SetSessionAuthorization{None}`.
//! * `RESET name|ALL` -> `Reset(...)`. Unknown GUC names are validated by the SESSION at execution
//!   (42704 `unrecognized configuration parameter "x"`), not here, except placeholder names `a.b` which
//!   are always allowed. Non-PG forms (`Set::MultipleAssignments`, `ParenthesizedAssignments`,
//!   `SetSessionParam`, hivevar, `SET @x`): 42601.
//! * `SET CONSTRAINTS ALL|names DEFERRED|IMMEDIATE` (extra) -> `Noop { tag: "SET CONSTRAINTS" }` in v1 for
//!   `ALL IMMEDIATE`; `DEFERRED` is 0A000 until deferred constraints exist.
//!
//! ## SHOW (`Statement::ShowVariable { variable: Vec<Ident> }`)
//! `SHOW name` -> `Show(Some(name))` (qualified names joined with `.`, lower-cased; `SHOW ALL` ->
//! `Show(None)`; special spellings: `SHOW TIME ZONE` -> `timezone`, `SHOW TRANSACTION ISOLATION LEVEL` ->
//! `transaction_isolation`, `SHOW SESSION AUTHORIZATION` -> `session_authorization`). Unknown names are
//! the session's 42704. Other `Show*` statements are classified as foreign syntax before reaching here.
//!
//! ## DISCARD (`Statement::Discard { object_type }`) -> `Discard(DiscardKind::{All,Plans,Sequences,Temp})`
//! (`DISCARD ALL` inside a transaction block is the session's 25001).
use pgarbor_common::{PgError, Result};
use sqlparser::ast::Statement;

use super::context::Binder;
use crate::ir::BoundStatement;

/// BEGIN / START TRANSACTION / COMMIT / END / ROLLBACK / ABORT / SAVEPOINT / RELEASE.
#[allow(unused_variables)]
pub fn bind_transaction(b: &mut Binder, stmt: &Statement) -> Result<BoundStatement> {
    Err(PgError::unimplemented("SQL-40", "transaction statement binding (utility_session::bind_transaction)"))
}

/// SET (all forms, including `SET TRANSACTION`) and RESET.
#[allow(unused_variables)]
pub fn bind_set(b: &mut Binder, stmt: &Statement) -> Result<BoundStatement> {
    Err(PgError::unimplemented("SQL-40", "SET/RESET binding (utility_session::bind_set)"))
}

/// SHOW.
#[allow(unused_variables)]
pub fn bind_show(b: &mut Binder, stmt: &Statement) -> Result<BoundStatement> {
    Err(PgError::unimplemented("SQL-40", "SHOW binding (utility_session::bind_show)"))
}

/// DISCARD.
#[allow(unused_variables)]
pub fn bind_discard(b: &mut Binder, stmt: &Statement) -> Result<BoundStatement> {
    Err(PgError::unimplemented("SQL-40", "DISCARD binding (utility_session::bind_discard)"))
}

/// `SET CONSTRAINTS` (from `ExtraStatement::SetConstraints`).
#[allow(unused_variables)]
pub fn bind_set_constraints(b: &mut Binder, names: Option<&[sqlparser::ast::ObjectName]>, deferred: bool) -> Result<BoundStatement> {
    Err(PgError::unimplemented("SQL-40", "SET CONSTRAINTS binding (utility_session::bind_set_constraints)"))
}
