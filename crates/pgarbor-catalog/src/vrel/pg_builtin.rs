//! Catalogs that list COMPILED-IN objects: `pg_collation`, `pg_am`, `pg_opclass`, `pg_opfamily`,
//! `pg_extension`, `pg_available_extensions`, `pg_available_extension_versions`. Lease CAT-35.
//! Data: `builtin::COLLATIONS`, `ACCESS_METHODS`, `OPCLASSES` (TextRows), `OPFAMILIES` (TextRows),
//! `EXTENSIONS`. All ordered by `oid`. `TextRows` cells are PG text-output strings: convert each with
//! `pgarbor_types::io::input(&ctx.io_ctx(), TypeId::new(col.type_oid, col.typmod), text)` using the
//! column types of `builtin::system_rel(rows.rel)`; `None` cell -> `Datum::Null`.
//!
//! - **pg_collation**: `BuiltinCollation` -> `oid, collname, collnamespace, collowner, collprovider`
//!   (`Char`), `collisdeterministic, collencoding` (-1 = any), `collcollate, collctype, colllocale,
//!   collicurules, collversion` (NULL when `None`). 800+ rows in PG; fast path eq `oid`/`collname`.
//! - **pg_am**: `BuiltinAm` -> `oid, amname, amhandler` = `pg_type::regproc_oid(handler)`, `amtype`
//!   (`Char`). PGArbor's `heap` (2) and `btree` (403) are real; hash/gist/gin/brin/spgist rows exist
//!   because PG lists them, although CREATE INDEX rejects them (0A000).
//! - **pg_opclass** / **pg_opfamily**: `OPCLASSES` / `OPFAMILIES` converted as above.
//! - **pg_extension**: `EXTENSIONS` (plpgsql 1.0): `oid, extname, extowner, extnamespace,
//!   extrelocatable, extversion, extconfig` NULL, `extcondition` NULL.
//! - **pg_available_extensions**: one row `plpgsql | 1.0 | 1.0 | PL/pgSQL procedural language`
//!   (name, default_version, installed_version, comment). **pg_available_extension_versions**: one row
//!   `plpgsql, 1.0, installed true, superuser true, trusted true, relocatable false, schema pg_catalog,
//!   requires NULL, comment 'PL/pgSQL procedural language'`.
//!   (Other contrib extensions are not available; `CREATE EXTENSION` is the binder's 0A000.)
use super::{Row, ScanContext, ScanRequest, VirtualRelation};
use crate::builtin::relid;
use pgarbor_common::{Oid, PgError, Result};

/// Producer for `pg_collation`.
#[derive(Clone, Copy, Debug)]
pub struct PgCollation;

impl VirtualRelation for PgCollation {
    fn oid(&self) -> Oid {
        relid::PG_COLLATION
    }
    #[allow(unused_variables)]
    fn scan(&self, ctx: &ScanContext, req: &ScanRequest) -> Result<Vec<Row>> {
        Err(PgError::unimplemented("CAT-35", "pg_collation rows"))
    }
}

/// Producer for `pg_am`.
#[derive(Clone, Copy, Debug)]
pub struct PgAm;

impl VirtualRelation for PgAm {
    fn oid(&self) -> Oid {
        relid::PG_AM
    }
    #[allow(unused_variables)]
    fn scan(&self, ctx: &ScanContext, req: &ScanRequest) -> Result<Vec<Row>> {
        Err(PgError::unimplemented("CAT-35", "pg_am rows"))
    }
}

/// Producer for `pg_opclass`.
#[derive(Clone, Copy, Debug)]
pub struct PgOpclass;

impl VirtualRelation for PgOpclass {
    fn oid(&self) -> Oid {
        relid::PG_OPCLASS
    }
    #[allow(unused_variables)]
    fn scan(&self, ctx: &ScanContext, req: &ScanRequest) -> Result<Vec<Row>> {
        Err(PgError::unimplemented("CAT-35", "pg_opclass rows"))
    }
}

/// Producer for `pg_opfamily`.
#[derive(Clone, Copy, Debug)]
pub struct PgOpfamily;

impl VirtualRelation for PgOpfamily {
    fn oid(&self) -> Oid {
        relid::PG_OPFAMILY
    }
    #[allow(unused_variables)]
    fn scan(&self, ctx: &ScanContext, req: &ScanRequest) -> Result<Vec<Row>> {
        Err(PgError::unimplemented("CAT-35", "pg_opfamily rows"))
    }
}

/// Producer for `pg_extension`.
#[derive(Clone, Copy, Debug)]
pub struct PgExtension;

impl VirtualRelation for PgExtension {
    fn oid(&self) -> Oid {
        relid::PG_EXTENSION
    }
    #[allow(unused_variables)]
    fn scan(&self, ctx: &ScanContext, req: &ScanRequest) -> Result<Vec<Row>> {
        Err(PgError::unimplemented("CAT-35", "pg_extension rows"))
    }
}

/// Producer for `pg_available_extensions`.
#[derive(Clone, Copy, Debug)]
pub struct PgAvailableExtensions;

impl VirtualRelation for PgAvailableExtensions {
    fn oid(&self) -> Oid {
        relid::PG_AVAILABLE_EXTENSIONS
    }
    #[allow(unused_variables)]
    fn scan(&self, ctx: &ScanContext, req: &ScanRequest) -> Result<Vec<Row>> {
        Err(PgError::unimplemented("CAT-35", "pg_available_extensions rows"))
    }
}

/// Producer for `pg_available_extension_versions`.
#[derive(Clone, Copy, Debug)]
pub struct PgAvailableExtensionVersions;

impl VirtualRelation for PgAvailableExtensionVersions {
    fn oid(&self) -> Oid {
        relid::PG_AVAILABLE_EXTENSION_VERSIONS
    }
    #[allow(unused_variables)]
    fn scan(&self, ctx: &ScanContext, req: &ScanRequest) -> Result<Vec<Row>> {
        Err(PgError::unimplemented("CAT-35", "pg_available_extension_versions rows"))
    }
}


/// Registry slice.
pub static RELATIONS: &[&dyn VirtualRelation] = &[
    &PgCollation,
    &PgAm,
    &PgOpclass,
    &PgOpfamily,
    &PgExtension,
    &PgAvailableExtensions,
    &PgAvailableExtensionVersions,
];
