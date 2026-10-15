//! Datetime field interpretation: PG `DecodeDateTime`, `DecodeTimeOnly`, `DecodeDate`,
//! `DecodeTime`, `DecodeNumber`, `DecodeNumberField`, `DecodeSpecial`, `ValidateDate`
//! (TYPE-06). Lease TYP-23 (sonnet).
//!
//! PG18 reference: `src/backend/utils/adt/datetime.c` (+ keyword table `datetktbl[]` and `deltatktbl[]`
//! for units). Port the algorithms faithfully, including:
//! - DateStyle field order (`IoConfig::date_order`: MDY/DMY/YMD) for ambiguous numeric dates
//!   (`01/02/2024` = Jan 2 under MDY); ISO `yyyy-mm-dd` always works; `yyyymmdd` / `yymmdd` concatenated
//!   forms; Julian `J2451545`; ISO 8601 `T` separator (`2024-02-29T13:01:02Z`); month/day names
//!   (`jan`, `january`, `mon`...); `AD`/`BC`; `AM`/`PM`; `allballs`; `epoch`, `infinity`, `-infinity`,
//!   `now`, `today`, `tomorrow`, `yesterday` (`IoCtx::now` supplies the transaction timestamp; if `None`
//!   these four → 0A000 via `PgError::unimplemented("TYP-23", ...)`), `z`/`zulu`/`utc`/`gmt` zones,
//!   numeric zones `+hh`, `+hhmm`, `+hh:mm`, `+hh:mm:ss` (|offset| > 15:59:59 → `DtErr::TzDispOverflow`),
//!   zone abbreviations and names via `tz::lookup_zone` (unknown alpha zone → `DtErr::BadTimezone(name)`).
//! - Range validation: month 1..12, day valid for month (`2020-13-01` → `MdFieldOverflow` when the
//!   ambiguity hint applies, as PG; `2023-02-29` → FieldOverflow), hour 0..24 (24 only with 0 min/sec),
//!   minute 0..59, second 0..60 (60 is a leap second that rolls over, PG accepts `23:59:60`).
//!   Oracle: `'25:00'::time` → `date/time field value out of range: "25:00"` (22008);
//!   `'2020-13-01'::date` → same message + HINT `Perhaps you need a different "DateStyle" setting.`;
//!   `'x'::date` → 22007 `invalid input syntax for type date: "x"`.
//! - BC years: `0001-01-01 BC` → astronomical year 0. Year 0 in input → FieldOverflow.
//! - Fractional seconds are rounded to microseconds (`rint`).
//!
//! Implementation notes (PGArbor specifics):
//! - `Decoded::tz` is `Some` only when the input carried an explicit zone; the session-zone fallback
//!   (`DetermineTimeZoneOffset`) is the caller's job (`tz::offset_at_local`).
//! - `now` returns `DateTimeKind::Now` with `tm` / `tz` already filled from `IoCtx::now` in the session
//!   zone; `today` / `tomorrow` / `yesterday` return `Date` with only the date fields from `IoCtx::now`.
//! - Zone words that name an IANA region (no rules shipped) are reported as
//!   `DtErr::UnsupportedTimezone` at the point where PG would compute the offset.
//! - Numeric zones are decoded by a private port of `DecodeTimezone` (same result as
//!   `tz::parse_numeric_offset`, but sign-flipped to PG's "seconds west" and tolerant of the
//!   `hhmmss-zz` run-together form PG supports).
use super::calendar::{date2j, days_in_month, j2date, timestamp2tm};
use super::lexer::{parse_datetime, Field, FieldKind};
use super::tz::{lookup_zone, offset_at_utc, ZoneSpec};
use super::{dt_parse_error, DateTimeKind, DtErr, Tm, USECS_PER_DAY, USECS_PER_HOUR, USECS_PER_MINUTE, USECS_PER_SEC};
use crate::context::{DateOrder, IoCtx};
use pgarbor_common::{PgError, Result};

/// Decoded value: kind, broken-down fields, and the zone that appeared in the input (seconds WEST,
/// PG sign) if any. For `DateTimeKind::Date` all `tm` fields are set (missing time fields = 0).
// CONTRACT
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Decoded {
    pub kind: DateTimeKind,
    pub tm: Tm,
    /// Explicit zone from the input (`+02`, `PST`, `UTC`), seconds WEST of UTC.
    pub tz: Option<i32>,
}

// ---------------------------------------------------------------------------------------------
// Field-type bit masks (PG `DTK_M(type)` with the `utils/datetime.h` type numbers).
// ---------------------------------------------------------------------------------------------
const M_MONTH: u32 = 1 << 1;
const M_YEAR: u32 = 1 << 2;
const M_DAY: u32 = 1 << 3;
const M_TZ: u32 = 1 << 5;
const M_DTZ: u32 = 1 << 6;
const M_AMPM: u32 = 1 << 9;
const M_HOUR: u32 = 1 << 10;
const M_MINUTE: u32 = 1 << 11;
const M_ALL_SECS: u32 = (1 << 12) | (1 << 13) | (1 << 14);
const M_DOY: u32 = 1 << 15;
const M_DOW: u32 = 1 << 16;
const M_ADBC: u32 = 1 << 18;
const M_DTZMOD: u32 = 1 << 28;
const DATE_M: u32 = M_YEAR | M_MONTH | M_DAY;
const TIME_M: u32 = M_HOUR | M_MINUTE | M_ALL_SECS;
const YM_M: u32 = M_YEAR | M_MONTH;
const MD_M: u32 = M_MONTH | M_DAY;

/// Maximum number of sub-fields in a DATE field (PG `MAXDATEFIELDS`).
const MAXDATEFIELDS: usize = 25;

/// Internal error: input error (`DtErr`) or engine error (`PgError`).
enum E {
    Dt(DtErr),
    Eng(PgError),
}
impl From<DtErr> for E {
    fn from(e: DtErr) -> E {
        E::Dt(e)
    }
}
impl From<PgError> for E {
    fn from(e: PgError) -> E {
        E::Eng(e)
    }
}
type R<T> = std::result::Result<T, E>;

fn bad_format<T>() -> R<T> {
    Err(E::Dt(DtErr::BadFormat))
}
fn field_overflow<T>() -> R<T> {
    Err(E::Dt(DtErr::FieldOverflow))
}

fn finish(r: R<Decoded>) -> Result<std::result::Result<Decoded, DtErr>> {
    match r {
        Ok(d) => Ok(Ok(d)),
        Err(E::Dt(e)) => Ok(Err(e)),
        Err(E::Eng(e)) => Err(e),
    }
}

// ---------------------------------------------------------------------------------------------
// Keyword table (PG `datetktbl[]`) and zone classification
// ---------------------------------------------------------------------------------------------
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Mer {
    Am,
    Pm,
    Hr24,
}

/// "Prefix type" remembered between fields (PG `ptype`).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Ptype {
    None,
    Julian,
    Time,
    /// Any other unit keyword (`d`, `h`, `y`, ...): accepted by the table but never usable.
    Other,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Resv {
    Late,
    Early,
    Zulu,
    Epoch,
    Now,
    Today,
    Tomorrow,
    Yesterday,
}

enum Kw {
    Reserv(Resv),
    Month(i32),
    AdBc(bool),
    AmPm(Mer),
    Dow(i32),
    Units(Ptype),
    IsoTime,
    DtzMod,
    Ignore,
    /// Fixed-offset zone, seconds EAST.
    Tz(i32),
    /// Zone that needs tz rules we do not ship.
    Region(String),
    Unknown,
}

/// PG `DecodeSpecial`: lookup in `datetktbl[]` (token already lower-cased).
fn decode_special(tok: &str) -> Option<Kw> {
    let m = |n: i32| Some(Kw::Month(n));
    let d = |n: i32| Some(Kw::Dow(n));
    match tok {
        "+infinity" | "infinity" => Some(Kw::Reserv(Resv::Late)),
        "-infinity" => Some(Kw::Reserv(Resv::Early)),
        "ad" => Some(Kw::AdBc(false)),
        "bc" => Some(Kw::AdBc(true)),
        "allballs" => Some(Kw::Reserv(Resv::Zulu)),
        "am" => Some(Kw::AmPm(Mer::Am)),
        "pm" => Some(Kw::AmPm(Mer::Pm)),
        "at" | "on" => Some(Kw::Ignore),
        "dst" => Some(Kw::DtzMod),
        "epoch" => Some(Kw::Reserv(Resv::Epoch)),
        "now" => Some(Kw::Reserv(Resv::Now)),
        "today" => Some(Kw::Reserv(Resv::Today)),
        "tomorrow" => Some(Kw::Reserv(Resv::Tomorrow)),
        "yesterday" => Some(Kw::Reserv(Resv::Yesterday)),
        "t" => Some(Kw::IsoTime),
        "j" | "jd" | "julian" => Some(Kw::Units(Ptype::Julian)),
        "d" | "dow" | "doy" | "h" | "isodow" | "isoyear" | "m" | "mm" | "s" | "y" => Some(Kw::Units(Ptype::Other)),
        "jan" | "january" => m(1),
        "feb" | "february" => m(2),
        "mar" | "march" => m(3),
        "apr" | "april" => m(4),
        "may" => m(5),
        "jun" | "june" => m(6),
        "jul" | "july" => m(7),
        "aug" | "august" => m(8),
        "sep" | "sept" | "september" => m(9),
        "oct" | "october" => m(10),
        "nov" | "november" => m(11),
        "dec" | "december" => m(12),
        "sun" | "sunday" => d(0),
        "mon" | "monday" => d(1),
        "tue" | "tues" | "tuesday" => d(2),
        "wed" | "weds" | "wednesday" => d(3),
        "thu" | "thur" | "thurs" | "thursday" => d(4),
        "fri" | "friday" => d(5),
        "sat" | "saturday" => d(6),
        _ => None,
    }
}

/// Keyword table first (no keyword collides with a PG Default zone abbreviation), then zone
/// names/abbreviations. A word that is neither is `Kw::Unknown` (PG: `pg_tzset` failed -> BAD_FORMAT).
fn classify(text: &str) -> R<Kw> {
    if let Some(k) = decode_special(text) {
        return Ok(k);
    }
    match lookup_zone(text)? {
        Ok(ZoneSpec::Fixed { secs_east, .. }) => Ok(Kw::Tz(secs_east)),
        Ok(ZoneSpec::Region(n)) => Ok(Kw::Region(n)),
        Err(DtErr::UnsupportedTimezone(n)) => Ok(Kw::Region(n)),
        Err(_) => Ok(Kw::Unknown),
    }
}

// ---------------------------------------------------------------------------------------------
// C-library style helpers
// ---------------------------------------------------------------------------------------------
fn at(b: &[u8], i: usize) -> u8 {
    b.get(i).copied().unwrap_or(0)
}

/// `strtol` core: (value, bytes consumed). Consumed 0 means no conversion (endptr == str).
fn strto(b: &[u8]) -> (i128, usize) {
    let mut i = 0;
    while i < b.len() && matches!(b[i], b' ' | b'\t' | b'\n' | 0x0b | 0x0c | b'\r') {
        i += 1;
    }
    let mut neg = false;
    if i < b.len() && (b[i] == b'+' || b[i] == b'-') {
        neg = b[i] == b'-';
        i += 1;
    }
    let ds = i;
    let mut acc: i128 = 0;
    while i < b.len() && b[i].is_ascii_digit() {
        if acc < (1i128 << 100) {
            acc = acc * 10 + (b[i] - b'0') as i128;
        }
        i += 1;
    }
    if i == ds {
        return (0, 0);
    }
    (if neg { -acc } else { acc }, i)
}

/// PG `strtoint`: (value, consumed, out_of_range).
fn strtoint(b: &[u8]) -> (i32, usize, bool) {
    let (v, n) = strto(b);
    if v > i32::MAX as i128 {
        (i32::MAX, n, true)
    } else if v < i32::MIN as i128 {
        (i32::MIN, n, true)
    } else {
        (v as i32, n, false)
    }
}

fn strtoi64(b: &[u8]) -> (i64, usize, bool) {
    let (v, n) = strto(b);
    if v > i64::MAX as i128 {
        (i64::MAX, n, true)
    } else if v < i64::MIN as i128 {
        (i64::MIN, n, true)
    } else {
        (v as i64, n, false)
    }
}

/// `atoi` on an all-digit string (wraps like a C cast on absurd lengths).
fn atoi(s: &str) -> i32 {
    s.bytes().filter(u8::is_ascii_digit).fold(0i32, |a, c| a.wrapping_mul(10).wrapping_add((c - b'0') as i32))
}

/// PG `ParseFraction`: `cp` starts at the '.'; returns the value in 0..1.
fn parse_fraction(cp: &str) -> R<f64> {
    let rest = cp.get(1..).unwrap_or("");
    if rest.is_empty() {
        return Ok(0.0);
    }
    if !rest.bytes().all(|c| c.is_ascii_digit()) {
        return bad_format();
    }
    match format!("0.{rest}").parse::<f64>() {
        Ok(v) => Ok(v),
        Err(_) => bad_format(),
    }
}

/// PG `ParseFractionalSecond`: fraction -> integer microseconds (`rint`).
fn parse_fractional_second(cp: &str) -> R<i32> {
    let frac = parse_fraction(cp)?;
    Ok((frac * 1_000_000.0).round_ties_even() as i32)
}

/// PG `dt2time`.
fn dt2time(mut t: i64) -> (i32, i32, i32, i32) {
    let hour = t / USECS_PER_HOUR;
    t -= hour * USECS_PER_HOUR;
    let min = t / USECS_PER_MINUTE;
    t -= min * USECS_PER_MINUTE;
    let sec = t / USECS_PER_SEC;
    let fsec = t - sec * USECS_PER_SEC;
    (hour as i32, min as i32, sec as i32, fsec as i32)
}

/// PG `time_overflows`.
fn time_overflows(hour: i32, min: i32, sec: i32, fsec: i32) -> bool {
    if !(0..=24).contains(&hour) || !(0..60).contains(&min) || !(0..=60).contains(&sec) || !(0..=1_000_000).contains(&fsec)
    {
        return true;
    }
    let total = (((hour as i64 * 60 + min as i64) * 60 + sec as i64) * USECS_PER_SEC) + fsec as i64;
    total > USECS_PER_DAY
}

/// Current transaction time as local broken-down time plus the session zone (seconds WEST).
fn local_now(ctx: &IoCtx) -> R<(Tm, i32)> {
    let now = match ctx.now {
        Some(n) => n,
        None => {
            return Err(E::Eng(PgError::unimplemented(
                "TYP-23",
                "now/today/tomorrow/yesterday datetime input without a transaction timestamp",
            )))
        }
    };
    let east = offset_at_utc(&ctx.config.time_zone, now)?;
    let tm = timestamp2tm(now, Some(-east))?;
    Ok((tm, -east))
}

fn set_ymd(tm: &mut Tm, jd: i32) {
    let (y, m, d) = j2date(jd);
    tm.year = y;
    tm.mon = m;
    tm.mday = d;
}

// ---------------------------------------------------------------------------------------------
// DecodeTimezone / DecodeTime / DecodeNumber / DecodeNumberField / DecodeDate / ValidateDate
// ---------------------------------------------------------------------------------------------

/// PG `DecodeTimezone`: `+hh`, `+hhmm`, `+hh:mm`, `+hh:mm:ss` -> seconds WEST (PG sign).
fn decode_timezone(s: &str) -> R<i32> {
    let b = s.as_bytes();
    if !matches!(b.first(), Some(b'+') | Some(b'-')) {
        return bad_format();
    }
    let (hr0, n, er) = strtoint(&b[1..]);
    if er {
        return Err(DtErr::TzDispOverflow.into());
    }
    let mut cp = 1 + n;
    let mut hr = hr0;
    let min: i32;
    let mut sec = 0;
    if at(b, cp) == b':' {
        let (m, n2, er) = strtoint(&b[cp + 1..]);
        if er {
            return Err(DtErr::TzDispOverflow.into());
        }
        cp += 1 + n2;
        min = m;
        if at(b, cp) == b':' {
            let (sv, n3, er) = strtoint(&b[cp + 1..]);
            if er {
                return Err(DtErr::TzDispOverflow.into());
            }
            cp += 1 + n3;
            sec = sv;
        }
    } else if cp == b.len() && b.len() > 3 {
        min = hr % 100;
        hr /= 100;
    } else {
        min = 0;
    }
    if !(0..=15).contains(&hr) || !(0..60).contains(&min) || !(0..60).contains(&sec) {
        return Err(DtErr::TzDispOverflow.into());
    }
    let mut tz = (hr * 60 + min) * 60 + sec;
    if b[0] == b'-' {
        tz = -tz;
    }
    if cp != b.len() {
        return bad_format();
    }
    Ok(-tz)
}

/// PG `DecodeTimeCommon` for timestamps: (hour, min, sec, usec) of `hh:mm[:ss[.fff]]` / `mm:ss.fff`.
fn decode_time_common(s: &str) -> R<(i64, i32, i32, i32)> {
    let b = s.as_bytes();
    let (h, n, er) = strtoi64(b);
    if er {
        return field_overflow();
    }
    let mut cp = n;
    if at(b, cp) != b':' {
        return bad_format();
    }
    let (m, n2, er) = strtoint(&b[cp + 1..]);
    if er {
        return field_overflow();
    }
    cp += 1 + n2;
    let mut hour = h;
    let mut min = m;
    let sec: i32;
    let mut fsec = 0;
    match at(b, cp) {
        0 => sec = 0,
        b'.' => {
            fsec = parse_fractional_second(&s[cp..])?;
            if hour > i32::MAX as i64 || hour < i32::MIN as i64 {
                return field_overflow();
            }
            sec = min;
            min = hour as i32;
            hour = 0;
        }
        b':' => {
            let (sv, n3, er) = strtoint(&b[cp + 1..]);
            if er {
                return field_overflow();
            }
            cp += 1 + n3;
            sec = sv;
            if at(b, cp) == b'.' {
                fsec = parse_fractional_second(&s[cp..])?;
            } else if at(b, cp) != 0 {
                return bad_format();
            }
        }
        _ => return bad_format(),
    }
    if hour < 0 || !(0..60).contains(&min) || !(0..=60).contains(&sec) || !(0..=1_000_000).contains(&fsec) {
        return field_overflow();
    }
    Ok((hour, min, sec, fsec))
}

/// PG `DecodeTime`: stores into tm/fsec, returns tmask.
fn decode_time(s: &str, tm: &mut Tm, fsec: &mut i32) -> R<u32> {
    let (h, m, sc, us) = decode_time_common(s)?;
    if h > i32::MAX as i64 {
        return field_overflow();
    }
    tm.hour = h as i32;
    tm.min = m;
    tm.sec = sc;
    *fsec = us;
    Ok(TIME_M)
}

/// PG `DecodeNumberField`: concatenated `yyyymmdd` / `hhmmss[.fff]` etc. Returns the tmask.
fn decode_number_field(s: &str, fmask: u32, tm: &mut Tm, fsec: &mut i32, is2digits: &mut bool) -> R<u32> {
    if !s.bytes().all(|c| c.is_ascii_digit() || c == b'.') {
        return bad_format();
    }
    let body: &str;
    if let Some(p) = s.find('.') {
        *fsec = parse_fractional_second(&s[p..])?;
        body = &s[..p];
    } else {
        body = s;
        if fmask & DATE_M != DATE_M && body.len() >= 6 {
            let len = body.len();
            tm.mday = atoi(&body[len - 2..]);
            tm.mon = atoi(&body[len - 4..len - 2]);
            tm.year = atoi(&body[..len - 4]);
            if len - 4 == 2 {
                *is2digits = true;
            }
            return Ok(DATE_M);
        }
    }
    let len = body.len();
    if fmask & TIME_M != TIME_M {
        if len == 6 {
            tm.sec = atoi(&body[4..]);
            tm.min = atoi(&body[2..4]);
            tm.hour = atoi(&body[..2]);
            return Ok(TIME_M);
        } else if len == 4 {
            tm.sec = 0;
            tm.min = atoi(&body[2..]);
            tm.hour = atoi(&body[..2]);
            return Ok(TIME_M);
        }
    }
    bad_format()
}

/// PG `DecodeNumber`: a plain numeric field interpreted in the context of what has been seen.
fn decode_number(
    s: &str,
    have_text_month: bool,
    fmask: u32,
    order: DateOrder,
    tm: &mut Tm,
    fsec: &mut i32,
    is2digits: &mut bool,
) -> R<u32> {
    let flen = s.len();
    let b = s.as_bytes();
    let (val, cp, er) = strtoint(b);
    if er {
        return field_overflow();
    }
    if cp == 0 {
        return bad_format();
    }
    if at(b, cp) == b'.' {
        if cp > 2 {
            return decode_number_field(s, fmask | DATE_M, tm, fsec, is2digits);
        }
        *fsec = parse_fractional_second(&s[cp..])?;
    } else if cp != b.len() {
        return bad_format();
    }

    // Special case for day of year.
    if flen == 3 && (fmask & DATE_M) == M_YEAR && (1..=366).contains(&val) {
        tm.yday = val;
        return Ok(M_DOY | M_MONTH | M_DAY);
    }

    let tmask: u32;
    match fmask & DATE_M {
        0 => {
            if flen >= 3 || order == DateOrder::Ymd {
                tmask = M_YEAR;
                tm.year = val;
            } else if order == DateOrder::Dmy {
                tmask = M_DAY;
                tm.mday = val;
            } else {
                tmask = M_MONTH;
                tm.mon = val;
            }
        }
        M_YEAR => {
            tmask = M_MONTH;
            tm.mon = val;
        }
        M_MONTH => {
            if have_text_month {
                if flen >= 3 || order == DateOrder::Ymd {
                    tmask = M_YEAR;
                    tm.year = val;
                } else {
                    tmask = M_DAY;
                    tm.mday = val;
                }
            } else {
                tmask = M_DAY;
                tm.mday = val;
            }
        }
        YM_M => {
            if have_text_month {
                if flen >= 3 && *is2digits {
                    tmask = M_DAY;
                    tm.mday = tm.year;
                    tm.year = val;
                    *is2digits = false;
                } else {
                    tmask = M_DAY;
                    tm.mday = val;
                }
            } else {
                tmask = M_DAY;
                tm.mday = val;
            }
        }
        M_DAY => {
            tmask = M_MONTH;
            tm.mon = val;
        }
        MD_M => {
            tmask = M_YEAR;
            tm.year = val;
        }
        DATE_M => {
            // We have all the date, so it must be a time field.
            return decode_number_field(s, fmask, tm, fsec, is2digits);
        }
        _ => return bad_format(),
    }
    if tmask == M_YEAR {
        *is2digits = flen <= 2;
    }
    Ok(tmask)
}

/// PG `DecodeDate`: a date field with delimiters. Returns the tmask of what was found.
fn decode_date(s: &str, mut fmask: u32, order: DateOrder, is2digits: &mut bool, tm: &mut Tm) -> R<u32> {
    let b = s.as_bytes();
    let mut tmask = 0u32;
    let mut fields: Vec<Option<&str>> = Vec::new();
    let mut p = 0usize;
    while p < b.len() && fields.len() < MAXDATEFIELDS {
        while p < b.len() && !b[p].is_ascii_alphanumeric() {
            p += 1;
        }
        if p >= b.len() {
            return bad_format();
        }
        let start = p;
        if b[p].is_ascii_digit() {
            while p < b.len() && b[p].is_ascii_digit() {
                p += 1;
            }
        } else {
            while p < b.len() && b[p].is_ascii_alphabetic() {
                p += 1;
            }
        }
        fields.push(Some(s.get(start..p).unwrap_or("")));
        // PG overwrites the delimiter that ended the run, whatever it is.
        if p < b.len() {
            p += 1;
        }
    }

    // Text fields first: they give an unambiguous month.
    let mut have_text_month = false;
    for slot in fields.iter_mut() {
        let f: &str = match *slot {
            Some(f) => f,
            None => continue,
        };
        if f.as_bytes().first().is_some_and(u8::is_ascii_alphabetic) {
            match decode_special(f) {
                Some(Kw::Ignore) => continue,
                Some(Kw::Month(v)) => {
                    tm.mon = v;
                    have_text_month = true;
                    if fmask & M_MONTH != 0 {
                        return bad_format();
                    }
                    fmask |= M_MONTH;
                    tmask |= M_MONTH;
                    *slot = None;
                }
                _ => return bad_format(),
            }
        }
    }

    // Remaining numeric fields.
    let mut junk_fsec = 0i32;
    for slot in fields.iter() {
        let f: &str = match *slot {
            Some(f) => f,
            None => continue,
        };
        if f.is_empty() {
            return bad_format();
        }
        let dmask = decode_number(f, have_text_month, fmask, order, tm, &mut junk_fsec, is2digits)?;
        if fmask & dmask != 0 {
            return bad_format();
        }
        fmask |= dmask;
        tmask |= dmask;
    }

    if (fmask & !(M_DOY | M_TZ)) != DATE_M {
        return bad_format();
    }
    Ok(tmask)
}

/// PG `ValidateDate`: BC / 2-digit years, day-of-year, month/day ranges.
fn validate_date(fmask: u32, isjulian: bool, is2digits: bool, bc: bool, tm: &mut Tm) -> R<()> {
    if fmask & M_YEAR != 0 {
        if isjulian {
            // year is already correct
        } else if bc {
            // there is no year zero in AD/BC notation
            if tm.year <= 0 {
                return field_overflow();
            }
            tm.year = -(tm.year - 1);
        } else if is2digits {
            if tm.year < 0 {
                return field_overflow();
            }
            if tm.year < 70 {
                tm.year += 2000;
            } else if tm.year < 100 {
                tm.year += 1900;
            }
        } else if tm.year <= 0 {
            return field_overflow();
        }
    }
    if fmask & M_DOY != 0 {
        let jd = date2j(tm.year, 1, 1).wrapping_add(tm.yday).wrapping_sub(1);
        set_ymd(tm, jd);
    }
    if fmask & M_MONTH != 0 && !(1..=12).contains(&tm.mon) {
        return Err(DtErr::MdFieldOverflow.into());
    }
    if fmask & M_DAY != 0 && !(1..=31).contains(&tm.mday) {
        return Err(DtErr::MdFieldOverflow.into());
    }
    if fmask & DATE_M == DATE_M && tm.mday > days_in_month(tm.year, tm.mon) {
        return field_overflow();
    }
    Ok(())
}

fn first_is_digit(s: &str) -> bool {
    s.as_bytes().first().is_some_and(u8::is_ascii_digit)
}

/// Apply AM/PM (PG: hour > 12 with a meridian is a field overflow).
fn apply_meridian(mer: Mer, tm: &mut Tm) -> R<()> {
    if mer != Mer::Hr24 && tm.hour > 12 {
        return field_overflow();
    }
    if mer == Mer::Am && tm.hour == 12 {
        tm.hour = 0;
    } else if mer == Mer::Pm && tm.hour != 12 {
        tm.hour += 12;
    }
    Ok(())
}

/// The number that follows a Julian / ISO-time prefix keyword (shared by both decoders). Returns tmask.
fn number_after_prefix(
    text: &str,
    ptype: Ptype,
    fmask: u32,
    tm: &mut Tm,
    fsec: &mut i32,
    is2digits: &mut bool,
    isjulian: &mut bool,
) -> R<u32> {
    let b = text.as_bytes();
    let (value, cp, er) = strtoint(b);
    if er {
        return field_overflow();
    }
    if at(b, cp) != b'.' && at(b, cp) != 0 {
        return bad_format();
    }
    match ptype {
        Ptype::Julian => {
            if value < 0 {
                return field_overflow();
            }
            let mut tmask = DATE_M;
            set_ymd(tm, value);
            *isjulian = true;
            if at(b, cp) == b'.' {
                let frac = parse_fraction(&text[cp..])?;
                let t = (frac * USECS_PER_DAY as f64) as i64;
                let (h, m, s, f) = dt2time(t);
                tm.hour = h;
                tm.min = m;
                tm.sec = s;
                *fsec = f;
                tmask |= TIME_M;
            }
            Ok(tmask)
        }
        Ptype::Time => {
            let tmask = decode_number_field(text, fmask | DATE_M, tm, fsec, is2digits)?;
            if tmask != TIME_M {
                return bad_format();
            }
            Ok(tmask)
        }
        _ => bad_format(),
    }
}

// ---------------------------------------------------------------------------------------------
// DecodeDateTime
// ---------------------------------------------------------------------------------------------

/// PG `DecodeDateTime`: fields of a date / timestamp / timestamptz literal.
/// Outer `Err` = engine error; inner `Err` = input error (see `dt_parse_error`).
// CONTRACT
pub fn decode_date_time(fields: &[Field], ctx: &IoCtx) -> Result<std::result::Result<Decoded, DtErr>> {
    finish(decode_date_time_inner(fields, ctx))
}

fn decode_date_time_inner(fields: &[Field], ctx: &IoCtx) -> R<Decoded> {
    let order = ctx.config.date_order;
    let mut fmask = 0u32;
    let mut ptype = Ptype::None;
    let mut kind = DateTimeKind::Date;
    let mut is_now = false;
    let mut tm = Tm::default();
    let mut fsec = 0i32;
    let mut tzv = 0i32;
    let mut mer = Mer::Hr24;
    let mut have_text_month = false;
    let mut isjulian = false;
    let mut is2digits = false;
    let mut bc = false;
    let mut unsupported: Option<String> = None;

    for f in fields {
        let text = f.text.as_str();
        let tmask: u32;
        match f.kind {
            FieldKind::Date => {
                if ptype == Ptype::Julian {
                    // Integral julian day with attached time zone.
                    let (jday, cp, er) = strtoint(text.as_bytes());
                    if er || jday < 0 {
                        return field_overflow();
                    }
                    set_ymd(&mut tm, jday);
                    isjulian = true;
                    tzv = decode_timezone(&text[cp..])?;
                    tmask = DATE_M | TIME_M | M_TZ;
                    ptype = Ptype::None;
                } else if ptype != Ptype::None || (fmask & MD_M) == MD_M {
                    // Already have a date: a zone name with punctuation or hhmmss-zz.
                    if first_is_digit(text) || ptype != Ptype::None {
                        if ptype != Ptype::None {
                            if ptype != Ptype::Time {
                                return bad_format();
                            }
                            ptype = Ptype::None;
                        }
                        if (fmask & TIME_M) == TIME_M {
                            return bad_format();
                        }
                        let cp = match text.find('-') {
                            Some(c) => c,
                            None => return bad_format(),
                        };
                        tzv = decode_timezone(&text[cp..])?;
                        let m = decode_number_field(&text[..cp], fmask, &mut tm, &mut fsec, &mut is2digits)?;
                        tmask = m | M_TZ;
                    } else {
                        match lookup_zone(text)? {
                            Ok(ZoneSpec::Fixed { secs_east, .. }) => tzv = -secs_east,
                            Ok(ZoneSpec::Region(n)) | Err(DtErr::UnsupportedTimezone(n)) => unsupported = Some(n),
                            Err(e) => return Err(e.into()),
                        }
                        tmask = M_TZ;
                    }
                } else {
                    tmask = decode_date(text, fmask, order, &mut is2digits, &mut tm)?;
                }
            }
            FieldKind::Time => {
                if ptype != Ptype::None {
                    if ptype != Ptype::Time {
                        return bad_format();
                    }
                    ptype = Ptype::None;
                }
                tmask = decode_time(text, &mut tm, &mut fsec)?;
                if time_overflows(tm.hour, tm.min, tm.sec, fsec) {
                    return field_overflow();
                }
            }
            FieldKind::Tz => {
                tzv = decode_timezone(text)?;
                tmask = M_TZ;
            }
            FieldKind::Number => {
                if ptype != Ptype::None {
                    tmask = number_after_prefix(text, ptype, fmask, &mut tm, &mut fsec, &mut is2digits, &mut isjulian)?;
                    ptype = Ptype::None;
                    kind = DateTimeKind::Date;
                } else {
                    let flen = text.len();
                    let dot = text.find('.');
                    if dot.is_some() && fmask & DATE_M == 0 {
                        // Embedded decimal and no date yet.
                        tmask = decode_date(text, fmask, order, &mut is2digits, &mut tm)?;
                    } else if dot.is_some_and(|d| d > 2) {
                        // Embedded decimal and several digits before: concatenated date or time.
                        tmask = decode_number_field(text, fmask, &mut tm, &mut fsec, &mut is2digits)?;
                    } else if flen >= 6 && (fmask & DATE_M == 0 || fmask & TIME_M == 0) {
                        tmask = decode_number_field(text, fmask, &mut tm, &mut fsec, &mut is2digits)?;
                    } else {
                        tmask =
                            decode_number(text, have_text_month, fmask, order, &mut tm, &mut fsec, &mut is2digits)?;
                    }
                }
            }
            FieldKind::String | FieldKind::Special => {
                let mut tm_ = 1u32; // DTK_M(RESERV)
                match classify(text)? {
                    Kw::Ignore => continue,
                    Kw::Reserv(r) => match r {
                        Resv::Now => {
                            let (n, west) = local_now(ctx)?;
                            tm_ = DATE_M | TIME_M | M_TZ;
                            kind = DateTimeKind::Date;
                            is_now = true;
                            tm.year = n.year;
                            tm.mon = n.mon;
                            tm.mday = n.mday;
                            tm.hour = n.hour;
                            tm.min = n.min;
                            tm.sec = n.sec;
                            fsec = n.fsec;
                            tzv = west;
                        }
                        Resv::Yesterday | Resv::Today | Resv::Tomorrow => {
                            let (n, _) = local_now(ctx)?;
                            tm_ = DATE_M;
                            kind = DateTimeKind::Date;
                            let jd = date2j(n.year, n.mon, n.mday);
                            let jd = match r {
                                Resv::Yesterday => jd - 1,
                                Resv::Tomorrow => jd + 1,
                                _ => jd,
                            };
                            set_ymd(&mut tm, jd);
                        }
                        Resv::Zulu => {
                            tm_ = TIME_M | M_TZ;
                            kind = DateTimeKind::Date;
                            tm.hour = 0;
                            tm.min = 0;
                            tm.sec = 0;
                            tzv = 0;
                        }
                        Resv::Epoch | Resv::Late | Resv::Early => {
                            tm_ = DATE_M | TIME_M | M_TZ;
                            kind = match r {
                                Resv::Epoch => DateTimeKind::Epoch,
                                Resv::Late => DateTimeKind::Late,
                                _ => DateTimeKind::Early,
                            };
                        }
                    },
                    Kw::Month(val) => {
                        tm_ = M_MONTH;
                        // Already have a (numeric) month? Then see if we can substitute.
                        if fmask & M_MONTH != 0
                            && !have_text_month
                            && fmask & M_DAY == 0
                            && tm.mon >= 1
                            && tm.mon <= 31
                        {
                            tm.mday = tm.mon;
                            tm_ = M_DAY;
                        }
                        have_text_month = true;
                        tm.mon = val;
                    }
                    Kw::DtzMod => {
                        tm_ = M_DTZMOD | M_DTZ;
                        tzv -= 3600;
                    }
                    Kw::Tz(east) => {
                        tm_ = M_TZ;
                        tzv = -east;
                    }
                    Kw::Region(n) => {
                        tm_ = M_TZ;
                        unsupported = Some(n);
                    }
                    Kw::AmPm(m) => {
                        tm_ = M_AMPM;
                        mer = m;
                    }
                    Kw::AdBc(b) => {
                        tm_ = M_ADBC;
                        bc = b;
                    }
                    Kw::Dow(v) => {
                        tm_ = M_DOW;
                        tm.wday = v;
                    }
                    Kw::Units(p) => {
                        tm_ = 0;
                        if ptype != Ptype::None {
                            return bad_format();
                        }
                        ptype = p;
                    }
                    Kw::IsoTime => {
                        tm_ = 0;
                        // Filler "t": requires a preceding date.
                        if fmask & DATE_M != DATE_M {
                            return bad_format();
                        }
                        if ptype != Ptype::None {
                            return bad_format();
                        }
                        ptype = Ptype::Time;
                    }
                    Kw::Unknown => return bad_format(),
                }
                tmask = tm_;
            }
        }
        if tmask & fmask != 0 {
            return bad_format();
        }
        fmask |= tmask;
    }

    // Reject if a prefix type appeared and was never handled.
    if ptype != Ptype::None {
        return bad_format();
    }

    let mut tz = None;
    if kind == DateTimeKind::Date {
        validate_date(fmask, isjulian, is2digits, bc, &mut tm)?;
        apply_meridian(mer, &mut tm)?;
        if fmask & DATE_M != DATE_M {
            // Incomplete input (PG returns a positive code that prints as bad syntax).
            return bad_format();
        }
        if let Some(name) = unsupported {
            if fmask & M_DTZMOD != 0 {
                return bad_format();
            }
            return Err(DtErr::UnsupportedTimezone(name).into());
        }
        if fmask & M_TZ == 0 && fmask & M_DTZMOD != 0 {
            return bad_format();
        }
        if fmask & M_TZ != 0 {
            tz = Some(tzv);
        }
    }
    tm.fsec = fsec;
    tm.tz = tz;
    let kind = if is_now && kind == DateTimeKind::Date { DateTimeKind::Now } else { kind };
    Ok(Decoded { kind, tm, tz })
}

// ---------------------------------------------------------------------------------------------
// DecodeTimeOnly
// ---------------------------------------------------------------------------------------------

/// PG `DecodeTimeOnly`: fields of a time / timetz literal (a date part is allowed and used only
/// to resolve the zone offset).
// CONTRACT
pub fn decode_time_only(fields: &[Field], ctx: &IoCtx) -> Result<std::result::Result<Decoded, DtErr>> {
    finish(decode_time_only_inner(fields, ctx))
}

fn decode_time_only_inner(fields: &[Field], ctx: &IoCtx) -> R<Decoded> {
    let order = ctx.config.date_order;
    let nf = fields.len();
    let mut fmask = 0u32;
    let mut ptype = Ptype::None;
    let mut tm = Tm::default();
    let mut fsec = 0i32;
    let mut tzv = 0i32;
    let mut mer = Mer::Hr24;
    let mut isjulian = false;
    let mut is2digits = false;
    let mut bc = false;
    let mut unsupported: Option<String> = None;

    for (i, f) in fields.iter().enumerate() {
        let text = f.text.as_str();
        let tmask: u32;
        match f.kind {
            FieldKind::Date => {
                if i == 0 && nf >= 2 && (fields[nf - 1].kind == FieldKind::Date || fields[1].kind == FieldKind::Time) {
                    // Under limited circumstances, we accept a date.
                    tmask = decode_date(text, fmask, order, &mut is2digits, &mut tm)?;
                } else if first_is_digit(text) {
                    if (fmask & TIME_M) == TIME_M {
                        return bad_format();
                    }
                    let cp = match text.find('-') {
                        Some(c) => c,
                        None => return bad_format(),
                    };
                    tzv = decode_timezone(&text[cp..])?;
                    let m = decode_number_field(&text[..cp], fmask | DATE_M, &mut tm, &mut fsec, &mut is2digits)?;
                    tmask = m | M_TZ;
                } else {
                    match lookup_zone(text)? {
                        Ok(ZoneSpec::Fixed { secs_east, .. }) => tzv = -secs_east,
                        Ok(ZoneSpec::Region(n)) | Err(DtErr::UnsupportedTimezone(n)) => unsupported = Some(n),
                        Err(e) => return Err(e.into()),
                    }
                    tmask = M_TZ;
                }
            }
            FieldKind::Time => {
                if ptype != Ptype::None {
                    if ptype != Ptype::Time {
                        return bad_format();
                    }
                    ptype = Ptype::None;
                }
                tmask = decode_time(text, &mut tm, &mut fsec)?;
            }
            FieldKind::Tz => {
                tzv = decode_timezone(text)?;
                tmask = M_TZ;
            }
            FieldKind::Number => {
                if ptype != Ptype::None {
                    tmask = number_after_prefix(text, ptype, fmask, &mut tm, &mut fsec, &mut is2digits, &mut isjulian)?;
                    ptype = Ptype::None;
                } else {
                    let flen = text.len();
                    if let Some(dot) = text.find('.') {
                        if i == 0 && nf >= 2 && fields[nf - 1].kind == FieldKind::Date {
                            tmask = decode_date(text, fmask, order, &mut is2digits, &mut tm)?;
                        } else if dot > 2 {
                            tmask = decode_number_field(text, fmask | DATE_M, &mut tm, &mut fsec, &mut is2digits)?;
                        } else {
                            return bad_format();
                        }
                    } else if flen > 4 {
                        tmask = decode_number_field(text, fmask | DATE_M, &mut tm, &mut fsec, &mut is2digits)?;
                    } else {
                        tmask = decode_number(
                            text,
                            false,
                            fmask | DATE_M,
                            order,
                            &mut tm,
                            &mut fsec,
                            &mut is2digits,
                        )?;
                    }
                }
            }
            FieldKind::String | FieldKind::Special => {
                let tm_: u32;
                match classify(text)? {
                    Kw::Ignore => continue,
                    Kw::Reserv(Resv::Now) => {
                        let (n, _) = local_now(ctx)?;
                        tm_ = TIME_M;
                        tm.hour = n.hour;
                        tm.min = n.min;
                        tm.sec = n.sec;
                        fsec = n.fsec;
                    }
                    Kw::Reserv(Resv::Zulu) => {
                        tm_ = TIME_M | M_TZ;
                        tm.hour = 0;
                        tm.min = 0;
                        tm.sec = 0;
                    }
                    Kw::Reserv(_) => return bad_format(),
                    Kw::DtzMod => {
                        tm_ = M_DTZMOD | M_DTZ;
                        tzv -= 3600;
                    }
                    Kw::Tz(east) => {
                        tm_ = M_TZ;
                        tzv = -east;
                    }
                    Kw::Region(n) => {
                        tm_ = M_TZ;
                        unsupported = Some(n);
                    }
                    Kw::AmPm(m) => {
                        tm_ = M_AMPM;
                        mer = m;
                    }
                    Kw::AdBc(b) => {
                        tm_ = M_ADBC;
                        bc = b;
                    }
                    Kw::Units(p) => {
                        tm_ = 0;
                        if ptype != Ptype::None {
                            return bad_format();
                        }
                        ptype = p;
                    }
                    Kw::IsoTime => {
                        tm_ = 0;
                        if ptype != Ptype::None {
                            return bad_format();
                        }
                        ptype = Ptype::Time;
                    }
                    Kw::Month(_) | Kw::Dow(_) | Kw::Unknown => return bad_format(),
                }
                tmask = tm_;
            }
        }
        if tmask & fmask != 0 {
            return bad_format();
        }
        fmask |= tmask;
    }

    if ptype != Ptype::None {
        return bad_format();
    }
    validate_date(fmask, isjulian, is2digits, bc, &mut tm)?;
    apply_meridian(mer, &mut tm)?;
    if time_overflows(tm.hour, tm.min, tm.sec, fsec) {
        return field_overflow();
    }
    if fmask & TIME_M != TIME_M {
        return bad_format();
    }
    if let Some(name) = unsupported {
        if fmask & M_DTZMOD != 0 {
            return bad_format();
        }
        return Err(DtErr::UnsupportedTimezone(name).into());
    }
    if fmask & M_TZ == 0 && fmask & M_DTZMOD != 0 {
        return bad_format();
    }
    let tz = if fmask & M_TZ != 0 { Some(tzv) } else { None };
    tm.fsec = fsec;
    tm.tz = tz;
    Ok(Decoded { kind: DateTimeKind::Date, tm, tz })
}

// ---------------------------------------------------------------------------------------------
// Convenience wrappers
// ---------------------------------------------------------------------------------------------

/// Convenience: lex + decode a full date/timestamp string, mapping failures to PG errors with
/// `type_name` (e.g. "timestamp with time zone").
// CONTRACT
pub fn parse_date_time(input: &str, ctx: &IoCtx, type_name: &str) -> Result<Decoded> {
    let fields = match parse_datetime(input)? {
        Ok(f) => f,
        Err(e) => return Err(dt_parse_error(&e, input, type_name)),
    };
    match decode_date_time(&fields, ctx)? {
        Ok(d) => Ok(d),
        Err(e) => Err(dt_parse_error(&e, input, type_name)),
    }
}

/// Convenience: lex + decode a time/timetz string.
// CONTRACT
pub fn parse_time_only(input: &str, ctx: &IoCtx, type_name: &str) -> Result<Decoded> {
    let fields = match parse_datetime(input)? {
        Ok(f) => f,
        Err(e) => return Err(dt_parse_error(&e, input, type_name)),
    };
    match decode_time_only(&fields, ctx)? {
        Ok(d) => Ok(d),
        Err(e) => Err(dt_parse_error(&e, input, type_name)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::context::IoConfig;
    use crate::registry::builtin_env;

    fn run_dt(s: &str, order: DateOrder, now: Option<i64>) -> std::result::Result<Decoded, DtErr> {
        let cfg = IoConfig { date_order: order, ..IoConfig::default() };
        let mut ctx = IoCtx::new(builtin_env(), &cfg);
        ctx.now = now;
        let fields = parse_datetime(s).unwrap().unwrap();
        decode_date_time(&fields, &ctx).unwrap()
    }
    fn dt(s: &str) -> std::result::Result<Decoded, DtErr> {
        run_dt(s, DateOrder::Mdy, None)
    }
    fn tmo(s: &str) -> std::result::Result<Decoded, DtErr> {
        let cfg = IoConfig::default();
        let ctx = IoCtx::new(builtin_env(), &cfg);
        let fields = parse_datetime(s).unwrap().unwrap();
        decode_time_only(&fields, &ctx).unwrap()
    }

    #[test]
    fn iso_date() {
        let d = dt("2024-02-29").unwrap();
        assert_eq!((d.tm.year, d.tm.mon, d.tm.mday), (2024, 2, 29));
        assert_eq!(d.tz, None);
        assert_eq!(d.kind, DateTimeKind::Date);
    }

    #[test]
    fn month_overflow() {
        assert_eq!(dt("2020-13-01"), Err(DtErr::MdFieldOverflow));
        assert_eq!(dt("2023-02-29"), Err(DtErr::FieldOverflow));
    }

    #[test]
    fn bad_syntax() {
        assert_eq!(dt("x"), Err(DtErr::BadFormat));
        assert_eq!(tmo("x"), Err(DtErr::BadFormat));
    }

    #[test]
    fn time_range() {
        assert_eq!(tmo("25:00"), Err(DtErr::FieldOverflow));
        assert!(tmo("24:00:00").is_ok());
        assert_eq!(tmo("24:00:01"), Err(DtErr::FieldOverflow));
        assert!(tmo("23:59:60").is_ok());
    }

    #[test]
    fn date_order() {
        let d = run_dt("01/02/2024", DateOrder::Mdy, None).unwrap();
        assert_eq!((d.tm.mon, d.tm.mday, d.tm.year), (1, 2, 2024));
        let d = run_dt("01/02/2024", DateOrder::Dmy, None).unwrap();
        assert_eq!((d.tm.mon, d.tm.mday, d.tm.year), (2, 1, 2024));
        let d = run_dt("24/01/02", DateOrder::Ymd, None).unwrap();
        assert_eq!((d.tm.year, d.tm.mon, d.tm.mday), (2024, 1, 2));
    }

    #[test]
    fn iso_t_zulu() {
        let d = dt("2024-02-29T13:01:02Z").unwrap();
        assert_eq!(d.tz, Some(0));
        assert_eq!((d.tm.hour, d.tm.min, d.tm.sec), (13, 1, 2));
    }

    #[test]
    fn numeric_zone_sign_is_west() {
        let d = dt("2024-02-29 13:01:02.5+02").unwrap();
        assert_eq!(d.tz, Some(-7200));
        assert_eq!(d.tm.fsec, 500_000);
        let d = dt("1999-01-08 04:05:06 -8:00").unwrap();
        assert_eq!(d.tz, Some(8 * 3600));
        assert_eq!(dt("2020-01-01 00:00 +16:00"), Err(DtErr::TzDispOverflow));
    }

    #[test]
    fn bc_year() {
        let d = dt("0044-03-15 BC").unwrap();
        assert_eq!(d.tm.year, -43);
        assert_eq!(dt("0000-01-01"), Err(DtErr::FieldOverflow));
        assert_eq!(dt("0001-01-01 BC").unwrap().tm.year, 0);
    }

    #[test]
    fn meridian() {
        assert_eq!(tmo("12:00 pm").unwrap().tm.hour, 12);
        assert_eq!(tmo("12:00 am").unwrap().tm.hour, 0);
        assert_eq!(tmo("01:30 pm").unwrap().tm.hour, 13);
        assert_eq!(tmo("13:00 pm"), Err(DtErr::FieldOverflow));
    }

    #[test]
    fn unknown_zone_name() {
        assert_eq!(dt("2020-01-01 00:00 Mars/Base"), Err(DtErr::BadTimezone("mars/base".to_string())));
    }

    #[test]
    fn specials() {
        assert_eq!(dt("epoch").unwrap().kind, DateTimeKind::Epoch);
        assert_eq!(dt("infinity").unwrap().kind, DateTimeKind::Late);
        assert_eq!(dt("-infinity").unwrap().kind, DateTimeKind::Early);
        assert_eq!(dt("epoch 2020-01-01"), Err(DtErr::BadFormat));
    }

    #[test]
    fn now_needs_clock() {
        let cfg = IoConfig::default();
        let ctx = IoCtx::new(builtin_env(), &cfg);
        let fields = parse_datetime("today").unwrap().unwrap();
        let e = decode_date_time(&fields, &ctx).unwrap_err();
        assert!(e.is_unimplemented());
    }

    #[test]
    fn today_tomorrow_yesterday() {
        // now = 2000-01-01 00:00:00 UTC
        let t = run_dt("today", DateOrder::Mdy, Some(0)).unwrap();
        assert_eq!((t.tm.year, t.tm.mon, t.tm.mday), (2000, 1, 1));
        let t = run_dt("tomorrow", DateOrder::Mdy, Some(0)).unwrap();
        assert_eq!((t.tm.year, t.tm.mon, t.tm.mday), (2000, 1, 2));
        let t = run_dt("yesterday", DateOrder::Mdy, Some(0)).unwrap();
        assert_eq!((t.tm.year, t.tm.mon, t.tm.mday), (1999, 12, 31));
        let n = run_dt("now", DateOrder::Mdy, Some(0)).unwrap();
        assert_eq!(n.kind, DateTimeKind::Now);
        assert_eq!(n.tz, Some(0));
    }

    #[test]
    fn text_months() {
        let d = dt("January 8, 1999").unwrap();
        assert_eq!((d.tm.year, d.tm.mon, d.tm.mday), (1999, 1, 8));
        let d = dt("8-Jan-1999").unwrap();
        assert_eq!((d.tm.year, d.tm.mon, d.tm.mday), (1999, 1, 8));
        let d = dt("1999-Jan-08").unwrap();
        assert_eq!((d.tm.year, d.tm.mon, d.tm.mday), (1999, 1, 8));
    }

    #[test]
    fn concatenated_forms() {
        let d = dt("19990108").unwrap();
        assert_eq!((d.tm.year, d.tm.mon, d.tm.mday), (1999, 1, 8));
        let d = dt("990108").unwrap();
        assert_eq!((d.tm.year, d.tm.mon, d.tm.mday), (1999, 1, 8));
        let d = dt("20011225T040506").unwrap();
        assert_eq!((d.tm.year, d.tm.hour, d.tm.min, d.tm.sec), (2001, 4, 5, 6));
        let d = dt("1999-01-08 040506").unwrap();
        assert_eq!((d.tm.hour, d.tm.min, d.tm.sec), (4, 5, 6));
    }

    #[test]
    fn day_of_year_and_julian() {
        let d = dt("2024.060").unwrap();
        assert_eq!((d.tm.mon, d.tm.mday), (2, 29));
        let d = dt("J2451545").unwrap();
        assert_eq!((d.tm.year, d.tm.mon, d.tm.mday), (2000, 1, 1));
    }

    #[test]
    fn two_digit_year() {
        assert_eq!(dt("1/2/69").unwrap().tm.year, 2069);
        assert_eq!(dt("1/2/70").unwrap().tm.year, 1970);
    }

    #[test]
    fn fraction_rounds() {
        let d = dt("2020-01-01 00:00:00.123456789").unwrap();
        assert_eq!(d.tm.fsec, 123_457);
    }

    #[test]
    fn allballs() {
        let d = tmo("allballs").unwrap();
        assert_eq!((d.tm.hour, d.tm.min, d.tm.sec), (0, 0, 0));
        assert_eq!(d.tz, Some(0));
    }

    #[test]
    fn time_without_date_is_bad_for_datetime() {
        assert_eq!(dt("12:00"), Err(DtErr::BadFormat));
    }

    #[test]
    fn timezone_decoder() {
        assert_eq!(decode_timezone("+05:30").ok(), Some(-19800));
        assert_eq!(decode_timezone("-0800").ok(), Some(28800));
        assert_eq!(decode_timezone("+5").ok(), Some(-18000));
        assert!(decode_timezone("+16").is_err());
        assert!(decode_timezone("05").is_err());
    }
}
