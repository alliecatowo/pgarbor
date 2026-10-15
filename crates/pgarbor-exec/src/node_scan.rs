//! Leaf operators (lease EXE-12, north-star): `Result`, `SeqScan`, `IndexScan`, `VirtualScan`,
//! `ValuesScan`. Row layouts are those of plan.rs (module docs and each node struct's docs).
//!
//! ## Result
//! Evaluates `exprs` once over the EMPTY row (`&[]`; `Param`/`OuterRef` allowed) and yields that single
//! row; if `one_time_filter` is `Some` and does not evaluate to TRUE yields no row. `rescan` makes
//! it yield again (re-evaluating, parameters may differ).
//!
//! ## SeqScan
//! * Open: `perm::require_relation_privilege(&b.cx.se, oid, acl_mode::SELECT, <needed attnums>)` (also for
//!   IndexScan), then `b.table_desc(table.oid)`. On the first `next` of each scan take `lock_relation(table.oid,
//!   LockMode::AccessShare, WaitPolicy::Block)` (conflicts only with DDL's AccessExclusive).
//! * Rows come from a `rowstore::HeapCursor` (batched; module docs there) decoded with the
//!   `needed` mask (`false` entries stay `Null`). Output row = table row (`ncols` slots; dropped
//!   columns `Null`) and, if `with_rowid`, one trailing `Datum::Int8(rowid as i64)` (bit-for-bit
//!   u64). `filter` is evaluated over the produced row (`eval_opt_qual`); non-TRUE rows are skipped.
//! * `ex.check_interrupts()` at least once per 1024 rows examined.
//!
//! ## IndexScan (btree; entry layout in `index_key`)
//! * At (re)start evaluate the bounds over the empty row (parameters allowed): each
//!   `IndexBounds::eq[i]`: `KeyEq::Value(e)` -> one value (NULL => the scan is empty); `KeyEq::AnyOf(e)`
//!   -> the array's non-NULL elements, sorted with `compare::cmp_datums` on the key type and
//!   de-duplicated, each visited in turn (cartesian product with the other eq columns, in key
//!   order; reverse order for `Backward`); `lower`/`upper` values likewise (NULL bound => empty).
//!   Values are cast to the index key column type if their static type differs (assignment cast).
//! * For each value combination compute the byte interval with `index_key::interval_for_bounds`
//!   (key columns from `IndexRef::keys`: `KeyColumn{ty, desc: descending, nulls_first}`), scan it in
//!   batches (re-seek from `Excluded(last_key)`; no iterator is kept between calls) and for every
//!   entry fetch the heap row by the rowid in the entry VALUE (`rowid_of_entry_value`); a missing
//!   heap row (stale entry) is skipped.
//! * `Backward`: ascending keys cannot be walked in reverse through `KvRead`; collect the keys of one
//!   interval (entries only, not rows) into a `Vec` and iterate it from the end. Interval order is
//!   reversed too.
//! * If `recheck_index_cond` evaluate every expression of `index_cond` over the produced row (all
//!   must be TRUE), then `filter`. Output row as SeqScan (`with_rowid` appends the rowid).
//! * Expression-index keys are not usable here in v1 (`IndexKeyRef::column == None` => XX000).
//!
//! ## VirtualScan (pg_catalog / information_schema)
//! On the first `next` evaluate `eq_filters` values and call `pgarbor_catalog::vrel::scan(
//! &ScanContext{env: &se.env, session: &ExecSessionInfo::new(&cx.se, &*cx.txn), io_config: &se.io},
//! table.oid, &ScanRequest{eq: [(attnum - 1) as usize, value]})`; buffer the returned rows and apply
//! `filter` (always) while yielding. Relations without a producer fail inside `vrel::scan` with 0A000.
//!
//! ## ValuesScan
//! For each row of `rows` evaluate the cell expressions over the empty row and apply `filter` over
//! the produced row. Cells with sub-plans/volatile functions are evaluated per row, lazily.
use std::cmp::Ordering;
use std::collections::VecDeque;
use std::ops::Bound;
use std::rc::Rc;

use pgarbor_catalog::model::common::acl_mode;
use pgarbor_catalog::vrel::{scan as vrel_scan, ScanContext, ScanRequest};
use pgarbor_common::{KvRead, Oid, PgError, Result};
use pgarbor_core::locks::LockMode;
use pgarbor_core::WaitPolicy;
use pgarbor_plan::plan::{IndexScan, KeyEq, PlanNode, RangeEnd, ResultNode, ScanDirection, SeqScan, ValuesScan, VirtualScan};
use pgarbor_sql::ir::BoundExpr;
use pgarbor_types::cast::{cast_datum, CastContext};
use pgarbor_types::compare::cmp_datums;
use pgarbor_types::context::{FuncCtx, TypeEnv};
use pgarbor_types::keycodec::KeyColumn;
use pgarbor_types::{Datum, TypeId};

use crate::build::BuildCtx;
use crate::error;
use crate::eval::{eval, eval_exprs, eval_opt_qual, eval_qual};
use crate::exec_session::{ExecSessionContext, ExecSessionInfo};
use crate::index_key::{interval_for_bounds, rowid_of_entry_value, KeyInterval};
use crate::perm;
use crate::rowstore::{self, HeapCursor, SCAN_BATCH};
use crate::runtime::{Executor, Row, RowSource};
use crate::tabledesc::TableDesc;
use crate::txn::kv_scan_vec;

/// One stored index entry or heap entry as read from the KV layer: `(key, value)`.
type Entry = (Vec<u8>, Vec<u8>);

/// Attribute numbers (1-based) of the slots flagged in `needed` (`needed[i]` = attnum `i + 1`).
fn needed_attnums(needed: &[bool]) -> Vec<i16> {
    let mut out = Vec::new();
    for (i, flag) in needed.iter().enumerate() {
        if *flag {
            if let Ok(attnum) = i16::try_from(i + 1) {
                out.push(attnum);
            }
        }
    }
    out
}

/// The `needed` mask as a decode mask: `None` when every slot is needed (decode everything).
fn needed_mask(needed: &[bool]) -> Option<Vec<bool>> {
    if needed.iter().all(|f| *f) {
        None
    } else {
        Some(needed.to_vec())
    }
}

/// Take the scan lock on the table (module docs): `AccessShare`, waiting for DDL only.
fn lock_scan_relation(ex: &mut Executor, relid: Oid, table_name: &str) -> Result<()> {
    if ex.txn().lock_relation(relid, LockMode::AccessShare, WaitPolicy::Block)? {
        Ok(())
    } else {
        Err(error::relation_lock_not_available(table_name))
    }
}

/// Assignment cast of `v` from `from` to `to` (no-op when the types are equal).
fn assign_cast(ex: &Executor, v: Datum, from: TypeId, to: TypeId) -> Result<Datum> {
    if from == to {
        return Ok(v);
    }
    let session = ExecSessionContext::new(&ex.cx.se, &*ex.cx.txn);
    let fctx = FuncCtx {
        env: &ex.cx.se.env,
        io: &ex.cx.se.io,
        session: &session,
        arg_types: &[],
        result_type: to,
        collation: 0,
    };
    cast_datum(v, from, to, CastContext::Assignment, &fctx)
}

/// Sort `vals` by the key type, drop duplicates and, for a DESC key column, reverse so that the
/// result is in index (byte) order. Values must be non-NULL.
fn sort_dedup(env: &dyn TypeEnv, col: &KeyColumn, mut vals: Vec<Datum>) -> Result<Vec<Datum>> {
    let mut err: Option<PgError> = None;
    vals.sort_by(|a, b| match cmp_datums(env, col.ty, a, b) {
        Ok(ord) => ord,
        Err(e) => {
            if err.is_none() {
                err = Some(e);
            }
            Ordering::Equal
        }
    });
    if let Some(e) = err {
        return Err(e);
    }
    let mut out: Vec<Datum> = Vec::with_capacity(vals.len());
    for v in vals {
        let duplicate = match out.last() {
            Some(last) => cmp_datums(env, col.ty, last, &v)? == Ordering::Equal,
            None => false,
        };
        if !duplicate {
            out.push(v);
        }
    }
    if col.desc {
        out.reverse();
    }
    Ok(out)
}

/// Cartesian product of the per-column value lists, lexicographic in column order (the first
/// column varies slowest). An empty list yields no combination.
fn cartesian(lists: Vec<Vec<Datum>>) -> Vec<Vec<Datum>> {
    let mut acc: Vec<Vec<Datum>> = vec![Vec::new()];
    for list in lists {
        let mut next: Vec<Vec<Datum>> = Vec::with_capacity(acc.len() * list.len());
        for prefix in &acc {
            for v in &list {
                let mut combo = prefix.clone();
                combo.push(v.clone());
                next.push(combo);
            }
        }
        acc = next;
    }
    acc
}

/// Values of one equality column in index order. `Ok(None)` = no row can match (a NULL value).
fn eq_column_values(ex: &mut Executor, eq: &KeyEq, col: &KeyColumn) -> Result<Option<Vec<Datum>>> {
    match eq {
        KeyEq::Value(e) => {
            let v = eval(ex, e, &[])?;
            if matches!(v, Datum::Null) {
                return Ok(None);
            }
            let v = assign_cast(ex, v, e.ty, col.ty)?;
            Ok(Some(vec![v]))
        }
        KeyEq::AnyOf(e) => {
            let v = eval(ex, e, &[])?;
            let arr = match v {
                Datum::Null => return Ok(None),
                Datum::Array(a) => *a,
                other => {
                    return Err(PgError::internal(format!(
                        "index scan = ANY argument is not an array: {other:?}"
                    )));
                }
            };
            let elem_ty = TypeId::of(arr.elem);
            let mut vals: Vec<Datum> = Vec::with_capacity(arr.items.len());
            for item in arr.items {
                if matches!(item, Datum::Null) {
                    continue;
                }
                vals.push(assign_cast(ex, item, elem_ty, col.ty)?);
            }
            Ok(Some(sort_dedup(&ex.cx.se.env, col, vals)?))
        }
    }
}

/// SQL-level range bound value and inclusiveness. `Ok(None)` = NULL bound (no row can match).
fn range_bound(ex: &mut Executor, end: &RangeEnd, col: &KeyColumn) -> Result<Option<(Datum, bool)>> {
    let v = eval(ex, &end.value, &[])?;
    if matches!(v, Datum::Null) {
        return Ok(None);
    }
    let v = assign_cast(ex, v, end.value.ty, col.ty)?;
    Ok(Some((v, end.inclusive)))
}

/// Borrowed view of a KV bound.
fn bound_ref(b: &Bound<Vec<u8>>) -> Bound<&[u8]> {
    match b {
        Bound::Included(v) => Bound::Included(v.as_slice()),
        Bound::Excluded(v) => Bound::Excluded(v.as_slice()),
        Bound::Unbounded => Bound::Unbounded,
    }
}

// ---------------------------------------------------------------------------------------------
// Result
// ---------------------------------------------------------------------------------------------

struct ResultSource {
    exprs: Vec<BoundExpr>,
    one_time_filter: Option<BoundExpr>,
    done: bool,
}

impl RowSource for ResultSource {
    fn next(&mut self, ex: &mut Executor) -> Result<Option<Row>> {
        if self.done {
            return Ok(None);
        }
        self.done = true;
        if let Some(filter) = &self.one_time_filter {
            if !eval_qual(ex, filter, &[])? {
                return Ok(None);
            }
        }
        let row = eval_exprs(ex, &self.exprs, &[])?;
        Ok(Some(row))
    }

    fn rescan(&mut self, _ex: &mut Executor, _params: &[Datum]) -> Result<()> {
        self.done = false;
        Ok(())
    }
}

pub fn build_result(_node: &PlanNode, n: &ResultNode, _b: &mut BuildCtx) -> Result<Box<dyn RowSource>> {
    Ok(Box::new(ResultSource {
        exprs: n.exprs.clone(),
        one_time_filter: n.one_time_filter.clone(),
        done: false,
    }))
}

// ---------------------------------------------------------------------------------------------
// SeqScan
// ---------------------------------------------------------------------------------------------

struct SeqScanSource {
    relid: Oid,
    table_name: String,
    td: Rc<TableDesc>,
    filter: Option<BoundExpr>,
    with_rowid: bool,
    needed: Option<Vec<bool>>,
    cursor: HeapCursor,
    locked: bool,
    done: bool,
    counter: u32,
}

impl RowSource for SeqScanSource {
    fn next(&mut self, ex: &mut Executor) -> Result<Option<Row>> {
        if self.done {
            return Ok(None);
        }
        if !self.locked {
            lock_scan_relation(ex, self.relid, &self.table_name)?;
            self.locked = true;
        }
        loop {
            ex.tick(&mut self.counter)?;
            let (rowid, mut row) = match self.cursor.next(ex, &self.td, self.needed.as_deref())? {
                Some(item) => item,
                None => {
                    self.done = true;
                    return Ok(None);
                }
            };
            if self.with_rowid {
                row.push(Datum::Int8(rowid as i64));
            }
            if eval_opt_qual(ex, &self.filter, &row)? {
                return Ok(Some(row));
            }
        }
    }

    fn rescan(&mut self, _ex: &mut Executor, _params: &[Datum]) -> Result<()> {
        self.cursor.reset();
        self.locked = false;
        self.done = false;
        Ok(())
    }
}

pub fn build_seq_scan(_node: &PlanNode, n: &SeqScan, b: &mut BuildCtx) -> Result<Box<dyn RowSource>> {
    perm::require_relation_privilege(&b.cx.se, n.table.oid, acl_mode::SELECT, &needed_attnums(&n.needed))?;
    let td = b.table_desc(n.table.oid)?;
    Ok(Box::new(SeqScanSource {
        relid: n.table.oid,
        table_name: n.table.name.clone(),
        td,
        filter: n.filter.clone(),
        with_rowid: n.with_rowid,
        needed: needed_mask(&n.needed),
        cursor: HeapCursor::new(n.table.oid),
        locked: false,
        done: false,
        counter: 0,
    }))
}

// ---------------------------------------------------------------------------------------------
// IndexScan
// ---------------------------------------------------------------------------------------------

struct IndexScanSource {
    relid: Oid,
    table_name: String,
    td: Rc<TableDesc>,
    index_oid: Oid,
    /// Key columns of the index (`IndexRef::keys`) as the key codec sees them.
    key_cols: Vec<KeyColumn>,
    eq: Vec<KeyEq>,
    lower: Option<RangeEnd>,
    upper: Option<RangeEnd>,
    index_cond: Vec<BoundExpr>,
    recheck: bool,
    filter: Option<BoundExpr>,
    backward: bool,
    with_rowid: bool,
    needed: Option<Vec<bool>>,

    // ---- state of one pass (reset by `rescan`) ----
    started: bool,
    locked: bool,
    done: bool,
    counter: u32,
    /// Equality value combinations in visit order (reversed for `Backward`).
    order: Vec<Vec<Datum>>,
    /// Typed, non-NULL range bounds on key column `eq.len()`.
    range_lo: Option<(Datum, bool)>,
    range_hi: Option<(Datum, bool)>,
    /// Index into `order` of the next combination to set up.
    pos: usize,
    /// Byte interval being scanned.
    cur: Option<KeyInterval>,
    /// Every entry of `cur` has been read into `buf` (backward) or the last batch was short.
    cur_done: bool,
    /// Last key read from `cur` (forward re-seek point).
    after: Option<Vec<u8>>,
    /// Entries read but not yet consumed (front for forward, back for backward).
    buf: VecDeque<Entry>,
}

impl IndexScanSource {
    /// Evaluate the bounds (module docs) and reset the per-pass cursor.
    fn start(&mut self, ex: &mut Executor) -> Result<()> {
        let mut lists: Vec<Vec<Datum>> = Vec::with_capacity(self.eq.len());
        let mut possible = true;
        for (i, eq) in self.eq.iter().enumerate() {
            match eq_column_values(ex, eq, &self.key_cols[i])? {
                Some(vals) => lists.push(vals),
                None => {
                    possible = false;
                    break;
                }
            }
        }
        let mut lo: Option<(Datum, bool)> = None;
        let mut hi: Option<(Datum, bool)> = None;
        if possible {
            let col = self.eq.len();
            if let Some(r) = &self.lower {
                match range_bound(ex, r, &self.key_cols[col])? {
                    Some(b) => lo = Some(b),
                    None => possible = false,
                }
            }
            if possible {
                if let Some(r) = &self.upper {
                    match range_bound(ex, r, &self.key_cols[col])? {
                        Some(b) => hi = Some(b),
                        None => possible = false,
                    }
                }
            }
        }
        let mut order = if possible { cartesian(lists) } else { Vec::new() };
        if self.backward {
            order.reverse();
        }
        self.order = order;
        self.range_lo = lo;
        self.range_hi = hi;
        self.pos = 0;
        self.cur = None;
        self.cur_done = false;
        self.after = None;
        self.buf.clear();
        self.started = true;
        Ok(())
    }

    /// Byte interval of the combination at `self.pos` (`None` = matches nothing).
    fn interval_at_pos(&self, ex: &Executor) -> Result<Option<KeyInterval>> {
        let combo: &[Datum] = &self.order[self.pos];
        let lo = self.range_lo.as_ref().map(|(d, inc)| (d, *inc));
        let hi = self.range_hi.as_ref().map(|(d, inc)| (d, *inc));
        interval_for_bounds(&ex.cx.se.env, self.index_oid, &self.key_cols, combo, lo, hi)
    }

    /// Next batch of entries of `cur`, starting after `after`. Holds no iterator across calls.
    fn fetch_batch(&self, ex: &mut Executor) -> Result<Vec<Entry>> {
        ex.check_interrupts()?;
        let iv = match &self.cur {
            Some(iv) => iv,
            None => return Ok(Vec::new()),
        };
        let lower: Bound<&[u8]> = match &self.after {
            Some(k) => Bound::Excluded(k.as_slice()),
            None => bound_ref(&iv.start),
        };
        let upper = bound_ref(&iv.end);
        let kv: &dyn KvRead = &*ex.txn();
        kv_scan_vec(kv, lower, upper, SCAN_BATCH)
    }

    /// Next index entry in scan order, or `None` when every combination is exhausted.
    fn next_entry(&mut self, ex: &mut Executor) -> Result<Option<Entry>> {
        loop {
            let popped = if self.backward { self.buf.pop_back() } else { self.buf.pop_front() };
            if popped.is_some() {
                return Ok(popped);
            }
            if self.cur.is_some() && self.cur_done {
                self.cur = None;
                self.cur_done = false;
            }
            if self.cur.is_none() {
                if self.pos >= self.order.len() {
                    return Ok(None);
                }
                let iv = self.interval_at_pos(ex)?;
                self.pos += 1;
                self.after = None;
                self.cur = iv;
                self.cur_done = false;
                continue;
            }
            if self.backward {
                // Collect the whole interval (entries only), then consume it from the end.
                loop {
                    let batch = self.fetch_batch(ex)?;
                    let short = batch.len() < SCAN_BATCH;
                    if let Some(last) = batch.last() {
                        self.after = Some(last.0.clone());
                    }
                    self.buf.extend(batch);
                    if short {
                        break;
                    }
                }
                self.cur_done = true;
            } else {
                let batch = self.fetch_batch(ex)?;
                self.cur_done = batch.len() < SCAN_BATCH;
                if let Some(last) = batch.last() {
                    self.after = Some(last.0.clone());
                }
                self.buf.extend(batch);
            }
        }
    }
}

impl RowSource for IndexScanSource {
    fn next(&mut self, ex: &mut Executor) -> Result<Option<Row>> {
        if self.done {
            return Ok(None);
        }
        if !self.started {
            if !self.locked {
                lock_scan_relation(ex, self.relid, &self.table_name)?;
                self.locked = true;
            }
            self.start(ex)?;
        }
        loop {
            ex.tick(&mut self.counter)?;
            let (_key, value) = match self.next_entry(ex)? {
                Some(entry) => entry,
                None => {
                    self.done = true;
                    return Ok(None);
                }
            };
            let rowid = rowid_of_entry_value(&value)?;
            let mut row = match rowstore::get_row(ex, &self.td, rowid)? {
                Some(r) => r,
                None => continue, // stale index entry
            };
            if self.with_rowid {
                row.push(Datum::Int8(rowid as i64));
            }
            let mut keep = true;
            if self.recheck {
                for cond in &self.index_cond {
                    if !eval_qual(ex, cond, &row)? {
                        keep = false;
                        break;
                    }
                }
            }
            if keep && eval_opt_qual(ex, &self.filter, &row)? {
                return Ok(Some(row));
            }
        }
    }

    fn rescan(&mut self, _ex: &mut Executor, _params: &[Datum]) -> Result<()> {
        self.started = false;
        self.locked = false;
        self.done = false;
        self.order.clear();
        self.range_lo = None;
        self.range_hi = None;
        self.pos = 0;
        self.cur = None;
        self.cur_done = false;
        self.after = None;
        self.buf.clear();
        Ok(())
    }
}

pub fn build_index_scan(_node: &PlanNode, n: &IndexScan, b: &mut BuildCtx) -> Result<Box<dyn RowSource>> {
    perm::require_relation_privilege(&b.cx.se, n.table.oid, acl_mode::SELECT, &needed_attnums(&n.needed))?;
    let td = b.table_desc(n.table.oid)?;
    let nkeys = n.index.keys.len();
    let nrange = usize::from(n.bounds.lower.is_some() || n.bounds.upper.is_some());
    let nused = n.bounds.eq.len() + nrange;
    if nused > nkeys {
        return Err(PgError::internal(format!(
            "index scan on {} constrains {} key columns but the index has {}",
            n.index.name, nused, nkeys
        )));
    }
    if n.index.keys.iter().take(nused).any(|k| k.column.is_none()) {
        return Err(PgError::internal(format!(
            "index scan on {} uses an expression key column, which is not supported",
            n.index.name
        )));
    }
    let key_cols: Vec<KeyColumn> = n
        .index
        .keys
        .iter()
        .map(|k| KeyColumn { ty: k.ty, desc: k.descending, nulls_first: k.nulls_first })
        .collect();
    Ok(Box::new(IndexScanSource {
        relid: n.table.oid,
        table_name: n.table.name.clone(),
        td,
        index_oid: n.index.oid,
        key_cols,
        eq: n.bounds.eq.clone(),
        lower: n.bounds.lower.clone(),
        upper: n.bounds.upper.clone(),
        index_cond: n.index_cond.clone(),
        recheck: n.recheck_index_cond,
        filter: n.filter.clone(),
        backward: matches!(n.direction, ScanDirection::Backward),
        with_rowid: n.with_rowid,
        needed: needed_mask(&n.needed),
        started: false,
        locked: false,
        done: false,
        counter: 0,
        order: Vec::new(),
        range_lo: None,
        range_hi: None,
        pos: 0,
        cur: None,
        cur_done: false,
        after: None,
        buf: VecDeque::new(),
    }))
}

// ---------------------------------------------------------------------------------------------
// VirtualScan
// ---------------------------------------------------------------------------------------------

struct VirtualScanSource {
    oid: Oid,
    /// Output width (relation columns), checked against every produced row.
    width: usize,
    eq_filters: Vec<(i16, BoundExpr)>,
    filter: Option<BoundExpr>,
    loaded: bool,
    buf: VecDeque<Row>,
    counter: u32,
}

impl VirtualScanSource {
    fn load(&mut self, ex: &mut Executor) -> Result<()> {
        let mut eq: Vec<(usize, Datum)> = Vec::with_capacity(self.eq_filters.len());
        for (attnum, expr) in &self.eq_filters {
            let value = eval(ex, expr, &[])?;
            // A NULL hint can never match; the complete `filter` is re-applied anyway, so skip it.
            if matches!(value, Datum::Null) {
                continue;
            }
            let idx = usize::try_from(i32::from(*attnum) - 1).map_err(|_| {
                PgError::internal(format!("invalid attribute number {attnum} in virtual relation scan"))
            })?;
            eq.push((idx, value));
        }
        let rows = {
            let info = ExecSessionInfo::new(&ex.cx.se, &*ex.cx.txn);
            let ctx = ScanContext { env: &ex.cx.se.env, session: &info, io_config: &ex.cx.se.io };
            vrel_scan(&ctx, self.oid, &ScanRequest { eq })?
        };
        self.buf = rows.into();
        Ok(())
    }
}

impl RowSource for VirtualScanSource {
    fn next(&mut self, ex: &mut Executor) -> Result<Option<Row>> {
        if !self.loaded {
            self.load(ex)?;
            self.loaded = true;
        }
        loop {
            ex.tick(&mut self.counter)?;
            let row = match self.buf.pop_front() {
                Some(r) => r,
                None => return Ok(None),
            };
            if row.len() != self.width {
                return Err(PgError::internal(format!(
                    "virtual relation produced a row of {} values, expected {}",
                    row.len(),
                    self.width
                )));
            }
            if eval_opt_qual(ex, &self.filter, &row)? {
                return Ok(Some(row));
            }
        }
    }

    fn rescan(&mut self, _ex: &mut Executor, _params: &[Datum]) -> Result<()> {
        self.loaded = false;
        self.buf.clear();
        Ok(())
    }
}

pub fn build_virtual_scan(node: &PlanNode, n: &VirtualScan, _b: &mut BuildCtx) -> Result<Box<dyn RowSource>> {
    Ok(Box::new(VirtualScanSource {
        oid: n.table.oid,
        width: node.width(),
        eq_filters: n.eq_filters.clone(),
        filter: n.filter.clone(),
        loaded: false,
        buf: VecDeque::new(),
        counter: 0,
    }))
}

// ---------------------------------------------------------------------------------------------
// ValuesScan
// ---------------------------------------------------------------------------------------------

struct ValuesScanSource {
    rows: Vec<Vec<BoundExpr>>,
    filter: Option<BoundExpr>,
    idx: usize,
    counter: u32,
}

impl RowSource for ValuesScanSource {
    fn next(&mut self, ex: &mut Executor) -> Result<Option<Row>> {
        loop {
            if self.idx >= self.rows.len() {
                return Ok(None);
            }
            ex.tick(&mut self.counter)?;
            let cells = &self.rows[self.idx];
            self.idx += 1;
            let mut row: Row = Vec::with_capacity(cells.len());
            for cell in cells {
                row.push(eval(ex, cell, &[])?);
            }
            if eval_opt_qual(ex, &self.filter, &row)? {
                return Ok(Some(row));
            }
        }
    }

    fn rescan(&mut self, _ex: &mut Executor, _params: &[Datum]) -> Result<()> {
        self.idx = 0;
        Ok(())
    }
}

pub fn build_values_scan(_node: &PlanNode, n: &ValuesScan, _b: &mut BuildCtx) -> Result<Box<dyn RowSource>> {
    Ok(Box::new(ValuesScanSource { rows: n.rows.clone(), filter: n.filter.clone(), idx: 0, counter: 0 }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::context::{ExecContext, NoGuc, NoServices, SessionEnv};
    use crate::memtxn::MemTxn;
    use pgarbor_sql::ir::ExprKind;
    use pgarbor_types::context::IoConfig;
    use std::cell::Cell;

    fn int4_key(desc: bool) -> KeyColumn {
        KeyColumn { ty: TypeId::INT4, desc, nulls_first: false }
    }

    fn int4_const(v: i32) -> BoundExpr {
        BoundExpr { kind: ExprKind::Const(Datum::Int4(v)), ty: TypeId::INT4 }
    }

    #[test]
    fn needed_helpers_follow_attnum_order() {
        assert_eq!(needed_attnums(&[true, false, true]), vec![1, 3]);
        assert_eq!(needed_mask(&[true, true]), None);
        assert_eq!(needed_mask(&[true, false]), Some(vec![true, false]));
    }

    #[test]
    fn cartesian_is_lexicographic() {
        let lists = vec![vec![Datum::Int4(1), Datum::Int4(2)], vec![Datum::Int4(7)]];
        assert_eq!(
            cartesian(lists),
            vec![vec![Datum::Int4(1), Datum::Int4(7)], vec![Datum::Int4(2), Datum::Int4(7)]]
        );
        assert!(cartesian(vec![Vec::new()]).is_empty());
        assert_eq!(cartesian(Vec::new()), vec![Vec::<Datum>::new()]);
    }

    #[test]
    fn sort_dedup_is_in_index_order() -> Result<()> {
        let (_kv, env) = pgarbor_catalog::testing::bootstrapped()?;
        let vals = vec![Datum::Int4(3), Datum::Int4(1), Datum::Int4(3), Datum::Int4(2)];
        assert_eq!(
            sort_dedup(&env, &int4_key(false), vals.clone())?,
            vec![Datum::Int4(1), Datum::Int4(2), Datum::Int4(3)]
        );
        assert_eq!(
            sort_dedup(&env, &int4_key(true), vals)?,
            vec![Datum::Int4(3), Datum::Int4(2), Datum::Int4(1)]
        );
        Ok(())
    }

    #[test]
    fn bound_ref_keeps_inclusiveness() {
        assert_eq!(bound_ref(&Bound::Excluded(vec![1, 2])), Bound::Excluded(&[1u8, 2][..]));
        assert_eq!(bound_ref(&Bound::Unbounded), Bound::<&[u8]>::Unbounded);
    }

    fn with_exec<T>(f: impl FnOnce(&mut Executor) -> Result<T>) -> Result<T> {
        let (_kv, env) = pgarbor_catalog::testing::bootstrapped()?;
        let rng = Cell::new(1u64);
        let se = SessionEnv::new(env, IoConfig::default(), &NoGuc, &NoServices, &rng, 0, 0);
        let mut txn = MemTxn::new();
        let mut cx = ExecContext::new(&mut txn, se);
        let mut rt = crate::runtime::Runtime::default();
        let mut ex = Executor::new(&mut rt, &mut cx);
        f(&mut ex)
    }

    #[test]
    fn values_scan_yields_each_row_once() -> Result<()> {
        with_exec(|ex| {
            let mut op = ValuesScanSource {
                rows: vec![vec![int4_const(1), int4_const(2)], vec![int4_const(3), int4_const(4)]],
                filter: None,
                idx: 0,
                counter: 0,
            };
            assert_eq!(op.next(ex)?, Some(vec![Datum::Int4(1), Datum::Int4(2)]));
            assert_eq!(op.next(ex)?, Some(vec![Datum::Int4(3), Datum::Int4(4)]));
            assert_eq!(op.next(ex)?, None);
            op.rescan(ex, &[])?;
            assert_eq!(op.next(ex)?, Some(vec![Datum::Int4(1), Datum::Int4(2)]));
            Ok(())
        })
    }

    #[test]
    #[ignore = "needs EXE-05 (expression evaluation)"]
    fn result_node_yields_once_and_respects_one_time_filter() -> Result<()> {
        with_exec(|ex| {
            let mut op = ResultSource {
                exprs: vec![int4_const(9)],
                one_time_filter: Some(BoundExpr { kind: ExprKind::Const(Datum::Bool(false)), ty: TypeId::BOOL }),
                done: false,
            };
            assert_eq!(op.next(ex)?, None);
            op.rescan(ex, &[])?;
            assert_eq!(op.next(ex)?, None);
            Ok(())
        })
    }

    #[test]
    #[ignore = "needs EXE-05 (expression evaluation) and EXE-01/EXE-02 (heap rows)"]
    fn seq_scan_style_row_source_adds_rowid_in_rowid_order() {
        // Covered centrally once table descriptions and heap rows exist: 600 rows come back in
        // rowid order with a trailing Int8 rowid when `with_rowid` is set (acceptance criterion).
    }

    #[test]
    #[ignore = "needs EXE-03 (index keys) and EXE-02 (heap rows)"]
    fn index_scan_equality_range_and_backward_order() {
        // Covered centrally: equality on a unique index returns one row, `a >= 5 and a < 9`
        // returns 5..8 and Backward returns descending order.
    }
}
