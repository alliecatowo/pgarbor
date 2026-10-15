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
use crate::compare;
use crate::context::{FuncCtx, IoConfig, IoCtx, TimeZoneSetting};
use crate::datetime::calendar::{date2j, days_in_month, j2date, timestamp2tm, tm2timestamp};
use crate::datetime::encode;
use crate::datetime::tz;
use crate::datetime::{
    DateTimeKind, END_TIMESTAMP, MAX_TIMESTAMP_PRECISION, MIN_TIMESTAMP, POSTGRES_EPOCH_JDATE,
    TIMESTAMP_END_JULIAN, UNIX_EPOCH_JDATE, USECS_PER_DAY, USECS_PER_SEC,
};
use crate::datum::{
    type_mismatch, Datum, Interval, TypeId, DATE_INFINITY, DATE_NEG_INFINITY, TIMESTAMP_INFINITY,
    TIMESTAMP_NEG_INFINITY,
};
use crate::func::{FuncImpl, ScalarFn};
use crate::io::invalid_binary;
use crate::oids;
use pgarbor_common::{PgError, Result, SqlState};

/// `1970-01-01 00:00:00` on the 2000-epoch µs axis (`SetEpochTimestamp`).
const EPOCH_TS: i64 = (UNIX_EPOCH_JDATE as i64 - POSTGRES_EPOCH_JDATE as i64) * USECS_PER_DAY;
/// Dates at or beyond this many days after 2000-01-01 do not fit a timestamp (PG `date2timestamp`).
const DATE_OVERFLOW_DAYS: i32 = TIMESTAMP_END_JULIAN - POSTGRES_EPOCH_JDATE;

// ---------------------------------------------------------------------------------------------
// Small helpers
// ---------------------------------------------------------------------------------------------

#[inline]
fn is_finite_ts(ts: i64) -> bool {
    ts != TIMESTAMP_INFINITY && ts != TIMESTAMP_NEG_INFINITY
}

#[inline]
fn is_valid_ts(ts: i64) -> bool {
    (MIN_TIMESTAMP..END_TIMESTAMP).contains(&ts)
}

fn out_of_range_ts() -> PgError {
    PgError::new(SqlState::DATETIME_VALUE_OUT_OF_RANGE, "timestamp out of range")
}

fn out_of_range_ts_input(s: &str) -> PgError {
    PgError::new(SqlState::DATETIME_VALUE_OUT_OF_RANGE, format!("timestamp out of range: \"{s}\""))
}

fn date_out_of_range_for_ts() -> PgError {
    PgError::new(SqlState::DATETIME_VALUE_OUT_OF_RANGE, "date out of range for timestamp")
}

fn interval_out_of_range() -> PgError {
    PgError::new(SqlState::DATETIME_VALUE_OUT_OF_RANGE, "interval out of range")
}

/// Error for a finite timestamp outside the valid range.
fn ensure_valid(ts: i64) -> Result<i64> {
    if is_valid_ts(ts) { Ok(ts) } else { Err(out_of_range_ts()) }
}

/// Re-raise a 22008 from a calendar routine with the input string, as PG's input routines do.
fn remap_range(e: PgError, s: &str) -> PgError {
    if e.sqlstate == SqlState::DATETIME_VALUE_OUT_OF_RANGE { out_of_range_ts_input(s) } else { e }
}

fn cmp_i64(a: i64, b: i64) -> i32 {
    a.cmp(&b) as i32
}

fn arg(args: &[Datum], i: usize) -> Result<&Datum> {
    args.get(i).ok_or_else(|| PgError::internal(format!("missing function argument {i}")))
}

fn ts_arg(d: &Datum) -> Result<i64> {
    match d {
        Datum::Timestamp(v) | Datum::TimestampTz(v) => Ok(*v),
        other => Err(type_mismatch("timestamp", other)),
    }
}

fn date_arg(d: &Datum) -> Result<i32> {
    match d {
        Datum::Date(v) => Ok(*v),
        other => Err(type_mismatch("date", other)),
    }
}

fn time_arg(d: &Datum) -> Result<i64> {
    match d {
        Datum::Time(v) => Ok(*v),
        other => Err(type_mismatch("time", other)),
    }
}

fn timetz_arg(d: &Datum) -> Result<(i64, i32)> {
    match d {
        Datum::TimeTz(t, z) => Ok((*t, *z)),
        other => Err(type_mismatch("timetz", other)),
    }
}

fn iv_arg(d: &Datum) -> Result<Interval> {
    match d {
        Datum::Interval(iv) => Ok(*iv),
        other => Err(type_mismatch("interval", other)),
    }
}

fn bool_arg(d: &Datum) -> Result<bool> {
    match d {
        Datum::Bool(b) => Ok(*b),
        other => Err(type_mismatch("boolean", other)),
    }
}

fn int8_arg(d: &Datum) -> Result<i64> {
    match d {
        Datum::Int8(v) => Ok(*v),
        other => Err(type_mismatch("bigint", other)),
    }
}

fn tz_flag(ty: TypeId) -> Result<bool> {
    match ty.oid {
        oids::TIMESTAMP => Ok(false),
        oids::TIMESTAMPTZ => Ok(true),
        other => Err(PgError::internal(format!("timestamp routine called for type oid {other}"))),
    }
}

fn make_datum(with_tz: bool, v: i64) -> Datum {
    if with_tz { Datum::TimestampTz(v) } else { Datum::Timestamp(v) }
}

/// Session-zone offset (seconds EAST) at a UTC instant, as i64 microsecond math helper.
fn east_at_utc(setting: &TimeZoneSetting, utc: i64) -> Result<i64> {
    Ok(i64::from(tz::offset_at_utc(setting, utc)?))
}

/// UTC → session-local wall clock. Infinities pass through.
fn utc_to_local(setting: &TimeZoneSetting, utc: i64) -> Result<i64> {
    if !is_finite_ts(utc) {
        return Ok(utc);
    }
    let east = east_at_utc(setting, utc)?;
    Ok(utc.saturating_add(east * USECS_PER_SEC))
}

/// Session-local wall clock → UTC with PG's overflow report (`timestamp2timestamptz_opt_overflow`):
/// the returned sign is `+1` / `-1` when the converted value falls above / below the valid range,
/// in which case the value is the matching infinity. Infinities pass through.
fn local_to_utc(setting: &TimeZoneSetting, local: i64) -> Result<(i64, i8)> {
    if !is_finite_ts(local) {
        return Ok((local, 0));
    }
    let east = i64::from(tz::offset_at_local(setting, local)?);
    let utc = local.saturating_sub(east * USECS_PER_SEC);
    if is_valid_ts(utc) {
        Ok((utc, 0))
    } else if utc < MIN_TIMESTAMP {
        Ok((TIMESTAMP_NEG_INFINITY, -1))
    } else {
        Ok((TIMESTAMP_INFINITY, 1))
    }
}

/// Strict local `timestamp` value of a date (PG `date2timestamp`): overflow → 22008.
fn date_to_ts_strict(d: i32) -> Result<i64> {
    if d == DATE_INFINITY {
        return Ok(TIMESTAMP_INFINITY);
    }
    if d == DATE_NEG_INFINITY {
        return Ok(TIMESTAMP_NEG_INFINITY);
    }
    if d >= DATE_OVERFLOW_DAYS {
        return Err(date_out_of_range_for_ts());
    }
    Ok(i64::from(d) * USECS_PER_DAY)
}

/// `date2timestamp_opt_overflow`: value plus overflow sign (+1 above the range, 0 otherwise).
fn date_to_ts_opt(d: i32) -> (i64, i8) {
    if d == DATE_INFINITY {
        (TIMESTAMP_INFINITY, 0)
    } else if d == DATE_NEG_INFINITY {
        (TIMESTAMP_NEG_INFINITY, 0)
    } else if d >= DATE_OVERFLOW_DAYS {
        (TIMESTAMP_INFINITY, 1)
    } else {
        (i64::from(d) * USECS_PER_DAY, 0)
    }
}

/// `date2timestamptz_opt_overflow` for the session zone.
fn date_to_tstz_opt(setting: &TimeZoneSetting, d: i32) -> Result<(i64, i8)> {
    let (local, ov) = date_to_ts_opt(d);
    if ov != 0 {
        return Ok((local, ov));
    }
    local_to_utc(setting, local)
}

/// PG `date + time` (`datetime_timestamp`): strict date conversion, then add the time of day.
fn date_plus_time(d: i32, t: i64) -> Result<i64> {
    let base = date_to_ts_strict(d)?;
    if !is_finite_ts(base) {
        return Ok(base);
    }
    let sum = base.checked_add(t).ok_or_else(out_of_range_ts)?;
    ensure_valid(sum)
}

/// Strict timestamptz → timestamp-with-zone conversion (`timestamp2timestamptz`).
fn local_to_utc_strict(setting: &TimeZoneSetting, local: i64) -> Result<i64> {
    let (utc, ov) = local_to_utc(setting, local)?;
    if ov != 0 {
        return Err(out_of_range_ts());
    }
    Ok(utc)
}

/// Compare a timestamptz-with-overflow value against a timestamptz (PG `timestamp_cmp_timestamptz_internal`).
fn cmp_tstz_ov(dt1: (i64, i8), dt2: i64) -> i32 {
    let (v, ov) = dt1;
    if ov > 0 {
        // dt1 is larger than any finite timestamp, but less than infinity.
        return if dt2 == TIMESTAMP_INFINITY { -1 } else { 1 };
    }
    if ov < 0 {
        // dt1 is less than any finite timestamp, but more than -infinity.
        return if dt2 == TIMESTAMP_NEG_INFINITY { 1 } else { -1 };
    }
    cmp_i64(v, dt2)
}

fn cmp_ts_tstz(setting: &TimeZoneSetting, ts: i64, tstz: i64) -> Result<i32> {
    let dt1 = local_to_utc(setting, ts)?;
    Ok(cmp_tstz_ov(dt1, tstz))
}

/// PG `date_cmp_timestamp_internal` (date first).
fn cmp_date_ts(d: i32, ts: i64) -> i32 {
    let (dt1, ov) = date_to_ts_opt(d);
    if ov > 0 {
        return if ts == TIMESTAMP_INFINITY { -1 } else { 1 };
    }
    cmp_i64(dt1, ts)
}

fn cmp_date_tstz(setting: &TimeZoneSetting, d: i32, tstz: i64) -> Result<i32> {
    let dt1 = date_to_tstz_opt(setting, d)?;
    Ok(cmp_tstz_ov(dt1, tstz))
}

/// Low/high halves folding used by PG `hashint8` / `hashint8extended`.
fn hash_fold(v: i64) -> u32 {
    let lo = v as u32;
    let hi = (v >> 32) as u32;
    let mix = if v >= 0 { hi } else { !hi };
    lo ^ mix
}

/// Pure timestamp validity for the time-of-day / date extractors.
fn tod_of_local(local: i64) -> i64 {
    local.rem_euclid(USECS_PER_DAY)
}

/// Julian-calendar split of a finite local timestamp into (year, month, day, time-of-day µs).
fn split_local(local: i64) -> Result<(i32, i32, i32, i64)> {
    let days = local.div_euclid(USECS_PER_DAY);
    let tod = local.rem_euclid(USECS_PER_DAY);
    let jd = days + i64::from(POSTGRES_EPOCH_JDATE);
    if jd < 0 || jd > i64::from(i32::MAX) {
        return Err(out_of_range_ts());
    }
    let (y, m, d) = j2date(jd as i32);
    Ok((y, m, d, tod))
}

fn join_local(year: i32, mon: i32, mday: i32, tod: i64) -> Result<i64> {
    let jd = i64::from(date2j(year, mon, mday));
    let days = jd - i64::from(POSTGRES_EPOCH_JDATE);
    let local = days
        .checked_mul(USECS_PER_DAY)
        .and_then(|v| v.checked_add(tod))
        .ok_or_else(out_of_range_ts)?;
    ensure_valid(local)
}

/// Add calendar months to a local timestamp: same day of month, clamped to the target month's length.
fn add_months_local(local: i64, months: i32) -> Result<i64> {
    let (year, mon, mday, tod) = split_local(local)?;
    let total = i64::from(year) * 12 + i64::from(mon - 1) + i64::from(months);
    let new_year = i32::try_from(total.div_euclid(12)).map_err(|_| out_of_range_ts())?;
    let new_mon = total.rem_euclid(12) as i32 + 1;
    let new_day = mday.min(days_in_month(new_year, new_mon));
    join_local(new_year, new_mon, new_day, tod)
}

/// `interval_justify_hours` restricted to a time-only span (days = whole 24h periods).
fn justify_hours(micros: i64) -> Result<Interval> {
    let whole = micros / USECS_PER_DAY;
    let mut time = micros - whole * USECS_PER_DAY;
    let mut day = i32::try_from(whole).map_err(|_| interval_out_of_range())?;
    if day > 0 && time < 0 {
        time += USECS_PER_DAY;
        day -= 1;
    } else if day < 0 && time > 0 {
        time -= USECS_PER_DAY;
        day += 1;
    }
    Ok(Interval::new(0, day, time))
}

/// PG `timestamp_mi`: `a - b` as an interval in days and time (never months).
fn timestamp_diff(a: i64, b: i64) -> Result<Interval> {
    if !is_finite_ts(a) || !is_finite_ts(b) {
        if a == TIMESTAMP_NEG_INFINITY {
            if b == TIMESTAMP_NEG_INFINITY {
                return Err(interval_out_of_range());
            }
            return Ok(Interval::NEG_INFINITY);
        }
        if a == TIMESTAMP_INFINITY {
            if b == TIMESTAMP_INFINITY {
                return Err(interval_out_of_range());
            }
            return Ok(Interval::INFINITY);
        }
        // `a` is finite here.
        if b == TIMESTAMP_NEG_INFINITY {
            return Ok(Interval::INFINITY);
        }
        return Ok(Interval::NEG_INFINITY);
    }
    let diff = a.checked_sub(b).ok_or_else(interval_out_of_range)?;
    justify_hours(diff)
}

/// `-iv` (PG `interval_um`): infinities flip, overflow → 22008.
fn negate_interval(iv: &Interval) -> Result<Interval> {
    if *iv == Interval::INFINITY {
        return Ok(Interval::NEG_INFINITY);
    }
    if *iv == Interval::NEG_INFINITY {
        return Ok(Interval::INFINITY);
    }
    let months = iv.months.checked_neg().ok_or_else(interval_out_of_range)?;
    let days = iv.days.checked_neg().ok_or_else(interval_out_of_range)?;
    let micros = iv.micros.checked_neg().ok_or_else(interval_out_of_range)?;
    Ok(Interval::new(months, days, micros))
}

/// Infinite-interval and infinite-timestamp rules shared by timestamp and timestamptz `+ interval`
/// (PG `timestamp_pl_interval` / `timestamptz_pl_interval_internal`). `None` = compute normally.
fn add_infinite_rules(ts: i64, iv: &Interval) -> Option<Result<i64>> {
    if *iv == Interval::NEG_INFINITY {
        return Some(if ts == TIMESTAMP_INFINITY { Err(out_of_range_ts()) } else { Ok(TIMESTAMP_NEG_INFINITY) });
    }
    if *iv == Interval::INFINITY {
        return Some(if ts == TIMESTAMP_NEG_INFINITY { Err(out_of_range_ts()) } else { Ok(TIMESTAMP_INFINITY) });
    }
    if !is_finite_ts(ts) {
        return Some(Ok(ts));
    }
    None
}

/// Finite timestamp + interval, without zone: months (clamped), then days, then µs.
fn add_interval_local(local: i64, iv: &Interval) -> Result<i64> {
    let mut ts = local;
    if iv.months != 0 {
        ts = add_months_local(ts, iv.months)?;
    }
    if iv.days != 0 {
        let delta = i64::from(iv.days).checked_mul(USECS_PER_DAY).ok_or_else(out_of_range_ts)?;
        ts = ts.checked_add(delta).ok_or_else(out_of_range_ts)?;
        ts = ensure_valid(ts)?;
    }
    let ts = ts.checked_add(iv.micros).ok_or_else(out_of_range_ts)?;
    ensure_valid(ts)
}

/// Finite timestamptz + interval in the session zone (PG `timestamptz_pl_interval_internal`).
fn add_interval_tz(setting: &TimeZoneSetting, utc: i64, iv: &Interval) -> Result<i64> {
    let mut t = utc;
    if iv.months != 0 {
        let local = utc_to_local(setting, t)?;
        let local2 = add_months_local(local, iv.months)?;
        let east2 = i64::from(tz::offset_at_local(setting, local2)?);
        t = ensure_valid(local2.saturating_sub(east2 * USECS_PER_SEC))?;
    }
    if iv.days != 0 {
        let local = utc_to_local(setting, t)?;
        let delta = i64::from(iv.days).checked_mul(USECS_PER_DAY).ok_or_else(out_of_range_ts)?;
        let local2 = ensure_valid(local.checked_add(delta).ok_or_else(out_of_range_ts)?)?;
        let east2 = i64::from(tz::offset_at_local(setting, local2)?);
        t = ensure_valid(local2.saturating_sub(east2 * USECS_PER_SEC))?;
    }
    let t = t.checked_add(iv.micros).ok_or_else(out_of_range_ts)?;
    ensure_valid(t)
}

fn tz_add_interval(setting: &TimeZoneSetting, ts: i64, iv: &Interval) -> Result<i64> {
    if let Some(r) = add_infinite_rules(ts, iv) {
        return r;
    }
    add_interval_tz(setting, ts, iv)
}

/// Shared body of `in_range_timestamp(tz)_interval` (PG `in_range_timestamp_interval`).
fn in_range_impl(
    setting: &TimeZoneSetting,
    val: i64,
    base: i64,
    offset: &Interval,
    sub: bool,
    less: bool,
    with_tz: bool,
) -> Result<bool> {
    if offset.cmp_span() < 0 {
        return Err(PgError::new(
            SqlState(*b"22013"),
            "invalid preceding or following size in window function",
        ));
    }
    let base_is_open = if sub { base == TIMESTAMP_INFINITY } else { base == TIMESTAMP_NEG_INFINITY };
    if *offset == Interval::INFINITY && base_is_open {
        return Ok(true);
    }
    let sum = match (with_tz, sub) {
        (false, false) => timestamp_add_interval(base, offset)?,
        (false, true) => timestamp_add_interval(base, &negate_interval(offset)?)?,
        (true, false) => tz_add_interval(setting, base, offset)?,
        (true, true) => tz_add_interval(setting, base, &negate_interval(offset)?)?,
    };
    Ok(if less { val <= sum } else { val >= sum })
}

/// Output for a value that PG prints as a special keyword, if any.
fn special_text(v: i64) -> Option<&'static str> {
    if v == TIMESTAMP_INFINITY {
        Some("infinity")
    } else if v == TIMESTAMP_NEG_INFINITY {
        Some("-infinity")
    } else {
        None
    }
}

// ---------------------------------------------------------------------------------------------
// Public contract functions
// ---------------------------------------------------------------------------------------------

/// Round a timestamp to `precision` fractional digits (no-op when precision < 0 or >= 6, or infinite).
// CONTRACT
pub fn adjust_timestamp_precision(ts: i64, precision: i32) -> Result<i64> {
    const SCALES: [i64; 7] = [1_000_000, 100_000, 10_000, 1_000, 100, 10, 1];
    const OFFSETS: [i64; 7] = [500_000, 50_000, 5_000, 500, 50, 5, 0];
    if !is_finite_ts(ts) || !(0..MAX_TIMESTAMP_PRECISION).contains(&precision) {
        return Ok(ts);
    }
    let p = precision as usize;
    let scale = SCALES[p];
    let off = OFFSETS[p];
    // Half away from zero, as PG's AdjustTimestampForTypmod.
    let rounded = if ts >= 0 {
        ((ts + off) / scale) * scale
    } else {
        -((((-ts) + off) / scale) * scale)
    };
    ensure_valid(rounded)
}

/// `ts + iv` for timestamp WITHOUT time zone (PG `timestamp_pl_interval`).
// CONTRACT
pub fn timestamp_add_interval(ts: i64, iv: &Interval) -> Result<i64> {
    if let Some(r) = add_infinite_rules(ts, iv) {
        return r;
    }
    add_interval_local(ts, iv)
}

/// `timestamp_in` / `timestamptz_in` by `ty.oid`.
// CONTRACT
pub fn input(ctx: &IoCtx, ty: TypeId, s: &str) -> Result<Datum> {
    let with_tz = tz_flag(ty)?;
    let type_name = if with_tz { "timestamp with time zone" } else { "timestamp without time zone" };
    let dec = crate::datetime::decode::parse_date_time(s, ctx, type_name)?;
    let value = match dec.kind {
        DateTimeKind::Date => {
            let v = if with_tz {
                match dec.tz {
                    // tm2timestamp with an explicit zone yields the UTC instant.
                    Some(west) => tm2timestamp(&dec.tm, Some(west)).map_err(|e| remap_range(e, s))?,
                    None => {
                        let local = tm2timestamp(&dec.tm, None).map_err(|e| remap_range(e, s))?;
                        let (utc, ov) = local_to_utc(&ctx.config.time_zone, local)?;
                        if ov != 0 {
                            return Err(out_of_range_ts_input(s));
                        }
                        utc
                    }
                }
            } else {
                tm2timestamp(&dec.tm, None).map_err(|e| remap_range(e, s))?
            };
            ensure_valid(v).map_err(|_| out_of_range_ts_input(s))?
        }
        DateTimeKind::Epoch => EPOCH_TS,
        DateTimeKind::Late => TIMESTAMP_INFINITY,
        DateTimeKind::Early => TIMESTAMP_NEG_INFINITY,
        DateTimeKind::Now => {
            let now = ctx.now.ok_or_else(|| {
                PgError::new(
                    SqlState::FEATURE_NOT_SUPPORTED,
                    format!("cannot use \"{s}\" without a transaction timestamp"),
                )
            })?;
            if with_tz { now } else { utc_to_local(&ctx.config.time_zone, now)? }
        }
    };
    let v = adjust_timestamp_precision(value, ty.typmod).map_err(|_| out_of_range_ts_input(s))?;
    Ok(make_datum(with_tz, v))
}

/// `timestamp_out` / `timestamptz_out`.
// CONTRACT
pub fn output(ctx: &IoCtx, ty: TypeId, d: &Datum) -> Result<String> {
    let (with_tz, v) = match (ty.oid, d) {
        (oids::TIMESTAMP, Datum::Timestamp(v)) => (false, *v),
        (oids::TIMESTAMPTZ, Datum::TimestampTz(v)) => (true, *v),
        _ => {
            let expected = if ty.oid == oids::TIMESTAMPTZ { "timestamptz" } else { "timestamp" };
            return Err(type_mismatch(expected, d));
        }
    };
    if let Some(s) = special_text(v) {
        return Ok(s.to_string());
    }
    let style = ctx.config.date_style;
    let order = ctx.config.date_order;
    if !with_tz {
        let tm = timestamp2tm(v, None)?;
        return encode::encode_date_time(&tm, None, None, style, order);
    }
    let setting = &ctx.config.time_zone;
    let east = tz::offset_at_utc(setting, v)?;
    let tm = timestamp2tm(v, Some(-east))?;
    let abbrev = tz::abbrev_at_utc(setting, v)?;
    encode::encode_date_time(&tm, Some(-east), Some(abbrev.as_str()), style, order)
}

/// `timestamp_recv` / `timestamptz_recv`.
// CONTRACT
pub fn recv(ctx: &IoCtx, ty: TypeId, buf: &[u8]) -> Result<Datum> {
    let _ = ctx;
    let with_tz = tz_flag(ty)?;
    let bytes = <[u8; 8]>::try_from(buf).map_err(|_| invalid_binary(""))?;
    let v = i64::from_be_bytes(bytes);
    if is_finite_ts(v) && !is_valid_ts(v) {
        return Err(out_of_range_ts());
    }
    let v = adjust_timestamp_precision(v, ty.typmod)?;
    Ok(make_datum(with_tz, v))
}

/// `timestamp_send` / `timestamptz_send`.
// CONTRACT
pub fn send(ctx: &IoCtx, ty: TypeId, d: &Datum) -> Result<Vec<u8>> {
    let _ = ctx;
    let expected = if ty.oid == oids::TIMESTAMPTZ { "timestamptz" } else { "timestamp" };
    match d {
        Datum::Timestamp(v) | Datum::TimestampTz(v) => Ok(v.to_be_bytes().to_vec()),
        other => Err(type_mismatch(expected, other)),
    }
}

// ---------------------------------------------------------------------------------------------
// Builtin function implementations
// ---------------------------------------------------------------------------------------------

/// Generates `name_eq .. name_ge` (bool) and `name_cmp` (int4) over one comparison function.
macro_rules! cmp_family {
    ($cmp:ident, $eq:ident, $ne:ident, $lt:ident, $le:ident, $gt:ident, $ge:ident, $ord:ident) => {
        fn $eq(args: &[Datum], ctx: &FuncCtx) -> Result<Datum> {
            Ok(Datum::Bool($cmp(args, ctx)? == 0))
        }
        fn $ne(args: &[Datum], ctx: &FuncCtx) -> Result<Datum> {
            Ok(Datum::Bool($cmp(args, ctx)? != 0))
        }
        fn $lt(args: &[Datum], ctx: &FuncCtx) -> Result<Datum> {
            Ok(Datum::Bool($cmp(args, ctx)? < 0))
        }
        fn $le(args: &[Datum], ctx: &FuncCtx) -> Result<Datum> {
            Ok(Datum::Bool($cmp(args, ctx)? <= 0))
        }
        fn $gt(args: &[Datum], ctx: &FuncCtx) -> Result<Datum> {
            Ok(Datum::Bool($cmp(args, ctx)? > 0))
        }
        fn $ge(args: &[Datum], ctx: &FuncCtx) -> Result<Datum> {
            Ok(Datum::Bool($cmp(args, ctx)? >= 0))
        }
        fn $ord(args: &[Datum], ctx: &FuncCtx) -> Result<Datum> {
            Ok(Datum::Int4($cmp(args, ctx)?))
        }
    };
}

fn cmp_ts_ts(args: &[Datum], _ctx: &FuncCtx) -> Result<i32> {
    Ok(cmp_i64(ts_arg(arg(args, 0)?)?, ts_arg(arg(args, 1)?)?))
}

fn cmp_ts_date(args: &[Datum], _ctx: &FuncCtx) -> Result<i32> {
    let ts = ts_arg(arg(args, 0)?)?;
    let d = date_arg(arg(args, 1)?)?;
    Ok(-cmp_date_ts(d, ts))
}

fn cmp_tz_date(args: &[Datum], ctx: &FuncCtx) -> Result<i32> {
    let tstz = ts_arg(arg(args, 0)?)?;
    let d = date_arg(arg(args, 1)?)?;
    Ok(-cmp_date_tstz(&ctx.io.time_zone, d, tstz)?)
}

fn cmp_ts_tz(args: &[Datum], ctx: &FuncCtx) -> Result<i32> {
    let ts = ts_arg(arg(args, 0)?)?;
    let tstz = ts_arg(arg(args, 1)?)?;
    cmp_ts_tstz(&ctx.io.time_zone, ts, tstz)
}

fn cmp_tz_ts(args: &[Datum], ctx: &FuncCtx) -> Result<i32> {
    let tstz = ts_arg(arg(args, 0)?)?;
    let ts = ts_arg(arg(args, 1)?)?;
    Ok(-cmp_ts_tstz(&ctx.io.time_zone, ts, tstz)?)
}

cmp_family!(cmp_ts_ts, ts_eq, ts_ne, ts_lt, ts_le, ts_gt, ts_ge, ts_cmp);
cmp_family!(cmp_ts_date, ts_date_eq, ts_date_ne, ts_date_lt, ts_date_le, ts_date_gt, ts_date_ge, ts_date_cmp);
cmp_family!(cmp_tz_date, tz_date_eq, tz_date_ne, tz_date_lt, tz_date_le, tz_date_gt, tz_date_ge, tz_date_cmp);
cmp_family!(cmp_ts_tz, ts_tz_eq, ts_tz_ne, ts_tz_lt, ts_tz_le, ts_tz_gt, ts_tz_ge, ts_tz_cmp);
cmp_family!(cmp_tz_ts, tz_ts_eq, tz_ts_ne, tz_ts_lt, tz_ts_le, tz_ts_gt, tz_ts_ge, tz_ts_cmp);

fn ts_larger(args: &[Datum], _ctx: &FuncCtx) -> Result<Datum> {
    let a = arg(args, 0)?;
    let b = arg(args, 1)?;
    Ok(if ts_arg(a)? > ts_arg(b)? { a.clone() } else { b.clone() })
}

fn ts_smaller(args: &[Datum], _ctx: &FuncCtx) -> Result<Datum> {
    let a = arg(args, 0)?;
    let b = arg(args, 1)?;
    Ok(if ts_arg(a)? < ts_arg(b)? { a.clone() } else { b.clone() })
}

fn hash_ts(args: &[Datum], _ctx: &FuncCtx) -> Result<Datum> {
    let v = ts_arg(arg(args, 0)?)?;
    let h = compare::pg_hash_uint32(hash_fold(v))?;
    Ok(Datum::Int4(h as i32))
}

fn hash_ts_extended(args: &[Datum], _ctx: &FuncCtx) -> Result<Datum> {
    let v = ts_arg(arg(args, 0)?)?;
    let seed = int8_arg(arg(args, 1)?)? as u64;
    let h = compare::pg_hash_uint32_extended(hash_fold(v), seed)?;
    Ok(Datum::Int8(h as i64))
}

fn ts_finite(args: &[Datum], _ctx: &FuncCtx) -> Result<Datum> {
    Ok(Datum::Bool(is_finite_ts(ts_arg(arg(args, 0)?)?)))
}

fn ts_mi(args: &[Datum], _ctx: &FuncCtx) -> Result<Datum> {
    let a = ts_arg(arg(args, 0)?)?;
    let b = ts_arg(arg(args, 1)?)?;
    Ok(Datum::Interval(timestamp_diff(a, b)?))
}

fn ts_pl_iv(args: &[Datum], _ctx: &FuncCtx) -> Result<Datum> {
    let ts = ts_arg(arg(args, 0)?)?;
    let iv = iv_arg(arg(args, 1)?)?;
    Ok(Datum::Timestamp(timestamp_add_interval(ts, &iv)?))
}

fn ts_mi_iv(args: &[Datum], _ctx: &FuncCtx) -> Result<Datum> {
    let ts = ts_arg(arg(args, 0)?)?;
    let iv = negate_interval(&iv_arg(arg(args, 1)?)?)?;
    Ok(Datum::Timestamp(timestamp_add_interval(ts, &iv)?))
}

fn tz_pl_iv(args: &[Datum], ctx: &FuncCtx) -> Result<Datum> {
    let tstz = ts_arg(arg(args, 0)?)?;
    let iv = iv_arg(arg(args, 1)?)?;
    Ok(Datum::TimestampTz(tz_add_interval(&ctx.io.time_zone, tstz, &iv)?))
}

fn tz_mi_iv(args: &[Datum], ctx: &FuncCtx) -> Result<Datum> {
    let tstz = ts_arg(arg(args, 0)?)?;
    let iv = negate_interval(&iv_arg(arg(args, 1)?)?)?;
    Ok(Datum::TimestampTz(tz_add_interval(&ctx.io.time_zone, tstz, &iv)?))
}

/// `timestamp::timestamptz` (PG `timestamp_timestamptz`).
fn cast_ts_tz(args: &[Datum], ctx: &FuncCtx) -> Result<Datum> {
    let ts = ts_arg(arg(args, 0)?)?;
    if !is_finite_ts(ts) {
        return Ok(Datum::TimestampTz(ts));
    }
    Ok(Datum::TimestampTz(local_to_utc_strict(&ctx.io.time_zone, ts)?))
}

/// `timestamptz::timestamp` (PG `timestamptz_timestamp`).
fn cast_tz_ts(args: &[Datum], ctx: &FuncCtx) -> Result<Datum> {
    let tstz = ts_arg(arg(args, 0)?)?;
    if !is_finite_ts(tstz) {
        return Ok(Datum::Timestamp(tstz));
    }
    let local = ensure_valid(utc_to_local(&ctx.io.time_zone, tstz)?)?;
    Ok(Datum::Timestamp(local))
}

/// `timestamp::date` (PG `timestamp_date`).
fn cast_ts_date(args: &[Datum], _ctx: &FuncCtx) -> Result<Datum> {
    let ts = ts_arg(arg(args, 0)?)?;
    if ts == TIMESTAMP_INFINITY {
        return Ok(Datum::Date(DATE_INFINITY));
    }
    if ts == TIMESTAMP_NEG_INFINITY {
        return Ok(Datum::Date(DATE_NEG_INFINITY));
    }
    let days = i32::try_from(ts.div_euclid(USECS_PER_DAY)).map_err(|_| out_of_range_ts())?;
    Ok(Datum::Date(days))
}

/// `timestamptz::date` in the session zone (PG `timestamptz_date`).
fn cast_tz_date(args: &[Datum], ctx: &FuncCtx) -> Result<Datum> {
    let tstz = ts_arg(arg(args, 0)?)?;
    if tstz == TIMESTAMP_INFINITY {
        return Ok(Datum::Date(DATE_INFINITY));
    }
    if tstz == TIMESTAMP_NEG_INFINITY {
        return Ok(Datum::Date(DATE_NEG_INFINITY));
    }
    let local = utc_to_local(&ctx.io.time_zone, tstz)?;
    let days = i32::try_from(local.div_euclid(USECS_PER_DAY)).map_err(|_| out_of_range_ts())?;
    Ok(Datum::Date(days))
}

/// `timestamp::time` (PG `timestamp_time`): infinities give NULL.
fn cast_ts_time(args: &[Datum], _ctx: &FuncCtx) -> Result<Datum> {
    let ts = ts_arg(arg(args, 0)?)?;
    if !is_finite_ts(ts) {
        return Ok(Datum::Null);
    }
    Ok(Datum::Time(tod_of_local(ts)))
}

/// `timestamptz::time` in the session zone (PG `timestamptz_time`).
fn cast_tz_time(args: &[Datum], ctx: &FuncCtx) -> Result<Datum> {
    let tstz = ts_arg(arg(args, 0)?)?;
    if !is_finite_ts(tstz) {
        return Ok(Datum::Null);
    }
    let local = utc_to_local(&ctx.io.time_zone, tstz)?;
    Ok(Datum::Time(tod_of_local(local)))
}

/// `timestamptz::timetz` (PG `timestamptz_timetz`): zone is seconds WEST of UTC.
fn cast_tz_timetz(args: &[Datum], ctx: &FuncCtx) -> Result<Datum> {
    let tstz = ts_arg(arg(args, 0)?)?;
    if !is_finite_ts(tstz) {
        return Ok(Datum::Null);
    }
    let setting = &ctx.io.time_zone;
    let east = tz::offset_at_utc(setting, tstz)?;
    let local = utc_to_local(setting, tstz)?;
    Ok(Datum::TimeTz(tod_of_local(local), -east))
}

/// SQL `timestamptz(date, time)`: `CAST(date + time AS timestamptz)`.
fn make_tz(args: &[Datum], ctx: &FuncCtx) -> Result<Datum> {
    let d = date_arg(arg(args, 0)?)?;
    let t = time_arg(arg(args, 1)?)?;
    let local = date_plus_time(d, t)?;
    Ok(Datum::TimestampTz(if is_finite_ts(local) {
        local_to_utc_strict(&ctx.io.time_zone, local)?
    } else {
        local
    }))
}

/// SQL `timedate_pl(time, date)` = `date + time`.
fn time_date_pl(args: &[Datum], _ctx: &FuncCtx) -> Result<Datum> {
    let t = time_arg(arg(args, 0)?)?;
    let d = date_arg(arg(args, 1)?)?;
    Ok(Datum::Timestamp(date_plus_time(d, t)?))
}

/// SQL `timetzdate_pl(timetz, date)` = `date + timetz` (PG `datetimetz_timestamptz`).
fn timetz_date_pl(args: &[Datum], _ctx: &FuncCtx) -> Result<Datum> {
    let (t, zone) = timetz_arg(arg(args, 0)?)?;
    let d = date_arg(arg(args, 1)?)?;
    if d == DATE_INFINITY {
        return Ok(Datum::TimestampTz(TIMESTAMP_INFINITY));
    }
    if d == DATE_NEG_INFINITY {
        return Ok(Datum::TimestampTz(TIMESTAMP_NEG_INFINITY));
    }
    if d >= DATE_OVERFLOW_DAYS {
        return Err(date_out_of_range_for_ts());
    }
    let v = i64::from(d) * USECS_PER_DAY + t + i64::from(zone) * USECS_PER_SEC;
    if !is_valid_ts(v) {
        return Err(date_out_of_range_for_ts());
    }
    Ok(Datum::TimestampTz(v))
}

/// SQL `interval_pl_date(interval, date)` = `date + interval` (timestamp result).
fn iv_pl_date(args: &[Datum], _ctx: &FuncCtx) -> Result<Datum> {
    let iv = iv_arg(arg(args, 0)?)?;
    let d = date_arg(arg(args, 1)?)?;
    let base = date_to_ts_strict(d)?;
    Ok(Datum::Timestamp(timestamp_add_interval(base, &iv)?))
}

/// SQL `interval_pl_timestamp(interval, timestamp)`.
fn iv_pl_ts(args: &[Datum], _ctx: &FuncCtx) -> Result<Datum> {
    let iv = iv_arg(arg(args, 0)?)?;
    let ts = ts_arg(arg(args, 1)?)?;
    Ok(Datum::Timestamp(timestamp_add_interval(ts, &iv)?))
}

/// SQL `interval_pl_timestamptz(interval, timestamptz)`.
fn iv_pl_tz(args: &[Datum], ctx: &FuncCtx) -> Result<Datum> {
    let iv = iv_arg(arg(args, 0)?)?;
    let tstz = ts_arg(arg(args, 1)?)?;
    Ok(Datum::TimestampTz(tz_add_interval(&ctx.io.time_zone, tstz, &iv)?))
}

/// `in_range_timestamp_interval(val, base, offset, sub, less)`.
fn in_range_ts(args: &[Datum], ctx: &FuncCtx) -> Result<Datum> {
    let val = ts_arg(arg(args, 0)?)?;
    let base = ts_arg(arg(args, 1)?)?;
    let offset = iv_arg(arg(args, 2)?)?;
    let sub = bool_arg(arg(args, 3)?)?;
    let less = bool_arg(arg(args, 4)?)?;
    Ok(Datum::Bool(in_range_impl(&ctx.io.time_zone, val, base, &offset, sub, less, false)?))
}

/// `in_range_timestamptz_interval(val, base, offset, sub, less)`.
fn in_range_tz(args: &[Datum], ctx: &FuncCtx) -> Result<Datum> {
    let val = ts_arg(arg(args, 0)?)?;
    let base = ts_arg(arg(args, 1)?)?;
    let offset = iv_arg(arg(args, 2)?)?;
    let sub = bool_arg(arg(args, 3)?)?;
    let less = bool_arg(arg(args, 4)?)?;
    Ok(Datum::Bool(in_range_impl(&ctx.io.time_zone, val, base, &offset, sub, less, true)?))
}

/// Function registry hook (see `crate::func`).
pub(crate) fn lookup(proc: &BuiltinProc) -> Option<FuncImpl> {
    let f: ScalarFn = if proc.prosrc.is_empty() {
        // SQL-language builtins have no C symbol: match on the OID.
        match proc.oid {
            1176 => make_tz,
            1296 => time_date_pl,
            1298 => timetz_date_pl,
            2546 => iv_pl_date,
            2548 => iv_pl_ts,
            2549 => iv_pl_tz,
            _ => return None,
        }
    } else {
        match proc.prosrc {
            "timestamp_eq" => ts_eq,
            "timestamp_ne" => ts_ne,
            "timestamp_lt" => ts_lt,
            "timestamp_le" => ts_le,
            "timestamp_gt" => ts_gt,
            "timestamp_ge" => ts_ge,
            "timestamp_cmp" => ts_cmp,
            "timestamp_eq_date" => ts_date_eq,
            "timestamp_ne_date" => ts_date_ne,
            "timestamp_lt_date" => ts_date_lt,
            "timestamp_le_date" => ts_date_le,
            "timestamp_gt_date" => ts_date_gt,
            "timestamp_ge_date" => ts_date_ge,
            "timestamp_cmp_date" => ts_date_cmp,
            "timestamptz_eq_date" => tz_date_eq,
            "timestamptz_ne_date" => tz_date_ne,
            "timestamptz_lt_date" => tz_date_lt,
            "timestamptz_le_date" => tz_date_le,
            "timestamptz_gt_date" => tz_date_gt,
            "timestamptz_ge_date" => tz_date_ge,
            "timestamptz_cmp_date" => tz_date_cmp,
            "timestamp_eq_timestamptz" => ts_tz_eq,
            "timestamp_ne_timestamptz" => ts_tz_ne,
            "timestamp_lt_timestamptz" => ts_tz_lt,
            "timestamp_le_timestamptz" => ts_tz_le,
            "timestamp_gt_timestamptz" => ts_tz_gt,
            "timestamp_ge_timestamptz" => ts_tz_ge,
            "timestamp_cmp_timestamptz" => ts_tz_cmp,
            "timestamptz_eq_timestamp" => tz_ts_eq,
            "timestamptz_ne_timestamp" => tz_ts_ne,
            "timestamptz_lt_timestamp" => tz_ts_lt,
            "timestamptz_le_timestamp" => tz_ts_le,
            "timestamptz_gt_timestamp" => tz_ts_gt,
            "timestamptz_ge_timestamp" => tz_ts_ge,
            "timestamptz_cmp_timestamp" => tz_ts_cmp,
            "timestamp_larger" => ts_larger,
            "timestamp_smaller" => ts_smaller,
            "timestamp_hash" | "timestamptz_hash" => hash_ts,
            "timestamp_hash_extended" | "timestamptz_hash_extended" => hash_ts_extended,
            "timestamp_finite" => ts_finite,
            "timestamp_mi" => ts_mi,
            "timestamp_pl_interval" => ts_pl_iv,
            "timestamp_mi_interval" => ts_mi_iv,
            "timestamptz_pl_interval" => tz_pl_iv,
            "timestamptz_mi_interval" => tz_mi_iv,
            "timestamp_timestamptz" => cast_ts_tz,
            "timestamptz_timestamp" => cast_tz_ts,
            "timestamp_date" => cast_ts_date,
            "timestamptz_date" => cast_tz_date,
            "timestamp_time" => cast_ts_time,
            "timestamptz_time" => cast_tz_time,
            "timestamptz_timetz" => cast_tz_timetz,
            "in_range_timestamp_interval" => in_range_ts,
            "in_range_timestamptz_interval" => in_range_tz,
            _ => return None,
        }
    };
    Some(FuncImpl::Scalar(f))
}

/// 0A000 for a Timestamp-module proc without implementation.
pub(crate) fn unsupported(proc: &BuiltinProc) -> PgError {
    PgError::unimplemented("TYP-28", format!("function {}", proc.name))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::context::{IoConfig, IoCtx};
    use crate::datetime::USECS_PER_SEC;

    /// Local (no zone) timestamp of a calendar date at midnight.
    fn ts_of(y: i32, m: i32, d: i32) -> i64 {
        (i64::from(date2j(y, m, d)) - i64::from(POSTGRES_EPOCH_JDATE)) * USECS_PER_DAY
    }

    #[test]
    fn precision_rounds_half_away_from_zero() {
        assert_eq!(adjust_timestamp_precision(125_000, 2).unwrap(), 130_000);
        assert_eq!(adjust_timestamp_precision(-125_000, 2).unwrap(), -130_000);
        assert_eq!(adjust_timestamp_precision(500_000, 0).unwrap(), 1_000_000);
        assert_eq!(adjust_timestamp_precision(123_456, 6).unwrap(), 123_456);
        assert_eq!(adjust_timestamp_precision(123_456, -1).unwrap(), 123_456);
        assert_eq!(adjust_timestamp_precision(TIMESTAMP_INFINITY, 0).unwrap(), TIMESTAMP_INFINITY);
    }

    #[test]
    fn add_one_month_clamps_to_leap_day() {
        let r = timestamp_add_interval(ts_of(2024, 1, 31), &Interval::new(1, 0, 0)).unwrap();
        assert_eq!(r, ts_of(2024, 2, 29));
    }

    #[test]
    fn add_one_month_clamps_to_non_leap_day() {
        let r = timestamp_add_interval(ts_of(2023, 1, 31), &Interval::new(1, 0, 0)).unwrap();
        assert_eq!(r, ts_of(2023, 2, 28));
    }

    #[test]
    fn add_days_then_micros() {
        let base = ts_of(2024, 3, 1);
        let r = timestamp_add_interval(base, &Interval::new(0, 2, USECS_PER_SEC)).unwrap();
        assert_eq!(r, ts_of(2024, 3, 3) + USECS_PER_SEC);
    }

    #[test]
    fn add_interval_to_infinities() {
        assert_eq!(timestamp_add_interval(TIMESTAMP_INFINITY, &Interval::new(1, 0, 0)).unwrap(), TIMESTAMP_INFINITY);
        assert_eq!(timestamp_add_interval(0, &Interval::INFINITY).unwrap(), TIMESTAMP_INFINITY);
        assert!(timestamp_add_interval(TIMESTAMP_INFINITY, &Interval::NEG_INFINITY).is_err());
    }

    #[test]
    fn subtract_timestamps_gives_days() {
        let d = timestamp_diff(ts_of(2024, 1, 1), ts_of(2023, 1, 1)).unwrap();
        assert_eq!(d, Interval::new(0, 365, 0));
    }

    #[test]
    fn subtract_infinite_timestamps() {
        assert_eq!(timestamp_diff(TIMESTAMP_INFINITY, 0).unwrap(), Interval::INFINITY);
        assert_eq!(timestamp_diff(0, TIMESTAMP_INFINITY).unwrap(), Interval::NEG_INFINITY);
        let e = timestamp_diff(TIMESTAMP_INFINITY, TIMESTAMP_INFINITY).unwrap_err();
        assert_eq!(e.sqlstate, SqlState::DATETIME_VALUE_OUT_OF_RANGE);
        assert_eq!(e.message, "interval out of range");
    }

    #[test]
    fn binary_roundtrip() {
        let cfg = IoConfig::default();
        let ctx = IoCtx::new(crate::registry::builtin_env(), &cfg);
        let d = Datum::Timestamp(ts_of(2024, 2, 29) + 13 * 3600 * USECS_PER_SEC + 500_000);
        let bytes = send(&ctx, TypeId::TIMESTAMP, &d).unwrap();
        assert_eq!(recv(&ctx, TypeId::TIMESTAMP, &bytes).unwrap(), d);
        let inf = Datum::TimestampTz(TIMESTAMP_NEG_INFINITY);
        let bytes = send(&ctx, TypeId::TIMESTAMPTZ, &inf).unwrap();
        assert_eq!(recv(&ctx, TypeId::TIMESTAMPTZ, &bytes).unwrap(), inf);
        assert!(recv(&ctx, TypeId::TIMESTAMP, &[0u8; 4]).is_err());
    }

    #[test]
    fn output_specials() {
        let cfg = IoConfig::default();
        let ctx = IoCtx::new(crate::registry::builtin_env(), &cfg);
        assert_eq!(output(&ctx, TypeId::TIMESTAMP, &Datum::Timestamp(TIMESTAMP_INFINITY)).unwrap(), "infinity");
        assert_eq!(output(&ctx, TypeId::TIMESTAMPTZ, &Datum::TimestampTz(TIMESTAMP_NEG_INFINITY)).unwrap(), "-infinity");
    }

    #[test]
    fn input_out_of_range_names_the_input() {
        let cfg = IoConfig::default();
        let ctx = IoCtx::new(crate::registry::builtin_env(), &cfg);
        let e = input(&ctx, TypeId::TIMESTAMP, "294277-01-01").unwrap_err();
        assert_eq!(e.sqlstate, SqlState::DATETIME_VALUE_OUT_OF_RANGE);
        assert_eq!(e.message, "timestamp out of range: \"294277-01-01\"");
    }

    #[test]
    fn input_typmod_rounds_fraction() {
        let cfg = IoConfig::default();
        let ctx = IoCtx::new(crate::registry::builtin_env(), &cfg);
        let d = input(&ctx, TypeId::new(oids::TIMESTAMP, 2), "2020-01-01 00:00:00.125").unwrap();
        assert_eq!(output(&ctx, TypeId::new(oids::TIMESTAMP, 2), &d).unwrap(), "2020-01-01 00:00:00.13");
    }

    #[test]
    fn input_rounds_nanoseconds_to_micros() {
        let cfg = IoConfig::default();
        let ctx = IoCtx::new(crate::registry::builtin_env(), &cfg);
        let d = input(&ctx, TypeId::TIMESTAMP, "2020-01-01 00:00:00.123456789").unwrap();
        assert_eq!(output(&ctx, TypeId::TIMESTAMP, &d).unwrap(), "2020-01-01 00:00:00.123457");
    }

    #[test]
    fn timestamptz_input_normalizes_to_utc_session_output() {
        let cfg = IoConfig::default();
        let ctx = IoCtx::new(crate::registry::builtin_env(), &cfg);
        let d = input(&ctx, TypeId::TIMESTAMPTZ, "2024-02-29 13:01:02.5+05:30").unwrap();
        assert_eq!(output(&ctx, TypeId::TIMESTAMPTZ, &d).unwrap(), "2024-02-29 07:31:02.5+00");
    }
}
