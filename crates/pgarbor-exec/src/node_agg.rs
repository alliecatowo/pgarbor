//! Aggregation (lease EXE-19, north-star). PG: `nodeAgg.c`, `parse_agg.c`. Layout: plan.rs
//! `AggregateNode` / `AggCall`. Aggregate state machines: `pgarbor_types::agg` (`make_aggregate`,
//! `AggState::{transition, finalize}`; its module docs define NULL handling: the executor does NOT
//! skip NULL arguments, the aggregate does).
//!
//! ## Output
//! One row per group: `group_keys` values (in `group_keys` order) ++ one finalized value per `aggs`
//! entry ++ (only if `grouping_sets` is `Some`) the grouping mask `Int8` (bit k set = group key k is
//! NOT part of the set that produced the row; keys not in the set output NULL). `having` (executable
//! over the OUTPUT row) filters groups after finalization.
//!
//! ## Strategies
//! * `Plain`: no keys; consumes all input; ALWAYS emits exactly one row (aggregates over an empty
//!   input finalize their empty state: `count` 0, `sum` NULL, ...), subject to `having`.
//! * `Hashed`: `HashMap` from the group key bytes (`keycodec::encode_key_vec` with `KeyColumn::asc(ty)`
//!   for every group key; NULLs form one group) to group state; output in first-appearance order
//!   (deterministic). Consume all input first, then emit lazily.
//! * `Sorted`: input is sorted by the keys; consecutive rows with `compare::not_distinct` keys form a
//!   group; emit a group as soon as the next key differs (streaming). Empty input -> no rows (unless
//!   keyless).
//! * Grouping sets (`grouping_sets: Some(sets)`, each set = indexes into `group_keys`): every input
//!   row is aggregated once per set into that set's own hash table; output sets in the listed order,
//!   each set's groups in first-appearance order. The EMPTY set `()` outputs one row even when the
//!   input is empty (all keys NULL, mask all ones); a non-empty set outputs nothing for empty input.
//!
//! ## Per-group state and per-row work
//! For every `AggCall` of a group keep an `AggInstance`:
//! * created when the group is created with `agg::make_aggregate(&AggSpec{agg_oid: call.agg,
//!   arg_types: call.arg_types, result_type: call.result_type, direct_args}, &FuncCtx)`, where
//!   `direct_args` are `call.direct_args` evaluated over the group's FIRST input row;
//! * per input row: skip if `call.filter` evaluates to anything but TRUE; evaluate `call.args` over the
//!   row; `star` aggregates (`count(*)`) pass no args;
//! * `distinct`: keep a set of `keycodec` bytes of the argument tuple per group (NULL equals NULL for
//!   this purpose) and skip repeats BEFORE feeding the state;
//! * `order_by` non-empty (ordered-set `WITHIN GROUP` or `agg(x ORDER BY ...)`): do not transition
//!   yet; buffer `(sort key values, arg values)` per group and at finalization sort the buffer with
//!   `sortutil::compare_keys` and feed the sorted rows to `transition`;
//! * otherwise `state.transition(&arg_values, &FuncCtx)` immediately.
//! * Finalize with `state.finalize(&FuncCtx)`.
//! The `FuncCtx` is built like the evaluator's (`eval_call::with_func_ctx`) with `arg_types =
//! call.arg_types`, `result_type = call.result_type`.
//! Memory: all groups are held in memory (no spilling in v1). Call `ex.check_interrupts()` at least
//! every 1024 input rows.
use std::cmp::Ordering;
use std::collections::{HashMap, HashSet, VecDeque};

use pgarbor_common::{PgError, Result, SqlState};
use pgarbor_plan::plan::{AggCall, AggStrategy, AggregateNode, PlanNode};
use pgarbor_sql::ir::BoundExpr;
use pgarbor_types::agg::{make_aggregate, AggSpec, AggState};
use pgarbor_types::compare;
use pgarbor_types::keycodec::{encode_key_vec, KeyColumn};
use pgarbor_types::{Datum, TypeId};

use crate::build::BuildCtx;
use crate::eval::{eval_exprs, eval_opt_qual, eval_qual};
use crate::eval_call::with_func_ctx;
use crate::runtime::{Executor, Row, RowSource};
use crate::sortutil::{compare_keys, SortKeyRt};

/// Grouping masks are `int8` bit sets: at most this many group keys can be told apart.
const MAX_MASK_KEYS: usize = 63;

/// One buffered input row of an ordered aggregate: `(sort key values, argument values)`.
type SortItem = (Vec<Datum>, Vec<Datum>);

/// Static, per-call information derived from the plan's `AggCall`.
struct AggCallRt {
    call: AggCall,
    /// Expressions of `call.order_by`, evaluated over each input row.
    order_exprs: Vec<BoundExpr>,
    /// Comparison keys of `call.order_by`, used to sort the buffer at finalization.
    sort_keys: Vec<SortKeyRt>,
    /// Encoding of the aggregated argument tuple used by DISTINCT.
    distinct_cols: Vec<KeyColumn>,
}

/// One grouping set: the group-key positions it groups by, their encodings and its mask.
struct SetDef {
    members: Vec<usize>,
    key_cols: Vec<KeyColumn>,
    mask: i64,
}

/// State of one aggregate call within one group.
struct AggInstance {
    state: Box<dyn AggState>,
    /// DISTINCT: encoded argument tuples already fed to `state`.
    seen: HashSet<Vec<u8>>,
    /// ORDER BY inside the aggregate: rows waiting to be sorted and fed at finalization.
    buf: Vec<SortItem>,
}

/// One group: its key values and the state of every aggregate call.
struct Group {
    /// One value per `group_keys` entry; NULL for keys outside the grouping set.
    keys: Vec<Datum>,
    insts: Vec<AggInstance>,
}

/// The groups of one grouping set (hash table keyed by the encoded member keys).
#[derive(Default)]
struct SetTable {
    index: HashMap<Vec<u8>, usize>,
    groups: Vec<Group>,
}

/// The aggregation operator (module docs).
struct AggregateOp {
    input: Box<dyn RowSource>,
    /// Streaming mode: `Sorted` strategy with keys and no grouping sets.
    sorted_mode: bool,
    keys: Vec<BoundExpr>,
    key_types: Vec<TypeId>,
    calls: Vec<AggCallRt>,
    sets: Vec<SetDef>,
    /// Output the grouping mask column (`grouping_sets` was given).
    grouping: bool,
    having: Option<BoundExpr>,
    ticks: u32,
    // Hashed mode.
    consumed: bool,
    tables: Vec<SetTable>,
    emit_set: usize,
    emit_idx: usize,
    // Sorted (streaming) mode.
    input_done: bool,
    cur: Option<Group>,
}

pub fn build_aggregate(node: &PlanNode, n: &AggregateNode, b: &mut BuildCtx) -> Result<Box<dyn RowSource>> {
    let _ = node;
    let nkeys = n.group_keys.len();
    let grouping = n.grouping_sets.is_some();
    if grouping && nkeys > MAX_MASK_KEYS {
        return Err(PgError::new(
            SqlState::FEATURE_NOT_SUPPORTED,
            "grouping sets over more than 63 expressions are not supported",
        ));
    }
    let set_lists: Vec<Vec<usize>> = match &n.grouping_sets {
        Some(lists) => lists.clone(),
        None => vec![(0..nkeys).collect()],
    };
    let mut sets = Vec::with_capacity(set_lists.len());
    for members in &set_lists {
        sets.push(make_set(members, &n.group_keys)?);
    }
    let sorted_mode = !grouping && matches!(n.strategy, AggStrategy::Sorted) && nkeys > 0;
    let calls: Vec<AggCallRt> = n.aggs.iter().map(build_call).collect();
    let input = b.child(&n.input)?;
    let tables: Vec<SetTable> = (0..sets.len()).map(|_| SetTable::default()).collect();
    Ok(Box::new(AggregateOp {
        input,
        sorted_mode,
        keys: n.group_keys.clone(),
        key_types: n.group_keys.iter().map(|k| k.ty).collect(),
        calls,
        sets,
        grouping,
        having: n.having.clone(),
        ticks: 0,
        consumed: false,
        tables,
        emit_set: 0,
        emit_idx: 0,
        input_done: false,
        cur: None,
    }))
}

/// Static information of one aggregate call.
fn build_call(c: &AggCall) -> AggCallRt {
    AggCallRt {
        call: c.clone(),
        order_exprs: c.order_by.iter().map(|k| k.expr.clone()).collect(),
        sort_keys: c.order_by.iter().map(SortKeyRt::from_spec).collect(),
        distinct_cols: c.args.iter().map(|a| KeyColumn::asc(a.ty)).collect(),
    }
}

/// A grouping set over `members` (indexes into `keys`). Mask bit k is set when key k is NOT a member.
fn make_set(members: &[usize], keys: &[BoundExpr]) -> Result<SetDef> {
    let mut key_cols = Vec::with_capacity(members.len());
    for &k in members {
        let key = keys.get(k).ok_or_else(|| {
            PgError::internal(format!("grouping set refers to group key {k} of {}", keys.len()))
        })?;
        key_cols.push(KeyColumn::asc(key.ty));
    }
    let mut mask: u64 = 0;
    for k in 0..keys.len().min(MAX_MASK_KEYS) {
        if !members.contains(&k) {
            mask |= 1u64 << k;
        }
    }
    Ok(SetDef { members: members.to_vec(), key_cols, mask: mask as i64 })
}

/// Fresh states for every aggregate call of a new group. Direct arguments are evaluated over `row`,
/// the group's first input row (`&[]` for the empty set without input).
fn new_instances(ex: &mut Executor, calls: &[AggCallRt], row: &[Datum]) -> Result<Vec<AggInstance>> {
    let mut out = Vec::with_capacity(calls.len());
    for c in calls {
        let direct_args = eval_exprs(ex, &c.call.direct_args, row)?;
        let spec = AggSpec {
            agg_oid: c.call.agg,
            arg_types: c.call.arg_types.clone(),
            result_type: c.call.result_type,
            direct_args,
        };
        let state: Box<dyn AggState> = with_func_ctx(&*ex.cx, &c.call.arg_types, c.call.result_type, |fc| {
            make_aggregate(&spec, fc)
        })?;
        out.push(AggInstance { state, seen: HashSet::new(), buf: Vec::new() });
    }
    Ok(out)
}

/// Feed one input row to every aggregate call of a group (FILTER, DISTINCT and ORDER BY handling).
fn feed_row(ex: &mut Executor, calls: &[AggCallRt], insts: &mut [AggInstance], row: &[Datum]) -> Result<()> {
    for (c, inst) in calls.iter().zip(insts.iter_mut()) {
        if let Some(filter) = &c.call.filter {
            if !eval_qual(ex, filter, row)? {
                continue;
            }
        }
        let args: Vec<Datum> = if c.call.star { Vec::new() } else { eval_exprs(ex, &c.call.args, row)? };
        if c.call.distinct {
            let key = encode_key_vec(&ex.se().env, &c.distinct_cols, &args)?;
            if !inst.seen.insert(key) {
                continue;
            }
        }
        if c.call.order_by.is_empty() {
            with_func_ctx(&*ex.cx, &c.call.arg_types, c.call.result_type, |fc| inst.state.transition(&args, fc))?;
        } else {
            let keyvals = eval_exprs(ex, &c.order_exprs, row)?;
            inst.buf.push((keyvals, args));
        }
    }
    Ok(())
}

/// Stable merge sort of buffered ordered-aggregate rows (fallible comparison, no panics).
fn sort_items(ex: &mut Executor, keys: &[SortKeyRt], mut items: Vec<SortItem>) -> Result<Vec<SortItem>> {
    if items.len() <= 1 {
        return Ok(items);
    }
    let mid = items.len() / 2;
    let right_part = items.split_off(mid);
    let left = sort_items(ex, keys, items)?;
    let right = sort_items(ex, keys, right_part)?;
    let mut l: VecDeque<SortItem> = left.into();
    let mut r: VecDeque<SortItem> = right.into();
    let mut out = Vec::with_capacity(l.len() + r.len());
    loop {
        let take_right = match (l.front(), r.front()) {
            (Some(a), Some(b)) => compare_keys(&mut *ex, keys, &a.0, &b.0)? == Ordering::Greater,
            (Some(_), None) => false,
            (None, Some(_)) => true,
            (None, None) => break,
        };
        let next = if take_right { r.pop_front() } else { l.pop_front() };
        match next {
            Some(item) => out.push(item),
            None => break,
        }
    }
    Ok(out)
}

/// Finalize one aggregate call of a group: sort and feed the buffered rows when it has ORDER BY,
/// then produce the result.
fn finish_instance(ex: &mut Executor, c: &AggCallRt, inst: &mut AggInstance) -> Result<Datum> {
    if !c.call.order_by.is_empty() {
        let items = std::mem::take(&mut inst.buf);
        let sorted = sort_items(ex, &c.sort_keys, items)?;
        for (_, args) in sorted.iter() {
            with_func_ctx(&*ex.cx, &c.call.arg_types, c.call.result_type, |fc| inst.state.transition(args, fc))?;
        }
    }
    with_func_ctx(&*ex.cx, &c.call.arg_types, c.call.result_type, |fc| inst.state.finalize(fc))
}

/// Output row of a finished group (module docs) when it passes HAVING.
fn emit_group(
    ex: &mut Executor,
    calls: &[AggCallRt],
    set: &SetDef,
    grouping: bool,
    group: &mut Group,
    having: &Option<BoundExpr>,
) -> Result<Option<Row>> {
    let mut out: Row = Vec::with_capacity(group.keys.len() + calls.len() + 1);
    out.extend(group.keys.iter().cloned());
    for (c, inst) in calls.iter().zip(group.insts.iter_mut()) {
        let value = finish_instance(ex, c, inst)?;
        out.push(value);
    }
    if grouping {
        out.push(Datum::Int8(set.mask));
    }
    if eval_opt_qual(ex, having, &out)? {
        Ok(Some(out))
    } else {
        Ok(None)
    }
}

/// Find the group of `kv`'s member keys in `table`, creating it (from `row`) when absent.
fn find_or_create(
    ex: &mut Executor,
    calls: &[AggCallRt],
    set: &SetDef,
    table: &mut SetTable,
    kv: &[Datum],
    row: &[Datum],
) -> Result<usize> {
    let member_vals: Vec<Datum> = set.members.iter().map(|&k| kv[k].clone()).collect();
    let bytes = encode_key_vec(&ex.se().env, &set.key_cols, &member_vals)?;
    if let Some(&gi) = table.index.get(&bytes) {
        return Ok(gi);
    }
    let mut keys = vec![Datum::Null; kv.len()];
    for &k in &set.members {
        keys[k] = kv[k].clone();
    }
    let insts = new_instances(ex, calls, row)?;
    table.groups.push(Group { keys, insts });
    let gi = table.groups.len() - 1;
    table.index.insert(bytes, gi);
    Ok(gi)
}

/// `IS NOT DISTINCT FROM` over all group keys (streaming mode group boundaries).
fn keys_match(ex: &Executor, types: &[TypeId], a: &[Datum], b: &[Datum]) -> Result<bool> {
    for ((ty, x), y) in types.iter().zip(a.iter()).zip(b.iter()) {
        if !compare::not_distinct(&ex.se().env, *ty, x, y)? {
            return Ok(false);
        }
    }
    Ok(true)
}

impl AggregateOp {
    /// Hashed mode: consume the whole input into one hash table per grouping set.
    fn consume(&mut self, ex: &mut Executor) -> Result<()> {
        while let Some(row) = self.input.next(ex)? {
            ex.tick(&mut self.ticks)?;
            let kv = eval_exprs(ex, &self.keys, &row)?;
            for si in 0..self.sets.len() {
                let gi = find_or_create(ex, &self.calls, &self.sets[si], &mut self.tables[si], &kv, &row)?;
                let group = &mut self.tables[si].groups[gi];
                feed_row(ex, &self.calls, &mut group.insts, &row)?;
            }
        }
        // An empty grouping set always yields its one group, even for empty input.
        let nulls = vec![Datum::Null; self.keys.len()];
        let empty: Row = Vec::new();
        for si in 0..self.sets.len() {
            if self.sets[si].members.is_empty() && self.tables[si].groups.is_empty() {
                find_or_create(ex, &self.calls, &self.sets[si], &mut self.tables[si], &nulls, &empty)?;
            }
        }
        Ok(())
    }

    /// Hashed mode: emit the groups set by set, each set in first-appearance order.
    fn next_hashed(&mut self, ex: &mut Executor) -> Result<Option<Row>> {
        if !self.consumed {
            self.consume(ex)?;
            self.consumed = true;
        }
        while self.emit_set < self.sets.len() {
            let si = self.emit_set;
            let gi = self.emit_idx;
            if gi >= self.tables[si].groups.len() {
                self.emit_set += 1;
                self.emit_idx = 0;
                continue;
            }
            self.emit_idx += 1;
            let group = &mut self.tables[si].groups[gi];
            if let Some(out) = emit_group(ex, &self.calls, &self.sets[si], self.grouping, group, &self.having)? {
                return Ok(Some(out));
            }
        }
        Ok(None)
    }

    /// Streaming mode (`Sorted` input): a group ends when the next row's keys differ.
    fn next_sorted(&mut self, ex: &mut Executor) -> Result<Option<Row>> {
        loop {
            if self.input_done {
                // The last group is the only one still open: its result (or the end) is the answer.
                let Some(mut group) = self.cur.take() else {
                    return Ok(None);
                };
                return emit_group(ex, &self.calls, &self.sets[0], false, &mut group, &self.having);
            }
            let row = match self.input.next(ex)? {
                Some(r) => r,
                None => {
                    self.input_done = true;
                    continue;
                }
            };
            ex.tick(&mut self.ticks)?;
            let kv = eval_exprs(ex, &self.keys, &row)?;
            let same = match &self.cur {
                Some(g) => keys_match(ex, &self.key_types, &g.keys, &kv)?,
                None => false,
            };
            if same {
                if let Some(g) = self.cur.as_mut() {
                    feed_row(ex, &self.calls, &mut g.insts, &row)?;
                }
                continue;
            }
            let mut fresh = Group { keys: kv, insts: new_instances(ex, &self.calls, &row)? };
            feed_row(ex, &self.calls, &mut fresh.insts, &row)?;
            if let Some(mut finished) = self.cur.replace(fresh) {
                if let Some(out) = emit_group(ex, &self.calls, &self.sets[0], false, &mut finished, &self.having)? {
                    return Ok(Some(out));
                }
            }
        }
    }
}

impl RowSource for AggregateOp {
    fn next(&mut self, ex: &mut Executor) -> Result<Option<Row>> {
        if self.sorted_mode {
            self.next_sorted(ex)
        } else {
            self.next_hashed(ex)
        }
    }

    fn rescan(&mut self, ex: &mut Executor, params: &[Datum]) -> Result<()> {
        self.input.rescan(ex, params)?;
        self.consumed = false;
        for table in self.tables.iter_mut() {
            table.index.clear();
            table.groups.clear();
        }
        self.emit_set = 0;
        self.emit_idx = 0;
        self.input_done = false;
        self.cur = None;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::Cell;

    use pgarbor_common::Oid;
    use pgarbor_plan::plan::SortKeySpec;
    use pgarbor_types::oids;

    use crate::context::{ExecContext, NoGuc, NoServices, SessionEnv};
    use crate::memtxn::MemTxn;
    use crate::runtime::Runtime;

    /// Feeds fixed rows; `rescan` restarts them.
    struct VecSource {
        original: Vec<Row>,
        rows: VecDeque<Row>,
    }

    impl RowSource for VecSource {
        fn next(&mut self, _ex: &mut Executor) -> Result<Option<Row>> {
            Ok(self.rows.pop_front())
        }
        fn rescan(&mut self, _ex: &mut Executor, _params: &[Datum]) -> Result<()> {
            self.rows = self.original.iter().cloned().collect();
            Ok(())
        }
    }

    fn int4() -> TypeId {
        TypeId { oid: oids::INT4, typmod: -1 }
    }

    fn int8() -> TypeId {
        TypeId { oid: oids::INT8, typmod: -1 }
    }

    fn text() -> TypeId {
        TypeId { oid: oids::TEXT, typmod: -1 }
    }

    fn bool_ty() -> TypeId {
        TypeId { oid: oids::BOOL, typmod: -1 }
    }

    fn call(agg: Oid, args: Vec<BoundExpr>, arg_types: Vec<TypeId>, result: TypeId) -> AggCall {
        AggCall {
            agg,
            name: "agg".to_string(),
            args,
            direct_args: Vec::new(),
            distinct: false,
            filter: None,
            order_by: Vec::new(),
            star: false,
            result_type: result,
            arg_types,
        }
    }

    fn count_star() -> AggCall {
        AggCall { star: true, ..call(2803, Vec::new(), Vec::new(), int8()) }
    }

    fn sum_int4(col: u32) -> AggCall {
        call(2108, vec![BoundExpr::input_ref(col, int4())], vec![int4()], int8())
    }

    fn operator(
        input: Vec<Row>,
        keys: Vec<BoundExpr>,
        aggs: Vec<AggCall>,
        grouping: Option<Vec<Vec<usize>>>,
        sorted: bool,
    ) -> Result<AggregateOp> {
        let nkeys = keys.len();
        let grouping_flag = grouping.is_some();
        let set_lists: Vec<Vec<usize>> = match &grouping {
            Some(lists) => lists.clone(),
            None => vec![(0..nkeys).collect()],
        };
        let mut sets = Vec::new();
        for members in &set_lists {
            sets.push(make_set(members, &keys)?);
        }
        let tables: Vec<SetTable> = (0..sets.len()).map(|_| SetTable::default()).collect();
        let key_types: Vec<TypeId> = keys.iter().map(|k| k.ty).collect();
        let calls: Vec<AggCallRt> = aggs.iter().map(build_call).collect();
        let source = VecSource { original: input.clone(), rows: input.into() };
        Ok(AggregateOp {
            input: Box::new(source),
            sorted_mode: sorted && !grouping_flag && nkeys > 0,
            keys,
            key_types,
            calls,
            sets,
            grouping: grouping_flag,
            having: None,
            ticks: 0,
            consumed: false,
            tables,
            emit_set: 0,
            emit_idx: 0,
            input_done: false,
            cur: None,
        })
    }

    fn drain(op: &mut AggregateOp, ex: &mut Executor) -> Result<Vec<Row>> {
        let mut out = Vec::new();
        while let Some(row) = op.next(ex)? {
            out.push(row);
        }
        Ok(out)
    }

    /// Run `f` over an executor backed by the test catalog and an in-memory transaction.
    fn with_executor<T>(f: impl FnOnce(&mut Executor) -> Result<T>) -> Result<T> {
        let (_kv, env) = pgarbor_catalog::testing::bootstrapped()?;
        let guc = NoGuc;
        let services = NoServices;
        let rng = Cell::new(12345u64);
        let se = SessionEnv::new(env, pgarbor_types::IoConfig::default(), &guc, &services, &rng, 0, 0);
        let mut txn = MemTxn::new();
        let mut cx = ExecContext::new(&mut txn, se);
        let mut rt = Runtime::default();
        let mut ex = Executor::new(&mut rt, &mut cx);
        f(&mut ex)
    }

    #[test]
    fn grouping_set_masks_for_two_keys() {
        // GROUPING SETS ((a), (b), ()) over two keys: masks 2, 1 and 3.
        let keys = vec![BoundExpr::input_ref(0, int4()), BoundExpr::input_ref(1, int4())];
        let a = make_set(&[0], &keys).expect("set a");
        let b = make_set(&[1], &keys).expect("set b");
        let empty = make_set(&[], &keys).expect("empty set");
        assert_eq!((a.mask, b.mask, empty.mask), (2, 1, 3));
    }

    #[test]
    fn grouping_set_rejects_unknown_key() {
        let keys = vec![BoundExpr::input_ref(0, int4())];
        assert!(make_set(&[1], &keys).is_err());
    }

    #[test]
    fn build_call_records_argument_key_columns() {
        let c = build_call(&sum_int4(0));
        assert_eq!(c.distinct_cols.len(), 1);
        assert!(c.order_exprs.is_empty());
        assert!(c.sort_keys.is_empty());
    }

    #[test]
    #[ignore = "needs EXE-05, EXE-06"]
    fn plain_count_star_over_empty_input_yields_one_row() {
        let mut op = operator(Vec::new(), Vec::new(), vec![count_star()], None, false).expect("op");
        let rows = with_executor(|ex| drain(&mut op, ex)).expect("run");
        assert_eq!(rows, vec![vec![Datum::Int8(0)]]);
    }

    #[test]
    #[ignore = "needs EXE-05, EXE-06"]
    fn plain_sum_over_empty_input_is_null() {
        let mut op = operator(Vec::new(), Vec::new(), vec![sum_int4(0)], None, false).expect("op");
        let rows = with_executor(|ex| drain(&mut op, ex)).expect("run");
        assert_eq!(rows, vec![vec![Datum::Null]]);
    }

    #[test]
    #[ignore = "needs EXE-05, EXE-06"]
    fn hashed_sum_groups_in_first_appearance_order_with_null_key() {
        let input = vec![
            vec![Datum::Int4(1), Datum::Int4(10)],
            vec![Datum::Null, Datum::Int4(5)],
            vec![Datum::Int4(1), Datum::Int4(20)],
            vec![Datum::Null, Datum::Int4(7)],
        ];
        let mut op = operator(input, vec![BoundExpr::input_ref(0, int4())], vec![sum_int4(1)], None, false)
            .expect("op");
        let rows = with_executor(|ex| drain(&mut op, ex)).expect("run");
        assert_eq!(
            rows,
            vec![
                vec![Datum::Int4(1), Datum::Int8(30)],
                vec![Datum::Null, Datum::Int8(12)],
            ]
        );
    }

    #[test]
    #[ignore = "needs EXE-05, EXE-06"]
    fn grouping_sets_emit_masks_and_empty_set_row() {
        let keys = vec![BoundExpr::input_ref(0, int4()), BoundExpr::input_ref(1, int4())];
        let sets = Some(vec![vec![0], vec![1], vec![]]);
        let input = vec![vec![Datum::Int4(1), Datum::Int4(2)]];
        let mut op = operator(input, keys.clone(), vec![count_star()], sets.clone(), false).expect("op");
        let rows = with_executor(|ex| drain(&mut op, ex)).expect("run");
        assert_eq!(
            rows,
            vec![
                vec![Datum::Int4(1), Datum::Null, Datum::Int8(1), Datum::Int8(2)],
                vec![Datum::Null, Datum::Int4(2), Datum::Int8(1), Datum::Int8(1)],
                vec![Datum::Null, Datum::Null, Datum::Int8(1), Datum::Int8(3)],
            ]
        );
        let mut empty_op = operator(Vec::new(), keys, vec![count_star()], sets, false).expect("op");
        let empty_rows = with_executor(|ex| drain(&mut empty_op, ex)).expect("run");
        assert_eq!(empty_rows, vec![vec![Datum::Null, Datum::Null, Datum::Int8(0), Datum::Int8(3)]]);
    }

    #[test]
    #[ignore = "needs EXE-05, EXE-06, EXE-22"]
    fn string_agg_respects_order_by() {
        let mut agg = call(
            3538,
            vec![BoundExpr::input_ref(0, text()), BoundExpr::constant(Datum::Text(",".to_string()), text())],
            vec![text(), text()],
            text(),
        );
        agg.order_by = vec![SortKeySpec {
            expr: BoundExpr::input_ref(1, int4()),
            descending: true,
            nulls_first: false,
            collation: 0,
            using_op: None,
        }];
        let input = vec![
            vec![Datum::Text("a".to_string()), Datum::Int4(1)],
            vec![Datum::Text("c".to_string()), Datum::Int4(3)],
            vec![Datum::Text("b".to_string()), Datum::Int4(2)],
        ];
        let mut op = operator(input, Vec::new(), vec![agg], None, false).expect("op");
        let rows = with_executor(|ex| drain(&mut op, ex)).expect("run");
        assert_eq!(rows, vec![vec![Datum::Text("c,b,a".to_string())]]);
    }

    #[test]
    #[ignore = "needs EXE-05, EXE-06"]
    fn count_distinct_skips_duplicates_and_nulls() {
        let mut agg = call(2147, vec![BoundExpr::input_ref(0, int4())], vec![int4()], int8());
        agg.distinct = true;
        let input = vec![
            vec![Datum::Int4(1)],
            vec![Datum::Int4(1)],
            vec![Datum::Null],
            vec![Datum::Int4(2)],
        ];
        let mut op = operator(input, Vec::new(), vec![agg], None, false).expect("op");
        let rows = with_executor(|ex| drain(&mut op, ex)).expect("run");
        assert_eq!(rows, vec![vec![Datum::Int8(2)]]);
    }

    #[test]
    #[ignore = "needs EXE-05, EXE-06"]
    fn filter_clause_restricts_input_rows() {
        let mut agg = sum_int4(1);
        agg.filter = Some(BoundExpr::input_ref(0, bool_ty()));
        let input = vec![
            vec![Datum::Bool(true), Datum::Int4(4)],
            vec![Datum::Bool(false), Datum::Int4(100)],
            vec![Datum::Null, Datum::Int4(1000)],
            vec![Datum::Bool(true), Datum::Int4(6)],
        ];
        let mut op = operator(input, Vec::new(), vec![agg], None, false).expect("op");
        let rows = with_executor(|ex| drain(&mut op, ex)).expect("run");
        assert_eq!(rows, vec![vec![Datum::Int8(10)]]);
    }

    #[test]
    #[ignore = "needs EXE-05, EXE-06, EXE-22"]
    fn sorted_strategy_streams_consecutive_groups() {
        let input = vec![
            vec![Datum::Int4(1), Datum::Int4(1)],
            vec![Datum::Int4(1), Datum::Int4(2)],
            vec![Datum::Int4(2), Datum::Int4(3)],
        ];
        let mut op = operator(input, vec![BoundExpr::input_ref(0, int4())], vec![sum_int4(1)], None, true)
            .expect("op");
        let rows = with_executor(|ex| drain(&mut op, ex)).expect("run");
        assert_eq!(
            rows,
            vec![
                vec![Datum::Int4(1), Datum::Int8(3)],
                vec![Datum::Int4(2), Datum::Int8(3)],
            ]
        );
    }

    #[test]
    #[ignore = "needs EXE-05, EXE-06"]
    fn having_filters_finalized_groups() {
        let input = vec![vec![Datum::Int4(1)], vec![Datum::Int4(2)]];
        let mut op = operator(input, Vec::new(), vec![count_star()], None, false).expect("op");
        op.having = Some(BoundExpr::bool_const(false));
        let rows = with_executor(|ex| drain(&mut op, ex)).expect("run");
        assert!(rows.is_empty());
    }
}
