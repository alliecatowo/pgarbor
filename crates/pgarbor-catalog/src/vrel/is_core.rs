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
//!
//! ## Implementation notes (CAT-40)
//! - `user_defined_types.is_final` is NULL, as in PG 18.6 `information_schema.sql` (`CAST(null AS yes_or_no)`),
//!   not the `YES` listed above.
//! - System relations have no stored ACL in the compiled-in tables: they are treated as SELECT-granted to
//!   PUBLIC and owned by the bootstrap superuser (`relation_acl`). Non-superusers therefore see every system
//!   table and view, while PG hides relations such as `pg_authid` that have no PUBLIC grant.
//! - `applicable_roles` follows PG's view (UNION of `pg_auth_members`, the database owner pseudo-membership,
//!   filtered by `pg_has_role(member, 'USAGE')`). PG does not recurse; this filter already covers inherited roles.
//!   Builtin predefined-role memberships from `builtin::AUTH_MEMBERS` are included.
//! - `views.view_definition` is `deparse::viewdef`, shown only when `pg_has_role(relowner, 'USAGE')` holds, as in PG.
use super::is_columns::describe_type_for_info_schema;
use super::rowbuild::{d, RowBuilder};
use super::{Row, ScanContext, ScanRequest, VirtualRelation};
use crate::acl::{self, AclObject, RoleGraph};
use crate::builtin::{self, relid, BuiltinCollation, SystemRel, PG_DATABASE_OWNER};
use crate::deparse;
use crate::env::CatalogEnv;
use crate::model::{acl_mode, Acl, AclItem, RelKind, Relation, TypeDef};
use crate::naming;
use crate::snapshot::{CatalogSnapshot, UserTypeRef};
use pgarbor_common::{Oid, PgError, Result};
use pgarbor_types::registry::{self, TypeInfo};
use pgarbor_types::{oids, Datum};
use std::collections::BTreeSet;

/// Privileges named by `has_table_privilege(oid, 'SELECT, INSERT, UPDATE, DELETE, TRUNCATE,
/// REFERENCES, TRIGGER')`: true when ANY of them is held (PG semantics of comma lists).
const TABLE_ANY: u32 = acl_mode::SELECT
    | acl_mode::INSERT
    | acl_mode::UPDATE
    | acl_mode::DELETE
    | acl_mode::TRUNCATE
    | acl_mode::REFERENCES
    | acl_mode::TRIGGER;
/// `has_sequence_privilege(oid, 'SELECT, UPDATE, USAGE')`.
const SEQUENCE_ANY: u32 = acl_mode::SELECT | acl_mode::UPDATE | acl_mode::USAGE;
/// `has_schema_privilege(oid, 'CREATE, USAGE')`.
const SCHEMA_ANY: u32 = acl_mode::CREATE | acl_mode::USAGE;
/// `has_type_privilege(oid, 'USAGE')`.
const TYPE_ANY: u32 = acl_mode::USAGE;
/// First OID of user objects; system relations (and builtins) are below it.
const FIRST_USER_OID: Oid = 16384;
/// `getdatabaseencoding()`: PGArbor databases are always UTF8 (`builtin::UTF8_ENCODING`).
const DB_CHARSET: &str = "UTF8";

/// Per-scan state shared by the information_schema producers (module docs, "Visibility"): the
/// current user, the role graph built once for the scan, and the current database's attributes.
struct Session<'a> {
    env: &'a CatalogEnv,
    snap: &'a CatalogSnapshot,
    graph: RoleGraph,
    user: Oid,
    db_name: String,
    db_owner: Oid,
    db_encoding: i32,
    db_collate: String,
    db_ctype: String,
}

/// `YES` / `NO` as the information_schema `yes_or_no` domain.
fn yes_no(b: bool) -> Datum {
    d::text(if b { "YES" } else { "NO" })
}

/// Parse a builtin text cell holding an OID (malformed builtin data is a bug, reported as XX000).
fn text_oid(v: Option<&str>) -> Result<Oid> {
    v.and_then(|s| s.parse::<Oid>().ok())
        .ok_or_else(|| PgError::internal("malformed builtin pg_auth_members row"))
}

/// Effective table ACL of a relation. User relations use their stored ACL or the default. System
/// relations have no stored ACL in the compiled-in tables, so they are modelled as readable by
/// PUBLIC (SELECT) and owned by the bootstrap superuser: an approximation of PG's per-relation
/// `relacl` documented in the module docs.
fn relation_acl(rel: &Relation) -> Result<Acl> {
    if rel.oid < FIRST_USER_OID {
        return Ok(vec![AclItem { grantee: 0, grantor: rel.owner, privileges: acl_mode::SELECT, grant_options: 0 }]);
    }
    acl::effective_acl(AclObject::Table, rel.owner, rel.acl.as_ref())
}

impl<'a> Session<'a> {
    fn new(env: &'a CatalogEnv) -> Result<Session<'a>> {
        let snap = env.snapshot();
        let db_oid = env.database();
        let db = snap
            .database(db_oid)
            .ok_or_else(|| PgError::corruption(format!("current database {db_oid} does not exist")))?;
        Ok(Session {
            env,
            snap,
            graph: RoleGraph::build(snap, db_oid)?,
            user: env.current_user(),
            db_name: db.name.clone(),
            db_owner: db.owner,
            db_encoding: db.encoding,
            db_collate: db.collate.clone(),
            db_ctype: db.ctype.clone(),
        })
    }

    /// `pg_has_role(role, 'USAGE')`: the current user has the privileges of `role`.
    fn has_role_usage(&self, role: Oid) -> Result<bool> {
        self.graph.has_privs_of_role(self.user, role)
    }

    /// Any of `mask` held by the current user on an object with the given owner and effective ACL.
    fn any_privilege(&self, owner: Oid, acl: &Acl, mask: u32) -> Result<bool> {
        Ok(acl::privilege_mask(&self.graph, self.user, owner, acl, mask, false)? != 0)
    }

    fn ns_name(&self, oid: Oid) -> Result<&'a str> {
        match self.snap.namespace(oid) {
            Some(n) => Ok(n.name.as_str()),
            None => Err(PgError::corruption(format!("namespace {oid} does not exist"))),
        }
    }

    fn role_name(&self, oid: Oid) -> Option<&'a str> {
        self.snap.role_name(oid)
    }

    /// Every relation known to the snapshot: user relations and the builtin system tables/views,
    /// sorted by OID.
    fn all_relations(&self) -> Vec<&'a Relation> {
        let mut rels: Vec<&'a Relation> = self.snap.user_relations().collect();
        rels.extend(builtin::builtin_relations().iter());
        rels.sort_by_key(|r| r.oid);
        rels
    }

    /// Composite-type relations (`relkind c`), sorted by OID.
    fn composites(&self) -> Vec<&'a Relation> {
        self.snap.user_relations().filter(|r| r.kind == RelKind::CompositeType).collect()
    }

    /// Visibility of a table, view or composite relation (module docs): owner role, any table
    /// privilege, or any column privilege.
    fn relation_visible(&self, rel: &Relation) -> Result<bool> {
        if self.has_role_usage(rel.owner)? {
            return Ok(true);
        }
        if self.any_privilege(rel.owner, &relation_acl(rel)?, TABLE_ANY)? {
            return Ok(true);
        }
        for col in rel.columns.iter().filter(|c| !c.dropped) {
            let acl = acl::effective_acl(AclObject::Column, rel.owner, col.acl.as_ref())?;
            if self.any_privilege(rel.owner, &acl, acl_mode::ALL_COLUMN)? {
                return Ok(true);
            }
        }
        Ok(false)
    }

    /// Visibility of a composite type (`user_defined_types`, `attributes`): owner role or USAGE.
    fn type_usable(&self, rel: &Relation) -> Result<bool> {
        if self.has_role_usage(rel.owner)? {
            return Ok(true);
        }
        let acl = acl::effective_acl(AclObject::Type, rel.owner, rel.acl.as_ref())?;
        self.any_privilege(rel.owner, &acl, TYPE_ANY)
    }

    /// `(schema, name)` of a sequence-free composite or base type OID as `udt_*` / `attribute_udt_*`
    /// print the DECLARED type (domains are not resolved to their base type here).
    fn declared_type_name(&self, oid: Oid) -> Result<(String, String)> {
        if let Some(ti) = registry::lookup_type(oid).map(TypeInfo::from) {
            return Ok((self.ns_name(ti.namespace)?.to_string(), ti.name.into_owned()));
        }
        match self.snap.user_type(oid) {
            Some(UserTypeRef::Type(_)) => {
                let t = self
                    .snap
                    .type_obj(oid)
                    .ok_or_else(|| PgError::internal(format!("cache lookup failed for type {oid}")))?;
                Ok((self.ns_name(t.namespace)?.to_string(), t.name.clone()))
            }
            Some(UserTypeRef::TypeArray(elem)) => {
                let t = self
                    .snap
                    .type_obj(elem)
                    .ok_or_else(|| PgError::internal(format!("cache lookup failed for type {elem}")))?;
                let name = t.array_type_name.clone().unwrap_or_else(|| naming::array_type_name(&t.name));
                Ok((self.ns_name(t.namespace)?.to_string(), name))
            }
            Some(UserTypeRef::RowType(rel)) => {
                let r = self
                    .snap
                    .relation(rel)
                    .ok_or_else(|| PgError::internal(format!("cache lookup failed for type {oid}")))?;
                Ok((self.ns_name(r.namespace)?.to_string(), r.name.clone()))
            }
            Some(UserTypeRef::RowTypeArray(rel)) => {
                let r = self
                    .snap
                    .relation(rel)
                    .ok_or_else(|| PgError::internal(format!("cache lookup failed for type {oid}")))?;
                let name = r.array_type_name.clone().unwrap_or_else(|| naming::array_type_name(&r.name));
                Ok((self.ns_name(r.namespace)?.to_string(), name))
            }
            None => Err(PgError::internal(format!("cache lookup failed for type {oid}"))),
        }
    }

    /// PG `t.typelem <> 0 AND t.typlen = -1` on the declared type (domains over arrays included).
    fn is_array_type(&self, oid: Oid) -> Result<bool> {
        if let Some(ti) = registry::lookup_type(oid).map(TypeInfo::from) {
            return Ok(ti.elem != 0 && ti.len == -1);
        }
        match self.snap.user_type(oid) {
            Some(UserTypeRef::TypeArray(_)) | Some(UserTypeRef::RowTypeArray(_)) => Ok(true),
            Some(UserTypeRef::RowType(_)) => Ok(false),
            Some(UserTypeRef::Type(_)) => match self.snap.type_obj(oid).map(|t| &t.def) {
                Some(TypeDef::Domain { base, .. }) => self.is_array_type(base.oid),
                _ => Ok(false),
            },
            None => Err(PgError::internal(format!("cache lookup failed for type {oid}"))),
        }
    }

    /// `(schema, name)` of a collation as `attributes` prints it: NULL for the default collation and
    /// for collation 0.
    fn collation_names(&self, oid: Oid) -> Result<Option<(String, String)>> {
        let Some(c) = builtin::builtin_collation(oid) else {
            return Ok(None);
        };
        let schema = self.ns_name(c.namespace)?;
        if schema == "pg_catalog" && c.name == "default" {
            return Ok(None);
        }
        Ok(Some((schema.to_string(), c.name.to_string())))
    }

    /// The builtin collation whose `collate`/`ctype` equal the database's, longest name first then
    /// name (PG `character_sets` ORDER BY char_length DESC, collname ASC LIMIT 1).
    fn db_default_collation(&self) -> Option<&'static BuiltinCollation> {
        let mut found: Vec<&'static BuiltinCollation> = builtin::COLLATIONS
            .iter()
            .filter(|c| c.collate == Some(self.db_collate.as_str()) && c.ctype == Some(self.db_ctype.as_str()))
            .collect();
        found.sort_by(|a, b| b.name.chars().count().cmp(&a.name.chars().count()).then_with(|| a.name.cmp(b.name)));
        found.first().copied()
    }

    /// Builtin collations usable in the current database (`collencoding IN (-1, db encoding)`).
    fn usable_collations(&self) -> impl Iterator<Item = &'static BuiltinCollation> + '_ {
        builtin::COLLATIONS.iter().filter(move |c| c.encoding == -1 || c.encoding == self.db_encoding)
    }

    /// `schemata`.
    fn schemata(&self, s: &'static SystemRel) -> Result<Vec<Row>> {
        let mut rows = Vec::new();
        for ns in self.snap.namespaces() {
            let Some(owner) = self.role_name(ns.owner) else {
                continue;
            };
            let acl = acl::effective_acl(AclObject::Schema, ns.owner, ns.acl.as_ref())?;
            let visible = self.has_role_usage(ns.owner)? || self.any_privilege(ns.owner, &acl, SCHEMA_ANY)?;
            if !visible {
                continue;
            }
            let mut b = RowBuilder::new(s);
            b.set("catalog_name", d::text(&self.db_name))
                .set("schema_name", d::text(&ns.name))
                .set("schema_owner", d::text(owner))
                .set("default_character_set_catalog", Datum::Null)
                .set("default_character_set_schema", Datum::Null)
                .set("default_character_set_name", Datum::Null)
                .set("sql_path", Datum::Null);
            rows.push(b.finish()?);
        }
        Ok(rows)
    }

    /// `tables`: relkinds r, p (BASE TABLE), v (VIEW), f (FOREIGN); never matviews or sequences.
    fn tables(&self, s: &'static SystemRel) -> Result<Vec<Row>> {
        let mut rows = Vec::new();
        for rel in self.all_relations() {
            let (table_type, insertable) = match rel.kind {
                RelKind::Table | RelKind::PartitionedTable => ("BASE TABLE", true),
                RelKind::View => ("VIEW", false),
                RelKind::ForeignTable => ("FOREIGN", false),
                _ => continue,
            };
            if !self.relation_visible(rel)? {
                continue;
            }
            let mut b = RowBuilder::new(s);
            b.set("table_catalog", d::text(&self.db_name))
                .set("table_schema", d::text(self.ns_name(rel.namespace)?))
                .set("table_name", d::text(&rel.name))
                .set("table_type", d::text(table_type))
                .set("self_referencing_column_name", Datum::Null)
                .set("reference_generation", Datum::Null)
                .set("user_defined_type_catalog", Datum::Null)
                .set("user_defined_type_schema", Datum::Null)
                .set("user_defined_type_name", Datum::Null)
                .set("is_insertable_into", yes_no(insertable))
                .set("is_typed", yes_no(false))
                .set("commit_action", Datum::Null);
            rows.push(b.finish()?);
        }
        Ok(rows)
    }

    /// `views`: relkind v. `view_definition` is shown only to the owner's members (PG
    /// `pg_has_role(relowner, 'USAGE')`).
    fn views(&self, s: &'static SystemRel) -> Result<Vec<Row>> {
        let mut rows = Vec::new();
        for rel in self.all_relations() {
            if rel.kind != RelKind::View || !self.relation_visible(rel)? {
                continue;
            }
            let definition = if self.has_role_usage(rel.owner)? {
                deparse::viewdef(self.env, rel.oid, false)?
            } else {
                None
            };
            let check_option = match rel.view.as_ref().and_then(|v| v.check_option.as_deref()) {
                Some("cascaded") => "CASCADED",
                Some("local") => "LOCAL",
                _ => "NONE",
            };
            let mut b = RowBuilder::new(s);
            b.set("table_catalog", d::text(&self.db_name))
                .set("table_schema", d::text(self.ns_name(rel.namespace)?))
                .set("table_name", d::text(&rel.name))
                .set("view_definition", d::opt_text(definition.as_deref()))
                .set("check_option", d::text(check_option))
                .set("is_updatable", yes_no(false))
                .set("is_insertable_into", yes_no(false))
                .set("is_trigger_updatable", yes_no(false))
                .set("is_trigger_deletable", yes_no(false))
                .set("is_trigger_insertable_into", yes_no(false));
            rows.push(b.finish()?);
        }
        Ok(rows)
    }

    /// `sequences`: user sequences that are not identity sequences (identity sequences hang off an
    /// internal dependency and are excluded by PG's NOT EXISTS clause).
    fn sequences(&self, s: &'static SystemRel) -> Result<Vec<Row>> {
        let mut rows = Vec::new();
        for seq in self.snap.data().sequences.values() {
            if seq.identity {
                continue;
            }
            let acl = acl::effective_acl(AclObject::Sequence, seq.owner, seq.acl.as_ref())?;
            let visible = self.has_role_usage(seq.owner)? || self.any_privilege(seq.owner, &acl, SEQUENCE_ANY)?;
            if !visible {
                continue;
            }
            // format_type(seqtypid) and _pg_numeric_precision: sequences are int2, int4 or int8.
            let (data_type, precision) = match seq.type_oid {
                oids::INT2 => ("smallint", 16),
                oids::INT4 => ("integer", 32),
                _ => ("bigint", 64),
            };
            let mut b = RowBuilder::new(s);
            b.set("sequence_catalog", d::text(&self.db_name))
                .set("sequence_schema", d::text(self.ns_name(seq.namespace)?))
                .set("sequence_name", d::text(&seq.name))
                .set("data_type", d::text(data_type))
                .set("numeric_precision", d::int4(precision))
                .set("numeric_precision_radix", d::int4(2))
                .set("numeric_scale", d::int4(0))
                .set("start_value", d::text(&seq.start.to_string()))
                .set("minimum_value", d::text(&seq.min.to_string()))
                .set("maximum_value", d::text(&seq.max.to_string()))
                .set("increment", d::text(&seq.increment.to_string()))
                .set("cycle_option", yes_no(seq.cycle));
            rows.push(b.finish()?);
        }
        Ok(rows)
    }

    /// `information_schema_catalog_name`: one row.
    fn catalog_name(&self, s: &'static SystemRel) -> Result<Vec<Row>> {
        let mut b = RowBuilder::new(s);
        b.set("catalog_name", d::text(&self.db_name));
        Ok(vec![b.finish()?])
    }

    /// `character_sets`: one row describing the database encoding.
    fn character_sets(&self, s: &'static SystemRel) -> Result<Vec<Row>> {
        let (collate_schema, collate_name) = match self.db_default_collation() {
            Some(c) => (Some(self.ns_name(c.namespace)?), Some(c.name)),
            None => (None, None),
        };
        let mut b = RowBuilder::new(s);
        b.set("character_set_catalog", Datum::Null)
            .set("character_set_schema", Datum::Null)
            .set("character_set_name", d::text(DB_CHARSET))
            .set("character_repertoire", d::text("UCS"))
            .set("form_of_use", d::text(DB_CHARSET))
            .set("default_collate_catalog", d::text(&self.db_name))
            .set("default_collate_schema", d::opt_text(collate_schema))
            .set("default_collate_name", d::opt_text(collate_name));
        Ok(vec![b.finish()?])
    }

    /// `collations`: every usable builtin collation (no privilege filter in PG).
    fn collations(&self, s: &'static SystemRel) -> Result<Vec<Row>> {
        let mut rows = Vec::new();
        for c in self.usable_collations() {
            let mut b = RowBuilder::new(s);
            b.set("collation_catalog", d::text(&self.db_name))
                .set("collation_schema", d::text(self.ns_name(c.namespace)?))
                .set("collation_name", d::text(c.name))
                .set("pad_attribute", d::text("NO PAD"));
            rows.push(b.finish()?);
        }
        Ok(rows)
    }

    /// `collation_character_set_applicability`: one row per usable builtin collation.
    fn collation_applicability(&self, s: &'static SystemRel) -> Result<Vec<Row>> {
        let mut rows = Vec::new();
        for c in self.usable_collations() {
            let mut b = RowBuilder::new(s);
            b.set("collation_catalog", d::text(&self.db_name))
                .set("collation_schema", d::text(self.ns_name(c.namespace)?))
                .set("collation_name", d::text(c.name))
                .set("character_set_catalog", Datum::Null)
                .set("character_set_schema", Datum::Null)
                .set("character_set_name", d::text(DB_CHARSET));
            rows.push(b.finish()?);
        }
        Ok(rows)
    }

    /// `enabled_roles`: every role the current user has the privileges of (all roles for a
    /// superuser), sorted by OID.
    fn enabled_roles(&self, s: &'static SystemRel) -> Result<Vec<Row>> {
        let mut roles: Vec<(Oid, &str)> =
            self.snap.indexes().roles.values().map(|r| (r.oid, r.name.as_str())).collect();
        roles.sort_by_key(|(oid, _)| *oid);
        let mut rows = Vec::new();
        for (oid, name) in roles {
            if self.graph.has_privs_of_role(self.user, oid)? {
                let mut b = RowBuilder::new(s);
                b.set("role_name", d::text(name));
                rows.push(b.finish()?);
            }
        }
        Ok(rows)
    }

    /// `(member, roleid, admin_option)` edges of `pg_auth_members` plus the implicit
    /// `pg_database_owner` membership of the database owner (PG UNION in `applicable_roles`).
    fn membership_edges(&self) -> Result<BTreeSet<(Oid, Oid, bool)>> {
        let mut edges = BTreeSet::new();
        for m in self.snap.data().memberships.values() {
            edges.insert((m.member, m.role, m.admin_option));
        }
        for row in builtin::AUTH_MEMBERS.rows.iter() {
            let col = |i: usize| row.get(i).copied().flatten();
            let roleid = text_oid(col(1))?;
            let member = text_oid(col(2))?;
            edges.insert((member, roleid, col(4) == Some("t")));
        }
        edges.insert((self.db_owner, PG_DATABASE_OWNER, false));
        Ok(edges)
    }

    /// Shared body of `applicable_roles` (all edges) and `administrable_role_authorizations`
    /// (`admin_only`: edges with the admin option).
    fn applicable_roles(&self, s: &'static SystemRel, admin_only: bool) -> Result<Vec<Row>> {
        let mut rows = Vec::new();
        for (member, roleid, admin) in self.membership_edges()? {
            if admin_only && !admin {
                continue;
            }
            let (Some(grantee), Some(role_name)) = (self.role_name(member), self.role_name(roleid)) else {
                continue;
            };
            if !self.graph.has_privs_of_role(self.user, member)? {
                continue;
            }
            let mut b = RowBuilder::new(s);
            b.set("grantee", d::text(grantee))
                .set("role_name", d::text(role_name))
                .set("is_grantable", yes_no(admin));
            rows.push(b.finish()?);
        }
        Ok(rows)
    }

    /// `user_defined_types`: composite types only (PG `relkind = 'c'`). Only the four columns PG
    /// fills are set; the rest stay NULL.
    fn user_defined_types(&self, s: &'static SystemRel) -> Result<Vec<Row>> {
        let mut rows = Vec::new();
        for rel in self.composites() {
            if !self.type_usable(rel)? {
                continue;
            }
            let mut b = RowBuilder::new(s);
            b.set("user_defined_type_catalog", d::text(&self.db_name))
                .set("user_defined_type_schema", d::text(self.ns_name(rel.namespace)?))
                .set("user_defined_type_name", d::text(&rel.name))
                .set("user_defined_type_category", d::text("STRUCTURED"))
                .set("is_instantiable", yes_no(true));
            rows.push(b.finish()?);
        }
        Ok(rows)
    }

    /// `attributes`: the non-dropped attributes of visible composite types, in attnum order.
    fn attributes(&self, s: &'static SystemRel) -> Result<Vec<Row>> {
        let mut rows = Vec::new();
        for rel in self.composites() {
            if !self.type_usable(rel)? {
                continue;
            }
            let udt_schema = self.ns_name(rel.namespace)?;
            for col in rel.columns.iter().filter(|c| !c.dropped) {
                let oid = col.type_id.oid;
                let info = describe_type_for_info_schema(self.env, oid, col.type_id.typmod)?;
                let (decl_schema, decl_name) = self.declared_type_name(oid)?;
                // `data_type` of attributes looks at the DECLARED type: ARRAY, the format_type() text
                // for pg_catalog types, and USER-DEFINED for everything else.
                let data_type = if self.is_array_type(oid)? {
                    "ARRAY".to_string()
                } else if decl_schema == "pg_catalog" {
                    describe_type_for_info_schema(self.env, oid, -1)?.data_type
                } else {
                    "USER-DEFINED".to_string()
                };
                let domain_not_null = matches!(
                    self.snap.type_obj(oid).map(|t| &t.def),
                    Some(TypeDef::Domain { not_null: true, .. })
                );
                let nullable = !(col.not_null || domain_not_null);
                let collation = self.collation_names(col.collation)?;
                let mut b = RowBuilder::new(s);
                b.set("udt_catalog", d::text(&self.db_name))
                    .set("udt_schema", d::text(udt_schema))
                    .set("udt_name", d::text(&rel.name))
                    .set("attribute_name", d::text(&col.name))
                    .set("ordinal_position", d::int4(i32::from(col.attnum)))
                    .set("attribute_default", Datum::Null)
                    .set("is_nullable", yes_no(nullable))
                    .set("data_type", d::text(&data_type))
                    .set("character_maximum_length", d::opt_int4(info.character_maximum_length))
                    .set("character_octet_length", d::opt_int4(info.character_octet_length))
                    .set("character_set_catalog", Datum::Null)
                    .set("character_set_schema", Datum::Null)
                    .set("character_set_name", Datum::Null)
                    .set("collation_catalog", match &collation {
                        Some(_) => d::text(&self.db_name),
                        None => Datum::Null,
                    })
                    .set("collation_schema", d::opt_text(collation.as_ref().map(|c| c.0.as_str())))
                    .set("collation_name", d::opt_text(collation.as_ref().map(|c| c.1.as_str())))
                    .set("numeric_precision", d::opt_int4(info.numeric_precision))
                    .set("numeric_precision_radix", d::opt_int4(info.numeric_precision_radix))
                    .set("numeric_scale", d::opt_int4(info.numeric_scale))
                    .set("datetime_precision", d::opt_int4(info.datetime_precision))
                    .set("interval_type", d::opt_text(info.interval_type.as_deref()))
                    .set("interval_precision", Datum::Null)
                    .set("attribute_udt_catalog", d::text(&self.db_name))
                    .set("attribute_udt_schema", d::text(&decl_schema))
                    .set("attribute_udt_name", d::text(&decl_name))
                    .set("scope_catalog", Datum::Null)
                    .set("scope_schema", Datum::Null)
                    .set("scope_name", Datum::Null)
                    .set("maximum_cardinality", Datum::Null)
                    .set("dtd_identifier", d::text(&col.attnum.to_string()))
                    .set("is_derived_reference_attribute", yes_no(false));
                rows.push(b.finish()?);
            }
        }
        Ok(rows)
    }
}

/// Producer for `information_schema.schemata`.
#[derive(Clone, Copy, Debug)]
pub struct IsSchemata;

impl VirtualRelation for IsSchemata {
    fn oid(&self) -> Oid {
        relid::IS_SCHEMATA
    }
    fn scan(&self, ctx: &ScanContext, _req: &ScanRequest) -> Result<Vec<Row>> {
        let schema = self.schema()?;
        Session::new(ctx.env)?.schemata(schema)
    }
}

/// Producer for `information_schema.tables`.
#[derive(Clone, Copy, Debug)]
pub struct IsTables;

impl VirtualRelation for IsTables {
    fn oid(&self) -> Oid {
        relid::IS_TABLES
    }
    fn scan(&self, ctx: &ScanContext, _req: &ScanRequest) -> Result<Vec<Row>> {
        let schema = self.schema()?;
        Session::new(ctx.env)?.tables(schema)
    }
}

/// Producer for `information_schema.views`.
#[derive(Clone, Copy, Debug)]
pub struct IsViews;

impl VirtualRelation for IsViews {
    fn oid(&self) -> Oid {
        relid::IS_VIEWS
    }
    fn scan(&self, ctx: &ScanContext, _req: &ScanRequest) -> Result<Vec<Row>> {
        let schema = self.schema()?;
        Session::new(ctx.env)?.views(schema)
    }
}

/// Producer for `information_schema.sequences`.
#[derive(Clone, Copy, Debug)]
pub struct IsSequences;

impl VirtualRelation for IsSequences {
    fn oid(&self) -> Oid {
        relid::IS_SEQUENCES
    }
    fn scan(&self, ctx: &ScanContext, _req: &ScanRequest) -> Result<Vec<Row>> {
        let schema = self.schema()?;
        Session::new(ctx.env)?.sequences(schema)
    }
}

/// Producer for `information_schema.information_schema_catalog_name`.
#[derive(Clone, Copy, Debug)]
pub struct IsCatalogName;

impl VirtualRelation for IsCatalogName {
    fn oid(&self) -> Oid {
        relid::IS_INFORMATION_SCHEMA_CATALOG_NAME
    }
    fn scan(&self, ctx: &ScanContext, _req: &ScanRequest) -> Result<Vec<Row>> {
        let schema = self.schema()?;
        Session::new(ctx.env)?.catalog_name(schema)
    }
}

/// Producer for `information_schema.character_sets`.
#[derive(Clone, Copy, Debug)]
pub struct IsCharacterSets;

impl VirtualRelation for IsCharacterSets {
    fn oid(&self) -> Oid {
        relid::IS_CHARACTER_SETS
    }
    fn scan(&self, ctx: &ScanContext, _req: &ScanRequest) -> Result<Vec<Row>> {
        let schema = self.schema()?;
        Session::new(ctx.env)?.character_sets(schema)
    }
}

/// Producer for `information_schema.collations`.
#[derive(Clone, Copy, Debug)]
pub struct IsCollations;

impl VirtualRelation for IsCollations {
    fn oid(&self) -> Oid {
        relid::IS_COLLATIONS
    }
    fn scan(&self, ctx: &ScanContext, _req: &ScanRequest) -> Result<Vec<Row>> {
        let schema = self.schema()?;
        Session::new(ctx.env)?.collations(schema)
    }
}

/// Producer for `information_schema.collation_character_set_applicability`.
#[derive(Clone, Copy, Debug)]
pub struct IsCollationCharsetApplicability;

impl VirtualRelation for IsCollationCharsetApplicability {
    fn oid(&self) -> Oid {
        relid::IS_COLLATION_CHARACTER_SET_APPLICABILITY
    }
    fn scan(&self, ctx: &ScanContext, _req: &ScanRequest) -> Result<Vec<Row>> {
        let schema = self.schema()?;
        Session::new(ctx.env)?.collation_applicability(schema)
    }
}

/// Producer for `information_schema.enabled_roles`.
#[derive(Clone, Copy, Debug)]
pub struct IsEnabledRoles;

impl VirtualRelation for IsEnabledRoles {
    fn oid(&self) -> Oid {
        relid::IS_ENABLED_ROLES
    }
    fn scan(&self, ctx: &ScanContext, _req: &ScanRequest) -> Result<Vec<Row>> {
        let schema = self.schema()?;
        Session::new(ctx.env)?.enabled_roles(schema)
    }
}

/// Producer for `information_schema.applicable_roles`.
#[derive(Clone, Copy, Debug)]
pub struct IsApplicableRoles;

impl VirtualRelation for IsApplicableRoles {
    fn oid(&self) -> Oid {
        relid::IS_APPLICABLE_ROLES
    }
    fn scan(&self, ctx: &ScanContext, _req: &ScanRequest) -> Result<Vec<Row>> {
        let schema = self.schema()?;
        Session::new(ctx.env)?.applicable_roles(schema, false)
    }
}

/// Producer for `information_schema.administrable_role_authorizations`.
#[derive(Clone, Copy, Debug)]
pub struct IsAdministrableRoleAuthorizations;

impl VirtualRelation for IsAdministrableRoleAuthorizations {
    fn oid(&self) -> Oid {
        relid::IS_ADMINISTRABLE_ROLE_AUTHORIZATIONS
    }
    fn scan(&self, ctx: &ScanContext, _req: &ScanRequest) -> Result<Vec<Row>> {
        let schema = self.schema()?;
        Session::new(ctx.env)?.applicable_roles(schema, true)
    }
}

/// Producer for `information_schema.user_defined_types`.
#[derive(Clone, Copy, Debug)]
pub struct IsUserDefinedTypes;

impl VirtualRelation for IsUserDefinedTypes {
    fn oid(&self) -> Oid {
        relid::IS_USER_DEFINED_TYPES
    }
    fn scan(&self, ctx: &ScanContext, _req: &ScanRequest) -> Result<Vec<Row>> {
        let schema = self.schema()?;
        Session::new(ctx.env)?.user_defined_types(schema)
    }
}

/// Producer for `information_schema.attributes`.
#[derive(Clone, Copy, Debug)]
pub struct IsAttributes;

impl VirtualRelation for IsAttributes {
    fn oid(&self) -> Oid {
        relid::IS_ATTRIBUTES
    }
    fn scan(&self, ctx: &ScanContext, _req: &ScanRequest) -> Result<Vec<Row>> {
        let schema = self.schema()?;
        Session::new(ctx.env)?.attributes(schema)
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session::NullSessionInfo;
    use crate::testing;
    use pgarbor_types::IoConfig;

    fn scan_rows(env: &CatalogEnv, oid: Oid) -> Vec<Row> {
        let io = IoConfig::default();
        let session = NullSessionInfo;
        let ctx = ScanContext { env, session: &session, io_config: &io };
        crate::vrel::scan(&ctx, oid, &ScanRequest::all()).unwrap()
    }

    fn col_idx(oid: Oid, name: &str) -> usize {
        builtin::system_rel(oid).unwrap().columns.iter().position(|c| c.name == name).unwrap()
    }

    fn text_at(row: &Row, idx: usize) -> Option<String> {
        match &row[idx] {
            Datum::Text(s) => Some(s.clone()),
            _ => None,
        }
    }

    #[test]
    fn schemata_lists_builtin_schemas_in_oid_order() {
        let (_kv, env) = testing::bootstrapped().unwrap();
        let rows = scan_rows(&env, relid::IS_SCHEMATA);
        let name_idx = col_idx(relid::IS_SCHEMATA, "schema_name");
        let owner_idx = col_idx(relid::IS_SCHEMATA, "schema_owner");
        let names: Vec<String> = rows.iter().filter_map(|r| text_at(r, name_idx)).collect();
        assert_eq!(names, vec!["pg_catalog", "pg_toast", "public", "information_schema"]);
        let public = rows.iter().find(|r| text_at(r, name_idx).as_deref() == Some("public")).unwrap();
        assert_eq!(text_at(public, owner_idx).as_deref(), Some("pg_database_owner"));
    }

    #[test]
    fn tables_show_system_tables_as_base_tables_and_views_as_views() {
        let (_kv, env) = testing::bootstrapped().unwrap();
        let rows = scan_rows(&env, relid::IS_TABLES);
        let schema_idx = col_idx(relid::IS_TABLES, "table_schema");
        let name_idx = col_idx(relid::IS_TABLES, "table_name");
        let type_idx = col_idx(relid::IS_TABLES, "table_type");
        let ins_idx = col_idx(relid::IS_TABLES, "is_insertable_into");
        let class = rows
            .iter()
            .find(|r| text_at(r, schema_idx).as_deref() == Some("pg_catalog") && text_at(r, name_idx).as_deref() == Some("pg_class"))
            .unwrap();
        assert_eq!(text_at(class, type_idx).as_deref(), Some("BASE TABLE"));
        assert_eq!(text_at(class, ins_idx).as_deref(), Some("YES"));
        let view = rows.iter().find(|r| text_at(r, name_idx).as_deref() == Some("pg_tables")).unwrap();
        assert_eq!(text_at(view, type_idx).as_deref(), Some("VIEW"));
        assert_eq!(text_at(view, ins_idx).as_deref(), Some("NO"));
    }

    #[test]
    fn views_report_check_option_none() {
        let (_kv, env) = testing::bootstrapped().unwrap();
        let rows = scan_rows(&env, relid::IS_VIEWS);
        let name_idx = col_idx(relid::IS_VIEWS, "table_name");
        let opt_idx = col_idx(relid::IS_VIEWS, "check_option");
        let row = rows.iter().find(|r| text_at(r, name_idx).as_deref() == Some("pg_tables")).unwrap();
        assert_eq!(text_at(row, opt_idx).as_deref(), Some("NONE"));
    }

    #[test]
    fn character_sets_has_one_utf8_row_for_the_database() {
        let (_kv, env) = testing::bootstrapped().unwrap();
        let rows = scan_rows(&env, relid::IS_CHARACTER_SETS);
        assert_eq!(rows.len(), 1);
        let row = &rows[0];
        assert_eq!(text_at(row, col_idx(relid::IS_CHARACTER_SETS, "character_set_name")).as_deref(), Some("UTF8"));
        assert_eq!(text_at(row, col_idx(relid::IS_CHARACTER_SETS, "character_repertoire")).as_deref(), Some("UCS"));
        assert_eq!(text_at(row, col_idx(relid::IS_CHARACTER_SETS, "form_of_use")).as_deref(), Some("UTF8"));
        assert_eq!(
            text_at(row, col_idx(relid::IS_CHARACTER_SETS, "default_collate_catalog")).as_deref(),
            Some("postgres")
        );
    }

    #[test]
    fn catalog_name_is_the_current_database() {
        let (_kv, env) = testing::bootstrapped().unwrap();
        let rows = scan_rows(&env, relid::IS_INFORMATION_SCHEMA_CATALOG_NAME);
        assert_eq!(rows.len(), 1);
        assert_eq!(text_at(&rows[0], 0).as_deref(), Some("postgres"));
    }

    #[test]
    fn enabled_roles_lists_every_role_for_a_superuser() {
        let (_kv, env) = testing::bootstrapped().unwrap();
        let rows = scan_rows(&env, relid::IS_ENABLED_ROLES);
        assert_eq!(rows.len(), env.snapshot().indexes().roles.len());
        let names: Vec<String> = rows.iter().filter_map(|r| text_at(r, 0)).collect();
        assert!(names.iter().any(|n| n == "postgres"));
    }

    #[test]
    fn applicable_roles_include_database_owner_membership() {
        let (_kv, env) = testing::bootstrapped().unwrap();
        let rows = scan_rows(&env, relid::IS_APPLICABLE_ROLES);
        let grantee_idx = col_idx(relid::IS_APPLICABLE_ROLES, "grantee");
        let role_idx = col_idx(relid::IS_APPLICABLE_ROLES, "role_name");
        let grant_idx = col_idx(relid::IS_APPLICABLE_ROLES, "is_grantable");
        let row = rows
            .iter()
            .find(|r| {
                text_at(r, grantee_idx).as_deref() == Some("postgres")
                    && text_at(r, role_idx).as_deref() == Some("pg_database_owner")
            })
            .unwrap();
        assert_eq!(text_at(row, grant_idx).as_deref(), Some("NO"));
        let admin = scan_rows(&env, relid::IS_ADMINISTRABLE_ROLE_AUTHORIZATIONS);
        assert!(admin.iter().all(|r| text_at(r, grant_idx).as_deref() == Some("YES")));
    }

    #[test]
    fn collations_are_pg_catalog_no_pad() {
        let (_kv, env) = testing::bootstrapped().unwrap();
        let rows = scan_rows(&env, relid::IS_COLLATIONS);
        assert!(!rows.is_empty());
        let schema_idx = col_idx(relid::IS_COLLATIONS, "collation_schema");
        let pad_idx = col_idx(relid::IS_COLLATIONS, "pad_attribute");
        for r in &rows {
            assert_eq!(text_at(r, schema_idx).as_deref(), Some("pg_catalog"));
            assert_eq!(text_at(r, pad_idx).as_deref(), Some("NO PAD"));
        }
    }

    #[test]
    fn sequences_and_composite_views_are_empty_on_a_fresh_branch() {
        let (_kv, env) = testing::bootstrapped().unwrap();
        assert!(scan_rows(&env, relid::IS_SEQUENCES).is_empty());
        assert!(scan_rows(&env, relid::IS_USER_DEFINED_TYPES).is_empty());
        assert!(scan_rows(&env, relid::IS_ATTRIBUTES).is_empty());
    }
}
