//! Shared DDL helpers: ownership/privilege checks, name-clash checks, column list validation.
//! Lease CAT-08.
//!
//! PG sources: `utils/adt/acl.c` (`object_ownercheck`, `pg_class_aclcheck`, `aclcheck_error`),
//! `catalog/namespace.c` (`CheckSetNamespace`), `catalog/heap.c` (`CheckAttributeNamesTypes`,
//! `SystemAttributeByName`), `commands/tablecmds.c` (`DefineRelation` checks), `catalog/pg_type.c`
//! (`TypeCreate` name clash), `catalog/heap.c` (`heap_create_with_catalog` clash -> 42P07).
use crate::acl::{self, AclObject, RoleGraph};
use crate::builtin::{BOOTSTRAP_SUPERUSER, INFORMATION_SCHEMA_NAMESPACE, PG_CATALOG_NAMESPACE, PG_TOAST_NAMESPACE};
use crate::env::CatalogEnv;
use crate::errors;
use crate::model::*;
use crate::naming;
use crate::snapshot::{CatalogSnapshot, ClassRef, UserTypeRef};
use crate::txn::CatalogTxn;
use pgarbor_common::{Oid, Result, FIRST_NORMAL_OID};
use pgarbor_types::context::TypeEnv;
use pgarbor_types::names::display_name;
use pgarbor_types::registry;
use std::collections::HashSet;

/// PG system column names with their attnums (-1 ctid, -2 xmin, -3 cmin, -4 xmax, -5 cmax,
/// -6 tableoid). PG 18 has no `oid` system column.
// CONTRACT
pub const SYSTEM_COLUMNS: [(&str, i16); 6] =
    [("ctid", -1), ("xmin", -2), ("cmin", -3), ("xmax", -4), ("cmax", -5), ("tableoid", -6)];

/// True for `ctid`, `xmin`, `cmin`, `xmax`, `cmax`, `tableoid`.
pub fn is_system_column_name(name: &str) -> bool {
    SYSTEM_COLUMNS.iter().any(|(n, _)| *n == name)
}

/// attnum of a system column name.
pub fn system_column_attnum(name: &str) -> Option<i16> {
    SYSTEM_COLUMNS.iter().find(|(n, _)| *n == name).map(|(_, a)| *a)
}

/// Maximum columns per table (PG `MaxHeapAttributeNumber` 1600).
pub const MAX_COLUMNS: usize = 1600;

/// Validate a list of column definitions for a NEW relation or composite type
/// (PG `DefineRelation` / `CheckAttributeNamesTypes`), before any write:
/// - more than 1600 columns -> `errors::too_many_columns` (54011);
/// - duplicate names -> 42701 `errors::column_specified_more_than_once`;
/// - system column names (`xmin`, ...) -> 42701 `errors::column_conflicts_with_system_column`;
/// - pseudo-type columns (`record`, `anyelement`, `unknown`, `void`, `cstring` ...) ->
///   42P16 `column "x" has pseudo-type unknown` (oracle: `create table t(a unknown)` is rejected
///   with exactly that; `record` is also rejected; domains over pseudo-types too);
/// - a column of the table's own rowtype is not detectable here (binder);
/// - `serial` + `identity` + `generated` + `default` combinations: serial with default/identity/
///   generated -> 42601 `multiple default values specified for column "x" of table "t"` for
///   serial+default; identity+default -> 42601 `both default and identity specified for column "x"
///   of table "t"`; generated+default -> 42601 `both default and generation expression specified
///   for column "x" of table "t"`; generated+identity -> 42601 `both identity and generation
///   expression specified for column "x" of table "t"`.
/// `table_name` is used in those messages.
// CONTRACT
pub fn validate_column_specs(
    env: &CatalogEnv,
    table_name: &str,
    columns: &[crate::ddl::spec::ColumnSpec],
) -> Result<()> {
    // PG raises the option conflicts while the parser transforms each column, before DefineRelation
    // looks at the list as a whole.
    for c in columns {
        check_column_options(c, table_name)?;
    }
    if columns.len() > MAX_COLUMNS {
        return Err(errors::too_many_columns());
    }
    // PG MergeAttributes: duplicate names, in column order.
    let mut seen: HashSet<&str> = HashSet::with_capacity(columns.len());
    for c in columns {
        if !seen.insert(c.name.as_str()) {
            return Err(errors::column_specified_more_than_once(&c.name));
        }
    }
    // PG CheckAttributeNamesTypes: system names, then pseudo-types, per column in order.
    for c in columns {
        if is_system_column_name(&c.name) {
            return Err(errors::column_conflicts_with_system_column(&c.name));
        }
        check_not_pseudo(env, &c.name, c.type_id.oid)?;
    }
    Ok(())
}

/// The conflicting `DEFAULT` / `IDENTITY` / `GENERATED` combinations of one column (PG
/// `transformColumnDefinition`). `serial` implies a default, so it conflicts the same way.
fn check_column_options(c: &crate::ddl::spec::ColumnSpec, table: &str) -> Result<()> {
    let conflict = |what: &str| errors::column_option_conflict(what, &c.name, table);
    if c.serial.is_some() {
        if c.default.is_some() {
            return Err(conflict("multiple_defaults"));
        }
        if c.identity.is_some() {
            return Err(conflict("both_default_and_identity"));
        }
        if c.generated.is_some() {
            return Err(conflict("both_default_and_generation"));
        }
    } else {
        if c.identity.is_some() && c.default.is_some() {
            return Err(conflict("both_default_and_identity"));
        }
        if c.generated.is_some() && c.default.is_some() {
            return Err(conflict("both_default_and_generation"));
        }
    }
    if c.identity.is_some() && c.generated.is_some() {
        return Err(conflict("both_identity_and_generation"));
    }
    Ok(())
}

/// PG `CheckAttributeType` for pseudo-types: the column type itself, or (for arrays) its element
/// type, must not be a pseudo-type. The message names the offending type.
fn check_not_pseudo(env: &dyn TypeEnv, column: &str, type_oid: Oid) -> Result<()> {
    let mut oid = type_oid;
    loop {
        let info = registry::type_info(env, oid)?;
        if info.is_pseudo() {
            return Err(errors::column_has_pseudo_type(column, &display_name(env, oid)));
        }
        if info.is_array() && info.elem != 0 && info.elem != oid {
            oid = info.elem;
        } else {
            return Ok(());
        }
    }
}

/// Word used in `must be owner of <word> <name>` for a pg_class kind.
fn relkind_word(kind: RelKind) -> &'static str {
    match kind {
        RelKind::Table | RelKind::PartitionedTable => "table",
        RelKind::Index | RelKind::PartitionedIndex => "index",
        RelKind::Sequence => "sequence",
        RelKind::View => "view",
        RelKind::MatView => "materialized view",
        RelKind::CompositeType => "type",
        RelKind::ForeignTable => "foreign table",
        RelKind::Toast => "TOAST table",
    }
}

/// Word for a non-relation, non-type object class (ownership error text).
fn class_word(class: ObjClass) -> &'static str {
    match class {
        ObjClass::Relation | ObjClass::Rewrite => "table",
        ObjClass::Type => "type",
        ObjClass::Namespace => "schema",
        ObjClass::Constraint => "constraint",
        ObjClass::AttrDefault => "default value",
        ObjClass::Proc => "function",
        ObjClass::Role => "role",
        ObjClass::Database => "database",
        ObjClass::Collation => "collation",
        ObjClass::Operator => "operator",
        ObjClass::Cast => "cast",
        ObjClass::Language => "language",
        ObjClass::AccessMethod => "access method",
        ObjClass::Extension => "extension",
        ObjClass::Trigger => "trigger",
        ObjClass::Tablespace => "tablespace",
    }
}

/// Owner of a builtin object that is missing from the working data (compiled-in objects belong to
/// the bootstrap superuser); any other missing OID is XX000 `cache lookup failed`.
fn builtin_or_missing_owner(oid: Oid, what: &str) -> Result<Oid> {
    if oid < FIRST_NORMAL_OID {
        Ok(BOOTSTRAP_SUPERUSER)
    } else {
        Err(errors::cache_lookup_failed(what, oid))
    }
}

/// Owner of a pg_class OID (table, view, index, sequence, system relation).
fn relation_owner(snap: &CatalogSnapshot, oid: Oid) -> Result<Oid> {
    match snap.class(oid) {
        Some(ClassRef::Relation(o)) => {
            snap.relation(o).map(|r| r.owner).ok_or_else(|| errors::cache_lookup_failed("relation", oid))
        }
        Some(ClassRef::Index(o)) => {
            snap.index(o).map(|i| i.owner).ok_or_else(|| errors::cache_lookup_failed("index", oid))
        }
        Some(ClassRef::Sequence(o)) => {
            snap.sequence(o).map(|s| s.owner).ok_or_else(|| errors::cache_lookup_failed("sequence", oid))
        }
        Some(ClassRef::SystemIndex(_)) => Ok(BOOTSTRAP_SUPERUSER),
        None => builtin_or_missing_owner(oid, "relation"),
    }
}

/// Owner of a pg_type OID: enum/domain types, relation rowtypes and the implicit array types.
fn type_owner(snap: &CatalogSnapshot, oid: Oid) -> Result<Oid> {
    if let Some(t) = snap.type_obj(oid) {
        return Ok(t.owner);
    }
    match snap.user_type(oid) {
        Some(UserTypeRef::Type(t)) => {
            snap.type_obj(t).map(|t| t.owner).ok_or_else(|| errors::cache_lookup_failed("type", oid))
        }
        Some(UserTypeRef::TypeArray(elem)) => {
            snap.type_obj(elem).map(|t| t.owner).ok_or_else(|| errors::cache_lookup_failed("type", oid))
        }
        Some(UserTypeRef::RowType(rel) | UserTypeRef::RowTypeArray(rel)) => relation_owner(snap, rel),
        None => builtin_or_missing_owner(oid, "type"),
    }
}

/// Name of a relation for messages (bare name; the OID itself when unknown).
fn relation_name(snap: &CatalogSnapshot, oid: Oid) -> String {
    match snap.class_identity(oid) {
        Some((_, name, _)) => name.to_string(),
        None => oid.to_string(),
    }
}

/// Name or the OID when the object is unknown (builtin objects outside the working data).
fn name_or_oid(name: Option<&str>, oid: Oid) -> String {
    match name {
        Some(n) => n.to_string(),
        None => oid.to_string(),
    }
}

impl<'a> CatalogTxn<'a> {
    /// True if `role` is a superuser (role attribute `rolsuper`; the bootstrap superuser always is).
    // CONTRACT
    pub fn is_superuser(&mut self, role: Oid) -> Result<bool> {
        if role == BOOTSTRAP_SUPERUSER {
            return Ok(true);
        }
        let snap = self.snapshot()?;
        Ok(snap.role(role).is_some_and(|r| r.superuser))
    }

    /// `has_privs_of_role(current_user, owner)` or superuser: PG `object_ownercheck`. The caller of
    /// every ALTER/DROP/COMMENT/GRANT-as-owner calls this. Error: 42501 `must be owner of <kind> <name>`
    /// (kind word from `describe::type_description`-style words: `table`, `view`, `materialized view`,
    /// `index`, `sequence`, `type`, `schema`, `function`, `procedure`, `database`, `role`; for domains
    /// PG says `type`). Column addresses check the table. Objects with no owner concept (constraints,
    /// attrdefs) check their table.
    // CONTRACT
    pub fn require_owner(&mut self, addr: &ObjectAddress) -> Result<()> {
        let owner = self.owner_of(addr)?;
        let cur = self.current_user();
        if self.is_superuser(cur)? {
            return Ok(());
        }
        let snap = self.snapshot()?;
        let graph = RoleGraph::build(&snap, self.env.database())?;
        if graph.has_privs_of_role(cur, owner)? {
            return Ok(());
        }
        let (word, name) = self.object_word_and_name(&snap, addr)?;
        Err(errors::must_be_owner(word, &name))
    }

    /// CREATE privilege on schema `ns` for the current user (owner of the schema or superuser or an
    /// ACL grant of C), else 42501 `permission denied for schema <name>`. `pg_catalog` /
    /// `pg_toast`: 42501 `permission denied to create "pg_catalog.x"` is raised by
    /// `CatalogEnv::creation_namespace_for`, not here.
    // CONTRACT
    pub fn require_schema_create(&mut self, ns: Oid) -> Result<()> {
        let cur = self.current_user();
        if self.is_superuser(cur)? {
            return Ok(());
        }
        let snap = self.snapshot()?;
        let schema = snap.namespace(ns).ok_or_else(|| errors::cache_lookup_failed("schema", ns))?;
        let eff = acl::effective_acl(AclObject::Schema, schema.owner, schema.acl.as_ref())?;
        let graph = RoleGraph::build(&snap, self.env.database())?;
        if acl::has_privilege(&graph, cur, schema.owner, &eff, acl_mode::CREATE)? {
            Ok(())
        } else {
            Err(errors::permission_denied_for("schema", &schema.name))
        }
    }

    /// The pg_class name space check of PG `heap_create_with_catalog`: `(ns, name)` must be unused by
    /// any table/view/index/sequence/matview/composite type (system relations included):
    /// 42P07 `relation "name" already exists`; with `if_not_exists` emits the NOTICE
    /// `relation "name" already exists, skipping` and returns `Ok(false)`.
    /// Then the pg_type name check for the relation's ROWTYPE name (`name`): see
    /// [`CatalogTxn::resolve_type_name_clash`] (an enum/domain/other rowtype of that name ->
    /// 42710 `type "name" already exists` + the relation HINT; an auto-generated ARRAY type of that
    /// name is moved out of the way). Returns `Ok(true)` when the name is usable.
    // CONTRACT
    pub fn check_new_relation_name(&mut self, ns: Oid, name: &str, if_not_exists: bool) -> Result<bool> {
        let snap = self.snapshot()?;
        if snap.class_in_namespace(ns, name).is_some() {
            if if_not_exists {
                self.notice(errors::relation_already_exists_skipping(name));
                return Ok(false);
            }
            return Err(errors::relation_already_exists(name));
        }
        // PG heap.c: a clash with a type while creating a relation always carries the relation HINT.
        self.clash_with_type(ns, name, true)?;
        Ok(true)
    }

    /// The pg_type name space check for CREATE TYPE/DOMAIN (and for rowtypes): if `(ns, name)` is
    /// taken by an explicit type (enum/domain), a relation rowtype, or a builtin type visible in
    /// the namespace -> 42710 `type "name" already exists` (`errors::type_already_exists(name,
    /// relation_hint)` with the HINT when the clash is a relation rowtype). If it is taken by an
    /// AUTO-GENERATED ARRAY type (`_foo` for type `foo`) PG renames that array out of the way
    /// (`moveArrayTypeName`, oracle-verified): set the owner type/relation's `array_type_name` to
    /// `Some(naming::choose_array_type_name(snapshot, ns, name /* the NEW type's name */, &[]))` computed
    /// while the clashing array name is still taken (PG `moveArrayTypeName` calls
    /// `makeArrayTypeName(typeName)` with the NEW name; oracle: new type `_e` moves e's array to `__e`,
    /// then `_e`'s own array becomes `__e_1`), and return `Ok(())`. Builtin types in `pg_catalog` do not
    /// clash with user schemas.
    // CONTRACT
    pub fn resolve_type_name_clash(&mut self, ns: Oid, name: &str) -> Result<()> {
        self.clash_with_type(ns, name, false)
    }

    /// Shared body of [`CatalogTxn::resolve_type_name_clash`]. `relation` = the name is a relation's
    /// rowtype, which adds the relation HINT to a plain type clash (PG heap.c).
    fn clash_with_type(&mut self, ns: Oid, name: &str, relation: bool) -> Result<()> {
        let snap = self.snapshot()?;
        let Some(oid) = snap.user_type_in_namespace(ns, name) else {
            return Ok(());
        };
        match snap.user_type(oid) {
            None => Ok(()),
            Some(UserTypeRef::Type(_)) => Err(errors::type_already_exists(name, relation)),
            Some(UserTypeRef::RowType(_)) => Err(errors::type_already_exists(name, true)),
            Some(UserTypeRef::TypeArray(elem)) => {
                // moveArrayTypeName: the new name is picked while the clashing array name is taken.
                let new_name = naming::choose_array_type_name(&snap, ns, name, &[])?;
                self.update_type(elem, move |t| {
                    t.array_type_name = Some(new_name);
                    Ok(())
                })
            }
            Some(UserTypeRef::RowTypeArray(rel)) => {
                let new_name = naming::choose_array_type_name(&snap, ns, name, &[])?;
                self.update_relation(rel, move |r| {
                    r.array_type_name = Some(new_name);
                    Ok(())
                })
            }
        }
    }

    /// Reject creating/moving objects into `pg_catalog`, `pg_toast`, or temp schemas of other
    /// sessions: 42501 `permission denied to create "<schema>.<name>"` + DETAIL
    /// `System catalog modifications are currently disallowed.` (`errors::system_catalog_create_denied`).
    /// `information_schema` is allowed for superusers only.
    // CONTRACT
    pub fn check_creatable_namespace(&mut self, ns: Oid, name: &str) -> Result<()> {
        match ns {
            PG_CATALOG_NAMESPACE => Err(errors::system_catalog_create_denied("pg_catalog", name)),
            PG_TOAST_NAMESPACE => Err(errors::system_catalog_create_denied("pg_toast", name)),
            INFORMATION_SCHEMA_NAMESPACE => {
                let cur = self.current_user();
                if self.is_superuser(cur)? {
                    Ok(())
                } else {
                    Err(errors::permission_denied_for("schema", "information_schema"))
                }
            }
            _ => Ok(()),
        }
    }

    /// Owner OID of any object address (relations/indexes/sequences/types/functions/schemas/
    /// databases/roles(self)/constraints and attrdefs -> table owner). Missing -> XX000
    /// `cache lookup failed`.
    // CONTRACT
    pub fn owner_of(&mut self, addr: &ObjectAddress) -> Result<Oid> {
        let snap = self.snapshot()?;
        let oid = addr.oid;
        match addr.class {
            ObjClass::Relation | ObjClass::Rewrite => relation_owner(&snap, oid),
            ObjClass::Type => type_owner(&snap, oid),
            ObjClass::Namespace => match snap.namespace(oid) {
                Some(n) => Ok(n.owner),
                None => builtin_or_missing_owner(oid, "schema"),
            },
            ObjClass::Constraint => match snap.constraint(oid) {
                Some(c) if c.table != 0 => relation_owner(&snap, c.table),
                Some(c) => type_owner(&snap, c.domain),
                None => Err(errors::cache_lookup_failed("constraint", oid)),
            },
            ObjClass::AttrDefault => match snap.indexes().attrdef_owner.get(&oid) {
                Some((rel, _)) => relation_owner(&snap, *rel),
                None => Err(errors::cache_lookup_failed("attrdef", oid)),
            },
            ObjClass::Proc => match snap.function(oid) {
                Some(f) => Ok(f.owner),
                None => builtin_or_missing_owner(oid, "function"),
            },
            ObjClass::Role => match snap.role(oid) {
                // Roles have no owner: the role itself stands in (PG checks CREATEROLE instead).
                Some(_) => Ok(oid),
                None => Err(errors::cache_lookup_failed("role", oid)),
            },
            ObjClass::Database => match snap.database(oid) {
                Some(d) => Ok(d.owner),
                None => builtin_or_missing_owner(oid, "database"),
            },
            ObjClass::Trigger => Err(errors::cache_lookup_failed("trigger", oid)),
            other => builtin_or_missing_owner(oid, class_word(other)),
        }
    }

    /// `(kind word, bare name)` of an object address for ownership errors.
    fn object_word_and_name(&mut self, snap: &CatalogSnapshot, addr: &ObjectAddress) -> Result<(&'static str, String)> {
        let oid = addr.oid;
        let word_and_name = match addr.class {
            ObjClass::Relation => match snap.class_identity(oid) {
                Some((_, name, kind)) => (relkind_word(kind), name.to_string()),
                None => return Err(errors::cache_lookup_failed("relation", oid)),
            },
            ObjClass::Rewrite => ("view", relation_name(snap, oid)),
            ObjClass::Type => {
                let env = self.env()?;
                let shown = match snap.user_type(oid) {
                    Some(UserTypeRef::TypeArray(elem)) => elem,
                    _ => oid,
                };
                ("type", display_name(&env as &dyn TypeEnv, shown))
            }
            ObjClass::Namespace => ("schema", name_or_oid(snap.namespace(oid).map(|n| n.name.as_str()), oid)),
            ObjClass::Constraint => match snap.constraint(oid) {
                Some(c) if c.table != 0 => ("table", relation_name(snap, c.table)),
                Some(c) => {
                    let env = self.env()?;
                    ("type", display_name(&env as &dyn TypeEnv, c.domain))
                }
                None => return Err(errors::cache_lookup_failed("constraint", oid)),
            },
            ObjClass::AttrDefault => match snap.indexes().attrdef_owner.get(&oid) {
                Some((rel, _)) => ("table", relation_name(snap, *rel)),
                None => return Err(errors::cache_lookup_failed("attrdef", oid)),
            },
            ObjClass::Proc => match snap.function(oid) {
                Some(f) => {
                    let word = match f.kind {
                        FuncKind::Procedure => "procedure",
                        FuncKind::Aggregate => "aggregate",
                        FuncKind::Function | FuncKind::Window => "function",
                    };
                    (word, f.name.clone())
                }
                None => ("function", oid.to_string()),
            },
            ObjClass::Role => ("role", name_or_oid(snap.role(oid).map(|r| r.name.as_str()), oid)),
            ObjClass::Database => ("database", name_or_oid(snap.database(oid).map(|d| d.name.as_str()), oid)),
            other => (class_word(other), oid.to_string()),
        };
        Ok(word_and_name)
    }

    /// Rewrite the self placeholder (`SELF_REL`, 0) inside a `StoredExpr`'s dependency list to
    /// `rel` (pure; implemented fully because several leases share it).
    pub fn bind_self_deps(expr: &StoredExpr, rel: Oid) -> StoredExpr {
        let mut e = expr.clone();
        for d in e.depends_on.iter_mut() {
            if d.class == ObjClass::Relation && d.oid == crate::ddl::spec::SELF_REL {
                d.oid = rel;
            }
        }
        e
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ddl::spec::{ColumnSpec, IdentitySpec, SequenceOptions, SerialKind};
    use crate::testing;
    use pgarbor_types::TypeId;

    #[test]
    fn column_list_validation() {
        let (_kv, env) = testing::bootstrapped().expect("bootstrap");

        let dup = vec![ColumnSpec::new("a", TypeId::INT4), ColumnSpec::new("a", TypeId::INT4)];
        let e = validate_column_specs(&env, "t", &dup).err().expect("duplicate");
        assert_eq!(e.sqlstate.code(), "42701");
        assert_eq!(e.message, "column \"a\" specified more than once");

        let sys = vec![ColumnSpec::new("xmin", TypeId::INT4)];
        let e = validate_column_specs(&env, "t", &sys).err().expect("system name");
        assert_eq!(e.sqlstate.code(), "42701");
        assert_eq!(e.message, "column name \"xmin\" conflicts with a system column name");

        let pseudo = vec![ColumnSpec::new("a", TypeId::of(705))];
        let e = validate_column_specs(&env, "t", &pseudo).err().expect("pseudo-type");
        assert_eq!(e.sqlstate.code(), "42P16");
        assert_eq!(e.message, "column \"a\" has pseudo-type unknown");

        let mut c = ColumnSpec::new("id", TypeId::INT4);
        c.serial = Some(SerialKind::Int);
        c.identity = Some(IdentitySpec { kind: IdentityKind::Always, options: SequenceOptions::default(), sequence_name: None });
        let e = validate_column_specs(&env, "t", &[c]).err().expect("serial + identity");
        assert_eq!(e.sqlstate.code(), "42601");
        assert_eq!(e.message, "both default and identity specified for column \"id\" of table \"t\"");

        let ok = vec![ColumnSpec::new("a", TypeId::INT4), ColumnSpec::new("b", TypeId::TEXT)];
        assert!(validate_column_specs(&env, "t", &ok).is_ok());
    }

    #[test]
    fn namespace_and_name_checks_through_txn() {
        let (mut kv, env) = testing::bootstrapped().expect("bootstrap");

        let e = testing::run(&mut kv, &env, |t| t.check_creatable_namespace(PG_CATALOG_NAMESPACE, "x"))
            .err()
            .expect("pg_catalog is closed");
        assert_eq!(e.sqlstate.code(), "42501");
        assert_eq!(e.message, "permission denied to create \"pg_catalog.x\"");
        assert_eq!(e.detail.as_deref(), Some("System catalog modifications are currently disallowed."));

        let e = testing::run(&mut kv, &env, |t| t.check_new_relation_name(PG_CATALOG_NAMESPACE, "pg_class", false).map(|_| ()))
            .err()
            .expect("relation exists");
        assert_eq!(e.sqlstate.code(), "42P07");
        assert_eq!(e.message, "relation \"pg_class\" already exists");

        let (res, _) = testing::run(&mut kv, &env, |t| {
            let usable = t.check_new_relation_name(PG_CATALOG_NAMESPACE, "pg_class", true)?;
            assert!(!usable);
            Ok(())
        })
        .expect("if not exists");
        assert_eq!(res.outcome.notices.len(), 1);
        assert_eq!(res.outcome.notices[0].message, "relation \"pg_class\" already exists, skipping");
    }

    #[test]
    fn array_type_moves_out_of_the_way() {
        let (mut kv, env) = testing::bootstrapped().expect("bootstrap");
        let (_, env2) = testing::run(&mut kv, &env, |t| {
            t.put(CatalogObject::Type(TypeObj {
                oid: 16400,
                name: "e".to_string(),
                namespace: 2200,
                owner: 10,
                array_type: 16401,
                array_type_name: None,
                def: TypeDef::Enum { labels: Vec::new() },
                acl: None,
            }))
        })
        .expect("create enum");
        testing::run(&mut kv, &env2, |t| {
            // A new type `_e` clashes with e's array `_e`: that array becomes `__e`.
            t.resolve_type_name_clash(2200, "_e")?;
            let snap = t.snapshot()?;
            assert_eq!(snap.user_type_in_namespace(2200, "__e"), Some(16401));
            assert_eq!(snap.user_type_in_namespace(2200, "_e"), None);
            Ok(())
        })
        .expect("move array");
    }

    #[test]
    fn ownership_checks() {
        let (mut kv, env) = testing::bootstrapped().expect("bootstrap");
        // The bootstrap superuser owns builtin objects and passes.
        testing::run(&mut kv, &env, |t| t.require_owner(&ObjectAddress::type_(23))).expect("superuser passes");

        // A non-superuser that owns nothing gets 42501.
        let stranger =
            CatalogEnv::with_resolved(env.snapshot().clone(), crate::ResolvedSearchPath::builtin_default(), 16999);
        let e = testing::run(&mut kv, &stranger, |t| t.require_owner(&ObjectAddress::type_(23)))
            .err()
            .expect("not owner");
        assert_eq!(e.sqlstate.code(), "42501");
        assert_eq!(e.message, "must be owner of type integer");
    }
}
