//! `Distinct` and `SetOp` operators (lease EXE-24). PG: `nodeUnique.c`, `nodeSetOp.c`,
//! `nodeAgg.c` (hashed DISTINCT).
//!
//! ## Distinct (`DistinctNode{strategy, input, key_slots}`)
//! Emits the FIRST input row of every distinct key, where the key = the values at `key_slots` compared
//! with NULLs EQUAL (`sortutil::row_key_bytes` over the input column types and `key_slots`).
//! * `Hashed`: a `HashSet` of key bytes; rows stream out as they are first seen.
//! * `Sorted`: input is ordered so equal keys are adjacent: compare with the previous emitted key
//!   (`compare::not_distinct` per slot) and emit only changes.
//! Output row = input row (junk columns are stripped by a `Project` above).
//!
//! ## SetOp (`SetOpNode{cmd, left, right}`; all slots compared, NULLs equal)
//! Drain `right` into a multiset (`HashMap<key bytes, count>`), then stream `left`:
//! * `Intersect`: emit a left row the first time its key is seen if the key is present in `right`
//!   (distinct result); `IntersectAll`: each key is emitted `min(left_count, right_count)` times (emit
//!   while the remaining right count is > 0, decrementing it);
//! * `Except`: emit a left row the first time its key is seen if the key is ABSENT from `right`;
//!   `ExceptAll`: each key `max(left_count - right_count, 0)` times (skip the first `right_count`
//!   occurrences of the key, emit the rest, i.e. decrement a remaining-right counter per key).
//! Output order follows the left input (first occurrence order). Output row = left row layout.
//! `rescan(params)` restarts both children and clears state.
use std::collections::{HashMap, HashSet};

use pgarbor_common::{PgError, Result};
use pgarbor_plan::plan::{DistinctNode, DistinctStrategy, PlanNode, SetOpCmd, SetOpNode};
use pgarbor_types::{Datum, TypeId};

use crate::build::BuildCtx;
use crate::runtime::{Executor, Row, RowSource};
use crate::sortutil::row_key_bytes;

/// The slots that form a row key, with their types (`types[i]` is the type of `slots[i]`).
/// Keys are compared with `row_key_bytes`, so equal bytes <=> NULLs-equal SQL equality.
struct KeySpec {
    types: Vec<TypeId>,
    slots: Vec<usize>,
}

impl KeySpec {
    fn key(&self, ex: &Executor, row: &[Datum]) -> Result<Vec<u8>> {
        row_key_bytes(ex, &self.types, &self.slots, row)
    }
}

/// DISTINCT / DISTINCT ON operator (module docs).
struct DistinctOp {
    /// `true` for `DistinctStrategy::Hashed`, `false` for `Sorted`.
    hashed: bool,
    input: Box<dyn RowSource>,
    key: KeySpec,
    /// Hashed mode: keys already emitted.
    seen: HashSet<Vec<u8>>,
    /// Sorted mode: key of the previous input row.
    last: Option<Vec<u8>>,
    counter: u32,
}

impl RowSource for DistinctOp {
    fn next(&mut self, ex: &mut Executor) -> Result<Option<Row>> {
        loop {
            let row = match self.input.next(ex)? {
                Some(r) => r,
                None => return Ok(None),
            };
            ex.tick(&mut self.counter)?;
            let key = self.key.key(ex, &row)?;
            if self.hashed {
                if self.seen.insert(key) {
                    return Ok(Some(row));
                }
            } else {
                // Sorted input: equal keys are adjacent, so only the previous row is compared.
                let fresh = self.last.as_ref() != Some(&key);
                self.last = Some(key);
                if fresh {
                    return Ok(Some(row));
                }
            }
        }
    }

    fn rescan(&mut self, ex: &mut Executor, params: &[Datum]) -> Result<()> {
        self.input.rescan(ex, params)?;
        self.seen.clear();
        self.last = None;
        Ok(())
    }
}

/// INTERSECT / EXCEPT [ALL] operator (module docs). `right_counts` is the multiset of the right input,
/// consumed in place by the `*All` variants (the remaining count per key).
struct SetOpOp {
    cmd: SetOpCmd,
    left: Box<dyn RowSource>,
    right: Box<dyn RowSource>,
    key: KeySpec,
    right_counts: HashMap<Vec<u8>, u64>,
    right_drained: bool,
    /// Distinct variants: keys already emitted from the left input.
    seen: HashSet<Vec<u8>>,
    counter: u32,
}

impl SetOpOp {
    /// Drain the right input into `right_counts` (once per pass).
    fn drain_right(&mut self, ex: &mut Executor) -> Result<()> {
        while let Some(row) = self.right.next(ex)? {
            ex.tick(&mut self.counter)?;
            let key = self.key.key(ex, &row)?;
            *self.right_counts.entry(key).or_insert(0) += 1;
        }
        self.right_drained = true;
        Ok(())
    }

    /// Whether a left row with this key is emitted. Consumes one right occurrence for the `*All`
    /// variants (PG: `nodeSetOp.c`, per-group counters `first_flag`/`numLeft`/`numRight`).
    fn admit(&mut self, key: Vec<u8>) -> bool {
        match self.cmd {
            SetOpCmd::Intersect => {
                let in_right = self.right_counts.contains_key(&key);
                in_right && self.seen.insert(key)
            }
            SetOpCmd::IntersectAll => match self.right_counts.get_mut(&key) {
                Some(c) if *c > 0 => {
                    *c -= 1;
                    true
                }
                _ => false,
            },
            SetOpCmd::Except => {
                let in_right = self.right_counts.contains_key(&key);
                !in_right && self.seen.insert(key)
            }
            SetOpCmd::ExceptAll => match self.right_counts.get_mut(&key) {
                // The first `right_count` occurrences cancel out against the right side.
                Some(c) if *c > 0 => {
                    *c -= 1;
                    false
                }
                _ => true,
            },
        }
    }
}

impl RowSource for SetOpOp {
    fn next(&mut self, ex: &mut Executor) -> Result<Option<Row>> {
        if !self.right_drained {
            self.drain_right(ex)?;
        }
        loop {
            let row = match self.left.next(ex)? {
                Some(r) => r,
                None => return Ok(None),
            };
            ex.tick(&mut self.counter)?;
            let key = self.key.key(ex, &row)?;
            if self.admit(key) {
                return Ok(Some(row));
            }
        }
    }

    fn rescan(&mut self, ex: &mut Executor, params: &[Datum]) -> Result<()> {
        self.left.rescan(ex, params)?;
        self.right.rescan(ex, params)?;
        self.right_counts.clear();
        self.right_drained = false;
        self.seen.clear();
        Ok(())
    }
}

/// Build a `Distinct` operator (module docs). Key slots are validated against the input width.
pub fn build_distinct(node: &PlanNode, n: &DistinctNode, b: &mut BuildCtx) -> Result<Box<dyn RowSource>> {
    let _ = node;
    let mut slots: Vec<usize> = Vec::with_capacity(n.key_slots.len());
    let mut types: Vec<TypeId> = Vec::with_capacity(n.key_slots.len());
    for &s in &n.key_slots {
        let i = s as usize;
        let col = n
            .input
            .out
            .get(i)
            .ok_or_else(|| PgError::internal(format!("DISTINCT key slot {s} out of range")))?;
        slots.push(i);
        types.push(col.ty);
    }
    let input = b.child(&n.input)?;
    Ok(Box::new(DistinctOp {
        hashed: matches!(n.strategy, DistinctStrategy::Hashed),
        input,
        key: KeySpec { types, slots },
        seen: HashSet::new(),
        last: None,
        counter: 0,
    }))
}

/// Build an INTERSECT / EXCEPT operator (module docs). Both inputs must have the same width; all
/// slots form the key.
pub fn build_setop(node: &PlanNode, n: &SetOpNode, b: &mut BuildCtx) -> Result<Box<dyn RowSource>> {
    let _ = node;
    let width = n.left.width();
    if n.right.width() != width {
        return Err(PgError::internal("set operation inputs have different column counts"));
    }
    let types: Vec<TypeId> = n.left.out_types();
    let slots: Vec<usize> = (0..width).collect();
    let left = b.child(&n.left)?;
    let right = b.child(&n.right)?;
    Ok(Box::new(SetOpOp {
        cmd: n.cmd,
        left,
        right,
        key: KeySpec { types, slots },
        right_counts: HashMap::new(),
        right_drained: false,
        seen: HashSet::new(),
        counter: 0,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::context::{ExecContext, NoGuc, NoServices, SessionEnv};
    use crate::memtxn::MemTxn;
    use crate::runtime::Runtime;
    use pgarbor_types::context::IoConfig;
    use std::cell::Cell;

    /// Rows served from memory (stands in for a child operator).
    struct VecSource {
        rows: Vec<Row>,
        pos: usize,
    }

    impl RowSource for VecSource {
        fn next(&mut self, _ex: &mut Executor) -> Result<Option<Row>> {
            if self.pos < self.rows.len() {
                self.pos += 1;
                Ok(Some(self.rows[self.pos - 1].clone()))
            } else {
                Ok(None)
            }
        }
        fn rescan(&mut self, _ex: &mut Executor, _params: &[Datum]) -> Result<()> {
            self.pos = 0;
            Ok(())
        }
    }

    fn ints(vals: &[i32]) -> Vec<Row> {
        vals.iter().map(|v| vec![Datum::Int4(*v)]).collect()
    }

    fn to_ints(rows: &[Row]) -> Vec<i32> {
        rows.iter()
            .map(|r| match &r[0] {
                Datum::Int4(v) => *v,
                _ => panic!("test expects int4 values"),
            })
            .collect()
    }

    fn key1() -> KeySpec {
        KeySpec { types: vec![TypeId::INT4], slots: vec![0] }
    }

    fn setop(cmd: SetOpCmd, left: Vec<Row>, right: Vec<Row>) -> SetOpOp {
        SetOpOp {
            cmd,
            left: Box::new(VecSource { rows: left, pos: 0 }),
            right: Box::new(VecSource { rows: right, pos: 0 }),
            key: key1(),
            right_counts: HashMap::new(),
            right_drained: false,
            seen: HashSet::new(),
            counter: 0,
        }
    }

    fn distinct(hashed: bool, input: Vec<Row>) -> DistinctOp {
        DistinctOp {
            hashed,
            input: Box::new(VecSource { rows: input, pos: 0 }),
            key: key1(),
            seen: HashSet::new(),
            last: None,
            counter: 0,
        }
    }

    fn drain(ex: &mut Executor, op: &mut dyn RowSource) -> Result<Vec<Row>> {
        let mut out = Vec::new();
        while let Some(r) = op.next(ex)? {
            out.push(r);
        }
        Ok(out)
    }

    /// Run `f` with a real executor over an in-memory transaction and bootstrapped catalog.
    fn run_ex<T>(f: impl FnOnce(&mut Executor) -> Result<T>) -> Result<T> {
        let cell = Cell::new(1u64);
        let (_kv, env) = pgarbor_catalog::testing::bootstrapped()?;
        let g = NoGuc;
        let s = NoServices;
        let mut txn = MemTxn::new();
        let se = SessionEnv::new(env, IoConfig::default(), &g, &s, &cell, 0, 0);
        let mut cx = ExecContext::new(&mut txn, se);
        let mut rt = Runtime::default();
        let mut ex = Executor::new(&mut rt, &mut cx);
        f(&mut ex)
    }

    #[test]
    #[ignore = "needs EXE-22"]
    fn hashed_distinct_keeps_first_row_nulls_equal() -> Result<()> {
        let rows = vec![
            vec![Datum::Int4(1)],
            vec![Datum::Null],
            vec![Datum::Int4(1)],
            vec![Datum::Null],
            vec![Datum::Int4(2)],
        ];
        let got = run_ex(|ex| {
            let mut op = distinct(true, rows.clone());
            drain(ex, &mut op)
        })?;
        assert_eq!(got.len(), 3);
        assert_eq!(got[0], vec![Datum::Int4(1)]);
        assert_eq!(got[1], vec![Datum::Null]);
        assert_eq!(got[2], vec![Datum::Int4(2)]);
        Ok(())
    }

    #[test]
    #[ignore = "needs EXE-22"]
    fn sorted_distinct_only_compares_adjacent_rows() -> Result<()> {
        // Sorted input: 1 1 2 2 2 3 -> 1 2 3. A repeat after a different key is emitted again,
        // which is the defined behavior of the sorted strategy on unsorted input.
        let got = run_ex(|ex| {
            let mut op = distinct(false, ints(&[1, 1, 2, 2, 2, 3]));
            drain(ex, &mut op)
        })?;
        assert_eq!(to_ints(&got), vec![1, 2, 3]);
        let got = run_ex(|ex| {
            let mut op = distinct(false, ints(&[1, 2, 1]));
            drain(ex, &mut op)
        })?;
        assert_eq!(to_ints(&got), vec![1, 2, 1]);
        Ok(())
    }

    #[test]
    #[ignore = "needs EXE-22"]
    fn intersect_all_emits_min_counts() -> Result<()> {
        let got = run_ex(|ex| {
            let mut op = setop(SetOpCmd::IntersectAll, ints(&[1, 1, 1, 2, 3]), ints(&[1, 1, 2, 2]));
            drain(ex, &mut op)
        })?;
        assert_eq!(to_ints(&got), vec![1, 1, 2]);
        Ok(())
    }

    #[test]
    #[ignore = "needs EXE-22"]
    fn except_all_emits_max_counts() -> Result<()> {
        let got = run_ex(|ex| {
            let mut op = setop(SetOpCmd::ExceptAll, ints(&[1, 1, 1, 2, 3]), ints(&[1, 2, 2, 4]));
            drain(ex, &mut op)
        })?;
        assert_eq!(to_ints(&got), vec![1, 1, 3]);
        Ok(())
    }

    #[test]
    #[ignore = "needs EXE-22"]
    fn intersect_and_except_are_distinct_in_left_order() -> Result<()> {
        let got = run_ex(|ex| {
            let mut op = setop(SetOpCmd::Intersect, ints(&[3, 1, 3, 2, 1]), ints(&[1, 3, 3]));
            drain(ex, &mut op)
        })?;
        assert_eq!(to_ints(&got), vec![3, 1]);
        let got = run_ex(|ex| {
            let mut op = setop(SetOpCmd::Except, ints(&[3, 1, 3, 2, 1]), ints(&[1]));
            drain(ex, &mut op)
        })?;
        assert_eq!(to_ints(&got), vec![3, 2]);
        Ok(())
    }

    #[test]
    #[ignore = "needs EXE-22"]
    fn setop_rescan_restarts_both_inputs() -> Result<()> {
        let got = run_ex(|ex| {
            let mut op = setop(SetOpCmd::IntersectAll, ints(&[1, 2]), ints(&[1, 2]));
            let first = drain(ex, &mut op)?;
            op.rescan(ex, &[])?;
            let second = drain(ex, &mut op)?;
            Ok((first, second))
        })?;
        assert_eq!(to_ints(&got.0), vec![1, 2]);
        assert_eq!(to_ints(&got.1), vec![1, 2]);
        Ok(())
    }
}
