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
use crate::acl::{aclnewowner, revoke_privileges, AclObject, AclRequest, RoleGraph};
use crate::builtin;
use crate::depend::{self, SharedDepKind};
use crate::errors;
use crate::model::*;
use crate::snapshot::{CatalogSnapshot, ClassRef};
use crate::txn::CatalogTxn;
use pgarbor_common::{Oid, PgError, Result, SqlState};
use std::collections::BTreeSet;

/// The ACL-bearing object a privilege revocation works on: where its ACL lives and what it is called.
struct AclSlot {
    /// Whole-object address (sub_id cleared).
    addr: ObjectAddress,
    owner: Oid,
    kind: AclObject,
    name: String,
    acl: Option<Acl>,
}

/// PG `get_relkind_objtype` words used in ownership messages.
fn relkind_word(kind: RelKind) -> &'static str {
    match kind {
        RelKind::Table | RelKind::PartitionedTable | RelKind::ForeignTable | RelKind::Toast => "table",
        RelKind::View => "view",
        RelKind::MatView => "materialized view",
        RelKind::Index | RelKind::PartitionedIndex => "index",
        RelKind::Sequence => "sequence",
        RelKind::CompositeType => "type",
    }
}

/// Name of a role for messages (XX000 when the OID is unknown).
fn role_name(snap: &CatalogSnapshot, oid: Oid) -> Result<String> {
    snap.role_name(oid).map(str::to_string).ok_or_else(|| errors::cache_lookup_failed("role", oid))
}

/// Existence check for an address (builtin objects that the snapshot does not index are accepted
/// when they are pinned OIDs, see the module notes in the report).
fn ensure_exists(snap: &CatalogSnapshot, addr: &ObjectAddress) -> Result<()> {
    let (found, what) = match addr.class {
        ObjClass::Relation => (snap.class(addr.oid).is_some(), "relation"),
        ObjClass::Type => (snap.user_type(addr.oid).is_some() || addr.is_pinned(), "type"),
        ObjClass::Namespace => {
            (snap.namespace(addr.oid).is_some() || builtin::builtin_namespace(addr.oid).is_some(), "namespace")
        }
        ObjClass::Constraint => (snap.constraint(addr.oid).is_some(), "constraint"),
        ObjClass::Proc => (snap.function(addr.oid).is_some() || addr.is_pinned(), "function"),
        ObjClass::Role => (snap.role(addr.oid).is_some() || builtin::builtin_role(addr.oid).is_some(), "role"),
        ObjClass::Database => {
            (snap.database(addr.oid).is_some() || builtin::builtin_database(addr.oid).is_some(), "database")
        }
        _ => (true, "object"),
    };
    if found {
        Ok(())
    } else {
        Err(errors::cache_lookup_failed(what, addr.oid))
    }
}

/// 42703 for a column address whose attnum is not a live column of the relation.
fn ensure_column_live(snap: &CatalogSnapshot, addr: &ObjectAddress) -> Result<()> {
    let attnum = addr.sub_id as i16;
    let live = snap.relation(addr.oid).and_then(|r| r.column(attnum)).is_some_and(|c| !c.dropped);
    if live {
        Ok(())
    } else {
        Err(PgError::new(SqlState::UNDEFINED_COLUMN, format!("column {} of relation {} does not exist", attnum, addr.oid)))
    }
}

/// 42501 `must be owner of <kind> <name>` for an address (used for the system-object rule).
fn owner_error(snap: &CatalogSnapshot, addr: &ObjectAddress) -> PgError {
    let (word, name): (&str, String) = match addr.class {
        ObjClass::Relation => match snap.class_identity(addr.oid) {
            Some((_, n, k)) => (relkind_word(k), n.to_string()),
            None => ("table", addr.oid.to_string()),
        },
        ObjClass::Type => ("type", snap.type_obj(addr.oid).map(|t| t.name.clone()).unwrap_or_else(|| addr.oid.to_string())),
        ObjClass::Namespace => {
            ("schema", snap.namespace(addr.oid).map(|n| n.name.clone()).unwrap_or_else(|| addr.oid.to_string()))
        }
        ObjClass::Proc => ("function", snap.function(addr.oid).map(|f| f.name.clone()).unwrap_or_else(|| addr.oid.to_string())),
        ObjClass::Database => {
            ("database", snap.database(addr.oid).map(|d| d.name.clone()).unwrap_or_else(|| addr.oid.to_string()))
        }
        ObjClass::Role => ("role", snap.role_name(addr.oid).map(str::to_string).unwrap_or_else(|| addr.oid.to_string())),
        _ => ("object", addr.oid.to_string()),
    };
    errors::must_be_owner(word, &name)
}

impl<'a> CatalogTxn<'a> {
    /// COMMENT ON (module docs).
    // CONTRACT
    pub fn comment_on(&mut self, addr: &ObjectAddress, text: Option<&str>) -> Result<()> {
        let key = Self::description_key_for(addr)
            .ok_or_else(|| PgError::new(SqlState::WRONG_OBJECT_TYPE, "cannot comment on this object type"))?;
        let snap = self.snapshot()?;
        ensure_exists(&snap, addr)?;
        if addr.is_column() {
            ensure_column_live(&snap, addr)?;
        }
        self.require_owner(addr)?;
        if addr.is_pinned() && !self.current_is_superuser(&snap) {
            return Err(owner_error(&snap, addr));
        }
        // Empty text deletes the row (handled by set_description).
        self.set_description(key, text)
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
    pub fn alter_owner(&mut self, addr: &ObjectAddress, new_owner: Oid) -> Result<()> {
        let snap = self.snapshot()?;
        ensure_exists(&snap, addr)?;
        match addr.class {
            ObjClass::Relation => match snap.class(addr.oid) {
                Some(ClassRef::Index(oid)) | Some(ClassRef::SystemIndex(oid)) => {
                    let name = snap
                        .class_identity(oid)
                        .map(|(_, n, _)| n.to_string())
                        .unwrap_or_else(|| oid.to_string());
                    Err(PgError::new(SqlState::WRONG_OBJECT_TYPE, format!("cannot change owner of index \"{name}\""))
                        .with_hint("Change the ownership of the index's table instead."))
                }
                Some(ClassRef::Relation(oid)) | Some(ClassRef::Sequence(oid)) => self.alter_relation_owner(oid, new_owner),
                None => Err(errors::cache_lookup_failed("relation", addr.oid)),
            },
            ObjClass::Type => self.alter_type_owner(addr.oid, new_owner),
            ObjClass::Proc => self.alter_function_owner(addr.oid, new_owner),
            ObjClass::Namespace => {
                let name = snap
                    .namespace(addr.oid)
                    .map(|n| n.name.clone())
                    .ok_or_else(|| errors::cache_lookup_failed("namespace", addr.oid))?;
                self.alter_schema_owner(&name, new_owner)
            }
            ObjClass::Database => {
                let name = snap
                    .database(addr.oid)
                    .map(|d| d.name.clone())
                    .ok_or_else(|| errors::cache_lookup_failed("database", addr.oid))?;
                self.alter_database_owner(&name, new_owner)
            }
            other => Err(PgError::internal(format!("unrecognized object class {other:?} for ALTER OWNER"))),
        }
    }

    /// REASSIGN OWNED BY old_roles TO new_owner.
    // CONTRACT
    pub fn reassign_owned(&mut self, old_roles: &[Oid], new_owner: Oid) -> Result<()> {
        self.check_reassign_permission(old_roles, Some(new_owner))?;

        // 1. schemas (PG order: schemas, relations, types, functions, databases).
        let schemas: Vec<String> = self
            .data()
            .namespaces
            .values()
            .filter(|n| old_roles.contains(&n.owner))
            .map(|n| n.name.clone())
            .collect();
        for name in schemas {
            self.alter_schema_owner(&name, new_owner)?;
        }

        // 2. relations and sequences. Changing a table's owner also moves its owned sequences, so each
        // candidate is re-checked against the current owner before its turn.
        let rels: Vec<Oid> =
            self.data().relations.values().filter(|r| old_roles.contains(&r.owner)).map(|r| r.oid).collect();
        let seqs: Vec<Oid> =
            self.data().sequences.values().filter(|s| old_roles.contains(&s.owner)).map(|s| s.oid).collect();
        for oid in rels.into_iter().chain(seqs) {
            if self.relation_like_owner(oid).is_some_and(|o| old_roles.contains(&o)) {
                self.alter_relation_owner(oid, new_owner)?;
            }
        }

        // 3. indexes follow their table: PG never reassigns an index on its own.
        let idxs: Vec<Oid> =
            self.data().indexes.values().filter(|i| old_roles.contains(&i.owner)).map(|i| i.oid).collect();
        for oid in idxs {
            if self.relation_like_owner(oid).is_some_and(|o| old_roles.contains(&o)) {
                self.update_index(oid, |i| {
                    i.owner = new_owner;
                    Ok(())
                })?;
            }
        }

        // 4. types, functions, databases.
        let types: Vec<Oid> = self.data().types.values().filter(|t| old_roles.contains(&t.owner)).map(|t| t.oid).collect();
        for oid in types {
            self.alter_type_owner(oid, new_owner)?;
        }
        let funcs: Vec<Oid> =
            self.data().functions.values().filter(|f| old_roles.contains(&f.owner)).map(|f| f.oid).collect();
        for oid in funcs {
            self.alter_function_owner(oid, new_owner)?;
        }
        let dbs: Vec<String> = self
            .data()
            .databases
            .values()
            .filter(|d| old_roles.contains(&d.owner))
            .map(|d| d.name.clone())
            .collect();
        for name in dbs {
            self.alter_database_owner(&name, new_owner)?;
        }

        // 5. ACL entries that name an old role (as grantee or grantor) are rewritten.
        self.rewrite_acls_for_reassign(old_roles, new_owner)
    }

    /// DROP OWNED BY roles [CASCADE|RESTRICT].
    // CONTRACT
    pub fn drop_owned(&mut self, roles: &[Oid], behavior: DropBehavior) -> Result<()> {
        self.check_reassign_permission(roles, None)?;
        let cascade = matches!(behavior, DropBehavior::Cascade);

        // 1. Owned objects. Databases are never dropped (PG only removes local objects and role grants).
        let mut targets: BTreeSet<ObjectAddress> = BTreeSet::new();
        {
            let env = self.env()?;
            for &role in roles {
                for dep in depend::role_dependencies(env.snapshot(), role)? {
                    if dep.kind == SharedDepKind::Owner && dep.object.class != ObjClass::Database {
                        targets.insert(dep.object.whole());
                    }
                }
            }
        }
        if !targets.is_empty() {
            let list: Vec<ObjectAddress> = targets.into_iter().collect();
            self.drop_objects(&list, behavior)?;
        }

        // 2. Privileges granted to or by the roles, on the objects that survived step 1.
        let env = self.env()?;
        let mut acl_addrs: BTreeSet<ObjectAddress> = BTreeSet::new();
        for &role in roles {
            for dep in depend::role_dependencies(env.snapshot(), role)? {
                if dep.kind == SharedDepKind::Acl {
                    // Column ACLs keep their sub_id: they live in the column row, not the table ACL.
                    acl_addrs.insert(dep.object);
                }
            }
        }
        for addr in acl_addrs {
            let slot = if addr.is_column() {
                match self.column_acl_slot(&addr) {
                    Some(s) => s,
                    None => continue,
                }
            } else {
                match self.acl_slot(&addr) {
                    Some(s) => s,
                    None => continue,
                }
            };
            let original = match slot.acl.clone() {
                Some(a) => a,
                None => continue,
            };
            let items: Vec<AclItem> = original
                .iter()
                .filter(|i| roles.contains(&i.grantee) || roles.contains(&i.grantor))
                .cloned()
                .collect();
            if items.is_empty() {
                continue;
            }
            let mut working: Option<Acl> = Some(original.clone());
            for item in items {
                // Each entry is revoked as its grantor (PG RemoveRoleFromObjectACL). PG removes these
                // entries silently, so the revoke's warnings are not raised here.
                let change = revoke_privileges(&AclRequest {
                    snapshot: env.snapshot(),
                    kind: slot.kind,
                    object_name: &slot.name,
                    owner: slot.owner,
                    current: working.as_ref(),
                    caller: item.grantor,
                    grantor: item.grantor,
                    privileges: item.privileges,
                    grantees: std::slice::from_ref(&item.grantee),
                    grant_option: false,
                    cascade,
                })?;
                working = change.acl;
            }
            if working.as_ref() != Some(&original) {
                if slot.addr.is_column() {
                    let attnum = slot.addr.sub_id;
                    self.update_relation(slot.addr.oid, |r| {
                        if let Some(c) = r.columns.iter_mut().find(|c| i32::from(c.attnum) == attnum) {
                            c.acl = working;
                        }
                        Ok(())
                    })?;
                } else {
                    self.store_acl(&slot.addr, working)?;
                }
            }
        }

        // 3. Role memberships the roles hold or granted (the roles themselves stay).
        let memberships: Vec<Oid> = self
            .data()
            .memberships
            .values()
            .filter(|m| roles.contains(&m.member) || roles.contains(&m.grantor))
            .map(|m| m.oid)
            .collect();
        for oid in memberships {
            self.remove(ObjKind::RoleMembership, oid)?;
        }
        Ok(())
    }

    // ------------------------------------------------------------------ private helpers

    /// True when the current role is a superuser (per the snapshot's role row).
    fn current_is_superuser(&self, snap: &CatalogSnapshot) -> bool {
        snap.role(self.current_user()).is_some_and(|r| r.superuser)
    }

    /// Owner of a pg_class-like object in the working data (relation, sequence or index).
    fn relation_like_owner(&self, oid: Oid) -> Option<Oid> {
        let d = self.data();
        d.relations
            .get(&oid)
            .map(|r| r.owner)
            .or_else(|| d.sequences.get(&oid).map(|s| s.owner))
            .or_else(|| d.indexes.get(&oid).map(|i| i.owner))
    }

    /// Permission for REASSIGN OWNED (`new_owner = Some`) and DROP OWNED (`None`): superuser, or the
    /// current role has the privileges of every old role and is a member of the new owner.
    fn check_reassign_permission(&mut self, old_roles: &[Oid], new_owner: Option<Oid>) -> Result<()> {
        let env = self.env()?;
        let snap = env.snapshot();
        let cur = self.current_user();
        let graph = RoleGraph::build(snap, env.database())?;
        if graph.is_superuser(cur) {
            return Ok(());
        }
        for &old in old_roles {
            if !graph.has_privs_of_role(cur, old)? {
                let name = role_name(snap, old)?;
                return Err(PgError::new(SqlState::INSUFFICIENT_PRIVILEGE, "permission denied to reassign objects")
                    .with_detail(format!("Only roles with privileges of role \"{name}\" may reassign objects owned by it.")));
            }
        }
        if let Some(new) = new_owner {
            if !graph.is_member_of_role(cur, new)? {
                let name = role_name(snap, new)?;
                return Err(PgError::new(SqlState::INSUFFICIENT_PRIVILEGE, format!("must be member of role \"{name}\"")));
            }
        }
        Ok(())
    }

    /// Every object that carries an ACL in the working data (schemas, tables/views/matviews,
    /// sequences, enums/domains, functions, databases).
    fn acl_bearing_objects(&self) -> Vec<ObjectAddress> {
        let d = self.data();
        let mut v = Vec::new();
        v.extend(d.namespaces.keys().map(|o| ObjectAddress::namespace(*o)));
        v.extend(d.relations.keys().map(|o| ObjectAddress::relation(*o)));
        v.extend(d.sequences.keys().map(|o| ObjectAddress::new(ObjClass::Relation, *o)));
        v.extend(d.types.keys().map(|o| ObjectAddress::type_(*o)));
        v.extend(d.functions.keys().map(|o| ObjectAddress::proc(*o)));
        v.extend(d.databases.keys().map(|o| ObjectAddress::new(ObjClass::Database, *o)));
        v
    }

    /// Where the ACL of an address lives (None for objects without ACL storage: indexes, builtins,
    /// constraints, ...). Column addresses resolve to their table.
    /// ACL slot of a column address (`sub_id` = attnum): the owner is the relation's owner and the ACL
    /// is the column's own (PG `attacl`). Unknown or dropped columns have no slot.
    fn column_acl_slot(&self, addr: &ObjectAddress) -> Option<AclSlot> {
        let r = self.data().relations.get(&addr.oid)?;
        let c = r.columns.iter().find(|c| i32::from(c.attnum) == addr.sub_id && !c.dropped)?;
        Some(AclSlot { addr: *addr, owner: r.owner, kind: AclObject::Column, name: c.name.clone(), acl: c.acl.clone() })
    }

    fn acl_slot(&self, addr: &ObjectAddress) -> Option<AclSlot> {
        let d = self.data();
        let whole = addr.whole();
        let (owner, kind, name, acl) = match addr.class {
            ObjClass::Namespace => {
                let n = d.namespaces.get(&addr.oid)?;
                (n.owner, AclObject::Schema, n.name.clone(), n.acl.clone())
            }
            ObjClass::Relation => match d.relations.get(&addr.oid) {
                Some(r) => (r.owner, AclObject::Table, r.name.clone(), r.acl.clone()),
                None => {
                    let s = d.sequences.get(&addr.oid)?;
                    (s.owner, AclObject::Sequence, s.name.clone(), s.acl.clone())
                }
            },
            ObjClass::Type => {
                let t = d.types.get(&addr.oid)?;
                (t.owner, AclObject::Type, t.name.clone(), t.acl.clone())
            }
            ObjClass::Proc => {
                let f = d.functions.get(&addr.oid)?;
                (f.owner, AclObject::Function, f.name.clone(), f.acl.clone())
            }
            ObjClass::Database => {
                let db = d.databases.get(&addr.oid)?;
                (db.owner, AclObject::Database, db.name.clone(), db.acl.clone())
            }
            _ => return None,
        };
        Some(AclSlot { addr: whole, owner, kind, name, acl })
    }

    /// Write back the ACL of an address (`None` = equal to the default).
    fn store_acl(&mut self, addr: &ObjectAddress, acl: Option<Acl>) -> Result<()> {
        match addr.class {
            ObjClass::Namespace => self.update_namespace(addr.oid, |n| {
                n.acl = acl;
                Ok(())
            }),
            ObjClass::Relation => {
                if self.data().relations.contains_key(&addr.oid) {
                    self.update_relation(addr.oid, |r| {
                        r.acl = acl;
                        Ok(())
                    })
                } else {
                    self.update_sequence(addr.oid, |s| {
                        s.acl = acl;
                        Ok(())
                    })
                }
            }
            ObjClass::Type => self.update_type(addr.oid, |t| {
                t.acl = acl;
                Ok(())
            }),
            ObjClass::Proc => self.update_function(addr.oid, |f| {
                f.acl = acl;
                Ok(())
            }),
            ObjClass::Database => self.update_database(addr.oid, |d| {
                d.acl = acl;
                Ok(())
            }),
            _ => Ok(()),
        }
    }

    /// ACL rewrite of REASSIGN OWNED: every ACL that names an old role gets `aclnewowner` applied.
    fn rewrite_acls_for_reassign(&mut self, old_roles: &[Oid], new_owner: Oid) -> Result<()> {
        for addr in self.acl_bearing_objects() {
            let slot = match self.acl_slot(&addr) {
                Some(s) => s,
                None => continue,
            };
            let original = match slot.acl.clone() {
                Some(a) => a,
                None => continue,
            };
            let mut rewritten = original.clone();
            for &old in old_roles {
                if rewritten.iter().any(|i| i.grantee == old || i.grantor == old) {
                    rewritten = aclnewowner(&rewritten, old, new_owner)?;
                }
            }
            if rewritten != original {
                self.store_acl(&slot.addr, Some(rewritten))?;
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pgarbor_common::FIRST_NORMAL_OID;

    #[test]
    fn column_comment_key_uses_attnum_as_objsubid() {
        let key = CatalogTxn::description_key_for(&ObjectAddress::column(16400, 3)).unwrap();
        assert_eq!(key, DescriptionKey { objoid: 16400, classoid: 1259, objsubid: 3 });
    }

    #[test]
    fn relation_and_whole_object_keys_have_subid_zero() {
        let key = CatalogTxn::description_key_for(&ObjectAddress::relation(16400)).unwrap();
        assert_eq!(key, DescriptionKey { objoid: 16400, classoid: 1259, objsubid: 0 });
        let key = CatalogTxn::description_key_for(&ObjectAddress::new(ObjClass::Constraint, 16401)).unwrap();
        assert_eq!(key, DescriptionKey { objoid: 16401, classoid: 2606, objsubid: 0 });
    }

    #[test]
    fn class_oids_match_pg_catalog() {
        let cases = [
            (ObjectAddress::type_(23), 1247),
            (ObjectAddress::proc(16402), 1255),
            (ObjectAddress::namespace(2200), 2615),
            (ObjectAddress::new(ObjClass::Role, 10), 1260),
            (ObjectAddress::new(ObjClass::Database, 5), 1262),
        ];
        for (addr, classoid) in cases {
            let key = CatalogTxn::description_key_for(&addr).unwrap();
            assert_eq!(key.classoid, classoid);
            assert_eq!(key.objoid, addr.oid);
            assert_eq!(key.objsubid, 0);
        }
    }

    #[test]
    fn non_commentable_classes_have_no_key() {
        assert!(CatalogTxn::description_key_for(&ObjectAddress::new(ObjClass::AttrDefault, 16403)).is_none());
        assert!(CatalogTxn::description_key_for(&ObjectAddress::new(ObjClass::Collation, 100)).is_none());
    }

    #[test]
    fn pinned_addresses_are_below_first_normal_oid() {
        assert!(ObjectAddress::type_(23).oid < FIRST_NORMAL_OID);
        assert!(ObjectAddress::type_(23).is_pinned());
        assert!(!ObjectAddress::relation(FIRST_NORMAL_OID).is_pinned());
    }
}
