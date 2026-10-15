//! `to_char`, `to_date`, `to_timestamp(text, text)`, `to_number` (TYPE-13, TYPE-14).
//! Lease TYP-31 (sonnet).
//!
//! PG18 reference: `src/backend/utils/adt/formatting.c` (template parser `parse_format`, keyword
//! tables `DCH_keywords` / `NUM_keywords`, `DCH_to_char`, `DCH_from_char`, `NUM_processor`).
//! Owned procs (ImplModule::Formatting): `timestamp_to_char timestamptz_to_char interval_to_char
//! numeric_to_char int4_to_char int8_to_char float4_to_char float8_to_char to_date to_timestamp
//! numeric_to_number`.
//!
//! Datetime patterns: `HH HH12 HH24 MI SS MS US FF1..FF6 SSSS AM/PM/am/pm/A.M. Y,YYY YYYY YYY YY Y IYYY
//! IYY IY I BC/AD MONTH Month month MON Mon mon MM DAY Day day DY Dy dy DDD IDDD DD D ID W WW IW CC J Q
//! RM rm TZ tz TZH TZM OF`; modifiers `FM` (fill mode: no padding), `TH`/`th` (ordinal suffix), `FX`,
//! `TM` (translation: C locale = English), quoted text `"..."`, backslash escapes. Upper/lower/capitalized
//! keyword variants change the output case and padding (Month is blank-padded to 9 chars unless FM).
//! Number patterns: `9 0 . , D G S PR MI PL SG EEEE V RN TH`, `FM`; overflow of the pattern prints `#`s.
//! Examples (PG docs/oracle): `to_char(timestamp '2002-04-20 17:31:12.66', 'HH12:MI:SS')` = `05:31:12`;
//! `to_char(125.8::real, '999D99')` = ` 125.80`; `to_char(-0.1, 'FM9.99')` = `-.1`;
//! `to_char(485, 'RN')` = `        CDLXXXV`; `to_date('05 Dec 2000', 'DD Mon YYYY')` = `2000-12-05`;
//! `to_number('12,454.8-', '99G999D9S')` = `-12454.8`. Errors follow PG wording, e.g. 22007
//! `invalid value "xx" for "MON"`, 22008 `date/time field value out of range: "..."`.
use crate::catalog_data::BuiltinProc;
use crate::context::{FuncCtx, TimeZoneSetting};
use crate::datetime::calendar::{
    date2isoweek, date2isoyear, date2j, days_in_month, interval2itm, isoweek2j, itm2interval, j2date, j2day,
    time2tm, timestamp2tm, tm2timestamp,
};
use crate::datetime::date::date_to_timestamp;
use crate::datetime::timestamp::timestamp_add_interval;
use crate::datetime::tz::{self, ZoneSpec};
use crate::datetime::{
    dt_parse_error, interval, out_of_range, DtErr, Itm, Tm, MIN_TIMESTAMP, END_TIMESTAMP, POSTGRES_EPOCH_JDATE,
    TIMESTAMP_END_JULIAN, UNIX_EPOCH_JDATE, USECS_PER_DAY, USECS_PER_HOUR, USECS_PER_MINUTE, USECS_PER_SEC,
};
use crate::datum::{
    type_mismatch, Datum, Interval, TypeId, DATE_INFINITY, DATE_NEG_INFINITY, TIMESTAMP_INFINITY,
    TIMESTAMP_NEG_INFINITY,
};
use crate::func::{FuncImpl, ScalarFn};
use crate::numeric::arith;
use crate::numeric::Numeric;
use bigdecimal::BigDecimal;
use num_bigint::BigInt;
use pgarbor_common::{PgError, Result, SqlState};

// ---------------------------------------------------------------------------------------------
// Registry hook
// ---------------------------------------------------------------------------------------------

/// Function registry hook (see `crate::func`).
pub(crate) fn lookup(proc: &BuiltinProc) -> Option<FuncImpl> {
    let f: ScalarFn = match proc.oid {
        // SQL-language builtins (empty prosrc).
        1305 | 1306 | 1307 | 1309 | 1310 | 1311 | 2042 | 2043 | 2044 => overlaps_sql,
        1384 => date_part_date,
        1386 => age_timestamptz_now,
        2059 => age_timestamp_now,
        _ => match proc.prosrc {
            "extract_date" => extract_date,
            "extract_time" => extract_time,
            "extract_timetz" => extract_timetz,
            "extract_timestamp" => extract_timestamp,
            "extract_timestamptz" => extract_timestamptz,
            "extract_interval" => extract_interval,
            "timestamp_part" => timestamp_part,
            "timestamptz_part" => timestamptz_part,
            "time_part" => time_part,
            "timetz_part" => timetz_part,
            "interval_part" => interval_part,
            "timestamp_trunc" => timestamp_trunc,
            "timestamptz_trunc" => timestamptz_trunc,
            "timestamptz_trunc_zone" => timestamptz_trunc_zone,
            "interval_trunc" => interval_trunc,
            "timestamp_bin" => timestamp_bin,
            "timestamptz_bin" => timestamptz_bin,
            "timestamp_age" => timestamp_age,
            "timestamptz_age" => timestamptz_age,
            "make_interval" => make_interval,
            "make_timestamp" => make_timestamp,
            "make_timestamptz" => make_timestamptz,
            "make_timestamptz_at_timezone" => make_timestamptz_at_timezone,
            "now" => now,
            "statement_timestamp" => statement_timestamp,
            "clock_timestamp" => clock_timestamp,
            "float8_timestamptz" => float8_timestamptz,
            "timestamp_zone" => timestamp_zone,
            "timestamptz_zone" => timestamptz_zone,
            "timetz_zone" => timetz_zone,
            "timestamp_izone" => timestamp_izone,
            "timestamptz_izone" => timestamptz_izone,
            "timetz_izone" => timetz_izone,
            "timestamp_at_local" => timestamp_at_local,
            "timestamptz_at_local" => timestamptz_at_local,
            "timetz_at_local" => timetz_at_local,
            "timestamptz_pl_interval_at_zone" => timestamptz_pl_interval_at_zone,
            "timestamptz_mi_interval_at_zone" => timestamptz_mi_interval_at_zone,
            "overlaps_timestamp" => overlaps_timestamp,
            "overlaps_time" => overlaps_time,
            "overlaps_timetz" => overlaps_timetz,
            _ => return None,
        },
    };
    Some(FuncImpl::Scalar(f))
}

/// 0A000 for a DatetimeFuncs-module proc without implementation.
#[allow(dead_code)]
pub(crate) fn unsupported(proc: &BuiltinProc) -> PgError {
    PgError::unimplemented("TYP-30", format!("function {}", proc.name))
}

// ---------------------------------------------------------------------------------------------
// Argument helpers
// ---------------------------------------------------------------------------------------------

const T_DATE: &str = "date";
const T_TIME: &str = "time without time zone";
const T_TIMETZ: &str = "time with time zone";
const T_TS: &str = "timestamp without time zone";
const T_TSTZ: &str = "timestamp with time zone";
const T_INTERVAL: &str = "interval";

/// `1970-01-01 00:00:00` on the 2000-epoch µs axis (`SetEpochTimestamp`).
const EPOCH_TS: i64 = (UNIX_EPOCH_JDATE as i64 - POSTGRES_EPOCH_JDATE as i64) * USECS_PER_DAY;

fn missing_arg(i: usize) -> PgError {
    PgError::internal(format!("missing function argument {i}"))
}

fn arg(args: &[Datum], i: usize) -> Result<&Datum> {
    args.get(i).ok_or_else(|| missing_arg(i))
}

fn text_arg(args: &[Datum], i: usize) -> Result<&str> {
    match arg(args, i)? {
        Datum::Text(s) => Ok(s.as_str()),
        other => Err(type_mismatch("text", other)),
    }
}

fn ts_arg(args: &[Datum], i: usize) -> Result<i64> {
    match arg(args, i)? {
        Datum::Timestamp(v) | Datum::TimestampTz(v) => Ok(*v),
        other => Err(type_mismatch("timestamp", other)),
    }
}

fn date_arg(args: &[Datum], i: usize) -> Result<i32> {
    match arg(args, i)? {
        Datum::Date(v) => Ok(*v),
        other => Err(type_mismatch("date", other)),
    }
}

fn time_arg(args: &[Datum], i: usize) -> Result<i64> {
    match arg(args, i)? {
        Datum::Time(v) => Ok(*v),
        other => Err(type_mismatch("time", other)),
    }
}

fn timetz_arg(args: &[Datum], i: usize) -> Result<(i64, i32)> {
    match arg(args, i)? {
        Datum::TimeTz(t, z) => Ok((*t, *z)),
        other => Err(type_mismatch("timetz", other)),
    }
}

fn interval_arg(args: &[Datum], i: usize) -> Result<Interval> {
    match arg(args, i)? {
        Datum::Interval(v) => Ok(*v),
        other => Err(type_mismatch("interval", other)),
    }
}

fn int4_arg(args: &[Datum], i: usize) -> Result<i32> {
    match arg(args, i)? {
        Datum::Int4(v) => Ok(*v),
        other => Err(type_mismatch("int4", other)),
    }
}

fn float8_arg(args: &[Datum], i: usize) -> Result<f64> {
    match arg(args, i)? {
        Datum::Float8(v) => Ok(*v),
        other => Err(type_mismatch("float8", other)),
    }
}

fn is_inf_ts(ts: i64) -> bool {
    ts == TIMESTAMP_INFINITY || ts == TIMESTAMP_NEG_INFINITY
}

fn ts_out_of_range() -> PgError {
    out_of_range("timestamp")
}

fn interval_out_of_range() -> PgError {
    out_of_range("interval")
}

/// `IS_VALID_TIMESTAMP` check with PG's plain 22008 error.
fn valid_ts(ts: i64) -> Result<i64> {
    if (MIN_TIMESTAMP..END_TIMESTAMP).contains(&ts) {
        Ok(ts)
    } else {
        Err(ts_out_of_range())
    }
}

/// `dt2local(ts, tz)`: `ts - tz seconds`, range-checked.
fn dt2local(ts: i64, tz_secs: i64) -> Result<i64> {
    let shift = tz_secs.checked_mul(USECS_PER_SEC).ok_or_else(ts_out_of_range)?;
    let r = ts.checked_sub(shift).ok_or_else(ts_out_of_range)?;
    valid_ts(r)
}

// ---------------------------------------------------------------------------------------------
// Numeric / float result helpers
// ---------------------------------------------------------------------------------------------

fn num_int(v: i64) -> Datum {
    Datum::Numeric(Numeric::from_i64(v))
}

/// `v / 10^scale` as a numeric whose display scale is `scale` (PG `int64_div_fast_to_numeric`).
fn num_scaled(v: i128, scale: i64) -> Datum {
    Datum::Numeric(Numeric::Finite(BigDecimal::new(BigInt::from(v), scale)))
}

fn out_int(v: i64, numeric: bool) -> Datum {
    if numeric {
        num_int(v)
    } else {
        Datum::Float8(v as f64)
    }
}

/// Result for the non-finite helpers: 0.0 means NULL, otherwise ±Infinity.
fn inf_result(r: f64, numeric: bool) -> Datum {
    if r == 0.0 {
        Datum::Null
    } else if numeric {
        Datum::Numeric(if r < 0.0 { Numeric::NegInf } else { Numeric::PosInf })
    } else {
        Datum::Float8(r)
    }
}

// ---------------------------------------------------------------------------------------------
// Unit decoding (DecodeUnits + DecodeSpecial)
// ---------------------------------------------------------------------------------------------

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Unit {
    Microsec,
    Millisec,
    Second,
    Minute,
    Hour,
    Day,
    Week,
    Month,
    Quarter,
    Year,
    Decade,
    Century,
    Millennium,
    Julian,
    Dow,
    Isodow,
    Doy,
    Isoyear,
    Tz,
    TzHour,
    TzMinute,
    /// The RESERV token `epoch` (treated like a unit: it is the only reserved word with a value).
    Epoch,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Decoded {
    Unit(Unit),
    /// A reserved word other than `epoch` (`now`, `infinity`, ...): type RESERV in PG.
    ReservOther,
    /// UNKNOWN_FIELD or a token of another class (month names, `am`, ...).
    Unknown,
}

/// Token lookup key: PG compares at most `TOKMAXLEN` (10) bytes.
fn token_key(low: &str) -> Option<&str> {
    let b = low.as_bytes();
    std::str::from_utf8(&b[..b.len().min(10)]).ok()
}

/// `DecodeUnits` (delta table).
fn decode_delta(low: &str) -> Option<Unit> {
    let key = token_key(low)?;
    Some(match key {
        "c" | "cent" | "centuries" | "century" => Unit::Century,
        "d" | "day" | "days" => Unit::Day,
        "dec" | "decade" | "decades" | "decs" => Unit::Decade,
        "h" | "hour" | "hours" | "hr" | "hrs" => Unit::Hour,
        "m" | "min" | "mins" | "minute" | "minutes" => Unit::Minute,
        "microsecon" | "us" | "usec" | "usecond" | "useconds" | "usecs" => Unit::Microsec,
        "mil" | "millennia" | "millennium" | "mils" => Unit::Millennium,
        "millisecon" | "ms" | "msec" | "msecond" | "mseconds" | "msecs" => Unit::Millisec,
        "mon" | "mons" | "month" | "months" => Unit::Month,
        "qtr" | "quarter" => Unit::Quarter,
        "s" | "sec" | "second" | "seconds" | "secs" => Unit::Second,
        "timezone" => Unit::Tz,
        "timezone_h" => Unit::TzHour,
        "timezone_m" => Unit::TzMinute,
        "w" | "week" | "weeks" => Unit::Week,
        "y" | "year" | "years" | "yr" | "yrs" => Unit::Year,
        _ => return None,
    })
}

/// `DecodeSpecial` restricted to the classes that matter here.
fn decode_special(low: &str) -> Decoded {
    let key = match token_key(low) {
        Some(k) => k,
        None => return Decoded::Unknown,
    };
    match key {
        "j" | "jd" | "julian" => Decoded::Unit(Unit::Julian),
        "dow" => Decoded::Unit(Unit::Dow),
        "doy" => Decoded::Unit(Unit::Doy),
        "isodow" => Decoded::Unit(Unit::Isodow),
        "isoyear" => Decoded::Unit(Unit::Isoyear),
        "mm" => Decoded::Unit(Unit::Minute),
        "epoch" => Decoded::Unit(Unit::Epoch),
        "infinity" | "+infinity" | "-infinity" | "now" | "today" | "tomorrow" | "yesterday" | "allballs" => {
            Decoded::ReservOther
        }
        _ => Decoded::Unknown,
    }
}

/// `DecodeUnits` then `DecodeSpecial` (EXTRACT / date_part).
fn decode_full(low: &str) -> Decoded {
    match decode_delta(low) {
        Some(u) => Decoded::Unit(u),
        None => decode_special(low),
    }
}

fn not_recognized(low: &str, ty: &str) -> PgError {
    PgError::new(SqlState::INVALID_PARAMETER_VALUE, format!("unit \"{low}\" not recognized for type {ty}"))
}

fn not_supported(low: &str, ty: &str) -> PgError {
    PgError::new(SqlState::FEATURE_NOT_SUPPORTED, format!("unit \"{low}\" not supported for type {ty}"))
}

// ---------------------------------------------------------------------------------------------
// Shared EXTRACT pieces
// ---------------------------------------------------------------------------------------------

fn year_display(y: i32) -> i64 {
    let y = i64::from(y);
    if y > 0 {
        y
    } else {
        y - 1
    }
}

fn decade_of(y: i32) -> i64 {
    let y = i64::from(y);
    if y >= 0 {
        y / 10
    } else {
        -((8 - (y - 1)) / 10)
    }
}

fn century_of(y: i32) -> i64 {
    let y = i64::from(y);
    if y > 0 {
        (y + 99) / 100
    } else {
        -((99 - (y - 1)) / 100)
    }
}

fn millennium_of(y: i32) -> i64 {
    let y = i64::from(y);
    if y > 0 {
        (y + 999) / 1000
    } else {
        -((999 - (y - 1)) / 1000)
    }
}

/// Units derived from the calendar date alone. `Ok(None)` = not a date-derived unit.
fn ymd_value(u: Unit, y: i32, m: i32, d: i32) -> Result<Option<i64>> {
    Ok(Some(match u {
        Unit::Day => i64::from(d),
        Unit::Month => i64::from(m),
        Unit::Quarter => i64::from((m - 1) / 3 + 1),
        Unit::Week => i64::from(date2isoweek(y, m, d)?),
        Unit::Year => year_display(y),
        Unit::Decade => decade_of(y),
        Unit::Century => century_of(y),
        Unit::Millennium => millennium_of(y),
        Unit::Isoyear => {
            let v = i64::from(date2isoyear(y, m, d)?);
            if v <= 0 {
                v - 1
            } else {
                v
            }
        }
        Unit::Dow | Unit::Isodow => {
            let v = j2day(date2j(y, m, d));
            if u == Unit::Isodow && v == 0 {
                7
            } else {
                i64::from(v)
            }
        }
        Unit::Doy => i64::from(date2j(y, m, d) - date2j(y, 1, 1) + 1),
        _ => return Ok(None),
    }))
}

/// Time-of-day units (shared by time, timetz, timestamp, timestamptz).
fn hms_unit_value(u: Unit, hour: i64, min: i32, sec: i32, fsec: i32, numeric: bool) -> Option<Datum> {
    let sec_us = i64::from(sec) * USECS_PER_SEC + i64::from(fsec);
    Some(match u {
        Unit::Microsec => out_int(sec_us, numeric),
        Unit::Millisec => {
            if numeric {
                num_scaled(i128::from(sec_us), 3)
            } else {
                Datum::Float8(f64::from(sec) * 1000.0 + f64::from(fsec) / 1000.0)
            }
        }
        Unit::Second => {
            if numeric {
                num_scaled(i128::from(sec_us), 6)
            } else {
                Datum::Float8(f64::from(sec) + f64::from(fsec) / 1_000_000.0)
            }
        }
        Unit::Minute => out_int(i64::from(min), numeric),
        Unit::Hour => out_int(hour, numeric),
        _ => return None,
    })
}

/// Value of unit `u` for a broken-down timestamp (`None` = unit not supported).
fn ts_unit_value(u: Unit, tm: &Tm, numeric: bool) -> Result<Option<Datum>> {
    if let Some(d) = hms_unit_value(u, i64::from(tm.hour), tm.min, tm.sec, tm.fsec, numeric) {
        return Ok(Some(d));
    }
    if u == Unit::Julian {
        let j = date2j(tm.year, tm.mon, tm.mday);
        if numeric {
            let micros = ((i64::from(tm.hour) * 60 + i64::from(tm.min)) * 60 + i64::from(tm.sec)) * USECS_PER_SEC
                + i64::from(tm.fsec);
            let frac = arith::div(&Numeric::from_i64(micros), &Numeric::from_i64(USECS_PER_DAY))?;
            return Ok(Some(Datum::Numeric(arith::add(&Numeric::from_i64(i64::from(j)), &frac)?)));
        }
        let secs = ((i64::from(tm.hour) * 60 + i64::from(tm.min)) * 60 + i64::from(tm.sec)) as f64;
        return Ok(Some(Datum::Float8(
            f64::from(j) + (secs + f64::from(tm.fsec) / 1_000_000.0) / 86400.0,
        )));
    }
    Ok(ymd_value(u, tm.year, tm.mon, tm.mday)?.map(|v| out_int(v, numeric)))
}

/// `epoch` of a timestamp: `(ts - epoch) / 1e6` as numeric(…,6) or float8.
fn ts_epoch(ts: i64, numeric: bool) -> Datum {
    if numeric {
        num_scaled(i128::from(ts) - i128::from(EPOCH_TS), 6)
    } else if let Some(d) = ts.checked_sub(EPOCH_TS) {
        Datum::Float8(d as f64 / 1_000_000.0)
    } else {
        Datum::Float8((ts as f64 - EPOCH_TS as f64) / 1_000_000.0)
    }
}

/// `NonFiniteTimestampTzPart`: 0.0 (→ NULL) or ±Infinity.
fn non_finite_ts_part(dec: Decoded, low: &str, negative: bool, ty: &str) -> Result<f64> {
    let u = match dec {
        Decoded::Unknown => return Err(not_recognized(low, ty)),
        Decoded::ReservOther => return Err(not_supported(low, ty)),
        Decoded::Unit(u) => u,
    };
    match u {
        Unit::Microsec
        | Unit::Millisec
        | Unit::Second
        | Unit::Minute
        | Unit::Hour
        | Unit::Day
        | Unit::Month
        | Unit::Quarter
        | Unit::Week
        | Unit::Dow
        | Unit::Isodow
        | Unit::Doy
        | Unit::Tz
        | Unit::TzMinute
        | Unit::TzHour => Ok(0.0),
        Unit::Year
        | Unit::Decade
        | Unit::Century
        | Unit::Millennium
        | Unit::Julian
        | Unit::Isoyear
        | Unit::Epoch => Ok(if negative { f64::NEG_INFINITY } else { f64::INFINITY }),
    }
}

/// session-zone offset helper bundle.
#[derive(Clone, Copy)]
enum ZoneRef<'a> {
    Session(&'a TimeZoneSetting),
    /// Fixed offset in seconds EAST of UTC.
    Fixed(i32),
}

impl ZoneRef<'_> {
    fn east_at_utc(&self, ts: i64) -> Result<i32> {
        match self {
            ZoneRef::Session(s) => tz::offset_at_utc(s, ts),
            ZoneRef::Fixed(e) => Ok(*e),
        }
    }
    fn east_at_local(&self, local: i64) -> Result<i32> {
        match self {
            ZoneRef::Session(s) => tz::offset_at_local(s, local),
            ZoneRef::Fixed(e) => Ok(*e),
        }
    }
}

fn ts_part_impl(units: &str, ts: i64, ctx: &FuncCtx, numeric: bool, is_tz: bool) -> Result<Datum> {
    let ty = if is_tz { T_TSTZ } else { T_TS };
    let low = units.to_ascii_lowercase();
    let dec = decode_full(&low);
    if is_inf_ts(ts) {
        let r = non_finite_ts_part(dec, &low, ts == TIMESTAMP_NEG_INFINITY, ty)?;
        return Ok(inf_result(r, numeric));
    }
    let unit = match dec {
        Decoded::Unit(u) => u,
        Decoded::ReservOther => return Err(not_supported(&low, ty)),
        Decoded::Unknown => return Err(not_recognized(&low, ty)),
    };
    if unit == Unit::Epoch {
        return Ok(ts_epoch(ts, numeric));
    }
    let (west, east) = if is_tz {
        let e = tz::offset_at_utc(&ctx.io.time_zone, ts)?;
        (Some(-e), e)
    } else {
        (None, 0)
    };
    let tm = timestamp2tm(ts, west)?;
    if is_tz {
        match unit {
            Unit::Tz => return Ok(out_int(i64::from(east), numeric)),
            Unit::TzMinute => return Ok(out_int(i64::from((east / 60) % 60), numeric)),
            Unit::TzHour => return Ok(out_int(i64::from(east / 3600), numeric)),
            _ => {}
        }
    }
    match ts_unit_value(unit, &tm, numeric)? {
        Some(d) => Ok(d),
        None => Err(not_supported(&low, ty)),
    }
}

fn extract_timestamp(args: &[Datum], ctx: &FuncCtx) -> Result<Datum> {
    ts_part_impl(text_arg(args, 0)?, ts_arg(args, 1)?, ctx, true, false)
}

fn extract_timestamptz(args: &[Datum], ctx: &FuncCtx) -> Result<Datum> {
    ts_part_impl(text_arg(args, 0)?, ts_arg(args, 1)?, ctx, true, true)
}

fn timestamp_part(args: &[Datum], ctx: &FuncCtx) -> Result<Datum> {
    ts_part_impl(text_arg(args, 0)?, ts_arg(args, 1)?, ctx, false, false)
}

fn timestamptz_part(args: &[Datum], ctx: &FuncCtx) -> Result<Datum> {
    ts_part_impl(text_arg(args, 0)?, ts_arg(args, 1)?, ctx, false, true)
}

/// SQL-language `date_part(text, date)` = `date_part($1, $2::timestamp)`.
fn date_part_date(args: &[Datum], ctx: &FuncCtx) -> Result<Datum> {
    let ts = date_to_timestamp(date_arg(args, 1)?)?;
    ts_part_impl(text_arg(args, 0)?, ts, ctx, false, false)
}

fn extract_date(args: &[Datum], _ctx: &FuncCtx) -> Result<Datum> {
    let low = text_arg(args, 0)?.to_ascii_lowercase();
    let date = date_arg(args, 1)?;
    let dec = decode_full(&low);
    let ty = T_DATE;
    if (date == DATE_INFINITY || date == DATE_NEG_INFINITY) && dec != Decoded::Unknown {
        return match dec {
            Decoded::Unit(
                Unit::Day | Unit::Month | Unit::Quarter | Unit::Week | Unit::Dow | Unit::Isodow | Unit::Doy,
            ) => Ok(Datum::Null),
            Decoded::Unit(
                Unit::Year
                | Unit::Decade
                | Unit::Century
                | Unit::Millennium
                | Unit::Julian
                | Unit::Isoyear
                | Unit::Epoch,
            ) => Ok(Datum::Numeric(if date == DATE_NEG_INFINITY { Numeric::NegInf } else { Numeric::PosInf })),
            _ => Err(not_supported(&low, ty)),
        };
    }
    match dec {
        Decoded::Unit(Unit::Epoch) => {
            let days = i64::from(date) + i64::from(POSTGRES_EPOCH_JDATE) - i64::from(UNIX_EPOCH_JDATE);
            Ok(num_int(days * 86400))
        }
        Decoded::Unit(Unit::Julian) => Ok(num_int(i64::from(date) + i64::from(POSTGRES_EPOCH_JDATE))),
        Decoded::Unit(u) => {
            let (y, m, d) = j2date(date.wrapping_add(POSTGRES_EPOCH_JDATE));
            match ymd_value(u, y, m, d)? {
                Some(v) => Ok(num_int(v)),
                None => Err(not_supported(&low, ty)),
            }
        }
        Decoded::ReservOther => Err(not_supported(&low, ty)),
        Decoded::Unknown => Err(not_recognized(&low, ty)),
    }
}

fn time_part_impl(args: &[Datum], numeric: bool) -> Result<Datum> {
    let low = text_arg(args, 0)?.to_ascii_lowercase();
    let time = time_arg(args, 1)?;
    match decode_full(&low) {
        Decoded::Unit(Unit::Epoch) => Ok(if numeric {
            num_scaled(i128::from(time), 6)
        } else {
            Datum::Float8(time as f64 / 1_000_000.0)
        }),
        Decoded::Unit(u) => {
            let tm = time2tm(time)?;
            match hms_unit_value(u, i64::from(tm.hour), tm.min, tm.sec, tm.fsec, numeric) {
                Some(d) => Ok(d),
                None => Err(not_supported(&low, T_TIME)),
            }
        }
        _ => Err(not_recognized(&low, T_TIME)),
    }
}

fn extract_time(args: &[Datum], _ctx: &FuncCtx) -> Result<Datum> {
    time_part_impl(args, true)
}

fn time_part(args: &[Datum], _ctx: &FuncCtx) -> Result<Datum> {
    time_part_impl(args, false)
}

fn timetz_part_impl(args: &[Datum], numeric: bool) -> Result<Datum> {
    let low = text_arg(args, 0)?.to_ascii_lowercase();
    let (time, zone) = timetz_arg(args, 1)?;
    match decode_full(&low) {
        Decoded::Unit(Unit::Epoch) => Ok(if numeric {
            num_scaled(i128::from(time) + i128::from(zone) * i128::from(USECS_PER_SEC), 6)
        } else {
            Datum::Float8(time as f64 / 1_000_000.0 + f64::from(zone))
        }),
        Decoded::Unit(u) => {
            let tm = time2tm(time)?;
            match u {
                Unit::Tz => Ok(out_int(-i64::from(zone), numeric)),
                Unit::TzMinute => Ok(out_int(i64::from((-zone / 60) % 60), numeric)),
                Unit::TzHour => Ok(out_int(i64::from(-zone / 3600), numeric)),
                _ => match hms_unit_value(u, i64::from(tm.hour), tm.min, tm.sec, tm.fsec, numeric) {
                    Some(d) => Ok(d),
                    None => Err(not_supported(&low, T_TIMETZ)),
                },
            }
        }
        _ => Err(not_recognized(&low, T_TIMETZ)),
    }
}

fn extract_timetz(args: &[Datum], _ctx: &FuncCtx) -> Result<Datum> {
    timetz_part_impl(args, true)
}

fn timetz_part(args: &[Datum], _ctx: &FuncCtx) -> Result<Datum> {
    timetz_part_impl(args, false)
}

/// `NonFiniteIntervalPart`.
fn non_finite_interval_part(dec: Decoded, low: &str, negative: bool) -> Result<f64> {
    let u = match dec {
        Decoded::Unknown => return Err(not_recognized(low, T_INTERVAL)),
        Decoded::ReservOther => return Err(not_supported(low, T_INTERVAL)),
        Decoded::Unit(u) => u,
    };
    match u {
        Unit::Microsec | Unit::Millisec | Unit::Second | Unit::Minute | Unit::Week | Unit::Month | Unit::Quarter => {
            Ok(0.0)
        }
        Unit::Hour
        | Unit::Day
        | Unit::Year
        | Unit::Decade
        | Unit::Century
        | Unit::Millennium
        | Unit::Epoch => Ok(if negative { f64::NEG_INFINITY } else { f64::INFINITY }),
        _ => Err(not_supported(low, T_INTERVAL)),
    }
}

fn interval_part_impl(args: &[Datum], numeric: bool) -> Result<Datum> {
    let low = text_arg(args, 0)?.to_ascii_lowercase();
    let iv = interval_arg(args, 1)?;
    let dec = decode_full(&low);
    if iv.is_infinite() {
        let r = non_finite_interval_part(dec, &low, iv == Interval::NEG_INFINITY)?;
        return Ok(inf_result(r, numeric));
    }
    let u = match dec {
        Decoded::Unit(u) => u,
        _ => return Err(not_recognized(&low, T_INTERVAL)),
    };
    if u == Unit::Epoch {
        let months = i64::from(iv.months);
        let days = i64::from(iv.days);
        if numeric {
            let secs = (1461 * (months / 12) + 120 * (months % 12) + 4 * days) * 21600;
            return Ok(num_scaled(i128::from(secs) * i128::from(USECS_PER_SEC) + i128::from(iv.micros), 6));
        }
        let mut r = iv.micros as f64 / 1_000_000.0;
        r += (365.25 * 86400.0) * (months / 12) as f64;
        r += (30.0 * 86400.0) * (months % 12) as f64;
        r += 86400.0 * days as f64;
        return Ok(Datum::Float8(r));
    }
    let tm = interval2itm(&iv)?;
    if let Some(d) = hms_unit_value(u, tm.hour, tm.min, tm.sec, tm.usec, numeric) {
        return Ok(d);
    }
    let v: i64 = match u {
        Unit::Day => i64::from(tm.mday),
        Unit::Week => i64::from(tm.mday / 7),
        Unit::Month => i64::from(tm.mon),
        Unit::Quarter => {
            if iv.months >= 0 {
                i64::from(tm.mon / 3 + 1)
            } else {
                -(((-i64::from(iv.months) % 12) / 3) + 1)
            }
        }
        Unit::Year => i64::from(tm.year),
        Unit::Decade => i64::from(tm.year / 10),
        Unit::Century => i64::from(tm.year / 100),
        Unit::Millennium => i64::from(tm.year / 1000),
        _ => return Err(not_supported(&low, T_INTERVAL)),
    };
    Ok(out_int(v, numeric))
}

fn extract_interval(args: &[Datum], _ctx: &FuncCtx) -> Result<Datum> {
    interval_part_impl(args, true)
}

fn interval_part(args: &[Datum], _ctx: &FuncCtx) -> Result<Datum> {
    interval_part_impl(args, false)
}

// ---------------------------------------------------------------------------------------------
// date_trunc
// ---------------------------------------------------------------------------------------------

/// Truncation level (coarse to fine): millennium 0 ... second 9. `None` for other units.
fn trunc_level(u: Unit) -> Option<u8> {
    Some(match u {
        Unit::Millennium => 0,
        Unit::Century => 1,
        Unit::Decade => 2,
        Unit::Year => 3,
        Unit::Quarter => 4,
        Unit::Month => 5,
        Unit::Day => 6,
        Unit::Hour => 7,
        Unit::Minute => 8,
        Unit::Second => 9,
        _ => return None,
    })
}

/// Units for which an infinite timestamp is returned unchanged.
fn trunc_passes_infinity(u: Unit) -> bool {
    u == Unit::Week || u == Unit::Millisec || u == Unit::Microsec || trunc_level(u).is_some()
}

/// Truncate the broken-down timestamp in place. Returns `redotz` (the zone offset must be re-derived
/// from the truncated local time). Unsupported units raise 0A000 naming `ty`.
fn trunc_tm(unit: Unit, tm: &mut Tm, low: &str, ty: &str) -> Result<bool> {
    if unit == Unit::Week {
        let woy = date2isoweek(tm.year, tm.mon, tm.mday)?;
        // Week 52/53 in January belongs to the previous year; week 1 in December to the next.
        if woy >= 52 && tm.mon == 1 {
            tm.year -= 1;
        }
        if woy <= 1 && tm.mon == 12 {
            tm.year += 1;
        }
        let (y, m, d) = j2date(isoweek2j(tm.year, woy)?);
        tm.year = y;
        tm.mon = m;
        tm.mday = d;
        tm.hour = 0;
        tm.min = 0;
        tm.sec = 0;
        tm.fsec = 0;
        return Ok(true);
    }
    if unit == Unit::Millisec {
        tm.fsec = (tm.fsec / 1000) * 1000;
        return Ok(false);
    }
    if unit == Unit::Microsec {
        return Ok(false);
    }
    let level = match trunc_level(unit) {
        Some(l) => l,
        None => return Err(not_supported(low, ty)),
    };
    let y = i64::from(tm.year);
    if level == 0 {
        tm.year = if y > 0 { (((y + 999) / 1000) * 1000 - 999) as i32 } else { (-((999 - (y - 1)) / 1000) * 1000 + 1) as i32 };
    }
    let y = i64::from(tm.year);
    if level <= 1 {
        tm.year = if y > 0 { (((y + 99) / 100) * 100 - 99) as i32 } else { (-((99 - (y - 1)) / 100) * 100 + 1) as i32 };
    }
    let y = i64::from(tm.year);
    if level == 2 {
        tm.year = if y > 0 { ((y / 10) * 10) as i32 } else { (-((8 - (y - 1)) / 10) * 10) as i32 };
    }
    if level <= 3 {
        tm.mon = 1;
    }
    if level <= 4 {
        tm.mon = 3 * ((tm.mon - 1) / 3) + 1;
    }
    if level <= 5 {
        tm.mday = 1;
    }
    let mut redotz = false;
    if level <= 6 {
        tm.hour = 0;
        redotz = true;
    }
    if level <= 7 {
        tm.min = 0;
    }
    if level <= 8 {
        tm.sec = 0;
    }
    tm.fsec = 0;
    Ok(redotz)
}

fn timestamp_trunc(args: &[Datum], _ctx: &FuncCtx) -> Result<Datum> {
    let low = text_arg(args, 0)?.to_ascii_lowercase();
    let ts = ts_arg(args, 1)?;
    let unit = match decode_delta(&low) {
        Some(u) => u,
        None => return Err(not_recognized(&low, T_TS)),
    };
    if is_inf_ts(ts) {
        return if trunc_passes_infinity(unit) {
            Ok(Datum::Timestamp(ts))
        } else {
            Err(not_supported(&low, T_TS))
        };
    }
    let mut tm = timestamp2tm(ts, None)?;
    trunc_tm(unit, &mut tm, &low, T_TS)?;
    Ok(Datum::Timestamp(tm2timestamp(&tm, None)?))
}

fn tstz_trunc(units: &str, ts: i64, zone: ZoneRef) -> Result<i64> {
    let low = units.to_ascii_lowercase();
    let unit = match decode_delta(&low) {
        Some(u) => u,
        None => return Err(not_recognized(&low, T_TSTZ)),
    };
    if is_inf_ts(ts) {
        return if trunc_passes_infinity(unit) { Ok(ts) } else { Err(not_supported(&low, T_TSTZ)) };
    }
    let east = zone.east_at_utc(ts)?;
    let mut west = -east;
    let mut tm = timestamp2tm(ts, Some(west))?;
    let redotz = trunc_tm(unit, &mut tm, &low, T_TSTZ)?;
    if redotz {
        let local = tm2timestamp(&tm, None)?;
        west = -zone.east_at_local(local)?;
    }
    tm2timestamp(&tm, Some(west))
}

fn timestamptz_trunc(args: &[Datum], ctx: &FuncCtx) -> Result<Datum> {
    let r = tstz_trunc(text_arg(args, 0)?, ts_arg(args, 1)?, ZoneRef::Session(&ctx.io.time_zone))?;
    Ok(Datum::TimestampTz(r))
}

fn timestamptz_trunc_zone(args: &[Datum], _ctx: &FuncCtx) -> Result<Datum> {
    let east = resolve_zone_name(text_arg(args, 2)?)?;
    let r = tstz_trunc(text_arg(args, 0)?, ts_arg(args, 1)?, ZoneRef::Fixed(east))?;
    Ok(Datum::TimestampTz(r))
}

fn interval_trunc(args: &[Datum], _ctx: &FuncCtx) -> Result<Datum> {
    let low = text_arg(args, 0)?.to_ascii_lowercase();
    let iv = interval_arg(args, 1)?;
    let unit = match decode_delta(&low) {
        Some(u) => u,
        None => return Err(not_recognized(&low, T_INTERVAL)),
    };
    let unsupported = |low: &str, unit: Unit| {
        let e = not_supported(low, T_INTERVAL);
        if unit == Unit::Week {
            e.with_detail("Months usually have fractional weeks.")
        } else {
            e
        }
    };
    if iv.is_infinite() {
        let ok = unit != Unit::Week && (unit == Unit::Millisec || unit == Unit::Microsec || trunc_level(unit).is_some());
        return if ok { Ok(Datum::Interval(iv)) } else { Err(unsupported(&low, unit)) };
    }
    let mut tm = interval2itm(&iv)?;
    match unit {
        Unit::Millisec => tm.usec = (tm.usec / 1000) * 1000,
        Unit::Microsec => {}
        _ => {
            let level = match trunc_level(unit) {
                Some(l) => l,
                None => return Err(unsupported(&low, unit)),
            };
            if level == 0 {
                tm.year = (tm.year / 1000) * 1000;
            }
            if level <= 1 {
                tm.year = (tm.year / 100) * 100;
            }
            if level <= 2 {
                tm.year = (tm.year / 10) * 10;
            }
            if level <= 3 {
                tm.mon = 0;
            }
            if level <= 4 {
                tm.mon = 3 * (tm.mon / 3);
            }
            if level <= 5 {
                tm.mday = 0;
            }
            if level <= 6 {
                tm.hour = 0;
            }
            if level <= 7 {
                tm.min = 0;
            }
            if level <= 8 {
                tm.sec = 0;
            }
            tm.usec = 0;
        }
    }
    Ok(Datum::Interval(itm2interval(&tm)?))
}

// ---------------------------------------------------------------------------------------------
// date_bin
// ---------------------------------------------------------------------------------------------

fn bin_impl(stride: &Interval, ts: i64, origin: i64) -> Result<i64> {
    if is_inf_ts(ts) {
        return Ok(ts);
    }
    if is_inf_ts(origin) {
        return Err(PgError::new(SqlState::DATETIME_VALUE_OUT_OF_RANGE, "origin out of range"));
    }
    if stride.is_infinite() {
        return Err(PgError::new(
            SqlState::DATETIME_VALUE_OUT_OF_RANGE,
            "timestamps cannot be binned into infinite intervals",
        ));
    }
    if stride.months != 0 {
        return Err(PgError::new(
            SqlState::FEATURE_NOT_SUPPORTED,
            "timestamps cannot be binned into intervals containing months or years",
        ));
    }
    let stride_usecs = i64::from(stride.days)
        .checked_mul(USECS_PER_DAY)
        .and_then(|d| d.checked_add(stride.micros))
        .ok_or_else(interval_out_of_range)?;
    if stride_usecs <= 0 {
        return Err(PgError::new(SqlState::DATETIME_VALUE_OUT_OF_RANGE, "stride must be greater than zero"));
    }
    let tm_diff = ts.checked_sub(origin).ok_or_else(interval_out_of_range)?;
    let tm_modulo = tm_diff % stride_usecs;
    let tm_delta = tm_diff - tm_modulo;
    let mut result = origin + tm_delta;
    if tm_modulo < 0 {
        result = result.checked_sub(stride_usecs).ok_or_else(ts_out_of_range)?;
        valid_ts(result)?;
    }
    Ok(result)
}

fn timestamp_bin(args: &[Datum], _ctx: &FuncCtx) -> Result<Datum> {
    Ok(Datum::Timestamp(bin_impl(&interval_arg(args, 0)?, ts_arg(args, 1)?, ts_arg(args, 2)?)?))
}

fn timestamptz_bin(args: &[Datum], _ctx: &FuncCtx) -> Result<Datum> {
    Ok(Datum::TimestampTz(bin_impl(&interval_arg(args, 0)?, ts_arg(args, 1)?, ts_arg(args, 2)?)?))
}

// ---------------------------------------------------------------------------------------------
// age
// ---------------------------------------------------------------------------------------------

/// `timestamp_age` / `timestamptz_age` core: `w1`/`w2` are the zone offsets (seconds WEST) used to
/// break the two instants into fields (None = no zone).
fn age_core(dt1: i64, dt2: i64, w1: Option<i32>, w2: Option<i32>) -> Result<Interval> {
    if dt1 == TIMESTAMP_NEG_INFINITY {
        return if dt2 == TIMESTAMP_NEG_INFINITY { Err(interval_out_of_range()) } else { Ok(Interval::NEG_INFINITY) };
    }
    if dt1 == TIMESTAMP_INFINITY {
        return if dt2 == TIMESTAMP_INFINITY { Err(interval_out_of_range()) } else { Ok(Interval::INFINITY) };
    }
    if dt2 == TIMESTAMP_NEG_INFINITY {
        return Ok(Interval::INFINITY);
    }
    if dt2 == TIMESTAMP_INFINITY {
        return Ok(Interval::NEG_INFINITY);
    }
    let tm1 = timestamp2tm(dt1, w1)?;
    let tm2 = timestamp2tm(dt2, w2)?;
    let neg = dt1 < dt2;
    let mut usec = tm1.fsec - tm2.fsec;
    let mut sec = tm1.sec - tm2.sec;
    let mut min = tm1.min - tm2.min;
    let mut hour = tm1.hour - tm2.hour;
    let mut mday = tm1.mday - tm2.mday;
    let mut mon = tm1.mon - tm2.mon;
    let mut year = tm1.year - tm2.year;
    if neg {
        usec = -usec;
        sec = -sec;
        min = -min;
        hour = -hour;
        mday = -mday;
        mon = -mon;
        year = -year;
    }
    while usec < 0 {
        usec += 1_000_000;
        sec -= 1;
    }
    while sec < 0 {
        sec += 60;
        min -= 1;
    }
    while min < 0 {
        min += 60;
        hour -= 1;
    }
    while hour < 0 {
        hour += 24;
        mday -= 1;
    }
    while mday < 0 {
        if neg {
            mday += days_in_month(tm1.year, tm1.mon);
        } else {
            mday += days_in_month(tm2.year, tm2.mon);
        }
        mon -= 1;
    }
    while mon < 0 {
        mon += 12;
        year -= 1;
    }
    if neg {
        usec = -usec;
        sec = -sec;
        min = -min;
        hour = -hour;
        mday = -mday;
        mon = -mon;
        year = -year;
    }
    itm2interval(&Itm { year, mon, mday, hour: i64::from(hour), min, sec, usec })
}

fn timestamp_age(args: &[Datum], _ctx: &FuncCtx) -> Result<Datum> {
    Ok(Datum::Interval(age_core(ts_arg(args, 0)?, ts_arg(args, 1)?, None, None)?))
}

/// Zone offset (seconds WEST) of a finite instant in the session zone; None for infinities.
fn session_west(ctx: &FuncCtx, ts: i64) -> Result<Option<i32>> {
    if is_inf_ts(ts) {
        return Ok(None);
    }
    Ok(Some(-tz::offset_at_utc(&ctx.io.time_zone, ts)?))
}

fn timestamptz_age(args: &[Datum], ctx: &FuncCtx) -> Result<Datum> {
    let dt1 = ts_arg(args, 0)?;
    let dt2 = ts_arg(args, 1)?;
    let w1 = session_west(ctx, dt1)?;
    let w2 = session_west(ctx, dt2)?;
    Ok(Datum::Interval(age_core(dt1, dt2, w1, w2)?))
}

/// Midnight of `current_date` (transaction start, session zone) as a local timestamp.
fn current_date_midnight(ctx: &FuncCtx) -> Result<i64> {
    let now = ctx.session.transaction_timestamp();
    let east = tz::offset_at_utc(&ctx.io.time_zone, now)?;
    let local = now.checked_add(i64::from(east) * USECS_PER_SEC).ok_or_else(ts_out_of_range)?;
    Ok(local.div_euclid(USECS_PER_DAY) * USECS_PER_DAY)
}

/// SQL-language `age(timestamp)` = `age(current_date::timestamp, $1)`.
fn age_timestamp_now(args: &[Datum], ctx: &FuncCtx) -> Result<Datum> {
    let today = current_date_midnight(ctx)?;
    Ok(Datum::Interval(age_core(today, ts_arg(args, 0)?, None, None)?))
}

/// SQL-language `age(timestamptz)` = `age(current_date::timestamptz, $1)`.
fn age_timestamptz_now(args: &[Datum], ctx: &FuncCtx) -> Result<Datum> {
    let midnight = current_date_midnight(ctx)?;
    let east = tz::offset_at_local(&ctx.io.time_zone, midnight)?;
    let dt1 = dt2local(midnight, i64::from(east))?;
    let dt2 = ts_arg(args, 0)?;
    let w1 = session_west(ctx, dt1)?;
    let w2 = session_west(ctx, dt2)?;
    Ok(Datum::Interval(age_core(dt1, dt2, w1, w2)?))
}

// ---------------------------------------------------------------------------------------------
// make_interval / make_timestamp / make_timestamptz
// ---------------------------------------------------------------------------------------------

fn make_interval(args: &[Datum], _ctx: &FuncCtx) -> Result<Datum> {
    let years = int4_arg(args, 0)?;
    let months = int4_arg(args, 1)?;
    let weeks = int4_arg(args, 2)?;
    let days = int4_arg(args, 3)?;
    let hours = int4_arg(args, 4)?;
    let mins = int4_arg(args, 5)?;
    let secs = float8_arg(args, 6)?;
    if secs.is_nan() || secs.is_infinite() {
        return Err(interval_out_of_range());
    }
    let month = years
        .checked_mul(12)
        .and_then(|m| m.checked_add(months))
        .ok_or_else(interval_out_of_range)?;
    let day = weeks
        .checked_mul(7)
        .and_then(|d| d.checked_add(days))
        .ok_or_else(interval_out_of_range)?;
    let base = i64::from(hours) * USECS_PER_HOUR + i64::from(mins) * USECS_PER_MINUTE;
    let scaled = secs * USECS_PER_SEC as f64;
    if scaled.is_infinite() {
        return Err(PgError::new(SqlState::NUMERIC_VALUE_OUT_OF_RANGE, "value out of range: overflow"));
    }
    let scaled = scaled.round_ties_even();
    if !(scaled >= -9_223_372_036_854_775_808.0 && scaled < 9_223_372_036_854_775_808.0) {
        return Err(interval_out_of_range());
    }
    let micros = base.checked_add(scaled as i64).ok_or_else(interval_out_of_range)?;
    let iv = Interval::new(month, day, micros);
    if iv.is_infinite() {
        return Err(interval_out_of_range());
    }
    Ok(Datum::Interval(iv))
}

/// C `printf("%g")`.
fn fmt_g(x: f64) -> String {
    if x.is_nan() {
        return "nan".to_string();
    }
    if x.is_infinite() {
        return if x < 0.0 { "-inf".to_string() } else { "inf".to_string() };
    }
    if x == 0.0 {
        return if x.is_sign_negative() { "-0".to_string() } else { "0".to_string() };
    }
    fn strip(s: &str) -> String {
        if s.contains('.') {
            s.trim_end_matches('0').trim_end_matches('.').to_string()
        } else {
            s.to_string()
        }
    }
    let sci = format!("{:.5e}", x);
    let (mant, exp) = match sci.split_once('e') {
        Some((m, e)) => (m.to_string(), e.parse::<i32>().unwrap_or(0)),
        None => (sci.clone(), 0),
    };
    if !(-4..6).contains(&exp) {
        format!("{}e{}{:02}", strip(&mant), if exp < 0 { '-' } else { '+' }, exp.abs())
    } else {
        let prec = (5 - exp).max(0) as usize;
        strip(&format!("{:.*}", prec, x))
    }
}

/// `%02g`.
fn fmt_g2(x: f64) -> String {
    let s = fmt_g(x);
    if s.len() < 2 {
        format!("0{s}")
    } else {
        s
    }
}

/// `float_time_overflows`.
fn float_time_overflows(hour: i32, min: i32, sec: f64) -> bool {
    if !(0..=24).contains(&hour) || !(0..60).contains(&min) {
        return true;
    }
    if sec.is_nan() {
        return true;
    }
    let us = (sec * USECS_PER_SEC as f64).round_ties_even();
    if us < 0.0 || us > 60.0 * USECS_PER_SEC as f64 {
        return true;
    }
    ((i64::from(hour) * 60 + i64::from(min)) * 60 * USECS_PER_SEC + us as i64) > USECS_PER_DAY
}

fn make_timestamp_internal(year: i32, month: i32, day: i32, hour: i32, min: i32, sec: f64) -> Result<i64> {
    let date_err = || {
        PgError::new(
            SqlState::DATETIME_FIELD_OVERFLOW,
            format!("date field value out of range: {year}-{month:02}-{day:02}"),
        )
    };
    // Negative years are BC.
    let (mut y, bc) = if year < 0 { (year.checked_neg().ok_or_else(date_err)?, true) } else { (year, false) };
    if bc {
        if y <= 0 {
            return Err(date_err());
        }
        y = -(y - 1);
    } else if y < 1 {
        return Err(date_err());
    }
    if !(1..=12).contains(&month) || !(1..=31).contains(&day) || day > days_in_month(y, month) {
        return Err(date_err());
    }
    // IS_VALID_JULIAN
    let valid_julian = (y > -4713 || (y == -4713 && month >= 11)) && y < 5874898;
    if !valid_julian {
        return Err(PgError::new(
            SqlState::DATETIME_VALUE_OUT_OF_RANGE,
            format!("date out of range: {year}-{month:02}-{day:02}"),
        ));
    }
    let date = i64::from(date2j(y, month, day)) - i64::from(POSTGRES_EPOCH_JDATE);
    if float_time_overflows(hour, min, sec) {
        return Err(PgError::new(
            SqlState::DATETIME_FIELD_OVERFLOW,
            format!("time field value out of range: {hour}:{min:02}:{}", fmt_g2(sec)),
        ));
    }
    let time = (i64::from(hour) * 60 + i64::from(min)) * 60 * USECS_PER_SEC
        + (sec * USECS_PER_SEC as f64).round_ties_even() as i64;
    let oor = || {
        PgError::new(
            SqlState::DATETIME_VALUE_OUT_OF_RANGE,
            format!("timestamp out of range: {year}-{month:02}-{day:02} {hour}:{min:02}:{}", fmt_g2(sec)),
        )
    };
    let result = date.checked_mul(USECS_PER_DAY).and_then(|d| d.checked_add(time)).ok_or_else(oor)?;
    if !(MIN_TIMESTAMP..END_TIMESTAMP).contains(&result) {
        return Err(oor());
    }
    Ok(result)
}

fn make_ts_from_args(args: &[Datum]) -> Result<i64> {
    make_timestamp_internal(
        int4_arg(args, 0)?,
        int4_arg(args, 1)?,
        int4_arg(args, 2)?,
        int4_arg(args, 3)?,
        int4_arg(args, 4)?,
        float8_arg(args, 5)?,
    )
}

fn make_timestamp(args: &[Datum], _ctx: &FuncCtx) -> Result<Datum> {
    Ok(Datum::Timestamp(make_ts_from_args(args)?))
}

/// `timestamp2timestamptz`: local wall clock in the session zone → UTC instant.
fn local_to_utc_session(ctx: &FuncCtx, local: i64) -> Result<i64> {
    if is_inf_ts(local) {
        return Ok(local);
    }
    let east = tz::offset_at_local(&ctx.io.time_zone, local)?;
    dt2local(local, i64::from(east))
}

fn make_timestamptz(args: &[Datum], ctx: &FuncCtx) -> Result<Datum> {
    let local = make_ts_from_args(args)?;
    Ok(Datum::TimestampTz(local_to_utc_session(ctx, local)?))
}

fn make_timestamptz_at_timezone(args: &[Datum], _ctx: &FuncCtx) -> Result<Datum> {
    let local = make_ts_from_args(args)?;
    let zone = text_arg(args, 6)?;
    let east = parse_sane_timezone(zone)?;
    Ok(Datum::TimestampTz(dt2local(local, i64::from(east))?))
}

/// `parse_sane_timezone`: numeric offset (ISO sign) or zone name/abbreviation → seconds EAST.
fn parse_sane_timezone(name: &str) -> Result<i32> {
    if name.as_bytes().first().is_some_and(|b| b.is_ascii_digit()) {
        return Err(PgError::new(
            SqlState::INVALID_PARAMETER_VALUE,
            format!("invalid input syntax for type numeric time zone: \"{name}\""),
        )
        .with_hint("Numeric time zones must have \"-\" or \"+\" as first character."));
    }
    match tz::parse_numeric_offset(name)? {
        Ok(east) => Ok(east),
        Err(DtErr::TzDispOverflow) => Err(PgError::new(
            SqlState::INVALID_PARAMETER_VALUE,
            format!("numeric time zone \"{name}\" out of range"),
        )),
        Err(DtErr::BadFormat) => resolve_zone_name(name),
        Err(_) => Err(bad_zone(name)),
    }
}

// ---------------------------------------------------------------------------------------------
// now(), to_timestamp(double)
// ---------------------------------------------------------------------------------------------

fn now(_args: &[Datum], ctx: &FuncCtx) -> Result<Datum> {
    Ok(Datum::TimestampTz(ctx.session.transaction_timestamp()))
}

fn statement_timestamp(_args: &[Datum], ctx: &FuncCtx) -> Result<Datum> {
    Ok(Datum::TimestampTz(ctx.session.statement_timestamp()))
}

fn clock_timestamp(_args: &[Datum], ctx: &FuncCtx) -> Result<Datum> {
    Ok(Datum::TimestampTz(ctx.session.clock_timestamp()))
}

fn float8_timestamptz(args: &[Datum], _ctx: &FuncCtx) -> Result<Datum> {
    let seconds = float8_arg(args, 0)?;
    if seconds.is_nan() {
        return Err(PgError::new(SqlState::DATETIME_VALUE_OUT_OF_RANGE, "timestamp cannot be NaN"));
    }
    if seconds.is_infinite() {
        return Ok(Datum::TimestampTz(if seconds < 0.0 { TIMESTAMP_NEG_INFINITY } else { TIMESTAMP_INFINITY }));
    }
    let oor = || {
        PgError::new(SqlState::DATETIME_VALUE_OUT_OF_RANGE, format!("timestamp out of range: \"{}\"", fmt_g(seconds)))
    };
    let lo = 86400.0 * (0 - UNIX_EPOCH_JDATE) as f64;
    let hi = 86400.0 * (TIMESTAMP_END_JULIAN - UNIX_EPOCH_JDATE) as f64;
    if seconds < lo || seconds >= hi {
        return Err(oor());
    }
    let shifted = seconds - f64::from((POSTGRES_EPOCH_JDATE - UNIX_EPOCH_JDATE) * 86400);
    let result = (shifted * USECS_PER_SEC as f64).round_ties_even() as i64;
    if !(MIN_TIMESTAMP..END_TIMESTAMP).contains(&result) {
        return Err(oor());
    }
    Ok(Datum::TimestampTz(result))
}

// ---------------------------------------------------------------------------------------------
// Time zone name resolution (AT TIME ZONE)
// ---------------------------------------------------------------------------------------------

fn bad_zone(name: &str) -> PgError {
    PgError::new(SqlState::INVALID_PARAMETER_VALUE, format!("time zone \"{name}\" not recognized"))
}

fn unsupported_zone(name: &str) -> PgError {
    dt_parse_error(&DtErr::UnsupportedTimezone(name.to_string()), "", "")
}

/// POSIX-style bare offset `[+-]h[h][:mm[:ss]]` (POSIX sign: `+05` is five hours WEST).
/// Returns seconds EAST.
fn posix_numeric_east(name: &str) -> Option<i32> {
    let (sign_west, rest) = match *name.as_bytes().first()? {
        b'+' => (1i32, &name[1..]),
        b'-' => (-1i32, &name[1..]),
        _ => (1i32, name),
    };
    if rest.is_empty() {
        return None;
    }
    let mut fields = [0i32; 3];
    let mut n = 0;
    for part in rest.split(':') {
        if n >= 3 || part.is_empty() || part.len() > 2 || !part.bytes().all(|b| b.is_ascii_digit()) {
            return None;
        }
        fields[n] = part.parse::<i32>().ok()?;
        n += 1;
    }
    if fields[0] > 24 || fields[1] > 59 || fields[2] > 59 {
        return None;
    }
    let west = sign_west * (fields[0] * 3600 + fields[1] * 60 + fields[2]);
    Some(-west)
}

/// Resolve a zone name / abbreviation / POSIX spec to a fixed offset in seconds EAST.
/// IANA region names (no tz rules shipped) fail with 0A000.
fn resolve_zone_name(name: &str) -> Result<i32> {
    let lower = name.to_ascii_lowercase();
    match tz::lookup_zone(&lower)? {
        Ok(ZoneSpec::Fixed { secs_east, .. }) => return Ok(secs_east),
        Ok(ZoneSpec::Region(n)) => return Err(unsupported_zone(&n)),
        Err(DtErr::UnsupportedTimezone(n)) => return Err(unsupported_zone(&n)),
        Err(_) => {}
    }
    if let Some(east) = posix_numeric_east(name) {
        return Ok(east);
    }
    // POSIX specs with a leading name, e.g. `UTC+3`.
    let first_alpha = name.as_bytes().first().is_some_and(|b| b.is_ascii_alphabetic());
    if first_alpha && name.bytes().any(|b| b.is_ascii_digit()) && !lower.starts_with("interval") {
        if let Ok(setting) = tz::parse_timezone_setting(name) {
            return match setting {
                TimeZoneSetting::Utc => Ok(0),
                TimeZoneSetting::Fixed { secs_east, .. } => Ok(secs_east),
                TimeZoneSetting::Named(n) => Err(unsupported_zone(&n)),
            };
        }
    }
    Err(bad_zone(name))
}

/// Seconds of a finite, month/day-free interval used as a time zone (ISO sign: positive = east).
fn izone_secs(iv: &Interval, ctx: &FuncCtx) -> Result<i64> {
    let shown = || interval::output(&ctx.io_ctx(), TypeId::INTERVAL, &Datum::Interval(*iv)).unwrap_or_default();
    if iv.is_infinite() {
        return Err(PgError::new(
            SqlState::INVALID_PARAMETER_VALUE,
            format!("interval time zone \"{}\" must be finite", shown()),
        ));
    }
    if iv.months != 0 || iv.days != 0 {
        return Err(PgError::new(
            SqlState::INVALID_PARAMETER_VALUE,
            format!("interval time zone \"{}\" must not include months or days", shown()),
        ));
    }
    Ok(iv.micros / USECS_PER_SEC)
}

fn timestamp_zone(args: &[Datum], _ctx: &FuncCtx) -> Result<Datum> {
    let ts = ts_arg(args, 1)?;
    if is_inf_ts(ts) {
        return Ok(Datum::TimestampTz(ts));
    }
    let east = resolve_zone_name(text_arg(args, 0)?)?;
    Ok(Datum::TimestampTz(dt2local(ts, i64::from(east))?))
}

fn timestamp_izone(args: &[Datum], ctx: &FuncCtx) -> Result<Datum> {
    let iv = interval_arg(args, 0)?;
    let ts = ts_arg(args, 1)?;
    if is_inf_ts(ts) {
        return Ok(Datum::TimestampTz(ts));
    }
    let east = izone_secs(&iv, ctx)?;
    Ok(Datum::TimestampTz(dt2local(ts, east)?))
}

fn timestamptz_zone(args: &[Datum], _ctx: &FuncCtx) -> Result<Datum> {
    let ts = ts_arg(args, 1)?;
    if is_inf_ts(ts) {
        return Ok(Datum::Timestamp(ts));
    }
    let east = resolve_zone_name(text_arg(args, 0)?)?;
    Ok(Datum::Timestamp(dt2local(ts, -i64::from(east))?))
}

fn timestamptz_izone(args: &[Datum], ctx: &FuncCtx) -> Result<Datum> {
    let iv = interval_arg(args, 0)?;
    let ts = ts_arg(args, 1)?;
    if is_inf_ts(ts) {
        return Ok(Datum::Timestamp(ts));
    }
    let east = izone_secs(&iv, ctx)?;
    Ok(Datum::Timestamp(dt2local(ts, -east)?))
}

/// New time-of-day and zone for `timetz AT TIME ZONE` (both zones in seconds WEST).
fn timetz_shift(time: i64, old_west: i32, new_west: i64) -> Datum {
    let shifted = i128::from(time) + (i128::from(old_west) - i128::from(new_west)) * i128::from(USECS_PER_SEC);
    let t = shifted.rem_euclid(i128::from(USECS_PER_DAY)) as i64;
    // Zones are bounded well inside i32 for any sane input; clamp defensively.
    let z = new_west.clamp(i64::from(i32::MIN), i64::from(i32::MAX)) as i32;
    Datum::TimeTz(t, z)
}

fn timetz_zone(args: &[Datum], _ctx: &FuncCtx) -> Result<Datum> {
    let (time, zone) = timetz_arg(args, 1)?;
    let east = resolve_zone_name(text_arg(args, 0)?)?;
    Ok(timetz_shift(time, zone, -i64::from(east)))
}

fn timetz_izone(args: &[Datum], ctx: &FuncCtx) -> Result<Datum> {
    let iv = interval_arg(args, 0)?;
    let (time, zone) = timetz_arg(args, 1)?;
    let east = izone_secs(&iv, ctx)?;
    Ok(timetz_shift(time, zone, -east))
}

fn timestamp_at_local(args: &[Datum], ctx: &FuncCtx) -> Result<Datum> {
    let ts = ts_arg(args, 0)?;
    Ok(Datum::TimestampTz(local_to_utc_session(ctx, ts)?))
}

fn timestamptz_at_local(args: &[Datum], ctx: &FuncCtx) -> Result<Datum> {
    let ts = ts_arg(args, 0)?;
    if is_inf_ts(ts) {
        return Ok(Datum::Timestamp(ts));
    }
    let east = tz::offset_at_utc(&ctx.io.time_zone, ts)?;
    Ok(Datum::Timestamp(dt2local(ts, -i64::from(east))?))
}

fn timetz_at_local(args: &[Datum], ctx: &FuncCtx) -> Result<Datum> {
    let (time, zone) = timetz_arg(args, 0)?;
    let east = tz::offset_at_utc(&ctx.io.time_zone, ctx.session.transaction_timestamp())?;
    Ok(timetz_shift(time, zone, -i64::from(east)))
}

// ---------------------------------------------------------------------------------------------
// date_add / date_subtract with a zone
// ---------------------------------------------------------------------------------------------

fn negate_interval(iv: &Interval) -> Result<Interval> {
    if *iv == Interval::NEG_INFINITY {
        return Ok(Interval::INFINITY);
    }
    if *iv == Interval::INFINITY {
        return Ok(Interval::NEG_INFINITY);
    }
    let m = iv.months.checked_neg().ok_or_else(interval_out_of_range)?;
    let d = iv.days.checked_neg().ok_or_else(interval_out_of_range)?;
    let t = iv.micros.checked_neg().ok_or_else(interval_out_of_range)?;
    let r = Interval::new(m, d, t);
    if r.is_infinite() {
        return Err(interval_out_of_range());
    }
    Ok(r)
}

/// `timestamptz + interval` evaluated in a fixed zone (seconds EAST).
fn tz_fixed_add(ts: i64, iv: &Interval, east: i32) -> Result<i64> {
    if is_inf_ts(ts) || iv.is_infinite() {
        return timestamp_add_interval(ts, iv);
    }
    let shift = i64::from(east) * USECS_PER_SEC;
    let local = ts.checked_add(shift).ok_or_else(ts_out_of_range)?;
    let moved = timestamp_add_interval(local, iv)?;
    let utc = moved.checked_sub(shift).ok_or_else(ts_out_of_range)?;
    valid_ts(utc)
}

fn timestamptz_pl_interval_at_zone(args: &[Datum], _ctx: &FuncCtx) -> Result<Datum> {
    let ts = ts_arg(args, 0)?;
    let iv = interval_arg(args, 1)?;
    let east = resolve_zone_name(text_arg(args, 2)?)?;
    Ok(Datum::TimestampTz(tz_fixed_add(ts, &iv, east)?))
}

fn timestamptz_mi_interval_at_zone(args: &[Datum], _ctx: &FuncCtx) -> Result<Datum> {
    let ts = ts_arg(args, 0)?;
    let iv = interval_arg(args, 1)?;
    let east = resolve_zone_name(text_arg(args, 2)?)?;
    let neg = negate_interval(&iv)?;
    Ok(Datum::TimestampTz(tz_fixed_add(ts, &neg, east)?))
}

// ---------------------------------------------------------------------------------------------
// OVERLAPS
// ---------------------------------------------------------------------------------------------

/// SQL-standard OVERLAPS with PG's NULL handling (`overlaps_timestamp` and friends).
fn overlaps_core<T: Ord + Copy>(
    ts1: Option<T>,
    te1: Option<T>,
    ts2: Option<T>,
    te2: Option<T>,
) -> Option<bool> {
    // Normalize one interval: (start, optional end) with start <= end when both are present.
    fn normalize<T: Ord + Copy>(ts: Option<T>, te: Option<T>) -> Option<(T, Option<T>)> {
        match (ts, te) {
            (None, None) => None,
            (None, Some(e)) => Some((e, None)),
            (Some(s), None) => Some((s, None)),
            (Some(s), Some(e)) => {
                if s > e {
                    Some((e, Some(s)))
                } else {
                    Some((s, Some(e)))
                }
            }
        }
    }
    let (s1, e1) = normalize(ts1, te1)?;
    let (s2, e2) = normalize(ts2, te2)?;
    if s1 > s2 {
        let e2 = e2?;
        if s1 < e2 {
            return Some(true);
        }
        e1?;
        Some(false)
    } else if s1 < s2 {
        let e1 = e1?;
        if s2 < e1 {
            return Some(true);
        }
        e2?;
        Some(false)
    } else if e1.is_none() || e2.is_none() {
        None
    } else {
        Some(true)
    }
}

fn opt_i64(d: &Datum) -> Result<Option<i64>> {
    match d {
        Datum::Null => Ok(None),
        Datum::Timestamp(v) | Datum::TimestampTz(v) | Datum::Time(v) => Ok(Some(*v)),
        other => Err(type_mismatch("timestamp/time", other)),
    }
}

fn opt_timetz(d: &Datum) -> Result<Option<(i64, i32)>> {
    match d {
        Datum::Null => Ok(None),
        // Primary order is the GMT-equivalent time, then the zone.
        Datum::TimeTz(t, z) => Ok(Some((t.saturating_add(i64::from(*z) * USECS_PER_SEC), *z))),
        other => Err(type_mismatch("timetz", other)),
    }
}

fn bool_or_null(r: Option<bool>) -> Datum {
    match r {
        Some(b) => Datum::Bool(b),
        None => Datum::Null,
    }
}

fn overlaps_timestamp(args: &[Datum], _ctx: &FuncCtx) -> Result<Datum> {
    let r = overlaps_core(
        opt_i64(arg(args, 0)?)?,
        opt_i64(arg(args, 1)?)?,
        opt_i64(arg(args, 2)?)?,
        opt_i64(arg(args, 3)?)?,
    );
    Ok(bool_or_null(r))
}

fn overlaps_time(args: &[Datum], _ctx: &FuncCtx) -> Result<Datum> {
    overlaps_timestamp(args, _ctx)
}

fn overlaps_timetz(args: &[Datum], _ctx: &FuncCtx) -> Result<Datum> {
    let r = overlaps_core(
        opt_timetz(arg(args, 0)?)?,
        opt_timetz(arg(args, 1)?)?,
        opt_timetz(arg(args, 2)?)?,
        opt_timetz(arg(args, 3)?)?,
    );
    Ok(bool_or_null(r))
}

/// `start + span` for the SQL-language OVERLAPS variants (NULL in, NULL out).
fn overlaps_end(start: &Datum, span: &Datum, ctx: &FuncCtx) -> Result<Datum> {
    let iv = match span {
        Datum::Null => return Ok(Datum::Null),
        Datum::Interval(iv) => *iv,
        other => return Err(type_mismatch("interval", other)),
    };
    match start {
        Datum::Null => Ok(Datum::Null),
        Datum::Timestamp(ts) => Ok(Datum::Timestamp(timestamp_add_interval(*ts, &iv)?)),
        Datum::TimestampTz(ts) => {
            let args = [Datum::TimestampTz(*ts), Datum::Interval(iv)];
            // timestamptz_pl_interval (oid 1189): months/days in the session zone.
            crate::func::call(1189, &args, ctx)
        }
        Datum::Time(t) => {
            if iv.is_infinite() {
                return Err(PgError::new(
                    SqlState::DATETIME_VALUE_OUT_OF_RANGE,
                    "cannot add infinite interval to time",
                ));
            }
            let sum = i128::from(*t) + i128::from(iv.micros);
            Ok(Datum::Time(sum.rem_euclid(i128::from(USECS_PER_DAY)) as i64))
        }
        other => Err(type_mismatch("timestamp/time", other)),
    }
}

/// SQL-language `overlaps(...)` variants: `(a, a + iv)` / `(a, b)` pairs, then the typed OVERLAPS.
fn overlaps_sql(args: &[Datum], ctx: &FuncCtx) -> Result<Datum> {
    let s1 = arg(args, 0)?;
    let s2 = arg(args, 2)?;
    let e1 = match arg(args, 1)? {
        Datum::Interval(_) => overlaps_end(s1, arg(args, 1)?, ctx)?,
        other => other.clone(),
    };
    let e2 = match arg(args, 3)? {
        Datum::Interval(_) => overlaps_end(s2, arg(args, 3)?, ctx)?,
        other => other.clone(),
    };
    let typed = [s1.clone(), e1, s2.clone(), e2];
    let is_timetz = typed.iter().any(|d| matches!(d, Datum::TimeTz(..)));
    if is_timetz {
        overlaps_timetz(&typed, ctx)
    } else {
        overlaps_timestamp(&typed, ctx)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::context::{IoConfig, IoCtx, TestSession};
    use crate::datetime::{date, time, timestamp};
    use crate::registry;

    fn call(f: ScalarFn, args: &[Datum]) -> Result<Datum> {
        let env = registry::builtin_env();
        let cfg = IoConfig::default();
        let sess = TestSession::default();
        let ctx = FuncCtx { env, io: &cfg, session: &sess, arg_types: &[], result_type: TypeId::TEXT, collation: 0 };
        f(args, &ctx)
    }

    fn io<T>(f: impl FnOnce(&IoCtx) -> T) -> T {
        let env = registry::builtin_env();
        let cfg = IoConfig::default();
        let ctx = IoCtx::new(env, &cfg);
        f(&ctx)
    }

    fn txt(s: &str) -> Datum {
        Datum::Text(s.to_string())
    }
    fn ts(s: &str) -> Datum {
        io(|c| timestamp::input(c, TypeId::TIMESTAMP, s)).expect("timestamp")
    }
    fn tstz(s: &str) -> Datum {
        io(|c| timestamp::input(c, TypeId::TIMESTAMPTZ, s)).expect("timestamptz")
    }
    fn dt(s: &str) -> Datum {
        io(|c| date::input(c, TypeId::DATE, s)).expect("date")
    }
    fn tm(s: &str) -> Datum {
        io(|c| time::input(c, TypeId::TIME, s)).expect("time")
    }
    fn show_ts(d: &Datum) -> String {
        io(|c| timestamp::output(c, TypeId::TIMESTAMP, d)).expect("output")
    }
    fn show_tstz(d: &Datum) -> String {
        io(|c| timestamp::output(c, TypeId::TIMESTAMPTZ, d)).expect("output")
    }
    fn show_iv(d: &Datum) -> String {
        io(|c| interval::output(c, TypeId::INTERVAL, d)).expect("output")
    }
    fn iv(months: i32, days: i32, micros: i64) -> Datum {
        Datum::Interval(Interval::new(months, days, micros))
    }
    fn num(d: &Datum) -> (Numeric, i64) {
        match d {
            Datum::Numeric(n) => (n.clone(), n.dscale()),
            other => panic!("expected numeric, got {other:?}"),
        }
    }
    fn err(r: Result<Datum>) -> (String, String) {
        let e = r.expect_err("expected error");
        (e.sqlstate.code().to_string(), e.message)
    }

    #[test]
    fn extract_epoch_timestamptz() {
        let r = call(extract_timestamptz, &[txt("epoch"), tstz("2024-01-01 00:00:00+00")]).expect("ok");
        let (n, scale) = num(&r);
        assert_eq!(n, Numeric::from_i64(1704067200));
        assert_eq!(scale, 6);
    }

    #[test]
    fn extract_second_time_scale() {
        let r = call(extract_time, &[txt("second"), tm("1:2:3.5")]).expect("ok");
        let (n, scale) = num(&r);
        assert_eq!(scale, 6);
        assert_eq!(n, Numeric::Finite(BigDecimal::new(BigInt::from(35), 1)));
    }

    #[test]
    fn extract_date_fields() {
        let r = call(extract_date, &[txt("year"), dt("2024-02-29")]).expect("ok");
        assert_eq!(num(&r), (Numeric::from_i64(2024), 0));
        let r = call(extract_date, &[txt("julian"), dt("2024-01-01")]).expect("ok");
        assert_eq!(num(&r), (Numeric::from_i64(2460311), 0));
        let r = call(extract_date, &[txt("DOW"), dt("2024-02-29")]).expect("ok");
        assert_eq!(num(&r), (Numeric::from_i64(4), 0));
    }

    #[test]
    fn extract_unknown_unit() {
        let (code, msg) = err(call(extract_date, &[txt("foo"), dt("2024-02-29")]));
        assert_eq!(code, "22023");
        assert_eq!(msg, "unit \"foo\" not recognized for type date");
        let (code, msg) = err(call(extract_date, &[txt("hour"), dt("2024-02-29")]));
        assert_eq!(code, "0A000");
        assert_eq!(msg, "unit \"hour\" not supported for type date");
    }

    #[test]
    fn date_part_dow() {
        let r = call(date_part_date, &[txt("dow"), dt("2024-02-29")]).expect("ok");
        assert_eq!(r, Datum::Float8(4.0));
    }

    #[test]
    fn trunc_month() {
        let r = call(timestamp_trunc, &[txt("month"), ts("2024-02-29 13:01")]).expect("ok");
        assert_eq!(show_ts(&r), "2024-02-01 00:00:00");
        let r = call(timestamp_trunc, &[txt("week"), ts("2024-02-29 13:01")]).expect("ok");
        assert_eq!(show_ts(&r), "2024-02-26 00:00:00");
        let r = call(timestamp_trunc, &[txt("century"), ts("2024-02-29 13:01")]).expect("ok");
        assert_eq!(show_ts(&r), "2001-01-01 00:00:00");
    }

    #[test]
    fn age_basic() {
        let r = call(timestamp_age, &[ts("2024-02-29"), ts("2023-03-31")]).expect("ok");
        assert_eq!(show_iv(&r), "10 mons 29 days");
        let r = call(timestamp_age, &[ts("2023-03-31"), ts("2024-02-29")]).expect("ok");
        assert_eq!(show_iv(&r), "-10 mons -29 days");
    }

    #[test]
    fn bin_basic() {
        let r = call(
            timestamp_bin,
            &[iv(0, 0, 15 * 60 * 1_000_000), ts("2024-02-29 13:07"), ts("2000-01-01")],
        )
        .expect("ok");
        assert_eq!(show_ts(&r), "2024-02-29 13:00:00");
        let (code, msg) = err(call(timestamp_bin, &[iv(1, 0, 0), ts("2024-02-29 13:07"), ts("2000-01-01")]));
        assert_eq!(code, "0A000");
        assert_eq!(msg, "timestamps cannot be binned into intervals containing months or years");
    }

    #[test]
    fn at_time_zone() {
        let r = call(timestamptz_zone, &[txt("+05"), tstz("2024-02-29 12:00+00")]).expect("ok");
        assert_eq!(show_ts(&r), "2024-02-29 07:00:00");
        let r = call(timestamptz_zone, &[txt("PST"), tstz("2024-02-29 12:00+00")]).expect("ok");
        assert_eq!(show_ts(&r), "2024-02-29 04:00:00");
        let r = call(timestamp_zone, &[txt("UTC"), ts("2024-02-29 12:00")]).expect("ok");
        assert_eq!(show_tstz(&r), "2024-02-29 12:00:00+00");
        let (code, msg) = err(call(timestamptz_zone, &[txt("Mars/Base"), tstz("2024-02-29 12:00+00")]));
        assert_eq!(code, "22023");
        assert_eq!(msg, "time zone \"Mars/Base\" not recognized");
    }

    #[test]
    fn to_timestamp_ranges() {
        let r = call(float8_timestamptz, &[Datum::Float8(0.0)]).expect("ok");
        assert_eq!(show_tstz(&r), "1970-01-01 00:00:00+00");
        let (code, msg) = err(call(float8_timestamptz, &[Datum::Float8(-1e20)]));
        assert_eq!(code, "22008");
        assert_eq!(msg, "timestamp out of range: \"-1e+20\"");
        let (_, msg) = err(call(float8_timestamptz, &[Datum::Float8(f64::NAN)]));
        assert_eq!(msg, "timestamp cannot be NaN");
    }

    #[test]
    fn make_things() {
        let r = call(
            make_timestamp,
            &[Datum::Int4(2024), Datum::Int4(2), Datum::Int4(29), Datum::Int4(13), Datum::Int4(1), Datum::Float8(2.5)],
        )
        .expect("ok");
        assert_eq!(show_ts(&r), "2024-02-29 13:01:02.5");
        let r = call(
            make_interval,
            &[
                Datum::Int4(1),
                Datum::Int4(2),
                Datum::Int4(1),
                Datum::Int4(3),
                Datum::Int4(4),
                Datum::Int4(5),
                Datum::Float8(6.5),
            ],
        )
        .expect("ok");
        assert_eq!(show_iv(&r), "1 year 2 mons 10 days 04:05:06.5");
        let (code, msg) = err(call(
            make_timestamp,
            &[Datum::Int4(2024), Datum::Int4(2), Datum::Int4(30), Datum::Int4(0), Datum::Int4(0), Datum::Float8(0.0)],
        ));
        assert_eq!(code, "22008");
        assert_eq!(msg, "date field value out of range: 2024-02-30");
    }

    #[test]
    fn overlaps_nulls() {
        let a = ts("2024-01-01");
        let b = ts("2024-01-10");
        let c = ts("2024-01-05");
        let d = ts("2024-01-20");
        let r = call(overlaps_timestamp, &[a.clone(), b.clone(), c.clone(), d.clone()]).expect("ok");
        assert_eq!(r, Datum::Bool(true));
        let r = call(overlaps_timestamp, &[a.clone(), c.clone(), b.clone(), d.clone()]).expect("ok");
        assert_eq!(r, Datum::Bool(false));
        let r = call(overlaps_timestamp, &[a, Datum::Null, b, d]).expect("ok");
        assert_eq!(r, Datum::Null);
    }

    #[test]
    fn g_format() {
        assert_eq!(fmt_g(-1e20), "-1e+20");
        assert_eq!(fmt_g(61.0), "61");
        assert_eq!(fmt_g(0.5), "0.5");
        assert_eq!(fmt_g(1234567.0), "1.23457e+06");
        assert_eq!(fmt_g(0.0001), "0.0001");
    }
}
