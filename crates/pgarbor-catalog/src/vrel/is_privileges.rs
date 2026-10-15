//! information_schema, part 5 (privileges): `table_privileges`, `column_privileges`,
//! `role_table_grants`, `role_column_grants`, `usage_privileges`, `role_usage_grants`,
//! `routine_privileges`, `role_routine_grants`, `udt_privileges`, `role_udt_grants`,
//! `data_type_privileges`. Lease CAT-44. Authoritative semantics: the CREATE VIEW of each name in PG
//! 18.6 `information_schema.sql` (they all expand `aclexplode(relacl)` / `acldefault` per grantee);
//! conventions: `is_core.rs`.
//!
//! Core operation: for an object with owner `o` and ACL `acl` (`None` = `acl::acldefault`), each
//! `AclItem` is exploded into one row per privilege bit: `grantor` (role name), `grantee` (role name,
//! or the literal `PUBLIC` for grantee 0), privilege_type (`SELECT`, `INSERT`, `UPDATE`, `DELETE`,
//! `TRUNCATE`, `REFERENCES`, `TRIGGER` for tables (oracle: MAINTAIN is NOT listed; the default-ACL table yields exactly 7 rows INSERT, SELECT, UPDATE, DELETE, TRUNCATE, REFERENCES, TRIGGER in that order); `USAGE` for sequences/types/schemas
//! ... per view), `is_grantable` (`YES` if the bit is in `grant_options`), and for table_privileges
//! `with_hierarchy` (`YES` only for SELECT, `NO` otherwise - oracle). Rows are visible to a user who is grantor or grantee (or inherits them:
//! `pg_has_role(grantor, 'USAGE') OR pg_has_role(grantee, 'USAGE') OR grantee = PUBLIC`).
//! `role_*_grants` restrict to grants where the grantor or grantee is an ENABLED role of the current
//! user. `column_privileges` / `role_column_grants` combine table-level ACL bits for SELECT/INSERT/
//! UPDATE/REFERENCES with `Column::acl` entries. `usage_privileges` lists USAGE on sequences, domains
//! (types), schemas? (PG: collations, domains, sequences, user-defined types; FDWs/servers
//! unsupported). `routine_privileges` lists EXECUTE (PUBLIC has it by default on every function).
//! `data_type_privileges` has one row per column/domain/parameter-type usage the user can see with
//! `object_type` (`TABLE`, `DOMAIN`, `ROUTINE`, `USER-DEFINED TYPE`), `dtd_identifier`.
//! Default ACLs matter: a table with `relacl = None` still yields owner rows because the views use
//! `aclexplode(coalesce(relacl, acldefault('r', relowner)))`; the owner's implicit rows are
//! `grantor = owner`, `grantee = owner`, `is_grantable = YES` (the views OR in the ownership right
//! to grant: port the CASE in `table_privileges`).
use super::{Row, ScanContext, ScanRequest, VirtualRelation};
use crate::builtin::relid;
use pgarbor_common::{Oid, PgError, Result};

/// Producer for `information_schema.table_privileges`.
#[derive(Clone, Copy, Debug)]
pub struct IsTablePrivileges;

impl VirtualRelation for IsTablePrivileges {
    fn oid(&self) -> Oid {
        relid::IS_TABLE_PRIVILEGES
    }
    #[allow(unused_variables)]
    fn scan(&self, ctx: &ScanContext, req: &ScanRequest) -> Result<Vec<Row>> {
        Err(PgError::unimplemented("CAT-44", "information_schema.table_privileges rows"))
    }
}

/// Producer for `information_schema.column_privileges`.
#[derive(Clone, Copy, Debug)]
pub struct IsColumnPrivileges;

impl VirtualRelation for IsColumnPrivileges {
    fn oid(&self) -> Oid {
        relid::IS_COLUMN_PRIVILEGES
    }
    #[allow(unused_variables)]
    fn scan(&self, ctx: &ScanContext, req: &ScanRequest) -> Result<Vec<Row>> {
        Err(PgError::unimplemented("CAT-44", "information_schema.column_privileges rows"))
    }
}

/// Producer for `information_schema.role_table_grants`.
#[derive(Clone, Copy, Debug)]
pub struct IsRoleTableGrants;

impl VirtualRelation for IsRoleTableGrants {
    fn oid(&self) -> Oid {
        relid::IS_ROLE_TABLE_GRANTS
    }
    #[allow(unused_variables)]
    fn scan(&self, ctx: &ScanContext, req: &ScanRequest) -> Result<Vec<Row>> {
        Err(PgError::unimplemented("CAT-44", "information_schema.role_table_grants rows"))
    }
}

/// Producer for `information_schema.role_column_grants`.
#[derive(Clone, Copy, Debug)]
pub struct IsRoleColumnGrants;

impl VirtualRelation for IsRoleColumnGrants {
    fn oid(&self) -> Oid {
        relid::IS_ROLE_COLUMN_GRANTS
    }
    #[allow(unused_variables)]
    fn scan(&self, ctx: &ScanContext, req: &ScanRequest) -> Result<Vec<Row>> {
        Err(PgError::unimplemented("CAT-44", "information_schema.role_column_grants rows"))
    }
}

/// Producer for `information_schema.usage_privileges`.
#[derive(Clone, Copy, Debug)]
pub struct IsUsagePrivileges;

impl VirtualRelation for IsUsagePrivileges {
    fn oid(&self) -> Oid {
        relid::IS_USAGE_PRIVILEGES
    }
    #[allow(unused_variables)]
    fn scan(&self, ctx: &ScanContext, req: &ScanRequest) -> Result<Vec<Row>> {
        Err(PgError::unimplemented("CAT-44", "information_schema.usage_privileges rows"))
    }
}

/// Producer for `information_schema.role_usage_grants`.
#[derive(Clone, Copy, Debug)]
pub struct IsRoleUsageGrants;

impl VirtualRelation for IsRoleUsageGrants {
    fn oid(&self) -> Oid {
        relid::IS_ROLE_USAGE_GRANTS
    }
    #[allow(unused_variables)]
    fn scan(&self, ctx: &ScanContext, req: &ScanRequest) -> Result<Vec<Row>> {
        Err(PgError::unimplemented("CAT-44", "information_schema.role_usage_grants rows"))
    }
}

/// Producer for `information_schema.routine_privileges`.
#[derive(Clone, Copy, Debug)]
pub struct IsRoutinePrivileges;

impl VirtualRelation for IsRoutinePrivileges {
    fn oid(&self) -> Oid {
        relid::IS_ROUTINE_PRIVILEGES
    }
    #[allow(unused_variables)]
    fn scan(&self, ctx: &ScanContext, req: &ScanRequest) -> Result<Vec<Row>> {
        Err(PgError::unimplemented("CAT-44", "information_schema.routine_privileges rows"))
    }
}

/// Producer for `information_schema.role_routine_grants`.
#[derive(Clone, Copy, Debug)]
pub struct IsRoleRoutineGrants;

impl VirtualRelation for IsRoleRoutineGrants {
    fn oid(&self) -> Oid {
        relid::IS_ROLE_ROUTINE_GRANTS
    }
    #[allow(unused_variables)]
    fn scan(&self, ctx: &ScanContext, req: &ScanRequest) -> Result<Vec<Row>> {
        Err(PgError::unimplemented("CAT-44", "information_schema.role_routine_grants rows"))
    }
}

/// Producer for `information_schema.udt_privileges`.
#[derive(Clone, Copy, Debug)]
pub struct IsUdtPrivileges;

impl VirtualRelation for IsUdtPrivileges {
    fn oid(&self) -> Oid {
        relid::IS_UDT_PRIVILEGES
    }
    #[allow(unused_variables)]
    fn scan(&self, ctx: &ScanContext, req: &ScanRequest) -> Result<Vec<Row>> {
        Err(PgError::unimplemented("CAT-44", "information_schema.udt_privileges rows"))
    }
}

/// Producer for `information_schema.role_udt_grants`.
#[derive(Clone, Copy, Debug)]
pub struct IsRoleUdtGrants;

impl VirtualRelation for IsRoleUdtGrants {
    fn oid(&self) -> Oid {
        relid::IS_ROLE_UDT_GRANTS
    }
    #[allow(unused_variables)]
    fn scan(&self, ctx: &ScanContext, req: &ScanRequest) -> Result<Vec<Row>> {
        Err(PgError::unimplemented("CAT-44", "information_schema.role_udt_grants rows"))
    }
}

/// Producer for `information_schema.data_type_privileges`.
#[derive(Clone, Copy, Debug)]
pub struct IsDataTypePrivileges;

impl VirtualRelation for IsDataTypePrivileges {
    fn oid(&self) -> Oid {
        relid::IS_DATA_TYPE_PRIVILEGES
    }
    #[allow(unused_variables)]
    fn scan(&self, ctx: &ScanContext, req: &ScanRequest) -> Result<Vec<Row>> {
        Err(PgError::unimplemented("CAT-44", "information_schema.data_type_privileges rows"))
    }
}


/// Registry slice.
pub static RELATIONS: &[&dyn VirtualRelation] = &[
    &IsTablePrivileges,
    &IsColumnPrivileges,
    &IsRoleTableGrants,
    &IsRoleColumnGrants,
    &IsUsagePrivileges,
    &IsRoleUsageGrants,
    &IsRoutinePrivileges,
    &IsRoleRoutineGrants,
    &IsUdtPrivileges,
    &IsRoleUdtGrants,
    &IsDataTypePrivileges,
];
