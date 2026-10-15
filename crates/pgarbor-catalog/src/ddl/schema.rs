//! CREATE SCHEMA, ALTER SCHEMA (RENAME / OWNER), ALTER DATABASE / `ALTER DATABASE ... SET`.
//! Lease CAT-09. PG sources: `commands/schemacmds.c` (`CreateSchemaCommand`, `RenameSchema`,
//! `AlterSchemaOwner`), `commands/dbcommands.c` (`RenameDatabase`, `AlterDatabaseOwner`,
//! `AlterDatabaseSet`).
//!
//! DROP SCHEMA is `drop_stmt` + `drop` (dependency-driven). `CREATE SCHEMA s CREATE TABLE ...`
//! sub-commands are executed by the binder/executor as separate statements after this call.
//!
//! Oracle-verified messages (PG 18.6):
//! - `create schema pg_x` -> 42939 `unacceptable schema name "pg_x"` DETAIL `The prefix "pg_" is
//!   reserved for system schemas.` (`errors::reserved_schema_name`)
//! - duplicate -> 42P06 `schema "s" already exists`; `IF NOT EXISTS` -> NOTICE `schema "s" already
//!   exists, skipping`
//! - `alter schema nosuch rename to x` -> 3F000 `schema "nosuch" does not exist`
//! - rename onto an existing name -> 42P06 `schema "x" already exists`
//! - rename to `pg_x` -> 42939 as above. Renaming/altering `pg_catalog`, `pg_toast`,
//!   `information_schema`: they are builtin (not in the persisted data): 42501
//!   `permission denied: "pg_catalog" is a system catalog` (`errors::system_catalog_permission`).
use crate::model::*;
use crate::txn::CatalogTxn;
use pgarbor_common::{Oid, PgError, Result};

impl<'a> CatalogTxn<'a> {
    /// CREATE SCHEMA `name` AUTHORIZATION `owner` (owner = current user when not given).
    ///
    /// Order of checks (PG `CreateSchemaCommand`): the current user needs CREATE on the database
    /// (`has_privs`: superuser, database owner, or an ACL grant `C`; PUBLIC has none by default) else
    /// 42501 `permission denied for database postgres`; when `owner` differs from the current user
    /// the current user must be a member of `owner` (42501 `must be able to SET ROLE "owner"`);
    /// reserved prefix `pg_` -> 42939; existing name -> 42P06 (or NOTICE + `Ok(None)` when
    /// `if_not_exists`). Stores `Namespace{oid: new_oid, name, owner, acl: None}`.
    /// Returns the new OID.
    // CONTRACT
    #[allow(unused_variables)]
    pub fn create_schema(&mut self, name: &str, owner: Oid, if_not_exists: bool) -> Result<Option<Oid>> {
        Err(PgError::unimplemented("CAT-09", "CREATE SCHEMA"))
    }

    /// ALTER SCHEMA `old_name` RENAME TO `new_name` (checks in the PG order shown in the module
    /// docs: existence 3F000, new name free 42P06, ownership `must be owner of schema old`, CREATE on
    /// database, reserved prefix 42939). Objects in the schema keep their OIDs; `Constraint::namespace`
    /// etc. are OID-based so nothing else changes. Descriptions are unaffected.
    // CONTRACT
    #[allow(unused_variables)]
    pub fn rename_schema(&mut self, old_name: &str, new_name: &str) -> Result<()> {
        Err(PgError::unimplemented("CAT-09", "ALTER SCHEMA RENAME"))
    }

    /// ALTER SCHEMA `name` OWNER TO `new_owner`: must be owner (or superuser) AND member of
    /// `new_owner` (42501 `must be able to SET ROLE "new_owner"`) AND `new_owner` needs CREATE on the
    /// database. No-op when unchanged. The schema's ACL entries referencing the old owner are
    /// rewritten (`acl::aclnewowner`) when an explicit ACL exists. Contained objects keep their
    /// owners (PG does not cascade).
    // CONTRACT
    #[allow(unused_variables)]
    pub fn alter_schema_owner(&mut self, name: &str, new_owner: Oid) -> Result<()> {
        Err(PgError::unimplemented("CAT-09", "ALTER SCHEMA OWNER"))
    }

    /// ALTER DATABASE `name` RENAME TO `new_name`: unknown -> 3D000 `database "x" does not exist`;
    /// the CURRENT database -> 55006 `current database cannot be renamed`; builtin templates ->
    /// 0A000-class refusal `permission denied to rename database`; duplicate -> 42P04
    /// `database "x" already exists`; must be owner + CREATEDB (42501).
    // CONTRACT
    #[allow(unused_variables)]
    pub fn rename_database(&mut self, name: &str, new_name: &str) -> Result<()> {
        Err(PgError::unimplemented("CAT-09", "ALTER DATABASE RENAME"))
    }

    /// ALTER DATABASE `name` OWNER TO `new_owner` (owner or superuser, member of the new owner,
    /// new owner needs CREATEDB).
    // CONTRACT
    #[allow(unused_variables)]
    pub fn alter_database_owner(&mut self, name: &str, new_owner: Oid) -> Result<()> {
        Err(PgError::unimplemented("CAT-09", "ALTER DATABASE OWNER"))
    }

    /// `ALTER DATABASE db SET name = value` / `RESET name` (`value = None` removes the entry):
    /// updates `Database::config` (`name=value` strings, replacing an existing `name=`). The
    /// executor validates the GUC name/value before calling (this function only stores). Owner or
    /// superuser required.
    // CONTRACT
    #[allow(unused_variables)]
    pub fn set_database_config(&mut self, name: &str, guc: &str, value: Option<&str>) -> Result<()> {
        Err(PgError::unimplemented("CAT-09", "ALTER DATABASE SET"))
    }

    /// ALTER DATABASE ... ALLOW_CONNECTIONS / CONNECTION LIMIT / IS_TEMPLATE.
    // CONTRACT
    #[allow(unused_variables)]
    pub fn alter_database_options(
        &mut self,
        name: &str,
        allow_conn: Option<bool>,
        conn_limit: Option<i32>,
        is_template: Option<bool>,
    ) -> Result<()> {
        Err(PgError::unimplemented("CAT-09", "ALTER DATABASE options"))
    }

    /// Used by `drop` and tests: the schema row by OID (persisted schemas only).
    pub fn schema_row(&self, oid: Oid) -> Option<&Namespace> {
        self.data.namespaces.get(&oid)
    }
}
