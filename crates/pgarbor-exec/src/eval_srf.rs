//! Set-returning function evaluation (lease EXE-08). Used by `node_func` (`FunctionScan`,
//! `ProjectSet`). PG references: `executor/nodeFunctionscan.c`, `nodeProjectSet.c`,
//! `execSRF.c` (`ExecMakeFunctionResultSet`).
//!
//! ## Opening a set-returning call
//! [`open_srf`] evaluates the `FuncCall`'s arguments over `row`, resolves the handle
//! (`eval_call::resolve_handle`) and returns a LAZY iterator ([`SrfIter`]):
//! * `FuncImpl::Set(f)` (`generate_series`, `unnest`, `jsonb_each`, `regexp_matches`, `string_to_table`,
//!   ...): `handle.call_set(args, &FuncCtx)`. Strict procs with a NULL argument yield an empty
//!   iterator without calling (`FuncHandle::call_set` does it).
//! * `FuncImpl::External` with `retset` (`pg_listening_channels`, `pg_get_keywords`,
//!   `pg_options_to_table`, `pg_show_all_settings`, `pg_stat_get_activity`, `pg_lock_status`, ...):
//!   exec-owned ones through `session_fns::call_set`; catalog ones through
//!   `pgarbor_catalog::sysfuncs::call_set` (returns a `Vec<Datum>`; wrap in an iterator).
//! * A scalar function in a FROM item (`FROM now()`, `ProjectSet` of a non-SRF expression): evaluate
//!   once, yield exactly one row.
//! The `FuncCtx` borrows only `ex.cx`; the returned iterator must not borrow anything ('static).
//!
//! ## Row shape
//! A SRF whose result type is a scalar yields one value per row. A SRF returning a composite
//! (`FunctionItem::columns.len() > 1`, `Datum::Record`) is EXPANDED by [`expand_record`] into one
//! value per column; a NULL record expands to NULLs. `columns.len() == 1` with a record-typed column
//! is NOT expanded (`SELECT * FROM unnest(array[row(1,2)])` keeps one column).
use pgarbor_common::{PgError, Result};
use pgarbor_sql::ir::FuncCallExpr;
use pgarbor_types::{Datum, TypeId};

use crate::runtime::Executor;

/// Lazy rows of one set-returning call.
pub type SrfIter = Box<dyn Iterator<Item = Result<Datum>>>;

/// Evaluate arguments and start a SRF (module docs). `result_type` is the call's static type.
#[allow(unused_variables)]
pub fn open_srf(ex: &mut Executor, call: &FuncCallExpr, result_type: TypeId, row: &[Datum]) -> Result<SrfIter> {
    Err(PgError::unimplemented("EXE-08", "set-returning function start (eval_srf::open_srf)"))
}

/// Expand a SRF output `value` into `ncols` column values (module docs, "Row shape").
#[allow(unused_variables)]
pub fn expand_record(value: Datum, ncols: usize) -> Result<Vec<Datum>> {
    Err(PgError::unimplemented("EXE-08", "composite expansion (eval_srf::expand_record)"))
}

/// True if `e` is, or contains outside any sub-plan, a `FuncCall` with `retset` (the
/// `ProjectSet` lock-step rule in plan.rs). Pure tree walk (`BoundExpr::walk`).
#[allow(unused_variables)]
pub fn contains_srf(e: &pgarbor_sql::ir::BoundExpr) -> Result<bool> {
    Err(PgError::unimplemented("EXE-08", "SRF detection (eval_srf::contains_srf)"))
}
