//! GRANT / REVOKE privileges on objects. Lease CAT-21. PG sources: `catalog/aclchk.c`
//! (`ExecuteGrantStmt`, `ExecGrant_Relation`, `ExecGrant_Attribute`, `ExecGrant_common`,
//! `objectNamesToOids`, `objectsInSchemaToOids`, `string_to_privilege`, `privilege_to_string`),
//! `utils/adt/acl.c` (`aclupdate`).
//!
//! `CatalogTxn::grant_object_privileges(spec, revoke)` expands the target to its object list
//! (`GrantTarget::AllTablesInSchema(ns)` -> tables, views, matviews, foreign tables of the schema,
//! `AllSequencesInSchema`, `AllFunctionsInSchema`), then for each object:
//! 1. kind check: the privilege bits must be applicable to the kind (`AclObject::all_privileges`);
//!    otherwise 0LP01 `invalid privilege type SELECT for schema` (`errors`: build with
//!    `PgError::new(SqlState(*b"0LP01"), format!("invalid privilege type {PRIV} for {kind_word}"))`,
//!    PRIV upper-case word `SELECT`, `INSERT`, `UPDATE`, `DELETE`, `TRUNCATE`, `REFERENCES`,
//!    `TRIGGER`, `MAINTAIN`, `EXECUTE`, `USAGE`, `CREATE`, `TEMPORARY`, `CONNECT`, `SET`,
//!    `ALTER SYSTEM`). `ALL` expands per kind.
//! 2. sequences accept table-style `GRANT SELECT ON TABLE seq` (privilege set restricted to
//!    `ALL_SEQUENCE`: PG treats USAGE/SELECT/UPDATE; others -> 0LP01 `invalid privilege type INSERT for
//!    sequence`).
//! 3. the ACL change is computed by `acl::grant_privileges`/`revoke_privileges` with
//!    `AclRequest{caller = current_user, grantor = spec.grantor, ...}`; its WARNINGs go to the
//!    outcome notices; the new ACL is stored in the object (`Relation::acl`, `Sequence::acl`,
//!    `TypeObj::acl`, `Function::acl`, `Namespace::acl`, `Database::acl`) or, for column grants
//!    (`spec.columns` non-empty, tables/views only), in `Column::acl` per column. Column-level
//!    REVOKE of table-level privileges follows PG (`ExecGrant_Attribute`; `REVOKE SELECT ON t` also
//!    clears column-level SELECT grants).
//! 4. a GRANT to a non-existent role is the binder's 42704; granting to `0` (PUBLIC) is allowed.
//! Ownership is not required: any role holding the privilege WITH GRANT OPTION may grant it
//! (the grantor-selection step inside `acl::grant_privileges` (PG `select_best_grantor`)); if neither -> 42501 `permission denied for table t` (or
//! `must be owner of`, PG `aclcheck_error` rules: owner-only kinds report `must be owner of`).
//! pg_catalog objects (persisted ACL overrides of builtin relations) are refused: 42501
//! `permission denied: "pg_class" is a system catalog`.
//! `GRANT role TO role` is `role::grant_role`.
use super::spec::*;
use crate::txn::CatalogTxn;
use pgarbor_common::{PgError, Result};

impl<'a> CatalogTxn<'a> {
    /// GRANT (`revoke = false`) or REVOKE (`revoke = true`) privileges (module docs).
    // CONTRACT
    #[allow(unused_variables)]
    pub fn grant_object_privileges(&mut self, spec: &GrantSpec, revoke: bool) -> Result<()> {
        Err(PgError::unimplemented("CAT-21", "GRANT/REVOKE privileges"))
    }
}
