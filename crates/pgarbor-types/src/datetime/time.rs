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
//!
//! Notes on the implementation:
//! - `interval_time` (oid 1419) is generated with `ImplModule::Interval`, so it is not routed here;
//!   its body is provided as `interval_time_value` for the interval module.
//! - Session-zone lookups for a fixed or UTC zone are exact without date context. For a named (IANA)
//!   zone the date anchor comes from the transaction timestamp (`0` = 2000-01-01 when absent); named
//!   zones are unsupported in v1 and fail inside `tz`.
use super::{decode, encode, out_of_range, tz, Tm, MAX_TIMESTAMP_PRECISION, TZDISP_LIMIT, USECS_PER_DAY};
use super::{USECS_PER_HOUR, USECS_PER_MINUTE, USECS_PER_SEC};
use crate::catalog_data::BuiltinProc;
use crate::compare;
use crate::context::{FuncCtx, IoCtx, TimeZoneSetting};
use crate::datum::{type_mismatch, Datum, Interval, TypeId};
use crate::func::{FuncImpl, ScalarFn};
use crate::oids;
use pgarbor_common::{PgError, Result, SqlState};

const TIME_TYPE_NAME: &str = "time without time zone";
const TIMETZ_TYPE_NAME: &str = "time with time zone";

/// PG `TimeScales`: 10^(6 - precision).
const TIME_SCALES: [i64; 7] = [1_000_000, 100_000, 10_000, 1_000, 100, 10, 1];
/// PG `TimeOffsets`: half of the scale (rounding offset), 0 for precision 6.
const TIME_OFFSETS: [i64; 7] = [500_000, 50_000, 5_000, 500, 50, 5, 0];

/// Round µs-since-midnight to `precision` fractional digits (no-op for precision < 0 or >= 6).
// CONTRACT
#[allow(unused_variables)]
pub fn adjust_time_precision(t: i64, precision: i32) -> Result<i64> {
    // PG `AdjustTimeForTypmod`: symmetric rounding (half away from zero).
    if !(0..=MAX_TIMESTAMP_PRECISION).contains(&precision) {
        return Ok(t);
    }
    let p = precision as usize;
    let scale = TIME_SCALES[p];
    let off = TIME_OFFSETS[p];
    if t >= 0 {
        let v = t.checked_add(off).ok_or_else(|| out_of_range("time"))?;
        Ok((v / scale) * scale)
    } else {
        let neg = t.checked_neg().ok_or_else(|| out_of_range("time"))?;
        let v = neg.checked_add(off).ok_or_else(|| out_of_range("time"))?;
        Ok(-((v / scale) * scale))
    }
}

/// `time_in` / `timetz_in` by `ty.oid`.
#[allow(unused_variables)]
pub fn input(ctx: &IoCtx, ty: TypeId, s: &str) -> Result<Datum> {
    if ty.oid == oids::TIMETZ {
        let d = decode::parse_time_only(s, ctx, TIMETZ_TYPE_NAME)?;
        let tod = tm_to_time(&d.tm);
        let zone = match d.tz {
            Some(z) => z,
            None => session_offset_west(&ctx.config.time_zone, ctx.now, tod)?,
        };
        Ok(Datum::TimeTz(adjust_time_precision(tod, ty.typmod)?, zone))
    } else {
        let d = decode::parse_time_only(s, ctx, TIME_TYPE_NAME)?;
        Ok(Datum::Time(adjust_time_precision(tm_to_time(&d.tm), ty.typmod)?))
    }
}

/// `time_out` / `timetz_out`.
#[allow(unused_variables)]
pub fn output(ctx: &IoCtx, ty: TypeId, d: &Datum) -> Result<String> {
    let style = ctx.config.date_style;
    if ty.oid == oids::TIMETZ {
        match d {
            Datum::TimeTz(t, z) => encode::encode_time_only(&time_to_tm(*t)?, Some(*z), style),
            other => Err(type_mismatch("timetz", other)),
        }
    } else {
        match d {
            Datum::Time(t) => encode::encode_time_only(&time_to_tm(*t)?, None, style),
            other => Err(type_mismatch("time", other)),
        }
    }
}

/// `time_recv` / `timetz_recv`.
#[allow(unused_variables)]
pub fn recv(ctx: &IoCtx, ty: TypeId, buf: &[u8]) -> Result<Datum> {
    let _ = ctx;
    if ty.oid == oids::TIMETZ {
        if buf.len() < 12 {
            return Err(insufficient_binary());
        }
        let t = be_i64(&buf[..8]);
        check_time_range(t)?;
        let zone = be_i32(&buf[8..12]);
        if zone <= -TZDISP_LIMIT || zone >= TZDISP_LIMIT {
            return Err(PgError::new(
                SqlState::INVALID_TIME_ZONE_DISPLACEMENT_VALUE,
                "time zone displacement out of range",
            ));
        }
        if buf.len() != 12 {
            return Err(invalid_binary());
        }
        Ok(Datum::TimeTz(adjust_time_precision(t, ty.typmod)?, zone))
    } else {
        if buf.len() < 8 {
            return Err(insufficient_binary());
        }
        let t = be_i64(&buf[..8]);
        check_time_range(t)?;
        if buf.len() != 8 {
            return Err(invalid_binary());
        }
        Ok(Datum::Time(adjust_time_precision(t, ty.typmod)?))
    }
}

/// `time_send` / `timetz_send`.
#[allow(unused_variables)]
pub fn send(ctx: &IoCtx, ty: TypeId, d: &Datum) -> Result<Vec<u8>> {
    let _ = ctx;
    match d {
        Datum::TimeTz(t, z) if ty.oid == oids::TIMETZ => {
            let mut v = Vec::with_capacity(12);
            v.extend_from_slice(&t.to_be_bytes());
            v.extend_from_slice(&z.to_be_bytes());
            Ok(v)
        }
        Datum::Time(t) if ty.oid != oids::TIMETZ => Ok(t.to_be_bytes().to_vec()),
        other => Err(type_mismatch(if ty.oid == oids::TIMETZ { "timetz" } else { "time" }, other)),
    }
}

/// Function registry hook (see `crate::func`).
#[allow(unused_variables)]
pub(crate) fn lookup(proc: &BuiltinProc) -> Option<FuncImpl> {
    let f: ScalarFn = match proc.oid {
        // SQL-language builtins (empty prosrc).
        1848 => interval_pl_time,
        2547 => interval_pl_timetz,
        _ => match proc.prosrc {
            "time_eq" => time_eq,
            "time_ne" => time_ne,
            "time_lt" => time_lt,
            "time_le" => time_le,
            "time_gt" => time_gt,
            "time_ge" => time_ge,
            "time_cmp" => time_cmp,
            "time_larger" => time_larger,
            "time_smaller" => time_smaller,
            "time_hash" => time_hash,
            "time_hash_extended" => time_hash_extended,
            "time_pl_interval" => time_pl_interval,
            "time_mi_interval" => time_mi_interval,
            "time_mi_time" => time_mi_time,
            "time_interval" => time_interval,
            "time_timetz" => time_timetz,
            "timetz_time" => timetz_time,
            "timetz_eq" => timetz_eq,
            "timetz_ne" => timetz_ne,
            "timetz_lt" => timetz_lt,
            "timetz_le" => timetz_le,
            "timetz_gt" => timetz_gt,
            "timetz_ge" => timetz_ge,
            "timetz_cmp" => timetz_cmp,
            "timetz_larger" => timetz_larger,
            "timetz_smaller" => timetz_smaller,
            "timetz_hash" => timetz_hash,
            "timetz_hash_extended" => timetz_hash_extended,
            "timetz_pl_interval" => timetz_pl_interval,
            "timetz_mi_interval" => timetz_mi_interval,
            "in_range_time_interval" => in_range_time_interval,
            "in_range_timetz_interval" => in_range_timetz_interval,
            "make_time" => make_time,
            _ => return None,
        },
    };
    Some(FuncImpl::Scalar(f))
}

/// 0A000 for a Time-module proc without implementation.
pub(crate) fn unsupported(proc: &BuiltinProc) -> PgError {
    PgError::unimplemented("TYP-27", format!("function {}", proc.name))
}

// ---------------------------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------------------------

/// µs since midnight → broken-down time (PG `time2tm`). Values outside `0..=86400e6` are a bug.
fn time_to_tm(t: i64) -> Result<Tm> {
    if !(0..=USECS_PER_DAY).contains(&t) {
        return Err(PgError::internal(format!("time value {t} outside 0..=86400000000 microseconds")));
    }
    let hour = t / USECS_PER_HOUR;
    let rem = t % USECS_PER_HOUR;
    let min = rem / USECS_PER_MINUTE;
    let rem = rem % USECS_PER_MINUTE;
    Ok(Tm {
        hour: hour as i32,
        min: min as i32,
        sec: (rem / USECS_PER_SEC) as i32,
        fsec: (rem % USECS_PER_SEC) as i32,
        ..Tm::default()
    })
}

/// Broken-down time → µs since midnight (PG `tm2time`).
fn tm_to_time(tm: &Tm) -> i64 {
    ((i64::from(tm.hour) * 60 + i64::from(tm.min)) * 60 + i64::from(tm.sec)) * USECS_PER_SEC
        + i64::from(tm.fsec)
}

fn check_time_range(t: i64) -> Result<()> {
    if (0..=USECS_PER_DAY).contains(&t) {
        Ok(())
    } else {
        Err(out_of_range("time"))
    }
}

/// Session zone offset in seconds WEST for local time-of-day `tod` (µs). Used by timetz input without
/// an explicit zone and by `time → timetz`. Fixed and UTC zones are exact; named zones use the date of
/// the transaction timestamp.
fn session_offset_west(setting: &TimeZoneSetting, now: Option<i64>, tod: i64) -> Result<i32> {
    let east = match setting {
        TimeZoneSetting::Utc => 0,
        TimeZoneSetting::Fixed { secs_east, .. } => *secs_east,
        TimeZoneSetting::Named(_) => {
            let now = now.unwrap_or(0);
            let off = tz::offset_at_utc(setting, now)?;
            let local_now = now.wrapping_add(i64::from(off) * USECS_PER_SEC);
            let day_start = local_now.div_euclid(USECS_PER_DAY) * USECS_PER_DAY;
            tz::offset_at_local(setting, day_start.wrapping_add(tod))?
        }
    };
    Ok(-east)
}

fn insufficient_binary() -> PgError {
    PgError::new(SqlState::PROTOCOL_VIOLATION, "insufficient data left in message")
}

fn invalid_binary() -> PgError {
    PgError::new(SqlState::INVALID_BINARY_REPRESENTATION, "incorrect binary data format")
}

fn be_i64(b: &[u8]) -> i64 {
    let mut a = [0u8; 8];
    for (dst, src) in a.iter_mut().zip(b.iter()) {
        *dst = *src;
    }
    i64::from_be_bytes(a)
}

fn be_i32(b: &[u8]) -> i32 {
    let mut a = [0u8; 4];
    for (dst, src) in a.iter_mut().zip(b.iter()) {
        *dst = *src;
    }
    i32::from_be_bytes(a)
}

fn missing_arg(i: usize) -> PgError {
    PgError::internal(format!("missing function argument {i}"))
}

fn arg_time(args: &[Datum], i: usize) -> Result<i64> {
    match args.get(i) {
        Some(Datum::Time(t)) => Ok(*t),
        Some(other) => Err(type_mismatch("time", other)),
        None => Err(missing_arg(i)),
    }
}

fn arg_timetz(args: &[Datum], i: usize) -> Result<(i64, i32)> {
    match args.get(i) {
        Some(Datum::TimeTz(t, z)) => Ok((*t, *z)),
        Some(other) => Err(type_mismatch("timetz", other)),
        None => Err(missing_arg(i)),
    }
}

fn arg_interval(args: &[Datum], i: usize) -> Result<Interval> {
    match args.get(i) {
        Some(Datum::Interval(iv)) => Ok(*iv),
        Some(other) => Err(type_mismatch("interval", other)),
        None => Err(missing_arg(i)),
    }
}

fn arg_int4(args: &[Datum], i: usize) -> Result<i32> {
    match args.get(i) {
        Some(Datum::Int4(v)) => Ok(*v),
        Some(other) => Err(type_mismatch("integer", other)),
        None => Err(missing_arg(i)),
    }
}

fn arg_int8(args: &[Datum], i: usize) -> Result<i64> {
    match args.get(i) {
        Some(Datum::Int8(v)) => Ok(*v),
        Some(other) => Err(type_mismatch("bigint", other)),
        None => Err(missing_arg(i)),
    }
}

fn arg_f64(args: &[Datum], i: usize) -> Result<f64> {
    match args.get(i) {
        Some(Datum::Float8(v)) => Ok(*v),
        Some(other) => Err(type_mismatch("double precision", other)),
        None => Err(missing_arg(i)),
    }
}

fn arg_bool(args: &[Datum], i: usize) -> Result<bool> {
    match args.get(i) {
        Some(Datum::Bool(v)) => Ok(*v),
        Some(other) => Err(type_mismatch("boolean", other)),
        None => Err(missing_arg(i)),
    }
}

/// Wrap a µs value into `0..USECS_PER_DAY` (PG: `r -= r / DAY * DAY; if r < 0 { r += DAY }`).
fn wrap_day(r: i64) -> i64 {
    let mut r = r - (r / USECS_PER_DAY) * USECS_PER_DAY;
    if r < 0 {
        r += USECS_PER_DAY;
    }
    r
}

/// `time ± interval` (months and days are ignored, the result wraps at 24h).
fn shift_time(t: i64, span: Interval, add: bool) -> Result<i64> {
    if span.is_infinite() {
        let msg = if add {
            "cannot add infinite interval to time"
        } else {
            "cannot subtract infinite interval from time"
        };
        return Err(PgError::new(SqlState::DATETIME_VALUE_OUT_OF_RANGE, msg));
    }
    let r = if add { t.wrapping_add(span.micros) } else { t.wrapping_sub(span.micros) };
    Ok(wrap_day(r))
}

/// PG `timetz_cmp_internal`: compare UTC-adjusted times, then zones.
fn timetz_cmp_internal(t1: i64, z1: i32, t2: i64, z2: i32) -> i32 {
    let u1 = t1.wrapping_add(i64::from(z1) * USECS_PER_SEC);
    let u2 = t2.wrapping_add(i64::from(z2) * USECS_PER_SEC);
    if u1 > u2 {
        1
    } else if u1 < u2 {
        -1
    } else if z1 > z2 {
        1
    } else if z1 < z2 {
        -1
    } else {
        0
    }
}

/// PG `hashint8` (`src/backend/access/hash/hashfunc.c`): fold the halves, then `hash_uint32`.
fn hashint8_u32(v: i64) -> Result<u32> {
    let lo = v as u32;
    let hi = (v >> 32) as u32;
    let folded = if v >= 0 { lo ^ hi } else { lo ^ !hi };
    compare::pg_hash_uint32(folded)
}

/// PG `hashint8extended`.
fn hashint8_extended(v: i64, seed: i64) -> Result<u64> {
    let lo = v as u32;
    let hi = (v >> 32) as u32;
    let folded = if v >= 0 { lo ^ hi } else { lo ^ !hi };
    compare::pg_hash_uint32_extended(folded, seed as u64)
}

/// PG `rint` (round half to even) for finite values; non-finite values pass through.
fn rint(x: f64) -> f64 {
    if !x.is_finite() {
        return x;
    }
    let f = x.floor();
    let d = x - f;
    if d > 0.5 {
        f + 1.0
    } else if d < 0.5 {
        f
    } else if f % 2.0 == 0.0 {
        f
    } else {
        f + 1.0
    }
}

/// PG `float_time_overflows` (date.c).
fn float_time_overflows(hour: i32, min: i32, sec: f64) -> bool {
    if hour < 0 || hour > 24 || min < 0 || min >= 60 {
        return true;
    }
    if sec.is_nan() {
        return true;
    }
    let us = rint(sec * USECS_PER_SEC as f64);
    if us < 0.0 || us > 60.0 * USECS_PER_SEC as f64 {
        return true;
    }
    let total = ((i64::from(hour) * 60 + i64::from(min)) * 60) * USECS_PER_SEC + us as i64;
    total > USECS_PER_DAY
}

/// C `%g` (precision 6) for the `make_time` error message.
fn c_fmt_g(x: f64) -> String {
    if x.is_nan() {
        return "nan".to_string();
    }
    if x.is_infinite() {
        return if x > 0.0 { "inf".to_string() } else { "-inf".to_string() };
    }
    if x == 0.0 {
        return "0".to_string();
    }
    const P: i32 = 6;
    let sci = format!("{:.*e}", (P - 1) as usize, x);
    let Some((mant, exp_str)) = sci.split_once('e').map(|(m, e)| (m.to_string(), e.to_string())) else {
        return sci;
    };
    let exp: i32 = exp_str.parse().unwrap_or(0);
    if exp < -4 || exp >= P {
        let m = strip_fraction_zeros(&mant);
        format!("{}e{}{:02}", m, if exp < 0 { '-' } else { '+' }, exp.abs())
    } else {
        let decimals = (P - 1 - exp) as usize;
        strip_fraction_zeros(&format!("{:.*}", decimals, x))
    }
}

fn strip_fraction_zeros(s: &str) -> String {
    if s.contains('.') {
        s.trim_end_matches('0').trim_end_matches('.').to_string()
    } else {
        s.to_string()
    }
}

/// Interval → time-of-day, modulo one day (PG `interval_time`). Used by the interval module.
#[allow(dead_code)]
pub(crate) fn interval_time_value(span: &Interval) -> Result<i64> {
    if span.is_infinite() {
        return Err(PgError::new(SqlState::DATETIME_VALUE_OUT_OF_RANGE, "cannot convert infinite interval to time"));
    }
    let mut r = span.micros % USECS_PER_DAY;
    if r < 0 {
        r += USECS_PER_DAY;
    }
    Ok(r)
}

// ---------------------------------------------------------------------------------------------
// time procs
// ---------------------------------------------------------------------------------------------

macro_rules! time_cmp_fn {
    ($name:ident, $op:tt) => {
        fn $name(args: &[Datum], _ctx: &FuncCtx) -> Result<Datum> {
            let a = arg_time(args, 0)?;
            let b = arg_time(args, 1)?;
            Ok(Datum::Bool(a $op b))
        }
    };
}

time_cmp_fn!(time_eq, ==);
time_cmp_fn!(time_ne, !=);
time_cmp_fn!(time_lt, <);
time_cmp_fn!(time_le, <=);
time_cmp_fn!(time_gt, >);
time_cmp_fn!(time_ge, >=);

fn time_cmp(args: &[Datum], _ctx: &FuncCtx) -> Result<Datum> {
    let a = arg_time(args, 0)?;
    let b = arg_time(args, 1)?;
    let r: i32 = if a < b {
        -1
    } else if a > b {
        1
    } else {
        0
    };
    Ok(Datum::Int4(r))
}

fn time_larger(args: &[Datum], _ctx: &FuncCtx) -> Result<Datum> {
    let a = arg_time(args, 0)?;
    let b = arg_time(args, 1)?;
    Ok(Datum::Time(if a > b { a } else { b }))
}

fn time_smaller(args: &[Datum], _ctx: &FuncCtx) -> Result<Datum> {
    let a = arg_time(args, 0)?;
    let b = arg_time(args, 1)?;
    Ok(Datum::Time(if a < b { a } else { b }))
}

fn time_hash(args: &[Datum], _ctx: &FuncCtx) -> Result<Datum> {
    let t = arg_time(args, 0)?;
    Ok(Datum::Int4(hashint8_u32(t)? as i32))
}

fn time_hash_extended(args: &[Datum], _ctx: &FuncCtx) -> Result<Datum> {
    let t = arg_time(args, 0)?;
    let seed = arg_int8(args, 1)?;
    Ok(Datum::Int8(hashint8_extended(t, seed)? as i64))
}

fn time_pl_interval(args: &[Datum], _ctx: &FuncCtx) -> Result<Datum> {
    let t = arg_time(args, 0)?;
    let span = arg_interval(args, 1)?;
    Ok(Datum::Time(shift_time(t, span, true)?))
}

fn time_mi_interval(args: &[Datum], _ctx: &FuncCtx) -> Result<Datum> {
    let t = arg_time(args, 0)?;
    let span = arg_interval(args, 1)?;
    Ok(Datum::Time(shift_time(t, span, false)?))
}

fn time_mi_time(args: &[Datum], _ctx: &FuncCtx) -> Result<Datum> {
    let t1 = arg_time(args, 0)?;
    let t2 = arg_time(args, 1)?;
    Ok(Datum::Interval(Interval::new(0, 0, t1.wrapping_sub(t2))))
}

fn time_interval(args: &[Datum], _ctx: &FuncCtx) -> Result<Datum> {
    let t = arg_time(args, 0)?;
    Ok(Datum::Interval(Interval::new(0, 0, t)))
}

fn time_timetz(args: &[Datum], ctx: &FuncCtx) -> Result<Datum> {
    let t = arg_time(args, 0)?;
    let now = ctx.io_ctx().now;
    let zone = session_offset_west(&ctx.io.time_zone, now, t)?;
    Ok(Datum::TimeTz(t, zone))
}

fn in_range_time_interval(args: &[Datum], _ctx: &FuncCtx) -> Result<Datum> {
    let val = arg_time(args, 0)?;
    let base = arg_time(args, 1)?;
    let offset = arg_interval(args, 2)?;
    let sub = arg_bool(args, 3)?;
    let less = arg_bool(args, 4)?;
    // Month and day fields are ignored; a negative (or -infinity) offset is an error.
    if offset.micros < 0 {
        return Err(PgError::new(
            SqlState(*b"22013"),
            "invalid preceding or following size in window function",
        ));
    }
    // Non-wrapping arithmetic (PG); an overflowing sum is treated as "beyond the end".
    let sum = if sub {
        base.wrapping_sub(offset.micros)
    } else {
        match base.checked_add(offset.micros) {
            Some(s) => s,
            None => return Ok(Datum::Bool(less)),
        }
    };
    Ok(Datum::Bool(if less { val <= sum } else { val >= sum }))
}

fn make_time(args: &[Datum], _ctx: &FuncCtx) -> Result<Datum> {
    let hour = arg_int4(args, 0)?;
    let min = arg_int4(args, 1)?;
    let sec = arg_f64(args, 2)?;
    if float_time_overflows(hour, min, sec) {
        let padded = {
            let s = c_fmt_g(sec);
            if s.len() >= 2 {
                s
            } else {
                format!("0{s}")
            }
        };
        return Err(PgError::new(
            SqlState::DATETIME_FIELD_OVERFLOW,
            format!("time field value out of range: {}:{:02}:{}", hour, min, padded),
        ));
    }
    let total = ((i64::from(hour) * 60 + i64::from(min)) * 60) * USECS_PER_SEC
        + rint(sec * USECS_PER_SEC as f64) as i64;
    Ok(Datum::Time(total))
}

fn interval_pl_time(args: &[Datum], _ctx: &FuncCtx) -> Result<Datum> {
    let span = arg_interval(args, 0)?;
    let t = arg_time(args, 1)?;
    Ok(Datum::Time(shift_time(t, span, true)?))
}

// ---------------------------------------------------------------------------------------------
// timetz procs
// ---------------------------------------------------------------------------------------------

macro_rules! timetz_cmp_fn {
    ($name:ident, $op:tt) => {
        fn $name(args: &[Datum], _ctx: &FuncCtx) -> Result<Datum> {
            let (t1, z1) = arg_timetz(args, 0)?;
            let (t2, z2) = arg_timetz(args, 1)?;
            Ok(Datum::Bool(timetz_cmp_internal(t1, z1, t2, z2) $op 0))
        }
    };
}

timetz_cmp_fn!(timetz_eq, ==);
timetz_cmp_fn!(timetz_ne, !=);
timetz_cmp_fn!(timetz_lt, <);
timetz_cmp_fn!(timetz_le, <=);
timetz_cmp_fn!(timetz_gt, >);
timetz_cmp_fn!(timetz_ge, >=);

fn timetz_cmp(args: &[Datum], _ctx: &FuncCtx) -> Result<Datum> {
    let (t1, z1) = arg_timetz(args, 0)?;
    let (t2, z2) = arg_timetz(args, 1)?;
    Ok(Datum::Int4(timetz_cmp_internal(t1, z1, t2, z2)))
}

fn timetz_larger(args: &[Datum], _ctx: &FuncCtx) -> Result<Datum> {
    let (t1, z1) = arg_timetz(args, 0)?;
    let (t2, z2) = arg_timetz(args, 1)?;
    if timetz_cmp_internal(t1, z1, t2, z2) > 0 {
        Ok(Datum::TimeTz(t1, z1))
    } else {
        Ok(Datum::TimeTz(t2, z2))
    }
}

fn timetz_smaller(args: &[Datum], _ctx: &FuncCtx) -> Result<Datum> {
    let (t1, z1) = arg_timetz(args, 0)?;
    let (t2, z2) = arg_timetz(args, 1)?;
    if timetz_cmp_internal(t1, z1, t2, z2) < 0 {
        Ok(Datum::TimeTz(t1, z1))
    } else {
        Ok(Datum::TimeTz(t2, z2))
    }
}

fn timetz_hash(args: &[Datum], _ctx: &FuncCtx) -> Result<Datum> {
    let (t, z) = arg_timetz(args, 0)?;
    let h = hashint8_u32(t)? ^ compare::pg_hash_uint32(z as u32)?;
    Ok(Datum::Int4(h as i32))
}

fn timetz_hash_extended(args: &[Datum], _ctx: &FuncCtx) -> Result<Datum> {
    let (t, z) = arg_timetz(args, 0)?;
    let seed = arg_int8(args, 1)?;
    let h = hashint8_extended(t, seed)? ^ compare::pg_hash_uint32_extended(z as u32, seed as u64)?;
    Ok(Datum::Int8(h as i64))
}

fn timetz_pl_interval(args: &[Datum], _ctx: &FuncCtx) -> Result<Datum> {
    let (t, z) = arg_timetz(args, 0)?;
    let span = arg_interval(args, 1)?;
    Ok(Datum::TimeTz(shift_time(t, span, true)?, z))
}

fn timetz_mi_interval(args: &[Datum], _ctx: &FuncCtx) -> Result<Datum> {
    let (t, z) = arg_timetz(args, 0)?;
    let span = arg_interval(args, 1)?;
    Ok(Datum::TimeTz(shift_time(t, span, false)?, z))
}

fn timetz_time(args: &[Datum], _ctx: &FuncCtx) -> Result<Datum> {
    let (t, _z) = arg_timetz(args, 0)?;
    Ok(Datum::Time(t))
}

fn in_range_timetz_interval(args: &[Datum], _ctx: &FuncCtx) -> Result<Datum> {
    let (vt, vz) = arg_timetz(args, 0)?;
    let (bt, bz) = arg_timetz(args, 1)?;
    let offset = arg_interval(args, 2)?;
    let sub = arg_bool(args, 3)?;
    let less = arg_bool(args, 4)?;
    if offset.micros < 0 {
        return Err(PgError::new(
            SqlState(*b"22013"),
            "invalid preceding or following size in window function",
        ));
    }
    let sum_t = if sub {
        bt.wrapping_sub(offset.micros)
    } else {
        match bt.checked_add(offset.micros) {
            Some(s) => s,
            None => return Ok(Datum::Bool(less)),
        }
    };
    let c = timetz_cmp_internal(vt, vz, sum_t, bz);
    Ok(Datum::Bool(if less { c <= 0 } else { c >= 0 }))
}

fn interval_pl_timetz(args: &[Datum], _ctx: &FuncCtx) -> Result<Datum> {
    let span = arg_interval(args, 0)?;
    let (t, z) = arg_timetz(args, 1)?;
    Ok(Datum::TimeTz(shift_time(t, span, true)?, z))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::context::{IoConfig, TestSession};
    use crate::registry::builtin_env;

    fn io_ctx(cfg: &IoConfig) -> IoCtx<'_> {
        IoCtx::new(builtin_env(), cfg)
    }

    fn fn_ctx<'a>(cfg: &'a IoConfig, session: &'a TestSession) -> FuncCtx<'a> {
        FuncCtx {
            env: builtin_env(),
            io: cfg,
            session,
            arg_types: &[],
            result_type: TypeId::INTERVAL,
            collation: 0,
        }
    }

    #[test]
    fn precision_rounds_half_away_from_zero() {
        assert_eq!(adjust_time_precision(43_200_123_456, 2).unwrap(), 43_200_120_000);
        assert_eq!(adjust_time_precision(125_000, 2).unwrap(), 130_000);
        assert_eq!(adjust_time_precision(-125_000, 2).unwrap(), -130_000);
        assert_eq!(adjust_time_precision(123, -1).unwrap(), 123);
        assert_eq!(adjust_time_precision(123, 6).unwrap(), 123);
    }

    #[test]
    fn time_input_accepts_24h_and_applies_typmod() {
        let cfg = IoConfig::default();
        let ctx = io_ctx(&cfg);
        assert_eq!(input(&ctx, TypeId::new(oids::TIME, 2), "12:00:00.123456").unwrap(), Datum::Time(43_200_120_000));
        assert_eq!(input(&ctx, TypeId::TIME, "24:00:00").unwrap(), Datum::Time(USECS_PER_DAY));
    }

    #[test]
    fn time_input_out_of_range_is_22008() {
        let cfg = IoConfig::default();
        let ctx = io_ctx(&cfg);
        let err = input(&ctx, TypeId::TIME, "25:00").unwrap_err();
        assert_eq!(err.sqlstate, SqlState::DATETIME_FIELD_OVERFLOW);
        assert_eq!(err.message, "date/time field value out of range: \"25:00\"");
    }

    #[test]
    fn time_output_fraction() {
        let cfg = IoConfig::default();
        let ctx = io_ctx(&cfg);
        // 13:01:02.5
        assert_eq!(output(&ctx, TypeId::TIME, &Datum::Time(46_862_500_000)).unwrap(), "13:01:02.5");
    }

    #[test]
    fn timetz_roundtrip_keeps_zone() {
        let cfg = IoConfig::default();
        let ctx = io_ctx(&cfg);
        let d = input(&ctx, TypeId::TIMETZ, "12:34:56.789+05:30").unwrap();
        assert_eq!(d, Datum::TimeTz(45_296_789_000, -19_800));
        assert_eq!(output(&ctx, TypeId::TIMETZ, &d).unwrap(), "12:34:56.789+05:30");
    }

    #[test]
    fn timetz_binary_roundtrip_and_zone_check() {
        let cfg = IoConfig::default();
        let ctx = io_ctx(&cfg);
        let d = Datum::TimeTz(45_296_789_000, -19_800);
        let bytes = send(&ctx, TypeId::TIMETZ, &d).unwrap();
        assert_eq!(bytes.len(), 12);
        assert_eq!(recv(&ctx, TypeId::TIMETZ, &bytes).unwrap(), d);

        let mut bad = Vec::new();
        bad.extend_from_slice(&0i64.to_be_bytes());
        bad.extend_from_slice(&(16 * 3600i32).to_be_bytes());
        let err = recv(&ctx, TypeId::TIMETZ, &bad).unwrap_err();
        assert_eq!(err.sqlstate, SqlState::INVALID_TIME_ZONE_DISPLACEMENT_VALUE);
    }

    #[test]
    fn time_mi_time_and_pl_interval_wrap() {
        let cfg = IoConfig::default();
        let session = TestSession::default();
        let ctx = fn_ctx(&cfg, &session);
        let noon = Datum::Time(43_200_000_000);
        let half_past_eleven = Datum::Time(41_400_000_000);
        assert_eq!(
            time_mi_time(&[noon.clone(), half_past_eleven], &ctx).unwrap(),
            Datum::Interval(Interval::new(0, 0, 1_800_000_000))
        );
        let eleven_pm = Datum::Time(82_800_000_000);
        let two_hours = Datum::Interval(Interval::new(0, 0, 7_200_000_000));
        assert_eq!(time_pl_interval(&[eleven_pm, two_hours], &ctx).unwrap(), Datum::Time(3_600_000_000));
    }

    #[test]
    fn make_time_overflow_message() {
        let cfg = IoConfig::default();
        let session = TestSession::default();
        let ctx = fn_ctx(&cfg, &session);
        let err = make_time(&[Datum::Int4(25), Datum::Int4(0), Datum::Float8(0.0)], &ctx).unwrap_err();
        assert_eq!(err.sqlstate, SqlState::DATETIME_FIELD_OVERFLOW);
        assert_eq!(err.message, "time field value out of range: 25:00:00");
        assert_eq!(
            make_time(&[Datum::Int4(13), Datum::Int4(1), Datum::Float8(2.5)], &ctx).unwrap(),
            Datum::Time(46_862_500_000)
        );
    }

    #[test]
    fn timetz_compare_uses_utc_then_zone() {
        // 12:00 at -05:30 (zone -19800 west) is 06:30 UTC; 06:30 at UTC is the same instant.
        assert_eq!(timetz_cmp_internal(43_200_000_000, -19_800, 23_400_000_000, 0), 1);
        assert_eq!(timetz_cmp_internal(23_400_000_000, 0, 43_200_000_000, -19_800), -1);
    }
}
