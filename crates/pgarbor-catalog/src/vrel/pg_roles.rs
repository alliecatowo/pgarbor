//! Roles, memberships, databases, tablespaces and their views. Lease CAT-34. PG sources:
//! `catalog/pg_authid.h`, `pg_auth_members.h`, `pg_database.h`, `pg_db_role_setting.h`,
//! `pg_tablespace.h`, `catalog/system_views.sql` (pg_roles, pg_shadow, pg_group, pg_user).
//! Roles = `snapshot.indexes().roles` (builtin predefined + persisted `postgres` + user roles),
//! all ordered by OID. Fast paths: eq on `oid`/`rolname`.
//!
//! - **pg_authid**: `oid, rolname, rolsuper, rolinherit, rolcreaterole, rolcreatedb, rolcanlogin,
//!   rolreplication, rolbypassrls, rolconnlimit, rolpassword` (the stored verifier or NULL),
//!   `rolvaliduntil` (`TimestampTz` or NULL).
//! - **pg_roles**: `rolname, rolsuper, rolinherit, rolcreaterole, rolcreatedb, rolcanlogin,
//!   rolreplication, rolconnlimit, rolpassword` = the literal text `********` ALWAYS (even when
//!   NULL in pg_authid: oracle), `rolvaliduntil, rolbypassrls, rolconfig` (= `Role::config` as
//!   `text[]`, NULL when empty), `oid`. Note the column ORDER of pg_roles differs from pg_authid:
//!   use `RowBuilder` by name.
//! - **pg_shadow**: roles with `can_login`: `usename, usesysid, usecreatedb, usesuper, userepl,
//!   usebypassrls, passwd` (verifier), `valuntil, useconfig`. **pg_user**: same rows, `passwd` =
//!   `********`.
//! - **pg_group**: roles WITHOUT `can_login`: `groname, grosysid, grolist` = `oid[]` of members
//!   (`RoleMembership::member` where `role` = this role), empty array when none.
//! - **pg_auth_members**: persisted `RoleMembership` rows (`oid, roleid, member, grantor,
//!   admin_option, inherit_option, set_option`) plus the builtin predefined-role grants of
//!   `builtin::AUTH_MEMBERS` (`TextRows`: convert cells with `io::input`). Ordered by `oid`.
//! - **pg_database**: `snapshot.indexes().databases` (template1 1, template0 4, postgres 5):
//!   `oid, datname, datdba, encoding, datlocprovider` (`Char`), `datistemplate, datallowconn,
//!   dathasloginevt` (false), `datconnlimit, datfrozenxid` (`Oid(3)` for persisted rows; builtin
//!   `frozen_xid`), `datminmxid` (1 / builtin `min_mxid`), `dattablespace` 1663 (builtin
//!   `tablespace`), `datcollate, datctype, datlocale, daticurules` NULL, `datcollversion` NULL,
//!   `datacl`.
//! - **pg_db_role_setting**: one row per non-empty `Database::config` (`setdatabase` = db OID,
//!   `setrole` 0) and per non-empty `Role::config` (`setdatabase` 0, `setrole` = role OID);
//!   `setconfig` = `text[]` of the `name=value` strings.
//! - **pg_tablespace**: `builtin::TABLESPACES` (pg_default 1663, pg_global 1664): `oid, spcname,
//!   spcowner, spcacl` NULL, `spcoptions` NULL.
use super::{Row, ScanContext, ScanRequest, VirtualRelation};
use crate::builtin::relid;
use pgarbor_common::{Oid, PgError, Result};

/// Producer for `pg_authid`.
#[derive(Clone, Copy, Debug)]
pub struct PgAuthid;

impl VirtualRelation for PgAuthid {
    fn oid(&self) -> Oid {
        relid::PG_AUTHID
    }
    #[allow(unused_variables)]
    fn scan(&self, ctx: &ScanContext, req: &ScanRequest) -> Result<Vec<Row>> {
        Err(PgError::unimplemented("CAT-34", "pg_authid rows"))
    }
}

/// Producer for `pg_auth_members`.
#[derive(Clone, Copy, Debug)]
pub struct PgAuthMembers;

impl VirtualRelation for PgAuthMembers {
    fn oid(&self) -> Oid {
        relid::PG_AUTH_MEMBERS
    }
    #[allow(unused_variables)]
    fn scan(&self, ctx: &ScanContext, req: &ScanRequest) -> Result<Vec<Row>> {
        Err(PgError::unimplemented("CAT-34", "pg_auth_members rows"))
    }
}

/// Producer for `pg_database`.
#[derive(Clone, Copy, Debug)]
pub struct PgDatabase;

impl VirtualRelation for PgDatabase {
    fn oid(&self) -> Oid {
        relid::PG_DATABASE
    }
    #[allow(unused_variables)]
    fn scan(&self, ctx: &ScanContext, req: &ScanRequest) -> Result<Vec<Row>> {
        Err(PgError::unimplemented("CAT-34", "pg_database rows"))
    }
}

/// Producer for `pg_db_role_setting`.
#[derive(Clone, Copy, Debug)]
pub struct PgDbRoleSetting;

impl VirtualRelation for PgDbRoleSetting {
    fn oid(&self) -> Oid {
        relid::PG_DB_ROLE_SETTING
    }
    #[allow(unused_variables)]
    fn scan(&self, ctx: &ScanContext, req: &ScanRequest) -> Result<Vec<Row>> {
        Err(PgError::unimplemented("CAT-34", "pg_db_role_setting rows"))
    }
}

/// Producer for `pg_tablespace`.
#[derive(Clone, Copy, Debug)]
pub struct PgTablespace;

impl VirtualRelation for PgTablespace {
    fn oid(&self) -> Oid {
        relid::PG_TABLESPACE
    }
    #[allow(unused_variables)]
    fn scan(&self, ctx: &ScanContext, req: &ScanRequest) -> Result<Vec<Row>> {
        Err(PgError::unimplemented("CAT-34", "pg_tablespace rows"))
    }
}

/// Producer for `pg_roles`.
#[derive(Clone, Copy, Debug)]
pub struct PgRoles;

impl VirtualRelation for PgRoles {
    fn oid(&self) -> Oid {
        relid::PG_ROLES
    }
    #[allow(unused_variables)]
    fn scan(&self, ctx: &ScanContext, req: &ScanRequest) -> Result<Vec<Row>> {
        Err(PgError::unimplemented("CAT-34", "pg_roles rows"))
    }
}

/// Producer for `pg_shadow`.
#[derive(Clone, Copy, Debug)]
pub struct PgShadow;

impl VirtualRelation for PgShadow {
    fn oid(&self) -> Oid {
        relid::PG_SHADOW
    }
    #[allow(unused_variables)]
    fn scan(&self, ctx: &ScanContext, req: &ScanRequest) -> Result<Vec<Row>> {
        Err(PgError::unimplemented("CAT-34", "pg_shadow rows"))
    }
}

/// Producer for `pg_group`.
#[derive(Clone, Copy, Debug)]
pub struct PgGroup;

impl VirtualRelation for PgGroup {
    fn oid(&self) -> Oid {
        relid::PG_GROUP
    }
    #[allow(unused_variables)]
    fn scan(&self, ctx: &ScanContext, req: &ScanRequest) -> Result<Vec<Row>> {
        Err(PgError::unimplemented("CAT-34", "pg_group rows"))
    }
}

/// Producer for `pg_user`.
#[derive(Clone, Copy, Debug)]
pub struct PgUser;

impl VirtualRelation for PgUser {
    fn oid(&self) -> Oid {
        relid::PG_USER
    }
    #[allow(unused_variables)]
    fn scan(&self, ctx: &ScanContext, req: &ScanRequest) -> Result<Vec<Row>> {
        Err(PgError::unimplemented("CAT-34", "pg_user rows"))
    }
}


/// Registry slice.
pub static RELATIONS: &[&dyn VirtualRelation] =
    &[&PgAuthid, &PgAuthMembers, &PgDatabase, &PgDbRoleSetting, &PgTablespace, &PgRoles, &PgShadow, &PgGroup, &PgUser];
