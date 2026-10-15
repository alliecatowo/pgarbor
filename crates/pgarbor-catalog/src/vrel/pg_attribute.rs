//! `pg_attribute` and `pg_attrdef`. Lease CAT-29. PG sources: `catalog/pg_attribute.h`,
//! `pg_attrdef.h`, `catalog/heap.c` (`SystemAttributeDefinition`, `AddNewAttributeTuples`),
//! `utils/cache/relcache.c`. Oracle-checked against PG 18.6.
//!
//! ## pg_attribute rows (order: `attrelid` ascending, then `attnum` ascending; PG's physical order is
//! heap order, tools use ORDER BY). Fast path: eq on `attrelid` (col 0) - REQUIRED to be fast because
//! every driver introspection query filters by it.
//! Relations covered: builtin system relations (columns from `SystemRel::columns`, tables get system
//! columns, views and indexes do not), user relations (tables, matviews, partitioned tables,
//! views, composite types), user indexes (one row per key/include column), sequences (the three
//! columns `last_value bigint`, `log_cnt bigint`, `is_called boolean` + system columns).
//! System columns (relations with `RelKind::has_system_columns()`, i.e. tables, matviews, sequences,
//! partitioned/foreign tables; NOT views, indexes, composite types), attnums -1..-6:
//! | attnum | attname | atttypid | attlen | attbyval | attalign |
//! |---|---|---|---|---|---|
//! | -1 | ctid | tid (27) | 6 | false | 's' |
//! | -2 | xmin | xid (28) | 4 | true | 'i' |
//! | -3 | cmin | cid (29) | 4 | true | 'i' |
//! | -4 | xmax | xid (28) | 4 | true | 'i' |
//! | -5 | cmax | cid (29) | 4 | true | 'i' |
//! | -6 | tableoid | oid (26) | 4 | true | 'i' |
//! all with `attstorage` 'p', `attnotnull` true, `attislocal` true, everything else default.
//! Ordinary column derivation (`Column` -> row): `attname`; `atttypid` = `type_id.oid`; `atttypmod` =
//! `type_id.typmod`; `attlen/attbyval/attalign` from `env.type_info(oid)` (`TypeInfo` len, byval,
//! align); `attstorage` = `Column::storage` or the type's `typstorage` (text 'x', int 'p');
//! `attcompression` = `Column::compression` or 0 (`Char(0)`); `attndims`; `attnotnull`;
//! `atthasdef` = `default.is_some()` (generated columns too); `atthasmissing` =
//! `missing_value.is_some()`; `attmissingval` = a one-element array of the column's type built by
//! `io::input(ctx.io_ctx(), array type of the column, "{<escaped value>}")` (NULL when absent or no
//! array type exists); `attidentity` = `IdentityKind::as_char` or 0; `attgenerated` =
//! `GeneratedKind::as_char` or 0; `attisdropped`; `attislocal`; `attinhcount`; `attcollation`;
//! `attstattarget` = `stat_target` as Int2 or NULL (PG 18 NULL = default); `attacl`; `attoptions`
//! (text[] or NULL); `attfdwoptions` NULL.
//! Dropped columns: `attname` = `Column::dropped_name`, `atttypid` 0, `attlen` 0, `attbyval` false,
//! `attalign` 'i', `attstorage` 'p', `attisdropped` true, `attnotnull` false (PG keeps the old
//! `attlen`/`attalign`; PGArbor does not retain them: documented divergence).
//! Index rows: per key `IndexKey` (`attnum` = 1..n): `attname` = `IndexKey::name`, `atttypid`/typmod
//! from the key, `attcollation` = key collation, `attnotnull` false, storage/len/align from the type,
//! no system columns. Sequence rows: attnum 1..3 as above, `attnotnull` true.
//!
//! ## pg_attrdef
//! One row per column with a default or generation expression, ordered by `adrelid, adnum`:
//! `oid` = `ColumnDefault::oid`, `adrelid`, `adnum` (attnum), `adbin` = the expression text
//! (`StoredExpr::sql`; PG stores a node tree: `pg_get_expr(adbin, adrelid)` returns text unchanged,
//! see `sysfuncs::info`). Fast path: eq on `adrelid` (col 1).
use super::rowbuild::{d, RowBuilder};
use super::{Row, ScanContext, ScanRequest, VirtualRelation};
use crate::builtin::relid;
use pgarbor_common::{Oid, PgError, Result};

/// Producer for `pg_attribute`.
#[derive(Clone, Copy, Debug)]
pub struct PgAttribute;

impl VirtualRelation for PgAttribute {
    fn oid(&self) -> Oid {
        relid::PG_ATTRIBUTE
    }
    #[allow(unused_variables)]
    fn scan(&self, ctx: &ScanContext, req: &ScanRequest) -> Result<Vec<Row>> {
        Err(PgError::unimplemented("CAT-29", "pg_attribute rows"))
    }
}

/// Producer for `pg_attrdef`.
#[derive(Clone, Copy, Debug)]
pub struct PgAttrdef;

impl VirtualRelation for PgAttrdef {
    fn oid(&self) -> Oid {
        relid::PG_ATTRDEF
    }
    #[allow(unused_variables)]
    fn scan(&self, ctx: &ScanContext, req: &ScanRequest) -> Result<Vec<Row>> {
        Err(PgError::unimplemented("CAT-29", "pg_attrdef rows"))
    }
}


/// Registry slice.
pub static RELATIONS: &[&dyn VirtualRelation] = &[&PgAttribute, &PgAttrdef];

/// The six system columns (module docs table): `(attnum, name, type oid, len, byval, align)`.
// CONTRACT
pub const SYSTEM_COLUMN_DEFS: [(i16, &str, Oid, i16, bool, u8); 6] = [
    (-1, "ctid", 27, 6, false, b's'),
    (-2, "xmin", 28, 4, true, b'i'),
    (-3, "cmin", 29, 4, true, b'i'),
    (-4, "xmax", 28, 4, true, b'i'),
    (-5, "cmax", 29, 4, true, b'i'),
    (-6, "tableoid", 26, 4, true, b'i'),
];

#[allow(dead_code)]
fn _uses(_: &RowBuilder) {
    let _ = d::oid(0);
}
