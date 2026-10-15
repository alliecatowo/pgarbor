//! COMMENT ON, generic ALTER ... OWNER TO dispatch, REASSIGN OWNED, DROP OWNED. Lease CAT-20.
//! PG sources: `commands/comment.c` (`CommentObject`, `CreateComments`), `catalog/aclchk.c`
//! (`shdepReassignOwned` via `pg_shdepend.c`: `shdepReassignOwned`, `shdepDropOwned`),
//! `commands/alter.c` (`ExecAlterOwnerStmt`).
//!
//! ## COMMENT ON
//! Descriptions are persisted as `DescriptionKey{objoid, classoid, objsubid}` -> text
//! (`CatalogTxn::set_description`). Mapping from [`ObjectAddress`]:
//! | address | classoid | objoid | objsubid |
//! |---|---|---|---|
//! | table/view/matview/index/sequence/composite | 1259 | relation oid | 0 |
//! | column | 1259 | relation oid | attnum |
//! | constraint (table or domain) | 2606 | constraint oid | 0 |
//! | type/domain | 1247 | type oid | 0 |
//! | function/procedure | 1255 | function oid | 0 |
//! | schema | 2615 | oid | 0 |
//! | role | 1260 | oid | 0 (PG keeps these in pg_shdescription; same key shape here) |
//! | database | 1262 | oid | 0 |
//! | attrdef (default) | not commentable (42809) | | |
//! `text = None` or `Some("")` deletes the comment (PG: `COMMENT ON ... IS NULL` or `''` removes it).
//! Requires ownership (`require_owner`, 42501 `must be owner of table t`); for columns: owner of the
//! table; system (pg_catalog) objects: superuser only (comments on builtin objects override the
//! compiled-in description; deleting the override re-exposes the builtin text, a documented
//! divergence from PG which would have no comment). Unknown column on a table -> 42703.
//! COMMENT ON CONSTRAINT needs `(table, name)` resolution (binder) - pass the constraint address.
//! Comments disappear when their object is dropped (`drop.rs` calls `clear_descriptions`).
//!
//! ## Ownership dispatch
//! [`CatalogTxn::alter_owner`] routes an address to the typed operation:
//! relation (table, view, matview, sequence, composite) -> `alter_relation_owner`; index -> 42809
//! `cannot change owner of index "i"` HINT `Change the ownership of the index's table instead.`;
//! type/domain -> `alter_type_owner`; function -> `alter_function_owner`; schema/database by OID
//! (looked up to names) -> `alter_schema_owner` / `alter_database_owner`. Unknown -> XX000
//! `cache lookup failed`.
//!
//! ## REASSIGN OWNED BY old TO new
//! Every object whose owner is in `old_roles` gets `new_owner`, in this order: schemas, relations
//! (+ their indexes and owned sequences, which follow the table), types, functions, databases;
//! ACL entries (as grantee or grantor) of `old_roles` are rewritten with `acl::aclnewowner`
//! semantics. Permission: superuser, or member of both `old_roles` and `new_owner`
//! (42501 `permission denied to reassign objects` DETAIL `Only roles with privileges of role "r"
//! may reassign objects owned by it.`). Roles that own nothing are skipped. Pinned/builtin objects
//! are never reassigned (owner `postgres` rows of system catalogs are not persisted).
//!
//! ## DROP OWNED BY roles [CASCADE]
//! Drops every object owned by the roles (through `drop_objects` with the given behavior, so dependent
//! objects follow the usual RESTRICT/CASCADE rules), then revokes every privilege granted to or
//! by the roles (`acl::revoke_privileges` per object, cascade when `behavior == Cascade`) and
//! removes memberships they hold/granted (not the roles themselves). Owned schemas ARE dropped (as PG
//! does); the database itself is never dropped. Permission as REASSIGN.
use crate::model::*;
use crate::txn::CatalogTxn;
use pgarbor_common::{Oid, PgError, Result};

impl<'a> CatalogTxn<'a> {
    /// COMMENT ON (module docs).
    // CONTRACT
    #[allow(unused_variables)]
    pub fn comment_on(&mut self, addr: &ObjectAddress, text: Option<&str>) -> Result<()> {
        Err(PgError::unimplemented("CAT-20", "COMMENT ON"))
    }

    /// The `DescriptionKey` for an address (module docs table); `None` for non-commentable classes.
    pub fn description_key_for(addr: &ObjectAddress) -> Option<DescriptionKey> {
        let classoid = match addr.class {
            ObjClass::Relation | ObjClass::Type | ObjClass::Proc | ObjClass::Namespace | ObjClass::Constraint
            | ObjClass::Role | ObjClass::Database => addr.class.classid(),
            _ => return None,
        };
        Some(DescriptionKey { objoid: addr.oid, classoid, objsubid: addr.sub_id })
    }

    /// ALTER ... OWNER TO dispatch (module docs).
    // CONTRACT
    #[allow(unused_variables)]
    pub fn alter_owner(&mut self, addr: &ObjectAddress, new_owner: Oid) -> Result<()> {
        Err(PgError::unimplemented("CAT-20", "ALTER OWNER dispatch"))
    }

    /// REASSIGN OWNED BY old_roles TO new_owner.
    // CONTRACT
    #[allow(unused_variables)]
    pub fn reassign_owned(&mut self, old_roles: &[Oid], new_owner: Oid) -> Result<()> {
        Err(PgError::unimplemented("CAT-20", "REASSIGN OWNED"))
    }

    /// DROP OWNED BY roles [CASCADE|RESTRICT].
    // CONTRACT
    #[allow(unused_variables)]
    pub fn drop_owned(&mut self, roles: &[Oid], behavior: DropBehavior) -> Result<()> {
        Err(PgError::unimplemented("CAT-20", "DROP OWNED"))
    }
}
