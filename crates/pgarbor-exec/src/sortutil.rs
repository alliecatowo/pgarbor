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
use pgarbor_common::{PgError, Result, SqlState};
use pgarbor_plan::plan::SortKeySpec;
use pgarbor_sql::ir::{BoundExpr, OpRef};
use pgarbor_types::compare::cmp_sort;
use pgarbor_types::keycodec::{encode_key_vec, KeyColumn};
use pgarbor_types::{Datum, TypeId};
use std::cell::RefCell;
use std::cmp::Ordering;

use crate::eval::eval;
use crate::eval_call::call_operator;
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

/// A key vector paired with its row, the unit that sorting moves around.
type KeyedRow = (Vec<Datum>, Row);

/// `ORDER BY ... USING op` accepts only btree `<` and `>` (PG parse_clause.c, 42809).
fn check_ordering_op(name: &str) -> Result<()> {
    if name == "<" || name == ">" {
        return Ok(());
    }
    Err(PgError::new(
        SqlState::WRONG_OBJECT_TYPE,
        format!("operator {name} is not a valid ordering operator"),
    )
    .with_hint("Ordering operators must be \"<\" or \">\" members of btree operator families."))
}

/// Evaluate `op(a, b)` and report whether it is TRUE (NULL counts as not true).
fn operator_is_true(ex: &mut Executor, op: &OpRef, ty: TypeId, a: &Datum, b: &Datum) -> Result<bool> {
    let args = [a.clone(), b.clone()];
    let v = call_operator(ex, op, &args, &[ty, ty], TypeId::BOOL)?;
    Ok(v.as_bool() == Some(true))
}

/// One key compared with a `USING` operator. NULLs are placed by `nulls_first` before the operator is
/// consulted; `DESC` is implied by the operator itself (`>`), so `descending` is not read here.
fn using_compare(ex: &mut Executor, op: &OpRef, ty: TypeId, a: &Datum, b: &Datum, nulls_first: bool) -> Result<Ordering> {
    match (a.is_null(), b.is_null()) {
        (true, true) => return Ok(Ordering::Equal),
        (true, false) => return Ok(if nulls_first { Ordering::Less } else { Ordering::Greater }),
        (false, true) => return Ok(if nulls_first { Ordering::Greater } else { Ordering::Less }),
        (false, false) => {}
    }
    if operator_is_true(ex, op, ty, a, b)? {
        return Ok(Ordering::Less);
    }
    if operator_is_true(ex, op, ty, b, a)? {
        return Ok(Ordering::Greater);
    }
    Ok(Ordering::Equal)
}

/// Compare two evaluated key vectors (`a.len() == b.len() == keys.len()`).
pub fn compare_keys(ex: &mut Executor, keys: &[SortKeyRt], a: &[Datum], b: &[Datum]) -> Result<Ordering> {
    for (i, key) in keys.iter().enumerate() {
        let (x, y) = match (a.get(i), b.get(i)) {
            (Some(x), Some(y)) => (x, y),
            _ => return Err(PgError::internal("sort key vectors have different lengths")),
        };
        let ord = match &key.using_op {
            None => cmp_sort(&ex.se().env, key.ty, x, y, key.descending, key.nulls_first)?,
            Some(op) => {
                check_ordering_op(&op.name)?;
                using_compare(ex, op, key.ty, x, y, key.nulls_first)?
            }
        };
        if ord != Ordering::Equal {
            return Ok(ord);
        }
    }
    Ok(Ordering::Equal)
}

/// Evaluate all sort keys of one row (`eval` of each `keys[i].expr` over `row`).
pub fn eval_keys(ex: &mut Executor, keys: &[SortKeyRt], row: &[Datum]) -> Result<Vec<Datum>> {
    let mut out = Vec::with_capacity(keys.len());
    for key in keys {
        out.push(eval(ex, &key.expr, row)?);
    }
    Ok(out)
}

/// Stable sort of keyed rows. The comparator stores the first error and reports Equal afterwards, so
/// the sort finishes; the error is returned once the sort is done.
fn sort_pairs(ex: &mut Executor, keys: &[SortKeyRt], pairs: &mut [KeyedRow]) -> Result<()> {
    let first_err: RefCell<Option<PgError>> = RefCell::new(None);
    pairs.sort_by(|x, y| {
        let failed = first_err.borrow().is_some();
        if failed {
            return Ordering::Equal;
        }
        match compare_keys(&mut *ex, keys, &x.0, &y.0) {
            Ok(o) => o,
            Err(e) => {
                *first_err.borrow_mut() = Some(e);
                Ordering::Equal
            }
        }
    });
    match first_err.into_inner() {
        Some(e) => Err(e),
        None => Ok(()),
    }
}

/// Stable in-memory sort (module docs). Keys are evaluated once per row; on error `rows` is left
/// holding the same rows (in unspecified order).
pub fn sort_rows(ex: &mut Executor, keys: &[SortKeyRt], rows: &mut Vec<Row>) -> Result<()> {
    let mut key_vectors: Vec<Vec<Datum>> = Vec::with_capacity(rows.len());
    for row in rows.iter() {
        key_vectors.push(eval_keys(&mut *ex, keys, row)?);
    }
    let taken = std::mem::take(rows);
    let mut pairs: Vec<KeyedRow> = key_vectors.into_iter().zip(taken).collect();
    let result = sort_pairs(ex, keys, &mut pairs);
    rows.extend(pairs.into_iter().map(|(_, row)| row));
    result
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
    ///
    /// The buffer is trimmed to the best `limit` rows whenever it reaches `2 * limit` entries. The trim
    /// is a stable sort, and rows that arrived earlier stay ahead of later rows with equal keys, so the
    /// result equals the first `limit` rows of a full stable sort.
    pub fn push(&mut self, ex: &mut Executor, keys: &[SortKeyRt], key_values: Vec<Datum>, row: Row) -> Result<()> {
        if self.limit == 0 {
            return Ok(());
        }
        self.items.push((key_values, row));
        if self.items.len() >= self.limit.saturating_mul(2) {
            sort_pairs(ex, keys, &mut self.items)?;
            self.items.truncate(self.limit);
        }
        Ok(())
    }

    /// The kept rows ordered by the keys.
    pub fn into_sorted(self, ex: &mut Executor, keys: &[SortKeyRt]) -> Result<Vec<Row>> {
        let limit = self.limit;
        let mut items = self.items;
        sort_pairs(ex, keys, &mut items)?;
        items.truncate(limit);
        Ok(items.into_iter().map(|(_, row)| row).collect())
    }
}

/// Memcomparable equality key over the given slots of `row` (module docs). `types[i]` is the type of
/// slot `slots[i]`.
pub fn row_key_bytes(ex: &Executor, types: &[TypeId], slots: &[usize], row: &[Datum]) -> Result<Vec<u8>> {
    if types.len() != slots.len() {
        return Err(PgError::internal("row key: types and slots have different lengths"));
    }
    let mut cols = Vec::with_capacity(types.len());
    let mut values = Vec::with_capacity(slots.len());
    for (ty, slot) in types.iter().zip(slots.iter()) {
        let value = match row.get(*slot) {
            Some(v) => v.clone(),
            None => return Err(PgError::internal("row key: slot index out of range")),
        };
        cols.push(KeyColumn::asc(*ty));
        values.push(value);
    }
    encode_key_vec(&ex.se().env, &cols, &values)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::context::{ExecContext, NoGuc, NoServices, SessionEnv};
    use crate::memtxn::MemTxn;
    use crate::runtime::Runtime;
    use pgarbor_types::context::IoConfig;
    use pgarbor_types::numeric::io::parse_numeric;
    use std::cell::Cell;

    fn key(ty: TypeId, descending: bool, nulls_first: bool) -> SortKeyRt {
        SortKeyRt { expr: BoundExpr::constant(Datum::Null, ty), ty, descending, nulls_first, using_op: None }
    }

    /// Runs `f` with a real executor over an empty in-memory catalog and transaction.
    fn with_executor<T>(f: impl FnOnce(&mut Executor) -> Result<T>) -> Result<T> {
        let (_kv, env) = pgarbor_catalog::testing::bootstrapped()?;
        let mut txn = MemTxn::new();
        let guc = NoGuc;
        let services = NoServices;
        let rng = Cell::new(1u64);
        let se = SessionEnv::new(env, IoConfig::default(), &guc, &services, &rng, 0, 0);
        let mut cx = ExecContext::new(&mut txn, se);
        let mut rt = Runtime::default();
        let mut ex = Executor::new(&mut rt, &mut cx);
        f(&mut ex)
    }

    /// Keyed rows whose row payload is `[value, arrival index]`.
    fn keyed(values: &[Datum]) -> Vec<KeyedRow> {
        values
            .iter()
            .enumerate()
            .map(|(i, v)| (vec![v.clone()], vec![v.clone(), Datum::Int4(i as i32)]))
            .collect()
    }

    fn tags(pairs: &[KeyedRow]) -> Vec<i32> {
        pairs
            .iter()
            .map(|(_, row)| match row[1] {
                Datum::Int4(t) => t,
                _ => -1,
            })
            .collect()
    }

    #[test]
    fn int_sort_directions_and_nulls() -> Result<()> {
        let values = [Datum::Int4(3), Datum::Null, Datum::Int4(1), Datum::Int4(3), Datum::Int4(2)];
        let cases = [
            (false, false, vec![2, 4, 0, 3, 1]),
            (true, true, vec![1, 0, 3, 4, 2]),
            (true, false, vec![0, 3, 4, 2, 1]),
        ];
        with_executor(|ex| {
            for (descending, nulls_first, expected) in cases.iter() {
                let keys = vec![key(TypeId::INT4, *descending, *nulls_first)];
                let mut pairs = keyed(&values);
                sort_pairs(ex, &keys, &mut pairs)?;
                assert_eq!(tags(&pairs), *expected);
            }
            Ok(())
        })
    }

    #[test]
    fn text_sort_uses_code_point_order() -> Result<()> {
        let values = [Datum::Text("b".into()), Datum::Text("a".into()), Datum::Text("B".into()), Datum::Null];
        let keys = vec![key(TypeId::TEXT, false, false)];
        with_executor(|ex| {
            let mut pairs = keyed(&values);
            sort_pairs(ex, &keys, &mut pairs)?;
            assert_eq!(tags(&pairs), vec![2, 1, 0, 3]);
            Ok(())
        })
    }

    #[test]
    fn sort_is_stable_for_equal_keys() -> Result<()> {
        let values = [Datum::Int4(7), Datum::Int4(7), Datum::Int4(7)];
        let keys = vec![key(TypeId::INT4, false, false)];
        with_executor(|ex| {
            let mut pairs = keyed(&values);
            sort_pairs(ex, &keys, &mut pairs)?;
            assert_eq!(tags(&pairs), vec![0, 1, 2]);
            Ok(())
        })
    }

    #[test]
    fn topn_limit_three_of_ten_is_ordered() -> Result<()> {
        let keys = vec![key(TypeId::INT4, false, false)];
        let input = [5, 9, 1, 7, 3, 8, 2, 10, 4, 6];
        with_executor(|ex| {
            let mut top = TopN::new(3);
            for (i, v) in input.iter().enumerate() {
                let row = vec![Datum::Int4(*v), Datum::Int4(i as i32)];
                top.push(ex, &keys, vec![Datum::Int4(*v)], row)?;
            }
            let out = top.into_sorted(ex, &keys)?;
            let got: Vec<i32> = out
                .iter()
                .map(|r| match r[0] {
                    Datum::Int4(v) => v,
                    _ => -1,
                })
                .collect();
            assert_eq!(got, vec![1, 2, 3]);
            Ok(())
        })
    }

    #[test]
    fn topn_zero_keeps_nothing() -> Result<()> {
        let keys = vec![key(TypeId::INT4, false, false)];
        with_executor(|ex| {
            let mut top = TopN::new(0);
            top.push(ex, &keys, vec![Datum::Int4(1)], vec![Datum::Int4(1)])?;
            assert!(top.into_sorted(ex, &keys)?.is_empty());
            Ok(())
        })
    }

    #[test]
    fn row_key_bytes_null_and_numeric_scale() -> Result<()> {
        let types = [TypeId::NUMERIC, TypeId::INT4];
        let slots = [0usize, 1usize];
        let a = vec![Datum::Numeric(parse_numeric("1.0")?), Datum::Null];
        let b = vec![Datum::Numeric(parse_numeric("1.00")?), Datum::Null];
        let c = vec![Datum::Numeric(parse_numeric("2")?), Datum::Null];
        with_executor(|ex| {
            let ka = row_key_bytes(ex, &types, &slots, &a)?;
            let kb = row_key_bytes(ex, &types, &slots, &b)?;
            let kc = row_key_bytes(ex, &types, &slots, &c)?;
            assert_eq!(ka, kb);
            assert_ne!(ka, kc);
            Ok(())
        })
    }

    #[test]
    fn ordering_operator_must_be_lt_or_gt() {
        assert!(check_ordering_op("<").is_ok());
        assert!(check_ordering_op(">").is_ok());
        assert!(check_ordering_op("~~").is_err());
    }
}
