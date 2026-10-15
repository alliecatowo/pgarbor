//! Hash join (lease EXE-17, north-star). PG: `nodeHashjoin.c`, `nodeHash.c`. Layout: plan.rs
//! `HashJoinNode` (INNER child = build side).
//!
//! ## Build
//! On the first `next` (and again after `rescan` if `ex.epoch()` changed since the build, otherwise
//! the table is reused): drain the inner child; for each inner row evaluate `inner_keys` over it; a row
//! with ANY NULL key can never match (`=` semantics) and is not inserted (but it is remembered for
//! `Right`/`Full` unmatched output). Hash table: `HashMap<Vec<u8>, Vec<usize>>` from the key bytes to
//! indexes into `Vec<InnerEntry{row, matched: bool}>`, where the key bytes are
//! `keycodec::encode_key_vec(env, [KeyColumn::asc(ty_i)], key values)` (equal SQL values <=> equal
//! bytes by the keycodec contract, so no separate hash/eq callbacks are needed). `outer_keys[i].ty`
//! and `inner_keys[i].ty` must be identical (the planner inserts casts); if not, XX000
//! `hash join key types differ`.
//!
//! ## Probe, per outer row `o`
//! Evaluate `outer_keys` over `o`. NULL key (any column) -> no match. Candidates = the bucket; for each
//! candidate evaluate the residual `qual` over `o ++ inner` (`eval_opt_qual`); a candidate satisfying it
//! is a match (mark `matched`).
//! * `Inner`: emit `o ++ inner` per match.   * `Left`: matches, else `o ++ NULLs` if none.
//! * `Semi`: emit `o` at the first match.   * `Anti`: emit `o` if there was no match.
//! * `Right`: matches as Inner; after the outer side is exhausted emit `NULLs ++ inner` for every inner
//!   entry (including NULL-key rows) never matched.   * `Full`: Left behavior plus the Right tail.
//! Multiple matches produce multiple output rows; emit lazily (keep a pending-output queue so `next`
//! returns one row per call). Output row = `outer ++ inner` (Semi/Anti: outer only).
//! `rescan(params)`: rescan the outer child; the build side is reused when the epoch is unchanged,
//! otherwise rebuilt after rescanning the inner child.
use std::collections::{HashMap, VecDeque};

use pgarbor_common::{PgError, Result};
use pgarbor_plan::plan::{HashJoinNode, JoinType, PlanNode};
use pgarbor_sql::ir::BoundExpr;
use pgarbor_types::keycodec::{encode_key_vec, KeyColumn};
use pgarbor_types::Datum;

use crate::build::BuildCtx;
use crate::eval::{eval, eval_opt_qual};
use crate::runtime::{Executor, Row, RowSource};

/// One inner (build side) row.
struct InnerEntry {
    row: Row,
    /// Satisfied the residual qual for at least one outer row (Right/Full tail).
    matched: bool,
}

/// The outer row currently being probed.
struct Probe {
    row: Row,
    /// Key bytes of the outer row; `None` when a key value is NULL (no candidates).
    key: Option<Vec<u8>>,
    /// Next position in the candidate bucket.
    next_cand: usize,
    /// Some candidate satisfied the residual qual.
    matched: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Phase {
    /// Pull outer rows and probe them.
    Probe,
    /// Outer side exhausted: emit never-matched inner rows (Right/Full).
    Tail,
    /// Exhausted; every further call returns `None`.
    Done,
}

/// The hash join operator. Owns both children and copies of every expression it needs.
struct HashJoinOp {
    join_type: JoinType,
    outer: Box<dyn RowSource>,
    inner: Box<dyn RowSource>,
    outer_keys: Vec<BoundExpr>,
    inner_keys: Vec<BoundExpr>,
    /// One ascending key column per join key, shared by both sides (`encode_key_vec` input).
    key_cols: Vec<KeyColumn>,
    qual: Option<BoundExpr>,
    outer_width: usize,
    inner_width: usize,
    /// Right/Full: NULL-key inner rows are kept in `entries` for the unmatched tail.
    need_tail: bool,
    /// The table in `entries`/`table` is valid (built and not invalidated by a rescan).
    built: bool,
    /// `Executor::epoch()` observed when the table was built.
    built_epoch: u64,
    table: HashMap<Vec<u8>, Vec<usize>>,
    entries: Vec<InnerEntry>,
    phase: Phase,
    probe: Option<Probe>,
    /// Output rows produced but not yet returned by `next`.
    pending: VecDeque<Row>,
    /// Cursor over `entries` for the Right/Full tail.
    tail_pos: usize,
    /// Loop counter for interrupt polling (`Executor::tick`).
    ticks: u32,
}

impl HashJoinOp {
    /// Validate the key lists and build the operator. Key types are compared by type OID: keycodec
    /// encodes values by OID only, so a typmod difference (`numeric(10,2)` vs `numeric(12,2)`) cannot
    /// change the key bytes. Callers pass `outer_keys`/`inner_keys` in matching order.
    #[allow(clippy::too_many_arguments)]
    fn new(
        join_type: JoinType,
        outer: Box<dyn RowSource>,
        inner: Box<dyn RowSource>,
        outer_keys: Vec<BoundExpr>,
        inner_keys: Vec<BoundExpr>,
        qual: Option<BoundExpr>,
        outer_width: usize,
        inner_width: usize,
    ) -> Result<HashJoinOp> {
        if outer_keys.len() != inner_keys.len() {
            return Err(PgError::internal("hash join key count differs"));
        }
        let mut key_cols = Vec::with_capacity(outer_keys.len());
        for (o, i) in outer_keys.iter().zip(inner_keys.iter()) {
            if o.ty.oid != i.ty.oid {
                return Err(PgError::internal("hash join key types differ"));
            }
            key_cols.push(KeyColumn::asc(o.ty));
        }
        Ok(HashJoinOp {
            join_type,
            outer,
            inner,
            outer_keys,
            inner_keys,
            key_cols,
            qual,
            outer_width,
            inner_width,
            need_tail: matches!(join_type, JoinType::Right | JoinType::Full),
            built: false,
            built_epoch: 0,
            table: HashMap::new(),
            entries: Vec::new(),
            phase: Phase::Probe,
            probe: None,
            pending: VecDeque::new(),
            tail_pos: 0,
            ticks: 0,
        })
    }

    /// Drain the inner child into the hash table (module docs, "Build").
    fn build(&mut self, ex: &mut Executor) -> Result<()> {
        self.table.clear();
        self.entries.clear();
        while let Some(row) = self.inner.next(ex)? {
            ex.tick(&mut self.ticks)?;
            match eval_key(ex, &self.inner_keys, &self.key_cols, &row)? {
                Some(key) => {
                    let idx = self.entries.len();
                    self.entries.push(InnerEntry { row, matched: false });
                    self.table.entry(key).or_default().push(idx);
                }
                None => {
                    if self.need_tail {
                        self.entries.push(InnerEntry { row, matched: false });
                    }
                }
            }
        }
        self.built = true;
        self.built_epoch = ex.epoch();
        Ok(())
    }
}

impl RowSource for HashJoinOp {
    fn next(&mut self, ex: &mut Executor) -> Result<Option<Row>> {
        if !self.built {
            self.build(ex)?;
        }
        loop {
            if let Some(row) = self.pending.pop_front() {
                return Ok(Some(row));
            }
            match self.phase {
                Phase::Done => return Ok(None),
                Phase::Tail => {
                    while self.tail_pos < self.entries.len() {
                        let i = self.tail_pos;
                        self.tail_pos += 1;
                        let entry = &self.entries[i];
                        if !entry.matched {
                            let mut row = vec![Datum::Null; self.outer_width];
                            row.extend(entry.row.iter().cloned());
                            return Ok(Some(row));
                        }
                    }
                    self.phase = Phase::Done;
                    return Ok(None);
                }
                Phase::Probe => {}
            }

            ex.tick(&mut self.ticks)?;
            let mut probe = match self.probe.take() {
                Some(p) => p,
                None => match self.outer.next(ex)? {
                    Some(row) => {
                        let key = eval_key(ex, &self.outer_keys, &self.key_cols, &row)?;
                        Probe { row, key, next_cand: 0, matched: false }
                    }
                    None => {
                        self.phase = if self.need_tail { Phase::Tail } else { Phase::Done };
                        self.tail_pos = 0;
                        continue;
                    }
                },
            };

            // Examine the next candidate of this outer row's bucket (one per loop turn, so output
            // stays lazy and interrupts are polled between candidates).
            let cand = probe
                .key
                .as_ref()
                .and_then(|k| self.table.get(k))
                .and_then(|bucket| bucket.get(probe.next_cand))
                .copied();
            match cand {
                Some(idx) => {
                    probe.next_cand += 1;
                    let is_match = match &self.qual {
                        None => true,
                        Some(_) => {
                            let combined = concat(&probe.row, &self.entries[idx].row);
                            eval_opt_qual(ex, &self.qual, &combined)?
                        }
                    };
                    let mut keep = true;
                    if is_match {
                        self.entries[idx].matched = true;
                        probe.matched = true;
                        match self.join_type {
                            JoinType::Semi => {
                                self.pending.push_back(std::mem::take(&mut probe.row));
                                keep = false;
                            }
                            JoinType::Anti => {
                                // A match disqualifies the outer row: nothing is emitted for it.
                                keep = false;
                            }
                            _ => {
                                let row = concat(&probe.row, &self.entries[idx].row);
                                self.pending.push_back(row);
                            }
                        }
                    }
                    if keep {
                        self.probe = Some(probe);
                    }
                }
                None => {
                    // Bucket exhausted: this outer row is finished.
                    if !probe.matched {
                        match self.join_type {
                            JoinType::Left | JoinType::Full => {
                                let mut row = probe.row;
                                row.extend(std::iter::repeat_n(Datum::Null, self.inner_width));
                                self.pending.push_back(row);
                            }
                            JoinType::Anti => self.pending.push_back(probe.row),
                            _ => {}
                        }
                    }
                }
            }
        }
    }

    fn rescan(&mut self, ex: &mut Executor, params: &[Datum]) -> Result<()> {
        // The epoch is read before the children are rescanned: their own frames must not count as
        // a change of the build side's inputs. Parameterized rescans always rebuild.
        let reuse = self.built && params.is_empty() && ex.epoch() == self.built_epoch;
        self.outer.rescan(ex, params)?;
        if reuse {
            for entry in self.entries.iter_mut() {
                entry.matched = false;
            }
        } else {
            self.built = false;
            self.table.clear();
            self.entries.clear();
            self.inner.rescan(ex, params)?;
        }
        self.phase = Phase::Probe;
        self.probe = None;
        self.pending.clear();
        self.tail_pos = 0;
        Ok(())
    }
}

/// Key bytes of `row` under `exprs` (module docs), `None` when any key value is NULL. Every key
/// expression is evaluated even after a NULL, as PG evaluates the whole key list.
fn eval_key(ex: &mut Executor, exprs: &[BoundExpr], cols: &[KeyColumn], row: &[Datum]) -> Result<Option<Vec<u8>>> {
    let mut values: Vec<Datum> = Vec::with_capacity(exprs.len());
    let mut has_null = false;
    for e in exprs {
        let v = eval(ex, e, row)?;
        has_null |= v.is_null();
        values.push(v);
    }
    if has_null {
        return Ok(None);
    }
    let bytes = encode_key_vec(&ex.se().env, cols, &values)?;
    Ok(Some(bytes))
}

/// `left ++ right` as a new row.
fn concat(left: &[Datum], right: &[Datum]) -> Row {
    let mut row = Vec::with_capacity(left.len() + right.len());
    row.extend(left.iter().cloned());
    row.extend(right.iter().cloned());
    row
}

pub fn build_hash_join(node: &PlanNode, n: &HashJoinNode, b: &mut BuildCtx) -> Result<Box<dyn RowSource>> {
    let _ = node;
    let outer = b.child(&n.outer)?;
    let inner = b.child(&n.inner)?;
    let op = HashJoinOp::new(
        n.join_type,
        outer,
        inner,
        n.outer_keys.clone(),
        n.inner_keys.clone(),
        n.qual.clone(),
        n.outer.width(),
        n.inner.width(),
    )?;
    Ok(Box::new(op))
}

#[cfg(test)]
mod tests {
    use std::cell::Cell;

    use pgarbor_types::context::IoConfig;
    use pgarbor_types::TypeId;

    use super::*;
    use crate::context::{ExecContext, NoGuc, NoServices, SessionEnv};
    use crate::memtxn::MemTxn;
    use crate::runtime::Runtime;

    /// In-memory row source; `rescan` restarts it.
    struct VecSource {
        rows: Vec<Row>,
        pos: usize,
    }

    impl RowSource for VecSource {
        fn next(&mut self, _ex: &mut Executor) -> Result<Option<Row>> {
            match self.rows.get(self.pos) {
                Some(r) => {
                    self.pos += 1;
                    Ok(Some(r.clone()))
                }
                None => Ok(None),
            }
        }
        fn rescan(&mut self, _ex: &mut Executor, _params: &[Datum]) -> Result<()> {
            self.pos = 0;
            Ok(())
        }
    }

    fn src(rows: Vec<Row>) -> Box<dyn RowSource> {
        Box::new(VecSource { rows, pos: 0 })
    }

    /// `(key int4, payload int4)`; a NULL key when `k` is None.
    fn int_row(k: Option<i32>, payload: i32) -> Row {
        vec![k.map_or(Datum::Null, Datum::Int4), Datum::Int4(payload)]
    }

    /// `(key int4, flag bool)`.
    fn flag_row(k: Option<i32>, flag: bool) -> Row {
        vec![k.map_or(Datum::Null, Datum::Int4), Datum::Bool(flag)]
    }

    /// Run `f` with an executor over an empty in-memory transaction.
    fn with_executor<T>(f: impl FnOnce(&mut Executor) -> Result<T>) -> Result<T> {
        let mut txn = MemTxn::new();
        let (_kv, env) = pgarbor_catalog::testing::bootstrapped()?;
        let guc = NoGuc;
        let services = NoServices;
        let rng = Cell::new(1u64);
        let se = SessionEnv::new(env, IoConfig::default(), &guc, &services, &rng, 0, 0);
        let mut cx = ExecContext::new(&mut txn, se);
        let mut rt = Runtime::default();
        let mut ex = Executor::new(&mut rt, &mut cx);
        f(&mut ex)
    }

    fn drain(op: &mut HashJoinOp, ex: &mut Executor) -> Result<Vec<Row>> {
        let mut out = Vec::new();
        while let Some(r) = op.next(ex)? {
            out.push(r);
        }
        Ok(out)
    }

    /// Hash join over column 0 of both sides (INT4 keys), two-column rows.
    fn join(join_type: JoinType, outer: Vec<Row>, inner: Vec<Row>, qual: Option<BoundExpr>) -> Result<HashJoinOp> {
        HashJoinOp::new(
            join_type,
            src(outer),
            src(inner),
            vec![BoundExpr::input_ref(0, TypeId::INT4)],
            vec![BoundExpr::input_ref(0, TypeId::INT4)],
            qual,
            2,
            2,
        )
    }

    fn run(join_type: JoinType, outer: Vec<Row>, inner: Vec<Row>, qual: Option<BoundExpr>) -> Result<Vec<Row>> {
        let mut op = join(join_type, outer, inner, qual)?;
        with_executor(|ex| drain(&mut op, ex))
    }

    fn cat(a: Row, b: Row) -> Row {
        concat(&a, &b)
    }

    #[test]
    fn mismatched_key_types_are_rejected() {
        let r = HashJoinOp::new(
            JoinType::Inner,
            src(Vec::new()),
            src(Vec::new()),
            vec![BoundExpr::input_ref(0, TypeId::INT4)],
            vec![BoundExpr::input_ref(0, TypeId::INT8)],
            None,
            2,
            2,
        );
        assert!(r.is_err());
    }

    #[test]
    fn key_count_mismatch_is_rejected() {
        let r = HashJoinOp::new(
            JoinType::Inner,
            src(Vec::new()),
            src(Vec::new()),
            vec![BoundExpr::input_ref(0, TypeId::INT4)],
            Vec::new(),
            None,
            2,
            2,
        );
        assert!(r.is_err());
    }

    #[test]
    #[ignore = "needs EXE-05"]
    fn inner_join_emits_cross_product_per_key() -> Result<()> {
        let outer = vec![int_row(Some(1), 10), int_row(Some(1), 11), int_row(Some(2), 12)];
        let inner = vec![int_row(Some(1), 20), int_row(Some(1), 21), int_row(Some(3), 22)];
        let got = run(JoinType::Inner, outer, inner, None)?;
        assert_eq!(
            got,
            vec![
                cat(int_row(Some(1), 10), int_row(Some(1), 20)),
                cat(int_row(Some(1), 10), int_row(Some(1), 21)),
                cat(int_row(Some(1), 11), int_row(Some(1), 20)),
                cat(int_row(Some(1), 11), int_row(Some(1), 21)),
            ]
        );
        Ok(())
    }

    #[test]
    #[ignore = "needs EXE-05"]
    fn left_join_pads_unmatched_outer_rows() -> Result<()> {
        let outer = vec![int_row(Some(1), 10), int_row(Some(3), 11)];
        let inner = vec![int_row(Some(1), 20)];
        let got = run(JoinType::Left, outer, inner, None)?;
        assert_eq!(
            got,
            vec![
                cat(int_row(Some(1), 10), int_row(Some(1), 20)),
                cat(int_row(Some(3), 11), vec![Datum::Null, Datum::Null]),
            ]
        );
        Ok(())
    }

    #[test]
    #[ignore = "needs EXE-05"]
    fn right_join_emits_unmatched_inner_including_null_keys() -> Result<()> {
        let outer = vec![int_row(Some(1), 10)];
        let inner = vec![int_row(Some(1), 20), int_row(Some(2), 21), int_row(None, 22)];
        let got = run(JoinType::Right, outer, inner, None)?;
        let nulls = || vec![Datum::Null, Datum::Null];
        assert_eq!(
            got,
            vec![
                cat(int_row(Some(1), 10), int_row(Some(1), 20)),
                cat(nulls(), int_row(Some(2), 21)),
                cat(nulls(), int_row(None, 22)),
            ]
        );
        Ok(())
    }

    #[test]
    #[ignore = "needs EXE-05"]
    fn full_join_combines_left_padding_and_right_tail() -> Result<()> {
        let outer = vec![int_row(Some(1), 10), int_row(Some(3), 11), int_row(None, 12)];
        let inner = vec![int_row(Some(1), 20), int_row(Some(2), 21), int_row(None, 22)];
        let got = run(JoinType::Full, outer, inner, None)?;
        let nulls = || vec![Datum::Null, Datum::Null];
        assert_eq!(
            got,
            vec![
                cat(int_row(Some(1), 10), int_row(Some(1), 20)),
                cat(int_row(Some(3), 11), nulls()),
                cat(int_row(None, 12), nulls()),
                cat(nulls(), int_row(Some(2), 21)),
                cat(nulls(), int_row(None, 22)),
            ]
        );
        Ok(())
    }

    #[test]
    #[ignore = "needs EXE-05"]
    fn semi_join_emits_each_outer_row_once() -> Result<()> {
        let outer = vec![int_row(Some(1), 10), int_row(Some(1), 11), int_row(Some(2), 12)];
        let inner = vec![int_row(Some(1), 20), int_row(Some(1), 21)];
        let got = run(JoinType::Semi, outer, inner, None)?;
        assert_eq!(got, vec![int_row(Some(1), 10), int_row(Some(1), 11)]);
        Ok(())
    }

    #[test]
    #[ignore = "needs EXE-05"]
    fn anti_join_emits_rows_without_match() -> Result<()> {
        let outer = vec![int_row(Some(1), 10), int_row(Some(2), 11), int_row(None, 12)];
        let inner = vec![int_row(Some(1), 20)];
        let got = run(JoinType::Anti, outer, inner, None)?;
        assert_eq!(got, vec![int_row(Some(2), 11), int_row(None, 12)]);
        Ok(())
    }

    #[test]
    #[ignore = "needs EXE-05"]
    fn null_keys_never_match() -> Result<()> {
        let outer = vec![int_row(None, 10)];
        let inner = vec![int_row(None, 20)];
        assert_eq!(run(JoinType::Inner, outer.clone(), inner.clone(), None)?, Vec::<Row>::new());
        assert_eq!(run(JoinType::Anti, outer, inner, None)?, vec![int_row(None, 10)]);
        Ok(())
    }

    #[test]
    #[ignore = "needs EXE-05"]
    fn residual_qual_failing_for_all_candidates_leaves_left_row_unmatched() -> Result<()> {
        // Combined row is [outer key, outer flag, inner key, inner flag]; qual = inner flag.
        let qual = Some(BoundExpr::input_ref(3, TypeId::BOOL));
        let outer = vec![flag_row(Some(1), true)];
        let inner = vec![flag_row(Some(1), false), flag_row(Some(1), false)];
        let got = run(JoinType::Left, outer, inner, qual)?;
        assert_eq!(got, vec![cat(flag_row(Some(1), true), vec![Datum::Null, Datum::Null])]);
        Ok(())
    }

    #[test]
    #[ignore = "needs EXE-05"]
    fn rescan_replays_full_join_output() -> Result<()> {
        let outer = vec![int_row(Some(1), 10), int_row(Some(3), 11)];
        let inner = vec![int_row(Some(1), 20), int_row(Some(2), 21)];
        let mut op = join(JoinType::Full, outer, inner, None)?;
        with_executor(|ex| {
            let first = drain(&mut op, ex)?;
            op.rescan(ex, &[])?;
            let second = drain(&mut op, ex)?;
            assert_eq!(first, second);
            Ok(())
        })
    }
}
