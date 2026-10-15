//! Shared DDL helpers: ownership/privilege checks, name-clash checks, column list validation.
//! Lease CAT-08.
//!
//! PG sources: `utils/adt/acl.c` (`object_ownercheck`, `pg_class_aclcheck`, `aclcheck_error`),
//! `catalog/namespace.c` (`CheckSetNamespace`), `catalog/heap.c` (`CheckAttributeNamesTypes`,
//! `SystemAttributeByName`), `commands/tablecmds.c` (`DefineRelation` checks), `catalog/pg_type.c`
//! (`TypeCreate` name clash), `catalog/heap.c` (`heap_create_with_catalog` clash -> 42P07).
use crate::env::CatalogEnv;
use crate::model::*;
use crate::txn::CatalogTxn;
use pgarbor_common::{Oid, PgError, Result};

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
#[allow(unused_variables)]
pub fn validate_column_specs(
    env: &CatalogEnv,
    table_name: &str,
    columns: &[crate::ddl::spec::ColumnSpec],
) -> Result<()> {
    Err(PgError::unimplemented("CAT-08", "column definition validation"))
}

impl<'a> CatalogTxn<'a> {
    /// True if `role` is a superuser (role attribute `rolsuper`; the bootstrap superuser always is).
    // CONTRACT
    #[allow(unused_variables)]
    pub fn is_superuser(&mut self, role: Oid) -> Result<bool> {
        Err(PgError::unimplemented("CAT-08", "superuser check"))
    }

    /// `has_privs_of_role(current_user, owner)` or superuser: PG `object_ownercheck`. The caller of
    /// every ALTER/DROP/COMMENT/GRANT-as-owner calls this. Error: 42501 `must be owner of <kind> <name>`
    /// (kind word from `describe::type_description`-style words: `table`, `view`, `materialized view`,
    /// `index`, `sequence`, `type`, `schema`, `function`, `procedure`, `database`, `role`; for domains
    /// PG says `type`). Column addresses check the table. Objects with no owner concept (constraints,
    /// attrdefs) check their table.
    // CONTRACT
    #[allow(unused_variables)]
    pub fn require_owner(&mut self, addr: &ObjectAddress) -> Result<()> {
        Err(PgError::unimplemented("CAT-08", "ownership check"))
    }

    /// CREATE privilege on schema `ns` for the current user (owner of the schema or superuser or an
    /// ACL grant of C), else 42501 `permission denied for schema <name>`. `pg_catalog` /
    /// `pg_toast`: 42501 `permission denied to create "pg_catalog.x"` is raised by
    /// `CatalogEnv::creation_namespace_for`, not here.
    // CONTRACT
    #[allow(unused_variables)]
    pub fn require_schema_create(&mut self, ns: Oid) -> Result<()> {
        Err(PgError::unimplemented("CAT-08", "schema CREATE privilege check"))
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
    #[allow(unused_variables)]
    pub fn check_new_relation_name(&mut self, ns: Oid, name: &str, if_not_exists: bool) -> Result<bool> {
        Err(PgError::unimplemented("CAT-08", "relation name clash check"))
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
    #[allow(unused_variables)]
    pub fn resolve_type_name_clash(&mut self, ns: Oid, name: &str) -> Result<()> {
        Err(PgError::unimplemented("CAT-08", "type name clash resolution"))
    }

    /// Reject creating/moving objects into `pg_catalog`, `pg_toast`, or temp schemas of other
    /// sessions: 42501 `permission denied to create "<schema>.<name>"` + DETAIL
    /// `System catalog modifications are currently disallowed.` (`errors::system_catalog_create_denied`).
    /// `information_schema` is allowed for superusers only.
    // CONTRACT
    #[allow(unused_variables)]
    pub fn check_creatable_namespace(&mut self, ns: Oid, name: &str) -> Result<()> {
        Err(PgError::unimplemented("CAT-08", "creatable namespace check"))
    }

    /// Owner OID of any object address (relations/indexes/sequences/types/functions/schemas/
    /// databases/roles(self)/constraints and attrdefs -> table owner). Missing -> XX000
    /// `cache lookup failed`.
    // CONTRACT
    #[allow(unused_variables)]
    pub fn owner_of(&mut self, addr: &ObjectAddress) -> Result<Oid> {
        Err(PgError::unimplemented("CAT-08", "object owner lookup"))
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
