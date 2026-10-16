//! VIRTUAL system relations: `pg_catalog` and `information_schema` tables/views whose rows are
//! generated from the [`CatalogSnapshot`](crate::snapshot::CatalogSnapshot), the compiled-in PG 18.6
//! builtin tables and the session ([`SessionInfo`]) at scan time. Nothing is stored; the binder
//! resolves `pg_class`, `information_schema.columns`, ... exactly like user relations
//! (`builtin::builtin_relation`, `Relation::is_virtual()`), and the executor, upon seeing a virtual
//! relation OID, calls [`scan`] instead of reading `[relid][rowid]` keys. The stored view
//! definitions of system views (`Relation::view`, `builtin::SYSTEM_VIEW_DEFS`) are NEVER expanded:
//! they exist for `pg_get_viewdef` only.
//!
//! ## Contract
//! - [`VirtualRelation::schema`]: the exact column list (names, types, typmods, order) of the PG 18.6
//!   relation, from `builtin::SYSTEM_RELS` (generated from `contracts/reference/pg18/catalog_attributes.csv`).
//! - [`scan`]: returns ALL rows (`Vec<Row>`, each exactly `schema().columns.len()` Datums in column
//!   order, values shaped per the table below). Row order is the order PG's heap scan would typically
//!   give for a fresh database: ascending OID where the relation has an `oid` column, else a stable
//!   documented order. The [`ScanRequest`] carries optional equality filters that a producer MAY use
//!   to avoid building every row; the executor re-applies the full WHERE clause, so ignoring the
//!   request is always correct. Producers must be deterministic for a fixed snapshot.
//! - Visibility: v1 shows every row to every role (no row filtering by privileges, the same as PG for
//!   catalogs; `pg_stat_activity.query` redaction for other roles is a documented gap).
//! - Correctly empty relations: [`empty::EMPTY_RELATIONS`]; relations PGArbor cannot serve yet fail
//!   with 0A000 via `unsupported` (listed in `UNSUPPORTED_RELATIONS`); everything else has a producer
//!   file below. `gen/classify_vrel.py` checks that all 213 relations are classified exactly once.
//!
//! ## Datum shapes of column types (frozen by `pgarbor_types::datum`)
//! | column type | Datum |
//! |---|---|
//! | `name`, `text`, `varchar`, `pg_node_tree`, `aclitem` | `Text` |
//! | information_schema domains: `sql_identifier`, `character_data`, `yes_or_no` (all over text/varchar/name) | `Text` |
//! | `cardinal_number` (domain over int4) | `Int4` |
//! | `time_stamp` (domain over timestamptz) | `TimestampTz` |
//! | `oid`, `regproc`, `regclass`, `regtype`, `xid`, `cid` | `Oid(u32)` |
//! | `"char"` | `Char(u8)` (0 for none) |
//! | `bool` / `int2` / `int4` / `int8` / `float4` | `Bool` / `Int2` / `Int4` / `Int8` / `Float4` |
//! | `timestamptz` | `TimestampTz(µs since 2000-01-01)` |
//! | `interval` | `Interval` |
//! | arrays (`text[]`, `oid[]`, `smallint[]`, `"char"[]`, `aclitem[]`, `name[]`) | `Array{elem: <elem oid>, dims: [(n, 1)], items}` (empty array: no dims) |
//! | `int2vector`, `oidvector` | `Array` with lower bound 0: `dims: [(n, 0)]`; empty vector: no dims |
//! | `anyarray` (`attmissingval`) | `Array` of the column's type, or NULL |
//! | `inet` (`pg_stat_activity.client_addr`), `pg_lsn`, `xid8` | not representable (amendment pgarbor-types-03): emit `Null` (inet) / `Int8` (lsn, xid8) |
//! Use the helpers in [`rowbuild`] instead of building these by hand.
//!
//! ## Where each relation lives
//! | file | relations |
//! |---|---|
//! | [`pg_class`] | pg_namespace, pg_class, pg_inherits, pg_sequence, pg_rewrite |
//! | [`pg_attribute`] | pg_attribute, pg_attrdef |
//! | [`pg_type`] | pg_type, pg_enum, pg_range |
//! | [`pg_constraint`] | pg_index, pg_constraint |
//! | [`pg_depend`] | pg_depend, pg_description, pg_shdescription, pg_shdepend |
//! | [`pg_proc`] | pg_proc, pg_aggregate, pg_operator, pg_cast, pg_language |
//! | [`pg_roles`] | pg_authid, pg_auth_members, pg_database, pg_db_role_setting, pg_tablespace, pg_roles, pg_shadow, pg_group, pg_user |
//! | [`pg_ext`] | pg_amop, pg_amproc, pg_conversion (+ user rows of the extensibility catalogs) |
//! | [`pg_builtin`] | pg_collation, pg_am, pg_opclass, pg_opfamily, pg_extension, pg_available_extensions, pg_available_extension_versions |
//! | [`pg_views`] | pg_tables, pg_views, pg_matviews, pg_indexes, pg_sequences |
//! | [`runtime`] | pg_settings, pg_stat_activity, pg_locks, pg_prepared_statements, pg_cursors, pg_stat_ssl, pg_stat_gssapi |
//! | [`stats`] | pg_stat_*/pg_statio_* per-relation views (zero counters) |
//! | [`is_core`], [`is_columns`], [`is_constraints`], [`is_routines`], [`is_privileges`] | information_schema |
//! | [`empty`], [`unsupported`] | the rest |
pub mod empty;
pub mod is_columns;
pub mod is_constraints;
pub mod is_core;
pub mod is_privileges;
pub mod is_routines;
pub mod pg_attribute;
pub mod pg_builtin;
pub mod pg_class;
pub mod pg_constraint;
pub mod pg_depend;
pub mod pg_ext;
pub mod pg_policy;
pub mod pg_proc;
pub mod pg_roles;
pub mod pg_type;
pub mod pg_views;
pub mod rowbuild;
pub mod runtime;
pub mod stats;
pub mod unsupported;

use crate::builtin::{self, SystemRel};
use crate::env::CatalogEnv;
use crate::session::SessionInfo;
use pgarbor_common::{Oid, PgError, Result};
use pgarbor_types::context::IoCtx;
use pgarbor_types::{Datum, IoConfig};
use std::collections::HashMap;
use std::sync::OnceLock;

/// One result row, one `Datum` per column in `schema().columns` order.
pub type Row = Vec<Datum>;

/// Everything a producer may read.
// CONTRACT
pub struct ScanContext<'a> {
    /// Snapshot + search_path + current user.
    pub env: &'a CatalogEnv,
    /// Session/engine services (settings, activity, sequence state, ...).
    pub session: &'a dyn SessionInfo,
    /// Output configuration (DateStyle ...) for text -> Datum conversions of builtin text rows.
    pub io_config: &'a IoConfig,
}

impl<'a> ScanContext<'a> {
    /// `IoCtx` over the env's type environment.
    pub fn io_ctx(&self) -> IoCtx<'_> {
        IoCtx::new(self.env, self.io_config)
    }
}

/// Optional equality filters (`column index` = position in `schema().columns`, 0-based).
/// Producers may use them (see each producer's docs for the supported fast paths).
// CONTRACT
#[derive(Clone, Debug, Default)]
pub struct ScanRequest {
    pub eq: Vec<(usize, Datum)>,
}

impl ScanRequest {
    pub fn all() -> ScanRequest {
        ScanRequest::default()
    }
    /// The filter value on column `idx`, if any.
    pub fn eq_on(&self, idx: usize) -> Option<&Datum> {
        self.eq.iter().find(|(i, _)| *i == idx).map(|(_, d)| d)
    }
    /// The filter on column `idx` as an OID.
    pub fn oid_eq_on(&self, idx: usize) -> Option<Oid> {
        match self.eq_on(idx) {
            Some(Datum::Oid(o)) => Some(*o),
            _ => None,
        }
    }
}

/// A scannable system relation.
// CONTRACT
pub trait VirtualRelation: Sync + Send {
    /// The relation's `pg_class` OID (key into `builtin::SYSTEM_RELS`).
    fn oid(&self) -> Oid;

    /// Column list. Default: looked up in `builtin::SYSTEM_RELS`.
    fn schema(&self) -> Result<&'static SystemRel> {
        builtin::system_rel(self.oid())
            .ok_or_else(|| PgError::internal(format!("virtual relation {} has no system relation entry", self.oid())))
    }

    /// Produce the rows (module docs).
    fn scan(&self, ctx: &ScanContext, req: &ScanRequest) -> Result<Vec<Row>>;
}

fn registry() -> &'static HashMap<Oid, &'static dyn VirtualRelation> {
    static REG: OnceLock<HashMap<Oid, &'static dyn VirtualRelation>> = OnceLock::new();
    REG.get_or_init(|| {
        let mut m: HashMap<Oid, &'static dyn VirtualRelation> = HashMap::new();
        for r in empty::EMPTY_RELATIONS {
            m.insert(r.0, r as &'static dyn VirtualRelation);
        }
        for r in unsupported::UNSUPPORTED_RELATIONS {
            m.insert(r.0, r as &'static dyn VirtualRelation);
        }
        let groups: [&'static [&'static dyn VirtualRelation]; 18] = [
            pg_ext::RELATIONS,
            pg_policy::RELATIONS,
            pg_class::RELATIONS,
            pg_attribute::RELATIONS,
            pg_type::RELATIONS,
            pg_constraint::RELATIONS,
            pg_depend::RELATIONS,
            pg_proc::RELATIONS,
            pg_roles::RELATIONS,
            pg_builtin::RELATIONS,
            pg_views::RELATIONS,
            runtime::RELATIONS,
            stats::RELATIONS,
            is_core::RELATIONS,
            is_columns::RELATIONS,
            is_constraints::RELATIONS,
            is_routines::RELATIONS,
            is_privileges::RELATIONS,
        ];
        for g in groups {
            for r in g {
                m.insert(r.oid(), *r);
            }
        }
        m
    })
}

/// The producer for a system relation OID (None for ordinary relations and for system INDEXES).
pub fn lookup(oid: Oid) -> Option<&'static dyn VirtualRelation> {
    registry().get(&oid).copied()
}

/// Every registered virtual relation (unordered).
pub fn all() -> impl Iterator<Item = &'static dyn VirtualRelation> {
    registry().values().copied()
}

/// Scan a system relation by OID and verify the row shape (every row has exactly the schema's
/// column count; a mismatch is XX000 `virtual relation <name> produced a row with N columns,
/// expected M`). Unknown OID -> XX000 `no virtual relation with OID n`.
pub fn scan(ctx: &ScanContext, oid: Oid, req: &ScanRequest) -> Result<Vec<Row>> {
    let rel = lookup(oid).ok_or_else(|| PgError::internal(format!("no virtual relation with OID {oid}")))?;
    let schema = rel.schema()?;
    let rows = rel.scan(ctx, req)?;
    for r in &rows {
        if r.len() != schema.columns.len() {
            return Err(PgError::internal(format!(
                "virtual relation {} produced a row with {} columns, expected {}",
                schema.name,
                r.len(),
                schema.columns.len()
            )));
        }
    }
    Ok(rows)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_system_table_and_view_has_a_producer() {
        for r in builtin::SYSTEM_RELS.iter().filter(|r| r.kind == b'r' || r.kind == b'v') {
            let p = lookup(r.oid);
            assert!(p.is_some(), "no producer for {}.{} ({})", r.namespace, r.name, r.oid);
            assert_eq!(p.unwrap().oid(), r.oid);
        }
        // indexes are not scannable
        assert!(builtin::SYSTEM_RELS.iter().filter(|r| r.kind == b'i').all(|r| lookup(r.oid).is_none()));
    }

    #[test]
    fn producers_have_unique_oids() {
        let mut seen = std::collections::HashSet::new();
        for g in [
            pg_ext::RELATIONS,
            pg_policy::RELATIONS,
            pg_class::RELATIONS,
            pg_attribute::RELATIONS,
            pg_type::RELATIONS,
            pg_constraint::RELATIONS,
            pg_depend::RELATIONS,
            pg_proc::RELATIONS,
            pg_roles::RELATIONS,
            pg_builtin::RELATIONS,
            pg_views::RELATIONS,
            runtime::RELATIONS,
            stats::RELATIONS,
            is_core::RELATIONS,
            is_columns::RELATIONS,
            is_constraints::RELATIONS,
            is_routines::RELATIONS,
            is_privileges::RELATIONS,
        ] {
            for r in g {
                assert!(seen.insert(r.oid()), "duplicate producer for {}", r.oid());
            }
        }
        for r in empty::EMPTY_RELATIONS {
            assert!(seen.insert(r.0), "duplicate producer for {}", r.0);
        }
        for r in unsupported::UNSUPPORTED_RELATIONS {
            assert!(seen.insert(r.0), "duplicate producer for {}", r.0);
        }
        assert_eq!(seen.len(), 213);
    }
}
