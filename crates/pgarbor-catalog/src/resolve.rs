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
use crate::env::CatalogEnv;
use crate::model::Relation;
use crate::snapshot::ClassRef;
use pgarbor_common::{Oid, PgError, Result};
use pgarbor_types::resolve::func::FuncCandidate;

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
    #[allow(unused_variables)]
    pub fn lookup_class(&self, schema: Option<&str>, name: &str) -> Result<Option<ClassRef>> {
        Err(PgError::unimplemented("CAT-04", "relation name lookup"))
    }

    /// Like `lookup_class` but 42P01 `relation "x" does not exist` when not found.
    // CONTRACT
    #[allow(unused_variables)]
    pub fn resolve_class(&self, schema: Option<&str>, name: &str) -> Result<ClassRef> {
        Err(PgError::unimplemented("CAT-04", "relation name resolution"))
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
    #[allow(unused_variables)]
    pub fn resolve_relation(&self, schema: Option<&str>, name: &str) -> Result<&Relation> {
        Err(PgError::unimplemented("CAT-04", "relation resolution"))
    }

    /// Target schema for CREATE of `name` (see module docs).
    // CONTRACT
    #[allow(unused_variables)]
    pub fn creation_namespace_for(&self, schema: Option<&str>, name: &str) -> Result<Oid> {
        Err(PgError::unimplemented("CAT-04", "creation namespace selection"))
    }

    /// Type OID by catalog typname (builtin or user), honoring the search path (module docs).
    // CONTRACT
    #[allow(unused_variables)]
    pub fn lookup_type(&self, schema: Option<&str>, typname: &str) -> Result<Option<Oid>> {
        Err(PgError::unimplemented("CAT-04", "type name lookup"))
    }

    /// Candidate functions/procedures/aggregates named `name` (module docs).
    // CONTRACT
    #[allow(unused_variables)]
    pub fn function_candidates(&self, schema: Option<&str>, name: &str) -> Result<Vec<FuncCandidate>> {
        Err(PgError::unimplemented("CAT-04", "function candidate lookup"))
    }

    /// Role OID by name, or 42704 `role "x" does not exist`.
    // CONTRACT
    pub fn role_oid(&self, name: &str) -> Result<Oid> {
        self.snapshot().role_by_name(name).map(|r| r.oid).ok_or_else(|| crate::errors::role_does_not_exist(name))
    }

    /// Collation OID by possibly-qualified name (module docs).
    // CONTRACT
    #[allow(unused_variables)]
    pub fn collation_oid(&self, schema: Option<&str>, name: &str) -> Result<Oid> {
        Err(PgError::unimplemented("CAT-04", "collation lookup"))
    }

    /// PG `RelationIsVisible` for any pg_class OID (false for unknown OIDs).
    // CONTRACT
    #[allow(unused_variables)]
    pub fn relation_is_visible(&self, oid: Oid) -> Result<bool> {
        Err(PgError::unimplemented("CAT-04", "pg_table_is_visible"))
    }

    /// PG `TypeIsVisible` (builtin or user type).
    // CONTRACT
    #[allow(unused_variables)]
    pub fn type_is_visible(&self, oid: Oid) -> Result<bool> {
        Err(PgError::unimplemented("CAT-04", "pg_type_is_visible"))
    }

    /// PG `FunctionIsVisible` (builtin or user function).
    // CONTRACT
    #[allow(unused_variables)]
    pub fn function_is_visible(&self, oid: Oid) -> Result<bool> {
        Err(PgError::unimplemented("CAT-04", "pg_function_is_visible"))
    }

    /// Display name of a pg_class object: `t`, `"Mixed"`, `other_schema.t` (module docs).
    /// Unknown OID → XX000 `cache lookup failed for relation <oid>`.
    // CONTRACT
    #[allow(unused_variables)]
    pub fn qualified_class_name(&self, oid: Oid) -> Result<String> {
        Err(PgError::unimplemented("CAT-04", "relation display name"))
    }

    /// Schema-qualified-if-invisible name of a namespace object for messages and descriptions:
    /// returns `Some(schema_name)` when the schema is NOT in the search path (PG
    /// `getObjectDescription` qualifies with `quote_identifier(schema)` only then), else None.
    // CONTRACT
    #[allow(unused_variables)]
    pub fn schema_qualifier(&self, namespace: Oid, name: &str, for_relation: bool) -> Result<Option<String>> {
        Err(PgError::unimplemented("CAT-04", "schema qualification"))
    }
}
