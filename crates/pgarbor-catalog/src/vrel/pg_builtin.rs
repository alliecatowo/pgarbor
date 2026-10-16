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
use super::pg_type::regproc_oid;
use super::rowbuild::{d, RowBuilder};
use super::{Row, ScanContext, ScanRequest, VirtualRelation};
use crate::builtin::{self, relid, TextRows};
use pgarbor_common::{Oid, PgError, Result};
use pgarbor_types::{io, Datum, TypeId};

/// True unless the request has an equality filter on column `idx` that contradicts `oid`.
/// Ignoring a filter is always correct (the executor re-applies the WHERE clause).
fn oid_passes(req: &ScanRequest, idx: usize, oid: Oid) -> bool {
    match req.oid_eq_on(idx) {
        Some(want) => want == oid,
        None => true,
    }
}

/// True unless the request has a text equality filter on column `idx` that contradicts `name`.
fn text_passes(req: &ScanRequest, idx: usize, name: &str) -> bool {
    match req.eq_on(idx) {
        Some(Datum::Text(t)) => t.as_str() == name,
        _ => true,
    }
}

/// Rows of a builtin text-form table: every cell is converted with `io::input` using the column
/// type of the system relation `data.rel`; NULL cells become `Datum::Null`.
fn text_rows(ctx: &ScanContext, data: &TextRows, req: &ScanRequest) -> Result<Vec<Row>> {
    let schema = builtin::system_rel(data.rel).ok_or_else(|| {
        PgError::internal(format!("builtin text rows reference unknown relation {}", data.rel))
    })?;
    let io_ctx = ctx.io_ctx();
    let mut out = Vec::with_capacity(data.rows.len());
    for cells in data.rows {
        if cells.len() != schema.columns.len() {
            return Err(PgError::internal(format!(
                "builtin row of {} has {} cells, expected {}",
                schema.name,
                cells.len(),
                schema.columns.len()
            )));
        }
        let mut row: Row = Vec::with_capacity(cells.len());
        for (col, cell) in schema.columns.iter().zip(cells.iter()) {
            row.push(match cell {
                None => Datum::Null,
                Some(text) => io::input(&io_ctx, TypeId::new(col.type_oid, col.typmod), *text)?,
            });
        }
        if let Some(want) = req.oid_eq_on(0) {
            if !matches!(row.first(), Some(Datum::Oid(o)) if *o == want) {
                continue;
            }
        }
        out.push(row);
    }
    Ok(out)
}

/// `pg_available_extensions` / `pg_available_extension_versions` entry (static, compiled in).
struct AvailableExtension {
    name: &'static str,
    default_version: &'static str,
    comment: &'static str,
    superuser: bool,
    trusted: bool,
    relocatable: bool,
    schema: &'static str,
}

/// The only extension PGArbor can serve (other contrib extensions are unavailable).
static AVAILABLE_EXTENSIONS: &[AvailableExtension] = &[AvailableExtension {
    name: "plpgsql",
    default_version: "1.0",
    comment: "PL/pgSQL procedural language",
    superuser: true,
    trusted: true,
    relocatable: false,
    schema: "pg_catalog",
}];

/// Version of the installed (`pg_extension`) row for `name`, if any.
fn installed_version(name: &str) -> Option<&'static str> {
    builtin::EXTENSIONS.iter().find(|e| e.name == name).map(|e| e.version)
}

/// Producer for `pg_collation`.
#[derive(Clone, Copy, Debug)]
pub struct PgCollation;

impl VirtualRelation for PgCollation {
    fn oid(&self) -> Oid {
        relid::PG_COLLATION
    }
    fn scan(&self, _ctx: &ScanContext, req: &ScanRequest) -> Result<Vec<Row>> {
        let schema = self.schema()?;
        let mut rows = Vec::with_capacity(builtin::COLLATIONS.len());
        for c in builtin::COLLATIONS {
            if !oid_passes(req, 0, c.oid) || !text_passes(req, 1, c.name) {
                continue;
            }
            let mut b = RowBuilder::new(schema);
            b.set("oid", d::oid(c.oid))
                .set("collname", d::name(c.name))
                .set("collnamespace", d::oid(c.namespace))
                .set("collowner", d::oid(c.owner))
                .set("collprovider", d::char_(c.provider))
                .set("collisdeterministic", d::bool_(c.deterministic))
                .set("collencoding", d::int4(c.encoding))
                .set("collcollate", d::opt_text(c.collate))
                .set("collctype", d::opt_text(c.ctype))
                .set("colllocale", d::opt_text(c.locale))
                .set("collicurules", d::opt_text(c.icu_rules))
                .set("collversion", d::opt_text(c.version));
            rows.push(b.finish()?);
        }
        rows.extend(super::pg_ext::collation_user_rows(_ctx, schema, req)?);
        Ok(rows)
    }
}

/// Producer for `pg_am`.
#[derive(Clone, Copy, Debug)]
pub struct PgAm;

impl VirtualRelation for PgAm {
    fn oid(&self) -> Oid {
        relid::PG_AM
    }
    fn scan(&self, _ctx: &ScanContext, req: &ScanRequest) -> Result<Vec<Row>> {
        let schema = self.schema()?;
        let mut rows = Vec::with_capacity(builtin::ACCESS_METHODS.len());
        for am in builtin::ACCESS_METHODS {
            if !oid_passes(req, 0, am.oid) || !text_passes(req, 1, am.name) {
                continue;
            }
            let handler = regproc_oid(am.handler);
            if handler == 0 {
                return Err(PgError::internal(format!(
                    "builtin access method {} has unknown handler {}",
                    am.name, am.handler
                )));
            }
            let mut b = RowBuilder::new(schema);
            b.set("oid", d::oid(am.oid))
                .set("amname", d::name(am.name))
                .set("amhandler", d::oid(handler))
                .set("amtype", d::char_(am.amtype));
            rows.push(b.finish()?);
        }
        Ok(rows)
    }
}

/// Producer for `pg_opclass`.
#[derive(Clone, Copy, Debug)]
pub struct PgOpclass;

impl VirtualRelation for PgOpclass {
    fn oid(&self) -> Oid {
        relid::PG_OPCLASS
    }
    fn scan(&self, ctx: &ScanContext, req: &ScanRequest) -> Result<Vec<Row>> {
        let mut rows = text_rows(ctx, &builtin::OPCLASSES, req)?;
        rows.extend(super::pg_ext::opclass_user_rows(ctx, req)?);
        Ok(rows)
    }
}

/// Producer for `pg_opfamily`.
#[derive(Clone, Copy, Debug)]
pub struct PgOpfamily;

impl VirtualRelation for PgOpfamily {
    fn oid(&self) -> Oid {
        relid::PG_OPFAMILY
    }
    fn scan(&self, ctx: &ScanContext, req: &ScanRequest) -> Result<Vec<Row>> {
        let mut rows = text_rows(ctx, &builtin::OPFAMILIES, req)?;
        rows.extend(super::pg_ext::opfamily_user_rows(ctx, req)?);
        Ok(rows)
    }
}

/// Producer for `pg_extension`.
#[derive(Clone, Copy, Debug)]
pub struct PgExtension;

impl VirtualRelation for PgExtension {
    fn oid(&self) -> Oid {
        relid::PG_EXTENSION
    }
    fn scan(&self, _ctx: &ScanContext, req: &ScanRequest) -> Result<Vec<Row>> {
        let schema = self.schema()?;
        let mut rows = Vec::with_capacity(builtin::EXTENSIONS.len());
        for e in builtin::EXTENSIONS {
            if !oid_passes(req, 0, e.oid) || !text_passes(req, 1, e.name) {
                continue;
            }
            // extconfig and extcondition stay NULL (RowBuilder default).
            let mut b = RowBuilder::new(schema);
            b.set("oid", d::oid(e.oid))
                .set("extname", d::name(e.name))
                .set("extowner", d::oid(e.owner))
                .set("extnamespace", d::oid(e.namespace))
                .set("extrelocatable", d::bool_(e.relocatable))
                .set("extversion", d::text(e.version));
            rows.push(b.finish()?);
        }
        rows.extend(super::pg_ext::extension_user_rows(_ctx, schema, req)?);
        Ok(rows)
    }
}

/// Producer for `pg_available_extensions`.
#[derive(Clone, Copy, Debug)]
pub struct PgAvailableExtensions;

impl VirtualRelation for PgAvailableExtensions {
    fn oid(&self) -> Oid {
        relid::PG_AVAILABLE_EXTENSIONS
    }
    fn scan(&self, _ctx: &ScanContext, req: &ScanRequest) -> Result<Vec<Row>> {
        let schema = self.schema()?;
        let mut rows = Vec::with_capacity(AVAILABLE_EXTENSIONS.len());
        for ext in AVAILABLE_EXTENSIONS {
            if !text_passes(req, 0, ext.name) {
                continue;
            }
            let mut b = RowBuilder::new(schema);
            b.set("name", d::name(ext.name))
                .set("default_version", d::text(ext.default_version))
                .set("installed_version", d::opt_text(installed_version(ext.name)))
                .set("comment", d::text(ext.comment));
            rows.push(b.finish()?);
        }
        Ok(rows)
    }
}

/// Producer for `pg_available_extension_versions`.
#[derive(Clone, Copy, Debug)]
pub struct PgAvailableExtensionVersions;

impl VirtualRelation for PgAvailableExtensionVersions {
    fn oid(&self) -> Oid {
        relid::PG_AVAILABLE_EXTENSION_VERSIONS
    }
    fn scan(&self, _ctx: &ScanContext, req: &ScanRequest) -> Result<Vec<Row>> {
        let schema = self.schema()?;
        let mut rows = Vec::with_capacity(AVAILABLE_EXTENSIONS.len());
        for ext in AVAILABLE_EXTENSIONS {
            if !text_passes(req, 0, ext.name) {
                continue;
            }
            let installed = installed_version(ext.name) == Some(ext.default_version);
            let mut b = RowBuilder::new(schema);
            // `requires` stays NULL.
            b.set("name", d::name(ext.name))
                .set("version", d::text(ext.default_version))
                .set("installed", d::bool_(installed))
                .set("superuser", d::bool_(ext.superuser))
                .set("trusted", d::bool_(ext.trusted))
                .set("relocatable", d::bool_(ext.relocatable))
                .set("schema", d::name(ext.schema))
                .set("comment", d::text(ext.comment));
            rows.push(b.finish()?);
        }
        Ok(rows)
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session::NullSessionInfo;
    use crate::testing;
    use pgarbor_types::IoConfig;

    /// Runs `f` with a context over a freshly bootstrapped catalog.
    fn with_ctx<R>(f: impl FnOnce(&ScanContext) -> R) -> R {
        let (_kv, env) = testing::bootstrapped().expect("bootstrap");
        let cfg = IoConfig::default();
        let ctx = ScanContext { env: &env, session: &NullSessionInfo, io_config: &cfg };
        f(&ctx)
    }

    fn text_of(value: &Datum) -> Option<&str> {
        match value {
            Datum::Text(s) => Some(s.as_str()),
            _ => None,
        }
    }

    #[test]
    fn collation_rows_match_builtin_table() {
        with_ctx(|ctx| {
            let rows = PgCollation.scan(ctx, &ScanRequest::all()).expect("scan");
            assert_eq!(rows.len(), builtin::COLLATIONS.len());
            // "default" has no libc strings: NULL collcollate, encoding -1.
            let default = &rows[0];
            assert!(matches!(default[0], Datum::Oid(100)));
            assert_eq!(text_of(&default[1]), Some("default"));
            assert!(matches!(default[7], Datum::Null));
            assert!(matches!(default[6], Datum::Int4(-1)));
            // "C" carries collcollate = "C".
            let c = rows.iter().find(|r| text_of(&r[1]) == Some("C")).expect("C row");
            assert_eq!(text_of(&c[7]), Some("C"));
            assert!(matches!(c[4], Datum::Char(b'c')));
        });
    }

    #[test]
    fn collation_oid_filter_returns_one_row() {
        with_ctx(|ctx| {
            let req = ScanRequest { eq: vec![(0, Datum::Oid(950))] };
            let rows = PgCollation.scan(ctx, &req).expect("scan");
            assert_eq!(rows.len(), 1);
            assert_eq!(text_of(&rows[0][1]), Some("C"));
        });
    }

    #[test]
    fn am_rows_have_handlers_and_types() {
        with_ctx(|ctx| {
            let rows = PgAm.scan(ctx, &ScanRequest::all()).expect("scan");
            assert_eq!(rows.len(), builtin::ACCESS_METHODS.len());
            let btree = rows.iter().find(|r| matches!(r[0], Datum::Oid(403))).expect("btree");
            let bthandler = regproc_oid("bthandler");
            assert!(bthandler != 0);
            assert!(matches!(btree[2], Datum::Oid(h) if h == bthandler));
            assert!(matches!(btree[3], Datum::Char(b'i')));
            let heap = rows.iter().find(|r| matches!(r[0], Datum::Oid(2))).expect("heap");
            assert!(matches!(heap[3], Datum::Char(b't')));
        });
    }

    #[test]
    fn text_catalogs_convert_cells() {
        with_ctx(|ctx| {
            let opc = PgOpclass.scan(ctx, &ScanRequest::all()).expect("opclass");
            assert_eq!(opc.len(), builtin::OPCLASSES.rows.len());
            let int4 = &opc[0];
            assert!(matches!(int4[0], Datum::Oid(1978)));
            assert_eq!(text_of(&int4[2]), Some("int4_ops"));
            assert!(matches!(int4[7], Datum::Bool(true)));

            let opf = PgOpfamily.scan(ctx, &ScanRequest::all()).expect("opfamily");
            assert_eq!(opf.len(), builtin::OPFAMILIES.rows.len());
            let filtered = PgOpfamily
                .scan(ctx, &ScanRequest { eq: vec![(0, Datum::Oid(397))] })
                .expect("filtered");
            assert_eq!(filtered.len(), 1);
            assert_eq!(text_of(&filtered[0][2]), Some("array_ops"));
        });
    }

    #[test]
    fn extension_catalogs_show_plpgsql_only() {
        with_ctx(|ctx| {
            let ext = PgExtension.scan(ctx, &ScanRequest::all()).expect("extension");
            assert_eq!(ext.len(), 1);
            assert_eq!(text_of(&ext[0][1]), Some("plpgsql"));
            assert_eq!(text_of(&ext[0][5]), Some("1.0"));
            assert!(matches!(ext[0][6], Datum::Null));
            assert!(matches!(ext[0][7], Datum::Null));

            let avail = PgAvailableExtensions.scan(ctx, &ScanRequest::all()).expect("available");
            assert_eq!(avail.len(), 1);
            assert_eq!(text_of(&avail[0][0]), Some("plpgsql"));
            assert_eq!(text_of(&avail[0][1]), Some("1.0"));
            assert_eq!(text_of(&avail[0][2]), Some("1.0"));
            assert_eq!(text_of(&avail[0][3]), Some("PL/pgSQL procedural language"));

            let versions = PgAvailableExtensionVersions.scan(ctx, &ScanRequest::all()).expect("versions");
            assert_eq!(versions.len(), 1);
            assert!(matches!(versions[0][2], Datum::Bool(true)));
            assert!(matches!(versions[0][5], Datum::Bool(false)));
            assert_eq!(text_of(&versions[0][6]), Some("pg_catalog"));
            assert!(matches!(versions[0][7], Datum::Null));
        });
    }
}
