//! Roles and role membership: CREATE/ALTER/DROP ROLE, ALTER ROLE ... SET, GRANT/REVOKE role.
//! Lease CAT-10. PG sources: `commands/user.c` (`CreateRole`, `AlterRole`, `DropRole`,
//! `RenameRole`, `GrantRole`, `AddRoleMems`, `DelRoleMems`, `check_role_membership_authorization`,
//! `roleSpecsToIds`), `utils/adt/acl.c` (`is_admin_of_role`, `roles_is_member_of`).
//!
//! Roles are CLUSTER-wide in PG; in PGArbor they live in the branch catalog like everything else (a
//! branch forks its roles). Predefined roles (`pg_monitor`, `pg_read_all_data`, ...) are builtin (not
//! persisted): they cannot be created, renamed, altered or dropped (CREATE/RENAME to a `pg_` name:
//! 42939 `errors::reserved_role_name`; ALTER/DROP of a builtin role: 42501
//! `errors::permission_denied_to("alter role")` / `("drop role")` with DETAIL `The role "pg_x" is reserved.`
//! style text from PG 18 `user.c`).
//!
//! Oracle-verified (PG 18.6):
//! - `create role postgres` -> 42710 `role "postgres" already exists` (`errors::role_already_exists`)
//! - `create role pg_x` -> 42939 `role name "pg_x" is reserved` DETAIL `Role names starting with "pg_" are reserved.`
//! - `drop role nosuch` -> 42704 `role "nosuch" does not exist`; IF EXISTS -> NOTICE `role "nosuch" does not exist, skipping`
//! - `drop role` of the current user -> 55006 `current user cannot be dropped`
//! - dropping a role that owns objects -> 2BP01 `role "r" cannot be dropped because some objects
//!   depend on it` DETAIL `owner of table zz.t` lines (`errors::role_cannot_be_dropped`; every object
//!   whose `owner` is the role, in OID order, described by `describe`; plus `privileges for ...` for
//!   ACL grants to the role)
//! - GRANT creating a cycle -> 0LP01 `role "a" is a member of role "b"` (`errors::circular_membership`)
use super::spec::*;
use crate::model::*;
use crate::txn::CatalogTxn;
use pgarbor_common::{Oid, PgError, Result};

impl<'a> CatalogTxn<'a> {
    /// CREATE ROLE. Requires the CREATEROLE attribute (or superuser); creating a SUPERUSER /
    /// REPLICATION / BYPASSRLS role needs the corresponding attribute yourself (superuser for the
    /// first); else 42501 `errors::permission_denied_create_role`. Checks: reserved `pg_` prefix
    /// (42939), duplicate (42710), password hashed (22023 `errors::password_not_hashed` unless it
    /// starts with `SCRAM-SHA-256$` or is `md5` + 32 hex). Inserts `Role{..}`; processes `in_roles`,
    /// `members`, `admins` through the membership logic below (grantor = current user). Returns the
    /// new OID.
    // CONTRACT
    #[allow(unused_variables)]
    pub fn create_role(&mut self, spec: &CreateRoleSpec) -> Result<Oid> {
        Err(PgError::unimplemented("CAT-10", "CREATE ROLE"))
    }

    /// ALTER ROLE `role` attribute changes (`user.c AlterRole`): the superuser-only attributes
    /// (SUPERUSER, REPLICATION, BYPASSRLS) need superuser; the others need CREATEROLE plus ADMIN OPTION
    /// on the target role; any role may change its own PASSWORD. The bootstrap superuser (OID 10)
    /// cannot lose SUPERUSER: 42501 `permission denied to alter role` DETAIL `The bootstrap
    /// superuser must have the SUPERUSER attribute.`
    // CONTRACT
    #[allow(unused_variables)]
    pub fn alter_role(&mut self, role: Oid, changes: &RoleChanges) -> Result<()> {
        Err(PgError::unimplemented("CAT-10", "ALTER ROLE"))
    }

    /// ALTER ROLE `role` RENAME TO `new_name`: duplicate 42710; `pg_` prefix 42939; the session /
    /// current user cannot be renamed (`session user cannot be renamed`, 0A000); a role with an MD5
    /// password loses it in PG (`MD5 password cleared because of role rename` NOTICE) - only when
    /// the stored verifier starts with `md5` (SCRAM verifiers are kept).
    // CONTRACT
    #[allow(unused_variables)]
    pub fn rename_role(&mut self, role: Oid, new_name: &str) -> Result<()> {
        Err(PgError::unimplemented("CAT-10", "ALTER ROLE RENAME"))
    }

    /// ALTER ROLE `role` SET name = value / RESET (`value = None`): updates `Role::config`.
    // CONTRACT
    #[allow(unused_variables)]
    pub fn set_role_config(&mut self, role: Oid, guc: &str, value: Option<&str>) -> Result<()> {
        Err(PgError::unimplemented("CAT-10", "ALTER ROLE SET"))
    }

    /// DROP ROLE `role` (already resolved): checks in PG order - not the current/session user
    /// (55006), not a builtin role, caller has CREATEROLE + admin (or superuser; dropping a
    /// superuser needs superuser), no owned objects / no ACL grants anywhere (2BP01
    /// `errors::role_cannot_be_dropped`; the role must be the `owner` of nothing: scan relations,
    /// indexes, sequences, types, functions, schemas, databases; also the grantee/grantor in any
    /// ACL). Then removes the role row, all `RoleMembership` rows where it is role or member, its
    /// descriptions, and `Role::config`. Roles that merely HAVE privileges (acl entries) block the
    /// drop with `privileges for <object>` detail lines.
    // CONTRACT
    #[allow(unused_variables)]
    pub fn drop_role(&mut self, role: Oid) -> Result<()> {
        Err(PgError::unimplemented("CAT-10", "DROP ROLE"))
    }

    /// GRANT `spec.role` TO `spec.member` (PG 16+ semantics; rows keyed by (role, member, grantor)):
    /// authorization via `check_role_membership_authorization` (superuser, or admin option on the
    /// role); no self-membership (`role "a" is a member of role "a"` 0LP01); no cycles (0LP01
    /// `errors::circular_membership(role, member)`); granting to a role that already has the
    /// grant from the same grantor updates the options and emits NOTICE `role "m" has already been
    /// granted membership in role "r" by role "g"` (00000? severity NOTICE, SQLSTATE 00000) when
    /// nothing changes. Inserts/updates `RoleMembership{oid, role, member, grantor, admin_option,
    /// inherit_option (default = member's `inherit`), set_option}`. Granting a `pg_` predefined role
    /// is allowed for superusers.
    // CONTRACT
    #[allow(unused_variables)]
    pub fn grant_role(&mut self, spec: &MembershipSpec) -> Result<()> {
        Err(PgError::unimplemented("CAT-10", "GRANT role"))
    }

    /// REVOKE role FROM member [GRANTED BY grantor] (`admin_only` = REVOKE ADMIN OPTION FOR ...,
    /// which only clears the option). Missing membership -> WARNING 01000 `role "m" has not been
    /// granted membership in role "r" by role "g"` (not an error). CASCADE/RESTRICT dependent
    /// grants (grants made by `member` through its admin option) follow PG `DelRoleMems`
    /// (RESTRICT -> 2BP01 `dependent privileges exist` HINT `Use CASCADE to revoke them too.`).
    // CONTRACT
    #[allow(unused_variables)]
    pub fn revoke_role(&mut self, spec: &MembershipSpec, admin_option_only: bool, cascade: bool) -> Result<()> {
        Err(PgError::unimplemented("CAT-10", "REVOKE role"))
    }
}
