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

use pgarbor_common::kv::prefix_upper_bound;
use pgarbor_common::{Oid, PgError, Result};
use pgarbor_types::compare::not_distinct;
use pgarbor_types::context::TypeEnv;
use pgarbor_types::keycodec::{self, KeyColumn};
use pgarbor_types::Datum;

use crate::eval;
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
pub fn index_values(ex: &mut Executor, idx: &IndexDesc, row: &[Datum]) -> Result<Vec<Datum>> {
    let mut out = Vec::with_capacity(idx.keys.len());
    for k in &idx.keys {
        let v = if let Some(expr) = &k.expr {
            eval::eval(ex, expr, row)?
        } else if let Some(attnum) = k.column {
            column_value(row, attnum)?
        } else {
            return Err(PgError::internal(format!(
                "index \"{}\" has a key with neither a column nor an expression",
                idx.name
            )));
        };
        out.push(v);
    }
    Ok(out)
}

/// Value of table column `attnum` (1-based) in a full table row.
fn column_value(row: &[Datum], attnum: i16) -> Result<Datum> {
    if attnum < 1 {
        return Err(PgError::internal(format!("invalid index key column number {attnum}")));
    }
    row.get(attnum as usize - 1).cloned().ok_or_else(|| {
        PgError::internal(format!("index key column {attnum} is outside a table row of {} values", row.len()))
    })
}

/// True when the row belongs in the index (no predicate, or the predicate is TRUE).
pub fn row_in_index(ex: &mut Executor, idx: &IndexDesc, row: &[Datum]) -> Result<bool> {
    match &idx.predicate {
        None => Ok(true),
        Some(pred) => Ok(eval::eval_bool3(ex, pred, row)? == Some(true)),
    }
}

/// The entry of `row` (stored at heap `rowid`) in `idx`, or `None` if a partial index excludes it
/// (module docs for the layout).
pub fn make_entry(ex: &mut Executor, idx: &IndexDesc, rowid: u64, row: &[Datum]) -> Result<Option<EntryKey>> {
    if !row_in_index(ex, idx, row)? {
        return Ok(None);
    }
    let values = index_values(ex, idx, row)?;
    if values.len() != idx.key_cols.len() {
        return Err(PgError::internal(format!(
            "index \"{}\" has {} key values but {} key columns",
            idx.name,
            values.len(),
            idx.key_cols.len()
        )));
    }
    let has_null = values.iter().any(|v| v.is_null());
    // Unique key without a NULL (or NULLS NOT DISTINCT): the key itself is the uniqueness probe and
    // carries no rowid suffix. Everything else (non-unique, or unique with a NULL) gets the suffix.
    let own_key = idx.unique && (!has_null || idx.nulls_not_distinct);
    let env: &dyn TypeEnv = &ex.cx.se.env;
    let mut key = Vec::with_capacity(4 + 16 * values.len() + 8);
    key.extend_from_slice(&index_prefix(idx.oid));
    keycodec::encode_key(env, &idx.key_cols, &values, &mut key)?;
    let probe = if own_key { Some(key.clone()) } else { None };
    if !own_key {
        key.extend_from_slice(&rowid.to_be_bytes());
    }
    Ok(Some(EntryKey { key, probe, values }))
}

/// `[oid] ++ keycodec::encode_key(env, cols, values)` for a (possibly partial) list of leading key
/// values: the prefix shared by every entry with those leading values.
pub fn encode_prefix(env: &dyn TypeEnv, oid: Oid, cols: &[KeyColumn], values: &[Datum]) -> Result<Vec<u8>> {
    let mut out = Vec::with_capacity(4 + 16 * values.len());
    out.extend_from_slice(&index_prefix(oid));
    keycodec::encode_key(env, cols, values, &mut out)?;
    Ok(out)
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
pub fn interval_for_bounds(
    env: &dyn TypeEnv,
    oid: Oid,
    cols: &[KeyColumn],
    eq: &[Datum],
    lower: SqlBound,
    upper: SqlBound,
) -> Result<Option<KeyInterval>> {
    if eq.len() > cols.len() {
        return Err(PgError::internal(format!(
            "index scan has {} equality values but {} key columns",
            eq.len(),
            cols.len()
        )));
    }
    if eq.iter().any(|v| v.is_null()) {
        return Ok(None);
    }
    if lower.map_or(false, |(v, _)| v.is_null()) || upper.map_or(false, |(v, _)| v.is_null()) {
        return Ok(None);
    }
    let mut prefix = Vec::with_capacity(4 + 16 * eq.len());
    prefix.extend_from_slice(&index_prefix(oid));
    keycodec::encode_key(env, &cols[..eq.len()], eq, &mut prefix)?;

    if lower.is_none() && upper.is_none() {
        let end = match prefix_upper_bound(&prefix) {
            Some(e) => Bound::Excluded(e),
            None => Bound::Unbounded,
        };
        return Ok(Some(KeyInterval { start: Bound::Included(prefix), end }));
    }

    let k = eq.len();
    if k >= cols.len() {
        return Err(PgError::internal(format!(
            "index scan range on key column {} but the index has {} key columns",
            k + 1,
            cols.len()
        )));
    }
    let col = &cols[k];

    // Non-NULL region of column k after the prefix: [P ++ 0x80, P ++ 0x81). Tags are never inverted
    // by DESC, so this holds for both directions.
    let mut non_null_lo = prefix.clone();
    non_null_lo.push(0x80);
    let non_null_hi = match prefix_upper_bound(&non_null_lo) {
        Some(e) => Bound::Excluded(e),
        None => Bound::Unbounded,
    };

    // DESC mirrors the byte order: the SQL lower bound is the byte END side and vice versa.
    let (start_sql, end_sql) = if col.desc { (upper, lower) } else { (lower, upper) };

    let start = match start_sql {
        None => Bound::Included(non_null_lo),
        Some((x, inclusive)) => {
            let at = value_prefix(env, &prefix, col, x)?;
            if inclusive {
                Bound::Included(at)
            } else {
                match prefix_upper_bound(&at) {
                    Some(after) => Bound::Included(after),
                    // No key can follow every key of this value: nothing matches.
                    None => return Ok(None),
                }
            }
        }
    };
    let end = match end_sql {
        None => non_null_hi,
        Some((x, inclusive)) => {
            let at = value_prefix(env, &prefix, col, x)?;
            if inclusive {
                match prefix_upper_bound(&at) {
                    Some(after) => Bound::Excluded(after),
                    None => Bound::Unbounded,
                }
            } else {
                Bound::Excluded(at)
            }
        }
    };
    // An empty interval (start at or past end) matches nothing; scanning it would be an invalid range.
    if let (Bound::Included(s), Bound::Excluded(e)) = (&start, &end) {
        if s >= e {
            return Ok(None);
        }
    }
    Ok(Some(KeyInterval { start, end }))
}

/// `P ++ enc(x)` for column `col`: the prefix shared by every entry whose column value is `x`.
fn value_prefix(env: &dyn TypeEnv, prefix: &[u8], col: &KeyColumn, x: &Datum) -> Result<Vec<u8>> {
    let mut out = Vec::with_capacity(prefix.len() + 16);
    out.extend_from_slice(prefix);
    keycodec::encode_value(env, col, x, &mut out)?;
    Ok(out)
}

/// Convenience used by DML: the heap rowid referenced by entry `(key, value)`.
pub fn entry_rowid(_key: &[u8], value: &[u8]) -> Result<u64> {
    rowid_of_entry_value(value)
}

/// Compare two evaluated key-value lists for index-key equality under unique semantics
/// (`compare::not_distinct` on each column; used to skip index maintenance when an UPDATE does not
/// change any key column).
pub fn same_key_values(env: &dyn TypeEnv, td: &TableDesc, idx: &IndexDesc, a: &[Datum], b: &[Datum]) -> Result<bool> {
    // The table description is not needed: key types come from the index itself.
    let _ = td;
    if a.len() != idx.keys.len() || b.len() != idx.keys.len() {
        return Err(PgError::internal(format!(
            "index \"{}\" key comparison over {} and {} values, expected {}",
            idx.name,
            a.len(),
            b.len(),
            idx.keys.len()
        )));
    }
    for (i, k) in idx.keys.iter().enumerate() {
        if !not_distinct(env, k.ty, &a[i], &b[i])? {
            return Ok(false);
        }
    }
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::context::{ExecContext, NoGuc, NoServices, SessionEnv};
    use crate::memtxn::MemTxn;
    use crate::runtime::Runtime;
    use crate::tabledesc::IdxKeyDesc;
    use pgarbor_types::context::IoConfig;
    use pgarbor_types::TypeId;
    use std::cell::Cell;

    const TEST_OID: Oid = 16400;

    /// One int4 key column: `(value, desc, nulls_first)`; a `None` value is NULL.
    type Col = (Option<i32>, bool, bool);

    /// Hand-encoded entry key (module docs): `[oid]`, then per column the NULL tag or `0x80` + int4
    /// body (sign bit flipped, bitwise inverted for DESC), then the optional rowid suffix.
    fn hand_key(oid: Oid, cols: &[Col], rowid: Option<u64>) -> Vec<u8> {
        let mut k = oid.to_be_bytes().to_vec();
        for (v, desc, nulls_first) in cols {
            match v {
                None => k.push(if *nulls_first { 0x00 } else { 0xFF }),
                Some(x) => {
                    let mut body = ((*x as u32) ^ 0x8000_0000).to_be_bytes();
                    if *desc {
                        for b in body.iter_mut() {
                            *b = !*b;
                        }
                    }
                    k.push(0x80);
                    k.extend_from_slice(&body);
                }
            }
        }
        if let Some(r) = rowid {
            k.extend_from_slice(&r.to_be_bytes());
        }
        k
    }

    fn in_interval(iv: &KeyInterval, key: &[u8]) -> bool {
        let after_start = match &iv.start {
            Bound::Included(s) => key >= s.as_slice(),
            Bound::Excluded(s) => key > s.as_slice(),
            Bound::Unbounded => true,
        };
        let before_end = match &iv.end {
            Bound::Included(e) => key <= e.as_slice(),
            Bound::Excluded(e) => key < e.as_slice(),
            Bound::Unbounded => true,
        };
        after_start && before_end
    }

    /// SQL meaning of `v` against the bounds (NULL never matches a range).
    fn sql_matches(v: Option<i32>, lower: Option<(i32, bool)>, upper: Option<(i32, bool)>) -> bool {
        let x = match v {
            Some(x) => x,
            None => return false,
        };
        let lo = match lower {
            None => true,
            Some((b, true)) => x >= b,
            Some((b, false)) => x > b,
        };
        let hi = match upper {
            None => true,
            Some((b, true)) => x <= b,
            Some((b, false)) => x < b,
        };
        lo && hi
    }

    fn scan_interval(
        env: &dyn TypeEnv,
        desc: bool,
        nulls_first: bool,
        lower: Option<(i32, bool)>,
        upper: Option<(i32, bool)>,
    ) -> Result<Option<KeyInterval>> {
        let col = KeyColumn { ty: TypeId::INT4, desc, nulls_first };
        let lo_d = lower.map(|(v, inc)| (Datum::Int4(v), inc));
        let up_d = upper.map(|(v, inc)| (Datum::Int4(v), inc));
        let lo = lo_d.as_ref().map(|(d, inc)| (d, *inc));
        let up = up_d.as_ref().map(|(d, inc)| (d, *inc));
        interval_for_bounds(env, TEST_OID, std::slice::from_ref(&col), &[], lo, up)
    }

    #[test]
    fn single_column_intervals_match_sql_semantics() -> Result<()> {
        let (_kv, env) = pgarbor_catalog::testing::bootstrapped()?;
        let bounds: [Option<(i32, bool)>; 5] =
            [None, Some((5, true)), Some((5, false)), Some((9, true)), Some((9, false))];
        let values: Vec<Option<i32>> = (-3..=12).map(Some).chain(std::iter::once(None)).collect();
        for &desc in &[false, true] {
            for &nulls_first in &[false, true] {
                for &lower in &bounds {
                    for &upper in &bounds {
                        let iv = scan_interval(&env, desc, nulls_first, lower, upper)?;
                        for (i, &v) in values.iter().enumerate() {
                            // With and without the rowid suffix (non-unique vs unique entries).
                            for rowid in [Some(1000 + i as u64), None] {
                                let key = hand_key(TEST_OID, &[(v, desc, nulls_first)], rowid);
                                let actual = iv.as_ref().map_or(false, |iv| in_interval(iv, &key));
                                let expected = sql_matches(v, lower, upper);
                                assert_eq!(
                                    actual, expected,
                                    "desc={desc} nulls_first={nulls_first} lower={lower:?} upper={upper:?} v={v:?} rowid={rowid:?}"
                                );
                            }
                        }
                    }
                }
            }
        }
        Ok(())
    }

    #[test]
    fn ascending_range_contains_five_to_eight() -> Result<()> {
        let (_kv, env) = pgarbor_catalog::testing::bootstrapped()?;
        let iv = scan_interval(&env, false, false, Some((5, true)), Some((9, false)))?.expect("non-empty");
        assert!(in_interval(&iv, &hand_key(TEST_OID, &[(Some(5), false, false)], Some(1))));
        assert!(in_interval(&iv, &hand_key(TEST_OID, &[(Some(8), false, false)], Some(2))));
        assert!(!in_interval(&iv, &hand_key(TEST_OID, &[(Some(9), false, false)], Some(3))));
        assert!(!in_interval(&iv, &hand_key(TEST_OID, &[(None, false, false)], Some(4))));
        Ok(())
    }

    #[test]
    fn empty_range_matches_nothing() -> Result<()> {
        let (_kv, env) = pgarbor_catalog::testing::bootstrapped()?;
        assert!(scan_interval(&env, false, false, Some((9, false)), Some((5, true)))?.is_none());
        assert!(scan_interval(&env, true, true, Some((5, true)), Some((5, false)))?.is_none());
        Ok(())
    }

    #[test]
    fn null_equality_or_bound_matches_nothing() -> Result<()> {
        let (_kv, env) = pgarbor_catalog::testing::bootstrapped()?;
        let null = Datum::Null;
        let five = Datum::Int4(5);
        let cols = [KeyColumn::asc(TypeId::INT4)];
        assert!(interval_for_bounds(&env, TEST_OID, &cols, std::slice::from_ref(&null), None, None)?.is_none());
        assert!(interval_for_bounds(&env, TEST_OID, &cols, &[], Some((&null, true)), None)?.is_none());
        assert!(interval_for_bounds(&env, TEST_OID, &cols, &[], None, Some((&null, false)))?.is_none());
        assert!(interval_for_bounds(&env, TEST_OID, &cols, &[], Some((&five, true)), None)?.is_some());
        Ok(())
    }

    #[test]
    fn equality_prefix_selects_one_leading_value() -> Result<()> {
        let (_kv, env) = pgarbor_catalog::testing::bootstrapped()?;
        let cols = [KeyColumn::desc(TypeId::INT4), KeyColumn::asc(TypeId::INT4)];
        let eq = [Datum::Int4(3)];
        let five = Datum::Int4(5);
        let iv = interval_for_bounds(&env, TEST_OID, &cols, &eq, Some((&five, true)), None)?;
        for a in [None, Some(2), Some(3), Some(4)] {
            for b in [None, Some(-1), Some(4), Some(5), Some(6)] {
                let key = hand_key(TEST_OID, &[(a, true, true), (b, false, false)], Some(77));
                let expected = a == Some(3) && b.map_or(false, |x| x >= 5);
                let actual = iv.as_ref().map_or(false, |iv| in_interval(iv, &key));
                assert_eq!(actual, expected, "a={a:?} b={b:?}");
            }
        }
        Ok(())
    }

    fn int4_index(unique: bool, nulls_not_distinct: bool) -> IndexDesc {
        IndexDesc {
            oid: 16401,
            name: "t_a_idx".to_string(),
            unique,
            primary: false,
            nulls_not_distinct,
            keys: vec![IdxKeyDesc {
                column: Some(1),
                expr: None,
                ty: TypeId::INT4,
                descending: false,
                nulls_first: false,
                label: "a".to_string(),
            }],
            key_cols: vec![KeyColumn::asc(TypeId::INT4)],
            predicate: None,
            constraint: None,
        }
    }

    #[test]
    fn entry_keys_follow_unique_and_null_suffix_rules() -> Result<()> {
        let (_kv, env) = pgarbor_catalog::testing::bootstrapped()?;
        let cell = Cell::new(1);
        let guc = NoGuc;
        let services = NoServices;
        let se = SessionEnv::new(env, IoConfig::default(), &guc, &services, &cell, 0, 0);
        let mut txn = MemTxn::new();
        let mut cx = ExecContext::new(&mut txn, se);
        let mut rt = Runtime::default();
        let mut ex = Executor::new(&mut rt, &mut cx);

        // Unique, no NULL: no suffix, the key is its own probe.
        let uniq = int4_index(true, false);
        let one = make_entry(&mut ex, &uniq, 7, &[Datum::Int4(1)])?.expect("entry");
        assert_eq!(one.key, hand_key(16401, &[(Some(1), false, false)], None));
        assert_eq!(one.probe, Some(one.key.clone()));
        assert_eq!(one.values, vec![Datum::Int4(1)]);

        // Unique with NULL (NULLS DISTINCT): rowid suffix, never a probe, distinct per row.
        let null1 = make_entry(&mut ex, &uniq, 8, &[Datum::Null])?.expect("entry");
        let null2 = make_entry(&mut ex, &uniq, 9, &[Datum::Null])?.expect("entry");
        assert_eq!(null1.key, hand_key(16401, &[(None, false, false)], Some(8)));
        assert_eq!(null1.probe, None);
        assert_ne!(null1.key, null2.key);

        // Unique with NULL under NULLS NOT DISTINCT: no suffix, probe present.
        let nnd = int4_index(true, true);
        let n = make_entry(&mut ex, &nnd, 10, &[Datum::Null])?.expect("entry");
        assert_eq!(n.key, hand_key(16401, &[(None, false, false)], None));
        assert_eq!(n.probe, Some(n.key.clone()));

        // Non-unique: always suffixed, never a probe.
        let plain = int4_index(false, false);
        let p = make_entry(&mut ex, &plain, 11, &[Datum::Int4(1)])?.expect("entry");
        assert_eq!(p.key, hand_key(16401, &[(Some(1), false, false)], Some(11)));
        assert_eq!(p.probe, None);
        Ok(())
    }

    #[test]
    fn entry_value_roundtrips_rowid() -> Result<()> {
        for n in [0u64, 1, 255, 1 << 40, u64::MAX] {
            assert_eq!(rowid_of_entry_value(&entry_value(n))?, n);
        }
        assert!(rowid_of_entry_value(&[1, 2, 3]).is_err());
        Ok(())
    }
}
