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
use pgarbor_common::{PgError, Result};
use pgarbor_plan::plan::{PlanNode, ResultNode, SeqScan, IndexScan, ValuesScan, VirtualScan};

use crate::build::BuildCtx;
use crate::runtime::RowSource;

#[allow(unused_variables)]
pub fn build_result(node: &PlanNode, n: &ResultNode, b: &mut BuildCtx) -> Result<Box<dyn RowSource>> {
    Err(PgError::unimplemented("EXE-12", "Result node (node_scan::build_result)"))
}

#[allow(unused_variables)]
pub fn build_seq_scan(node: &PlanNode, n: &SeqScan, b: &mut BuildCtx) -> Result<Box<dyn RowSource>> {
    Err(PgError::unimplemented("EXE-12", "SeqScan node (node_scan::build_seq_scan)"))
}

#[allow(unused_variables)]
pub fn build_index_scan(node: &PlanNode, n: &IndexScan, b: &mut BuildCtx) -> Result<Box<dyn RowSource>> {
    Err(PgError::unimplemented("EXE-12", "IndexScan node (node_scan::build_index_scan)"))
}

#[allow(unused_variables)]
pub fn build_virtual_scan(node: &PlanNode, n: &VirtualScan, b: &mut BuildCtx) -> Result<Box<dyn RowSource>> {
    Err(PgError::unimplemented("EXE-12", "VirtualScan node (node_scan::build_virtual_scan)"))
}

#[allow(unused_variables)]
pub fn build_values_scan(node: &PlanNode, n: &ValuesScan, b: &mut BuildCtx) -> Result<Box<dyn RowSource>> {
    Err(PgError::unimplemented("EXE-12", "ValuesScan node (node_scan::build_values_scan)"))
}
