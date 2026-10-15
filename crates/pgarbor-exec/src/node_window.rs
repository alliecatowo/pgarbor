//! Window aggregation operator (lease EXE-20, sonnet). PG: `nodeWindowAgg.c`. Layout: plan.rs
//! `WindowAggNode`: input sorted by `partition_by` then `order_by`; output row = input row ++ one
//! value per `funcs`.
//!
//! ## Algorithm
//! 1. Read input rows into a partition buffer until the `partition_by` expressions (evaluated per
//!    row, compared with `compare::not_distinct`) change or the input ends. (A partition is held in
//!    memory entirely: no spilling in v1.)
//! 2. Build `window_funcs::WinPartition`: evaluate the `order_by` key values for every row (via
//!    `sortutil::eval_keys`) and compute peer groups (consecutive rows whose keys are equal under
//!    `compare::not_distinct`; all rows are peers when there is no ORDER BY).
//! 3. For each row `i`, in order, compute its frame [`WinFrame`](crate::window_funcs::WinFrame) from `frame`
//!    ([`FrameSpec`](pgarbor_plan::plan::FrameSpec); offset expressions are evaluated once per
//!    partition-row: a NULL offset -> 22013 `frame starting offset must not be null`/`frame ending offset must
//!    not be null`, negative -> 22013 `frame starting offset must not be negative` (ending likewise)):
//!    * `ROWS`: start/end are row counts relative to `i` (`UNBOUNDED PRECEDING` = 0, `UNBOUNDED
//!      FOLLOWING` = len, `CURRENT ROW` = i itself, `N PRECEDING` = i - N, `N FOLLOWING` = i + N), clamped to
//!      `[0, len]`; end is exclusive (+1).
//!    * `GROUPS`: offsets count peer GROUPS: `CURRENT ROW` start = start of i's peer group, end = end of
//!      i's peer group; `N PRECEDING/FOLLOWING` move by whole groups.
//!    * `RANGE`: `CURRENT ROW` = peer group boundaries; `UNBOUNDED` as above; `N PRECEDING/FOLLOWING`
//!      compares the single ORDER BY key value with `key_i -/+ N` using the type's `+`/`-` operators
//!      (resolve via `pgarbor_types::resolve::oper` for the key type and offset type) honoring
//!      ASC/DESC (for DESC the arithmetic direction flips); NULL keys frame only their peers.
//!      Without ORDER BY only `UNBOUNDED`/`CURRENT ROW` are legal (the binder enforces).
//!    * `EXCLUDE CURRENT ROW` removes row i; `GROUP` removes i's peer group; `TIES` removes the peers
//!      except i. An empty or inverted range gives an empty frame.
//!    * The default frame (no frame clause, `frame_is_default`) arrives as `RANGE UNBOUNDED
//!      PRECEDING .. CURRENT ROW` with ORDER BY, or the whole partition without.
//! 4. For each `WindowCall` of `funcs`:
//!    * pure window function (`window_funcs::is_pure_window_function(name)`): `window_funcs::
//!      eval_window_func(ex, call, &part, i, &frame)`;
//!    * aggregate (`is_agg`): result over the frame rows with `pgarbor_types::agg::make_aggregate`:
//!      evaluate `call.filter` and `call.args` per frame row, skip filtered rows, `transition`, then
//!      `finalize`. Optimize the common growing frame (start fixed, end moving forward) by feeding only
//!      new rows; for sliding frames use `AggState::retract` when it returns `Ok(true)`, otherwise
//!      recompute the frame from scratch. `star` aggregates (`count(*) OVER`) take no args.
//! 5. Emit `input_row ++ results`; rows are emitted in input order, one per `next` call.
//! `rescan(params)` clears buffers and rescans the input.
use std::cmp::Ordering;
use std::collections::VecDeque;

use pgarbor_common::{PgError, Result, SqlState};
use pgarbor_plan::plan::{FrameSpec, PlanNode, WindowAggNode, WindowCall};
use pgarbor_sql::ir::{BoundExpr, FrameBound, FrameExclusion, FrameUnits, OpRef};
use pgarbor_types::agg::{make_aggregate, AggSpec, AggState};
use pgarbor_types::catalog_data::Volatility;
use pgarbor_types::compare::{cmp_sort, not_distinct};
use pgarbor_types::context::{FuncCtx, TypeEnv};
use pgarbor_types::numeric::arith;
use pgarbor_types::resolve::oper::resolve_builtin_operator;
use pgarbor_types::{Datum, Numeric, TypeId};

use crate::build::BuildCtx;
use crate::eval::{eval, eval_exprs, eval_opt_qual};
use crate::eval_call::{call_operator, with_func_ctx};
use crate::runtime::{Executor, Row, RowSource};
use crate::sortutil::{eval_keys, SortKeyRt};
use crate::window_funcs::{eval_window_func, is_pure_window_function, WinFrame, WinPartition};

/// PG `ERRCODE_INVALID_PRECEDING_OR_FOLLOWING_SIZE` (22013), raised by `nodeWindowAgg.c` for NULL and
/// negative frame offsets.
const INVALID_PRECEDING_OR_FOLLOWING_SIZE: SqlState = SqlState(*b"22013");

/// A frame bound with its offset (if any) evaluated for the current row.
enum Bd {
    UnPre,
    Pre(Datum, TypeId),
    Cur,
    Fol(Datum, TypeId),
    UnFol,
}

/// Peer groups of one partition: `groups[g] = (first row, one past last row)`, `group_of[i]` = g of row i.
struct Peers {
    group_of: Vec<usize>,
    groups: Vec<(usize, usize)>,
}

impl Peers {
    fn start(&self, i: usize) -> usize {
        self.groups[self.group_of[i]].0
    }
    fn end(&self, i: usize) -> usize {
        self.groups[self.group_of[i]].1
    }
}

/// The window definition copied out of the plan node (operators own their data).
struct WinSpec {
    partition_by: Vec<BoundExpr>,
    part_types: Vec<TypeId>,
    order_keys: Vec<SortKeyRt>,
    frame: FrameSpec,
    funcs: Vec<WindowCall>,
}

/// Resolved `+` / `-` operators for RANGE offsets, cached per partition.
/// Entries: `((minus, key type oid, offset type oid), operator, result type)`.
#[derive(Default)]
struct RangeOps {
    cache: Vec<((bool, u32, u32), OpRef, TypeId)>,
}

impl RangeOps {
    /// Index of the cached operator for `base op off`, resolving it on first use.
    fn lookup(&mut self, env: &dyn TypeEnv, minus: bool, base: TypeId, off: TypeId) -> Result<usize> {
        let key = (minus, base.oid, off.oid);
        if let Some(pos) = self.cache.iter().position(|e| e.0 == key) {
            return Ok(pos);
        }
        let name = if minus { "-" } else { "+" };
        let r = resolve_builtin_operator(env, name, Some(base.oid), off.oid)?;
        if r.left != Some(base.oid) || r.right != off.oid {
            return Err(PgError::unimplemented(
                "EXE-20",
                format!("RANGE frame offset arithmetic `{name}` on types {} and {} needs argument coercion", base.oid, off.oid),
            ));
        }
        let op = OpRef { op: r.oid, proc: r.proc, name: name.to_string(), volatility: Volatility::Immutable, strict: true };
        self.cache.push((key, op, r.result));
        Ok(self.cache.len() - 1)
    }
}

fn offset_error(is_start: bool, what: &str) -> PgError {
    let side = if is_start { "starting" } else { "ending" };
    PgError::new(INVALID_PRECEDING_OR_FOLLOWING_SIZE, format!("frame {side} offset {what}"))
}

/// Sign test for a RANGE offset value (interval offsets use PG's `interval_sign` span).
fn range_offset_negative(v: &Datum) -> Result<bool> {
    Ok(match v {
        Datum::Int2(x) => *x < 0,
        Datum::Int4(x) => *x < 0,
        Datum::Int8(x) => *x < 0,
        Datum::Float4(x) => *x < 0.0,
        Datum::Float8(x) => *x < 0.0,
        Datum::Numeric(n) => arith::sign(n)?.pg_cmp(&Numeric::zero()) == Ordering::Less,
        Datum::Interval(iv) => iv.cmp_span() < 0,
        other => {
            return Err(PgError::unimplemented(
                "EXE-20",
                format!("RANGE frame offset of type {}", other.variant_name()),
            ))
        }
    })
}

/// Validate one evaluated frame offset: NULL and negative offsets are 22013 (module docs).
fn check_offset(v: &Datum, range: bool, is_start: bool) -> Result<()> {
    if v.is_null() {
        return Err(offset_error(is_start, "must not be null"));
    }
    let negative = if range {
        range_offset_negative(v)?
    } else {
        match v.as_i64() {
            Some(x) => x < 0,
            None => return Err(PgError::internal("window ROWS/GROUPS frame offset is not int8")),
        }
    };
    if negative {
        return Err(offset_error(is_start, "must not be negative"));
    }
    Ok(())
}

/// The integer value of a validated ROWS/GROUPS offset.
fn int_offset(d: &Datum) -> Result<i128> {
    match d.as_i64() {
        Some(x) => Ok(x as i128),
        None => Err(PgError::internal("window ROWS/GROUPS frame offset is not int8")),
    }
}

/// Smallest index in `0..len` for which `pred` is false, assuming `pred` is true on a prefix.
fn partition_point(len: usize, mut pred: impl FnMut(usize) -> Result<bool>) -> Result<usize> {
    let (mut lo, mut hi) = (0usize, len);
    while lo < hi {
        let mid = lo + (hi - lo) / 2;
        if pred(mid)? {
            lo = mid + 1;
        } else {
            hi = mid;
        }
    }
    Ok(lo)
}

/// ROWS bound position (row index; start inclusive, end exclusive), clamped to `[0, len]`.
fn rows_pos(b: &Bd, i: usize, len: usize, is_start: bool) -> Result<usize> {
    let (ii, ll) = (i as i128, len as i128);
    let v = match b {
        Bd::UnPre => 0,
        Bd::UnFol => ll,
        Bd::Cur => {
            if is_start {
                ii
            } else {
                ii + 1
            }
        }
        Bd::Pre(d, _) => {
            let n = int_offset(d)?;
            if is_start {
                ii - n
            } else {
                ii - n + 1
            }
        }
        Bd::Fol(d, _) => {
            let n = int_offset(d)?;
            if is_start {
                ii + n
            } else {
                ii + n + 1
            }
        }
    };
    Ok(v.clamp(0, ll) as usize)
}

/// GROUPS bound position: offsets count whole peer groups (start inclusive, end exclusive).
fn groups_pos(b: &Bd, i: usize, len: usize, peers: &Peers, is_start: bool) -> Result<usize> {
    let g = peers.group_of[i];
    let ng = peers.groups.len();
    let pick = |gi: usize| {
        if is_start {
            peers.groups[gi].0
        } else {
            peers.groups[gi].1
        }
    };
    Ok(match b {
        Bd::UnPre => 0,
        Bd::UnFol => len,
        Bd::Cur => pick(g),
        Bd::Pre(d, _) => {
            let n = int_offset(d)?;
            if n > g as i128 {
                0
            } else {
                pick(g - n as usize)
            }
        }
        Bd::Fol(d, _) => {
            let t = g as i128 + int_offset(d)?;
            if t >= ng as i128 {
                len
            } else {
                pick(t as usize)
            }
        }
    })
}

/// RANGE bound position. `CURRENT ROW` and NULL current keys use the peer group; offsets compare the
/// single ORDER BY key with `key -/+ N` (`+`/`-` resolved for the key type, flipped for DESC).
#[allow(clippy::too_many_arguments)]
fn range_pos(
    ex: &mut Executor,
    ops: &mut RangeOps,
    key: Option<&SortKeyRt>,
    key_vals: &[Vec<Datum>],
    peers: &Peers,
    i: usize,
    b: &Bd,
    is_start: bool,
) -> Result<usize> {
    let len = key_vals.len();
    let (off, off_ty, preceding) = match b {
        Bd::UnPre => return Ok(0),
        Bd::UnFol => return Ok(len),
        Bd::Cur => return Ok(if is_start { peers.start(i) } else { peers.end(i) }),
        Bd::Pre(d, t) => (d, *t, true),
        Bd::Fol(d, t) => (d, *t, false),
    };
    let key = key.ok_or_else(|| PgError::internal("RANGE frame offset without ORDER BY"))?;
    let kv = match key_vals[i].first() {
        Some(v) => v,
        None => return Err(PgError::internal("window ORDER BY key missing")),
    };
    if kv.is_null() {
        return Ok(if is_start { peers.start(i) } else { peers.end(i) });
    }
    // In value terms: ASC preceding = key - N, ASC following = key + N; DESC flips both.
    let minus = preceding != key.descending;
    let pos = {
        let env: &dyn TypeEnv = &ex.se().env;
        ops.lookup(env, minus, key.ty, off_ty)?
    };
    let (op, res_ty) = (&ops.cache[pos].1, ops.cache[pos].2);
    let target = call_operator(ex, op, &[kv.clone(), off.clone()], &[key.ty, off_ty], res_ty)?;

    let env: &dyn TypeEnv = &ex.se().env;
    let (desc, nulls_first, ty) = (key.descending, key.nulls_first, key.ty);
    let cmp_at = |j: usize| -> Result<Ordering> {
        match key_vals[j].first() {
            Some(v) => cmp_sort(env, ty, v, &target, desc, nulls_first),
            None => Err(PgError::internal("window ORDER BY key missing")),
        }
    };
    if is_start {
        // First row at or after the target (in sort order): rows before it are excluded.
        partition_point(len, |j| Ok(cmp_at(j)? == Ordering::Less))
    } else {
        // One past the last row at or before the target.
        partition_point(len, |j| Ok(cmp_at(j)? != Ordering::Greater))
    }
}

/// Evaluate one frame bound for the current row (offset checks in PG order: start, then end).
fn eval_bound(ex: &mut Executor, range: bool, fb: &FrameBound, row: &[Datum], is_start: bool) -> Result<Bd> {
    Ok(match fb {
        FrameBound::UnboundedPreceding => Bd::UnPre,
        FrameBound::CurrentRow => Bd::Cur,
        FrameBound::UnboundedFollowing => Bd::UnFol,
        FrameBound::Preceding(e) => {
            let v = eval(ex, e, row)?;
            check_offset(&v, range, is_start)?;
            Bd::Pre(v, e.ty)
        }
        FrameBound::Following(e) => {
            let v = eval(ex, e, row)?;
            check_offset(&v, range, is_start)?;
            Bd::Fol(v, e.ty)
        }
    })
}

/// Build the frame of row `i` from its resolved positions and the EXCLUDE clause.
fn frame_from_positions(i: usize, start: usize, end: usize, exclusion: FrameExclusion, peers: &Peers) -> WinFrame {
    let end = end.max(start);
    let mut excluded = Vec::new();
    match exclusion {
        FrameExclusion::NoOthers => {}
        FrameExclusion::CurrentRow => {
            if i >= start && i < end {
                excluded.push(i);
            }
        }
        FrameExclusion::Group | FrameExclusion::Ties => {
            let ps = peers.start(i).max(start);
            let pe = peers.end(i).min(end);
            for x in ps..pe {
                if x == i && matches!(exclusion, FrameExclusion::Ties) {
                    continue;
                }
                excluded.push(x);
            }
        }
    }
    WinFrame { start, end, excluded }
}

/// The frame of row `i` (module docs, step 3).
fn compute_frame(
    ex: &mut Executor,
    spec: &WinSpec,
    part: &WinPartition,
    key_vals: &[Vec<Datum>],
    peers: &Peers,
    ops: &mut RangeOps,
    i: usize,
) -> Result<WinFrame> {
    let range = matches!(spec.frame.units, FrameUnits::Range);
    let len = part.rows.len();
    let sb = eval_bound(ex, range, &spec.frame.start, &part.rows[i], true)?;
    let eb = eval_bound(ex, range, &spec.frame.end, &part.rows[i], false)?;
    let (s, e) = match spec.frame.units {
        FrameUnits::Rows => (rows_pos(&sb, i, len, true)?, rows_pos(&eb, i, len, false)?),
        FrameUnits::Groups => (groups_pos(&sb, i, len, peers, true)?, groups_pos(&eb, i, len, peers, false)?),
        FrameUnits::Range => {
            let key = spec.order_keys.first();
            let s = range_pos(ex, ops, key, key_vals, peers, i, &sb, true)?;
            let e = range_pos(ex, ops, key, key_vals, peers, i, &eb, false)?;
            (s, e)
        }
    };
    Ok(frame_from_positions(i, s, e, spec.frame.exclusion, peers))
}

/// True when the two evaluated key vectors are equal under `IS NOT DISTINCT FROM`.
fn same_keys(env: &dyn TypeEnv, types: &[TypeId], a: &[Datum], b: &[Datum]) -> Result<bool> {
    for ((ty, x), y) in types.iter().zip(a.iter()).zip(b.iter()) {
        if !not_distinct(env, *ty, x, y)? {
            return Ok(false);
        }
    }
    Ok(true)
}

/// Peer groups from the ORDER BY key vectors (no ORDER BY: one group for the whole partition).
fn compute_peers(env: &dyn TypeEnv, types: &[TypeId], vals: &[Vec<Datum>]) -> Result<Peers> {
    let n = vals.len();
    let mut group_of = vec![0usize; n];
    let mut groups: Vec<(usize, usize)> = Vec::new();
    let mut j = 0usize;
    while j < n {
        let mut k = j + 1;
        while k < n && same_keys(env, types, &vals[j], &vals[k])? {
            k += 1;
        }
        let g = groups.len();
        groups.push((j, k));
        for slot in group_of.iter_mut().take(k).skip(j) {
            *slot = g;
        }
        j = k;
    }
    Ok(Peers { group_of, groups })
}

/// Evaluate the filter and the arguments of one window aggregate for every row of the partition.
/// `None` = the FILTER rejected the row; `Some(vec![])` for `count(*)`.
fn call_arg_values(ex: &mut Executor, call: &WindowCall, rows: &[Row]) -> Result<Vec<Option<Vec<Datum>>>> {
    let mut out = Vec::with_capacity(rows.len());
    let mut counter = 0u32;
    for row in rows {
        ex.tick(&mut counter)?;
        if !eval_opt_qual(ex, &call.filter, row)? {
            out.push(None);
            continue;
        }
        let args = if call.star { Vec::new() } else { eval_exprs(ex, &call.args, row)? };
        out.push(Some(args));
    }
    Ok(out)
}

/// Result of an aggregate window function for every row (inside the call's `FuncCtx`).
fn agg_column(ex: &Executor, call: &WindowCall, vals: &[Option<Vec<Datum>>], frames: &[WinFrame]) -> Result<Vec<Datum>> {
    let spec = AggSpec {
        agg_oid: call.func,
        arg_types: call.arg_types.clone(),
        result_type: call.result_type,
        direct_args: Vec::new(),
    };
    with_func_ctx(&*ex.cx, &call.arg_types, call.result_type, |ctx| agg_inner(ctx, &spec, vals, frames))
}

/// Aggregate over each frame. The state covers `[lo, hi)` of the partition; a frame that moves forward
/// feeds only the new rows, and rows leaving the front (or EXCLUDEd rows) are removed with `retract`.
/// When `retract` reports no inverse, the state is rebuilt from scratch for that frame.
fn agg_inner(ctx: &FuncCtx, spec: &AggSpec, vals: &[Option<Vec<Datum>>], frames: &[WinFrame]) -> Result<Vec<Datum>> {
    let mut out: Vec<Datum> = Vec::with_capacity(frames.len());
    let mut st: Option<Box<dyn AggState>> = None;
    let (mut lo, mut hi) = (0usize, 0usize);
    for f in frames {
        let mut ok = false;
        if let Some(state) = st.as_mut() {
            if f.start >= lo && f.start <= hi && f.end >= hi {
                ok = true;
                for r in lo..f.start {
                    if let Some(a) = &vals[r] {
                        if !state.retract(a, ctx)? {
                            ok = false;
                            break;
                        }
                    }
                }
                if ok {
                    for r in hi..f.end {
                        if let Some(a) = &vals[r] {
                            state.transition(a, ctx)?;
                        }
                    }
                }
            }
        }
        if !ok {
            let mut state = make_aggregate(spec, ctx)?;
            for r in f.start..f.end {
                if let Some(a) = &vals[r] {
                    state.transition(a, ctx)?;
                }
            }
            st = Some(state);
        }
        lo = f.start;
        hi = f.end;

        let state = st.as_mut().ok_or_else(|| PgError::internal("window aggregate state missing"))?;
        let value = if f.excluded.is_empty() {
            Some(state.finalize(ctx)?)
        } else {
            let mut all = true;
            for &x in &f.excluded {
                if let Some(a) = &vals[x] {
                    if !state.retract(a, ctx)? {
                        all = false;
                        break;
                    }
                }
            }
            if all {
                let v = state.finalize(ctx)?;
                for &x in &f.excluded {
                    if let Some(a) = &vals[x] {
                        state.transition(a, ctx)?;
                    }
                }
                Some(v)
            } else {
                None
            }
        };
        match value {
            Some(v) => out.push(v),
            None => {
                // The state may be half-retracted: drop it and recompute this frame from its rows.
                st = None;
                let mut fresh = make_aggregate(spec, ctx)?;
                for x in f.indexes() {
                    if let Some(a) = &vals[x] {
                        fresh.transition(a, ctx)?;
                    }
                }
                out.push(fresh.finalize(ctx)?);
            }
        }
    }
    Ok(out)
}

/// Compute all window results of one partition and return the rows with the results appended.
fn process_partition(ex: &mut Executor, spec: &WinSpec, rows: Vec<Row>) -> Result<Vec<Row>> {
    let n = rows.len();
    let mut key_vals: Vec<Vec<Datum>> = Vec::with_capacity(n);
    for row in &rows {
        key_vals.push(eval_keys(ex, &spec.order_keys, row)?);
    }
    let order_types: Vec<TypeId> = spec.order_keys.iter().map(|k| k.ty).collect();
    let peers = {
        let env: &dyn TypeEnv = &ex.se().env;
        compute_peers(env, &order_types, &key_vals)?
    };
    let mut peer_start = Vec::with_capacity(n);
    let mut peer_end = Vec::with_capacity(n);
    for i in 0..n {
        peer_start.push(peers.start(i));
        peer_end.push(peers.end(i));
    }
    let part = WinPartition { rows, peer_start, peer_end };

    let mut ops = RangeOps::default();
    let mut frames: Vec<WinFrame> = Vec::with_capacity(n);
    let mut counter = 0u32;
    for i in 0..n {
        ex.tick(&mut counter)?;
        frames.push(compute_frame(ex, spec, &part, &key_vals, &peers, &mut ops, i)?);
    }

    let mut cols: Vec<Vec<Datum>> = Vec::with_capacity(spec.funcs.len());
    for call in &spec.funcs {
        if call.is_agg {
            let vals = call_arg_values(ex, call, &part.rows)?;
            cols.push(agg_column(ex, call, &vals, &frames)?);
        } else if is_pure_window_function(&call.name) {
            let mut col = Vec::with_capacity(n);
            for i in 0..n {
                ex.tick(&mut counter)?;
                col.push(eval_window_func(ex, call, &part, i, &frames[i])?);
            }
            cols.push(col);
        } else {
            return Err(PgError::internal(format!("unsupported window function {}", call.name)));
        }
    }

    let WinPartition { rows, .. } = part;
    let mut out = rows;
    for col in cols {
        for (row, v) in out.iter_mut().zip(col) {
            row.push(v);
        }
    }
    Ok(out)
}

/// WindowAgg operator: buffers one partition at a time (input is sorted by the partition keys).
struct WindowAggOp {
    spec: WinSpec,
    input: Box<dyn RowSource>,
    /// First row of the next partition, read ahead, with its partition key values.
    pending: Option<(Row, Vec<Datum>)>,
    input_done: bool,
    done: bool,
    out: VecDeque<Row>,
}

impl WindowAggOp {
    /// Read the next partition from the input and compute its output rows into `out`.
    fn fill(&mut self, ex: &mut Executor) -> Result<()> {
        let mut rows: Vec<Row> = Vec::new();
        let mut key: Vec<Datum> = Vec::new();
        if let Some((row, k)) = self.pending.take() {
            rows.push(row);
            key = k;
        }
        while !self.input_done {
            let row = match self.input.next(ex)? {
                Some(r) => r,
                None => {
                    self.input_done = true;
                    break;
                }
            };
            let k = eval_exprs(ex, &self.spec.partition_by, &row)?;
            if rows.is_empty() {
                key = k;
                rows.push(row);
                continue;
            }
            let same = {
                let env: &dyn TypeEnv = &ex.se().env;
                same_keys(env, &self.spec.part_types, &key, &k)?
            };
            if same {
                rows.push(row);
            } else {
                self.pending = Some((row, k));
                break;
            }
        }
        if rows.is_empty() {
            self.done = true;
            return Ok(());
        }
        let produced = process_partition(ex, &self.spec, rows)?;
        self.out.extend(produced);
        Ok(())
    }
}

impl RowSource for WindowAggOp {
    fn next(&mut self, ex: &mut Executor) -> Result<Option<Row>> {
        loop {
            if let Some(row) = self.out.pop_front() {
                return Ok(Some(row));
            }
            if self.done {
                return Ok(None);
            }
            self.fill(ex)?;
        }
    }

    fn rescan(&mut self, ex: &mut Executor, params: &[Datum]) -> Result<()> {
        self.input.rescan(ex, params)?;
        self.pending = None;
        self.input_done = false;
        self.done = false;
        self.out.clear();
        Ok(())
    }
}

pub fn build_window_agg(node: &PlanNode, n: &WindowAggNode, b: &mut BuildCtx) -> Result<Box<dyn RowSource>> {
    let _ = node;
    let input = b.child(&n.input)?;
    let spec = WinSpec {
        partition_by: n.partition_by.clone(),
        part_types: n.partition_by.iter().map(|e| e.ty).collect(),
        order_keys: n.order_by.iter().map(SortKeyRt::from_spec).collect(),
        frame: n.frame.clone(),
        funcs: n.funcs.clone(),
    };
    Ok(Box::new(WindowAggOp { spec, input, pending: None, input_done: false, done: false, out: VecDeque::new() }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use pgarbor_types::context::{IoConfig, TestSession};
    use pgarbor_types::registry::builtin_env;

    fn peers_of(ints: &[i32]) -> Peers {
        let vals: Vec<Vec<Datum>> = ints.iter().map(|v| vec![Datum::Int4(*v)]).collect();
        compute_peers(builtin_env(), &[TypeId::INT4], &vals).expect("peer groups")
    }

    fn rows_frame(i: usize, len: usize, s: Bd, e: Bd) -> (usize, usize) {
        (rows_pos(&s, i, len, true).expect("start"), rows_pos(&e, i, len, false).expect("end"))
    }

    #[test]
    fn rows_sliding_one_preceding_one_following() {
        let pre = || Bd::Pre(Datum::Int8(1), TypeId::INT8);
        let fol = || Bd::Fol(Datum::Int8(1), TypeId::INT8);
        let expect = [(0, 2), (0, 3), (1, 4), (2, 5), (3, 5)];
        for (i, want) in expect.iter().enumerate() {
            assert_eq!(rows_frame(i, 5, pre(), fol()), *want, "row {i}");
        }
    }

    #[test]
    fn rows_unbounded_and_current_row() {
        assert_eq!(rows_frame(2, 5, Bd::UnPre, Bd::Cur), (0, 3));
        assert_eq!(rows_frame(2, 5, Bd::Cur, Bd::UnFol), (2, 5));
        assert_eq!(rows_frame(0, 0, Bd::UnPre, Bd::UnFol), (0, 0));
    }

    #[test]
    fn groups_offsets_move_by_peer_groups() {
        // Peer groups: [0,2) [2,3) [3,5).
        let peers = peers_of(&[1, 1, 2, 3, 3]);
        let expect = [(0, 2), (0, 2), (0, 3), (2, 5), (2, 5)];
        for (i, want) in expect.iter().enumerate() {
            let s = groups_pos(&Bd::Pre(Datum::Int8(1), TypeId::INT8), i, 5, &peers, true).expect("start");
            let e = groups_pos(&Bd::Cur, i, 5, &peers, false).expect("end");
            assert_eq!((s, e), *want, "row {i}");
        }
    }

    #[test]
    fn exclude_current_row_removes_only_self() {
        let peers = peers_of(&[1, 2, 3]);
        let f = frame_from_positions(2, 0, 3, FrameExclusion::CurrentRow, &peers);
        assert_eq!(f.excluded, vec![2]);
        assert_eq!(f.indexes().collect::<Vec<_>>(), vec![0, 1]);
    }

    #[test]
    fn exclude_ties_and_group() {
        let peers = peers_of(&[1, 1, 2]);
        let ties = frame_from_positions(0, 0, 3, FrameExclusion::Ties, &peers);
        assert_eq!(ties.indexes().collect::<Vec<_>>(), vec![0, 2]);
        let group = frame_from_positions(1, 0, 3, FrameExclusion::Group, &peers);
        assert_eq!(group.indexes().collect::<Vec<_>>(), vec![2]);
    }

    #[test]
    fn default_range_frame_includes_peers() {
        let peers = peers_of(&[1, 2, 2, 3]);
        let f = frame_from_positions(2, 0, peers.end(2), FrameExclusion::NoOthers, &peers);
        assert_eq!((f.start, f.end), (0, 3));
        assert!(f.excluded.is_empty());
    }

    #[test]
    fn empty_inverted_frame_is_empty() {
        let peers = peers_of(&[1, 2]);
        let f = frame_from_positions(0, 2, 1, FrameExclusion::NoOthers, &peers);
        assert!(f.is_empty());
        assert_eq!(f.start, f.end);
    }

    #[test]
    fn offsets_reject_null_and_negative() {
        let e = check_offset(&Datum::Null, false, true).expect_err("null");
        assert_eq!(e.sqlstate.code(), "22013");
        assert_eq!(e.message, "frame starting offset must not be null");
        let e = check_offset(&Datum::Int8(-1), false, false).expect_err("negative");
        assert_eq!(e.sqlstate.code(), "22013");
        assert_eq!(e.message, "frame ending offset must not be negative");
        assert!(check_offset(&Datum::Int8(0), false, true).is_ok());
        assert!(check_offset(&Datum::Int4(2), true, true).is_ok());
    }

    #[test]
    fn partition_point_on_prefix() {
        assert_eq!(partition_point(5, |j| Ok(j < 3)).expect("pp"), 3);
        assert_eq!(partition_point(0, |_| Ok(true)).expect("pp"), 0);
        assert_eq!(partition_point(4, |_| Ok(false)).expect("pp"), 0);
    }

    fn test_ctx<'a>(session: &'a TestSession, io: &'a IoConfig, arg_types: &'a [TypeId]) -> FuncCtx<'a> {
        FuncCtx { env: builtin_env(), io, session, arg_types, result_type: TypeId::INT8, collation: 0 }
    }

    fn sum_int4(vals: &[i32], frames: &[WinFrame]) -> Vec<Option<i64>> {
        let session = TestSession::default();
        let io = IoConfig::default();
        let arg_types = [TypeId::INT4];
        let ctx = test_ctx(&session, &io, &arg_types);
        let spec = AggSpec {
            agg_oid: 2108,
            arg_types: vec![TypeId::INT4],
            result_type: TypeId::INT8,
            direct_args: Vec::new(),
        };
        let v: Vec<Option<Vec<Datum>>> = vals.iter().map(|x| Some(vec![Datum::Int4(*x)])).collect();
        let out = agg_inner(&ctx, &spec, &v, frames).expect("sum over frames");
        out.iter().map(|d| d.as_i64()).collect()
    }

    #[test]
    fn running_sum_default_frame_with_peers() {
        // Values 1,2,2,3: RANGE UNBOUNDED PRECEDING .. CURRENT ROW gives equal sums for peers.
        let vals = [1, 2, 2, 3];
        let peers = peers_of(&vals);
        let frames: Vec<WinFrame> =
            (0..vals.len()).map(|i| frame_from_positions(i, 0, peers.end(i), FrameExclusion::NoOthers, &peers)).collect();
        assert_eq!(sum_int4(&vals, &frames), vec![Some(1), Some(5), Some(5), Some(8)]);
    }

    #[test]
    fn sliding_rows_sum() {
        let vals = [1, 2, 3, 4, 5];
        let frames: Vec<WinFrame> = (0..vals.len())
            .map(|i| {
                let (s, e) = rows_frame(
                    i,
                    vals.len(),
                    Bd::Pre(Datum::Int8(1), TypeId::INT8),
                    Bd::Fol(Datum::Int8(1), TypeId::INT8),
                );
                WinFrame { start: s, end: e, excluded: Vec::new() }
            })
            .collect();
        assert_eq!(sum_int4(&vals, &frames), vec![Some(3), Some(6), Some(9), Some(12), Some(9)]);
    }

    #[test]
    fn exclude_current_row_sum_uses_other_rows() {
        let vals = [1, 2, 3];
        let peers = peers_of(&vals);
        let frames: Vec<WinFrame> =
            (0..vals.len()).map(|i| frame_from_positions(i, 0, 3, FrameExclusion::CurrentRow, &peers)).collect();
        assert_eq!(sum_int4(&vals, &frames), vec![Some(5), Some(4), Some(3)]);
    }

    #[test]
    fn count_star_over_whole_partition() {
        let session = TestSession::default();
        let io = IoConfig::default();
        let arg_types: [TypeId; 0] = [];
        let ctx = test_ctx(&session, &io, &arg_types);
        let spec = AggSpec { agg_oid: 2803, arg_types: Vec::new(), result_type: TypeId::INT8, direct_args: Vec::new() };
        let vals: Vec<Option<Vec<Datum>>> = vec![Some(Vec::new()); 3];
        let frames: Vec<WinFrame> = (0..3).map(|_| WinFrame { start: 0, end: 3, excluded: Vec::new() }).collect();
        let out = agg_inner(&ctx, &spec, &vals, &frames).expect("count");
        assert_eq!(out.iter().map(|d| d.as_i64()).collect::<Vec<_>>(), vec![Some(3), Some(3), Some(3)]);
    }
}
