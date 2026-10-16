//! Name resolution over a [`CatalogEnv`] (PG `catalog/namespace.c`: `RangeVarGetRelid`,
//! `LookupExplicitNamespace`, `TypenameGetTypid`, `FuncnameGetCandidates`, `RelationIsVisible`,
//! `TypeIsVisible`, `FunctionIsVisible`; `utils/adt/regproc.c` `stringToQualifiedNameList`).
//! Lease CAT-04.
//!
//! All names are EXACT (the binder has already case-folded unquoted identifiers and stripped
//! quotes). `schema = Some(s)` means the name was qualified (`s.name`).
//!
//! Rules:
//! - **Relations** (`lookup_class`): qualified → only that schema; a missing schema yields `None`
//!   (callers then raise 42P01 `relation "nos.x" does not exist`, which is what PG says for
//!   `SELECT * FROM nos.x`). Unqualified → first match along `path().effective_for_relations`.
//!   Builtin system relations live in pg_catalog (11) / information_schema (13293) and are found
//!   through `snapshot.class_in_namespace` like user relations.
//! - **Creation targets** (`creation_namespace_for`): qualified → schema must exist (3F000
//!   `schema "nos" does not exist`); `pg_catalog`, `pg_toast` → 42501 `permission denied to
//!   create "pg_catalog.x"` + DETAIL (`errors::system_catalog_create_denied`); `information_schema`
//!   is allowed (PG allows superusers). Unqualified → `env.creation_namespace()`, with the same
//!   pg_catalog check.
//! - **Types** (`lookup_type`): qualified → user types in that schema (`snapshot.user_type_in_namespace`)
//!   or, for `pg_catalog` / `information_schema`, builtin types via
//!   `pgarbor_types::registry::builtin_env().type_by_name(Some(schema), name)`. Unqualified → walk
//!   `path().effective`; at the pg_catalog position consult the builtin registry
//!   (`registry::type_by_name`), at information_schema the builtin info-schema domains, elsewhere
//!   user types. Note: SQL aliases (`integer`, `varchar`) are NOT handled here (that is
//!   `pgarbor_types::names`), only catalog typnames (`int4`, `_int4`, `mood`, `_mood`).
//! - **Functions** (`function_candidates`): builtin procs from
//!   `pgarbor_types::catalog_data::procs_named(name)` whose `namespace` is in the searched set
//!   (pg_catalog procs, plus the few information_schema procs such as `_pg_expandarray`), plus user
//!   `Function`s (`snapshot.functions_in_namespace`). Each candidate's `path_pos` = position of its
//!   namespace in `path().effective` (qualified call → only that namespace, path_pos 0). Builtin
//!   candidates: `FuncCandidate{oid, name, arg_types, arg_names (proargnames restricted to IN args,
//!   "" when absent), variadic, ndefaults, ret, retset, kind}`; user candidates from
//!   `Function::in_arg_types()` etc. Returns an empty Vec when nothing matches (the binder raises
//!   42883). Qualified with a missing schema → 3F000 `schema "x" does not exist`.
//! - **Roles** (`role_oid`): 42704 `role "x" does not exist`. Special names `public` → 0 only where
//!   the caller asks (`GRANT ... TO PUBLIC` is parsed by the binder, not here).
//! - **Collations** (`collation_oid`): builtin collations in pg_catalog (`default` 100, `C` 950,
//!   `POSIX` 951, `pg_c_utf8`, `ucs_basic`, `unicode` ...) by exact name (encoding -1 or 6);
//!   42704 `collation "x" for encoding "UTF8" does not exist`.
//! - **Visibility** (PG `*IsVisible`): an object is visible iff its namespace is in the relevant
//!   search order AND no object of the same name (same identity for functions: name + IN arg types)
//!   appears earlier in that order.
//! - **Display names** (`qualified_class_name`, `qualified_type_name`): `quote_identifier(name)`,
//!   prefixed with `quote_identifier(schema).` when not visible (PG `generate_relation_name` /
//!   regclassout style). Quoting via `pgarbor_types::text::format::quote_identifier`.
//!
//! Errors are built with `crate::errors`.
use crate::builtin;
use crate::env::CatalogEnv;
use crate::model::{ArgMode, Relation};
use crate::snapshot::{ClassRef, UserTypeRef};
use pgarbor_common::{Oid, Result};
use pgarbor_types::catalog_data::{self, BuiltinProc};
use pgarbor_types::registry;
use pgarbor_types::resolve::func::FuncCandidate;

/// `any` pseudo-type (VARIADIC "any" keeps 2276 as its element type).
const ANY_OID: Oid = 2276;

impl CatalogEnv {
    /// Schema OID by name, or None.
    // CONTRACT
    pub fn lookup_namespace(&self, name: &str) -> Option<Oid> {
        self.snapshot().namespace_by_name(name).map(|n| n.oid)
    }

    /// Schema OID by name, or 3F000 `schema "x" does not exist`.
    // CONTRACT
    pub fn namespace_oid(&self, name: &str) -> Result<Oid> {
        self.lookup_namespace(name).ok_or_else(|| crate::errors::schema_does_not_exist(name))
    }

    /// pg_class object by possibly-qualified name (see module docs), `Ok(None)` when not found.
    // CONTRACT
    pub fn lookup_class(&self, schema: Option<&str>, name: &str) -> Result<Option<ClassRef>> {
        match schema {
            Some(s) => match self.explicit_namespace(s) {
                Some(ns) => Ok(self.class_in_ns(ns, name)),
                None => Ok(None),
            },
            None => {
                for ns in &self.path().effective_for_relations {
                    if let Some(c) = self.class_in_ns(*ns, name) {
                        return Ok(Some(c));
                    }
                }
                Ok(None)
            }
        }
    }

    /// Like `lookup_class` but 42P01 `relation "x" does not exist` when not found.
    // CONTRACT
    pub fn resolve_class(&self, schema: Option<&str>, name: &str) -> Result<ClassRef> {
        self.lookup_class(schema, name)?.ok_or_else(|| crate::errors::relation_does_not_exist(schema, name))
    }

    /// Resolve a FROM-clause / DML-target name to a `Relation` (table, view, matview, composite
    /// type, partitioned table, system table/view). Missing → 42P01 `relation "x" does not exist`.
    /// An index → 42809 `cannot open relation "t_pkey"` + DETAIL
    /// `This operation is not supported for indexes.` (`errors::operation_not_supported_for`).
    /// A sequence is NOT a `Relation`: callers that accept sequences (PG allows
    /// `SELECT * FROM seq` → columns last_value, log_cnt, is_called) must use `resolve_class`
    /// first; here a sequence → 42809 `cannot open relation "s"` + DETAIL
    /// `This operation is not supported for sequences.`
    // CONTRACT
    pub fn resolve_relation(&self, schema: Option<&str>, name: &str) -> Result<&Relation> {
        match self.resolve_class(schema, name)? {
            ClassRef::Relation(oid) => {
                self.snapshot().relation(oid).ok_or_else(|| crate::errors::cache_lookup_failed("relation", oid))
            }
            ClassRef::Index(_) | ClassRef::SystemIndex(_) => Err(crate::errors::operation_not_supported_for(
                &format!("cannot open relation \"{name}\""),
                "indexes",
            )),
            ClassRef::Sequence(_) => Err(crate::errors::operation_not_supported_for(
                &format!("cannot open relation \"{name}\""),
                "sequences",
            )),
        }
    }

    /// Target schema for CREATE of `name` (see module docs).
    // CONTRACT
    pub fn creation_namespace_for(&self, schema: Option<&str>, name: &str) -> Result<Oid> {
        let ns = match schema {
            Some(s) => self.explicit_namespace(s).ok_or_else(|| crate::errors::schema_does_not_exist(s))?,
            None => self.creation_namespace()?,
        };
        if ns == builtin::PG_CATALOG_NAMESPACE || ns == builtin::PG_TOAST_NAMESPACE {
            let ns_name = self.snapshot().namespace(ns).map(|n| n.name.as_str()).unwrap_or("pg_catalog");
            return Err(crate::errors::system_catalog_create_denied(ns_name, name));
        }
        Ok(ns)
    }

    /// CREATE privilege on schema `ns` for the current user (superuser, owner or ACL grant), else 42501
    /// `permission denied for schema <name>`. Lets the binder raise the error with a source position
    /// (PG `RangeVarGetAndCheckCreationNamespace`); the catalog transaction re-checks at execution.
    pub fn check_schema_create(&self, ns: Oid) -> Result<()> {
        let cur = self.current_user();
        let snap = self.snapshot();
        if cur == builtin::BOOTSTRAP_SUPERUSER || snap.role(cur).is_some_and(|r| r.superuser) {
            return Ok(());
        }
        let Some(schema) = snap.namespace(ns) else {
            return Ok(());
        };
        let eff = crate::acl::effective_acl(crate::acl::AclObject::Schema, schema.owner, schema.acl.as_ref())?;
        let graph = crate::acl::RoleGraph::build(snap, self.database())?;
        if crate::acl::has_privilege(&graph, cur, schema.owner, &eff, crate::model::acl_mode::CREATE)? {
            Ok(())
        } else {
            Err(crate::errors::permission_denied_for("schema", &schema.name))
        }
    }

    /// Type OID by catalog typname (builtin or user), honoring the search path (module docs).
    // CONTRACT
    pub fn lookup_type(&self, schema: Option<&str>, typname: &str) -> Result<Option<Oid>> {
        match schema {
            Some(s) => {
                let ns = self.explicit_namespace(s).ok_or_else(|| crate::errors::schema_does_not_exist(s))?;
                Ok(self.type_in_ns(ns, typname))
            }
            None => {
                for ns in &self.path().effective {
                    if let Some(t) = self.type_in_ns(*ns, typname) {
                        return Ok(Some(t));
                    }
                }
                Ok(None)
            }
        }
    }

    /// Candidate functions/procedures/aggregates named `name` (module docs).
    // CONTRACT
    pub fn function_candidates(&self, schema: Option<&str>, name: &str) -> Result<Vec<FuncCandidate>> {
        // (namespace, path_pos) pairs to search.
        let searched: Vec<(Oid, u32)> = match schema {
            Some(s) => {
                let ns = self.explicit_namespace(s).ok_or_else(|| crate::errors::schema_does_not_exist(s))?;
                vec![(ns, 0)]
            }
            None => self.path().effective.iter().enumerate().map(|(i, ns)| (*ns, i as u32)).collect(),
        };
        let pos_of = |ns: Oid| searched.iter().find(|(n, _)| *n == ns).map(|(_, p)| *p);

        let mut raw: Vec<FuncCandidate> = Vec::new();
        for p in catalog_data::procs_named(name) {
            if let Some(path_pos) = pos_of(p.namespace) {
                raw.push(FuncCandidate {
                    oid: p.oid,
                    name: p.name.to_string(),
                    arg_types: p.arg_types.to_vec(),
                    arg_names: builtin_in_arg_names(p),
                    variadic: p.variadic,
                    ndefaults: p.ndefaults,
                    ret: p.ret,
                    retset: p.retset,
                    kind: p.kind,
                    path_pos,
                });
            }
        }
        for (ns, path_pos) in &searched {
            for f in self.snapshot().functions_in_namespace(*ns, name) {
                let variadic = f
                    .args
                    .iter()
                    .find(|a| a.mode == ArgMode::Variadic)
                    .map(|a| {
                        if a.type_oid == ANY_OID {
                            ANY_OID
                        } else {
                            registry::element_type_of(self, a.type_oid).unwrap_or(0)
                        }
                    })
                    .unwrap_or(0);
                raw.push(FuncCandidate {
                    oid: f.oid,
                    name: f.name.clone(),
                    arg_types: f.in_arg_types(),
                    arg_names: f
                        .args
                        .iter()
                        .filter(|a| matches!(a.mode, ArgMode::In | ArgMode::InOut | ArgMode::Variadic))
                        .map(|a| a.name.clone())
                        .collect(),
                    variadic,
                    ndefaults: f.n_defaults(),
                    ret: f.ret,
                    retset: f.retset,
                    kind: f.kind.to_types(),
                    path_pos: *path_pos,
                });
            }
        }

        // PG FuncnameGetCandidates: of two functions with identical input argument types the one
        // earlier in the search path hides the later one.
        let mut out: Vec<FuncCandidate> = Vec::with_capacity(raw.len());
        for c in raw {
            match out.iter_mut().find(|e| e.arg_types == c.arg_types && e.path_pos != c.path_pos) {
                Some(existing) => {
                    if c.path_pos < existing.path_pos {
                        *existing = c;
                    }
                }
                None => out.push(c),
            }
        }
        out.sort_by_key(|c| c.path_pos);
        Ok(out)
    }

    /// Role OID by name, or 42704 `role "x" does not exist`.
    // CONTRACT
    pub fn role_oid(&self, name: &str) -> Result<Oid> {
        self.snapshot().role_by_name(name).map(|r| r.oid).ok_or_else(|| crate::errors::role_does_not_exist(name))
    }

    /// Collation OID by possibly-qualified name (module docs).
    // CONTRACT
    pub fn collation_oid(&self, schema: Option<&str>, name: &str) -> Result<Oid> {
        // Builtin pg_catalog collations and user collations (amendment pgarbor-catalog-09).
        let find = |ns: Oid| -> Option<Oid> { self.collation_in_ns(ns, name) };
        match schema {
            Some(s) => {
                let ns = self.explicit_namespace(s).ok_or_else(|| crate::errors::schema_does_not_exist(s))?;
                find(ns).ok_or_else(|| crate::errors::collation_does_not_exist(&format!("{s}.{name}")))
            }
            None => {
                for ns in &self.path().effective {
                    if let Some(o) = find(*ns) {
                        return Ok(o);
                    }
                }
                Err(crate::errors::collation_does_not_exist(name))
            }
        }
    }

    /// PG `RelationIsVisible` for any pg_class OID (false for unknown OIDs).
    // CONTRACT
    pub fn relation_is_visible(&self, oid: Oid) -> Result<bool> {
        let name = match self.snapshot().class_identity(oid) {
            Some((_, name, _)) => name,
            None => return Ok(false),
        };
        // The first same-named object along the path wins; visible iff that is `oid`.
        for ns in &self.path().effective_for_relations {
            if let Some(c) = self.class_in_ns(*ns, name) {
                return Ok(c.oid() == oid);
            }
        }
        Ok(false)
    }

    /// PG `TypeIsVisible` (builtin or user type).
    // CONTRACT
    pub fn type_is_visible(&self, oid: Oid) -> Result<bool> {
        let name = match self.type_identity(oid) {
            Some((_, name)) => name,
            None => return Ok(false),
        };
        for ns in &self.path().effective {
            if let Some(t) = self.type_in_ns(*ns, &name) {
                return Ok(t == oid);
            }
        }
        Ok(false)
    }

    /// PG `FunctionIsVisible` (builtin or user function).
    // CONTRACT
    pub fn function_is_visible(&self, oid: Oid) -> Result<bool> {
        let (ns, name, args): (Oid, String, Vec<Oid>) = if let Some(p) = catalog_data::builtin_proc(oid) {
            (p.namespace, p.name.to_string(), p.arg_types.to_vec())
        } else if let Some(f) = self.snapshot().function(oid) {
            (f.namespace, f.name.clone(), f.in_arg_types())
        } else {
            return Ok(false);
        };
        let pos = match self.path().position(ns) {
            Some(p) => p as usize,
            None => return Ok(false),
        };
        for earlier in &self.path().effective[..pos] {
            if catalog_data::procs_named(&name).any(|p| p.namespace == *earlier && p.arg_types == args.as_slice()) {
                return Ok(false);
            }
            if self.snapshot().functions_in_namespace(*earlier, &name).iter().any(|f| f.in_arg_types() == args) {
                return Ok(false);
            }
        }
        Ok(true)
    }

    /// Display name of a pg_class object: `t`, `"Mixed"`, `other_schema.t` (module docs).
    /// Unknown OID → XX000 `cache lookup failed for relation <oid>`.
    // CONTRACT
    pub fn qualified_class_name(&self, oid: Oid) -> Result<String> {
        let (ns, name, _) =
            self.snapshot().class_identity(oid).ok_or_else(|| crate::errors::cache_lookup_failed("relation", oid))?;
        let quoted = pgarbor_types::text::format::quote_identifier(name)?;
        if self.relation_is_visible(oid)? {
            return Ok(quoted);
        }
        let ns_name = self
            .snapshot()
            .namespace(ns)
            .map(|n| n.name.as_str())
            .ok_or_else(|| crate::errors::cache_lookup_failed("namespace", ns))?;
        Ok(format!("{}.{}", pgarbor_types::text::format::quote_identifier(ns_name)?, quoted))
    }

    /// Schema-qualified-if-invisible name of a namespace object for messages and descriptions:
    /// returns `Some(schema_name)` when the schema is NOT in the search path (PG
    /// `getObjectDescription` qualifies with `quote_identifier(schema)` only then), else None.
    // CONTRACT
    pub fn schema_qualifier(&self, namespace: Oid, name: &str, for_relation: bool) -> Result<Option<String>> {
        let path: &[Oid] = if for_relation { &self.path().effective_for_relations } else { &self.path().effective };
        let in_path = path.contains(&namespace);
        // A relation shadowed by an earlier same-named relation must be qualified too.
        let hidden = in_path
            && for_relation
            && path.iter().take_while(|n| **n != namespace).any(|n| self.class_in_ns(*n, name).is_some());
        if in_path && !hidden {
            return Ok(None);
        }
        let ns_name = self
            .snapshot()
            .namespace(namespace)
            .map(|n| n.name.clone())
            .ok_or_else(|| crate::errors::cache_lookup_failed("namespace", namespace))?;
        Ok(Some(ns_name))
    }

    // ------------------------------------------------------------------ private helpers

    /// Schema by name, with `pg_temp` meaning the session's temp schema when one exists.
    pub(crate) fn explicit_namespace(&self, name: &str) -> Option<Oid> {
        if name == "pg_temp" {
            if let Some(t) = self.path().temp_namespace {
                return Some(t);
            }
        }
        self.lookup_namespace(name)
    }

    /// pg_class lookup in one schema, falling back to the compiled-in system relations.
    fn class_in_ns(&self, ns: Oid, name: &str) -> Option<ClassRef> {
        if let Some(c) = self.snapshot().class_in_namespace(ns, name) {
            return Some(c);
        }
        if ns == builtin::PG_CATALOG_NAMESPACE || ns == builtin::INFORMATION_SCHEMA_NAMESPACE {
            return builtin::system_rel_by_name(ns, name).map(|r| {
                if r.kind == b'i' {
                    ClassRef::SystemIndex(r.oid)
                } else {
                    ClassRef::Relation(r.oid)
                }
            });
        }
        None
    }

    /// pg_type lookup in one schema (builtin types for pg_catalog / information_schema).
    fn type_in_ns(&self, ns: Oid, typname: &str) -> Option<Oid> {
        if ns == builtin::PG_CATALOG_NAMESPACE {
            return registry::type_by_name(typname).map(|t| t.oid);
        }
        if ns == builtin::INFORMATION_SCHEMA_NAMESPACE {
            return catalog_data::TYPES.iter().find(|t| t.namespace == ns && t.name == typname).map(|t| t.oid);
        }
        self.snapshot().user_type_in_namespace(ns, typname)
    }

    /// (namespace, typname) of any builtin or user type OID.
    fn type_identity(&self, oid: Oid) -> Option<(Oid, String)> {
        if let Some(t) = registry::lookup_type(oid) {
            return Some((t.namespace, t.name.to_string()));
        }
        let snap = self.snapshot();
        match snap.user_type(oid)? {
            UserTypeRef::Type(o) => snap.type_obj(o).map(|t| (t.namespace, t.name.clone())),
            UserTypeRef::TypeArray(o) => snap.type_obj(o).map(|t| {
                let n = t.array_type_name.clone().unwrap_or_else(|| crate::naming::array_type_name(&t.name));
                (t.namespace, n)
            }),
            UserTypeRef::RowType(r) => snap.relation(r).map(|r| (r.namespace, r.name.clone())),
            UserTypeRef::RowTypeArray(r) => snap.relation(r).map(|r| {
                let n = r.array_type_name.clone().unwrap_or_else(|| crate::naming::array_type_name(&r.name));
                (r.namespace, n)
            }),
        }
    }
}

/// `proargnames` restricted to the IN-style arguments (`proargtypes`), "" when unnamed.
fn builtin_in_arg_names(p: &BuiltinProc) -> Vec<String> {
    let n = p.arg_types.len();
    if p.arg_names.is_empty() {
        return vec![String::new(); n];
    }
    let mut v: Vec<String> = Vec::with_capacity(n);
    if p.arg_modes.is_empty() {
        for i in 0..n {
            v.push(p.arg_names.get(i).map(|s| s.to_string()).unwrap_or_default());
        }
    } else {
        for (i, m) in p.arg_modes.iter().enumerate() {
            if matches!(m, b'i' | b'b' | b'v') {
                v.push(p.arg_names.get(i).map(|s| s.to_string()).unwrap_or_default());
            }
        }
        v.resize(n, String::new());
    }
    v
}

#[cfg(test)]
mod tests {
    use crate::ddl::spec::{ColumnSpec, ConstraintSpec, ConstraintSpecKind, CreateEnumSpec, CreateTableSpec};
    use crate::model::{Persistence, RelKind};
    use crate::snapshot::ClassRef;
    use crate::{builtin, testing};
    use pgarbor_common::SqlState;
    use pgarbor_types::TypeId;

    fn table_spec(ns: u32, name: &str, pk: bool) -> CreateTableSpec {
        CreateTableSpec {
            namespace: ns,
            name: name.to_string(),
            owner: builtin::BOOTSTRAP_SUPERUSER,
            kind: RelKind::Table,
            persistence: Persistence::Permanent,
            columns: vec![ColumnSpec::new("a", TypeId::INT4)],
            constraints: if pk {
                vec![ConstraintSpec::new(ConstraintSpecKind::PrimaryKey {
                    columns: vec!["a".to_string()],
                    include: vec![],
                })]
            } else {
                vec![]
            },
            options: vec![],
            if_not_exists: false,
            inherits: vec![],
            extra: Default::default(),
        }
    }

    #[test]
    fn system_relation_via_implicit_pg_catalog() {
        let (_kv, env) = testing::bootstrapped().unwrap();
        let c = env.lookup_class(None, "pg_class").unwrap();
        assert!(matches!(c, Some(ClassRef::Relation(_))));
        assert_eq!(env.lookup_class(Some("nos"), "x").unwrap(), None);
        let e = env.resolve_class(None, "nosuch").unwrap_err();
        assert_eq!(e.sqlstate, SqlState::UNDEFINED_TABLE);
        assert_eq!(e.message, "relation \"nosuch\" does not exist");
    }

    #[test]
    fn creation_namespace_rules() {
        let (_kv, env) = testing::bootstrapped().unwrap();
        let e = env.creation_namespace_for(Some("pg_catalog"), "t").unwrap_err();
        assert_eq!(e.sqlstate, SqlState::INSUFFICIENT_PRIVILEGE);
        assert_eq!(e.message, "permission denied to create \"pg_catalog.t\"");
        let e = env.creation_namespace_for(Some("nos"), "t").unwrap_err();
        assert_eq!(e.sqlstate, SqlState::INVALID_SCHEMA_NAME);
        assert_eq!(e.message, "schema \"nos\" does not exist");
        assert_eq!(env.creation_namespace_for(None, "t").unwrap(), builtin::PUBLIC_NAMESPACE);
        assert_eq!(
            env.creation_namespace_for(Some("information_schema"), "t").unwrap(),
            builtin::INFORMATION_SCHEMA_NAMESPACE
        );
    }

    #[test]
    fn builtin_types_and_functions() {
        let (_kv, env) = testing::bootstrapped().unwrap();
        assert_eq!(env.lookup_type(None, "int4").unwrap(), Some(23));
        assert_eq!(env.lookup_type(Some("pg_catalog"), "_int4").unwrap(), Some(1007));
        assert_eq!(env.lookup_type(None, "nosuchtype").unwrap(), None);
        assert!(env.type_is_visible(23).unwrap());
        assert!(!env.type_is_visible(999_999).unwrap());
        let cands = env.function_candidates(None, "abs").unwrap();
        assert!(cands.len() >= 2);
        assert!(cands.iter().all(|c| c.path_pos == 0 && c.name == "abs"));
        assert!(env.function_candidates(None, "no_such_fn_xyz").unwrap().is_empty());
        let e = env.function_candidates(Some("nos"), "abs").unwrap_err();
        assert_eq!(e.message, "schema \"nos\" does not exist");
        assert!(env.function_is_visible(cands[0].oid).unwrap());
    }

    #[test]
    fn roles_and_collations() {
        let (_kv, env) = testing::bootstrapped().unwrap();
        assert_eq!(env.role_oid("postgres").unwrap(), 10);
        assert_eq!(env.role_oid("nosuch").unwrap_err().message, "role \"nosuch\" does not exist");
        assert_eq!(env.collation_oid(None, "default").unwrap(), 100);
        assert_eq!(env.collation_oid(None, "C").unwrap(), 950);
        assert_eq!(env.collation_oid(Some("pg_catalog"), "POSIX").unwrap(), 951);
        let e = env.collation_oid(None, "nosuch").unwrap_err();
        assert_eq!(e.message, "collation \"nosuch\" for encoding \"UTF8\" does not exist");
    }

    #[test]
    fn display_names_and_qualifier() {
        let (_kv, env) = testing::bootstrapped().unwrap();
        let oid = env.lookup_class(None, "pg_class").unwrap().map(|c| c.oid()).unwrap_or(0);
        assert_ne!(oid, 0);
        assert!(env.relation_is_visible(oid).unwrap());
        assert_eq!(env.qualified_class_name(oid).unwrap(), "pg_class");
        assert!(!env.relation_is_visible(4_000_000_000).unwrap());
        let e = env.qualified_class_name(4_000_000_000).unwrap_err();
        assert_eq!(e.message, "cache lookup failed for relation 4000000000");
        assert_eq!(env.schema_qualifier(builtin::PUBLIC_NAMESPACE, "t", true).unwrap(), None);
        assert_eq!(
            env.schema_qualifier(builtin::PG_TOAST_NAMESPACE, "t", true).unwrap(),
            Some("pg_toast".to_string())
        );
    }

    #[test]
    fn user_objects_resolution_and_shadowing() {
        let (mut kv, env) = testing::bootstrapped().unwrap();
        let public = builtin::PUBLIC_NAMESPACE;
        let (_r, env) =
            testing::run(&mut kv, &env, |t| t.create_table(&table_spec(public, "t", true)).map(|_| ())).unwrap();
        let (_r, env) = testing::run(&mut kv, &env, |t| {
            t.create_enum(&CreateEnumSpec {
                namespace: public,
                name: "mood".into(),
                owner: builtin::BOOTSTRAP_SUPERUSER,
                labels: vec!["sad".into(), "ok".into()],
            })
            .map(|_| ())
        })
        .unwrap();

        let rel = env.resolve_relation(None, "t").unwrap();
        assert_eq!(rel.name, "t");
        let e = env.resolve_relation(None, "t_pkey").unwrap_err();
        assert_eq!(e.sqlstate, SqlState::WRONG_OBJECT_TYPE);
        assert_eq!(e.message, "cannot open relation \"t_pkey\"");
        assert_eq!(e.detail.as_deref(), Some("This operation is not supported for indexes."));

        let mood = env.lookup_type(None, "mood").unwrap().unwrap();
        let arr = env.lookup_type(None, "_mood").unwrap().unwrap();
        assert_ne!(mood, arr);
        assert!(env.type_is_visible(mood).unwrap());
        assert!(env.type_is_visible(arr).unwrap());

        // A same-named table in public is hidden by pg_catalog's earlier entry.
        let (_r, env) =
            testing::run(&mut kv, &env, |t| t.create_table(&table_spec(public, "pg_class", false)).map(|_| ()))
                .unwrap();
        let shadow = env.lookup_class(Some("public"), "pg_class").unwrap().unwrap().oid();
        let sys = env.lookup_class(Some("pg_catalog"), "pg_class").unwrap().unwrap().oid();
        assert!(!env.relation_is_visible(shadow).unwrap());
        assert!(env.relation_is_visible(sys).unwrap());
        assert_eq!(env.qualified_class_name(shadow).unwrap(), "public.pg_class");
        assert_eq!(env.lookup_class(None, "pg_class").unwrap().unwrap().oid(), sys);
    }
}
