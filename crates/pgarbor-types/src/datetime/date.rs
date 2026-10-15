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
use super::calendar;
use super::decode;
use super::encode;
use super::timestamp::timestamp_add_interval;
use super::tz;
use super::{
    DateTimeKind, Tm, DATETIME_MIN_JULIAN, DATE_END_JULIAN, END_TIMESTAMP, MIN_TIMESTAMP, POSTGRES_EPOCH_JDATE,
    TIMESTAMP_END_JULIAN, UNIX_EPOCH_JDATE, USECS_PER_DAY, USECS_PER_SEC,
};
use crate::catalog_data::{BuiltinProc, ImplModule};
use crate::context::{FuncCtx, IoCtx, TimeZoneSetting};
use crate::datum::{
    type_mismatch, Datum, Interval, TypeId, DATE_INFINITY, DATE_NEG_INFINITY, TIMESTAMP_INFINITY,
    TIMESTAMP_NEG_INFINITY,
};
use crate::func::{FuncImpl, ScalarFn};
use crate::io::invalid_binary;
use pgarbor_common::{PgError, Result, SqlState};
use std::cmp::Ordering;

/// Days since 2000-01-01 of the first valid date (Julian day 0).
const DATE_MIN_DAYS: i32 = DATETIME_MIN_JULIAN - POSTGRES_EPOCH_JDATE;
/// Days since 2000-01-01 of the first INVALID date (`DATE_END_JULIAN`).
const DATE_END_DAYS: i32 = DATE_END_JULIAN - POSTGRES_EPOCH_JDATE;
/// Days since 2000-01-01 from which a midnight does not fit a timestamp (`date2timestamp` overflow).
const TIMESTAMP_OVERFLOW_DAYS: i32 = TIMESTAMP_END_JULIAN - POSTGRES_EPOCH_JDATE;

// PG `JULIAN_*` limits (`datatype/timestamp.h`), used by `IS_VALID_JULIAN`.
const JULIAN_MINYEAR: i32 = -4713;
const JULIAN_MINMONTH: i32 = 11;
const JULIAN_MAXYEAR: i32 = 5874898;
const JULIAN_MAXMONTH: i32 = 6;

fn is_not_finite(d: i32) -> bool {
    d == DATE_INFINITY || d == DATE_NEG_INFINITY
}

/// PG `IS_VALID_DATE` on day numbers since 2000-01-01.
fn is_valid_date(d: i32) -> bool {
    (DATE_MIN_DAYS..DATE_END_DAYS).contains(&d)
}

/// PG `IS_VALID_JULIAN(y, m, d)` (the day does not take part in the check).
fn is_valid_julian(year: i32, mon: i32) -> bool {
    (year > JULIAN_MINYEAR || (year == JULIAN_MINYEAR && mon >= JULIAN_MINMONTH))
        && (year < JULIAN_MAXYEAR || (year == JULIAN_MAXYEAR && mon < JULIAN_MAXMONTH))
}

/// PG `IS_VALID_TIMESTAMP` on a wide value (µs since 2000-01-01).
fn ts_in_range(v: i128) -> bool {
    v >= MIN_TIMESTAMP as i128 && v < END_TIMESTAMP as i128
}

/// Local midnight of a day number as µs since 2000-01-01, computed wide so that dates outside the
/// timestamp range do not overflow.
fn midnight_wide(d: i32) -> i128 {
    d as i128 * USECS_PER_DAY as i128
}

fn ts_overflow_error() -> PgError {
    PgError::new(SqlState::DATETIME_VALUE_OUT_OF_RANGE, "date out of range for timestamp")
}

fn date_out_of_range_input(s: &str) -> PgError {
    PgError::new(SqlState::DATETIME_VALUE_OUT_OF_RANGE, format!("date out of range: \"{s}\""))
}

fn field_overflow_ymd(year: i32, mon: i32, mday: i32) -> PgError {
    PgError::new(
        SqlState::DATETIME_FIELD_OVERFLOW,
        format!("date field value out of range: {year}-{mon:02}-{mday:02}"),
    )
}

fn date_range_ymd(year: i32, mon: i32, mday: i32) -> PgError {
    PgError::new(
        SqlState::DATETIME_VALUE_OUT_OF_RANGE,
        format!("date out of range: {year}-{mon:02}-{mday:02}"),
    )
}

/// Astronomical year/month/day → day number since 2000-01-01, or None if out of the valid range.
fn ymd_to_days(year: i32, mon: i32, mday: i32) -> Option<i32> {
    if !is_valid_julian(year, mon) {
        return None;
    }
    let days = calendar::date2j(year, mon, mday).checked_sub(POSTGRES_EPOCH_JDATE)?;
    if is_valid_date(days) { Some(days) } else { None }
}

/// Ordering of a date against a timestamp when either side is infinite (PG keeps infinities as
/// the extremes). `None` when both values are finite.
fn cmp_infinite(d: i32, ts: i64) -> Option<Ordering> {
    if d == DATE_INFINITY {
        Some(if ts == TIMESTAMP_INFINITY { Ordering::Equal } else { Ordering::Greater })
    } else if d == DATE_NEG_INFINITY {
        Some(if ts == TIMESTAMP_NEG_INFINITY { Ordering::Equal } else { Ordering::Less })
    } else if ts == TIMESTAMP_INFINITY {
        Some(Ordering::Less)
    } else if ts == TIMESTAMP_NEG_INFINITY {
        Some(Ordering::Greater)
    } else {
        None
    }
}

/// Date (days since 2000-01-01) → midnight timestamp (µs). Infinities map to timestamp infinities;
/// dates beyond the timestamp range → 22008 `date out of range for timestamp`.
// CONTRACT
#[allow(unused_variables)]
pub fn date_to_timestamp(d: i32) -> Result<i64> {
    if d == DATE_INFINITY {
        return Ok(TIMESTAMP_INFINITY);
    }
    if d == DATE_NEG_INFINITY {
        return Ok(TIMESTAMP_NEG_INFINITY);
    }
    let v = midnight_wide(d);
    if !ts_in_range(v) {
        return Err(ts_overflow_error());
    }
    Ok(v as i64)
}

/// Date → timestamptz: midnight in the session zone (PG `date2timestamptz`).
fn date_to_timestamptz(d: i32, setting: &TimeZoneSetting) -> Result<i64> {
    if d == DATE_INFINITY {
        return Ok(TIMESTAMP_INFINITY);
    }
    if d == DATE_NEG_INFINITY {
        return Ok(TIMESTAMP_NEG_INFINITY);
    }
    let local = midnight_wide(d);
    if !ts_in_range(local) {
        return Err(ts_overflow_error());
    }
    // PG `DetermineTimeZoneOffset` returns seconds WEST; `offset_at_local` returns seconds EAST.
    let east = tz::offset_at_local(setting, local as i64)?;
    let utc = local - east as i128 * USECS_PER_SEC as i128;
    if !ts_in_range(utc) {
        return Err(ts_overflow_error());
    }
    Ok(utc as i64)
}

/// Ordering of a date against a timestamp (PG `date_cmp_timestamp_internal`). A finite date past the
/// timestamp range compares above every finite timestamp and below +infinity.
fn cmp_date_ts(d: i32, ts: i64) -> Ordering {
    cmp_infinite(d, ts).unwrap_or_else(|| midnight_wide(d).cmp(&(ts as i128)))
}

/// Ordering of a date against a timestamptz (PG `date_cmp_timestamptz_internal`).
fn cmp_date_tstz(d: i32, ts: i64, setting: &TimeZoneSetting) -> Result<Ordering> {
    if let Some(o) = cmp_infinite(d, ts) {
        return Ok(o);
    }
    let local = midnight_wide(d);
    if !ts_in_range(local) {
        // Past the timestamp range: the sign of the overflow decides, as in PG's sentinels.
        return Ok(if local > 0 { Ordering::Greater } else { Ordering::Less });
    }
    let east = tz::offset_at_local(setting, local as i64)?;
    let utc = local - east as i128 * USECS_PER_SEC as i128;
    Ok(utc.cmp(&(ts as i128)))
}

/// Interval negation for `timestamp - interval` (PG `interval_um` semantics).
fn negate_interval(iv: &Interval) -> Result<Interval> {
    if *iv == Interval::INFINITY {
        return Ok(Interval::NEG_INFINITY);
    }
    if *iv == Interval::NEG_INFINITY {
        return Ok(Interval::INFINITY);
    }
    match (iv.months.checked_neg(), iv.days.checked_neg(), iv.micros.checked_neg()) {
        (Some(m), Some(d), Some(u)) => Ok(Interval::new(m, d, u)),
        _ => Err(PgError::new(SqlState::DATETIME_VALUE_OUT_OF_RANGE, "interval out of range")),
    }
}

/// `ts + span` for a timestamp without zone; an infinite timestamp is returned unchanged.
fn add_interval_ts(ts: i64, span: &Interval) -> Result<i64> {
    if ts == TIMESTAMP_INFINITY || ts == TIMESTAMP_NEG_INFINITY {
        return Ok(ts);
    }
    timestamp_add_interval(ts, span)
}

/// `ts - span` for a timestamp without zone; an infinite timestamp is returned unchanged.
fn sub_interval_ts(ts: i64, span: &Interval) -> Result<i64> {
    if ts == TIMESTAMP_INFINITY || ts == TIMESTAMP_NEG_INFINITY {
        return Ok(ts);
    }
    let neg = negate_interval(span)?;
    timestamp_add_interval(ts, &neg)
}

/// `date_in`.
#[allow(unused_variables)]
pub fn input(ctx: &IoCtx, ty: TypeId, s: &str) -> Result<Datum> {
    let dec = decode::parse_date_time(s, ctx, "date")?;
    let days = match dec.kind {
        DateTimeKind::Late => DATE_INFINITY,
        DateTimeKind::Early => DATE_NEG_INFINITY,
        DateTimeKind::Epoch => UNIX_EPOCH_JDATE - POSTGRES_EPOCH_JDATE,
        DateTimeKind::Now => {
            let now = ctx.now.ok_or_else(|| {
                PgError::unimplemented("TYP-26", "date 'now' without a transaction timestamp")
            })?;
            let east = tz::offset_at_utc(&ctx.config.time_zone, now)?;
            let local = now as i128 + east as i128 * USECS_PER_SEC as i128;
            let days = local.div_euclid(USECS_PER_DAY as i128);
            if days < DATE_MIN_DAYS as i128 || days >= DATE_END_DAYS as i128 {
                return Err(date_out_of_range_input(s));
            }
            days as i32
        }
        DateTimeKind::Date => match ymd_to_days(dec.tm.year, dec.tm.mon, dec.tm.mday) {
            Some(d) => d,
            None => return Err(date_out_of_range_input(s)),
        },
    };
    Ok(Datum::Date(days))
}

/// `date_out`.
#[allow(unused_variables)]
pub fn output(ctx: &IoCtx, ty: TypeId, d: &Datum) -> Result<String> {
    let days = match d {
        Datum::Date(v) => *v,
        other => return Err(type_mismatch("Date", other)),
    };
    if days == DATE_INFINITY {
        return Ok("infinity".to_string());
    }
    if days == DATE_NEG_INFINITY {
        return Ok("-infinity".to_string());
    }
    if !is_valid_date(days) {
        return Err(PgError::internal(format!("date value {days} outside the valid range")));
    }
    let jd = days + POSTGRES_EPOCH_JDATE;
    let (year, mon, mday) = calendar::j2date(jd);
    let tm = Tm { year, mon, mday, wday: calendar::j2day(jd), ..Tm::default() };
    encode::encode_date_only(&tm, ctx.config.date_style, ctx.config.date_order)
}

/// `date_recv`.
#[allow(unused_variables)]
pub fn recv(ctx: &IoCtx, ty: TypeId, buf: &[u8]) -> Result<Datum> {
    let bytes: [u8; 4] = match <[u8; 4]>::try_from(buf) {
        Ok(b) => b,
        Err(_) => return Err(invalid_binary("")),
    };
    let days = i32::from_be_bytes(bytes);
    if !is_not_finite(days) && !is_valid_date(days) {
        return Err(PgError::new(SqlState::DATETIME_VALUE_OUT_OF_RANGE, "date out of range"));
    }
    Ok(Datum::Date(days))
}

/// `date_send`.
#[allow(unused_variables)]
pub fn send(ctx: &IoCtx, ty: TypeId, d: &Datum) -> Result<Vec<u8>> {
    match d {
        Datum::Date(v) => Ok(v.to_be_bytes().to_vec()),
        other => Err(type_mismatch("Date", other)),
    }
}

/// Function registry hook (see `crate::func`).
pub(crate) fn lookup(proc: &BuiltinProc) -> Option<FuncImpl> {
    let f: ScalarFn = match proc.oid {
        // SQL-language builtin (empty prosrc): integer_pl_date(int4, date).
        2550 => integer_pl_date,
        _ => match proc.prosrc {
            "date_eq" => date_eq,
            "date_ne" => date_ne,
            "date_lt" => date_lt,
            "date_le" => date_le,
            "date_gt" => date_gt,
            "date_ge" => date_ge,
            "date_cmp" => date_cmp,
            "date_larger" => date_larger,
            "date_smaller" => date_smaller,
            "date_mi" => date_mi,
            "date_pli" => date_pli,
            "date_mii" => date_mii,
            "date_pl_interval" => date_pl_interval,
            "date_mi_interval" => date_mi_interval,
            "date_finite" => date_finite,
            "date_timestamp" => date_timestamp,
            "date_timestamptz" => date_timestamptz,
            "datetime_timestamp" => datetime_timestamp,
            "datetimetz_timestamptz" => datetimetz_timestamptz,
            "make_date" => make_date,
            "in_range_date_interval" => in_range_date_interval,
            "hashdate" => hashdate,
            "hashdateextended" => hashdateextended,
            "date_eq_timestamp" => date_eq_timestamp,
            "date_ne_timestamp" => date_ne_timestamp,
            "date_lt_timestamp" => date_lt_timestamp,
            "date_le_timestamp" => date_le_timestamp,
            "date_gt_timestamp" => date_gt_timestamp,
            "date_ge_timestamp" => date_ge_timestamp,
            "date_cmp_timestamp" => date_cmp_timestamp,
            "date_eq_timestamptz" => date_eq_timestamptz,
            "date_ne_timestamptz" => date_ne_timestamptz,
            "date_lt_timestamptz" => date_lt_timestamptz,
            "date_le_timestamptz" => date_le_timestamptz,
            "date_gt_timestamptz" => date_gt_timestamptz,
            "date_ge_timestamptz" => date_ge_timestamptz,
            "date_cmp_timestamptz" => date_cmp_timestamptz,
            _ => return None,
        },
    };
    Some(FuncImpl::Scalar(f))
}

/// 0A000 for a Date-module proc without implementation.
pub(crate) fn unsupported(proc: &BuiltinProc) -> PgError {
    PgError::unimplemented("TYP-26", format!("function {}", proc.name))
}

// ---- argument access -------------------------------------------------------------------------

fn arg(args: &[Datum], i: usize) -> Result<&Datum> {
    args.get(i).ok_or_else(|| PgError::internal(format!("date function: missing argument {i}")))
}

fn arg_date(args: &[Datum], i: usize) -> Result<i32> {
    match arg(args, i)? {
        Datum::Date(v) => Ok(*v),
        other => Err(type_mismatch("Date", other)),
    }
}

fn arg_i32(args: &[Datum], i: usize) -> Result<i32> {
    match arg(args, i)? {
        Datum::Int4(v) => Ok(*v),
        other => Err(type_mismatch("Int4", other)),
    }
}

fn arg_i64(args: &[Datum], i: usize) -> Result<i64> {
    match arg(args, i)? {
        Datum::Int8(v) => Ok(*v),
        other => Err(type_mismatch("Int8", other)),
    }
}

fn arg_bool(args: &[Datum], i: usize) -> Result<bool> {
    match arg(args, i)? {
        Datum::Bool(v) => Ok(*v),
        other => Err(type_mismatch("Bool", other)),
    }
}

fn arg_timestamp(args: &[Datum], i: usize) -> Result<i64> {
    match arg(args, i)? {
        Datum::Timestamp(v) => Ok(*v),
        other => Err(type_mismatch("Timestamp", other)),
    }
}

fn arg_timestamptz(args: &[Datum], i: usize) -> Result<i64> {
    match arg(args, i)? {
        Datum::TimestampTz(v) => Ok(*v),
        other => Err(type_mismatch("TimestampTz", other)),
    }
}

fn arg_time(args: &[Datum], i: usize) -> Result<i64> {
    match arg(args, i)? {
        Datum::Time(v) => Ok(*v),
        other => Err(type_mismatch("Time", other)),
    }
}

fn arg_timetz(args: &[Datum], i: usize) -> Result<(i64, i32)> {
    match arg(args, i)? {
        Datum::TimeTz(t, zone) => Ok((*t, *zone)),
        other => Err(type_mismatch("TimeTz", other)),
    }
}

fn arg_interval(args: &[Datum], i: usize) -> Result<Interval> {
    match arg(args, i)? {
        Datum::Interval(v) => Ok(*v),
        other => Err(type_mismatch("Interval", other)),
    }
}

// ---- comparisons -----------------------------------------------------------------------------

fn cmp_to_i32(o: Ordering) -> i32 {
    match o {
        Ordering::Less => -1,
        Ordering::Equal => 0,
        Ordering::Greater => 1,
    }
}

fn date_pair_ord(args: &[Datum]) -> Result<Ordering> {
    Ok(arg_date(args, 0)?.cmp(&arg_date(args, 1)?))
}

fn bool_of_date_cmp(args: &[Datum], want: fn(Ordering) -> bool) -> Result<Datum> {
    Ok(Datum::Bool(want(date_pair_ord(args)?)))
}

fn date_eq(args: &[Datum], _ctx: &FuncCtx) -> Result<Datum> {
    bool_of_date_cmp(args, Ordering::is_eq)
}

fn date_ne(args: &[Datum], _ctx: &FuncCtx) -> Result<Datum> {
    bool_of_date_cmp(args, Ordering::is_ne)
}

fn date_lt(args: &[Datum], _ctx: &FuncCtx) -> Result<Datum> {
    bool_of_date_cmp(args, Ordering::is_lt)
}

fn date_le(args: &[Datum], _ctx: &FuncCtx) -> Result<Datum> {
    bool_of_date_cmp(args, Ordering::is_le)
}

fn date_gt(args: &[Datum], _ctx: &FuncCtx) -> Result<Datum> {
    bool_of_date_cmp(args, Ordering::is_gt)
}

fn date_ge(args: &[Datum], _ctx: &FuncCtx) -> Result<Datum> {
    bool_of_date_cmp(args, Ordering::is_ge)
}

fn date_cmp(args: &[Datum], _ctx: &FuncCtx) -> Result<Datum> {
    Ok(Datum::Int4(cmp_to_i32(date_pair_ord(args)?)))
}

fn date_larger(args: &[Datum], _ctx: &FuncCtx) -> Result<Datum> {
    let a = arg_date(args, 0)?;
    let b = arg_date(args, 1)?;
    Ok(Datum::Date(if a > b { a } else { b }))
}

fn date_smaller(args: &[Datum], _ctx: &FuncCtx) -> Result<Datum> {
    let a = arg_date(args, 0)?;
    let b = arg_date(args, 1)?;
    Ok(Datum::Date(if a < b { a } else { b }))
}

fn date_finite(args: &[Datum], _ctx: &FuncCtx) -> Result<Datum> {
    Ok(Datum::Bool(!is_not_finite(arg_date(args, 0)?)))
}

fn hashdate(args: &[Datum], _ctx: &FuncCtx) -> Result<Datum> {
    let d = arg_date(args, 0)?;
    Ok(Datum::Int4(crate::compare::pg_hash_uint32(d as u32)? as i32))
}

fn hashdateextended(args: &[Datum], _ctx: &FuncCtx) -> Result<Datum> {
    let d = arg_date(args, 0)?;
    let seed = arg_i64(args, 1)?;
    Ok(Datum::Int8(crate::compare::pg_hash_uint32_extended(d as u32, seed as u64)? as i64))
}

// date vs timestamp

fn date_ts_ord(args: &[Datum]) -> Result<Ordering> {
    Ok(cmp_date_ts(arg_date(args, 0)?, arg_timestamp(args, 1)?))
}

fn date_eq_timestamp(args: &[Datum], _ctx: &FuncCtx) -> Result<Datum> {
    Ok(Datum::Bool(date_ts_ord(args)?.is_eq()))
}

fn date_ne_timestamp(args: &[Datum], _ctx: &FuncCtx) -> Result<Datum> {
    Ok(Datum::Bool(date_ts_ord(args)?.is_ne()))
}

fn date_lt_timestamp(args: &[Datum], _ctx: &FuncCtx) -> Result<Datum> {
    Ok(Datum::Bool(date_ts_ord(args)?.is_lt()))
}

fn date_le_timestamp(args: &[Datum], _ctx: &FuncCtx) -> Result<Datum> {
    Ok(Datum::Bool(date_ts_ord(args)?.is_le()))
}

fn date_gt_timestamp(args: &[Datum], _ctx: &FuncCtx) -> Result<Datum> {
    Ok(Datum::Bool(date_ts_ord(args)?.is_gt()))
}

fn date_ge_timestamp(args: &[Datum], _ctx: &FuncCtx) -> Result<Datum> {
    Ok(Datum::Bool(date_ts_ord(args)?.is_ge()))
}

fn date_cmp_timestamp(args: &[Datum], _ctx: &FuncCtx) -> Result<Datum> {
    Ok(Datum::Int4(cmp_to_i32(date_ts_ord(args)?)))
}

// date vs timestamptz (midnight in the session zone)

fn date_tstz_ord(args: &[Datum], ctx: &FuncCtx) -> Result<Ordering> {
    cmp_date_tstz(arg_date(args, 0)?, arg_timestamptz(args, 1)?, &ctx.io.time_zone)
}

fn date_eq_timestamptz(args: &[Datum], ctx: &FuncCtx) -> Result<Datum> {
    Ok(Datum::Bool(date_tstz_ord(args, ctx)?.is_eq()))
}

fn date_ne_timestamptz(args: &[Datum], ctx: &FuncCtx) -> Result<Datum> {
    Ok(Datum::Bool(date_tstz_ord(args, ctx)?.is_ne()))
}

fn date_lt_timestamptz(args: &[Datum], ctx: &FuncCtx) -> Result<Datum> {
    Ok(Datum::Bool(date_tstz_ord(args, ctx)?.is_lt()))
}

fn date_le_timestamptz(args: &[Datum], ctx: &FuncCtx) -> Result<Datum> {
    Ok(Datum::Bool(date_tstz_ord(args, ctx)?.is_le()))
}

fn date_gt_timestamptz(args: &[Datum], ctx: &FuncCtx) -> Result<Datum> {
    Ok(Datum::Bool(date_tstz_ord(args, ctx)?.is_gt()))
}

fn date_ge_timestamptz(args: &[Datum], ctx: &FuncCtx) -> Result<Datum> {
    Ok(Datum::Bool(date_tstz_ord(args, ctx)?.is_ge()))
}

fn date_cmp_timestamptz(args: &[Datum], ctx: &FuncCtx) -> Result<Datum> {
    Ok(Datum::Int4(cmp_to_i32(date_tstz_ord(args, ctx)?)))
}

// ---- arithmetic ------------------------------------------------------------------------------

fn date_add_days(d: i32, days: i32) -> Result<i32> {
    if is_not_finite(d) {
        return Ok(d);
    }
    let r = d.checked_add(days).ok_or_else(|| date_out_of_range_plain())?;
    if !is_valid_date(r) {
        return Err(date_out_of_range_plain());
    }
    Ok(r)
}

fn date_sub_days(d: i32, days: i32) -> Result<i32> {
    if is_not_finite(d) {
        return Ok(d);
    }
    let r = d.checked_sub(days).ok_or_else(|| date_out_of_range_plain())?;
    if !is_valid_date(r) {
        return Err(date_out_of_range_plain());
    }
    Ok(r)
}

fn date_out_of_range_plain() -> PgError {
    PgError::new(SqlState::DATETIME_VALUE_OUT_OF_RANGE, "date out of range")
}

fn date_pli(args: &[Datum], _ctx: &FuncCtx) -> Result<Datum> {
    Ok(Datum::Date(date_add_days(arg_date(args, 0)?, arg_i32(args, 1)?)?))
}

fn date_mii(args: &[Datum], _ctx: &FuncCtx) -> Result<Datum> {
    Ok(Datum::Date(date_sub_days(arg_date(args, 0)?, arg_i32(args, 1)?)?))
}

fn integer_pl_date(args: &[Datum], _ctx: &FuncCtx) -> Result<Datum> {
    Ok(Datum::Date(date_add_days(arg_date(args, 1)?, arg_i32(args, 0)?)?))
}

fn date_mi(args: &[Datum], _ctx: &FuncCtx) -> Result<Datum> {
    let a = arg_date(args, 0)?;
    let b = arg_date(args, 1)?;
    if is_not_finite(a) || is_not_finite(b) {
        return Err(PgError::new(SqlState::DATETIME_VALUE_OUT_OF_RANGE, "cannot subtract infinite dates"));
    }
    let diff = a as i64 - b as i64;
    match i32::try_from(diff) {
        Ok(v) => Ok(Datum::Int4(v)),
        Err(_) => Err(date_out_of_range_plain()),
    }
}

fn date_pl_interval(args: &[Datum], _ctx: &FuncCtx) -> Result<Datum> {
    let ts = date_to_timestamp(arg_date(args, 0)?)?;
    let span = arg_interval(args, 1)?;
    Ok(Datum::Timestamp(add_interval_ts(ts, &span)?))
}

fn date_mi_interval(args: &[Datum], _ctx: &FuncCtx) -> Result<Datum> {
    let ts = date_to_timestamp(arg_date(args, 0)?)?;
    let span = arg_interval(args, 1)?;
    Ok(Datum::Timestamp(sub_interval_ts(ts, &span)?))
}

// ---- casts and constructors ------------------------------------------------------------------

fn date_timestamp(args: &[Datum], _ctx: &FuncCtx) -> Result<Datum> {
    Ok(Datum::Timestamp(date_to_timestamp(arg_date(args, 0)?)?))
}

fn date_timestamptz(args: &[Datum], ctx: &FuncCtx) -> Result<Datum> {
    let d = arg_date(args, 0)?;
    Ok(Datum::TimestampTz(date_to_timestamptz(d, &ctx.io.time_zone)?))
}

fn datetime_timestamp(args: &[Datum], _ctx: &FuncCtx) -> Result<Datum> {
    let ts = date_to_timestamp(arg_date(args, 0)?)?;
    let t = arg_time(args, 1)?;
    if ts == TIMESTAMP_INFINITY || ts == TIMESTAMP_NEG_INFINITY {
        return Ok(Datum::Timestamp(ts));
    }
    let r = ts as i128 + t as i128;
    if !ts_in_range(r) {
        return Err(PgError::new(SqlState::DATETIME_VALUE_OUT_OF_RANGE, "timestamp out of range"));
    }
    Ok(Datum::Timestamp(r as i64))
}

fn datetimetz_timestamptz(args: &[Datum], _ctx: &FuncCtx) -> Result<Datum> {
    let d = arg_date(args, 0)?;
    let (t, zone) = arg_timetz(args, 1)?;
    if is_not_finite(d) {
        return Ok(Datum::TimestampTz(date_to_timestamp(d)?));
    }
    let local = midnight_wide(d);
    if !ts_in_range(local) {
        return Err(ts_overflow_error());
    }
    // `zone` is seconds WEST of UTC, so UTC = local + t + zone.
    let r = local + t as i128 + zone as i128 * USECS_PER_SEC as i128;
    if !ts_in_range(r) {
        return Err(ts_overflow_error());
    }
    Ok(Datum::TimestampTz(r as i64))
}

/// `make_date(year, month, day)` (PG `make_date`, with `ValidateDate` for DTK_DATE_M).
fn make_date(args: &[Datum], _ctx: &FuncCtx) -> Result<Datum> {
    let year_in = arg_i32(args, 0)?;
    let mon = arg_i32(args, 1)?;
    let mday = arg_i32(args, 2)?;

    // Negative years are BC; the magnitude is stored positive until ValidateDate.
    let bc = year_in < 0;
    let year = if bc {
        match year_in.checked_neg() {
            Some(v) => v,
            None => return Err(field_overflow_ymd(year_in, mon, mday)),
        }
    } else {
        year_in
    };
    // No year zero in AD/BC notation.
    if year <= 0 {
        return Err(field_overflow_ymd(year, mon, mday));
    }
    // Internally 1 BC is year 0, 2 BC is -1, ...
    let astro = if bc { -(year - 1) } else { year };

    if !(1..=12).contains(&mon) {
        return Err(field_overflow_ymd(astro, mon, mday));
    }
    if mday < 1 || mday > calendar::days_in_month(astro, mon) {
        return Err(field_overflow_ymd(astro, mon, mday));
    }
    if !is_valid_julian(astro, mon) {
        return Err(date_range_ymd(astro, mon, mday));
    }
    match ymd_to_days(astro, mon, mday) {
        Some(d) => Ok(Datum::Date(d)),
        None => Err(date_range_ymd(astro, mon, mday)),
    }
}

/// `in_range(date, date, interval, bool, bool)` for window RANGE frames (PG delegates to
/// `in_range_timestamp_interval` on the midnight timestamps).
fn in_range_date_interval(args: &[Datum], _ctx: &FuncCtx) -> Result<Datum> {
    let val = date_to_timestamp(arg_date(args, 0)?)?;
    let base = date_to_timestamp(arg_date(args, 1)?)?;
    let offset = arg_interval(args, 2)?;
    let sub = arg_bool(args, 3)?;
    let less = arg_bool(args, 4)?;

    if offset.cmp_span() < 0 {
        return Err(PgError::new(
            SqlState(*b"22013"),
            "invalid preceding or following size in window function",
        ));
    }
    // Both base and offset infinite: every value is within range (PG in_range_timestamp_interval).
    if offset == Interval::INFINITY
        && (if sub { base == TIMESTAMP_INFINITY } else { base == TIMESTAMP_NEG_INFINITY })
    {
        return Ok(Datum::Bool(true));
    }
    let sum = if sub { sub_interval_ts(base, &offset)? } else { add_interval_ts(base, &offset)? };
    Ok(Datum::Bool(if less { val <= sum } else { val >= sum }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::context::{IoConfig, TestSession};
    use crate::registry;

    fn parse(s: &str) -> Datum {
        let env = registry::builtin_env();
        let cfg = IoConfig::default();
        let ctx = IoCtx::new(env, &cfg);
        input(&ctx, TypeId::DATE, s).expect("date input")
    }

    fn render(d: &Datum) -> String {
        let env = registry::builtin_env();
        let cfg = IoConfig::default();
        let ctx = IoCtx::new(env, &cfg);
        output(&ctx, TypeId::DATE, d).expect("date output")
    }

    fn call(f: ScalarFn, args: &[Datum]) -> Result<Datum> {
        let env = registry::builtin_env();
        let cfg = IoConfig::default();
        let sess = TestSession::default();
        let ctx = FuncCtx {
            env,
            io: &cfg,
            session: &sess,
            arg_types: &[],
            result_type: TypeId::DATE,
            collation: 0,
        };
        f(args, &ctx)
    }

    fn err_parts(r: Result<Datum>) -> (String, String) {
        let e = r.expect_err("expected an error");
        (e.sqlstate.code().to_string(), e.message)
    }

    #[test]
    fn leap_day_round_trip() {
        let d = parse("2024-02-29");
        assert_eq!(render(&d), "2024-02-29");
    }

    #[test]
    fn out_of_range_input() {
        let env = registry::builtin_env();
        let cfg = IoConfig::default();
        let ctx = IoCtx::new(env, &cfg);
        let e = input(&ctx, TypeId::DATE, "5874898-01-01").expect_err("out of range");
        assert_eq!(e.sqlstate.code(), "22008");
        assert_eq!(e.message, "date out of range: \"5874898-01-01\"");
    }

    #[test]
    fn infinity_specials() {
        assert_eq!(parse("infinity"), Datum::Date(DATE_INFINITY));
        assert_eq!(parse("-infinity"), Datum::Date(DATE_NEG_INFINITY));
        assert_eq!(render(&Datum::Date(DATE_INFINITY)), "infinity");
        assert_eq!(render(&Datum::Date(DATE_NEG_INFINITY)), "-infinity");
    }

    #[test]
    fn plus_one_day_crosses_leap_day() {
        let r = call(date_pli, &[parse("2024-02-29"), Datum::Int4(1)]).expect("date + 1");
        assert_eq!(r, parse("2024-03-01"));
    }

    #[test]
    fn subtract_dates_gives_days() {
        let r = call(date_mi, &[parse("2024-03-01"), parse("2024-02-01")]).expect("date - date");
        assert_eq!(r, Datum::Int4(29));
    }

    #[test]
    fn subtract_infinite_dates_fails() {
        let (code, msg) = err_parts(call(date_mi, &[Datum::Date(DATE_INFINITY), parse("2024-02-01")]));
        assert_eq!(code, "22008");
        assert_eq!(msg, "cannot subtract infinite dates");
    }

    #[test]
    fn infinity_plus_days_stays_infinity() {
        let r = call(date_pli, &[Datum::Date(DATE_INFINITY), Datum::Int4(1)]).expect("infinity + 1");
        assert_eq!(r, Datum::Date(DATE_INFINITY));
    }

    #[test]
    fn make_date_rejects_feb_30() {
        let (code, msg) = err_parts(call(
            make_date,
            &[Datum::Int4(2024), Datum::Int4(2), Datum::Int4(30)],
        ));
        assert_eq!(code, "22008");
        assert_eq!(msg, "date field value out of range: 2024-02-30");
    }

    #[test]
    fn make_date_rejects_year_zero() {
        let (code, msg) = err_parts(call(
            make_date,
            &[Datum::Int4(0), Datum::Int4(1), Datum::Int4(1)],
        ));
        assert_eq!(code, "22008");
        assert_eq!(msg, "date field value out of range: 0-01-01");
    }

    #[test]
    fn make_date_bc_year_is_astronomical() {
        // 44 BC March 15 is astronomical year -43.
        let r = call(make_date, &[Datum::Int4(-44), Datum::Int4(3), Datum::Int4(15)]).expect("44 BC");
        assert_eq!(render(&r), "0044-03-15 BC");
    }

    #[test]
    fn send_epoch_offset_one_day() {
        let env = registry::builtin_env();
        let cfg = IoConfig::default();
        let ctx = IoCtx::new(env, &cfg);
        let bytes = send(&ctx, TypeId::DATE, &parse("2000-01-02")).expect("send");
        assert_eq!(bytes, vec![0, 0, 0, 1]);
    }

    #[test]
    fn recv_rejects_wrong_length() {
        let env = registry::builtin_env();
        let cfg = IoConfig::default();
        let ctx = IoCtx::new(env, &cfg);
        let e = recv(&ctx, TypeId::DATE, &[0, 0, 1]).expect_err("short buffer");
        assert_eq!(e.sqlstate.code(), "22P03");
    }

    #[test]
    fn infinite_date_equals_infinite_timestamp() {
        let r = call(
            date_eq_timestamp,
            &[Datum::Date(DATE_INFINITY), Datum::Timestamp(TIMESTAMP_INFINITY)],
        )
        .expect("compare");
        assert_eq!(r, Datum::Bool(true));
    }

    #[test]
    fn every_date_module_proc_is_dispatched() {
        for p in crate::catalog_data::PROCS.iter() {
            if matches!(p.module, ImplModule::Date) {
                assert!(lookup(p).is_some(), "no implementation for {}", p.name);
            }
        }
    }
}
