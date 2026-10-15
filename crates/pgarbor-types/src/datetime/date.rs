//! `date` I/O and operators (TYPE-06, TYPE-14). Lease TYP-26.
//!
//! PG18 reference: `src/backend/utils/adt/date.c`.
//!
//! I/O: input via `decode::parse_date_time(s, ctx, "date")`; specials `epoch` → 1970-01-01,
//! `infinity`/`-infinity` → `DATE_INFINITY`/`DATE_NEG_INFINITY`, `now`/`today` → current date in the
//! session zone, `tomorrow`/`yesterday`; a time part is accepted and ignored; a zone is ignored.
//! Out of range (outside Julian 0 ..< `DATE_END_JULIAN`) → 22008 `date out of range: "5874898-01-01"`.
//! Output via `encode::encode_date_only` under `ctx.config` (ISO: `2024-02-29`, `0044-03-15 BC`,
//! `infinity`). Binary: i32 big-endian days since 2000-01-01 (recv range-checks: 22008 `date out of range`).
//!
//! Owned procs (ImplModule::Date): `date_eq date_ne date_lt date_le date_gt date_ge date_cmp
//! date_larger date_smaller hashdate(+extended) date_finite (isfinite)`, cross-type comparisons with
//! timestamp/timestamptz `date_{eq,ne,lt,le,gt,ge,cmp}_timestamp(tz)` (date promoted to midnight; for
//! timestamptz, midnight in the SESSION zone `ctx.io.time_zone`), `date_pli` (date + int4),
//! `date_mii` (date - int4), `date_mi` (date - date → int4; infinite operand → 22008
//! `cannot subtract infinite dates`), `date_pl_interval` / `date_mi_interval` (→ timestamp, via
//! timestamp + interval), `date_timestamp`, `date_timestamptz` (casts), `datetime_timestamp`
//! (date + time → timestamp), `datetimetz_timestamptz` (date + timetz), `make_date(y, m, d)` (negative
//! year = BC; invalid → 22008 `date field value out of range: 2024-02-30`), SQL-language
//! `integer_pl_date` (oid 2550, int4 + date), `in_range_date_interval`.
//! Oracle: `date '2024-02-29' + 1` = `2024-03-01`; `date '2024-03-01' - date '2024-02-01'` = 29;
//! `date '2024-02-29' + interval '1 year'` = `2025-02-28 00:00:00`; `'infinity'::date + 1` = `infinity`.
//! Arithmetic overflow → 22008 `date out of range`.
use crate::catalog_data::BuiltinProc;
use crate::context::IoCtx;
use crate::datum::{Datum, TypeId};
use crate::func::FuncImpl;
use pgarbor_common::{PgError, Result};

/// Date (days since 2000-01-01) → midnight timestamp (µs). Infinities map to timestamp infinities;
/// dates beyond the timestamp range → 22008 `date out of range for timestamp`.
// CONTRACT
#[allow(unused_variables)]
pub fn date_to_timestamp(d: i32) -> Result<i64> {
    Err(PgError::unimplemented("TYP-26", "date to timestamp"))
}

/// `date_in`.
#[allow(unused_variables)]
pub fn input(ctx: &IoCtx, ty: TypeId, s: &str) -> Result<Datum> {
    Err(PgError::unimplemented("TYP-26", "date input"))
}

/// `date_out`.
#[allow(unused_variables)]
pub fn output(ctx: &IoCtx, ty: TypeId, d: &Datum) -> Result<String> {
    Err(PgError::unimplemented("TYP-26", "date output"))
}

/// `date_recv`.
#[allow(unused_variables)]
pub fn recv(ctx: &IoCtx, ty: TypeId, buf: &[u8]) -> Result<Datum> {
    Err(PgError::unimplemented("TYP-26", "date binary input"))
}

/// `date_send`.
#[allow(unused_variables)]
pub fn send(ctx: &IoCtx, ty: TypeId, d: &Datum) -> Result<Vec<u8>> {
    Err(PgError::unimplemented("TYP-26", "date binary output"))
}

/// Function registry hook (see `crate::func`).
#[allow(unused_variables)]
pub(crate) fn lookup(proc: &BuiltinProc) -> Option<FuncImpl> {
    None
}

/// 0A000 for a Date-module proc without implementation.
pub(crate) fn unsupported(proc: &BuiltinProc) -> PgError {
    PgError::unimplemented("TYP-26", format!("function {}", proc.name))
}
