//! Array subscripting, slicing, assignment and ARRAY[...] construction (TYPE-09). Lease TYP-39.
//!
//! PG18 reference: `utils/adt/arraysubs.c` (`array_subscript_fetch`, `_fetch_slice`, `_assign`,
//! `_assign_slice`), `arrayfuncs.c` (`array_get_element`, `array_get_slice`, `array_set_element`,
//! `array_set_slice`), `executor/execExprInterp.c` (`ExecEvalArrayExpr`).
//! Used by pgarbor-exec for `a[i]`, `a[i:j]`, `a[i][j]`, `UPDATE t SET a[3] = x`, and `ARRAY[...]`.
//!
//! Semantics (oracle-verified):
//! - Fetch with N subscripts on an N-dim array: element or NULL when out of bounds (never an error):
//!   `(array[1,2,3])[5]` → NULL; `('{{1,2},{3,4}}'::int[])[2][1]` → 3. Fewer subscripts than dims →
//!   NULL (PG semantics for non-slice). NULL subscript → NULL result.
//! - Slices (`[lo:hi]`, either bound omitted = array bound): `(array[1,2,3])[2:3]` → `{2,3}` (result lower
//!   bounds reset to 1); `[1:1][1:2]` on 2-D → `{{1,2}}`; empty intersection → `{}`. If any subscript is a
//!   slice, all are treated as slices (scalar `i` means `1:i`).
//! - Assignment: 1-D arrays may be extended (gaps filled with NULL); assigning into an empty array
//!   creates it with the given lower bound; multi-dim arrays cannot be extended: out-of-range on a
//!   multi-dim array → 2202E `array subscript out of range` (PG `array_set_element`).
//!   Too many subscripts → 2202E `wrong number of array subscripts`. NULL subscript in assignment →
//!   22004 `array subscript in assignment must not be null`.
//! - ARRAY[...] construction: scalar elements → 1-D (lbound 1); sub-array elements → (N+1)-D, all
//!   sub-arrays must have identical dims, else 2202E `multidimensional arrays must have array expressions
//!   with matching dimensions`; empty sub-arrays are skipped; NULL sub-array elements are skipped as PG does.
use super::{Array, MAXDIM};
use crate::datum::{type_mismatch, Datum};
use crate::names::display_name;
use crate::registry::builtin_env;
use pgarbor_common::{Oid, PgError, Result, SqlState};

/// PostgreSQL `MaxArraySize` = `MaxAllocSize / sizeof(Datum)`.
const MAX_ARRAY_SIZE: usize = 134_217_727;

/// One subscript: `Index(i)` or `Slice(lower, upper)` with `None` = omitted bound. `None` inside the
/// Option-wrapped vector element of callers means SQL NULL subscript.
// CONTRACT
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Subscript {
    Index(i32),
    Slice(Option<i32>, Option<i32>),
}

/// `arr[subs...]` fetch. `subs` entries of `None` are NULL subscripts. Returns the element or a
/// sliced array Datum per module docs.
// CONTRACT
#[allow(unused_variables)]
pub fn fetch(arr: &Array, subs: &[Option<Subscript>]) -> Result<Datum> {
    check_depth(subs.len())?;
    if subs.iter().any(|s| s.is_none()) {
        return Ok(Datum::Null);
    }
    if is_slice(subs) {
        let b = bounds(subs)?;
        Ok(Datum::Array(Box::new(fetch_slice(arr, &b)?)))
    } else {
        fetch_element(arr, &indices(subs)?)
    }
}

/// `arr[subs...] := value` (value is an element for index assignment, an array Datum for slices).
/// `arr` = None means the target was NULL (PG treats it as an empty array).
// CONTRACT
#[allow(unused_variables)]
pub fn assign(arr: Option<&Array>, elem: Oid, subs: &[Option<Subscript>], value: Datum) -> Result<Array> {
    check_depth(subs.len())?;
    if subs.iter().any(|s| s.is_none()) {
        return Err(PgError::new(SqlState::NULL_VALUE_NOT_ALLOWED, "array subscript in assignment must not be null"));
    }
    let base = match arr {
        Some(a) => a.clone(),
        None => Array::empty(elem),
    };
    if is_slice(subs) {
        let b = bounds(subs)?;
        match value {
            // arraysubs.c / array_set_slice: a NULL replacement leaves the array unchanged.
            Datum::Null => Ok(base),
            Datum::Array(src) => assign_slice(base, &b, &src),
            other => Err(type_mismatch("array", &other)),
        }
    } else {
        assign_element(base, &indices(subs)?, value)
    }
}

/// Build `ARRAY[e1, e2, ...]` with element type `elem`. When `elements_are_arrays` the inputs are
/// array Datums (or NULL) forming a higher-dimensional array.
// CONTRACT
#[allow(unused_variables)]
pub fn construct(elem: Oid, items: Vec<Datum>, elements_are_arrays: bool) -> Result<Array> {
    if !elements_are_arrays {
        if items.is_empty() {
            return Ok(Array::empty(elem));
        }
        let n = i32::try_from(items.len()).map_err(|_| too_big())?;
        let dims = vec![(n, 1)];
        n_items(&dims)?;
        return Ok(Array { elem, dims, items });
    }

    // ExecEvalArrayExpr, multidims branch.
    let mut sub_dims: Option<Vec<(i32, i32)>> = None;
    let mut have_empty = false;
    let mut outer: usize = 0;
    let mut out: Vec<Datum> = Vec::new();
    for item in items {
        let a: Array = match item {
            // NULL sub-arrays are ignored (flagged as "empty").
            Datum::Null => {
                have_empty = true;
                continue;
            }
            Datum::Array(a) => *a,
            other => return Err(type_mismatch("array", &other)),
        };
        if a.elem != elem {
            let detail = format!(
                "Array with element type {} cannot be included in ARRAY construct with element type {}.",
                display_name(builtin_env(), a.elem),
                display_name(builtin_env(), elem)
            );
            return Err(PgError::new(SqlState::DATATYPE_MISMATCH, "cannot merge incompatible arrays").with_detail(detail));
        }
        // Zero-dimensional sub-arrays are ignored (flagged as "empty").
        if a.dims.is_empty() {
            have_empty = true;
            continue;
        }
        if sub_dims.is_none() {
            check_depth(a.dims.len() + 1)?;
            sub_dims = Some(a.dims.clone());
        } else if let Some(d) = &sub_dims {
            if *d != a.dims {
                return Err(subscript_err(
                    "multidimensional arrays must have array expressions with matching dimensions",
                ));
            }
        }
        out.extend(a.items);
        outer += 1;
    }

    // Mixing empty/NULL sub-arrays with non-empty ones is an error; all-empty gives an empty array.
    if have_empty {
        if sub_dims.is_none() {
            return Ok(Array::empty(elem));
        }
        return Err(subscript_err("multidimensional arrays must have array expressions with matching dimensions"));
    }
    let inner = match sub_dims {
        Some(d) => d,
        None => return Ok(Array::empty(elem)),
    };
    let mut dims = Vec::with_capacity(inner.len() + 1);
    dims.push((i32::try_from(outer).map_err(|_| too_big())?, 1));
    dims.extend(inner);
    n_items(&dims)?;
    Ok(Array { elem, dims, items: out })
}

// ---------------------------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------------------------

fn too_big() -> PgError {
    PgError::new(
        SqlState::PROGRAM_LIMIT_EXCEEDED,
        format!("array size exceeds the maximum allowed ({MAX_ARRAY_SIZE})"),
    )
}

fn lower_too_large(lb: i32) -> PgError {
    PgError::new(SqlState::PROGRAM_LIMIT_EXCEEDED, format!("array lower bound is too large: {lb}"))
}

fn subscript_err(msg: &'static str) -> PgError {
    PgError::new(SqlState::ARRAY_SUBSCRIPT_ERROR, msg)
}

/// Parser-level limit on subscript / dimension counts (`MAXDIM`).
fn check_depth(ndims: usize) -> Result<()> {
    if ndims > MAXDIM {
        return Err(PgError::new(
            SqlState::PROGRAM_LIMIT_EXCEEDED,
            format!("number of array dimensions ({ndims}) exceeds the maximum allowed ({MAXDIM})"),
        ));
    }
    Ok(())
}

/// `ArrayGetNItems` + `ArrayCheckBounds`: element count of `dims`, rejecting negative lengths,
/// lower-bound overflow and totals above `MaxArraySize`. Empty `dims` gives 0.
fn n_items(dims: &[(i32, i32)]) -> Result<usize> {
    if dims.is_empty() {
        return Ok(0);
    }
    let mut total: usize = 1;
    for &(len, lb) in dims {
        if len < 0 {
            return Err(too_big());
        }
        if lb.checked_add(len).is_none() {
            return Err(lower_too_large(lb));
        }
        total = total.checked_mul(len as usize).ok_or_else(too_big)?;
        if total > MAX_ARRAY_SIZE {
            return Err(too_big());
        }
    }
    Ok(total)
}

fn is_slice(subs: &[Option<Subscript>]) -> bool {
    subs.iter().any(|s| matches!(s, Some(Subscript::Slice(..))))
}

/// Scalar subscripts of a non-slice list. Callers have already rejected NULL entries.
fn indices(subs: &[Option<Subscript>]) -> Result<Vec<i32>> {
    subs.iter()
        .map(|s| match s {
            Some(Subscript::Index(i)) => Ok(*i),
            _ => Err(PgError::internal("array subscript list is not a set of scalar indexes")),
        })
        .collect()
}

/// Bounds of a slice list. In slice mode a scalar `i` means `1:i` (arraysubs.c transform).
fn bounds(subs: &[Option<Subscript>]) -> Result<Vec<(Option<i32>, Option<i32>)>> {
    subs.iter()
        .map(|s| match s {
            Some(Subscript::Index(i)) => Ok((Some(1), Some(*i))),
            Some(Subscript::Slice(lo, hi)) => Ok((*lo, *hi)),
            None => Err(PgError::internal("array subscript is NULL after NULL check")),
        })
        .collect()
}

/// Row-major offset of `idx` inside `dims`. Callers guarantee `idx.len() == dims.len()` and that
/// every index is inside its dimension.
fn offset(dims: &[(i32, i32)], idx: &[i32]) -> usize {
    let mut off: i64 = 0;
    for (k, &i) in idx.iter().enumerate() {
        let (len, lb) = dims[k];
        off = off * len as i64 + (i as i64 - lb as i64);
    }
    off as usize
}

/// Element `off` of `arr`, cloned.
fn item(arr: &Array, off: usize) -> Result<Datum> {
    arr.items
        .get(off)
        .cloned()
        .ok_or_else(|| PgError::internal("array item count does not match its dimensions"))
}

/// Row-major offsets of every index tuple in `[lo, hi]` (inclusive, `lo <= hi` per dimension).
fn range_offsets(dims: &[(i32, i32)], lo: &[i32], hi: &[i32]) -> Vec<usize> {
    let ndim = dims.len();
    let mut total: usize = 1;
    for k in 0..ndim {
        total *= (hi[k] as i64 - lo[k] as i64 + 1) as usize;
    }
    let mut out = Vec::with_capacity(total);
    if ndim == 0 || total == 0 {
        return out;
    }
    let mut cur = lo.to_vec();
    loop {
        out.push(offset(dims, &cur));
        // Advance the odometer, last dimension fastest.
        let mut k = ndim;
        loop {
            if k == 0 {
                return out;
            }
            k -= 1;
            if cur[k] < hi[k] {
                cur[k] += 1;
                break;
            }
            cur[k] = lo[k];
        }
    }
}

// ---------------------------------------------------------------------------------------------
// Fetch
// ---------------------------------------------------------------------------------------------

/// `array_get_element`: NULL unless the subscript count equals the dimension count and every
/// index is in bounds.
fn fetch_element(arr: &Array, idx: &[i32]) -> Result<Datum> {
    let ndim = arr.dims.len();
    if ndim == 0 || ndim != idx.len() {
        return Ok(Datum::Null);
    }
    for (&i, &(len, lb)) in idx.iter().zip(&arr.dims) {
        if (i as i64) < lb as i64 || (i as i64) >= lb as i64 + len as i64 {
            return Ok(Datum::Null);
        }
    }
    item(arr, offset(&arr.dims, idx))
}

/// Clamp each slice bound to the array (`array_get_slice`). Returns `None` when the result is empty.
/// Omitted bounds, or bounds outside the array, are replaced by the array's bounds.
fn clamp_bounds(dims: &[(i32, i32)], bounds: &[(Option<i32>, Option<i32>)]) -> Option<(Vec<i32>, Vec<i32>)> {
    let mut lo = Vec::with_capacity(dims.len());
    let mut hi = Vec::with_capacity(dims.len());
    for (k, &(len, lb)) in dims.iter().enumerate() {
        let lb64 = lb as i64;
        let top = lb64 + len as i64 - 1;
        let (blo, bhi) = bounds.get(k).copied().unwrap_or((None, None));
        let l = match blo {
            Some(v) if v as i64 >= lb64 => v as i64,
            _ => lb64,
        };
        let h = match bhi {
            Some(v) if (v as i64) < top => v as i64,
            _ => top,
        };
        if l > h {
            return None;
        }
        lo.push(l as i32);
        hi.push(h as i32);
    }
    Some((lo, hi))
}

/// `array_get_slice`: always an array; the result has lower bounds reset to 1.
fn fetch_slice(arr: &Array, bounds: &[(Option<i32>, Option<i32>)]) -> Result<Array> {
    let ndim = arr.dims.len();
    if ndim == 0 || ndim < bounds.len() {
        return Ok(Array::empty(arr.elem));
    }
    let (lo, hi) = match clamp_bounds(&arr.dims, bounds) {
        Some(v) => v,
        None => return Ok(Array::empty(arr.elem)),
    };
    let offs = range_offsets(&arr.dims, &lo, &hi);
    let mut items = Vec::with_capacity(offs.len());
    for o in offs {
        items.push(item(arr, o)?);
    }
    let dims: Vec<(i32, i32)> = lo
        .iter()
        .zip(&hi)
        .map(|(&l, &h)| ((h as i64 - l as i64 + 1) as i32, 1))
        .collect();
    Ok(Array { elem: arr.elem, dims, items })
}

// ---------------------------------------------------------------------------------------------
// Assignment
// ---------------------------------------------------------------------------------------------

/// `array_set_element`.
fn assign_element(base: Array, idx: &[i32], value: Datum) -> Result<Array> {
    if idx.is_empty() {
        return Err(subscript_err("wrong number of array subscripts"));
    }
    let Array { elem, mut dims, items: old } = base;

    // Empty target: one element per dimension, lower bounds taken from the subscripts.
    if dims.is_empty() {
        let new_dims: Vec<(i32, i32)> = idx.iter().map(|&i| (1, i)).collect();
        n_items(&new_dims)?;
        return Ok(Array { elem, dims: new_dims, items: vec![value] });
    }
    if dims.len() != idx.len() {
        return Err(subscript_err("wrong number of array subscripts"));
    }

    if dims.len() == 1 {
        // 1-D arrays grow in either direction; the gap is filled with NULLs.
        let (len, lb) = dims[0];
        let i = idx[0] as i64;
        let lb64 = lb as i64;
        let top = lb64 + len as i64;
        let new_lb = if i < lb64 { i } else { lb64 };
        let new_top = if i >= top { i + 1 } else { top };
        let new_len = new_top - new_lb;
        if new_len > MAX_ARRAY_SIZE as i64 {
            return Err(too_big());
        }
        dims[0] = (new_len as i32, new_lb as i32);
        n_items(&dims)?;
        let mut items = vec![Datum::Null; new_len as usize];
        let shift = (lb64 - new_lb) as usize;
        for (k, d) in old.into_iter().enumerate() {
            let slot = items
                .get_mut(shift + k)
                .ok_or_else(|| PgError::internal("array item count does not match its dimensions"))?;
            *slot = d;
        }
        let at = (i - new_lb) as usize;
        let slot = items
            .get_mut(at)
            .ok_or_else(|| PgError::internal("array assignment index outside extended bounds"))?;
        *slot = value;
        return Ok(Array { elem, dims, items });
    }

    // Multi-dim: no extension allowed.
    for (&i, &(len, lb)) in idx.iter().zip(&dims) {
        if (i as i64) < lb as i64 || (i as i64) >= lb as i64 + len as i64 {
            return Err(subscript_err("array subscript out of range"));
        }
    }
    let off = offset(&dims, idx);
    let mut items = old;
    let slot = items
        .get_mut(off)
        .ok_or_else(|| PgError::internal("array item count does not match its dimensions"))?;
    *slot = value;
    Ok(Array { elem, dims, items })
}

/// `array_set_slice`. Only the first `span` source items are used, in row-major order (the source
/// dimensions are not checked, as in PG).
fn assign_slice(base: Array, bounds: &[(Option<i32>, Option<i32>)], src: &Array) -> Result<Array> {
    let Array { elem, dims, items: old } = base;

    // Empty target: both boundaries must be given for every subscript.
    if dims.is_empty() {
        let mut new_dims = Vec::with_capacity(bounds.len());
        for &(blo, bhi) in bounds {
            let (lo, hi) = match (blo, bhi) {
                (Some(l), Some(h)) => (l as i64, h as i64),
                _ => {
                    return Err(subscript_err("array slice subscript must provide both boundaries")
                        .with_detail(
                            "When assigning to a slice of an empty array value, slice boundaries must be fully specified.",
                        ))
                }
            };
            let d = hi - lo + 1;
            if d < 0 || d > i32::MAX as i64 {
                return Err(too_big());
            }
            new_dims.push((d as i32, lo as i32));
        }
        let total = n_items(&new_dims)?;
        if src.items.len() < total {
            return Err(subscript_err("source array too small"));
        }
        if total == 0 {
            return Ok(Array::empty(elem));
        }
        let items: Vec<Datum> = src.items.iter().take(total).cloned().collect();
        return Ok(Array { elem, dims: new_dims, items });
    }

    let ndim = dims.len();
    if ndim < bounds.len() {
        return Err(subscript_err("wrong number of array subscripts"));
    }

    if ndim == 1 {
        let (len, lb) = dims[0];
        let (blo, bhi) = bounds.first().copied().unwrap_or((None, None));
        let lb64 = lb as i64;
        let top = lb64 + len as i64; // exclusive
        let lo = blo.map_or(lb64, |v| v as i64);
        let hi = bhi.map_or(top - 1, |v| v as i64);
        if lo > hi {
            return Err(subscript_err("upper bound cannot be less than lower bound"));
        }
        let new_lb = lo.min(lb64);
        let new_top = (hi + 1).max(top);
        let new_len = new_top - new_lb;
        if new_len > MAX_ARRAY_SIZE as i64 {
            return Err(too_big());
        }
        let span = (hi - lo + 1) as usize;
        if src.items.len() < span {
            return Err(subscript_err("source array too small"));
        }
        let new_dims = vec![(new_len as i32, new_lb as i32)];
        n_items(&new_dims)?;

        let mut items = vec![Datum::Null; new_len as usize];
        let shift = (lb64 - new_lb) as usize;
        for (k, d) in old.into_iter().enumerate() {
            let slot = items
                .get_mut(shift + k)
                .ok_or_else(|| PgError::internal("array item count does not match its dimensions"))?;
            *slot = d;
        }
        let at = (lo - new_lb) as usize;
        for (k, d) in src.items.iter().take(span).enumerate() {
            let slot = items
                .get_mut(at + k)
                .ok_or_else(|| PgError::internal("array slice assignment outside extended bounds"))?;
            *slot = d.clone();
        }
        return Ok(Array { elem, dims: new_dims, items });
    }

    // Multi-dim: the slice must lie inside the array; omitted bounds are the array bounds.
    let mut lo = Vec::with_capacity(ndim);
    let mut hi = Vec::with_capacity(ndim);
    for (k, &(len, lb)) in dims.iter().enumerate() {
        let lb64 = lb as i64;
        let top = lb64 + len as i64; // exclusive
        let (blo, bhi) = bounds.get(k).copied().unwrap_or((None, None));
        let l = blo.map_or(lb64, |v| v as i64);
        let h = bhi.map_or(top - 1, |v| v as i64);
        if l > h {
            return Err(subscript_err("upper bound cannot be less than lower bound"));
        }
        if l < lb64 || h >= top {
            return Err(subscript_err("array subscript out of range"));
        }
        lo.push(l as i32);
        hi.push(h as i32);
    }
    let span: usize = lo.iter().zip(&hi).map(|(&l, &h)| (h as i64 - l as i64 + 1) as usize).product();
    if src.items.len() < span {
        return Err(subscript_err("source array too small"));
    }
    let offs = range_offsets(&dims, &lo, &hi);
    let mut items = old;
    for (j, off) in offs.into_iter().enumerate() {
        let v = src
            .items
            .get(j)
            .cloned()
            .ok_or_else(|| PgError::internal("array slice source shorter than its span"))?;
        let slot = items
            .get_mut(off)
            .ok_or_else(|| PgError::internal("array item count does not match its dimensions"))?;
        *slot = v;
    }
    Ok(Array { elem, dims, items })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::oids;

    fn ints(v: &[i32]) -> Vec<Datum> {
        v.iter().map(|&x| Datum::Int4(x)).collect()
    }
    fn arr(dims: Vec<(i32, i32)>, items: &[i32]) -> Array {
        Array { elem: oids::INT4, dims, items: ints(items) }
    }
    fn sub(i: i32) -> Option<Subscript> {
        Some(Subscript::Index(i))
    }
    fn sl(lo: Option<i32>, hi: Option<i32>) -> Option<Subscript> {
        Some(Subscript::Slice(lo, hi))
    }
    fn code(r: Result<Array>) -> &'static str {
        let e = r.expect_err("expected an error");
        match e.sqlstate.code() {
            "22004" => "22004",
            "2202E" => "2202E",
            _ => "other",
        }
    }

    #[test]
    fn fetch_out_of_range_is_null() {
        let a = arr(vec![(3, 1)], &[1, 2, 3]);
        assert_eq!(fetch(&a, &[sub(5)]).unwrap(), Datum::Null);
        assert_eq!(fetch(&a, &[sub(0)]).unwrap(), Datum::Null);
        assert_eq!(fetch(&a, &[sub(2)]).unwrap(), Datum::Int4(2));
    }

    #[test]
    fn fetch_null_subscript_is_null() {
        let a = arr(vec![(3, 1)], &[1, 2, 3]);
        assert_eq!(fetch(&a, &[None]).unwrap(), Datum::Null);
    }

    #[test]
    fn fetch_fewer_subscripts_is_null() {
        let a = arr(vec![(2, 1), (2, 1)], &[1, 2, 3, 4]);
        assert_eq!(fetch(&a, &[sub(1)]).unwrap(), Datum::Null);
    }

    #[test]
    fn fetch_2d_element() {
        let a = arr(vec![(2, 1), (2, 1)], &[1, 2, 3, 4]);
        assert_eq!(fetch(&a, &[sub(2), sub(1)]).unwrap(), Datum::Int4(3));
    }

    #[test]
    fn fetch_1d_slice() {
        let a = arr(vec![(3, 1)], &[1, 2, 3]);
        let got = fetch(&a, &[sl(Some(2), Some(3))]).unwrap();
        assert_eq!(got, Datum::Array(Box::new(arr(vec![(2, 1)], &[2, 3]))));
    }

    #[test]
    fn fetch_1d_slice_omitted_bounds_and_empty() {
        let a = arr(vec![(3, 1)], &[1, 2, 3]);
        let got = fetch(&a, &[sl(None, Some(2))]).unwrap();
        assert_eq!(got, Datum::Array(Box::new(arr(vec![(2, 1)], &[1, 2]))));
        let empty = fetch(&a, &[sl(Some(5), Some(7))]).unwrap();
        assert_eq!(empty, Datum::Array(Box::new(Array::empty(oids::INT4))));
    }

    #[test]
    fn fetch_2d_slice() {
        let a = arr(vec![(2, 1), (2, 1)], &[1, 2, 3, 4]);
        let got = fetch(&a, &[sl(Some(1), Some(1)), sl(Some(1), Some(2))]).unwrap();
        assert_eq!(got, Datum::Array(Box::new(arr(vec![(1, 1), (2, 1)], &[1, 2]))));
    }

    #[test]
    fn scalar_in_slice_mode_means_one_to_i() {
        // 2-D {{1,2},{3,4}}: [2:2][2] in slice mode is [2:2][1:2] = {{3,4}}.
        let a = arr(vec![(2, 1), (2, 1)], &[1, 2, 3, 4]);
        let got = fetch(&a, &[sl(Some(2), Some(2)), sub(2)]).unwrap();
        assert_eq!(got, Datum::Array(Box::new(arr(vec![(1, 1), (2, 1)], &[3, 4]))));
    }

    #[test]
    fn assign_beyond_end_fills_nulls() {
        let a = arr(vec![(2, 1)], &[1, 2]);
        let r = assign(Some(&a), oids::INT4, &[sub(4)], Datum::Int4(9)).unwrap();
        assert_eq!(r.dims, vec![(4, 1)]);
        assert_eq!(r.items, vec![Datum::Int4(1), Datum::Int4(2), Datum::Null, Datum::Int4(9)]);
    }

    #[test]
    fn assign_before_start_extends_lower_bound() {
        let a = arr(vec![(2, 3)], &[1, 2]);
        let r = assign(Some(&a), oids::INT4, &[sub(1)], Datum::Int4(9)).unwrap();
        assert_eq!(r.dims, vec![(4, 1)]);
        assert_eq!(r.items, vec![Datum::Int4(9), Datum::Null, Datum::Int4(1), Datum::Int4(2)]);
    }

    #[test]
    fn assign_into_null_array_creates_it_with_lower_bound() {
        let r = assign(None, oids::INT4, &[sub(0)], Datum::Int4(5)).unwrap();
        assert_eq!(r.dims, vec![(1, 0)]);
        assert_eq!(r.items, vec![Datum::Int4(5)]);
    }

    #[test]
    fn assign_null_subscript_is_22004() {
        assert_eq!(code(assign(None, oids::INT4, &[None], Datum::Int4(1))), "22004");
    }

    #[test]
    fn assign_2d_out_of_range_is_2202e() {
        let a = arr(vec![(2, 1), (2, 1)], &[1, 2, 3, 4]);
        assert_eq!(code(assign(Some(&a), oids::INT4, &[sub(3), sub(1)], Datum::Int4(9))), "2202E");
    }

    #[test]
    fn assign_wrong_subscript_count_is_2202e() {
        let a = arr(vec![(2, 1)], &[1, 2]);
        assert_eq!(code(assign(Some(&a), oids::INT4, &[sub(1), sub(1)], Datum::Int4(9))), "2202E");
    }

    #[test]
    fn assign_2d_element() {
        let a = arr(vec![(2, 1), (2, 1)], &[1, 2, 3, 4]);
        let r = assign(Some(&a), oids::INT4, &[sub(2), sub(2)], Datum::Int4(9)).unwrap();
        assert_eq!(r.items, ints(&[1, 2, 3, 9]));
    }

    #[test]
    fn assign_1d_slice_replaces_range() {
        let a = arr(vec![(4, 1)], &[1, 2, 3, 4]);
        let src = Datum::Array(Box::new(arr(vec![(2, 1)], &[7, 8])));
        let r = assign(Some(&a), oids::INT4, &[sl(Some(2), Some(3))], src).unwrap();
        assert_eq!(r.items, ints(&[1, 7, 8, 4]));
    }

    #[test]
    fn assign_null_slice_value_leaves_array_unchanged() {
        let a = arr(vec![(2, 1)], &[1, 2]);
        let r = assign(Some(&a), oids::INT4, &[sl(Some(1), Some(1))], Datum::Null).unwrap();
        assert_eq!(r, a);
    }

    #[test]
    fn assign_2d_slice_replaces_block() {
        let a = arr(vec![(2, 1), (2, 1)], &[1, 2, 3, 4]);
        let src = Datum::Array(Box::new(arr(vec![(1, 1), (2, 1)], &[9, 9])));
        let r = assign(Some(&a), oids::INT4, &[sl(Some(1), Some(1)), sl(Some(1), Some(2))], src).unwrap();
        assert_eq!(r.items, ints(&[9, 9, 3, 4]));
    }

    #[test]
    fn assign_slice_into_null_requires_both_bounds() {
        let src = Datum::Array(Box::new(arr(vec![(2, 1)], &[7, 8])));
        assert_eq!(code(assign(None, oids::INT4, &[sl(None, Some(2))], src.clone())), "2202E");
        let r = assign(None, oids::INT4, &[sl(Some(1), Some(2))], src).unwrap();
        assert_eq!(r.dims, vec![(2, 1)]);
        assert_eq!(r.items, ints(&[7, 8]));
    }

    #[test]
    fn assign_slice_source_too_small() {
        let a = arr(vec![(4, 1)], &[1, 2, 3, 4]);
        let src = Datum::Array(Box::new(arr(vec![(1, 1)], &[7])));
        assert_eq!(code(assign(Some(&a), oids::INT4, &[sl(Some(1), Some(2))], src)), "2202E");
    }

    #[test]
    fn construct_scalars_is_1d() {
        let r = construct(oids::INT4, ints(&[1, 2]), false).unwrap();
        assert_eq!(r, arr(vec![(2, 1)], &[1, 2]));
        assert_eq!(construct(oids::INT4, vec![], false).unwrap(), Array::empty(oids::INT4));
    }

    #[test]
    fn construct_nested_is_2d() {
        let items = vec![
            Datum::Array(Box::new(arr(vec![(2, 1)], &[1, 2]))),
            Datum::Array(Box::new(arr(vec![(2, 1)], &[3, 4]))),
        ];
        let r = construct(oids::INT4, items, true).unwrap();
        assert_eq!(r, arr(vec![(2, 1), (2, 1)], &[1, 2, 3, 4]));
    }

    #[test]
    fn construct_mismatched_sub_arrays_is_2202e() {
        let items = vec![
            Datum::Array(Box::new(arr(vec![(2, 1)], &[1, 2]))),
            Datum::Array(Box::new(arr(vec![(1, 1)], &[3]))),
        ];
        let e = construct(oids::INT4, items, true).expect_err("expected 2202E");
        assert_eq!(e.sqlstate.code(), "2202E");
        assert_eq!(
            e.message,
            "multidimensional arrays must have array expressions with matching dimensions"
        );
    }

    #[test]
    fn construct_skips_empty_and_null_sub_arrays_when_all_empty() {
        let items = vec![Datum::Null, Datum::Array(Box::new(Array::empty(oids::INT4)))];
        assert_eq!(construct(oids::INT4, items, true).unwrap(), Array::empty(oids::INT4));
    }

    #[test]
    fn construct_mixing_empty_and_nonempty_is_2202e() {
        let items = vec![Datum::Null, Datum::Array(Box::new(arr(vec![(1, 1)], &[3])))];
        let e = construct(oids::INT4, items, true).expect_err("expected 2202E");
        assert_eq!(e.sqlstate.code(), "2202E");
    }
}
