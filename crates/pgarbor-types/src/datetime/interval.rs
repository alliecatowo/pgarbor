//! `interval` I/O and arithmetic (TYPE-06, TYPE-14). Lease TYP-29 (sonnet).
//!
//! PG18 reference: `src/backend/utils/adt/timestamp.c` (`interval_in`, `interval_out`,
//! `AdjustIntervalForTypmod`, `interval_pl/mi/um/mul/div`, `interval_cmp_internal`,
//! `interval_justify_*`, infinite-interval rules), `datetime.c` (`DecodeInterval`,
//! `DecodeISO8601Interval`, `ClearPgItmIn`, unit table `deltatktbl`).
//!
//! Input (`interval_in`): lex with `lexer::parse_datetime`, then DecodeInterval (postgres syntax:
//! `1 year 2 mons 3 days 04:05:06.7`, `1.5 years` (= 1 year 6 mons), `1 day ago` (negates all),
//! `@ 1 minute`, `1 week` (= 7 days), units with plurals/abbreviations (`y yr yrs year years mon mons
//! month months w week d day days h hr hrs hour hours m min mins minute minutes s sec secs second seconds
//! ms millisecond(s) us microsecond(s) decade century millennium`), SQL standard `1-2` (Y-M) and
//! `3 4:05:06` (D H:M:S), fractional units cascade down (PG `AdjustFractDays`/`AdjustFractSeconds`)),
//! or ISO 8601 (`P1Y2M3DT4H5M6.7S`, alternative format `P0001-02-03T04:05:06`), `infinity`/`-infinity`.
//! Errors: junk → 22007 `invalid input syntax for type interval: "x"`; overflow → 22015
//! `interval field value out of range: "..."` / 22008 `interval out of range`.
//! Typmod (`ty.typmod`, range bits in `datetime::interval_fields`, precision in low 16 bits):
//! fields restriction truncates lower fields (`interval '1 year 2 months' year` → `1 year`), precision
//! rounds seconds (`'1 day 02:03:04.5678'::interval second(2)` → `1 day 02:03:04.57`).
//! Note the SQL-standard quirk: with `YEAR TO MONTH` etc. a bare number is interpreted in the
//! last field's unit (`interval '2' hour` = `02:00:00`).
//! Output: `encode::encode_interval` under `ctx.config.interval_style`.
//! Binary: `micros: i64 BE, days: i32 BE, months: i32 BE` (in that order).
//!
//! Arithmetic (oracle-verified): `interval '1 day' * 1.5` = `1 day 12:00:00`; `interval '1 mon' / 3` =
//! `10 days`; `'1 year'::interval / 7` = `1 mon 21 days 10:17:08.5344` (fractional months cascade
//! to days via 30, days to µs via 24h, PG `interval_mul` rounding with `rint`); `justify_interval(interval
//! '1 mon -1 day')` = `29 days`; `justify_hours(interval '27 hours')` = `1 day 03:00:00`. Overflow → 22008
//! `interval out of range`. Comparison/equality via `Interval::cmp_span` (`'1 mon' = '30 days'`).
//! Division by zero → 22012 `division by zero`. Infinite intervals per PG17+ rules (inf + -inf → 22008
//! `interval out of range`).
//!
//! Owned procs (ImplModule::Interval): `interval_pl interval_mi interval_um interval_mul mul_d_interval
//! interval_div interval_eq ne lt le gt ge interval_cmp interval_larger interval_smaller interval_hash(_extended)
//! interval_finite interval_justify_days interval_justify_hours interval_justify_interval interval_time
//! (cast, time of day modulo 24h) in_range_interval_interval`.
use super::lexer::{self, Field, FieldKind};
use super::{
    dt_parse_error, encode, interval_fields as fld, out_of_range, DtErr, Itm, DAYS_PER_MONTH, MAX_INTERVAL_PRECISION,
    MONTHS_PER_YEAR, SECS_PER_DAY, USECS_PER_DAY, USECS_PER_HOUR, USECS_PER_MINUTE, USECS_PER_SEC,
};
use crate::catalog_data::BuiltinProc;
use crate::compare;
use crate::context::{FuncCtx, IntervalStyle, IoCtx};
use crate::datum::{type_mismatch, Datum, Interval, TypeId};
use crate::func::FuncImpl;
use pgarbor_common::{PgError, Result, SqlState};

// ---------------------------------------------------------------------------------------------
// Public API
// ---------------------------------------------------------------------------------------------

/// Parse interval text (no typmod). Example: `"1 day 02:00"` → `Interval{0, 1, 7_200_000_000}`.
// CONTRACT
pub fn parse_interval(s: &str, ctx: &IoCtx) -> Result<Interval> {
    parse_interval_typmod(s, ctx, -1)
}

/// Apply an interval typmod (fields restriction + precision), PG `AdjustIntervalForTypmod`.
// CONTRACT
pub fn apply_interval_typmod(iv: Interval, typmod: i32) -> Result<Interval> {
    adjust_for_typmod(iv, typmod)
}

/// `a + b` with overflow checks.
// CONTRACT
pub fn add(a: &Interval, b: &Interval) -> Result<Interval> {
    if *a == Interval::NEG_INFINITY {
        if *b == Interval::INFINITY {
            return Err(interval_oor());
        }
        return Ok(Interval::NEG_INFINITY);
    }
    if *a == Interval::INFINITY {
        if *b == Interval::NEG_INFINITY {
            return Err(interval_oor());
        }
        return Ok(Interval::INFINITY);
    }
    if b.is_infinite() {
        return Ok(*b);
    }
    let months = a.months.checked_add(b.months);
    let days = a.days.checked_add(b.days);
    let micros = a.micros.checked_add(b.micros);
    finite_result(months, days, micros)
}

/// `iv * factor` (PG `interval_mul`).
// CONTRACT
pub fn mul(iv: &Interval, factor: f64) -> Result<Interval> {
    if factor.is_nan() {
        return Err(interval_oor());
    }
    if iv.is_infinite() {
        if factor == 0.0 {
            return Err(interval_oor());
        }
        if factor < 0.0 {
            return neg(iv);
        }
        return Ok(*iv);
    }
    if factor.is_infinite() {
        let sign = sign_of(iv);
        if sign == 0 {
            return Err(interval_oor());
        }
        if factor * (sign as f64) < 0.0 {
            return Ok(Interval::NEG_INFINITY);
        }
        return Ok(Interval::INFINITY);
    }
    scale_interval(iv, |x| x * factor)
}

/// `iv / divisor` (PG `interval_div`).
// CONTRACT
pub fn div(iv: &Interval, divisor: f64) -> Result<Interval> {
    if divisor == 0.0 {
        return Err(PgError::new(SqlState::DIVISION_BY_ZERO, "division by zero"));
    }
    if divisor.is_nan() {
        return Err(interval_oor());
    }
    if iv.is_infinite() {
        if divisor.is_infinite() {
            return Err(interval_oor());
        }
        if divisor < 0.0 {
            return neg(iv);
        }
        return Ok(*iv);
    }
    scale_interval(iv, |x| x / divisor)
}

/// `interval_in` (+ typmod).
pub fn input(ctx: &IoCtx, ty: TypeId, s: &str) -> Result<Datum> {
    Ok(Datum::Interval(parse_interval_typmod(s, ctx, ty.typmod)?))
}

/// `interval_out`.
pub fn output(ctx: &IoCtx, ty: TypeId, d: &Datum) -> Result<String> {
    let _ = ty;
    let iv = match d {
        Datum::Interval(v) => *v,
        other => return Err(type_mismatch("Interval", other)),
    };
    if iv == Interval::INFINITY {
        return Ok("infinity".to_string());
    }
    if iv == Interval::NEG_INFINITY {
        return Ok("-infinity".to_string());
    }
    encode::encode_interval(&interval_to_itm(&iv), ctx.config.interval_style)
}

/// `interval_recv` (+ typmod).
pub fn recv(ctx: &IoCtx, ty: TypeId, buf: &[u8]) -> Result<Datum> {
    let _ = ctx;
    if buf.len() < 16 {
        return Err(PgError::new(SqlState::PROTOCOL_VIOLATION, "insufficient data left in message"));
    }
    if buf.len() > 16 {
        return Err(PgError::new(SqlState::PROTOCOL_VIOLATION, "invalid message format"));
    }
    let micros = i64::from_be_bytes([buf[0], buf[1], buf[2], buf[3], buf[4], buf[5], buf[6], buf[7]]);
    let days = i32::from_be_bytes([buf[8], buf[9], buf[10], buf[11]]);
    let months = i32::from_be_bytes([buf[12], buf[13], buf[14], buf[15]]);
    let iv = adjust_for_typmod(Interval { months, days, micros }, ty.typmod)?;
    Ok(Datum::Interval(iv))
}

/// `interval_send`.
pub fn send(ctx: &IoCtx, ty: TypeId, d: &Datum) -> Result<Vec<u8>> {
    let _ = (ctx, ty);
    let iv = match d {
        Datum::Interval(v) => *v,
        other => return Err(type_mismatch("Interval", other)),
    };
    let mut out = Vec::with_capacity(16);
    out.extend_from_slice(&iv.micros.to_be_bytes());
    out.extend_from_slice(&iv.days.to_be_bytes());
    out.extend_from_slice(&iv.months.to_be_bytes());
    Ok(out)
}

/// Function registry hook (see `crate::func`).
pub(crate) fn lookup(proc: &BuiltinProc) -> Option<FuncImpl> {
    let f: fn(&[Datum], &FuncCtx) -> Result<Datum> = match proc.prosrc {
        "interval_pl" => f_pl,
        "interval_mi" => f_mi,
        "interval_um" => f_um,
        "interval_mul" => f_mul,
        "mul_d_interval" => f_mul_d,
        "interval_div" => f_div,
        "interval_eq" => f_eq,
        "interval_ne" => f_ne,
        "interval_lt" => f_lt,
        "interval_le" => f_le,
        "interval_gt" => f_gt,
        "interval_ge" => f_ge,
        "interval_cmp" => f_cmp,
        "interval_larger" => f_larger,
        "interval_smaller" => f_smaller,
        "interval_hash" => f_hash,
        "interval_hash_extended" => f_hash_extended,
        "interval_finite" => f_finite,
        "interval_justify_days" => f_justify_days,
        "interval_justify_hours" => f_justify_hours,
        "interval_justify_interval" => f_justify_interval,
        "interval_time" => f_time,
        "in_range_interval_interval" => f_in_range,
        _ => return None,
    };
    Some(FuncImpl::Scalar(f))
}

/// 0A000 for an Interval-module proc without implementation.
pub(crate) fn unsupported(proc: &BuiltinProc) -> PgError {
    PgError::unimplemented("TYP-29", format!("function {}", proc.name))
}

// ---------------------------------------------------------------------------------------------
// Small helpers
// ---------------------------------------------------------------------------------------------

fn interval_oor() -> PgError {
    out_of_range("interval")
}

fn fits_i32(x: f64) -> bool {
    x >= -2147483648.0 && x < 2147483648.0
}

fn fits_i64(x: f64) -> bool {
    x >= -9223372036854775808.0 && x < 9223372036854775808.0
}

fn finite_result(months: Option<i32>, days: Option<i32>, micros: Option<i64>) -> Result<Interval> {
    match (months, days, micros) {
        (Some(months), Some(days), Some(micros)) => {
            let r = Interval { months, days, micros };
            if r.is_infinite() {
                Err(interval_oor())
            } else {
                Ok(r)
            }
        }
        _ => Err(interval_oor()),
    }
}

fn sub(a: &Interval, b: &Interval) -> Result<Interval> {
    if *a == Interval::NEG_INFINITY {
        if *b == Interval::NEG_INFINITY {
            return Err(interval_oor());
        }
        return Ok(Interval::NEG_INFINITY);
    }
    if *a == Interval::INFINITY {
        if *b == Interval::INFINITY {
            return Err(interval_oor());
        }
        return Ok(Interval::INFINITY);
    }
    if *b == Interval::NEG_INFINITY {
        return Ok(Interval::INFINITY);
    }
    if *b == Interval::INFINITY {
        return Ok(Interval::NEG_INFINITY);
    }
    finite_result(a.months.checked_sub(b.months), a.days.checked_sub(b.days), a.micros.checked_sub(b.micros))
}

fn neg(a: &Interval) -> Result<Interval> {
    if *a == Interval::NEG_INFINITY {
        return Ok(Interval::INFINITY);
    }
    if *a == Interval::INFINITY {
        return Ok(Interval::NEG_INFINITY);
    }
    finite_result(a.months.checked_neg(), a.days.checked_neg(), a.micros.checked_neg())
}

fn sign_of(a: &Interval) -> i32 {
    match a.cmp_span().cmp(&0) {
        std::cmp::Ordering::Less => -1,
        std::cmp::Ordering::Equal => 0,
        std::cmp::Ordering::Greater => 1,
    }
}

/// Shared tail of `interval_mul` / `interval_div`: `op` multiplies or divides by the factor.
fn scale_interval(iv: &Interval, op: impl Fn(f64) -> f64) -> Result<Interval> {
    let orig_month = iv.months as f64;
    let orig_day = iv.days as f64;

    let rd = op(orig_month);
    if rd.is_nan() || !fits_i32(rd) {
        return Err(interval_oor());
    }
    let month = rd as i32;

    let rd = op(orig_day);
    if rd.is_nan() || !fits_i32(rd) {
        return Err(interval_oor());
    }
    let mut day = rd as i32;

    let mut month_remainder_days = (op(orig_month) - month as f64) * (DAYS_PER_MONTH as f64);
    month_remainder_days = tsround(month_remainder_days);
    let mut sec_remainder =
        (op(orig_day) - day as f64 + month_remainder_days - (month_remainder_days as i32) as f64) * (SECS_PER_DAY as f64);
    sec_remainder = tsround(sec_remainder);

    if sec_remainder.abs() >= SECS_PER_DAY as f64 {
        let extra = (sec_remainder / SECS_PER_DAY as f64) as i32;
        day = day.checked_add(extra).ok_or_else(interval_oor)?;
        sec_remainder -= (extra as f64) * (SECS_PER_DAY as f64);
    }

    day = day.checked_add(month_remainder_days as i32).ok_or_else(interval_oor)?;
    let rd = (op(iv.micros as f64) + sec_remainder * (USECS_PER_SEC as f64)).round_ties_even();
    if rd.is_nan() || !fits_i64(rd) {
        return Err(interval_oor());
    }
    let result = Interval { months: month, days: day, micros: rd as i64 };
    if result.is_infinite() {
        return Err(interval_oor());
    }
    Ok(result)
}

fn tsround(x: f64) -> f64 {
    (x * 1_000_000.0).round_ties_even() / 1_000_000.0
}

fn interval_to_itm(iv: &Interval) -> Itm {
    let mut time = iv.micros;
    let hour = time / USECS_PER_HOUR;
    time -= hour * USECS_PER_HOUR;
    let min = time / USECS_PER_MINUTE;
    time -= min * USECS_PER_MINUTE;
    let sec = time / USECS_PER_SEC;
    time -= sec * USECS_PER_SEC;
    Itm {
        year: iv.months / MONTHS_PER_YEAR,
        mon: iv.months % MONTHS_PER_YEAR,
        mday: iv.days,
        hour,
        min: min as i32,
        sec: sec as i32,
        usec: time as i32,
    }
}

// ---------------------------------------------------------------------------------------------
// Typmod
// ---------------------------------------------------------------------------------------------

fn adjust_for_typmod(mut iv: Interval, typmod: i32) -> Result<Interval> {
    const SCALES: [i64; 7] = [1_000_000, 100_000, 10_000, 1_000, 100, 10, 1];
    const OFFSETS: [i64; 7] = [500_000, 50_000, 5_000, 500, 50, 5, 0];

    if iv.is_infinite() || typmod < 0 {
        return Ok(iv);
    }
    let range = (typmod >> 16) & 0x7FFF;
    let precision = typmod & 0xFFFF;

    let (month, year, day, hour, minute, second) =
        (fld::MONTH, fld::YEAR, fld::DAY, fld::HOUR, fld::MINUTE, fld::SECOND);

    if range == fld::FULL_RANGE {
        // nothing
    } else if range == year {
        iv.months = (iv.months / MONTHS_PER_YEAR) * MONTHS_PER_YEAR;
        iv.days = 0;
        iv.micros = 0;
    } else if range == month || range == (year | month) {
        iv.days = 0;
        iv.micros = 0;
    } else if range == day {
        iv.micros = 0;
    } else if range == hour || range == (day | hour) {
        iv.micros = (iv.micros / USECS_PER_HOUR) * USECS_PER_HOUR;
    } else if range == minute || range == (day | hour | minute) || range == (hour | minute) {
        iv.micros = (iv.micros / USECS_PER_MINUTE) * USECS_PER_MINUTE;
    } else if range == second
        || range == (day | hour | minute | second)
        || range == (hour | minute | second)
        || range == (minute | second)
    {
        // fractional-second rounding handled below
    } else {
        return Err(PgError::internal(format!("unrecognized interval typmod: {typmod}")));
    }

    if precision != fld::FULL_PRECISION {
        if precision < 0 || precision > MAX_INTERVAL_PRECISION {
            return Err(PgError::new(
                SqlState::INVALID_PARAMETER_VALUE,
                format!("interval({precision}) precision must be between 0 and {MAX_INTERVAL_PRECISION}"),
            ));
        }
        let p = precision as usize;
        let adjusted = if iv.micros >= 0 {
            iv.micros.checked_add(OFFSETS[p])
        } else {
            iv.micros.checked_sub(OFFSETS[p])
        };
        let t = adjusted.ok_or_else(interval_oor)?;
        iv.micros = t - t % SCALES[p];
    }
    Ok(iv)
}

// ---------------------------------------------------------------------------------------------
// Input: DecodeInterval / DecodeISO8601Interval
// ---------------------------------------------------------------------------------------------

#[derive(Clone, Copy, Default)]
struct ItmIn {
    year: i32,
    mon: i32,
    mday: i32,
    usec: i64,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Dtype {
    Delta,
    Late,
    Early,
}

type DtResult<T> = std::result::Result<T, DtErr>;

fn parse_interval_typmod(s: &str, ctx: &IoCtx, typmod: i32) -> Result<Interval> {
    let range = if typmod >= 0 { (typmod >> 16) & 0x7FFF } else { fld::FULL_RANGE };

    let mut res: DtResult<(Dtype, ItmIn)> = match lexer::parse_datetime(s)? {
        Ok(fields) => decode_interval(&fields, range, ctx.config.interval_style),
        Err(e) => Err(e),
    };
    if matches!(res, Err(DtErr::BadFormat)) {
        res = decode_iso8601(s.as_bytes()).map(|itm| (Dtype::Delta, itm));
    }
    let (dtype, itm) = match res {
        Ok(v) => v,
        Err(e) => {
            let e = if e == DtErr::FieldOverflow { DtErr::IntervalOverflow } else { e };
            return Err(dt_parse_error(&e, s, "interval"));
        }
    };
    let iv = match dtype {
        Dtype::Delta => {
            let total = itm.year as i64 * MONTHS_PER_YEAR as i64 + itm.mon as i64;
            if total > i32::MAX as i64 || total < i32::MIN as i64 {
                return Err(interval_oor());
            }
            Interval { months: total as i32, days: itm.mday, micros: itm.usec }
        }
        Dtype::Late => Interval::INFINITY,
        Dtype::Early => Interval::NEG_INFINITY,
    };
    adjust_for_typmod(iv, typmod)
}

// Token-set bits (stand-ins for PG's DTK_M(type)).
const M_MICRO: u32 = 1 << 0;
const M_MILLI: u32 = 1 << 1;
const M_SECOND: u32 = 1 << 2;
const M_MINUTE: u32 = 1 << 3;
const M_HOUR: u32 = 1 << 4;
const M_DAY: u32 = 1 << 5;
const M_WEEK: u32 = 1 << 6;
const M_MONTH: u32 = 1 << 7;
const M_YEAR: u32 = 1 << 8;
const M_DECADE: u32 = 1 << 9;
const M_CENTURY: u32 = 1 << 10;
const M_MILLENNIUM: u32 = 1 << 11;
const M_ALL_SECS: u32 = M_SECOND | M_MILLI | M_MICRO;
const M_DATE: u32 = M_YEAR | M_MONTH | M_DAY;
const M_TIME: u32 = M_HOUR | M_MINUTE | M_ALL_SECS;

#[derive(Clone, Copy, PartialEq, Eq)]
enum Ty {
    Ignore,
    Micro,
    Milli,
    Second,
    Minute,
    Hour,
    Day,
    Week,
    Month,
    Year,
    Decade,
    Century,
    Millennium,
    /// Recognized but not usable in an interval (quarter, timezone, "ago" marker, reserved words).
    Other,
}

enum UnitTok {
    Ignore,
    Ago,
    Units(Ty),
}

/// PG `deltatktbl` (matching compares at most TOKMAXLEN = 10 bytes).
static DELTA_TABLE: &[(&str, UnitTok)] = &[
    ("@", UnitTok::Ignore),
    ("ago", UnitTok::Ago),
    ("c", UnitTok::Units(Ty::Century)),
    ("cent", UnitTok::Units(Ty::Century)),
    ("centuries", UnitTok::Units(Ty::Century)),
    ("century", UnitTok::Units(Ty::Century)),
    ("d", UnitTok::Units(Ty::Day)),
    ("day", UnitTok::Units(Ty::Day)),
    ("days", UnitTok::Units(Ty::Day)),
    ("dec", UnitTok::Units(Ty::Decade)),
    ("decade", UnitTok::Units(Ty::Decade)),
    ("decades", UnitTok::Units(Ty::Decade)),
    ("decs", UnitTok::Units(Ty::Decade)),
    ("h", UnitTok::Units(Ty::Hour)),
    ("hour", UnitTok::Units(Ty::Hour)),
    ("hours", UnitTok::Units(Ty::Hour)),
    ("hr", UnitTok::Units(Ty::Hour)),
    ("hrs", UnitTok::Units(Ty::Hour)),
    ("m", UnitTok::Units(Ty::Minute)),
    ("microsecon", UnitTok::Units(Ty::Micro)),
    ("mil", UnitTok::Units(Ty::Millennium)),
    ("millennia", UnitTok::Units(Ty::Millennium)),
    ("millennium", UnitTok::Units(Ty::Millennium)),
    ("millisecon", UnitTok::Units(Ty::Milli)),
    ("mils", UnitTok::Units(Ty::Millennium)),
    ("min", UnitTok::Units(Ty::Minute)),
    ("mins", UnitTok::Units(Ty::Minute)),
    ("minute", UnitTok::Units(Ty::Minute)),
    ("minutes", UnitTok::Units(Ty::Minute)),
    ("mon", UnitTok::Units(Ty::Month)),
    ("mons", UnitTok::Units(Ty::Month)),
    ("month", UnitTok::Units(Ty::Month)),
    ("months", UnitTok::Units(Ty::Month)),
    ("ms", UnitTok::Units(Ty::Milli)),
    ("msec", UnitTok::Units(Ty::Milli)),
    ("mseconds", UnitTok::Units(Ty::Milli)),
    ("msecs", UnitTok::Units(Ty::Milli)),
    ("qtr", UnitTok::Units(Ty::Other)),
    ("quarter", UnitTok::Units(Ty::Other)),
    ("s", UnitTok::Units(Ty::Second)),
    ("sec", UnitTok::Units(Ty::Second)),
    ("second", UnitTok::Units(Ty::Second)),
    ("seconds", UnitTok::Units(Ty::Second)),
    ("secs", UnitTok::Units(Ty::Second)),
    ("timezone", UnitTok::Units(Ty::Other)),
    ("timezone_h", UnitTok::Units(Ty::Other)),
    ("timezone_m", UnitTok::Units(Ty::Other)),
    ("us", UnitTok::Units(Ty::Micro)),
    ("usec", UnitTok::Units(Ty::Micro)),
    ("useconds", UnitTok::Units(Ty::Micro)),
    ("usecs", UnitTok::Units(Ty::Micro)),
    ("w", UnitTok::Units(Ty::Week)),
    ("week", UnitTok::Units(Ty::Week)),
    ("weeks", UnitTok::Units(Ty::Week)),
    ("y", UnitTok::Units(Ty::Year)),
    ("year", UnitTok::Units(Ty::Year)),
    ("years", UnitTok::Units(Ty::Year)),
    ("yr", UnitTok::Units(Ty::Year)),
    ("yrs", UnitTok::Units(Ty::Year)),
];

const TOKMAXLEN: usize = 10;

fn tok_eq(a: &str, b: &str) -> bool {
    let a = a.as_bytes();
    let b = b.as_bytes();
    a[..a.len().min(TOKMAXLEN)] == b[..b.len().min(TOKMAXLEN)]
}

fn lookup_unit(word: &str) -> Option<&'static UnitTok> {
    DELTA_TABLE.iter().find(|(tok, _)| tok_eq(word, tok)).map(|(_, v)| v)
}

enum SpecialTok {
    Ignore,
    Late,
    Early,
    Other,
}

/// The only `datetktbl` entries that matter to interval input.
fn lookup_special(word: &str) -> SpecialTok {
    if tok_eq(word, "at") || tok_eq(word, "on") {
        SpecialTok::Ignore
    } else if tok_eq(word, "infinity") || tok_eq(word, "+infinity") {
        SpecialTok::Late
    } else if tok_eq(word, "-infinity") {
        SpecialTok::Early
    } else {
        SpecialTok::Other
    }
}

/// C `strtoi64` prefix parse: `(value, bytes consumed, out_of_range)`. No digits → consumed 0.
fn strtoi64(s: &[u8]) -> (i64, usize, bool) {
    let mut i = 0;
    while i < s.len() && matches!(s[i], b' ' | b'\t' | b'\n' | b'\r' | 0x0b | 0x0c) {
        i += 1;
    }
    let mut neg = false;
    if i < s.len() && (s[i] == b'-' || s[i] == b'+') {
        neg = s[i] == b'-';
        i += 1;
    }
    let ds = i;
    let mut acc: i128 = 0;
    let mut ovf = false;
    while i < s.len() && s[i].is_ascii_digit() {
        if !ovf {
            acc = acc * 10 + (s[i] - b'0') as i128;
            if acc > (i64::MAX as i128) + 1 {
                ovf = true;
            }
        }
        i += 1;
    }
    if i == ds {
        return (0, 0, false);
    }
    let v = if neg { -acc } else { acc };
    if ovf || v > i64::MAX as i128 || v < i64::MIN as i128 {
        return (if neg { i64::MIN } else { i64::MAX }, i, true);
    }
    (v as i64, i, false)
}

/// C `strtoint`: like `strtoi64` but out of range for int32 also reports ERANGE.
fn strtoint(s: &[u8]) -> (i32, usize, bool) {
    let (v, n, ovf) = strtoi64(s);
    if ovf || v > i32::MAX as i64 || v < i32::MIN as i64 {
        return (if v < 0 { i32::MIN } else { i32::MAX }, n, true);
    }
    (v as i32, n, false)
}

fn mul_add(val: i64, mult: i64, sum: &mut i64) -> bool {
    match val.checked_mul(mult).and_then(|p| sum.checked_add(p)) {
        Some(s) => {
            *sum = s;
            true
        }
        None => false,
    }
}

fn adjust_fract_micro(frac: f64, scale: i64, itm: &mut ItmIn) -> bool {
    if frac == 0.0 {
        return true;
    }
    let mut f = frac * scale as f64;
    let mut usec = f as i64;
    f -= usec as f64;
    if f > 0.5 {
        usec += 1;
    } else if f < -0.5 {
        usec -= 1;
    }
    match itm.usec.checked_add(usec) {
        Some(v) => {
            itm.usec = v;
            true
        }
        None => false,
    }
}

fn adjust_fract_days(frac: f64, scale: i32, itm: &mut ItmIn) -> bool {
    if frac == 0.0 {
        return true;
    }
    let mut f = frac * scale as f64;
    let extra = f as i32;
    match itm.mday.checked_add(extra) {
        Some(v) => itm.mday = v,
        None => return false,
    }
    f -= extra as f64;
    adjust_fract_micro(f, USECS_PER_DAY, itm)
}

fn adjust_fract_years(frac: f64, scale: i32, itm: &mut ItmIn) -> bool {
    let extra = (frac * scale as f64 * MONTHS_PER_YEAR as f64).round_ties_even() as i32;
    match itm.mon.checked_add(extra) {
        Some(v) => {
            itm.mon = v;
            true
        }
        None => false,
    }
}

fn adjust_micro(val: i64, fval: f64, scale: i64, itm: &mut ItmIn) -> bool {
    mul_add(val, scale, &mut itm.usec) && adjust_fract_micro(fval, scale, itm)
}

fn adjust_days(val: i64, scale: i32, itm: &mut ItmIn) -> bool {
    if val < i32::MIN as i64 || val > i32::MAX as i64 {
        return false;
    }
    match (val as i32).checked_mul(scale).and_then(|d| itm.mday.checked_add(d)) {
        Some(v) => {
            itm.mday = v;
            true
        }
        None => false,
    }
}

fn adjust_months(val: i64, itm: &mut ItmIn) -> bool {
    if val < i32::MIN as i64 || val > i32::MAX as i64 {
        return false;
    }
    match itm.mon.checked_add(val as i32) {
        Some(v) => {
            itm.mon = v;
            true
        }
        None => false,
    }
}

fn adjust_years(val: i64, scale: i32, itm: &mut ItmIn) -> bool {
    if val < i32::MIN as i64 || val > i32::MAX as i64 {
        return false;
    }
    match (val as i32).checked_mul(scale).and_then(|y| itm.year.checked_add(y)) {
        Some(v) => {
            itm.year = v;
            true
        }
        None => false,
    }
}

/// PG `ParseFraction`: `s` starts at the '.'.
fn parse_fraction(s: &[u8]) -> DtResult<f64> {
    if s.len() <= 1 {
        return Ok(0.0);
    }
    if !s[1..].iter().all(|c| c.is_ascii_digit()) {
        return Err(DtErr::BadFormat);
    }
    let text = std::str::from_utf8(s).map_err(|_| DtErr::BadFormat)?;
    text.parse::<f64>().map_err(|_| DtErr::BadFormat)
}

fn parse_fractional_second(s: &[u8]) -> DtResult<i32> {
    let frac = parse_fraction(s)?;
    Ok((frac * 1_000_000.0).round_ties_even() as i32)
}

/// PG `DecodeTimeCommon`: returns (hour, min, sec, usec).
fn decode_time_common(s: &[u8], range: i32) -> DtResult<(i64, i32, i32, i32)> {
    let (mut hour, n, ovf) = strtoi64(s);
    if ovf {
        return Err(DtErr::FieldOverflow);
    }
    let rest = &s[n..];
    if rest.first() != Some(&b':') {
        return Err(DtErr::BadFormat);
    }
    let (mut min, n2, ovf) = strtoint(&rest[1..]);
    if ovf {
        return Err(DtErr::FieldOverflow);
    }
    let rest = &rest[1 + n2..];
    let mut sec = 0i32;
    let mut fsec = 0i32;
    if rest.is_empty() {
        if range == (fld::MINUTE | fld::SECOND) {
            if hour > i32::MAX as i64 || hour < i32::MIN as i64 {
                return Err(DtErr::FieldOverflow);
            }
            sec = min;
            min = hour as i32;
            hour = 0;
        }
    } else if rest[0] == b'.' {
        fsec = parse_fractional_second(rest)?;
        if hour > i32::MAX as i64 || hour < i32::MIN as i64 {
            return Err(DtErr::FieldOverflow);
        }
        sec = min;
        min = hour as i32;
        hour = 0;
    } else if rest[0] == b':' {
        let (sv, n3, ovf) = strtoint(&rest[1..]);
        if ovf {
            return Err(DtErr::FieldOverflow);
        }
        sec = sv;
        let rest = &rest[1 + n3..];
        if rest.first() == Some(&b'.') {
            fsec = parse_fractional_second(rest)?;
        } else if !rest.is_empty() {
            return Err(DtErr::BadFormat);
        }
    } else {
        return Err(DtErr::BadFormat);
    }

    if hour < 0 || min < 0 || min > 59 || sec < 0 || sec > 60 || fsec < 0 || fsec > USECS_PER_SEC as i32 {
        return Err(DtErr::FieldOverflow);
    }
    Ok((hour, min, sec, fsec))
}

/// PG `DecodeTimeForInterval`.
fn decode_time_for_interval(s: &[u8], range: i32, itm: &mut ItmIn) -> DtResult<()> {
    let (hour, min, sec, usec) = decode_time_common(s, range)?;
    itm.usec = usec as i64;
    if !mul_add(hour, USECS_PER_HOUR, &mut itm.usec)
        || !mul_add(min as i64, USECS_PER_MINUTE, &mut itm.usec)
        || !mul_add(sec as i64, USECS_PER_SEC, &mut itm.usec)
    {
        return Err(DtErr::FieldOverflow);
    }
    Ok(())
}

fn range_default_type(range: i32) -> Ty {
    let (month, year, day, hour, minute) = (fld::MONTH, fld::YEAR, fld::DAY, fld::HOUR, fld::MINUTE);
    if range == year {
        Ty::Year
    } else if range == month || range == (year | month) {
        Ty::Month
    } else if range == day {
        Ty::Day
    } else if range == hour || range == (day | hour) {
        Ty::Hour
    } else if range == minute || range == (hour | minute) || range == (day | hour | minute) {
        Ty::Minute
    } else {
        Ty::Second
    }
}

/// PG `DecodeInterval`.
fn decode_interval(fields: &[Field], range: i32, style: IntervalStyle) -> DtResult<(Dtype, ItmIn)> {
    let nf = fields.len();
    let mut dtype = Dtype::Delta;
    let mut itm = ItmIn::default();
    let mut ty = Ty::Ignore;
    let mut is_before = false;
    let mut parsing_unit_val = false;
    let mut fmask: u32 = 0;

    let mut force_negative = false;
    if style == IntervalStyle::SqlStandard && nf > 0 && fields[0].text.starts_with('-') {
        force_negative = true;
        for f in &fields[1..] {
            if f.text.starts_with('-') || f.text.starts_with('+') {
                force_negative = false;
                break;
            }
        }
    }

    for i in (0..nf).rev() {
        let field = &fields[i];
        let text = field.text.as_bytes();
        let mut tmask: u32 = 0;

        match field.kind {
            FieldKind::Time => {
                decode_time_for_interval(text, range, &mut itm)?;
                tmask = M_TIME;
                if force_negative && itm.usec > 0 {
                    itm.usec = -itm.usec;
                }
                ty = Ty::Day;
                parsing_unit_val = false;
            }
            FieldKind::Tz | FieldKind::Date | FieldKind::Number => {
                let mut handled = false;
                if field.kind == FieldKind::Tz {
                    // signed hh:mm or hh:mm:ss
                    if text.len() > 1 && text[1..].contains(&b':') {
                        let mut m = 0u32;
                        if decode_time_for_interval(&text[1..], range, &mut itm).is_ok() {
                            m = M_TIME;
                            if text[0] == b'-' {
                                if itm.usec == i64::MIN {
                                    return Err(DtErr::FieldOverflow);
                                }
                                itm.usec = -itm.usec;
                            }
                            if force_negative && itm.usec > 0 {
                                itm.usec = -itm.usec;
                            }
                            ty = Ty::Day;
                            parsing_unit_val = false;
                            handled = true;
                        }
                        if handled {
                            tmask = m;
                        }
                    }
                }
                if !handled {
                    if ty == Ty::Ignore {
                        ty = range_default_type(range);
                    }
                    let (mut val, n, ovf) = strtoi64(text);
                    if ovf {
                        return Err(DtErr::FieldOverflow);
                    }
                    let rest = &text[n..];
                    let mut fval: f64;
                    if rest.first() == Some(&b'-') {
                        // SQL "years-months" syntax
                        let (mut val2, n2, ovf2) = strtoint(&rest[1..]);
                        if ovf2 || val2 < 0 || val2 >= MONTHS_PER_YEAR {
                            return Err(DtErr::FieldOverflow);
                        }
                        if !rest[1 + n2..].is_empty() {
                            return Err(DtErr::BadFormat);
                        }
                        ty = Ty::Month;
                        if text[0] == b'-' {
                            val2 = -val2;
                        }
                        val = val.checked_mul(MONTHS_PER_YEAR as i64).ok_or(DtErr::FieldOverflow)?;
                        val = val.checked_add(val2 as i64).ok_or(DtErr::FieldOverflow)?;
                        fval = 0.0;
                    } else if rest.first() == Some(&b'.') {
                        fval = parse_fraction(rest)?;
                        if text[0] == b'-' {
                            fval = -fval;
                        }
                    } else if rest.is_empty() {
                        fval = 0.0;
                    } else {
                        return Err(DtErr::BadFormat);
                    }

                    if force_negative {
                        if val > 0 {
                            val = -val;
                        }
                        if fval > 0.0 {
                            fval = -fval;
                        }
                    }

                    let ovf = DtErr::FieldOverflow;
                    match ty {
                        Ty::Micro => {
                            if !adjust_micro(val, fval, 1, &mut itm) {
                                return Err(ovf);
                            }
                            tmask = M_MICRO;
                        }
                        Ty::Milli => {
                            if !adjust_micro(val, fval, 1000, &mut itm) {
                                return Err(ovf);
                            }
                            tmask = M_MILLI;
                        }
                        Ty::Second => {
                            if !adjust_micro(val, fval, USECS_PER_SEC, &mut itm) {
                                return Err(ovf);
                            }
                            tmask = if fval == 0.0 { M_SECOND } else { M_ALL_SECS };
                        }
                        Ty::Minute => {
                            if !adjust_micro(val, fval, USECS_PER_MINUTE, &mut itm) {
                                return Err(ovf);
                            }
                            tmask = M_MINUTE;
                        }
                        Ty::Hour => {
                            if !adjust_micro(val, fval, USECS_PER_HOUR, &mut itm) {
                                return Err(ovf);
                            }
                            tmask = M_HOUR;
                            ty = Ty::Day;
                        }
                        Ty::Day => {
                            if !adjust_days(val, 1, &mut itm) || !adjust_fract_micro(fval, USECS_PER_DAY, &mut itm) {
                                return Err(ovf);
                            }
                            tmask = M_DAY;
                        }
                        Ty::Week => {
                            if !adjust_days(val, 7, &mut itm) || !adjust_fract_days(fval, 7, &mut itm) {
                                return Err(ovf);
                            }
                            tmask = M_WEEK;
                        }
                        Ty::Month => {
                            if !adjust_months(val, &mut itm) || !adjust_fract_days(fval, DAYS_PER_MONTH, &mut itm) {
                                return Err(ovf);
                            }
                            tmask = M_MONTH;
                        }
                        Ty::Year => {
                            if !adjust_years(val, 1, &mut itm) || !adjust_fract_years(fval, 1, &mut itm) {
                                return Err(ovf);
                            }
                            tmask = M_YEAR;
                        }
                        Ty::Decade => {
                            if !adjust_years(val, 10, &mut itm) || !adjust_fract_years(fval, 10, &mut itm) {
                                return Err(ovf);
                            }
                            tmask = M_DECADE;
                        }
                        Ty::Century => {
                            if !adjust_years(val, 100, &mut itm) || !adjust_fract_years(fval, 100, &mut itm) {
                                return Err(ovf);
                            }
                            tmask = M_CENTURY;
                        }
                        Ty::Millennium => {
                            if !adjust_years(val, 1000, &mut itm) || !adjust_fract_years(fval, 1000, &mut itm) {
                                return Err(ovf);
                            }
                            tmask = M_MILLENNIUM;
                        }
                        Ty::Ignore | Ty::Other => return Err(DtErr::BadFormat),
                    }
                    parsing_unit_val = false;
                }
            }
            FieldKind::String | FieldKind::Special => {
                if parsing_unit_val {
                    return Err(DtErr::BadFormat);
                }
                match lookup_unit(&field.text) {
                    Some(UnitTok::Ignore) => {
                        ty = Ty::Ignore;
                        continue;
                    }
                    Some(UnitTok::Units(t)) => {
                        ty = *t;
                        parsing_unit_val = true;
                    }
                    Some(UnitTok::Ago) => {
                        if i != nf - 1 {
                            return Err(DtErr::BadFormat);
                        }
                        is_before = true;
                        ty = Ty::Other;
                    }
                    None => match lookup_special(&field.text) {
                        SpecialTok::Ignore => {
                            ty = Ty::Ignore;
                            continue;
                        }
                        SpecialTok::Late | SpecialTok::Early => {
                            let late = matches!(lookup_special(&field.text), SpecialTok::Late);
                            tmask = M_DATE | M_TIME;
                            if i != nf - 1 {
                                return Err(DtErr::BadFormat);
                            }
                            dtype = if late { Dtype::Late } else { Dtype::Early };
                            ty = Ty::Other;
                        }
                        SpecialTok::Other => return Err(DtErr::BadFormat),
                    },
                }
            }
        }

        if tmask & fmask != 0 {
            return Err(DtErr::BadFormat);
        }
        fmask |= tmask;
    }

    if fmask == 0 {
        return Err(DtErr::BadFormat);
    }
    if parsing_unit_val {
        return Err(DtErr::BadFormat);
    }
    if is_before {
        if itm.usec == i64::MIN || itm.mday == i32::MIN || itm.mon == i32::MIN || itm.year == i32::MIN {
            return Err(DtErr::FieldOverflow);
        }
        itm.usec = -itm.usec;
        itm.mday = -itm.mday;
        itm.mon = -itm.mon;
        itm.year = -itm.year;
    }
    Ok((dtype, itm))
}

fn byte_at(s: &[u8], p: usize) -> u8 {
    if p < s.len() {
        s[p]
    } else {
        0
    }
}

/// C `strtod` prefix scan for the shapes ISO 8601 input can contain (no hex, no inf/nan).
/// Returns (value, end index).
fn strtod_prefix(s: &[u8], start: usize) -> Option<(f64, usize)> {
    let mut i = start;
    if byte_at(s, i) == b'-' || byte_at(s, i) == b'+' {
        i += 1;
    }
    let ds = i;
    while byte_at(s, i).is_ascii_digit() {
        i += 1;
    }
    let mut ndigits = i - ds;
    if byte_at(s, i) == b'.' {
        i += 1;
        let fs = i;
        while byte_at(s, i).is_ascii_digit() {
            i += 1;
        }
        ndigits += i - fs;
    }
    if ndigits == 0 {
        return None;
    }
    if byte_at(s, i) == b'e' || byte_at(s, i) == b'E' {
        let mut j = i + 1;
        if byte_at(s, j) == b'+' || byte_at(s, j) == b'-' {
            j += 1;
        }
        if byte_at(s, j).is_ascii_digit() {
            while byte_at(s, j).is_ascii_digit() {
                j += 1;
            }
            i = j;
        }
    }
    let text = std::str::from_utf8(&s[start..i]).ok()?;
    let v: f64 = text.parse().ok()?;
    if v.is_infinite() {
        return None; // ERANGE
    }
    if v == 0.0 {
        // underflow (ERANGE) if any mantissa digit is nonzero
        let mantissa_end = text.find(|c| c == 'e' || c == 'E').unwrap_or(text.len());
        if text[..mantissa_end].bytes().any(|c| (b'1'..=b'9').contains(&c)) {
            return None;
        }
    }
    Some((v, i))
}

/// PG `ParseISO8601Number`: returns (ipart, fpart, end index).
fn parse_iso_number(s: &[u8], p: usize) -> DtResult<(i64, f64, usize)> {
    let c = byte_at(s, p);
    if !(c.is_ascii_digit() || c == b'-' || c == b'.') {
        return Err(DtErr::BadFormat);
    }
    if c == b'-' {
        let rest = &s[p + 1..];
        if rest.len() >= 3 && (rest[..3].eq_ignore_ascii_case(b"inf") || rest[..3].eq_ignore_ascii_case(b"nan")) {
            return Err(DtErr::FieldOverflow);
        }
    }
    let (val, end) = strtod_prefix(s, p).ok_or(DtErr::BadFormat)?;
    if val.is_nan() || val < -1.0e15 || val > 1.0e15 {
        return Err(DtErr::FieldOverflow);
    }
    let ipart = if val >= 0.0 { val.floor() as i64 } else { -((-val).floor() as i64) };
    let fpart = val - ipart as f64;
    Ok((ipart, fpart, end))
}

fn iso_integer_width(s: &[u8], mut p: usize) -> usize {
    if byte_at(s, p) == b'-' {
        p += 1;
    }
    let mut n = 0;
    while byte_at(s, p + n).is_ascii_digit() {
        n += 1;
    }
    n
}

/// PG `DecodeISO8601Interval`.
fn decode_iso8601(s: &[u8]) -> DtResult<ItmIn> {
    let mut itm = ItmIn::default();
    let ovf = DtErr::FieldOverflow;
    if s.len() < 2 || s[0] != b'P' {
        return Err(DtErr::BadFormat);
    }
    let mut p = 1usize;
    let mut datepart = true;
    let mut havefield = false;

    while p < s.len() {
        if s[p] == b'T' {
            datepart = false;
            havefield = false;
            p += 1;
            continue;
        }
        let fieldstart = p;
        let (val, fval, end) = parse_iso_number(s, p)?;
        p = end;
        let unit = byte_at(s, p);
        p += 1;

        if datepart {
            match unit {
                b'Y' => {
                    if !adjust_years(val, 1, &mut itm) || !adjust_fract_years(fval, 1, &mut itm) {
                        return Err(ovf);
                    }
                }
                b'M' => {
                    if !adjust_months(val, &mut itm) || !adjust_fract_days(fval, DAYS_PER_MONTH, &mut itm) {
                        return Err(ovf);
                    }
                }
                b'W' => {
                    if !adjust_days(val, 7, &mut itm) || !adjust_fract_days(fval, 7, &mut itm) {
                        return Err(ovf);
                    }
                }
                b'D' => {
                    if !adjust_days(val, 1, &mut itm) || !adjust_fract_micro(fval, USECS_PER_DAY, &mut itm) {
                        return Err(ovf);
                    }
                }
                b'T' | 0 | b'-' => {
                    if (unit == b'T' || unit == 0) && iso_integer_width(s, fieldstart) == 8 && !havefield {
                        if !adjust_years(val / 10000, 1, &mut itm)
                            || !adjust_months((val / 100) % 100, &mut itm)
                            || !adjust_days(val % 100, 1, &mut itm)
                            || !adjust_fract_micro(fval, USECS_PER_DAY, &mut itm)
                        {
                            return Err(ovf);
                        }
                        if unit == 0 {
                            return Ok(itm);
                        }
                        datepart = false;
                        havefield = false;
                        continue;
                    }
                    // extended alternative format
                    if havefield {
                        return Err(DtErr::BadFormat);
                    }
                    if !adjust_years(val, 1, &mut itm) || !adjust_fract_years(fval, 1, &mut itm) {
                        return Err(ovf);
                    }
                    if unit == 0 {
                        return Ok(itm);
                    }
                    if unit == b'T' {
                        datepart = false;
                        havefield = false;
                        continue;
                    }

                    let (v, f, e) = parse_iso_number(s, p)?;
                    p = e;
                    if !adjust_months(v, &mut itm) || !adjust_fract_days(f, DAYS_PER_MONTH, &mut itm) {
                        return Err(ovf);
                    }
                    if p >= s.len() {
                        return Ok(itm);
                    }
                    if s[p] == b'T' {
                        datepart = false;
                        havefield = false;
                        continue;
                    }
                    if s[p] != b'-' {
                        return Err(DtErr::BadFormat);
                    }
                    p += 1;

                    let (v, f, e) = parse_iso_number(s, p)?;
                    p = e;
                    if !adjust_days(v, 1, &mut itm) || !adjust_fract_micro(f, USECS_PER_DAY, &mut itm) {
                        return Err(ovf);
                    }
                    if p >= s.len() {
                        return Ok(itm);
                    }
                    if s[p] == b'T' {
                        datepart = false;
                        havefield = false;
                        continue;
                    }
                    return Err(DtErr::BadFormat);
                }
                _ => return Err(DtErr::BadFormat),
            }
        } else {
            match unit {
                b'H' => {
                    if !adjust_micro(val, fval, USECS_PER_HOUR, &mut itm) {
                        return Err(ovf);
                    }
                }
                b'M' => {
                    if !adjust_micro(val, fval, USECS_PER_MINUTE, &mut itm) {
                        return Err(ovf);
                    }
                }
                b'S' => {
                    if !adjust_micro(val, fval, USECS_PER_SEC, &mut itm) {
                        return Err(ovf);
                    }
                }
                0 | b':' => {
                    if unit == 0 && iso_integer_width(s, fieldstart) == 6 && !havefield {
                        if !adjust_micro(val / 10000, 0.0, USECS_PER_HOUR, &mut itm)
                            || !adjust_micro((val / 100) % 100, 0.0, USECS_PER_MINUTE, &mut itm)
                            || !adjust_micro(val % 100, 0.0, USECS_PER_SEC, &mut itm)
                            || !adjust_fract_micro(fval, 1, &mut itm)
                        {
                            return Err(ovf);
                        }
                        return Ok(itm);
                    }
                    if havefield {
                        return Err(DtErr::BadFormat);
                    }
                    if !adjust_micro(val, fval, USECS_PER_HOUR, &mut itm) {
                        return Err(ovf);
                    }
                    if unit == 0 {
                        return Ok(itm);
                    }

                    let (v, f, e) = parse_iso_number(s, p)?;
                    p = e;
                    if !adjust_micro(v, f, USECS_PER_MINUTE, &mut itm) {
                        return Err(ovf);
                    }
                    if p >= s.len() {
                        return Ok(itm);
                    }
                    if s[p] != b':' {
                        return Err(DtErr::BadFormat);
                    }
                    p += 1;

                    let (v, f, e) = parse_iso_number(s, p)?;
                    p = e;
                    if !adjust_micro(v, f, USECS_PER_SEC, &mut itm) {
                        return Err(ovf);
                    }
                    if p >= s.len() {
                        return Ok(itm);
                    }
                    return Err(DtErr::BadFormat);
                }
                _ => return Err(DtErr::BadFormat),
            }
        }
        havefield = true;
    }
    Ok(itm)
}

// ---------------------------------------------------------------------------------------------
// justify_*
// ---------------------------------------------------------------------------------------------

fn justify_days(span: &Interval) -> Result<Interval> {
    let mut r = *span;
    if r.is_infinite() {
        return Ok(r);
    }
    let whole = r.days / DAYS_PER_MONTH;
    r.days -= whole * DAYS_PER_MONTH;
    r.months = r.months.checked_add(whole).ok_or_else(interval_oor)?;
    if r.months > 0 && r.days < 0 {
        r.days += DAYS_PER_MONTH;
        r.months -= 1;
    } else if r.months < 0 && r.days > 0 {
        r.days -= DAYS_PER_MONTH;
        r.months += 1;
    }
    Ok(r)
}

fn justify_hours(span: &Interval) -> Result<Interval> {
    let mut r = *span;
    if r.is_infinite() {
        return Ok(r);
    }
    let whole = r.micros / USECS_PER_DAY;
    r.micros -= whole * USECS_PER_DAY;
    // |whole| <= ~1.07e8, always fits i32
    r.days = r.days.checked_add(whole as i32).ok_or_else(interval_oor)?;
    if r.days > 0 && r.micros < 0 {
        r.micros += USECS_PER_DAY;
        r.days -= 1;
    } else if r.days < 0 && r.micros > 0 {
        r.micros -= USECS_PER_DAY;
        r.days += 1;
    }
    Ok(r)
}

fn justify_interval(span: &Interval) -> Result<Interval> {
    let mut r = *span;
    if r.is_infinite() {
        return Ok(r);
    }
    if (r.days > 0 && r.micros > 0) || (r.days < 0 && r.micros < 0) {
        let whole = r.days / DAYS_PER_MONTH;
        r.days -= whole * DAYS_PER_MONTH;
        r.months = r.months.checked_add(whole).ok_or_else(interval_oor)?;
    }
    let whole_day = r.micros / USECS_PER_DAY;
    r.micros -= whole_day * USECS_PER_DAY;
    r.days = r.days.checked_add(whole_day as i32).ok_or_else(interval_oor)?;

    let whole = r.days / DAYS_PER_MONTH;
    r.days -= whole * DAYS_PER_MONTH;
    r.months = r.months.checked_add(whole).ok_or_else(interval_oor)?;

    if r.months > 0 && (r.days < 0 || (r.days == 0 && r.micros < 0)) {
        r.days += DAYS_PER_MONTH;
        r.months -= 1;
    } else if r.months < 0 && (r.days > 0 || (r.days == 0 && r.micros > 0)) {
        r.days -= DAYS_PER_MONTH;
        r.months += 1;
    }

    if r.days > 0 && r.micros < 0 {
        r.micros += USECS_PER_DAY;
        r.days -= 1;
    } else if r.days < 0 && r.micros > 0 {
        r.micros -= USECS_PER_DAY;
        r.days += 1;
    }
    Ok(r)
}

// ---------------------------------------------------------------------------------------------
// Registry entry points
// ---------------------------------------------------------------------------------------------

fn arg_interval(args: &[Datum], i: usize) -> Result<Interval> {
    match args.get(i) {
        Some(Datum::Interval(v)) => Ok(*v),
        Some(other) => Err(type_mismatch("Interval", other)),
        None => Err(PgError::internal("interval function called with too few arguments")),
    }
}

fn arg_f64(args: &[Datum], i: usize) -> Result<f64> {
    match args.get(i) {
        Some(Datum::Float8(v)) => Ok(*v),
        Some(other) => Err(type_mismatch("Float8", other)),
        None => Err(PgError::internal("interval function called with too few arguments")),
    }
}

fn arg_bool(args: &[Datum], i: usize) -> Result<bool> {
    match args.get(i) {
        Some(Datum::Bool(v)) => Ok(*v),
        Some(other) => Err(type_mismatch("Bool", other)),
        None => Err(PgError::internal("interval function called with too few arguments")),
    }
}

fn cmp_args(args: &[Datum]) -> Result<std::cmp::Ordering> {
    let a = arg_interval(args, 0)?;
    let b = arg_interval(args, 1)?;
    Ok(a.cmp_span().cmp(&b.cmp_span()))
}

fn f_pl(args: &[Datum], _ctx: &FuncCtx) -> Result<Datum> {
    Ok(Datum::Interval(add(&arg_interval(args, 0)?, &arg_interval(args, 1)?)?))
}

fn f_mi(args: &[Datum], _ctx: &FuncCtx) -> Result<Datum> {
    Ok(Datum::Interval(sub(&arg_interval(args, 0)?, &arg_interval(args, 1)?)?))
}

fn f_um(args: &[Datum], _ctx: &FuncCtx) -> Result<Datum> {
    Ok(Datum::Interval(neg(&arg_interval(args, 0)?)?))
}

fn f_mul(args: &[Datum], _ctx: &FuncCtx) -> Result<Datum> {
    Ok(Datum::Interval(mul(&arg_interval(args, 0)?, arg_f64(args, 1)?)?))
}

fn f_mul_d(args: &[Datum], _ctx: &FuncCtx) -> Result<Datum> {
    Ok(Datum::Interval(mul(&arg_interval(args, 1)?, arg_f64(args, 0)?)?))
}

fn f_div(args: &[Datum], _ctx: &FuncCtx) -> Result<Datum> {
    Ok(Datum::Interval(div(&arg_interval(args, 0)?, arg_f64(args, 1)?)?))
}

fn f_eq(args: &[Datum], _ctx: &FuncCtx) -> Result<Datum> {
    Ok(Datum::Bool(cmp_args(args)?.is_eq()))
}

fn f_ne(args: &[Datum], _ctx: &FuncCtx) -> Result<Datum> {
    Ok(Datum::Bool(cmp_args(args)?.is_ne()))
}

fn f_lt(args: &[Datum], _ctx: &FuncCtx) -> Result<Datum> {
    Ok(Datum::Bool(cmp_args(args)?.is_lt()))
}

fn f_le(args: &[Datum], _ctx: &FuncCtx) -> Result<Datum> {
    Ok(Datum::Bool(cmp_args(args)?.is_le()))
}

fn f_gt(args: &[Datum], _ctx: &FuncCtx) -> Result<Datum> {
    Ok(Datum::Bool(cmp_args(args)?.is_gt()))
}

fn f_ge(args: &[Datum], _ctx: &FuncCtx) -> Result<Datum> {
    Ok(Datum::Bool(cmp_args(args)?.is_ge()))
}

fn f_cmp(args: &[Datum], _ctx: &FuncCtx) -> Result<Datum> {
    Ok(Datum::Int4(cmp_args(args)? as i32))
}

fn f_larger(args: &[Datum], _ctx: &FuncCtx) -> Result<Datum> {
    let (a, b) = (arg_interval(args, 0)?, arg_interval(args, 1)?);
    Ok(Datum::Interval(if cmp_args(args)?.is_gt() { a } else { b }))
}

fn f_smaller(args: &[Datum], _ctx: &FuncCtx) -> Result<Datum> {
    let (a, b) = (arg_interval(args, 0)?, arg_interval(args, 1)?);
    Ok(Datum::Interval(if cmp_args(args)?.is_lt() { a } else { b }))
}

/// Low 64 bits of the comparison span, as PG hashes.
fn hash_input(iv: &Interval) -> i64 {
    iv.cmp_span() as i64
}

fn f_hash(args: &[Datum], _ctx: &FuncCtx) -> Result<Datum> {
    let v = hash_input(&arg_interval(args, 0)?);
    // hashint8
    let lo = v as u32;
    let hi = (v >> 32) as u32;
    let x = lo ^ if v >= 0 { hi } else { !hi };
    Ok(Datum::Int4(compare::pg_hash_uint32(x)? as i32))
}

fn f_hash_extended(args: &[Datum], _ctx: &FuncCtx) -> Result<Datum> {
    let v = hash_input(&arg_interval(args, 0)?);
    let seed = match args.get(1) {
        Some(Datum::Int8(s)) => *s as u64,
        Some(other) => return Err(type_mismatch("Int8", other)),
        None => return Err(PgError::internal("interval_hash_extended called with too few arguments")),
    };
    let lo = v as u32;
    let hi = (v >> 32) as u32;
    let x = lo ^ if v >= 0 { hi } else { !hi };
    Ok(Datum::Int8(compare::pg_hash_uint32_extended(x, seed)? as i64))
}

fn f_finite(args: &[Datum], _ctx: &FuncCtx) -> Result<Datum> {
    Ok(Datum::Bool(!arg_interval(args, 0)?.is_infinite()))
}

fn f_justify_days(args: &[Datum], _ctx: &FuncCtx) -> Result<Datum> {
    Ok(Datum::Interval(justify_days(&arg_interval(args, 0)?)?))
}

fn f_justify_hours(args: &[Datum], _ctx: &FuncCtx) -> Result<Datum> {
    Ok(Datum::Interval(justify_hours(&arg_interval(args, 0)?)?))
}

fn f_justify_interval(args: &[Datum], _ctx: &FuncCtx) -> Result<Datum> {
    Ok(Datum::Interval(justify_interval(&arg_interval(args, 0)?)?))
}

fn f_time(args: &[Datum], _ctx: &FuncCtx) -> Result<Datum> {
    let iv = arg_interval(args, 0)?;
    if iv.is_infinite() {
        return Err(out_of_range_msg("cannot convert infinite interval to time"));
    }
    let mut t = iv.micros % USECS_PER_DAY;
    if t < 0 {
        t += USECS_PER_DAY;
    }
    Ok(Datum::Time(t))
}

fn out_of_range_msg(msg: &str) -> PgError {
    PgError::new(SqlState::DATETIME_VALUE_OUT_OF_RANGE, msg)
}

fn f_in_range(args: &[Datum], _ctx: &FuncCtx) -> Result<Datum> {
    let val = arg_interval(args, 0)?;
    let base = arg_interval(args, 1)?;
    let offset = arg_interval(args, 2)?;
    let sub_flag = arg_bool(args, 3)?;
    let less = arg_bool(args, 4)?;

    if sign_of(&offset) < 0 {
        return Err(PgError::new(
            SqlState(*b"22013"),
            "invalid preceding or following size in window function",
        ));
    }
    if offset == Interval::INFINITY
        && (if sub_flag { base == Interval::INFINITY } else { base == Interval::NEG_INFINITY })
    {
        return Ok(Datum::Bool(true));
    }
    let sum = if sub_flag { sub(&base, &offset)? } else { add(&base, &offset)? };
    let c = val.cmp_span().cmp(&sum.cmp_span());
    Ok(Datum::Bool(if less { c.is_le() } else { c.is_ge() }))
}

// ---------------------------------------------------------------------------------------------
// Tests (executed centrally)
// ---------------------------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::context::{IoConfig, TestSession};
    use crate::registry::builtin_env;

    fn parse(s: &str) -> Result<Interval> {
        let cfg = IoConfig::default();
        let ctx = IoCtx::new(builtin_env(), &cfg);
        parse_interval(s, &ctx)
    }

    fn parse_ty(s: &str, typmod: i32) -> Result<Interval> {
        let cfg = IoConfig::default();
        let ctx = IoCtx::new(builtin_env(), &cfg);
        match input(&ctx, TypeId::new(crate::oids::INTERVAL, typmod), s)? {
            Datum::Interval(v) => Ok(v),
            other => panic!("unexpected {other:?}"),
        }
    }

    fn show(iv: Interval) -> String {
        let cfg = IoConfig::default();
        let ctx = IoCtx::new(builtin_env(), &cfg);
        output(&ctx, TypeId::INTERVAL, &Datum::Interval(iv)).unwrap()
    }

    fn p(s: &str) -> Interval {
        parse(s).unwrap()
    }

    #[test]
    fn fractional_years_cascade() {
        assert_eq!(p("1.5 years"), Interval::new(18, 0, 0));
        assert_eq!(show(p("1.5 years")), "1 year 6 mons");
    }

    #[test]
    fn ago_negates() {
        assert_eq!(p("1 day ago"), Interval::new(0, -1, 0));
        assert_eq!(show(p("1 day ago")), "-1 days");
    }

    #[test]
    fn iso8601_designators() {
        let iv = p("P1Y2M3DT4H5M6.7S");
        assert_eq!(iv, Interval::new(14, 3, 4 * 3_600_000_000 + 5 * 60_000_000 + 6_700_000));
        assert_eq!(show(iv), "1 year 2 mons 3 days 04:05:06.7");
    }

    #[test]
    fn iso8601_alternative() {
        let iv = p("P0001-02-03T04:05:06");
        assert_eq!(iv, Interval::new(14, 3, 4 * 3_600_000_000 + 5 * 60_000_000 + 6_000_000));
    }

    #[test]
    fn sql_standard_forms() {
        assert_eq!(show(p("1-2 3 4:05:06")), "1 year 2 mons 3 days 04:05:06");
        assert_eq!(p("1-2"), Interval::new(14, 0, 0));
    }

    #[test]
    fn postgres_syntax() {
        assert_eq!(p("1 year 2 mons 3 days 04:05:06.7"), Interval::new(14, 3, 14_706_700_000));
        assert_eq!(p("@ 1 minute"), Interval::new(0, 0, 60_000_000));
        assert_eq!(p("1 week"), Interval::new(0, 7, 0));
        assert_eq!(p("1 day 02:00"), Interval::new(0, 1, 7_200_000_000));
        assert_eq!(p("2 decades"), Interval::new(240, 0, 0));
        assert_eq!(p("1 century"), Interval::new(1200, 0, 0));
        assert_eq!(p("5 ms"), Interval::new(0, 0, 5_000));
    }

    #[test]
    fn infinities() {
        assert_eq!(p("infinity"), Interval::INFINITY);
        assert_eq!(p("-infinity"), Interval::NEG_INFINITY);
        assert_eq!(show(Interval::INFINITY), "infinity");
        assert_eq!(show(Interval::NEG_INFINITY), "-infinity");
    }

    #[test]
    fn junk_is_22007() {
        let e = parse("x").unwrap_err();
        assert_eq!(e.sqlstate, SqlState::INVALID_DATETIME_FORMAT);
        assert_eq!(e.message, "invalid input syntax for type interval: \"x\"");
        assert!(parse("1 day day").is_err());
        assert!(parse("1 day 2 days").is_err());
        assert!(parse("").is_err());
    }

    #[test]
    fn overflow_is_22015() {
        let e = parse("99999999999 years").unwrap_err();
        assert_eq!(e.sqlstate, SqlState::INTERVAL_FIELD_OVERFLOW);
    }

    #[test]
    fn typmod_second_precision() {
        let tm = (fld::SECOND << 16) | 2;
        let iv = parse_ty("1 day 02:03:04.5678", tm).unwrap();
        assert_eq!(show(iv), "1 day 02:03:04.57");
    }

    #[test]
    fn typmod_year_truncates() {
        let tm = (fld::YEAR << 16) | fld::FULL_PRECISION;
        let iv = parse_ty("1 year 2 months", tm).unwrap();
        assert_eq!(iv, Interval::new(12, 0, 0));
    }

    #[test]
    fn typmod_bare_number_uses_last_field() {
        let tm = (fld::HOUR << 16) | fld::FULL_PRECISION;
        assert_eq!(parse_ty("2", tm).unwrap(), Interval::new(0, 0, 7_200_000_000));
    }

    #[test]
    fn multiplication_and_division() {
        assert_eq!(show(mul(&p("1 day"), 1.5).unwrap()), "1 day 12:00:00");
        assert_eq!(show(div(&p("1 mon"), 3.0).unwrap()), "10 days");
        assert_eq!(show(div(&p("1 year"), 7.0).unwrap()), "1 mon 21 days 10:17:08.5344");
        assert_eq!(div(&p("1 day"), 0.0).unwrap_err().sqlstate, SqlState::DIVISION_BY_ZERO);
        assert_eq!(mul(&Interval::new(i32::MAX - 1, 0, 0), 2.0).unwrap_err().sqlstate, SqlState::DATETIME_VALUE_OUT_OF_RANGE);
    }

    #[test]
    fn justify() {
        assert_eq!(show(justify_interval(&p("1 mon -1 day")).unwrap()), "29 days");
        assert_eq!(show(justify_hours(&p("27 hours")).unwrap()), "1 day 03:00:00");
        assert_eq!(show(justify_days(&p("35 days")).unwrap()), "1 mon 5 days");
    }

    #[test]
    fn addition_and_infinity_rules() {
        assert_eq!(add(&p("1 day"), &p("2 days")).unwrap(), Interval::new(0, 3, 0));
        assert!(add(&Interval::INFINITY, &Interval::NEG_INFINITY).is_err());
        assert_eq!(add(&Interval::INFINITY, &p("1 day")).unwrap(), Interval::INFINITY);
        assert!(add(&Interval::new(i32::MAX, 0, 0), &Interval::new(1, 0, 0)).is_err());
    }

    #[test]
    fn comparison_by_span() {
        assert_eq!(p("1 mon").cmp_span(), p("30 days").cmp_span());
    }

    #[test]
    fn binary_roundtrip_order() {
        let cfg = IoConfig::default();
        let ctx = IoCtx::new(builtin_env(), &cfg);
        let iv = Interval::new(3, 2, 1);
        let bytes = send(&ctx, TypeId::INTERVAL, &Datum::Interval(iv)).unwrap();
        assert_eq!(bytes, vec![0, 0, 0, 0, 0, 0, 0, 1, 0, 0, 0, 2, 0, 0, 0, 3]);
        assert_eq!(recv(&ctx, TypeId::INTERVAL, &bytes).unwrap(), Datum::Interval(iv));
        assert!(recv(&ctx, TypeId::INTERVAL, &bytes[..8]).is_err());
    }

    #[test]
    fn scalar_functions() {
        let cfg = IoConfig::default();
        let session = TestSession::default();
        let ctx = FuncCtx {
            env: builtin_env(),
            io: &cfg,
            session: &session,
            arg_types: &[],
            result_type: TypeId::INTERVAL,
            collation: 0,
        };
        let a = Datum::Interval(Interval::new(0, 1, 0));
        let b = Datum::Interval(Interval::new(0, 0, 90_000_000_000));
        assert_eq!(f_lt(&[a.clone(), b.clone()], &ctx).unwrap(), Datum::Bool(true));
        assert_eq!(f_cmp(&[a.clone(), b.clone()], &ctx).unwrap(), Datum::Int4(-1));
        assert_eq!(f_time(&[Datum::Interval(Interval::new(0, 0, -3_600_000_000))], &ctx).unwrap(), Datum::Time(82_800_000_000));
        assert_eq!(f_finite(&[Datum::Interval(Interval::INFINITY)], &ctx).unwrap(), Datum::Bool(false));
        assert_eq!(
            f_in_range(&[b.clone(), a.clone(), Datum::Interval(Interval::new(0, 0, 1)), Datum::Bool(false), Datum::Bool(false)], &ctx)
                .unwrap(),
            Datum::Bool(true)
        );
    }
}
