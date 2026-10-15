//! `pg_type`, `pg_enum`, `pg_range`. Lease CAT-30. PG sources: `catalog/pg_type.h`, `pg_enum.h`,
//! `pg_range.h`. Oracle-checked.
//!
//! ## pg_type
//! Rows ordered by `oid`: (1) every builtin type of `pgarbor_types::catalog_data::TYPES` (622 rows:
//! base, array, pseudo, composite catalog rowtypes, information_schema domains), (2) user types.
//! Fast path: eq on `oid` (col 0) and `typname` (col 1, + `typnamespace` col 2).
//! Builtin row from `BuiltinType`: `oid`, `typname`=name, `typnamespace`, `typowner` 10 (bootstrap
//! superuser), `typlen`, `typbyval`, `typtype`, `typcategory`, `typispreferred`, `typisdefined` true,
//! `typdelim`, `typrelid`, `typelem`, `typarray`, `typalign`, `typstorage`, `typnotnull` false,
//! `typbasetype`, `typtypmod`, `typndims`, `typcollation`, `typdefaultbin` NULL, `typdefault` NULL,
//! `typacl` NULL. The regproc columns hold the proc OID (`Oid`) of the named function: `typinput`,
//! `typoutput`, `typreceive`, `typsend` via `regproc_oid(name)` = the builtin proc with that
//! `proname` in pg_catalog (unique for I/O functions; 0 when the name is "-" or unknown);
//! `typsubscript` = `regproc_oid("array_subscript_handler")` for every type of
//! `typcategory` 'A' (true arrays AND `int2vector`/`oidvector`), `regproc_oid("raw_array_subscript_handler")`
//! for `name`, `point`, `box`, `line`, `lseg`, `regproc_oid("jsonb_subscript_handler")` for `jsonb`, else 0
//! (oracle `select typname, typsubscript from pg_type`); `typmodin`/`typmodout` 0 (not recorded in
//! `BuiltinType`, amendment 03), `typanalyze` = `regproc_oid("array_typanalyze")` for true arrays (category
//! 'A' types that are some type's `typarray`), `range_typanalyze` for range types, `multirange_typanalyze`
//! for multirange types, `ts_typanalyze` for `tsvector`, else 0. (Never hard-code these OIDs: look the
//! names up with `regproc_oid`.)
//! User rows come from `CatalogEnv::user_type_info` (synthesized shapes documented in `typeenv.rs`) for
//! every `UserTypeRef` of `snapshot.indexes().user_types` sorted by OID, adding: `typowner` (TypeObj
//! owner / relation owner), `typacl`, domains: `typnotnull`, `typdefault` = default expr text and
//! `typdefaultbin` = same text, `typcollation`. Oracle facts: a table's rowtype has `typtype` 'c',
//! `typcategory` 'C', `typrelid` = the table, `typarray` = its array; an array type has `typtype` 'b',
//! category 'A', `typelem` = element, `typinput` `array_in`, `typalign` 'd' when the element's align
//! is 'd' (else 'i'), `typstorage` 'x', `typcollation` = the element's.
//!
//! ## pg_enum
//! One row per `EnumLabelDef`: `oid`, `enumtypid`, `enumsortorder` (`Float4`), `enumlabel`; ordered by
//! (`enumtypid`, `enumsortorder`). Fast path: eq on `enumtypid` (col 1). No builtin enums.
//!
//! ## pg_range
//! The builtin range types from `builtin::RANGES` (`TextRows`, PG text-output form): convert each
//! cell with `io::input(ctx.io_ctx(), TypeId::new(column type, typmod), text)`; NULL -> `Datum::Null`.
//! Ordered by `rngtypid`. No user ranges (CREATE TYPE ... AS RANGE is unsupported).
use super::{Row, ScanContext, ScanRequest, VirtualRelation};
use crate::builtin::relid;
use pgarbor_common::{Oid, PgError, Result};

/// Producer for `pg_type`.
#[derive(Clone, Copy, Debug)]
pub struct PgType;

impl VirtualRelation for PgType {
    fn oid(&self) -> Oid {
        relid::PG_TYPE
    }
    #[allow(unused_variables)]
    fn scan(&self, ctx: &ScanContext, req: &ScanRequest) -> Result<Vec<Row>> {
        Err(PgError::unimplemented("CAT-30", "pg_type rows"))
    }
}

/// Producer for `pg_enum`.
#[derive(Clone, Copy, Debug)]
pub struct PgEnum;

impl VirtualRelation for PgEnum {
    fn oid(&self) -> Oid {
        relid::PG_ENUM
    }
    #[allow(unused_variables)]
    fn scan(&self, ctx: &ScanContext, req: &ScanRequest) -> Result<Vec<Row>> {
        Err(PgError::unimplemented("CAT-30", "pg_enum rows"))
    }
}

/// Producer for `pg_range`.
#[derive(Clone, Copy, Debug)]
pub struct PgRange;

impl VirtualRelation for PgRange {
    fn oid(&self) -> Oid {
        relid::PG_RANGE
    }
    #[allow(unused_variables)]
    fn scan(&self, ctx: &ScanContext, req: &ScanRequest) -> Result<Vec<Row>> {
        Err(PgError::unimplemented("CAT-30", "pg_range rows"))
    }
}


/// Registry slice.
pub static RELATIONS: &[&dyn VirtualRelation] = &[&PgType, &PgEnum, &PgRange];

/// OID of the builtin pg_catalog function named `name` (first match), 0 for `"-"`/unknown.
/// Shared by `pg_type`, `pg_operator`, `pg_aggregate` and `pg_am` producers.
// CONTRACT
pub fn regproc_oid(name: &str) -> Oid {
    static MAP: std::sync::OnceLock<std::collections::HashMap<&'static str, Oid>> = std::sync::OnceLock::new();
    if name.is_empty() || name == "-" {
        return 0;
    }
    let map = MAP.get_or_init(|| {
        let mut m = std::collections::HashMap::new();
        for p in pgarbor_types::catalog_data::PROCS.iter() {
            if p.namespace == crate::builtin::PG_CATALOG_NAMESPACE {
                m.entry(p.name).or_insert(p.oid);
            }
        }
        m
    });
    map.get(name).copied().unwrap_or(0)
}
