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
use super::Array;
use crate::datum::Datum;
use pgarbor_common::{Oid, PgError, Result};

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
    Err(PgError::unimplemented("TYP-39", "array subscripting"))
}

/// `arr[subs...] := value` (value is an element for index assignment, an array Datum for slices).
/// `arr` = None means the target was NULL (PG treats it as an empty array).
// CONTRACT
#[allow(unused_variables)]
pub fn assign(arr: Option<&Array>, elem: Oid, subs: &[Option<Subscript>], value: Datum) -> Result<Array> {
    Err(PgError::unimplemented("TYP-39", "array subscript assignment"))
}

/// Build `ARRAY[e1, e2, ...]` with element type `elem`. When `elements_are_arrays` the inputs are
/// array Datums (or NULL) forming a higher-dimensional array.
// CONTRACT
#[allow(unused_variables)]
pub fn construct(elem: Oid, items: Vec<Datum>, elements_are_arrays: bool) -> Result<Array> {
    Err(PgError::unimplemented("TYP-39", "ARRAY constructor"))
}
