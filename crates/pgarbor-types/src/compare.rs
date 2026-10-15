//! SQL ordering, equality and hashing of Datums (TYPE-04, TYPE-11). Lease TYP-49.
//!
//! These are the semantics of each type's DEFAULT btree / hash opclass in PG18, used by ORDER BY,
//! DISTINCT, GROUP BY, UNION, joins, min/max, array/record comparison, unique checks and indexes
//! (`keycodec` must order bytes exactly like `cmp_datums`). Both operands have type `ty`
//! (cross-type comparisons are resolved to operator procs by the binder, not here).
//!
//! Per type (`ty.oid`, following domains to the base type):
//! - bool: false < true. ints: numeric. oid/reg*/xid/cid: UNSIGNED u32. "char": unsigned byte.
//! - float4/float8: total order with `-0 = 0`, NaN = NaN, NaN greater than everything (PG `float8_cmp_internal`).
//! - numeric: `Numeric::pg_cmp`.
//! - text/varchar/name/unknown: codepoint (= UTF-8 byte) order for every collation in v1 (the pinned
//!   oracle collation is C.UTF-8 / builtin). bpchar: compare with trailing spaces removed.
//! - bytea: memcmp then length. uuid: memcmp of the 16 bytes.
//! - date/time/timestamp(tz): integer order (infinities are MIN/MAX). timetz: by (time + zone*1e6) then
//!   zone (PG `timetz_cmp_internal`). interval: `Interval::cmp_span` (`'1 mon' = '30 days'`).
//! - jsonb: `json::ops::compare_jsonb`. json: no ordering → 42883 `could not identify an ordering
//!   operator for type json` (equality too: `could not identify an equality operator for type json`).
//! - arrays: element-wise via the element type (`array::funcs` docs), NULL elements last.
//! - enums: `enum_type::cmp_labels`. records/composites: field-wise with field types from
//!   `env.composite_fields` (NULL fields last).
//! - unsupported types → 0A000 via `PgError::unimplemented("TYP-49", ...)`.
//! NULL handling: `cmp_datums` orders NULL after every non-NULL value (NULLS LAST) and NULL = NULL;
//! `sql_eq` implements three-valued `=` (NULL operand → None); `not_distinct` implements
//! `IS NOT DISTINCT FROM` (NULLs equal).
//!
//! Hashing (`hash_datum`) must be consistent with equality (`1.0 = 1.00` hash the same; `-0.0` and `0.0`
//! hash the same; all NaNs hash the same; bpchar ignores trailing spaces; intervals hash by span). It
//! does NOT need to equal PG's hash values. PG-compatible hash values (`hashint4(1)` etc., needed by
//! the SQL-callable `hash*` functions and hash partitioning) come from `pg_hash_*` below, which port
//! `src/common/hashfn.c` (`hash_bytes`, `hash_bytes_uint32`, extended 64-bit variants) exactly.
//! Oracle fixtures: `hashint4(1)` = -1905060026, `hashint8(1)` = -1905060026, `hashtext('abc')` =
//! -785388649, `hashint4extended(1, 0)` = -3670598878359251130, `hashfloat8(0.0)` = `hashfloat8(-0.0)` = 0.
use crate::context::TypeEnv;
use crate::datum::{Datum, TypeId};
use pgarbor_common::{PgError, Result};
use std::cmp::Ordering;
use std::hash::Hasher;

/// Total order of two values of type `ty` (NULLS LAST, NULL = NULL).
// CONTRACT
#[allow(unused_variables)]
pub fn cmp_datums(env: &dyn TypeEnv, ty: TypeId, a: &Datum, b: &Datum) -> Result<Ordering> {
    Err(PgError::unimplemented("TYP-49", "datum comparison"))
}

/// Ordering for ORDER BY with explicit direction and NULLS placement.
/// `desc` reverses the non-NULL order; `nulls_first` places NULLs first (independently of `desc`).
// CONTRACT
#[allow(unused_variables)]
pub fn cmp_sort(env: &dyn TypeEnv, ty: TypeId, a: &Datum, b: &Datum, desc: bool, nulls_first: bool) -> Result<Ordering> {
    Err(PgError::unimplemented("TYP-49", "datum sort comparison"))
}

/// SQL `=` with three-valued logic: `None` when either side is NULL.
// CONTRACT
#[allow(unused_variables)]
pub fn sql_eq(env: &dyn TypeEnv, ty: TypeId, a: &Datum, b: &Datum) -> Result<Option<bool>> {
    Err(PgError::unimplemented("TYP-49", "datum equality"))
}

/// `a IS NOT DISTINCT FROM b` (NULLs compare equal; never NULL).
// CONTRACT
#[allow(unused_variables)]
pub fn not_distinct(env: &dyn TypeEnv, ty: TypeId, a: &Datum, b: &Datum) -> Result<bool> {
    Err(PgError::unimplemented("TYP-49", "IS NOT DISTINCT FROM"))
}

/// Feed a hash of `d` (type `ty`) consistent with `not_distinct` equality into `state`.
// CONTRACT
#[allow(unused_variables)]
pub fn hash_datum(env: &dyn TypeEnv, ty: TypeId, d: &Datum, state: &mut dyn Hasher) -> Result<()> {
    Err(PgError::unimplemented("TYP-49", "datum hashing"))
}

/// True if values of `ty` can be ordered (default btree opclass exists: false for json, xml, point...).
// CONTRACT
#[allow(unused_variables)]
pub fn is_orderable(env: &dyn TypeEnv, ty: TypeId) -> Result<bool> {
    Err(PgError::unimplemented("TYP-49", "orderability check"))
}

/// PG `hash_bytes_uint32(v)` (Bob Jenkins lookup3 as in `src/common/hashfn.c`) as i32 bits.
// CONTRACT
#[allow(unused_variables)]
pub fn pg_hash_uint32(v: u32) -> Result<u32> {
    Err(PgError::unimplemented("TYP-49", "PostgreSQL uint32 hash"))
}

/// PG `hash_bytes(k, len)`.
// CONTRACT
#[allow(unused_variables)]
pub fn pg_hash_bytes(k: &[u8]) -> Result<u32> {
    Err(PgError::unimplemented("TYP-49", "PostgreSQL bytes hash"))
}

/// PG `hash_bytes_uint32_extended(v, seed)`.
// CONTRACT
#[allow(unused_variables)]
pub fn pg_hash_uint32_extended(v: u32, seed: u64) -> Result<u64> {
    Err(PgError::unimplemented("TYP-49", "PostgreSQL uint32 extended hash"))
}

/// PG `hash_bytes_extended(k, len, seed)`.
// CONTRACT
#[allow(unused_variables)]
pub fn pg_hash_bytes_extended(k: &[u8], seed: u64) -> Result<u64> {
    Err(PgError::unimplemented("TYP-49", "PostgreSQL bytes extended hash"))
}
