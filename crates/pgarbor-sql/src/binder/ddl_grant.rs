//! GRANT / REVOKE of privileges and role membership (SQL-39). PG references: `aclchk.c`
//! (`ExecuteGrantStmt`, `objectNamesToOids`, `expand_all_col_privileges`), `user.c`
//! (`GrantRole`, `AddRoleMems`), `gram.y` (`GrantStmt`, `RevokeStmt`, `GrantRoleStmt`,
//! `privilege_target`, `grantee`).
//!
//! `bind_grant(b, stmt)` handles `Statement::Grant { privileges, objects, grantees, with_grant_option,
//! as_grantor, granted_by, current_grants }` and `Statement::Revoke { privileges, objects, grantees,
//! granted_by, cascade }`:
//! * Privileges: `ALL [PRIVILEGES]` -> `PrivilegeSet::All`; else the union of `Action`s as ACL bits via
//!   `pgarbor_catalog::acl` (`SELECT, INSERT, UPDATE, DELETE, TRUNCATE, REFERENCES, TRIGGER, USAGE,
//!   CREATE, CONNECT, TEMPORARY, EXECUTE, SET, ALTER SYSTEM`). A privilege invalid for the object kind:
//!   0LP01 `invalid privilege type TRUNCATE for sequence`/`... for schema` etc. (`restrict_privs`
//!   `aclchk.c`). Column lists `SELECT (a, b)` -> `GrantSpec.columns` (attnums; table targets only: 0LP01
//!   otherwise).
//! * Objects (`GrantObjects`): `Tables(names)` (no `TABLE` keyword = tables, views, sequences allowed
//!   by name), `Sequences`, `Schemas`, `Databases`, `Domains`/`Types`, `Functions`/`Procedures`/`Routines`
//!   (with argument types), `AllTablesInSchema`/`AllSequencesInSchema`/`AllFunctionsInSchema`; others
//!   (`FOREIGN DATA WRAPPER`, `LANGUAGE`, `LARGE OBJECT`, `TABLESPACE`, `PARAMETER`, Snowflake kinds): 0A000
//!   / 42601. One `GrantSpec` per named object (`GrantTarget::Object(ObjectAddress)`) or per `ALL ... IN
//!   SCHEMA` target; every spec lists ALL grantees.
//! * Grantees: role names, `PUBLIC` (OID 0), `CURRENT_USER`/`CURRENT_ROLE`/`SESSION_USER`; unknown role
//!   42704 `role "x" does not exist`. `WITH GRANT OPTION` -> `grant_option`; `GRANTED BY r` -> `grantor`
//!   (default current user). REVOKE: `GRANT OPTION FOR` -> revoke only the option (`grant_option = true` +
//!   `revoke`), `CASCADE|RESTRICT`.
//! * `ALTER DEFAULT PRIVILEGES` is an `ExtraStatement::Unsupported`.
//!
//! `bind_grant_role`: `GRANT r1, r2 TO m1, m2 [WITH ADMIN OPTION | WITH INHERIT TRUE|FALSE | WITH SET
//! TRUE|FALSE] [GRANTED BY g]` (sqlparser `Statement::Grant` with `GrantObjects::Roles`? or the role
//! form — check 0.63) -> `GrantRole(Vec<MembershipSpec>)`, one per (role, member); `REVOKE [ADMIN OPTION
//! FOR] r FROM m [CASCADE]` -> `RevokeRole { specs, admin_option_only, cascade }`. Self-membership: 0LP01?
//! PG: 0LP01 `role "r" is a member of role "r"`? -> `role "a" cannot be a member of itself`? the CATALOG
//! enforces (`22023`/`0LP01`); the binder only resolves names (42704).
use pgarbor_common::{PgError, Result};
use sqlparser::ast::Statement;

use super::context::Binder;
use crate::ir::BoundStatement;

/// `GRANT`/`REVOKE` on objects.
#[allow(unused_variables)]
pub fn bind_grant(b: &mut Binder, stmt: &Statement) -> Result<BoundStatement> {
    Err(PgError::unimplemented("SQL-39", "GRANT/REVOKE binding (ddl_grant::bind_grant)"))
}

/// `GRANT role TO member` / `REVOKE role FROM member` (called by `bind_grant` when the statement is the
/// role form).
#[allow(unused_variables)]
pub fn bind_grant_role(b: &mut Binder, stmt: &Statement) -> Result<BoundStatement> {
    Err(PgError::unimplemented("SQL-39", "role membership binding (ddl_grant::bind_grant_role)"))
}
