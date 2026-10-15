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
use crate::acl::{aclnewowner, effective_acl, has_privilege, AclObject, RoleGraph};
use crate::model::*;
use crate::snapshot::CatalogSnapshot;
use crate::txn::CatalogTxn;
use pgarbor_common::{Oid, PgError, Result, SqlState};

/// PG `IsReservedName`: names with the `pg_` prefix belong to the system.
fn is_reserved_name(name: &str) -> bool {
    name.starts_with("pg_")
}

/// PG `check_can_set_role` (miscinit.c) over `member_can_set_role` (acl.c): `member` may act as
/// `role` when they are the same role, `member` is a superuser, or `member` reaches `role` through
/// SET-ROLE-capable memberships. Otherwise 42501 `must be able to SET ROLE "role"`.
fn check_can_set_role(snap: &CatalogSnapshot, graph: &RoleGraph, member: Oid, role: Oid) -> Result<()> {
    if member == role || graph.is_superuser(member) || graph.can_set_role(member, role)? {
        return Ok(());
    }
    let name = snap.role_name(role).map(str::to_string).unwrap_or_else(|| role.to_string());
    Err(PgError::new(SqlState::INSUFFICIENT_PRIVILEGE, format!("must be able to SET ROLE \"{name}\"")))
}

/// The CREATE privilege on the session database for `role` (PG
/// `object_aclcheck(DatabaseRelationId, MyDatabaseId, role, ACL_CREATE)`), using the database's
/// effective ACL. Failure: 42501 `permission denied for database <name>`.
fn check_database_create(snap: &CatalogSnapshot, graph: &RoleGraph, database: Oid, role: Oid) -> Result<()> {
    let db = snap.database(database).ok_or_else(|| crate::errors::cache_lookup_failed("database", database))?;
    let acl = effective_acl(AclObject::Database, db.owner, db.acl.as_ref())?;
    if has_privilege(graph, role, db.owner, &acl, acl_mode::CREATE)? {
        Ok(())
    } else {
        Err(crate::errors::permission_denied_for("database", &db.name))
    }
}

/// Ownership of a database (PG `object_ownercheck`): the role has the privileges of the owner or
/// is a superuser. Failure: 42501 `must be owner of database <name>`. Works for builtin databases
/// too, since it only reads the snapshot row.
fn check_database_owner(graph: &RoleGraph, db: &Database, role: Oid) -> Result<()> {
    if graph.is_superuser(role) || graph.has_privs_of_role(role, db.owner)? {
        Ok(())
    } else {
        Err(crate::errors::must_be_owner("database", &db.name))
    }
}

/// PG `have_createdb_privilege` for `role`: superuser or `rolcreatedb`.
fn have_createdb(snap: &CatalogSnapshot, role: Oid) -> bool {
    snap.role(role).is_some_and(|r| r.superuser || r.create_db)
}

/// Apply `name=value` / removal to a per-database settings list (PG `AlterSetting`): an existing
/// entry for `guc` is replaced in place, otherwise the entry is appended; `None` removes it.
fn apply_guc(config: &mut Vec<String>, guc: &str, value: Option<&str>) {
    let same = |entry: &str| entry.split_once('=').map_or(entry, |(n, _)| n) == guc;
    match value {
        Some(v) => {
            let entry = format!("{guc}={v}");
            match config.iter().position(|e| same(e.as_str())) {
                Some(i) => config[i] = entry,
                None => config.push(entry),
            }
        }
        None => config.retain(|e| !same(e.as_str())),
    }
}

impl<'a> CatalogTxn<'a> {
    /// The session database OID from the statement's naming context.
    fn session_database(&mut self) -> Result<Oid> {
        Ok(self.env()?.database())
    }

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
    pub fn create_schema(&mut self, name: &str, owner: Oid, if_not_exists: bool) -> Result<Option<Oid>> {
        let snap = self.snapshot()?;
        let current = self.current_user();
        let database = self.session_database()?;
        let graph = RoleGraph::build(&snap, database)?;

        check_database_create(&snap, &graph, database, current)?;
        check_can_set_role(&snap, &graph, current, owner)?;
        if is_reserved_name(name) {
            return Err(crate::errors::reserved_schema_name(name));
        }
        if snap.namespace_by_name(name).is_some() {
            if if_not_exists {
                self.notice(crate::errors::schema_already_exists_skipping(name));
                return Ok(None);
            }
            return Err(crate::errors::schema_already_exists(name));
        }

        let oid = self.new_oid()?;
        self.put(CatalogObject::Namespace(Namespace { oid, name: name.to_string(), owner, acl: None }))?;
        self.record_created(ObjectAddress::new(ObjClass::Namespace, oid));
        Ok(Some(oid))
    }

    /// ALTER SCHEMA `old_name` RENAME TO `new_name` (checks in the PG order shown in the module
    /// docs: existence 3F000, new name free 42P06, ownership `must be owner of schema old`, CREATE on
    /// database, reserved prefix 42939). Objects in the schema keep their OIDs; `Constraint::namespace`
    /// etc. are OID-based so nothing else changes. Descriptions are unaffected.
    // CONTRACT
    pub fn rename_schema(&mut self, old_name: &str, new_name: &str) -> Result<()> {
        let snap = self.snapshot()?;
        let current = self.current_user();
        let database = self.session_database()?;

        let oid = match snap.namespace_by_name(old_name) {
            Some(ns) => ns.oid,
            None => return Err(crate::errors::schema_does_not_exist(old_name)),
        };
        if !self.data.namespaces.contains_key(&oid) {
            // builtin pg_catalog / pg_toast / information_schema
            return Err(crate::errors::system_catalog_permission(old_name));
        }
        if snap.namespace_by_name(new_name).is_some() {
            return Err(crate::errors::schema_already_exists(new_name));
        }
        self.require_owner(&ObjectAddress::new(ObjClass::Namespace, oid))?;
        let graph = RoleGraph::build(&snap, database)?;
        check_database_create(&snap, &graph, database, current)?;
        if is_reserved_name(new_name) {
            return Err(crate::errors::reserved_schema_name(new_name));
        }
        self.update_namespace(oid, |ns| {
            ns.name = new_name.to_string();
            Ok(())
        })
    }

    /// ALTER SCHEMA `name` OWNER TO `new_owner`: must be owner (or superuser) AND member of
    /// `new_owner` (42501 `must be able to SET ROLE "new_owner"`) AND `new_owner` needs CREATE on the
    /// database. No-op when unchanged. The schema's ACL entries referencing the old owner are
    /// rewritten (`acl::aclnewowner`) when an explicit ACL exists. Contained objects keep their
    /// owners (PG does not cascade).
    // CONTRACT
    pub fn alter_schema_owner(&mut self, name: &str, new_owner: Oid) -> Result<()> {
        let snap = self.snapshot()?;
        let current = self.current_user();
        let database = self.session_database()?;

        let (oid, owner) = match snap.namespace_by_name(name) {
            Some(ns) => (ns.oid, ns.owner),
            None => return Err(crate::errors::schema_does_not_exist(name)),
        };
        if !self.data.namespaces.contains_key(&oid) {
            return Err(crate::errors::system_catalog_permission(name));
        }
        if owner == new_owner {
            // PG treats an unchanged owner as success (dump restoration).
            return Ok(());
        }
        self.require_owner(&ObjectAddress::new(ObjClass::Namespace, oid))?;
        let graph = RoleGraph::build(&snap, database)?;
        check_can_set_role(&snap, &graph, current, new_owner)?;
        check_database_create(&snap, &graph, database, current)?;
        self.update_namespace(oid, |ns| {
            ns.acl = match ns.acl.take() {
                Some(acl) => Some(aclnewowner(&acl, owner, new_owner)?),
                None => None,
            };
            ns.owner = new_owner;
            Ok(())
        })
    }

    /// ALTER DATABASE `name` RENAME TO `new_name`: unknown -> 3D000 `database "x" does not exist`;
    /// the CURRENT database -> 55006 `current database cannot be renamed`; builtin templates ->
    /// 0A000-class refusal `permission denied to rename database`; duplicate -> 42P04
    /// `database "x" already exists`; must be owner + CREATEDB (42501).
    // CONTRACT
    pub fn rename_database(&mut self, name: &str, new_name: &str) -> Result<()> {
        let snap = self.snapshot()?;
        let current = self.current_user();
        let session_db = self.session_database()?;

        let db = snap.database_by_name(name).ok_or_else(|| crate::errors::database_does_not_exist(name))?;
        let graph = RoleGraph::build(&snap, session_db)?;
        check_database_owner(&graph, db, current)?;
        if !have_createdb(&snap, current) {
            return Err(crate::errors::permission_denied_to("rename database"));
        }
        if snap.database_by_name(new_name).is_some() {
            return Err(PgError::new(SqlState::DUPLICATE_DATABASE, format!("database \"{new_name}\" already exists")));
        }
        if db.oid == session_db {
            return Err(PgError::new(SqlState(*b"55006"), "current database cannot be renamed"));
        }
        if !self.data.databases.contains_key(&db.oid) {
            // builtin template databases are compiled in and cannot be renamed
            return Err(crate::errors::permission_denied_to("rename database"));
        }
        let oid = db.oid;
        self.update_database(oid, |d| {
            d.name = new_name.to_string();
            Ok(())
        })
    }

    /// ALTER DATABASE `name` OWNER TO `new_owner` (owner or superuser, member of the new owner,
    /// new owner needs CREATEDB).
    // CONTRACT
    pub fn alter_database_owner(&mut self, name: &str, new_owner: Oid) -> Result<()> {
        let snap = self.snapshot()?;
        let current = self.current_user();
        let session_db = self.session_database()?;

        let db = snap.database_by_name(name).ok_or_else(|| crate::errors::database_does_not_exist(name))?;
        if !self.data.databases.contains_key(&db.oid) {
            return Err(PgError::unimplemented("CAT-09", "ALTER DATABASE OWNER on a builtin template database"));
        }
        if db.owner == new_owner {
            return Ok(());
        }
        let graph = RoleGraph::build(&snap, session_db)?;
        check_database_owner(&graph, db, current)?;
        check_can_set_role(&snap, &graph, current, new_owner)?;
        if !have_createdb(&snap, current) {
            return Err(crate::errors::permission_denied_to("change owner of database"));
        }
        let oid = db.oid;
        let old_owner = db.owner;
        self.update_database(oid, |d| {
            d.acl = match d.acl.take() {
                Some(acl) => Some(aclnewowner(&acl, old_owner, new_owner)?),
                None => None,
            };
            d.owner = new_owner;
            Ok(())
        })
    }

    /// `ALTER DATABASE db SET name = value` / `RESET name` (`value = None` removes the entry):
    /// updates `Database::config` (`name=value` strings, replacing an existing `name=`). The
    /// executor validates the GUC name/value before calling (this function only stores). Owner or
    /// superuser required.
    // CONTRACT
    pub fn set_database_config(&mut self, name: &str, guc: &str, value: Option<&str>) -> Result<()> {
        let snap = self.snapshot()?;
        let current = self.current_user();
        let session_db = self.session_database()?;

        let db = snap.database_by_name(name).ok_or_else(|| crate::errors::database_does_not_exist(name))?;
        if !self.data.databases.contains_key(&db.oid) {
            return Err(PgError::unimplemented("CAT-09", "ALTER DATABASE SET on a builtin template database"));
        }
        let graph = RoleGraph::build(&snap, session_db)?;
        check_database_owner(&graph, db, current)?;
        let oid = db.oid;
        self.update_database(oid, |d| {
            apply_guc(&mut d.config, guc, value);
            Ok(())
        })
    }

    /// ALTER DATABASE ... ALLOW_CONNECTIONS / CONNECTION LIMIT / IS_TEMPLATE.
    // CONTRACT
    pub fn alter_database_options(
        &mut self,
        name: &str,
        allow_conn: Option<bool>,
        conn_limit: Option<i32>,
        is_template: Option<bool>,
    ) -> Result<()> {
        // PG AlterDatabase: the connection limit is validated before the catalog lookup.
        if let Some(limit) = conn_limit {
            if limit < -1 {
                return Err(PgError::new(
                    SqlState::INVALID_PARAMETER_VALUE,
                    format!("invalid connection limit: {limit}"),
                ));
            }
        }
        let snap = self.snapshot()?;
        let current = self.current_user();
        let session_db = self.session_database()?;

        let db = snap.database_by_name(name).ok_or_else(|| crate::errors::database_does_not_exist(name))?;
        if !self.data.databases.contains_key(&db.oid) {
            return Err(PgError::unimplemented("CAT-09", "ALTER DATABASE options on a builtin template database"));
        }
        let graph = RoleGraph::build(&snap, session_db)?;
        check_database_owner(&graph, db, current)?;
        if allow_conn == Some(false) && db.oid == session_db {
            return Err(PgError::new(
                SqlState::INVALID_PARAMETER_VALUE,
                "cannot disallow connections for current database",
            ));
        }
        let oid = db.oid;
        self.update_database(oid, |d| {
            if let Some(v) = allow_conn {
                d.allow_conn = v;
            }
            if let Some(v) = conn_limit {
                d.conn_limit = v;
            }
            if let Some(v) = is_template {
                d.is_template = v;
            }
            Ok(())
        })
    }

    /// Used by `drop` and tests: the schema row by OID (persisted schemas only).
    pub fn schema_row(&self, oid: Oid) -> Option<&Namespace> {
        self.data.namespaces.get(&oid)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing;

    #[test]
    fn guc_entries_are_replaced_appended_and_removed() {
        let mut cfg: Vec<String> = Vec::new();
        apply_guc(&mut cfg, "work_mem", Some("1MB"));
        apply_guc(&mut cfg, "timezone", Some("UTC"));
        assert_eq!(cfg, vec!["work_mem=1MB".to_string(), "timezone=UTC".to_string()]);
        apply_guc(&mut cfg, "work_mem", Some("2MB"));
        assert_eq!(cfg, vec!["work_mem=2MB".to_string(), "timezone=UTC".to_string()]);
        apply_guc(&mut cfg, "work", None);
        assert_eq!(cfg.len(), 2, "removing a name that is only a prefix must not remove anything");
        apply_guc(&mut cfg, "work_mem", None);
        assert_eq!(cfg, vec!["timezone=UTC".to_string()]);
        apply_guc(&mut cfg, "timezone", None);
        assert!(cfg.is_empty());
    }

    #[test]
    fn create_schema_allocates_user_oid() -> Result<()> {
        let (mut kv, env) = testing::bootstrapped()?;
        let (res, _env) = testing::run(&mut kv, &env, |t| {
            let oid = t.create_schema("s", 10, false)?.expect("schema created");
            assert!(oid >= pgarbor_common::FIRST_NORMAL_OID);
            Ok(())
        })?;
        let ns = res.snapshot.namespace_by_name("s").expect("schema s exists");
        assert_eq!(ns.owner, 10);
        assert!(ns.acl.is_none());
        Ok(())
    }

    #[test]
    fn create_schema_reserved_prefix_is_42939() -> Result<()> {
        let (mut kv, env) = testing::bootstrapped()?;
        let err = testing::run(&mut kv, &env, |t| t.create_schema("pg_x", 10, false).map(|_| ()))
            .expect_err("pg_ prefix must be rejected");
        assert_eq!(err.sqlstate.code(), "42939");
        assert_eq!(err.message, "unacceptable schema name \"pg_x\"");
        assert_eq!(err.detail.as_deref(), Some("The prefix \"pg_\" is reserved for system schemas."));
        Ok(())
    }

    #[test]
    fn create_schema_duplicate_and_if_not_exists() -> Result<()> {
        let (mut kv, env) = testing::bootstrapped()?;
        let (_res, env) = testing::run(&mut kv, &env, |t| t.create_schema("s", 10, false).map(|_| ()))?;
        let err = testing::run(&mut kv, &env, |t| t.create_schema("s", 10, false).map(|_| ()))
            .expect_err("duplicate schema");
        assert_eq!(err.sqlstate.code(), "42P06");
        assert_eq!(err.message, "schema \"s\" already exists");

        let mut got: Option<Option<Oid>> = None;
        let (res, _env) = testing::run(&mut kv, &env, |t| {
            got = Some(t.create_schema("s", 10, true)?);
            Ok(())
        })?;
        assert_eq!(got, Some(None));
        assert_eq!(res.outcome.notices.len(), 1);
        assert_eq!(res.outcome.notices[0].message, "schema \"s\" already exists, skipping");
        Ok(())
    }

    #[test]
    fn rename_schema_errors_and_success() -> Result<()> {
        let (mut kv, env) = testing::bootstrapped()?;
        let err = testing::run(&mut kv, &env, |t| t.rename_schema("nosuch", "x"))
            .expect_err("unknown schema");
        assert_eq!(err.sqlstate.code(), "3F000");
        assert_eq!(err.message, "schema \"nosuch\" does not exist");

        let (_r, env) = testing::run(&mut kv, &env, |t| {
            t.create_schema("a", 10, false)?;
            t.create_schema("b", 10, false)?;
            Ok(())
        })?;
        let err = testing::run(&mut kv, &env, |t| t.rename_schema("a", "b")).expect_err("name taken");
        assert_eq!(err.sqlstate.code(), "42P06");
        assert_eq!(err.message, "schema \"b\" already exists");

        let err = testing::run(&mut kv, &env, |t| t.rename_schema("a", "pg_a")).expect_err("reserved");
        assert_eq!(err.sqlstate.code(), "42939");

        let (res, _env) = testing::run(&mut kv, &env, |t| t.rename_schema("a", "c"))?;
        assert!(res.snapshot.namespace_by_name("a").is_none());
        assert!(res.snapshot.namespace_by_name("c").is_some());
        Ok(())
    }

    #[test]
    fn rename_current_database_is_55006() -> Result<()> {
        let (mut kv, env) = testing::bootstrapped()?;
        let err = testing::run(&mut kv, &env, |t| t.rename_database("postgres", "other"))
            .expect_err("current database");
        assert_eq!(err.sqlstate.code(), "55006");
        assert_eq!(err.message, "current database cannot be renamed");

        let err = testing::run(&mut kv, &env, |t| t.rename_database("nosuchdb", "other"))
            .expect_err("unknown database");
        assert_eq!(err.sqlstate.code(), "3D000");
        assert_eq!(err.message, "database \"nosuchdb\" does not exist");
        Ok(())
    }

    #[test]
    fn set_database_config_adds_replaces_removes() -> Result<()> {
        let (mut kv, env) = testing::bootstrapped()?;
        let (res, env) = testing::run(&mut kv, &env, |t| t.set_database_config("postgres", "work_mem", Some("1MB")))?;
        assert_eq!(res.snapshot.database_by_name("postgres").map(|d| d.config.clone()), Some(vec!["work_mem=1MB".to_string()]));
        let (res, env) = testing::run(&mut kv, &env, |t| t.set_database_config("postgres", "work_mem", Some("2MB")))?;
        assert_eq!(res.snapshot.database_by_name("postgres").map(|d| d.config.clone()), Some(vec!["work_mem=2MB".to_string()]));
        let (res, _env) = testing::run(&mut kv, &env, |t| t.set_database_config("postgres", "work_mem", None))?;
        assert_eq!(res.snapshot.database_by_name("postgres").map(|d| d.config.clone()), Some(Vec::new()));
        Ok(())
    }

    #[test]
    fn alter_database_options_rejects_bad_limit_and_current_db_disconnect() -> Result<()> {
        let (mut kv, env) = testing::bootstrapped()?;
        let err = testing::run(&mut kv, &env, |t| t.alter_database_options("postgres", None, Some(-2), None))
            .expect_err("bad limit");
        assert_eq!(err.sqlstate.code(), "22023");
        assert_eq!(err.message, "invalid connection limit: -2");

        let err = testing::run(&mut kv, &env, |t| t.alter_database_options("postgres", Some(false), None, None))
            .expect_err("current database");
        assert_eq!(err.sqlstate.code(), "22023");
        assert_eq!(err.message, "cannot disallow connections for current database");

        let (res, _env) = testing::run(&mut kv, &env, |t| t.alter_database_options("postgres", None, Some(7), Some(true)))?;
        let db = res.snapshot.database_by_name("postgres").expect("postgres database");
        assert_eq!(db.conn_limit, 7);
        assert!(db.is_template);
        Ok(())
    }
}
