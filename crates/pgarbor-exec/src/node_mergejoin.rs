//! Merge join (lease EXE-18). PG: `nodeMergejoin.c`. Layout: plan.rs `MergeJoinNode`; both inputs
//! arrive sorted by their keys (`left_keys`/`right_keys`: expression, `descending`, `nulls_first`).
//!
//! * Keys are evaluated per row over the child's row and compared pairwise with
//!   `compare::cmp_sort(env, ty, a, b, descending, nulls_first)`. A row with ANY NULL key never matches
//!   (its relative position still follows the sort order); NULL-key rows are passed through
//!   unmatched for the outer-join types.
//! * Standard merge: advance the side with the smaller key; on equal keys collect the whole group of
//!   equal-key rows of the RIGHT side into a buffer, then for every left row of the equal group pair it
//!   with each buffered right row, evaluating the residual `qual` over `left ++ right`; (re)use the
//!   buffered group for all left rows with the same key.
//! * Join types: `Inner`; `Left` (unmatched left rows emit `left ++ NULLs`); `Right` (unmatched right rows
//!   emit `NULLs ++ right`); `Full` (both); `Semi` (left row once at its first qualifying right row);
//!   `Anti` (left rows without any qualifying right row). A left row whose group matches by key but
//!   whose residual `qual` fails for every right row counts as unmatched.
//! * Output `left ++ right` (Semi/Anti: left only). Emit lazily, one row per `next`.
//! * `rescan(params)` rescans both children and clears all buffers.
//!
//! Implementation notes: the left input is consumed one row at a time. Each left row is processed in
//! full by `process_left`, which pushes its output rows onto a pending queue that `next` drains one row
//! per call. Right rows are pulled through `pull_right` (with a one-row lookahead slot) and collected
//! into `group` while their keys equal the group key. Unmatched right rows of the Right/Full kinds are
//! emitted in the order the merge passes them (when skipped, or when their group is dropped), which keeps
//! the unmatched output in key order.
use std::cmp::Ordering;
use std::collections::VecDeque;

use pgarbor_common::{PgError, Result};
use pgarbor_plan::plan::{JoinType, MergeJoinNode, MergeKey, PlanNode};
use pgarbor_sql::ir::BoundExpr;
use pgarbor_types::compare;
use pgarbor_types::Datum;

use crate::build::BuildCtx;
use crate::eval;
use crate::runtime::{Executor, Row, RowSource};

/// One right-side row with its evaluated join keys.
struct RightEntry {
    row: Row,
    keys: Vec<Datum>,
    null_key: bool,
    matched: bool,
}

/// The merge join operator (owns copies of everything it needs; module docs).
struct MergeJoinOp {
    join: JoinType,
    left: Box<dyn RowSource>,
    right: Box<dyn RowSource>,
    left_keys: Vec<MergeKey>,
    right_keys: Vec<MergeKey>,
    qual: Option<BoundExpr>,
    left_width: usize,
    right_width: usize,
    /// Buffered right rows whose keys all equal `group_keys` (non-NULL keys only).
    group: Vec<RightEntry>,
    group_keys: Vec<Datum>,
    group_valid: bool,
    /// Right row fetched but not yet consumed (lookahead).
    right_next: Option<RightEntry>,
    right_eof: bool,
    left_eof: bool,
    done: bool,
    /// Pending output rows.
    queue: VecDeque<Row>,
    ticks: u32,
}

/// `a ++ b` as a new row.
fn concat_rows(a: &[Datum], b: &[Datum]) -> Row {
    let mut out = Vec::with_capacity(a.len() + b.len());
    out.extend(a.iter().cloned());
    out.extend(b.iter().cloned());
    out
}

/// A row of `width` NULL slots.
fn null_row(width: usize) -> Row {
    vec![Datum::Null; width]
}

/// Evaluate the join keys over one row. Returns the key values and whether any of them is NULL.
fn eval_keys(ex: &mut Executor, keys: &[MergeKey], row: &[Datum]) -> Result<(Vec<Datum>, bool)> {
    let mut values = Vec::with_capacity(keys.len());
    let mut null_key = false;
    for k in keys {
        let v = eval::eval(ex, &k.expr, row)?;
        if v.is_null() {
            null_key = true;
        }
        values.push(v);
    }
    Ok((values, null_key))
}

impl MergeJoinOp {
    /// Compare a left key vector with a right key vector (both non-NULL in every column) using the
    /// left keys' types and ordering flags (`compare::cmp_sort`, PG `ExecQual`-free merge comparison).
    fn cmp_keys(&self, ex: &Executor, a: &[Datum], b: &[Datum]) -> Result<Ordering> {
        for (k, (x, y)) in self.left_keys.iter().zip(a.iter().zip(b.iter())) {
            let o = compare::cmp_sort(&ex.se().env, k.expr.ty, x, y, k.descending, k.nulls_first)?;
            if o != Ordering::Equal {
                return Ok(o);
            }
        }
        Ok(Ordering::Equal)
    }

    /// Emit `NULLs ++ right` for an unmatched right row (Right and Full joins only).
    fn emit_unmatched_right(&mut self, r: RightEntry) {
        if matches!(self.join, JoinType::Right | JoinType::Full) {
            let mut out = null_row(self.left_width);
            out.extend(r.row);
            self.queue.push_back(out);
        }
    }

    /// Discard the buffered group, emitting its unmatched rows for Right/Full joins.
    fn drop_group(&mut self) {
        let group = std::mem::take(&mut self.group);
        for e in group {
            if !e.matched {
                self.emit_unmatched_right(e);
            }
        }
        self.group_valid = false;
        self.group_keys.clear();
    }

    /// Next right row: the lookahead slot first, then the child.
    fn pull_right(&mut self, ex: &mut Executor) -> Result<Option<RightEntry>> {
        if let Some(r) = self.right_next.take() {
            return Ok(Some(r));
        }
        if self.right_eof {
            return Ok(None);
        }
        match self.right.next(ex)? {
            None => {
                self.right_eof = true;
                Ok(None)
            }
            Some(row) => {
                let (keys, null_key) = eval_keys(ex, &self.right_keys, &row)?;
                Ok(Some(RightEntry { row, keys, null_key, matched: false }))
            }
        }
    }

    /// Advance the right input for a left row with keys `lkeys` while no group is buffered: right rows
    /// smaller than `lkeys` are unmatched; the first right row not smaller either starts a group (when
    /// equal) or becomes the lookahead (when greater).
    fn load_group(&mut self, ex: &mut Executor, lkeys: &[Datum]) -> Result<()> {
        loop {
            let r = match self.pull_right(ex)? {
                Some(r) => r,
                None => return Ok(()),
            };
            if r.null_key {
                self.emit_unmatched_right(r);
                continue;
            }
            match self.cmp_keys(ex, lkeys, &r.keys)? {
                Ordering::Less => {
                    self.right_next = Some(r);
                    return Ok(());
                }
                Ordering::Greater => {
                    self.emit_unmatched_right(r);
                }
                Ordering::Equal => {
                    self.group_keys = r.keys.clone();
                    self.group.push(r);
                    loop {
                        let nx = match self.pull_right(ex)? {
                            Some(x) => x,
                            None => break,
                        };
                        if nx.null_key {
                            self.emit_unmatched_right(nx);
                            continue;
                        }
                        if self.cmp_keys(ex, &self.group_keys, &nx.keys)? == Ordering::Equal {
                            self.group.push(nx);
                        } else {
                            self.right_next = Some(nx);
                            break;
                        }
                    }
                    self.group_valid = true;
                    return Ok(());
                }
            }
        }
    }

    /// Pair one left row with every buffered group row that satisfies `qual`. Returns whether any did.
    fn pair_group(&mut self, ex: &mut Executor, lrow: &Row) -> Result<bool> {
        let mut matched = false;
        for i in 0..self.group.len() {
            let combined = concat_rows(lrow, &self.group[i].row);
            if !eval::eval_opt_qual(ex, &self.qual, &combined)? {
                continue;
            }
            matched = true;
            self.group[i].matched = true;
            match self.join {
                JoinType::Semi => {
                    self.queue.push_back(lrow.clone());
                    break;
                }
                JoinType::Anti => break,
                _ => self.queue.push_back(combined),
            }
        }
        Ok(matched)
    }

    /// Process one complete left row (module docs): match it against the buffered group, advancing
    /// the right side as needed, and queue its output.
    fn process_left(&mut self, ex: &mut Executor, lrow: Row, lkeys: Vec<Datum>, null_key: bool) -> Result<()> {
        let mut matched = false;
        if !null_key {
            loop {
                if self.group_valid {
                    match self.cmp_keys(ex, &lkeys, &self.group_keys)? {
                        Ordering::Greater => self.drop_group(),
                        _ => break,
                    }
                } else {
                    self.load_group(ex, &lkeys)?;
                    break;
                }
            }
            if self.group_valid && self.cmp_keys(ex, &lkeys, &self.group_keys)? == Ordering::Equal {
                matched = self.pair_group(ex, &lrow)?;
            }
        }
        if !matched {
            match self.join {
                JoinType::Left | JoinType::Full => {
                    let mut out = lrow;
                    out.extend(null_row(self.right_width));
                    self.queue.push_back(out);
                }
                JoinType::Anti => self.queue.push_back(lrow),
                _ => {}
            }
        }
        Ok(())
    }

    /// Left input exhausted: flush the group and, for Right/Full joins, the remaining right rows.
    fn finish(&mut self, ex: &mut Executor) -> Result<()> {
        if self.group_valid {
            self.drop_group();
        }
        if matches!(self.join, JoinType::Right | JoinType::Full) {
            if let Some(r) = self.right_next.take() {
                self.emit_unmatched_right(r);
            }
            while !self.right_eof {
                match self.right.next(ex)? {
                    Some(row) => {
                        let mut out = null_row(self.left_width);
                        out.extend(row);
                        self.queue.push_back(out);
                    }
                    None => self.right_eof = true,
                }
            }
        }
        self.done = true;
        Ok(())
    }

    /// One unit of progress: consume one left row, or finish.
    fn step(&mut self, ex: &mut Executor) -> Result<()> {
        ex.tick(&mut self.ticks)?;
        if self.left_eof {
            return self.finish(ex);
        }
        match self.left.next(ex)? {
            None => {
                self.left_eof = true;
                Ok(())
            }
            Some(row) => {
                let (keys, null_key) = eval_keys(ex, &self.left_keys, &row)?;
                self.process_left(ex, row, keys, null_key)
            }
        }
    }
}

impl RowSource for MergeJoinOp {
    fn next(&mut self, ex: &mut Executor) -> Result<Option<Row>> {
        loop {
            if let Some(r) = self.queue.pop_front() {
                return Ok(Some(r));
            }
            if self.done {
                return Ok(None);
            }
            self.step(ex)?;
        }
    }

    fn rescan(&mut self, ex: &mut Executor, params: &[Datum]) -> Result<()> {
        self.left.rescan(ex, params)?;
        self.right.rescan(ex, params)?;
        self.group.clear();
        self.group_keys.clear();
        self.group_valid = false;
        self.right_next = None;
        self.right_eof = false;
        self.left_eof = false;
        self.done = false;
        self.queue.clear();
        Ok(())
    }
}

pub fn build_merge_join(node: &PlanNode, n: &MergeJoinNode, b: &mut BuildCtx) -> Result<Box<dyn RowSource>> {
    let _ = node;
    if n.left_keys.len() != n.right_keys.len() {
        return Err(PgError::internal("merge join key counts differ"));
    }
    for (l, r) in n.left_keys.iter().zip(n.right_keys.iter()) {
        if l.expr.ty != r.expr.ty {
            return Err(PgError::internal("merge join key types differ"));
        }
    }
    let left = b.child(&n.left)?;
    let right = b.child(&n.right)?;
    Ok(Box::new(MergeJoinOp {
        join: n.join_type,
        left,
        right,
        left_keys: n.left_keys.clone(),
        right_keys: n.right_keys.clone(),
        qual: n.qual.clone(),
        left_width: n.left.width(),
        right_width: n.right.width(),
        group: Vec::new(),
        group_keys: Vec::new(),
        group_valid: false,
        right_next: None,
        right_eof: false,
        left_eof: false,
        done: false,
        queue: VecDeque::new(),
        ticks: 0,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn concat_and_null_padding() {
        let r = concat_rows(&[Datum::Int4(1)], &[Datum::Null, Datum::Int4(2)]);
        assert_eq!(r, vec![Datum::Int4(1), Datum::Null, Datum::Int4(2)]);
        assert_eq!(null_row(2), vec![Datum::Null, Datum::Null]);
        assert!(null_row(0).is_empty());
    }
}
