//! DROP, TRUNCATE, COMMENT ON, DROP OWNED, REASSIGN OWNED, DROP DATABASE (SQL-38).
//! PG references: `dropcmds.c` (`RemoveObjects`, `owningrel_does_not_exist_skipping`,
//! `does_not_exist_skipping` — the NOTICE texts), `tablecmds.c` (`ExecuteTruncate`, `truncate_check_rel`),
//! `comment.c` (`CommentObject`), `user.c` (`DropOwnedObjects`, `ReassignOwnedObjects`),
//! `dbcommands.c` (`dropdb`), `gram.y` (`DropStmt`).
//!
//! **DROP carries NAMES, not OIDs** (`BoundDrop { kind, targets: Vec<DropTarget>, if_exists, cascade }`):
//! the catalog's `CatalogTxn::drop_by_names` resolves them because NOTICE/ERROR wording depends on the
//! lookup outcome. The binder only converts syntax:
//! * `Statement::Drop { object_type, if_exists, names, cascade, restrict, purge, temporary, table }`:
//!   `ObjectType::Table|View|MaterializedView|Index|Sequence|Type|Schema|Role|User|Database|...` ->
//!   `DropKind` (`User` -> `Role`; `Database` -> `BoundStatement::DropDatabase`; `Collation|Stage|Stream|
//!   Warehouse|...` -> 42601/0A000 as appropriate; `PURGE`, `TEMPORARY` 42601). `CASCADE` + `RESTRICT`
//!   together: 42601. Targets: `DropTarget { schema, name, args: None }` via `Binder::qual`
//!   (3-part names with the wrong database 0A000 `cross-database references are not implemented`).
//!   `DROP INDEX CONCURRENTLY` flag ignored. `DROP TABLE a, b` -> two targets.
//! * `DropFunction { if_exists, func_desc: Vec<FunctionDesc>, drop_behavior }`, `DropProcedure`,
//!   `DropDomain { if_exists, name, drop_behavior }`: functions/procedures carry `args: Some(Vec<Oid>)`
//!   resolved from the written argument types (`types::resolve_data_type`; arg modes OUT/INOUT handled;
//!   omitted arg list -> `args: None` = "name must be unique"); domains `DropKind::Domain`; `DROP
//!   ROUTINE`/`AGGREGATE` similar.
//! * `DROP SCHEMA s CASCADE` etc. are plain targets. `DROP OWNED BY r` -> `DropOwned`;
//!   `REASSIGN OWNED BY a TO b` -> `ReassignOwned` (roles resolved with `env.role_oid`, 42704 `role "x"
//!   does not exist`).
//! * `DROP DATABASE [IF EXISTS] n [WITH (FORCE)]` -> `DropDatabase { name, if_exists, force }` (running
//!   inside a transaction block is the session's 25001).
//!
//! ## TRUNCATE (`Statement::Truncate(Truncate { table_names, partitions, table, identity, cascade, on_cluster
//! })`) -> `BoundTruncate { tables, restart_identity, cascade }`
//! `table_names: Vec<TruncateTableTarget { name, only }>`; resolve each with `env.resolve_relation`
//! (42P01); must be tables: 42809 `"v" is not a table`; `RESTART IDENTITY`/`CONTINUE IDENTITY`;
//! `CASCADE`/`RESTRICT`; partitions/`ON CLUSTER`: 42601. Duplicates removed (PG truncates each once).
//!
//! ## COMMENT (`Statement::Comment { object_type, object_name, comment, if_exists }`)
//! -> `BoundStatement::Comment { addr: ObjectAddress, text: Option<String> }` (`IS NULL` removes).
//! Supported object types: table, view, materialized view, index, sequence, column (`COMMENT ON COLUMN
//! t.c`: addr = `ObjectAddress{class: Relation, oid, sub_id: attnum}`), schema, type, domain, function,
//! constraint (`ON constraint ON table`: sqlparser exposes `CommentObject::Constraint`? else 0A000),
//! role, database, extension (0A000). Missing object: 42P01/42704/3F000 with the normal error; IF EXISTS
//! on missing -> `Noop` with NOTICE `relation "x" does not exist, skipping`.
use pgarbor_common::{PgError, Result};
use sqlparser::ast::Statement;

use super::context::Binder;
use crate::extra_stmt::ExtraStatement;
use crate::ir::BoundStatement;

/// `Statement::Drop`, `DropFunction`, `DropProcedure`, `DropDomain`.
#[allow(unused_variables)]
pub fn bind_drop(b: &mut Binder, stmt: &Statement) -> Result<BoundStatement> {
    Err(PgError::unimplemented("SQL-38", "DROP binding (ddl_drop::bind_drop)"))
}

/// `Statement::Truncate`.
#[allow(unused_variables)]
pub fn bind_truncate(b: &mut Binder, stmt: &Statement) -> Result<BoundStatement> {
    Err(PgError::unimplemented("SQL-38", "TRUNCATE binding (ddl_drop::bind_truncate)"))
}

/// `Statement::Comment`.
#[allow(unused_variables)]
pub fn bind_comment(b: &mut Binder, stmt: &Statement) -> Result<BoundStatement> {
    Err(PgError::unimplemented("SQL-38", "COMMENT binding (ddl_drop::bind_comment)"))
}

/// `ExtraStatement::DropOwned` / `ReassignOwned`.
#[allow(unused_variables)]
pub fn bind_owned(b: &mut Binder, stmt: &ExtraStatement) -> Result<BoundStatement> {
    Err(PgError::unimplemented("SQL-38", "DROP/REASSIGN OWNED binding (ddl_drop::bind_owned)"))
}
