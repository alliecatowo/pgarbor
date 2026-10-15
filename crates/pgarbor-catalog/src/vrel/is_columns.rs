//! information_schema, part 2 (columns and types): `columns`, `element_types`, `domains`,
//! `domain_constraints`, `column_domain_usage`, `column_udt_usage`, `domain_udt_usage`,
//! `column_column_usage`. Lease CAT-41. Authoritative semantics: the CREATE VIEW of each name in
//! PG 18.6 `src/backend/catalog/information_schema.sql`; visibility, domain/Datum conventions and the
//! `RoleGraph` privilege test: see `is_core.rs`.
//!
//! ## Shared helper [`describe_type_for_info_schema`]
//! Everything that prints a column/parameter/attribute type uses the same derivation (PG's
//! `_pg_char_max_length`, `_pg_char_octet_length`, `_pg_numeric_precision(_radix)`,
//! `_pg_numeric_scale`, `_pg_datetime_precision`, `_pg_interval_type`, `_pg_truetypid/_pg_truetypmod`,
//! and the `data_type`/`udt_*` CASE expressions). Oracle examples (PG 18.6, `information_schema.columns`):
//! | declared type | data_type | char_max_len | char_octet_len | num_prec | radix | scale | udt_name |
//! |---|---|---|---|---|---|---|---|
//! | `int` | integer | | | 32 | 2 | 0 | int4 |
//! | `varchar(10)` | character varying | 10 | 40 | | | | varchar |
//! | `text` | text | | 1073741824 | | | | text |
//! | `numeric(10,2)` | numeric | | | 10 | 10 | 2 | numeric |
//! | `int[]` | ARRAY | | | | | | _int4 |
//! | enum `mood` | USER-DEFINED | | | | | | mood (udt_schema = enum's schema) |
//! | domain `pos` over int | integer (the BASE type) | | | 32 | 2 | 0 | int4 (+ domain_catalog/schema/name filled) |
//! | `timestamp(3)` | timestamp without time zone | | | | | | datetime_precision 3 |
//! `udt_schema` is the type's schema (`pg_catalog` for builtins); `udt_catalog` the database. Octet
//! length is `4 * n` for UTF8 `varchar(n)`/`char(n)` and 1073741824 for unbounded text. Typmod decoding
//! lives in `pgarbor_types::typmod`. `collation_name` is filled ONLY when the column's collation
//! differs from its type's default collation (`NULL` for default text columns - oracle).
//!
//! ## Relations
//! - **columns**: one row per non-dropped, non-system column of tables/views/matviews?/composite? -
//!   PG: relkind IN ('r','v','f','p') (NOT matviews, NOT composite), visible per `is_core.rs`,
//!   `attnum > 0`. Oracle for `create table t(id serial ..., a varchar(10) not null default 'x', ...)`:
//!   `ordinal_position` = attnum (dropped columns leave gaps), `column_default` = the default text
//!   (`nextval('t_id_seq'::regclass)`, `'x'::character varying`) - NULL for generated columns,
//!   `is_nullable` `NO`/`YES` from `not_null`, `dtd_identifier` = attnum as text,
//!   `is_self_referencing` `NO`, `is_identity` `YES`/`NO` with `identity_generation`
//!   (`ALWAYS`/`BY DEFAULT`), `identity_start/increment/maximum/minimum` (text of the identity sequence
//!   values) and `identity_cycle` `YES`/`NO` (NO for non-identity), `is_generated` `ALWAYS`/`NEVER`,
//!   `generation_expression` (the generation expression text, e.g. `((a)::text || 'y'::text)`),
//!   `is_updatable` `YES` for base-table columns (`YES` for views only if updatable: `NO`).
//!   `scope_*`, `maximum_cardinality`, `character_set_*` NULL (`character_set_name` is set for
//!   character types in PG: `character_set_catalog/schema` NULL, `character_set_name` = `UTF8`).
//! - **element_types**: one row per array-typed column/parameter/attribute: `object_catalog,
//!   object_schema, object_name, object_type` (`TABLE`, `USER-DEFINED TYPE`, `ROUTINE`, ...),
//!   `collection_type_identifier` (= `dtd_identifier`), `data_type` of the ELEMENT (+ the same
//!   detail columns), `udt_*` of the element, `dtd_identifier` = `a`-style as PG prints.
//! - **domains**: `domain_catalog/schema/name, data_type` (base type) + type-detail columns,
//!   `domain_default` (default text), `udt_*` of the base type, `dtd_identifier` `1`. Visible by
//!   `pg_has_role(typowner, 'USAGE') OR has_type_privilege(oid, 'USAGE')`.
//! - **domain_constraints**: every `Constraint` with `domain != 0`: `constraint_catalog/schema/name,
//!   domain_catalog/schema/name, is_deferrable` `NO`, `initially_deferred` `NO` (includes PG 18
//!   `NOT NULL` domain constraints).
//! - **column_domain_usage**: columns whose declared type is a domain: `domain_catalog/schema/name,
//!   table_catalog/schema/name, column_name`.
//! - **column_udt_usage**: every column of tables/views with its `udt_catalog/schema/name` (base type
//!   name for domains): oracle shows `int4`, `text`, ... for all columns.
//! - **domain_udt_usage**: `udt_catalog/schema/name` of each domain's base type + domain identity.
//! - **column_column_usage**: for each generated column, one row per column its expression depends
//!   on: `table_catalog/schema/name, column_name, dependent_column` (from `Column::default.expr.depends_on`
//!   column addresses of the same table).
use super::{Row, ScanContext, ScanRequest, VirtualRelation};
use crate::builtin::relid;
use crate::env::CatalogEnv;
use pgarbor_common::{Oid, PgError, Result};

/// Type-derived columns shared by columns/attributes/parameters/domains/element_types
/// (module docs table). `None` = SQL NULL.
// CONTRACT
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct InfoSchemaType {
    pub data_type: String,
    pub character_maximum_length: Option<i32>,
    pub character_octet_length: Option<i32>,
    pub numeric_precision: Option<i32>,
    pub numeric_precision_radix: Option<i32>,
    pub numeric_scale: Option<i32>,
    pub datetime_precision: Option<i32>,
    pub interval_type: Option<String>,
    pub interval_precision: Option<i32>,
    /// Schema and name of the (base) UDT: `pg_catalog`/`int4`.
    pub udt_schema: String,
    pub udt_name: String,
    /// Set when the declared type is a domain.
    pub domain_schema: Option<String>,
    pub domain_name: Option<String>,
    /// Element type description for arrays (`data_type == "ARRAY"`).
    pub element: Option<Box<InfoSchemaType>>,
}

/// Derive [`InfoSchemaType`] for `(type_oid, typmod)` (module docs). Unknown type -> XX000
/// `cache lookup failed for type <oid>`.
// CONTRACT
#[allow(unused_variables)]
pub fn describe_type_for_info_schema(env: &CatalogEnv, type_oid: Oid, typmod: i32) -> Result<InfoSchemaType> {
    Err(PgError::unimplemented("CAT-41", "information_schema type description"))
}

/// Producer for `information_schema.columns`.
#[derive(Clone, Copy, Debug)]
pub struct IsColumns;

impl VirtualRelation for IsColumns {
    fn oid(&self) -> Oid {
        relid::IS_COLUMNS
    }
    #[allow(unused_variables)]
    fn scan(&self, ctx: &ScanContext, req: &ScanRequest) -> Result<Vec<Row>> {
        Err(PgError::unimplemented("CAT-41", "information_schema.columns rows"))
    }
}

/// Producer for `information_schema.element_types`.
#[derive(Clone, Copy, Debug)]
pub struct IsElementTypes;

impl VirtualRelation for IsElementTypes {
    fn oid(&self) -> Oid {
        relid::IS_ELEMENT_TYPES
    }
    #[allow(unused_variables)]
    fn scan(&self, ctx: &ScanContext, req: &ScanRequest) -> Result<Vec<Row>> {
        Err(PgError::unimplemented("CAT-41", "information_schema.element_types rows"))
    }
}

/// Producer for `information_schema.domains`.
#[derive(Clone, Copy, Debug)]
pub struct IsDomains;

impl VirtualRelation for IsDomains {
    fn oid(&self) -> Oid {
        relid::IS_DOMAINS
    }
    #[allow(unused_variables)]
    fn scan(&self, ctx: &ScanContext, req: &ScanRequest) -> Result<Vec<Row>> {
        Err(PgError::unimplemented("CAT-41", "information_schema.domains rows"))
    }
}

/// Producer for `information_schema.domain_constraints`.
#[derive(Clone, Copy, Debug)]
pub struct IsDomainConstraints;

impl VirtualRelation for IsDomainConstraints {
    fn oid(&self) -> Oid {
        relid::IS_DOMAIN_CONSTRAINTS
    }
    #[allow(unused_variables)]
    fn scan(&self, ctx: &ScanContext, req: &ScanRequest) -> Result<Vec<Row>> {
        Err(PgError::unimplemented("CAT-41", "information_schema.domain_constraints rows"))
    }
}

/// Producer for `information_schema.column_domain_usage`.
#[derive(Clone, Copy, Debug)]
pub struct IsColumnDomainUsage;

impl VirtualRelation for IsColumnDomainUsage {
    fn oid(&self) -> Oid {
        relid::IS_COLUMN_DOMAIN_USAGE
    }
    #[allow(unused_variables)]
    fn scan(&self, ctx: &ScanContext, req: &ScanRequest) -> Result<Vec<Row>> {
        Err(PgError::unimplemented("CAT-41", "information_schema.column_domain_usage rows"))
    }
}

/// Producer for `information_schema.column_udt_usage`.
#[derive(Clone, Copy, Debug)]
pub struct IsColumnUdtUsage;

impl VirtualRelation for IsColumnUdtUsage {
    fn oid(&self) -> Oid {
        relid::IS_COLUMN_UDT_USAGE
    }
    #[allow(unused_variables)]
    fn scan(&self, ctx: &ScanContext, req: &ScanRequest) -> Result<Vec<Row>> {
        Err(PgError::unimplemented("CAT-41", "information_schema.column_udt_usage rows"))
    }
}

/// Producer for `information_schema.domain_udt_usage`.
#[derive(Clone, Copy, Debug)]
pub struct IsDomainUdtUsage;

impl VirtualRelation for IsDomainUdtUsage {
    fn oid(&self) -> Oid {
        relid::IS_DOMAIN_UDT_USAGE
    }
    #[allow(unused_variables)]
    fn scan(&self, ctx: &ScanContext, req: &ScanRequest) -> Result<Vec<Row>> {
        Err(PgError::unimplemented("CAT-41", "information_schema.domain_udt_usage rows"))
    }
}

/// Producer for `information_schema.column_column_usage`.
#[derive(Clone, Copy, Debug)]
pub struct IsColumnColumnUsage;

impl VirtualRelation for IsColumnColumnUsage {
    fn oid(&self) -> Oid {
        relid::IS_COLUMN_COLUMN_USAGE
    }
    #[allow(unused_variables)]
    fn scan(&self, ctx: &ScanContext, req: &ScanRequest) -> Result<Vec<Row>> {
        Err(PgError::unimplemented("CAT-41", "information_schema.column_column_usage rows"))
    }
}


/// Registry slice.
pub static RELATIONS: &[&dyn VirtualRelation] = &[
    &IsColumns,
    &IsElementTypes,
    &IsDomains,
    &IsDomainConstraints,
    &IsColumnDomainUsage,
    &IsColumnUdtUsage,
    &IsDomainUdtUsage,
    &IsColumnColumnUsage,
];
