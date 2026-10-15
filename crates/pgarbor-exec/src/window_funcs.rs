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
use pgarbor_common::{PgError, Result};
use pgarbor_plan::plan::WindowCall;
use pgarbor_types::Datum;

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
#[allow(unused_variables)]
pub fn eval_window_func(
    ex: &mut Executor,
    call: &WindowCall,
    part: &WinPartition,
    i: usize,
    frame: &WinFrame,
) -> Result<Datum> {
    Err(PgError::unimplemented("EXE-21", "window function evaluation (window_funcs::eval_window_func)"))
}
