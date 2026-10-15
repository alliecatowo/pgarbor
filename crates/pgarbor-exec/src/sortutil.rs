//! Row/key comparison and sorting helpers shared by Sort, Aggregate (ORDER BY inside aggregates),
//! Window, MergeJoin, SetOp and DISTINCT (lease EXE-22).
//!
//! * [`SortKeyRt`]: a resolved sort key: expression (already executable), type, direction, nulls
//!   placement and the optional `USING` operator. Built from `SortKeySpec` by [`SortKeyRt::from_spec`].
//! * [`compare_keys`]: lexicographic comparison of two evaluated key vectors. For each key:
//!   `using_op == None` -> `compare::cmp_sort(env, ty, a, b, descending, nulls_first)`; with `USING <op>`
//!   ordering by the operator: the operator must be a btree `<` (ascending semantics: `a < b` => Less) or
//!   `>` (descending semantics: `a > b` => Less); evaluate `op(a, b)` via `eval_call::call_operator`
//!   (NULL handling still by `nulls_first`); equal = neither `op(a,b)` nor `op(b,a)`. Collation is
//!   ignored (code point order, ARCHITECTURE §0.6).
//! * [`sort_rows`]: sort `rows` in place with keys evaluated ONCE per row (decorate-sort-undecorate:
//!   evaluate `keys[i].expr` over each row up front), STABLE (ties keep input order; PG does not
//!   promise stability but determinism helps tests). Errors from comparisons propagate (collect the
//!   first error in a cell and return it after sorting).
//! * [`TopN`]: bounded collection for `Sort { limit: Some(n) }`: keeps the best `n` rows by the keys
//!   (binary heap or periodic truncation), `into_sorted()` returns them ordered; `n == 0` keeps nothing.
//! * [`row_key_bytes`]: `keycodec::encode_key_vec` over selected slots of a row with ascending
//!   NULLS-equal semantics: equal SQL values (NULL = NULL) <=> equal bytes; used as hash keys by
//!   DISTINCT, set operations, aggregation, hash join.
use pgarbor_common::{PgError, Result};
use pgarbor_plan::plan::SortKeySpec;
use pgarbor_sql::ir::{BoundExpr, OpRef};
use pgarbor_types::{Datum, TypeId};
use std::cmp::Ordering;

use crate::runtime::{Executor, Row};

/// A resolved sort key (module docs).
// CONTRACT
#[derive(Clone, Debug)]
pub struct SortKeyRt {
    pub expr: BoundExpr,
    pub ty: TypeId,
    pub descending: bool,
    pub nulls_first: bool,
    pub using_op: Option<OpRef>,
}

impl SortKeyRt {
    pub fn from_spec(spec: &SortKeySpec) -> SortKeyRt {
        SortKeyRt {
            expr: spec.expr.clone(),
            ty: spec.expr.ty,
            descending: spec.descending,
            nulls_first: spec.nulls_first,
            using_op: spec.using_op.clone(),
        }
    }
}

/// Compare two evaluated key vectors (`a.len() == b.len() == keys.len()`).
#[allow(unused_variables)]
pub fn compare_keys(ex: &mut Executor, keys: &[SortKeyRt], a: &[Datum], b: &[Datum]) -> Result<Ordering> {
    Err(PgError::unimplemented("EXE-22", "sort key comparison (sortutil::compare_keys)"))
}

/// Evaluate all sort keys of one row (`eval` of each `keys[i].expr` over `row`).
#[allow(unused_variables)]
pub fn eval_keys(ex: &mut Executor, keys: &[SortKeyRt], row: &[Datum]) -> Result<Vec<Datum>> {
    Err(PgError::unimplemented("EXE-22", "sort key evaluation (sortutil::eval_keys)"))
}

/// Stable in-memory sort (module docs).
#[allow(unused_variables)]
pub fn sort_rows(ex: &mut Executor, keys: &[SortKeyRt], rows: &mut Vec<Row>) -> Result<()> {
    Err(PgError::unimplemented("EXE-22", "row sorting (sortutil::sort_rows)"))
}

/// Bounded best-N collector (module docs).
// CONTRACT
#[derive(Debug)]
pub struct TopN {
    pub limit: usize,
    /// `(evaluated key vector, row)` kept so far (unordered until `into_sorted`).
    pub items: Vec<(Vec<Datum>, Row)>,
}

impl TopN {
    pub fn new(limit: usize) -> TopN {
        TopN { limit, items: Vec::new() }
    }

    /// Offer a row with its evaluated keys.
    #[allow(unused_variables)]
    pub fn push(&mut self, ex: &mut Executor, keys: &[SortKeyRt], key_values: Vec<Datum>, row: Row) -> Result<()> {
        Err(PgError::unimplemented("EXE-22", "top-N collection (sortutil::TopN::push)"))
    }

    /// The kept rows ordered by the keys.
    #[allow(unused_variables)]
    pub fn into_sorted(self, ex: &mut Executor, keys: &[SortKeyRt]) -> Result<Vec<Row>> {
        Err(PgError::unimplemented("EXE-22", "top-N result (sortutil::TopN::into_sorted)"))
    }
}

/// Memcomparable equality key over the given slots of `row` (module docs). `types[i]` is the type of
/// slot `slots[i]`.
#[allow(unused_variables)]
pub fn row_key_bytes(ex: &Executor, types: &[TypeId], slots: &[usize], row: &[Datum]) -> Result<Vec<u8>> {
    Err(PgError::unimplemented("EXE-22", "row key bytes (sortutil::row_key_bytes)"))
}
