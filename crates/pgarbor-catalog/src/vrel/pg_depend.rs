//! `pg_depend`, `pg_description`, `pg_shdescription`, `pg_shdepend`. Lease CAT-32.
//!
//! ## pg_depend
//! One row per `DepEntry` of `DepGraph::build(snapshot)` (`depend.rs` edge table): `classid` =
//! `ObjClass::classid()` of the dependent, `objid`, `objsubid` (= `sub_id`), `refclassid`,
//! `refobjid`, `refobjsubid`, `deptype` = `Char(DepType::as_char)`. Ordered by (classid, objid,
//! objsubid, refclassid, refobjid, refobjsubid). Builtin (pinned) referenced objects never appear
//! (PG shows `deptype` 'p' rows with refobjid 0; PGArbor omits them - documented). Fast paths:
//! eq on `objid` (col 1) and `refobjid` (col 4) (psql `\d` and ORMs filter by them).
//! `ObjClass::AttrDefault` rows use classid 2604 (pg_attrdef), `Rewrite` 2618.
//!
//! ## pg_description
//! One row per description: persisted `snapshot.data().descriptions` UNION builtin descriptions of
//! `builtin::BUILTIN_DESCRIPTIONS` (objsubid 0) minus builtin entries overridden by a persisted
//! description of the same key, EXCLUDING classoid 1260 (pg_authid) and 1262 (pg_database), which
//! belong to `pg_shdescription`. Columns `objoid, classoid, objsubid, description`. Ordered by
//! (objoid, classoid, objsubid). Fast path: eq on `objoid` (col 0).
//!
//! ## pg_shdescription
//! The classoid 1260/1262 descriptions (roles and databases; `COMMENT ON DATABASE postgres` is shown
//! with the bootstrap text `default administrative connection database`): columns `objoid, classoid,
//! description`.
//!
//! ## pg_shdepend
//! `depend::role_dependencies` for every role with at least one dependency: `dbid` = the database
//! OID (5), `classid` = class of the dependent object, `objid`, `objsubid` (column ACLs: attnum, else
//! 0), `refclassid` 1260 (pg_authid), `refobjid` = role OID, `deptype` 'o' (owner) or 'a' (ACL).
//! Owner rows for pinned/builtin roles of builtin objects are omitted. Ordered by
//! (classid, objid, objsubid, refobjid).
use super::{Row, ScanContext, ScanRequest, VirtualRelation};
use crate::builtin::relid;
use pgarbor_common::{Oid, PgError, Result};

/// Producer for `pg_depend`.
#[derive(Clone, Copy, Debug)]
pub struct PgDepend;

impl VirtualRelation for PgDepend {
    fn oid(&self) -> Oid {
        relid::PG_DEPEND
    }
    #[allow(unused_variables)]
    fn scan(&self, ctx: &ScanContext, req: &ScanRequest) -> Result<Vec<Row>> {
        Err(PgError::unimplemented("CAT-32", "pg_depend rows"))
    }
}

/// Producer for `pg_description`.
#[derive(Clone, Copy, Debug)]
pub struct PgDescription;

impl VirtualRelation for PgDescription {
    fn oid(&self) -> Oid {
        relid::PG_DESCRIPTION
    }
    #[allow(unused_variables)]
    fn scan(&self, ctx: &ScanContext, req: &ScanRequest) -> Result<Vec<Row>> {
        Err(PgError::unimplemented("CAT-32", "pg_description rows"))
    }
}

/// Producer for `pg_shdescription`.
#[derive(Clone, Copy, Debug)]
pub struct PgShdescription;

impl VirtualRelation for PgShdescription {
    fn oid(&self) -> Oid {
        relid::PG_SHDESCRIPTION
    }
    #[allow(unused_variables)]
    fn scan(&self, ctx: &ScanContext, req: &ScanRequest) -> Result<Vec<Row>> {
        Err(PgError::unimplemented("CAT-32", "pg_shdescription rows"))
    }
}

/// Producer for `pg_shdepend`.
#[derive(Clone, Copy, Debug)]
pub struct PgShdepend;

impl VirtualRelation for PgShdepend {
    fn oid(&self) -> Oid {
        relid::PG_SHDEPEND
    }
    #[allow(unused_variables)]
    fn scan(&self, ctx: &ScanContext, req: &ScanRequest) -> Result<Vec<Row>> {
        Err(PgError::unimplemented("CAT-32", "pg_shdepend rows"))
    }
}


/// Registry slice.
pub static RELATIONS: &[&dyn VirtualRelation] = &[&PgDepend, &PgDescription, &PgShdescription, &PgShdepend];
