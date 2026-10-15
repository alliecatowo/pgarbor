//! `pg_tables`, `pg_views`, `pg_matviews`, `pg_indexes`, `pg_sequences`: convenience views over
//! the model (PG defines them in `catalog/system_views.sql` as joins over pg_class). Lease CAT-36.
//! Ordered by (schemaname, name) like the usual `ORDER BY` clients add; the executor sorts anyway.
//! `*owner` columns are ROLE NAMES (`snapshot.role_name`, unknown -> `unknown (OID=n)`).
//!
//! - **pg_tables**: relkind r and p (builtin system tables too): `schemaname, tablename, tableowner,
//!   tablespace` NULL, `hasindexes` (`indexes_of` non-empty), `hasrules` false, `hastriggers` false,
//!   `rowsecurity`.
//! - **pg_views**: relkind v (user and system views): `schemaname, viewname, viewowner, definition`
//!   = `deparse::viewdef` text (for system views the stored `SYSTEM_VIEW_DEFS`).
//! - **pg_matviews**: relkind m: `schemaname, matviewname, matviewowner, tablespace` NULL,
//!   `hasindexes, ispopulated, definition`.
//! - **pg_indexes**: user indexes: `schemaname, tablename, indexname, tablespace` NULL, `indexdef` =
//!   `deparse::indexdef(env, index, None, false)`. System indexes are omitted (gap).
//! - **pg_sequences**: `schemaname, sequencename, sequenceowner, data_type` (`regtype` = `Oid(type_oid)`),
//!   `start_value, min_value, max_value, increment_by` (`Int8`), `cycle, cache_size`, `last_value` =
//!   `session.sequence_state(oid)?.last_value` (NULL if never called or no state).
use super::{Row, ScanContext, ScanRequest, VirtualRelation};
use crate::builtin::relid;
use pgarbor_common::{Oid, PgError, Result};

/// Producer for `pg_tables`.
#[derive(Clone, Copy, Debug)]
pub struct PgTables;

impl VirtualRelation for PgTables {
    fn oid(&self) -> Oid {
        relid::PG_TABLES
    }
    #[allow(unused_variables)]
    fn scan(&self, ctx: &ScanContext, req: &ScanRequest) -> Result<Vec<Row>> {
        Err(PgError::unimplemented("CAT-36", "pg_tables rows"))
    }
}

/// Producer for `pg_views`.
#[derive(Clone, Copy, Debug)]
pub struct PgViews;

impl VirtualRelation for PgViews {
    fn oid(&self) -> Oid {
        relid::PG_VIEWS
    }
    #[allow(unused_variables)]
    fn scan(&self, ctx: &ScanContext, req: &ScanRequest) -> Result<Vec<Row>> {
        Err(PgError::unimplemented("CAT-36", "pg_views rows"))
    }
}

/// Producer for `pg_matviews`.
#[derive(Clone, Copy, Debug)]
pub struct PgMatviews;

impl VirtualRelation for PgMatviews {
    fn oid(&self) -> Oid {
        relid::PG_MATVIEWS
    }
    #[allow(unused_variables)]
    fn scan(&self, ctx: &ScanContext, req: &ScanRequest) -> Result<Vec<Row>> {
        Err(PgError::unimplemented("CAT-36", "pg_matviews rows"))
    }
}

/// Producer for `pg_indexes`.
#[derive(Clone, Copy, Debug)]
pub struct PgIndexes;

impl VirtualRelation for PgIndexes {
    fn oid(&self) -> Oid {
        relid::PG_INDEXES
    }
    #[allow(unused_variables)]
    fn scan(&self, ctx: &ScanContext, req: &ScanRequest) -> Result<Vec<Row>> {
        Err(PgError::unimplemented("CAT-36", "pg_indexes rows"))
    }
}

/// Producer for `pg_sequences`.
#[derive(Clone, Copy, Debug)]
pub struct PgSequences;

impl VirtualRelation for PgSequences {
    fn oid(&self) -> Oid {
        relid::PG_SEQUENCES
    }
    #[allow(unused_variables)]
    fn scan(&self, ctx: &ScanContext, req: &ScanRequest) -> Result<Vec<Row>> {
        Err(PgError::unimplemented("CAT-36", "pg_sequences rows"))
    }
}


/// Registry slice.
pub static RELATIONS: &[&dyn VirtualRelation] = &[&PgTables, &PgViews, &PgMatviews, &PgIndexes, &PgSequences];
