//! information_schema, part 1: `schemata`, `tables`, `views`, `sequences`,
//! `information_schema_catalog_name`, `character_sets`, `collations`,
//! `collation_character_set_applicability`, `enabled_roles`, `applicable_roles`,
//! `administrable_role_authorizations`, `user_defined_types`, `attributes`. Lease CAT-40.
//!
//! Authoritative semantics: PostgreSQL's `src/backend/catalog/information_schema.sql` (PG 18.6; read
//! the CREATE VIEW of each relation named below and port the WHERE clauses and expressions to the
//! model). Column lists/types: `builtin::system_rel(relid::IS_*)`. information_schema column types
//! are DOMAINS: `sql_identifier`, `character_data`, `yes_or_no`, `cardinal_number` ->
//! `Datum::Text`/`Text`/`Text`/`Int4`; `time_stamp` -> `TimestampTz`.
//!
//! ## Visibility (applies to every IS view, here and in the other `is_*` files)
//! IS views only show objects the CURRENT USER may see: `pg_has_role(owner, 'USAGE')` (owner is the
//! user or a role the user inherits - `acl::RoleGraph::has_privs_of_role`) OR a privilege on the
//! object (`has_table_privilege(oid, 'SELECT, INSERT, UPDATE, DELETE, TRUNCATE, REFERENCES,
//! TRIGGER')` or any column privilege). Superusers see everything. Evaluate with
//! `acl::privilege_mask(graph, ctx.env.current_user(), owner, effective_acl, mask, false)` against
//! `RoleGraph::build(snapshot, ctx.env.database())`. `table_catalog` etc. = the current database
//! name (`snapshot.database(env.database()).name`, i.e. `postgres`). Pass the temp-schema test
//! `pg_is_other_temp_schema` as always false (no temp schemas yet).
//!
//! ## Relations
//! - **schemata**: every namespace (oracle: `information_schema`, `pg_catalog`, `pg_toast`, `public`
//!   plus per-session `pg_temp_N`/`pg_toast_temp_N`, which PGArbor does not have):
//!   `catalog_name, schema_name, schema_owner` (role name), `default_character_set_catalog` NULL,
//!   `default_character_set_schema` NULL, `default_character_set_name` NULL, `sql_path` NULL.
//!   Visible if `pg_has_role(nspowner, 'USAGE')` or `has_schema_privilege(oid, 'CREATE, USAGE')`.
//! - **tables**: relkind r, v, f, p (NOT matviews, NOT sequences): `table_catalog, table_schema,
//!   table_name, table_type` (`BASE TABLE` for r/p, `VIEW` for v, `FOREIGN` for f, `LOCAL TEMPORARY` for
//!   temp), `self_referencing_column_name` NULL, `reference_generation` NULL, `user_defined_type_*`
//!   NULL, `is_insertable_into` (`YES` for tables; views: `YES` when auto-updatable per
//!   `pg_relation_is_updatable(oid, false) & 8`: PGArbor supports no auto-updatable views in v1 -> `NO`),
//!   `is_typed` `NO`, `commit_action` NULL. Includes system tables/views of pg_catalog and
//!   information_schema (they are in `builtin::SYSTEM_RELS` and `snapshot.relation()`).
//! - **views**: relkind v: `table_catalog, table_schema, table_name, view_definition` (=
//!   `deparse::viewdef` text, NULL if the user lacks privilege like PG: only owner/`pg_has_role`),
//!   `check_option` (`NONE`, `LOCAL`, `CASCADED`), `is_updatable` / `is_insertable_into` (`NO`, v1),
//!   `is_trigger_updatable` / `is_trigger_deletable` / `is_trigger_insertable_into` (`NO`).
//! - **sequences**: `sequence_catalog, sequence_schema, sequence_name, data_type` (`smallint|integer|bigint`),
//!   `numeric_precision` (16/32/64), `numeric_precision_radix` 2, `numeric_scale` 0, `start_value,
//!   minimum_value, maximum_value, increment` (all `character_data` = decimal TEXT of the i64),
//!   `cycle_option` (`YES`/`NO`).
//! - **information_schema_catalog_name**: one row, `catalog_name` = current database.
//! - **character_sets**: one row (oracle): `character_set_catalog` NULL, `character_set_schema` NULL,
//!   `character_set_name` `UTF8`, `character_repertoire` `UCS`, `form_of_use` `UTF8`,
//!   `default_collate_catalog` = current database, `default_collate_schema` NULL,
//!   `default_collate_name` NULL.
//! - **collations** / **collation_character_set_applicability**: one row per `builtin::COLLATIONS`
//!   entry (oracle: 880 rows, all `pg_catalog`): `collation_catalog` = database, `collation_schema`
//!   `pg_catalog`, `collation_name`, `pad_attribute` `NO PAD`; applicability: `collation_catalog,
//!   collation_schema, collation_name, character_set_catalog` NULL, `character_set_schema` NULL,
//!   `character_set_name` `UTF8`.
//! - **enabled_roles**: `role_name` of every role `r` with `pg_has_role(current_user, r, 'USAGE')`
//!   (= `RoleGraph::has_privs_of_role`; a superuser therefore sees ALL roles - oracle lists every
//!   predefined role plus `postgres`). **applicable_roles**: `grantee, role_name, is_grantable` (`YES` if
//!   admin option) over `RoleMembership` edges reachable from the current user (PG: recursive).
//!   **administrable_role_authorizations**: applicable_roles rows with `is_grantable = YES`.
//! - **user_defined_types**: composite types only (relkind c): `user_defined_type_catalog/schema/name,
//!   user_defined_type_category` (`STRUCTURED`), `is_instantiable` `YES`, `is_final` `YES`, remaining
//!   columns NULL (`ordering_form` NULL, ... as PG).
//! - **attributes**: attributes of composite types (relkind c, visible): `udt_catalog, udt_schema,
//!   udt_name, attribute_name, ordinal_position` (attnum), `attribute_default` NULL, `is_nullable`
//!   (`YES`), `data_type` (`describe_type_for_info_schema`), the same type-detail columns as
//!   `information_schema.columns`, `attribute_udt_catalog/schema/name`, `dtd_identifier`
//!   (attnum text), `is_derived_reference_attribute` `NO`.
use super::{Row, ScanContext, ScanRequest, VirtualRelation};
use crate::builtin::relid;
use pgarbor_common::{Oid, PgError, Result};

/// Producer for `information_schema.schemata`.
#[derive(Clone, Copy, Debug)]
pub struct IsSchemata;

impl VirtualRelation for IsSchemata {
    fn oid(&self) -> Oid {
        relid::IS_SCHEMATA
    }
    #[allow(unused_variables)]
    fn scan(&self, ctx: &ScanContext, req: &ScanRequest) -> Result<Vec<Row>> {
        Err(PgError::unimplemented("CAT-40", "information_schema.schemata rows"))
    }
}

/// Producer for `information_schema.tables`.
#[derive(Clone, Copy, Debug)]
pub struct IsTables;

impl VirtualRelation for IsTables {
    fn oid(&self) -> Oid {
        relid::IS_TABLES
    }
    #[allow(unused_variables)]
    fn scan(&self, ctx: &ScanContext, req: &ScanRequest) -> Result<Vec<Row>> {
        Err(PgError::unimplemented("CAT-40", "information_schema.tables rows"))
    }
}

/// Producer for `information_schema.views`.
#[derive(Clone, Copy, Debug)]
pub struct IsViews;

impl VirtualRelation for IsViews {
    fn oid(&self) -> Oid {
        relid::IS_VIEWS
    }
    #[allow(unused_variables)]
    fn scan(&self, ctx: &ScanContext, req: &ScanRequest) -> Result<Vec<Row>> {
        Err(PgError::unimplemented("CAT-40", "information_schema.views rows"))
    }
}

/// Producer for `information_schema.sequences`.
#[derive(Clone, Copy, Debug)]
pub struct IsSequences;

impl VirtualRelation for IsSequences {
    fn oid(&self) -> Oid {
        relid::IS_SEQUENCES
    }
    #[allow(unused_variables)]
    fn scan(&self, ctx: &ScanContext, req: &ScanRequest) -> Result<Vec<Row>> {
        Err(PgError::unimplemented("CAT-40", "information_schema.sequences rows"))
    }
}

/// Producer for `information_schema.information_schema_catalog_name`.
#[derive(Clone, Copy, Debug)]
pub struct IsCatalogName;

impl VirtualRelation for IsCatalogName {
    fn oid(&self) -> Oid {
        relid::IS_INFORMATION_SCHEMA_CATALOG_NAME
    }
    #[allow(unused_variables)]
    fn scan(&self, ctx: &ScanContext, req: &ScanRequest) -> Result<Vec<Row>> {
        Err(PgError::unimplemented("CAT-40", "information_schema.information_schema_catalog_name rows"))
    }
}

/// Producer for `information_schema.character_sets`.
#[derive(Clone, Copy, Debug)]
pub struct IsCharacterSets;

impl VirtualRelation for IsCharacterSets {
    fn oid(&self) -> Oid {
        relid::IS_CHARACTER_SETS
    }
    #[allow(unused_variables)]
    fn scan(&self, ctx: &ScanContext, req: &ScanRequest) -> Result<Vec<Row>> {
        Err(PgError::unimplemented("CAT-40", "information_schema.character_sets rows"))
    }
}

/// Producer for `information_schema.collations`.
#[derive(Clone, Copy, Debug)]
pub struct IsCollations;

impl VirtualRelation for IsCollations {
    fn oid(&self) -> Oid {
        relid::IS_COLLATIONS
    }
    #[allow(unused_variables)]
    fn scan(&self, ctx: &ScanContext, req: &ScanRequest) -> Result<Vec<Row>> {
        Err(PgError::unimplemented("CAT-40", "information_schema.collations rows"))
    }
}

/// Producer for `information_schema.collation_character_set_applicability`.
#[derive(Clone, Copy, Debug)]
pub struct IsCollationCharsetApplicability;

impl VirtualRelation for IsCollationCharsetApplicability {
    fn oid(&self) -> Oid {
        relid::IS_COLLATION_CHARACTER_SET_APPLICABILITY
    }
    #[allow(unused_variables)]
    fn scan(&self, ctx: &ScanContext, req: &ScanRequest) -> Result<Vec<Row>> {
        Err(PgError::unimplemented("CAT-40", "information_schema.collation_character_set_applicability rows"))
    }
}

/// Producer for `information_schema.enabled_roles`.
#[derive(Clone, Copy, Debug)]
pub struct IsEnabledRoles;

impl VirtualRelation for IsEnabledRoles {
    fn oid(&self) -> Oid {
        relid::IS_ENABLED_ROLES
    }
    #[allow(unused_variables)]
    fn scan(&self, ctx: &ScanContext, req: &ScanRequest) -> Result<Vec<Row>> {
        Err(PgError::unimplemented("CAT-40", "information_schema.enabled_roles rows"))
    }
}

/// Producer for `information_schema.applicable_roles`.
#[derive(Clone, Copy, Debug)]
pub struct IsApplicableRoles;

impl VirtualRelation for IsApplicableRoles {
    fn oid(&self) -> Oid {
        relid::IS_APPLICABLE_ROLES
    }
    #[allow(unused_variables)]
    fn scan(&self, ctx: &ScanContext, req: &ScanRequest) -> Result<Vec<Row>> {
        Err(PgError::unimplemented("CAT-40", "information_schema.applicable_roles rows"))
    }
}

/// Producer for `information_schema.administrable_role_authorizations`.
#[derive(Clone, Copy, Debug)]
pub struct IsAdministrableRoleAuthorizations;

impl VirtualRelation for IsAdministrableRoleAuthorizations {
    fn oid(&self) -> Oid {
        relid::IS_ADMINISTRABLE_ROLE_AUTHORIZATIONS
    }
    #[allow(unused_variables)]
    fn scan(&self, ctx: &ScanContext, req: &ScanRequest) -> Result<Vec<Row>> {
        Err(PgError::unimplemented("CAT-40", "information_schema.administrable_role_authorizations rows"))
    }
}

/// Producer for `information_schema.user_defined_types`.
#[derive(Clone, Copy, Debug)]
pub struct IsUserDefinedTypes;

impl VirtualRelation for IsUserDefinedTypes {
    fn oid(&self) -> Oid {
        relid::IS_USER_DEFINED_TYPES
    }
    #[allow(unused_variables)]
    fn scan(&self, ctx: &ScanContext, req: &ScanRequest) -> Result<Vec<Row>> {
        Err(PgError::unimplemented("CAT-40", "information_schema.user_defined_types rows"))
    }
}

/// Producer for `information_schema.attributes`.
#[derive(Clone, Copy, Debug)]
pub struct IsAttributes;

impl VirtualRelation for IsAttributes {
    fn oid(&self) -> Oid {
        relid::IS_ATTRIBUTES
    }
    #[allow(unused_variables)]
    fn scan(&self, ctx: &ScanContext, req: &ScanRequest) -> Result<Vec<Row>> {
        Err(PgError::unimplemented("CAT-40", "information_schema.attributes rows"))
    }
}


/// Registry slice.
pub static RELATIONS: &[&dyn VirtualRelation] = &[
    &IsSchemata,
    &IsTables,
    &IsViews,
    &IsSequences,
    &IsCatalogName,
    &IsCharacterSets,
    &IsCollations,
    &IsCollationCharsetApplicability,
    &IsEnabledRoles,
    &IsApplicableRoles,
    &IsAdministrableRoleAuthorizations,
    &IsUserDefinedTypes,
    &IsAttributes,
];
