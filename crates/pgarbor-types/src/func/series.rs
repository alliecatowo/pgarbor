//! `generate_series` (TYPE-14). Lease TYP-44.
//!
//! PG18 reference: `utils/adt/int.c` (`generate_series_int4`, `_step_int4`), `int8.c`, `numeric.c`
//! (`generate_series_numeric`, `_step_numeric`), `timestamp.c` (`generate_series_timestamp`,
//! `generate_series_timestamptz`, `generate_series_timestamptz_at_zone`).
//! Owned procs (ImplModule::Series, all `FuncImpl::Set`, lazily iterating — `generate_series(1, 1e9)`
//! must not allocate the whole set): `generate_series_int4 generate_series_step_int4 generate_series_int8
//! generate_series_step_int8 generate_series_numeric generate_series_step_numeric generate_series_timestamp
//! generate_series_timestamptz generate_series_timestamptz_at_zone`.
//!
//! Semantics: inclusive bounds; step default 1; step 0 → 22023 `step size cannot equal zero`; negative
//! step counts down; empty when start > stop with positive step. Integer iteration stops before
//! overflow (no error: `generate_series(2147483646, 2147483647)` yields 2 rows). numeric: NaN start →
//! 22023 `start value cannot be NaN`, NaN stop → `stop value cannot be NaN`, NaN step → `step size cannot be NaN`;
//! infinite start/stop → 22023 `start value cannot be infinity` / `stop value cannot be infinity`
//! (step infinity → `step size cannot be infinity`); result dscale = max(start, step dscales).
//! timestamp: interval step (zero interval → 22023 `step size cannot equal zero`; infinite step →
//! `step size cannot be infinite`); timestamptz arithmetic in the session zone (or the given zone).
use crate::catalog_data::BuiltinProc;
use crate::func::FuncImpl;
use pgarbor_common::PgError;

/// Function registry hook (see `crate::func`).
#[allow(unused_variables)]
pub(crate) fn lookup(proc: &BuiltinProc) -> Option<FuncImpl> {
    None
}

/// 0A000 for a Series-module proc without implementation.
pub(crate) fn unsupported(proc: &BuiltinProc) -> PgError {
    PgError::unimplemented("TYP-44", format!("function {}", proc.name))
}
