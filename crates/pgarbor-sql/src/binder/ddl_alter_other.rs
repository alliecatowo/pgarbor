//! ALTER for everything but table columns/constraints (SQL-34): schemas, functions, types, domains,
//! sequences, roles, databases, materialized views, ownership and schema moves (SQL-34).
//! PG references: `alter.c` (`ExecRenameStmt`, `ExecAlterObjectSchemaStmt`, `ExecAlterOwnerStmt`),
//! `sequence.c` (`AlterSequence`, `init_params`), `typecmds.c` (`AlterEnum`, `AlterDomain*`),
//! `user.c` (`AlterRole`, `AlterRoleSet`), `dbcommands.c`, `functioncmds.c` (`AlterFunction`),
//! `schemacmds.c`.
//!
//! Every function returns `BoundStatement::Alter(Box<AlterAction>)` (one variant per `CatalogTxn`
//! method, see `ir::stmt::AlterAction`) or a `Noop` with the PG notice for `IF EXISTS` on a missing object
//! (`relation "x" does not exist, skipping`, `schema "x" does not exist, skipping`, ...; severity
//! NOTICE, SQLSTATE 00000; use `pgarbor_catalog::errors` constructors when present).
//!
//! | Entry point | Statements |
//! |---|---|
//! | [`bind_alter_schema`] | `ALTER SCHEMA s RENAME TO t` -> `RenameSchema`; `OWNER TO r` -> `SchemaOwner` |
//! | [`bind_alter_function`] | `ALTER FUNCTION f(args) RENAME TO / OWNER TO / SET SCHEMA / attribute changes (IMMUTABLE|STABLE|VOLATILE, [NOT] LEAKPROOF, STRICT|CALLED ON NULL INPUT, SECURITY DEFINER|INVOKER, PARALLEL .., COST n, ROWS n, SET/RESET config)` -> `RenameFunction`/`FunctionOwner`/`SetFunctionSchema`/`FunctionAttrs(FunctionChanges)`; the function is identified by name + argument TYPES (`pgarbor_catalog::ddl::function::format_signature` for messages; 42883 `function f(integer) does not exist`; 42725 `function name "f" is not unique` + HINT `Specify the argument list to select the function unambiguously.` when args omitted) |
//! | [`bind_alter_type`] | `ALTER TYPE t RENAME TO`, `OWNER TO`, `SET SCHEMA`, `ADD VALUE [IF NOT EXISTS] 'v' [BEFORE|AFTER 'w']` -> `AddEnumValue { position: EnumPosition }`, `RENAME VALUE 'a' TO 'b'` -> `RenameEnumValue`; composite `ADD/DROP/ALTER ATTRIBUTE` -> `AlterTable` command list with `keyword = Type` (ddl_alter column machinery) |
//! | [`bind_alter_role`] | `ALTER ROLE|USER r WITH options` -> `Role{changes: RoleChanges}` (SUPERUSER, CREATEDB, CREATEROLE, INHERIT, LOGIN, REPLICATION, BYPASSRLS, CONNECTION LIMIT n, [ENCRYPTED] PASSWORD 'x'|NULL, VALID UNTIL 'ts' parsed with `io::input` as timestamptz -> µs); `RENAME TO` -> `RenameRole`; `SET guc = v|TO DEFAULT|FROM CURRENT`, `RESET guc` -> `RoleConfig`; `IN DATABASE` 0A000 |
//! | extras: [`bind_alter_sequence`] | `ALTER SEQUENCE [IF EXISTS] s <options>` -> `Sequence{options: SequenceOptions, owned_by}` (value range checks as `CREATE SEQUENCE`: 22023 `MINVALUE (5) must be less than MAXVALUE (3)`, `START value (0) cannot be less than MINVALUE (1)`, `RESTART value (0) cannot be less than MINVALUE (1)`, `INCREMENT must not be zero`; `AS type` changes bounds); `RENAME TO`/`OWNER TO`/`SET SCHEMA`/`SET LOGGED|UNLOGGED` -> relation actions with `keyword = Sequence`; `OWNED BY t.c|NONE` (42809 `sequence must be in same schema as table it is linked to`, 55000 `sequence must have same owner as table it is linked to`) |
//! | extras: [`bind_alter_domain`] | `ALTER DOMAIN d SET DEFAULT e|DROP DEFAULT|SET NOT NULL|DROP NOT NULL|ADD CONSTRAINT c CHECK (..) [NOT VALID]|DROP CONSTRAINT [IF EXISTS] c|RENAME CONSTRAINT|VALIDATE CONSTRAINT|RENAME TO|OWNER TO|SET SCHEMA` -> the matching `AlterAction::Domain*` |
//! | extras: [`bind_alter_database`] | `ALTER DATABASE d RENAME TO|OWNER TO|WITH (ALLOW_CONNECTIONS, CONNECTION LIMIT, IS_TEMPLATE)|SET guc|RESET guc` -> `RenameDatabase`/`DatabaseOwner`/`DatabaseOptions`/`DatabaseConfig` |
//! | extras: [`bind_alter_matview`] | `ALTER MATERIALIZED VIEW m RENAME TO|SET SCHEMA|OWNER TO|RENAME COLUMN` |
//!
//! Role references (`OWNER TO r`, `CURRENT_USER`, `SESSION_USER`, `CURRENT_ROLE`) resolve with
//! `env.role_oid(name)` (42704 `role "x" does not exist`); permission checks are the catalog's.
//! Name resolution of the altered object uses `env.lookup_class` / `lookup_type` / schema lookup with
//! the exact PG error texts (`relation "s" does not exist` 42P01, `type "t" does not exist` 42704,
//! `schema "s" does not exist` 3F000, `sequence "s" does not exist`? -> 42P01 `relation`).
use pgarbor_common::{PgError, Result};
use sqlparser::ast::Statement;

use super::context::Binder;
use crate::extra_stmt::{AlterDatabaseAction, AlterDomainAction, AlterMatViewAction, AlterSequenceAction};
use crate::ir::BoundStatement;
use sqlparser::ast::{Ident, ObjectName};

/// `Statement::AlterSchema`.
#[allow(unused_variables)]
pub fn bind_alter_schema(b: &mut Binder, stmt: &Statement) -> Result<BoundStatement> {
    Err(PgError::unimplemented("SQL-34", "ALTER SCHEMA binding (ddl_alter_other::bind_alter_schema)"))
}

/// `Statement::AlterFunction`.
#[allow(unused_variables)]
pub fn bind_alter_function(b: &mut Binder, stmt: &Statement) -> Result<BoundStatement> {
    Err(PgError::unimplemented("SQL-34", "ALTER FUNCTION binding (ddl_alter_other::bind_alter_function)"))
}

/// `Statement::AlterType`.
#[allow(unused_variables)]
pub fn bind_alter_type(b: &mut Binder, stmt: &Statement) -> Result<BoundStatement> {
    Err(PgError::unimplemented("SQL-34", "ALTER TYPE binding (ddl_alter_other::bind_alter_type)"))
}

/// `Statement::AlterRole` / `AlterUser`.
#[allow(unused_variables)]
pub fn bind_alter_role(b: &mut Binder, stmt: &Statement) -> Result<BoundStatement> {
    Err(PgError::unimplemented("SQL-34", "ALTER ROLE binding (ddl_alter_other::bind_alter_role)"))
}

/// `ExtraStatement::AlterSequence`.
#[allow(unused_variables)]
pub fn bind_alter_sequence(b: &mut Binder, if_exists: bool, name: &ObjectName, action: &AlterSequenceAction) -> Result<BoundStatement> {
    Err(PgError::unimplemented("SQL-34", "ALTER SEQUENCE binding (ddl_alter_other::bind_alter_sequence)"))
}

/// `ExtraStatement::AlterDomain`.
#[allow(unused_variables)]
pub fn bind_alter_domain(b: &mut Binder, name: &ObjectName, action: &AlterDomainAction) -> Result<BoundStatement> {
    Err(PgError::unimplemented("SQL-34", "ALTER DOMAIN binding (ddl_alter_other::bind_alter_domain)"))
}

/// `ExtraStatement::AlterDatabase`.
#[allow(unused_variables)]
pub fn bind_alter_database(b: &mut Binder, name: &Ident, action: &AlterDatabaseAction) -> Result<BoundStatement> {
    Err(PgError::unimplemented("SQL-34", "ALTER DATABASE binding (ddl_alter_other::bind_alter_database)"))
}

/// `ExtraStatement::AlterMaterializedView`.
#[allow(unused_variables)]
pub fn bind_alter_matview(b: &mut Binder, if_exists: bool, name: &ObjectName, action: &AlterMatViewAction) -> Result<BoundStatement> {
    Err(PgError::unimplemented("SQL-34", "ALTER MATERIALIZED VIEW binding (ddl_alter_other::bind_alter_matview)"))
}
