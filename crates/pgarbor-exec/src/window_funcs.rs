//! Pure window functions and the partition/frame data model (lease EXE-21). The window operator
//! (`node_window`) owns partition buffering and frame computation; this file evaluates ONE window
//! function call for ONE row. PG: `utils/adt/windowfuncs.c`, `executor/nodeWindowAgg.c`.
//!
//! ## Data model (frozen)
//! * [`WinPartition`]: all rows of one partition (input rows, in sort order) plus, per row, its
//!   peer group `[peer_start[i], peer_end[i])` (rows equal under the window ORDER BY; the whole
//!   partition when there is no ORDER BY) and the row's ORDER BY key vector (for `RANGE`).
//! * [`WinFrame`]: the frame of one row: the contiguous range `[start, end)` of row indexes, with the
//!   EXCLUDE clause applied through `excluded` (a sorted list of row indexes inside the range that
//!   are NOT part of the frame).
//!
//! ## Functions by `WindowCall::name` (arguments evaluated with `eval` over the row they refer to)
//! * `row_number()`: `i + 1` (Int8). `rank()`: `peer_start[i] + 1`. `dense_rank()`: number of distinct
//!   peer groups up to and including row i's group. `percent_rank()`: `(rank - 1) / (n - 1)` as float8,
//!   0 when n == 1. `cume_dist()`: `peer_end[i] / n` float8. All independent of the frame.
//! * `ntile(n)`: bucket number 1..n of row i among the partition's rows (PG distribution: the first
//!   `rows % n` buckets get one extra row); n NULL -> NULL; n <= 0 -> 22014 `argument of ntile must be
//!   greater than zero`.
//! * `lag(v [, offset [, default]])` / `lead(...)`: the value of `v` evaluated over row `i -/+ offset`
//!   WITHIN THE PARTITION (not the frame); outside -> `default` (evaluated over the CURRENT row) or
//!   NULL; offset NULL -> NULL; default offset 1; negative offsets reverse direction.
//! * `first_value(v)`, `last_value(v)`, `nth_value(v, n)`: over the FRAME rows (after exclusion);
//!   `nth_value` n <= 0 -> 22016 `argument of nth_value must be greater than zero`; empty frame or
//!   n beyond the frame -> NULL. `IGNORE NULLS` (`call.ignore_nulls`) skips NULL values when counting
//!   (0A000 in PG 18 for most functions; honor it only where PG does: lag/lead/first_value/last_value/
//!   nth_value).
//! Aggregates used as window functions (`call.is_agg`) are NOT handled here (`node_window` drives
//! `pgarbor_types::agg` over the frame).
use pgarbor_common::{PgError, Result, SqlState};
use pgarbor_plan::plan::WindowCall;
use pgarbor_sql::ir::BoundExpr;
use pgarbor_types::Datum;

use crate::eval::eval;
use crate::runtime::{Executor, Row};

/// All rows of one partition (module docs).
// CONTRACT
#[derive(Clone, Debug, Default)]
pub struct WinPartition {
    pub rows: Vec<Row>,
    /// Per row: index of the first row of its peer group.
    pub peer_start: Vec<usize>,
    /// Per row: index one past the last row of its peer group.
    pub peer_end: Vec<usize>,
}

impl WinPartition {
    pub fn len(&self) -> usize {
        self.rows.len()
    }
    pub fn is_empty(&self) -> bool {
        self.rows.is_empty()
    }
    /// Number of distinct peer groups up to and including row `i`'s group (dense_rank).
    pub fn dense_rank(&self, i: usize) -> i64 {
        let mut n = 0i64;
        let mut j = 0usize;
        while j <= i && j < self.rows.len() {
            n += 1;
            j = self.peer_end[j];
        }
        n
    }
}

/// The frame of one row (module docs).
// CONTRACT
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct WinFrame {
    pub start: usize,
    pub end: usize,
    /// Sorted row indexes inside `[start, end)` removed by EXCLUDE CURRENT ROW / GROUP / TIES.
    pub excluded: Vec<usize>,
}

impl WinFrame {
    pub fn contains(&self, i: usize) -> bool {
        i >= self.start && i < self.end && self.excluded.binary_search(&i).is_err()
    }
    /// Indexes of the rows in the frame, ascending.
    pub fn indexes(&self) -> impl Iterator<Item = usize> + '_ {
        (self.start..self.end).filter(move |i| self.excluded.binary_search(i).is_err())
    }
    pub fn is_empty(&self) -> bool {
        self.indexes().next().is_none()
    }
}

/// True if `name` is one of the pure window functions implemented here.
pub fn is_pure_window_function(name: &str) -> bool {
    matches!(
        name,
        "row_number"
            | "rank"
            | "dense_rank"
            | "percent_rank"
            | "cume_dist"
            | "ntile"
            | "lag"
            | "lead"
            | "first_value"
            | "last_value"
            | "nth_value"
    )
}

/// Evaluate the pure window function `call` for row `i` of `part` with frame `frame`.
pub fn eval_window_func(
    ex: &mut Executor,
    call: &WindowCall,
    part: &WinPartition,
    i: usize,
    frame: &WinFrame,
) -> Result<Datum> {
    check_row(part, i)?;
    if call.is_agg || !is_pure_window_function(&call.name) {
        return Err(PgError::internal(format!("window function {} is not a pure window function", call.name)));
    }
    match call.name.as_str() {
        "row_number" => Ok(Datum::Int8(i as i64 + 1)),
        "rank" => Ok(Datum::Int8(rank_of(part, i))),
        "dense_rank" => Ok(Datum::Int8(part.dense_rank(i))),
        "percent_rank" => Ok(Datum::Float8(percent_rank_of(part, i))),
        "cume_dist" => Ok(Datum::Float8(cume_dist_of(part, i))),
        "ntile" => eval_ntile(ex, call, part, i),
        "lag" => eval_lead_lag(ex, call, part, i, false),
        "lead" => eval_lead_lag(ex, call, part, i, true),
        "first_value" => eval_first_value(ex, call, part, frame),
        "last_value" => eval_last_value(ex, call, part, frame),
        "nth_value" => eval_nth_value(ex, call, part, i, frame),
        other => Err(PgError::internal(format!("window function {other} is not implemented"))),
    }
}

/// Row `i` must exist and the peer vectors must be parallel to the rows.
fn check_row(part: &WinPartition, i: usize) -> Result<()> {
    let n = part.rows.len();
    if i >= n || part.peer_start.len() != n || part.peer_end.len() != n {
        return Err(PgError::internal(format!("window row {i} is outside a partition of {n} rows")));
    }
    Ok(())
}

/// Input row `j` of the partition (frame bounds are trusted to be inside it; violations are XX000).
fn row_at(part: &WinPartition, j: usize) -> Result<&Row> {
    part.rows
        .get(j)
        .ok_or_else(|| PgError::internal(format!("window frame row {j} is outside the partition")))
}

/// The `k`-th argument expression (0-based); a missing argument is XX000 (the binder checks arity).
fn arg_expr(call: &WindowCall, k: usize) -> Result<&BoundExpr> {
    call.args.get(k).ok_or_else(|| {
        PgError::internal(format!("window function {} is missing argument {}", call.name, k + 1))
    })
}

/// Integer value of an int2/int4/int8 datum; SQL NULL gives `None`.
fn int_value(d: &Datum) -> Result<Option<i64>> {
    match d {
        Datum::Null => Ok(None),
        Datum::Int2(v) => Ok(Some(i64::from(*v))),
        Datum::Int4(v) => Ok(Some(i64::from(*v))),
        Datum::Int8(v) => Ok(Some(*v)),
        _ => Err(PgError::internal("window function integer argument has a non-integer value")),
    }
}

/// `rank()`: the first row of the peer group, 1-based.
fn rank_of(part: &WinPartition, i: usize) -> i64 {
    part.peer_start[i] as i64 + 1
}

/// `percent_rank()`: `(rank - 1) / (n - 1)`, 0 for a single-row partition.
fn percent_rank_of(part: &WinPartition, i: usize) -> f64 {
    let n = part.len();
    if n <= 1 {
        return 0.0;
    }
    // rank - 1 == peer_start[i]
    part.peer_start[i] as f64 / (n - 1) as f64
}

/// `cume_dist()`: rows up to the end of the peer group divided by the partition size.
fn cume_dist_of(part: &WinPartition, i: usize) -> f64 {
    part.peer_end[i] as f64 / part.len() as f64
}

/// `ntile(buckets)` bucket (1-based) of row `i` in a partition of `total` rows, `buckets > 0`.
/// The first `total % buckets` buckets hold one extra row (PG `ntile` distribution).
fn ntile_bucket(i: usize, total: usize, buckets: i64) -> i64 {
    let total = total as i64;
    let i = i as i64;
    let rows_per = total / buckets;
    let remainder = total % buckets;
    let big = rows_per + 1;
    // rows held by the larger buckets
    let span = big * remainder;
    if i < span {
        i / big + 1
    } else {
        remainder + (i - span) / rows_per.max(1) + 1
    }
}

fn eval_ntile(ex: &mut Executor, call: &WindowCall, part: &WinPartition, i: usize) -> Result<Datum> {
    let e = arg_expr(call, 0)?;
    let buckets = match int_value(&eval(ex, e, row_at(part, i)?)?)? {
        None => return Ok(Datum::Null),
        Some(v) => v,
    };
    if buckets <= 0 {
        return Err(PgError::new(SqlState(*b"22014"), "argument of ntile must be greater than zero"));
    }
    let b = ntile_bucket(i, part.len(), buckets);
    // ntile returns int4 (pg_proc prorettype 23)
    let b = i32::try_from(b).map_err(|_| PgError::internal("ntile bucket out of int4 range"))?;
    Ok(Datum::Int4(b))
}

/// Partition index `i + signed` when it lies inside `[0, len)`.
fn offset_target(i: usize, len: usize, signed: i128) -> Option<usize> {
    let t = i as i128 + signed;
    if t < 0 || t >= len as i128 {
        None
    } else {
        Some(t as usize)
    }
}

/// The default of `lag`/`lead`, evaluated over the current row (NULL when absent).
fn default_value(ex: &mut Executor, call: &WindowCall, part: &WinPartition, i: usize) -> Result<Datum> {
    match call.args.get(2) {
        None => Ok(Datum::Null),
        Some(e) => eval(ex, e, row_at(part, i)?),
    }
}

/// `lag(v [, offset [, default]])` (`lead` when `lead` is true). Offsets are partition-relative;
/// a negative offset reverses the direction; `IGNORE NULLS` counts only non-NULL values.
fn eval_lead_lag(
    ex: &mut Executor,
    call: &WindowCall,
    part: &WinPartition,
    i: usize,
    lead: bool,
) -> Result<Datum> {
    let value_e = arg_expr(call, 0)?;
    let off = match call.args.get(1) {
        None => 1i64,
        Some(e) => match int_value(&eval(ex, e, row_at(part, i)?)?)? {
            None => return Ok(Datum::Null),
            Some(v) => v,
        },
    };
    let signed: i128 = if lead { i128::from(off) } else { -i128::from(off) };
    if signed == 0 {
        return eval(ex, value_e, row_at(part, i)?);
    }
    if !call.ignore_nulls {
        return match offset_target(i, part.len(), signed) {
            Some(t) => eval(ex, value_e, row_at(part, t)?),
            None => default_value(ex, call, part, i),
        };
    }
    let step: i128 = if signed < 0 { -1 } else { 1 };
    let mut remaining: i128 = signed.abs();
    let mut j = i as i128;
    loop {
        j += step;
        if j < 0 || j >= part.len() as i128 {
            return default_value(ex, call, part, i);
        }
        let v = eval(ex, value_e, row_at(part, j as usize)?)?;
        if v.is_null() {
            continue;
        }
        remaining -= 1;
        if remaining == 0 {
            return Ok(v);
        }
    }
}

/// `first_value(v)`: `v` of the first frame row (`IGNORE NULLS`: first non-NULL); NULL if none.
fn eval_first_value(ex: &mut Executor, call: &WindowCall, part: &WinPartition, frame: &WinFrame) -> Result<Datum> {
    let value_e = arg_expr(call, 0)?;
    for j in frame.indexes() {
        let v = eval(ex, value_e, row_at(part, j)?)?;
        if call.ignore_nulls && v.is_null() {
            continue;
        }
        return Ok(v);
    }
    Ok(Datum::Null)
}

/// `last_value(v)`: `v` of the last frame row (`IGNORE NULLS`: last non-NULL); NULL if none.
fn eval_last_value(ex: &mut Executor, call: &WindowCall, part: &WinPartition, frame: &WinFrame) -> Result<Datum> {
    let value_e = arg_expr(call, 0)?;
    for j in (frame.start..frame.end).rev() {
        if !frame.contains(j) {
            continue;
        }
        let v = eval(ex, value_e, row_at(part, j)?)?;
        if call.ignore_nulls && v.is_null() {
            continue;
        }
        return Ok(v);
    }
    Ok(Datum::Null)
}

/// `nth_value(v, n)`: `v` of the n-th frame row (1-based; `IGNORE NULLS` counts non-NULL values).
/// A NULL `n` gives NULL; `n <= 0` is 22016 and is checked before the frame is looked at.
fn eval_nth_value(
    ex: &mut Executor,
    call: &WindowCall,
    part: &WinPartition,
    i: usize,
    frame: &WinFrame,
) -> Result<Datum> {
    let value_e = arg_expr(call, 0)?;
    let n_e = arg_expr(call, 1)?;
    let n = match int_value(&eval(ex, n_e, row_at(part, i)?)?)? {
        None => return Ok(Datum::Null),
        Some(v) => v,
    };
    if n <= 0 {
        return Err(PgError::new(SqlState(*b"22016"), "argument of nth_value must be greater than zero"));
    }
    let mut remaining = n;
    for j in frame.indexes() {
        let v = eval(ex, value_e, row_at(part, j)?)?;
        if call.ignore_nulls && v.is_null() {
            continue;
        }
        remaining -= 1;
        if remaining == 0 {
            return Ok(v);
        }
    }
    Ok(Datum::Null)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn partition(peer_start: Vec<usize>, peer_end: Vec<usize>) -> WinPartition {
        let n = peer_start.len();
        WinPartition { rows: vec![vec![Datum::Int4(0)]; n], peer_start, peer_end }
    }

    #[test]
    fn rank_and_dense_rank_over_peers() {
        // rows 0,1 are peers; row 2 is alone
        let p = partition(vec![0, 0, 2], vec![2, 2, 3]);
        assert_eq!((0..3).map(|i| rank_of(&p, i)).collect::<Vec<_>>(), vec![1, 1, 3]);
        assert_eq!((0..3).map(|i| p.dense_rank(i)).collect::<Vec<_>>(), vec![1, 1, 2]);
    }

    #[test]
    fn percent_rank_and_cume_dist() {
        let p = partition(vec![0, 0, 2], vec![2, 2, 3]);
        let pr: Vec<f64> = (0..3).map(|i| percent_rank_of(&p, i)).collect();
        assert_eq!(pr, vec![0.0, 0.0, 1.0]);
        let cd: Vec<f64> = (0..3).map(|i| cume_dist_of(&p, i)).collect();
        assert!((cd[0] - 2.0 / 3.0).abs() < 1e-12);
        assert!((cd[1] - 2.0 / 3.0).abs() < 1e-12);
        assert_eq!(cd[2], 1.0);
        let single = partition(vec![0], vec![1]);
        assert_eq!(percent_rank_of(&single, 0), 0.0);
        assert_eq!(cume_dist_of(&single, 0), 1.0);
    }

    #[test]
    fn ntile_distribution_gives_extra_rows_to_first_buckets() {
        let buckets: Vec<i64> = (0..7).map(|i| ntile_bucket(i, 7, 3)).collect();
        assert_eq!(buckets, vec![1, 1, 1, 2, 2, 3, 3]);
        let sizes: Vec<usize> = (1..=3).map(|b| buckets.iter().filter(|&&x| x == b).count()).collect();
        assert_eq!(sizes, vec![3, 2, 2]);
        let six: Vec<i64> = (0..6).map(|i| ntile_bucket(i, 6, 3)).collect();
        assert_eq!(six, vec![1, 1, 2, 2, 3, 3]);
        // more buckets than rows: one row per bucket
        let few: Vec<i64> = (0..3).map(|i| ntile_bucket(i, 3, 5)).collect();
        assert_eq!(few, vec![1, 2, 3]);
    }

    #[test]
    fn offset_target_stays_inside_partition() {
        // lag(x, 2): signed = -2 over a 5-row partition
        assert_eq!(offset_target(0, 5, -2), None);
        assert_eq!(offset_target(1, 5, -2), None);
        assert_eq!(offset_target(2, 5, -2), Some(0));
        // lead(x, 2) at the end
        assert_eq!(offset_target(3, 5, 2), None);
        assert_eq!(offset_target(2, 5, 2), Some(4));
        // lag(x, -1) behaves as lead(x, 1): signed = +1
        assert_eq!(offset_target(0, 5, 1), Some(1));
    }

    #[test]
    fn int_value_accepts_integer_types_and_null() {
        assert_eq!(int_value(&Datum::Null).unwrap(), None);
        assert_eq!(int_value(&Datum::Int2(-3)).unwrap(), Some(-3));
        assert_eq!(int_value(&Datum::Int4(7)).unwrap(), Some(7));
        assert_eq!(int_value(&Datum::Int8(i64::MAX)).unwrap(), Some(i64::MAX));
        assert!(int_value(&Datum::Bool(true)).is_err());
    }

    #[test]
    fn pure_function_names_are_recognized() {
        for name in ["row_number", "rank", "dense_rank", "percent_rank", "cume_dist", "ntile", "lag", "lead", "first_value", "last_value", "nth_value"] {
            assert!(is_pure_window_function(name), "{name}");
        }
        assert!(!is_pure_window_function("sum"));
    }
}
