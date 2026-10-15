//! `time` and `timetz` I/O and operators (TYPE-06, TYPE-14). Lease TYP-27.
//!
//! PG18 reference: `src/backend/utils/adt/date.c` (time_* / timetz_* functions, `AdjustTimeForTypmod`).
//!
//! I/O:
//! - time: input via `decode::parse_time_only(s, ctx, "time without time zone")` (zone ignored),
//!   then precision typmod `ty.typmod` (0..=6) rounds the fraction half away from zero
//!   (`'12:00:00.123456'::time(2)` → `12:00:00.12`); `24:00:00` allowed. Output `13:01:02.5`.
//!   Binary: i64 BE µs.
//! - timetz: input via `parse_time_only(s, ctx, "time with time zone")`; zone from input or, if absent,
//!   the session zone (`tz::offset_at_local` evaluated for the current date). Stored as
//!   `Datum::TimeTz(µs, secs_west)`. Output `12:34:56.789+05:30`, `04:05:06-08`. Binary: i64 BE µs then
//!   i32 BE zone (seconds west, PG convention). recv: |zone| > 15:59:59 → 22009
//!   `time zone displacement out of range`.
//!
//! Owned procs (ImplModule::Time): time `time_eq ne lt le gt ge cmp larger smaller hash(_extended)
//! time_pl_interval time_mi_interval (wrap modulo 24h) time_mi_time (→ interval) time_interval (cast)
//! interval_time (cast, modulo day) time_timetz (session zone) make_time(h, m, s float8) (invalid →
//! 22008 `time field value out of range: 25:00:00`) in_range_time_interval`; timetz `timetz_eq ...
//! timetz_cmp` (compare UTC-adjusted time first, then zone: PG `timetz_cmp_internal`) `timetz_larger
//! timetz_smaller timetz_hash(_extended) timetz_pl_interval timetz_mi_interval timetz_time
//! in_range_timetz_interval`; SQL-language `interval_pl_time` (oid 1848) and `interval_pl_timetz` (2547).
use crate::catalog_data::BuiltinProc;
use crate::context::IoCtx;
use crate::datum::{Datum, TypeId};
use crate::func::FuncImpl;
use pgarbor_common::{PgError, Result};

/// Round µs-since-midnight to `precision` fractional digits (no-op for precision < 0 or >= 6).
// CONTRACT
#[allow(unused_variables)]
pub fn adjust_time_precision(t: i64, precision: i32) -> Result<i64> {
    Err(PgError::unimplemented("TYP-27", "time precision"))
}

/// `time_in` / `timetz_in` by `ty.oid`.
#[allow(unused_variables)]
pub fn input(ctx: &IoCtx, ty: TypeId, s: &str) -> Result<Datum> {
    Err(PgError::unimplemented("TYP-27", "time input"))
}

/// `time_out` / `timetz_out`.
#[allow(unused_variables)]
pub fn output(ctx: &IoCtx, ty: TypeId, d: &Datum) -> Result<String> {
    Err(PgError::unimplemented("TYP-27", "time output"))
}

/// `time_recv` / `timetz_recv`.
#[allow(unused_variables)]
pub fn recv(ctx: &IoCtx, ty: TypeId, buf: &[u8]) -> Result<Datum> {
    Err(PgError::unimplemented("TYP-27", "time binary input"))
}

/// `time_send` / `timetz_send`.
#[allow(unused_variables)]
pub fn send(ctx: &IoCtx, ty: TypeId, d: &Datum) -> Result<Vec<u8>> {
    Err(PgError::unimplemented("TYP-27", "time binary output"))
}

/// Function registry hook (see `crate::func`).
#[allow(unused_variables)]
pub(crate) fn lookup(proc: &BuiltinProc) -> Option<FuncImpl> {
    None
}

/// 0A000 for a Time-module proc without implementation.
pub(crate) fn unsupported(proc: &BuiltinProc) -> PgError {
    PgError::unimplemented("TYP-27", format!("function {}", proc.name))
}
