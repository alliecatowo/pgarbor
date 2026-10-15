//! Index entry layout and key-interval arithmetic (lease EXE-03, north-star). ON-DISK FORMAT
//! (amendment `pgarbor-exec-01-unique-null-entries`, extends ARCHITECTURE §4).
//!
//! ## Entry layout (btree indexes; frozen v1)
//! ```text
//! entry key   := [index oid u32 BE] [keycodec::encode_key(key columns)] [rowid u64 BE]?   -- suffix rules below
//! entry value := rowid u64 BE (8 bytes)           -- ALWAYS present; scans read the rowid from the VALUE
//! ```
//! * The key columns are encoded with `pgarbor_types::keycodec` using `IndexDesc::key_cols`
//!   (type, DESC, NULLS FIRST/LAST). INCLUDE columns are not stored.
//! * **Non-unique index**: the rowid SUFFIX is always appended (distinct entries per row).
//! * **Unique index, no NULL among the key columns, or `nulls_not_distinct`**: NO suffix. The key
//!   itself is unique: a point `get` on `[oid][encoded key]` is the uniqueness probe.
//! * **Unique index with a NULL key column and not `nulls_not_distinct`** (NULLs are distinct in a
//!   unique index): the rowid SUFFIX is appended so equal-looking NULL keys of different rows do not
//!   collide. Such entries are never uniqueness conflicts.
//! * **Partial index**: a row gets an entry only if the predicate evaluates to TRUE.
//! * Expression keys are evaluated over the full table row.
//! Because keycodec bodies are prefix-free, `[oid][enc(values)]` is a PREFIX of exactly the entries
//! with those key values (with or without suffix): equality scans use prefix range scans.
//!
//! ## Interval arithmetic for scans
//! Per column tag bytes (never inverted by DESC): NULL = `0x00` if `nulls_first` else `0xFF`;
//! non-NULL = `0x80` followed by the (DESC-inverted) body. Hence the non-NULL region of a column
//! after prefix `P` is `[P ++ 0x80, P ++ 0x81)`. [`interval_for_bounds`] converts SQL-level bounds to a
//! byte interval:
//! * equality prefix `eq` (k values) -> `P = [oid] ++ enc(eq)`;
//! * no range: `[P, upper_bound(P))`;
//! * range on column k with `lower`/`upper` SQL bounds `(value, inclusive)`; for an ASCENDING column
//!   `lower` is the start and `upper` the end side; for a DESCENDING column the byte order is
//!   mirrored: SQL `col >= x` (lower) is the byte END side (`<= enc(x)` region), SQL `col <= x` the
//!   START side. Inclusive vs exclusive: inclusive start = `Included(P ++ enc(x))`, exclusive start =
//!   `Included(upper_bound(P ++ enc(x)))` (skips every entry having that value, suffix included);
//!   inclusive end = `Excluded(upper_bound(P ++ enc(x)))`, exclusive end = `Excluded(P ++ enc(x))`.
//! * a missing side is bounded by the non-NULL region (`P ++ 0x80` as start, `upper_bound(P ++ 0x80)`
//!   as end) because a range predicate never matches NULL.
//! `upper_bound(x)` = `pgarbor_common::kv::prefix_upper_bound` (smallest key greater than all keys
//! with prefix `x`).
use std::ops::Bound;

use pgarbor_common::{Oid, PgError, Result};
use pgarbor_types::context::TypeEnv;
use pgarbor_types::keycodec::KeyColumn;
use pgarbor_types::Datum;

use crate::runtime::Executor;
use crate::tabledesc::{IndexDesc, TableDesc};

/// A computed index entry for one row.
// CONTRACT
#[derive(Clone, Debug, PartialEq)]
pub struct EntryKey {
    /// The full entry key (module docs), including the rowid suffix when applicable.
    pub key: Vec<u8>,
    /// `[oid][enc(values)]` prefix when the entry takes part in uniqueness checking (unique index
    /// whose key has no NULL, or `nulls_not_distinct`); `None` when it can never conflict.
    pub probe: Option<Vec<u8>>,
    /// The evaluated key column values (for `Key (a)=(1)` details).
    pub values: Vec<Datum>,
}

/// `[oid]` prefix of every key of index `oid`.
pub fn index_prefix(oid: Oid) -> [u8; 4] {
    oid.to_be_bytes()
}

/// Entry value for `rowid`.
pub fn entry_value(rowid: u64) -> [u8; 8] {
    rowid.to_be_bytes()
}

/// Parse an entry value; wrong length -> XX001 `invalid index entry value`.
pub fn rowid_of_entry_value(v: &[u8]) -> Result<u64> {
    if v.len() != 8 {
        return Err(PgError::corruption(format!("invalid index entry value of length {}", v.len())));
    }
    let mut b = [0u8; 8];
    b.copy_from_slice(v);
    Ok(u64::from_be_bytes(b))
}

/// Evaluate the key column values of `idx` over a full table row (`row[attnum-1]` for column keys,
/// `eval` for expression keys). Result length = `idx.keys.len()`.
#[allow(unused_variables)]
pub fn index_values(ex: &mut Executor, idx: &IndexDesc, row: &[Datum]) -> Result<Vec<Datum>> {
    Err(PgError::unimplemented("EXE-03", "index key values (index_key::index_values)"))
}

/// True when the row belongs in the index (no predicate, or the predicate is TRUE).
#[allow(unused_variables)]
pub fn row_in_index(ex: &mut Executor, idx: &IndexDesc, row: &[Datum]) -> Result<bool> {
    Err(PgError::unimplemented("EXE-03", "partial index predicate (index_key::row_in_index)"))
}

/// The entry of `row` (stored at heap `rowid`) in `idx`, or `None` if a partial index excludes it
/// (module docs for the layout).
#[allow(unused_variables)]
pub fn make_entry(ex: &mut Executor, idx: &IndexDesc, rowid: u64, row: &[Datum]) -> Result<Option<EntryKey>> {
    Err(PgError::unimplemented("EXE-03", "index entry construction (index_key::make_entry)"))
}

/// `[oid] ++ keycodec::encode_key(env, cols, values)` for a (possibly partial) list of leading key
/// values: the prefix shared by every entry with those leading values.
#[allow(unused_variables)]
pub fn encode_prefix(env: &dyn TypeEnv, oid: Oid, cols: &[KeyColumn], values: &[Datum]) -> Result<Vec<u8>> {
    Err(PgError::unimplemented("EXE-03", "index prefix encoding (index_key::encode_prefix)"))
}

/// A byte interval over entry keys (`scan(start, end)`).
// CONTRACT
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct KeyInterval {
    pub start: Bound<Vec<u8>>,
    pub end: Bound<Vec<u8>>,
}

/// SQL-level `(value, inclusive)` bound of [`interval_for_bounds`].
pub type SqlBound<'a> = Option<(&'a Datum, bool)>;

/// Byte interval of an index scan (module docs, "Interval arithmetic"). `eq` = values of the
/// leading equality columns, `lower`/`upper` constrain key column `eq.len()`. A NULL equality value
/// or NULL range bound means the scan matches nothing: return `Ok(None)`.
#[allow(unused_variables)]
pub fn interval_for_bounds(
    env: &dyn TypeEnv,
    oid: Oid,
    cols: &[KeyColumn],
    eq: &[Datum],
    lower: SqlBound,
    upper: SqlBound,
) -> Result<Option<KeyInterval>> {
    Err(PgError::unimplemented("EXE-03", "index scan interval (index_key::interval_for_bounds)"))
}

/// Convenience used by DML: the heap rowid referenced by entry `(key, value)`.
pub fn entry_rowid(_key: &[u8], value: &[u8]) -> Result<u64> {
    rowid_of_entry_value(value)
}

/// Compare two evaluated key-value lists for index-key equality under unique semantics
/// (`compare::not_distinct` on each column; used to skip index maintenance when an UPDATE does not
/// change any key column).
#[allow(unused_variables)]
pub fn same_key_values(env: &dyn TypeEnv, td: &TableDesc, idx: &IndexDesc, a: &[Datum], b: &[Datum]) -> Result<bool> {
    Err(PgError::unimplemented("EXE-03", "index key equality (index_key::same_key_values)"))
}
