//! Generic DDL for the extensibility objects of amendment pgarbor-catalog-09 (`ExtObject`):
//! ALTER ... OWNER TO / RENAME TO / SET SCHEMA and the drop of one object (called by `drop.rs`).
//! PG sources: `commands/alter.c` (`AlterObjectOwner_internal`, `AlterObjectRename_internal`,
//! `AlterObjectNamespace_internal`), `commands/collationcmds.c` (`IsThereCollationInNamespace`),
//! `commands/opclasscmds.c` (`IsThereOpClassInNamespace`, `IsThereOpFamilyInNamespace`),
//! `catalog/pg_operator.c` (`OperatorUpd` on drop).
//!
//! Permission order (PG): superusers skip every check; otherwise the current user must have the
//! privileges of the owner (42501 `must be owner of operator class x`), then for OWNER TO must be able
//! to SET ROLE to the new owner and the NEW owner needs CREATE on the object's schema; RENAME needs
//! CREATE on the schema for the current user; SET SCHEMA needs CREATE on the target schema.
//! Name conflicts: collation 42710 `collation "x" for encoding "UTF8" already exists in schema "s"`
//! (or the any-encoding form), operator class/family 42710 `operator class "x" for access method
//! "btree" already exists in schema "s"`, conversion 42710 `conversion "x" already exists in schema
//! "s"`, operator (no friendly check in PG) 23505 on `pg_operator_oprname_l_r_n_index`.
use crate::acl::{self, AclObject, RoleGraph};
use crate::builtin;
use crate::ddl::schema::check_can_set_role;
use crate::errors;
use crate::model::*;
use crate::txn::CatalogTxn;
use pgarbor_common::{Oid, PgError, Result, SqlState};

impl<'a> CatalogTxn<'a> {
    /// Write (insert or replace) an extensibility object.
    pub fn put_ext(&mut self, obj: ExtObject) -> Result<()> {
        self.put(CatalogObject::Ext(obj))
    }

    /// A persisted extensibility object by key OID.
    pub fn ext_object(&self, oid: Oid) -> Option<ExtObject> {
        self.data().ext.get(&oid).cloned()
    }

    /// Current user is a superuser.
    pub fn current_is_superuser_ext(&mut self) -> Result<bool> {
        let cur = self.current_user();
        self.is_superuser(cur)
    }

    /// CREATE privilege of `role` on schema `ns` (42501 `permission denied for schema s`).
    pub fn require_schema_create_for(&mut self, role: Oid, ns: Oid) -> Result<()> {
        if self.is_superuser(role)? {
            return Ok(());
        }
        let snap = self.snapshot()?;
        let schema = snap.namespace(ns).ok_or_else(|| errors::cache_lookup_failed("schema", ns))?;
        let eff = acl::effective_acl(AclObject::Schema, schema.owner, schema.acl.as_ref())?;
        let env = self.env()?;
        let graph = RoleGraph::build(&snap, env.database())?;
        if acl::has_privilege(&graph, role, schema.owner, &eff, acl_mode::CREATE)? {
            Ok(())
        } else {
            Err(errors::permission_denied_for("schema", &schema.name))
        }
    }

    fn schema_name(&mut self, ns: Oid) -> Result<String> {
        let snap = self.snapshot()?;
        Ok(snap.namespace(ns).map(|n| n.name.clone()).unwrap_or_else(|| ns.to_string()))
    }

    /// `(namespace, owner)` of a user extensibility object, or XX000 for builtin/missing ones.
    fn ext_ns_owner(&self, addr: &ObjectAddress) -> Result<(Oid, Oid)> {
        match self.data().ext.get(&addr.oid) {
            Some(ExtObject::Operator(o)) => Ok((o.namespace, o.owner)),
            Some(ExtObject::OpClass(c)) => Ok((c.namespace, c.owner)),
            Some(ExtObject::OpFamily(f)) => Ok((f.namespace, f.owner)),
            Some(ExtObject::Conversion(c)) => Ok((c.namespace, c.owner)),
            Some(ExtObject::Collation(c)) => Ok((c.namespace, c.owner)),
            Some(ExtObject::Extension(e)) => Ok((e.namespace, e.owner)),
            _ => Err(errors::cache_lookup_failed("object", addr.oid)),
        }
    }

    /// Name-conflict check of PG `IsThere*InNamespace` for `addr` renamed to `name` in `ns`.
    fn check_ext_name_free(&mut self, addr: &ObjectAddress, name: &str, ns: Oid) -> Result<()> {
        let env = self.env()?;
        let ns_name = self.schema_name(ns)?;
        match self.data().ext.get(&addr.oid).cloned() {
            Some(ExtObject::Collation(_)) => {
                let utf8 = |c: i32| c == builtin::UTF8_ENCODING;
                let in_ns = |enc_ok: &dyn Fn(i32) -> bool| -> bool {
                    (ns == builtin::PG_CATALOG_NAMESPACE
                        && builtin::COLLATIONS.iter().any(|c| c.name == name && enc_ok(c.encoding)))
                        || env.snapshot().user_collations().any(|c| c.namespace == ns && c.name == name && enc_ok(c.encoding))
                };
                if in_ns(&utf8) {
                    return Err(PgError::new(
                        SqlState::DUPLICATE_OBJECT,
                        format!("collation \"{name}\" for encoding \"UTF8\" already exists in schema \"{ns_name}\""),
                    ));
                }
                if in_ns(&|c| c == -1) {
                    return Err(PgError::new(
                        SqlState::DUPLICATE_OBJECT,
                        format!("collation \"{name}\" already exists in schema \"{ns_name}\""),
                    ));
                }
            }
            Some(ExtObject::OpClass(c)) => {
                if env.all_opclasses().iter().any(|o| o.namespace == ns && o.method == c.method && o.name == name) {
                    let am = env.access_method_name(c.method).unwrap_or("?");
                    return Err(PgError::new(
                        SqlState::DUPLICATE_OBJECT,
                        format!("operator class \"{name}\" for access method \"{am}\" already exists in schema \"{ns_name}\""),
                    ));
                }
            }
            Some(ExtObject::OpFamily(f)) => {
                if env.all_opfamilies().iter().any(|o| o.namespace == ns && o.method == f.method && o.name == name) {
                    let am = env.access_method_name(f.method).unwrap_or("?");
                    return Err(PgError::new(
                        SqlState::DUPLICATE_OBJECT,
                        format!(
                            "operator family \"{name}\" for access method \"{am}\" already exists in schema \"{ns_name}\""
                        ),
                    ));
                }
            }
            Some(ExtObject::Conversion(_)) => {
                if env.all_conversions().iter().any(|c| c.namespace == ns && c.name == name) {
                    return Err(PgError::new(
                        SqlState::DUPLICATE_OBJECT,
                        format!("conversion \"{name}\" already exists in schema \"{ns_name}\""),
                    ));
                }
            }
            Some(ExtObject::Operator(o)) => {
                if env.lookup_operator_opt(Some(&ns_name), name, o.left, o.right).is_some_and(|f| f != o.oid) {
                    return Err(PgError::new(
                        SqlState::UNIQUE_VIOLATION,
                        "duplicate key value violates unique constraint \"pg_operator_oprname_l_r_n_index\"",
                    )
                    .with_detail(format!(
                        "Key (oprname, oprleft, oprright, oprnamespace)=({name}, {}, {}, {ns}) already exists.",
                        o.left, o.right
                    )));
                }
            }
            _ => {}
        }
        Ok(())
    }

    /// Generic ALTER ... OWNER TO for operators, operator classes/families, conversions, collations.
    pub fn alter_ext_owner(&mut self, addr: &ObjectAddress, new_owner: Oid) -> Result<()> {
        let (ns, old_owner) = self.ext_ns_owner(addr)?;
        if old_owner == new_owner {
            return Ok(());
        }
        let cur = self.current_user();
        if !self.is_superuser(cur)? {
            self.require_owner(addr)?;
            let snap = self.snapshot()?;
            let env = self.env()?;
            let graph = RoleGraph::build(&snap, env.database())?;
            check_can_set_role(&snap, &graph, cur, new_owner)?;
            if !matches!(addr.class, ObjClass::Extension) {
                self.require_schema_create_for(new_owner, ns)?;
            }
        }
        let Some(mut obj) = self.ext_object(addr.oid) else {
            return Err(errors::cache_lookup_failed("object", addr.oid));
        };
        match &mut obj {
            ExtObject::Operator(o) => o.owner = new_owner,
            ExtObject::OpClass(c) => c.owner = new_owner,
            ExtObject::OpFamily(f) => f.owner = new_owner,
            ExtObject::Conversion(c) => c.owner = new_owner,
            ExtObject::Collation(c) => c.owner = new_owner,
            ExtObject::Extension(e) => e.owner = new_owner,
            _ => return Err(PgError::internal("ALTER OWNER on an object without owner")),
        }
        self.put_ext(obj)
    }

    /// Generic ALTER ... RENAME TO for operator classes/families, conversions, collations.
    pub fn rename_ext(&mut self, addr: &ObjectAddress, new_name: &str) -> Result<()> {
        let (ns, _) = self.ext_ns_owner(addr)?;
        let cur = self.current_user();
        if !self.is_superuser(cur)? {
            self.require_owner(addr)?;
            self.require_schema_create_for(cur, ns)?;
        }
        self.check_ext_name_free(addr, new_name, ns)?;
        let Some(mut obj) = self.ext_object(addr.oid) else {
            return Err(errors::cache_lookup_failed("object", addr.oid));
        };
        match &mut obj {
            ExtObject::OpClass(c) => c.name = new_name.to_string(),
            ExtObject::OpFamily(f) => f.name = new_name.to_string(),
            ExtObject::Conversion(c) => c.name = new_name.to_string(),
            ExtObject::Collation(c) => c.name = new_name.to_string(),
            ExtObject::Extension(e) => e.name = new_name.to_string(),
            ExtObject::Operator(o) => o.name = new_name.to_string(),
            _ => return Err(PgError::internal("RENAME on an object without a name")),
        }
        self.put_ext(obj)
    }

    /// Generic ALTER ... SET SCHEMA for operators, operator classes/families, conversions,
    /// collations. Same schema: no-op (no checks, like PG).
    pub fn set_ext_schema(&mut self, addr: &ObjectAddress, new_ns: Oid) -> Result<()> {
        let (old_ns, _) = self.ext_ns_owner(addr)?;
        if old_ns == new_ns {
            return Ok(());
        }
        self.check_set_namespace(old_ns, new_ns)?;
        let cur = self.current_user();
        if !self.is_superuser(cur)? {
            self.require_owner(addr)?;
            self.require_schema_create_for(cur, new_ns)?;
        }
        let name = match self.data().ext.get(&addr.oid) {
            Some(ExtObject::Operator(o)) => o.name.clone(),
            Some(ExtObject::OpClass(c)) => c.name.clone(),
            Some(ExtObject::OpFamily(f)) => f.name.clone(),
            Some(ExtObject::Conversion(c)) => c.name.clone(),
            Some(ExtObject::Collation(c)) => c.name.clone(),
            _ => return Err(errors::cache_lookup_failed("object", addr.oid)),
        };
        self.check_ext_name_free(addr, &name, new_ns)?;
        let Some(mut obj) = self.ext_object(addr.oid) else {
            return Err(errors::cache_lookup_failed("object", addr.oid));
        };
        match &mut obj {
            ExtObject::Operator(o) => o.namespace = new_ns,
            ExtObject::OpClass(c) => c.namespace = new_ns,
            ExtObject::OpFamily(f) => f.namespace = new_ns,
            ExtObject::Conversion(c) => c.namespace = new_ns,
            ExtObject::Collation(c) => c.namespace = new_ns,
            _ => return Err(PgError::internal("SET SCHEMA on an object without schema")),
        }
        self.put_ext(obj)
    }

    /// PG `CheckSetNamespace`: temp and TOAST schemas cannot take or give objects.
    pub fn check_set_namespace(&mut self, old_ns: Oid, new_ns: Oid) -> Result<()> {
        let env = self.env()?;
        let temp = env.path().temp_namespace;
        let is_temp = |ns: Oid| -> bool {
            Some(ns) == temp || env.snapshot().namespace(ns).is_some_and(|n| n.name.starts_with("pg_temp"))
        };
        if is_temp(old_ns) || is_temp(new_ns) {
            return Err(PgError::new(SqlState::FEATURE_NOT_SUPPORTED, "cannot move objects into or out of temporary schemas"));
        }
        if old_ns == builtin::PG_TOAST_NAMESPACE || new_ns == builtin::PG_TOAST_NAMESPACE {
            return Err(PgError::new(SqlState::FEATURE_NOT_SUPPORTED, "cannot move objects into or out of TOAST schema"));
        }
        Ok(())
    }

    /// Delete one extensibility object (a `drop.rs` plan item). Returns whether it was recorded.
    pub(crate) fn drop_ext_object(&mut self, a: ObjectAddress) -> Result<bool> {
        match a.class {
            ObjClass::AmOp | ObjClass::AmProc => {
                let owner = self.snapshot()?.amop_owner(a.oid).map(|(f, _)| f.oid);
                let Some(fam) = owner else { return Ok(false) };
                if let Some(ExtObject::OpFamily(mut f)) = self.ext_object(fam) {
                    f.operators.retain(|o| o.oid != a.oid);
                    f.procs.retain(|p| p.oid != a.oid);
                    self.put_ext(ExtObject::OpFamily(f))?;
                }
                return Ok(true);
            }
            _ => {}
        }
        if !self.data().ext.contains_key(&a.oid) {
            return Err(PgError::new(
                SqlState::DEPENDENT_OBJECTS_STILL_EXIST,
                format!("cannot drop builtin {:?} {}", a.class, a.oid),
            ));
        }
        if a.class == ObjClass::Operator {
            // PG `OperatorUpd(..., isDelete=true)`: clear the partner links that point at it.
            let partners: Vec<Operator> = self
                .snapshot()?
                .user_operators()
                .filter(|o| o.oid != a.oid && (o.commutator == a.oid || o.negator == a.oid))
                .cloned()
                .collect();
            for mut p in partners {
                if p.commutator == a.oid {
                    p.commutator = 0;
                }
                if p.negator == a.oid {
                    p.negator = 0;
                }
                self.put_ext(ExtObject::Operator(p))?;
            }
        }
        self.remove(ObjKind::Ext, a.oid)?;
        self.clear_descriptions(a.class.classid(), a.oid)?;
        // Membership rows of extensions that listed it.
        let exts: Vec<Extension> =
            self.snapshot()?.user_extensions().filter(|e| e.members.contains(&a) || e.auto_dependents.contains(&a)).cloned().collect();
        for mut e in exts {
            e.members.retain(|m| *m != a);
            e.auto_dependents.retain(|m| *m != a);
            self.put_ext(ExtObject::Extension(e))?;
        }
        Ok(true)
    }
}
