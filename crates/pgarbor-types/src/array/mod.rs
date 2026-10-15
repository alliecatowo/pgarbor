//! PostgreSQL arrays (TYPE-09).
//!
//! `Array { elem, dims, items }`:
//! - `elem`: element type OID (e.g. 23 for `int4[]`). An array Datum's own type OID is the element's
//!   `typarray` (`registry::array_type_of(elem)`).
//! - `dims`: one `(length, lower_bound)` pair per dimension, outermost first. An EMPTY array has
//!   `dims == []` and `items == []` (PG: ndims 0). PG allows at most 6 dimensions (`MAXDIM`).
//! - `items`: elements in row-major order; `items.len() == product(lengths)`. Elements may be `Null`.
//!   Elements are never themselves `Datum::Array` (multi-dimensional arrays are flat + dims).
//!
//! Default lower bound is 1. `int2vector`/`oidvector` are 1-D arrays with lower bound 0.
//!
//! Files: `io.rs` (text/binary I/O), `funcs.rs` (pg_proc implementations), `subscript.rs`
//! (subscripting/slicing/assignment and ARRAY[...] construction used by pgarbor-exec).
use crate::datum::Datum;
use pgarbor_common::Oid;

pub mod funcs;
pub mod io;
pub mod subscript;

/// PostgreSQL's maximum number of array dimensions (`MAXDIM`).
pub const MAXDIM: usize = 6;

/// An array value. See module docs for invariants.
// CONTRACT
#[derive(Clone, Debug, PartialEq)]
pub struct Array {
    pub elem: Oid,
    pub dims: Vec<(i32, i32)>,
    pub items: Vec<Datum>,
}

impl Array {
    /// Empty array of the given element type (`'{}'`).
    pub fn empty(elem: Oid) -> Array {
        Array { elem, dims: Vec::new(), items: Vec::new() }
    }
    /// One-dimensional array with lower bound 1 (empty input gives the empty array).
    pub fn from_vec(elem: Oid, items: Vec<Datum>) -> Array {
        if items.is_empty() {
            return Array::empty(elem);
        }
        Array { elem, dims: vec![(items.len() as i32, 1)], items }
    }
    /// Number of dimensions (0 for an empty array).
    pub fn ndims(&self) -> usize {
        self.dims.len()
    }
    pub fn is_empty(&self) -> bool {
        self.items.is_empty()
    }
    /// Product of dimension lengths (what `items.len()` must equal).
    pub fn expected_len(&self) -> usize {
        if self.dims.is_empty() {
            return 0;
        }
        self.dims.iter().map(|(l, _)| (*l).max(0) as usize).product()
    }
    /// True if any element is NULL.
    pub fn has_nulls(&self) -> bool {
        self.items.iter().any(|d| d.is_null())
    }
    /// True if every dimension has lower bound 1 (no `[lb:ub]=` decoration on output).
    pub fn default_bounds(&self) -> bool {
        self.dims.iter().all(|(_, lb)| *lb == 1)
    }
}
