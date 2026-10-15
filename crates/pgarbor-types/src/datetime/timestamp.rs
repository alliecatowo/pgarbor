//! `timestamp` / `timestamptz` I/O and operators (TYPE-06, TYPE-14). Lease TYP-28.
//!
//! PG18 reference: `src/backend/utils/adt/timestamp.c` (`timestamp_in`, `timestamptz_in`,
//! `timestamp_out`, `AdjustTimestampForTypmod`, `timestamp_pl_interval`, `timestamp_mi`,
//! `timestamptz_pl_interval_internal`, comparison & cast functions).
//!
//! I/O:
//! - Input via `decode::parse_date_time(s, ctx, "timestamp without time zone" | "timestamp with time zone")`.
//!   timestamp ignores any zone in the input. timestamptz uses the input zone, else the session zone
//!   (`tz::offset_at_local`). Specials: `epoch`, `infinity`, `-infinity`, `now`, `today`, ...
//!   Range: outside `MIN_TIMESTAMP ..< END_TIMESTAMP` → 22008 `timestamp out of range: "294277-01-01"`.
//!   Typmod precision p (0..=6) rounds half away from zero (`.125` at p=2 → `.13`, oracle). Precision > 6
//!   is clamped by the typmod layer with a WARNING (not here).
//!   Fractional input beyond µs rounds: `'2020-01-01 00:00:00.123456789'` → `...00.123457`.
//! - Output via `encode::encode_date_time` with `ctx.config.date_style`; timestamptz is converted to the
//!   session zone (`tz::offset_at_utc`) and prints its offset (`2024-02-29 07:31:02.5+00`).
//! - Binary: i64 BE µs since 2000-01-01 (UTC for timestamptz); recv range-checks (22008).
//!
//! Owned procs (ImplModule::Timestamp): comparisons `timestamp_{eq,ne,lt,le,gt,ge,cmp}` (shared by
//! timestamptz: same prosrc), cross-type with date and timestamp/timestamptz (`timestamp_eq_timestamptz`,
//! `timestamptz_lt_date`, ...: convert timestamp↔timestamptz in the session zone), `timestamp_larger/smaller`,
//! `timestamp_hash(_extended)`, `timestamptz_hash(_extended)`, `timestamp_finite`, `timestamp_mi`
//! (→ interval, days not months: `timestamp '2024-01-01' - timestamp '2023-01-01'` = `365 days`; infinite
//! operand → 22008 `cannot subtract infinite timestamps`), `timestamp_pl_interval`, `timestamp_mi_interval`,
//! `timestamptz_pl_interval`, `timestamptz_mi_interval` (add months clamping day-of-month: Jan 31 + 1 mon =
//! Feb 29 (leap); then days (in the session zone for tz), then µs), casts `timestamp_timestamptz`,
//! `timestamptz_timestamp`, `timestamp_date`, `timestamptz_date`, `timestamp_time`, `timestamptz_time`,
//! `timestamptz_timetz`; SQL-language `timestamptz(date, time)` (oid 1176), `timedate_pl` (1296),
//! `timetzdate_pl` (1298), `interval_pl_date` (2546), `interval_pl_timestamp` (2548),
//! `interval_pl_timestamptz` (2549); `in_range_timestamp(tz)_interval`.
use crate::catalog_data::BuiltinProc;
use crate::context::IoCtx;
use crate::datum::{Datum, Interval, TypeId};
use crate::func::FuncImpl;
use pgarbor_common::{PgError, Result};

/// Round a timestamp to `precision` fractional digits (no-op when precision < 0 or >= 6, or infinite).
// CONTRACT
#[allow(unused_variables)]
pub fn adjust_timestamp_precision(ts: i64, precision: i32) -> Result<i64> {
    Err(PgError::unimplemented("TYP-28", "timestamp precision"))
}

/// `ts + iv` for timestamp WITHOUT time zone (PG `timestamp_pl_interval`).
// CONTRACT
#[allow(unused_variables)]
pub fn timestamp_add_interval(ts: i64, iv: &Interval) -> Result<i64> {
    Err(PgError::unimplemented("TYP-28", "timestamp + interval"))
}

/// `timestamp_in` / `timestamptz_in` by `ty.oid`.
#[allow(unused_variables)]
pub fn input(ctx: &IoCtx, ty: TypeId, s: &str) -> Result<Datum> {
    Err(PgError::unimplemented("TYP-28", "timestamp input"))
}

/// `timestamp_out` / `timestamptz_out`.
#[allow(unused_variables)]
pub fn output(ctx: &IoCtx, ty: TypeId, d: &Datum) -> Result<String> {
    Err(PgError::unimplemented("TYP-28", "timestamp output"))
}

/// `timestamp_recv` / `timestamptz_recv`.
#[allow(unused_variables)]
pub fn recv(ctx: &IoCtx, ty: TypeId, buf: &[u8]) -> Result<Datum> {
    Err(PgError::unimplemented("TYP-28", "timestamp binary input"))
}

/// `timestamp_send` / `timestamptz_send`.
#[allow(unused_variables)]
pub fn send(ctx: &IoCtx, ty: TypeId, d: &Datum) -> Result<Vec<u8>> {
    Err(PgError::unimplemented("TYP-28", "timestamp binary output"))
}

/// Function registry hook (see `crate::func`).
#[allow(unused_variables)]
pub(crate) fn lookup(proc: &BuiltinProc) -> Option<FuncImpl> {
    None
}

/// 0A000 for a Timestamp-module proc without implementation.
pub(crate) fn unsupported(proc: &BuiltinProc) -> PgError {
    PgError::unimplemented("TYP-28", format!("function {}", proc.name))
}
