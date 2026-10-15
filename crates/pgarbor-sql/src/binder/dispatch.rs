//! Statement dispatch: `bind_statement` and the family routing table (SQL-04).
//!
//! ## `bind_statement(stmt, ctx)`
//! 1. `Binder::new(ctx, &stmt.source)`.
//! 2. `classify::check_supported(&stmt.parsed, first_token, Some(stmt.pos))` -> family (errors:
//!    0A000 for recognized-unsupported, 42601 for foreign syntax; normally already raised at parse
//!    time, repeated here for statements constructed by other paths such as re-binding view text).
//! 3. Route by `StmtFamily` / `Statement` variant to the owning module (table below) via
//!    [`bind_inner_statement`].
//! 4. `params::finalize(&mut binder)` -> `param_types` (42P18 if unresolved).
//! 5. Return `BindOutput { statement, param_types, notices: binder.notices }`.
//! A `Statement::Query` with no rows to return and `INTO` is `CreateTableAs` (`SELECT .. INTO t`).
//!
//! ## Routing table
//! | statement | function |
//! |---|---|
//! | `Query` (and `SELECT INTO`) | `select::bind_query_statement` |
//! | `Insert` | `insert::bind_insert` |
//! | `Update` / `Delete` | `update_delete::bind_update` / `bind_delete` |
//! | `CreateTable` (incl. `AS query`, `LIKE`, `PARTITION BY`: 0A000) | `ddl_table::bind_create_table` |
//! | `CreateIndex` | `ddl_index::bind_create_index` |
//! | `CreateView`, extra `RefreshMaterializedView` | `ddl_view::bind_create_view` / `bind_refresh_matview` |
//! | `CreateSchema`/`CreateSequence`/`CreateType`/`CreateDomain`/`CreateRole`/`CreateUser`/`CreateDatabase` | `ddl_objects::*` |
//! | `CreateFunction`/`CreateProcedure`, `Call`, extra `Do` | `ddl_function::*` |
//! | `AlterTable` | `ddl_alter::bind_alter_table`; `AlterView`/`AlterIndex` -> `bind_alter_view`/`bind_alter_index` |
//! | `AlterSchema`/`AlterFunction`/`AlterType`/`AlterRole`/`AlterUser`, extras `AlterSequence`/`AlterDomain`/`AlterDatabase`/`AlterMaterializedView` | `ddl_alter_other::*` |
//! | `Drop`/`DropFunction`/`DropDomain`/`DropProcedure`, `Truncate`, `Comment`, extra `DropOwned`/`ReassignOwned` | `ddl_drop::*` |
//! | `Grant`/`Revoke` | `ddl_grant::bind_grant` |
//! | `StartTransaction`/`Commit`/`Rollback`/`Savepoint`/`ReleaseSavepoint`, `Set`/`Reset`, `ShowVariable`, `Discard`, extra `SetConstraints` | `utility_session::*` |
//! | `Prepare`/`Execute`/`Deallocate`/`Explain`/`LISTEN`/`UNLISTEN`/`NOTIFY`/`Lock`/`Vacuum`/`Analyze`, extras `Reindex`/`Cluster`/`Checkpoint` | `utility::*` |
//! | `Copy` | `copy::bind_copy` |
//!
//! `BEGIN`/`COMMIT`/`ROLLBACK`/`SAVEPOINT` and friends are bound WITHOUT touching the catalog
//! (the session must be able to run them in a failed transaction).
//!
//! [`bind_inner_statement`] is also used by `EXPLAIN`, `PREPARE`, data-modifying CTEs and
//! `CREATE TABLE AS` for their nested statement; `top_level` is false there.
use pgarbor_common::{PgError, Result};
use sqlparser::ast::Statement;

use super::context::{BindContext, BindOutput, Binder};
use crate::ir::BoundStatement;
use crate::parser::RawStatement;

/// Bind one statement (module docs).
#[allow(unused_variables)]
pub fn bind_statement(stmt: &RawStatement, ctx: &BindContext) -> Result<BindOutput> {
    Err(PgError::unimplemented("SQL-04", "statement binding entry (dispatch::bind_statement)"))
}

/// Route an AST statement to its binder (routing table). `top_level` = it is the statement the
/// client submitted (matters for data-modifying CTE placement and transaction-control restrictions).
#[allow(unused_variables)]
pub fn bind_inner_statement(b: &mut Binder, stmt: &Statement, top_level: bool) -> Result<BoundStatement> {
    Err(PgError::unimplemented("SQL-04", "statement routing (dispatch::bind_inner_statement)"))
}

/// Route an `ExtraStatement`.
#[allow(unused_variables)]
pub fn bind_extra_statement(b: &mut Binder, stmt: &crate::extra_stmt::ExtraStatement) -> Result<BoundStatement> {
    Err(PgError::unimplemented("SQL-04", "extra statement routing (dispatch::bind_extra_statement)"))
}
