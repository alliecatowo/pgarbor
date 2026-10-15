//! Datetime text output: PG `EncodeDateOnly`, `EncodeTimeOnly`, `EncodeDateTime`,
//! `EncodeInterval`, `EncodeSpecialDate/Timestamp`, `AppendSeconds` (TYPE-06). Lease TYP-24.
//!
//! PG18 reference: `src/backend/utils/adt/datetime.c` (encoders), `timestamp.c` (`EncodeSpecialTimestamp`,
//! `EncodeSpecialInterval`), `date.c` (`EncodeSpecialDate`).
//!
//! Oracle fixtures (all from PG 18.6):
//! - ISO (default): date `2024-02-29`, `10000-01-01`, `0044-03-15 BC` (years zero-padded to 4 digits,
//!   BC suffix for astronomical year <= 0: year 0 prints `0001 BC`); timestamp `2024-02-29 13:01:02.5`
//!   (fraction printed with trailing zeros trimmed, omitted when 0; microsecond precision);
//!   timestamptz `2024-02-29 07:31:02.5+00`, `2024-02-29 13:01:02-05:30`, `2024-07-01 12:00:00-04`
//!   (zone as `+hh`, `+hh:mm`, `+hh:mm:ss` from seconds; note PG prints the OFFSET EAST with ISO sign);
//!   time `13:01:02.5`; timetz `12:34:56.789+05:30`, `04:05:06-08`, `00:00:00+05:30:15`.
//! - Postgres, MDY: `Thu Feb 29 13:01:02.5 2024`, tz: `Thu Feb 29 13:01:02.5 2024 UTC` (zone abbrev or
//!   numeric offset when no abbrev); date `02-29-2024` (DMY: `29-02-2024`).
//! - SQL, DMY: `29/02/2024 13:01:02.5`, date `29/02/2024` (MDY: `02/29/2024`), tz adds ` UTC`.
//! - German: `29.02.2024 13:01:02.5`, date `29.02.2024`, tz adds ` UTC`.
//! - Specials: date/timestamp `infinity` / `-infinity`.
//! - Interval, IntervalStyle postgres: `1 year 2 mons 3 days 04:05:06.7`, `-1 days -02:00:00`,
//!   `00:00:00` (zero), `36:00:00`, `1 day 12:00:00`, `-1 mons +2 days` (a `+` is printed on a field
//!   whose sign differs from the previous printed field), `1 mon 21 days 10:17:08.5344`; units
//!   `year(s)`, `mon(s)`, `day(s)`; time part `hh:mm:ss[.ffffff]` with hours >= 24 allowed.
//!   postgres_verbose: `@ 1 year 2 mons 3 days 4 hours 5 mins 6.7 secs`, `@ 1 day 2 hours ago`.
//!   sql_standard: `+1-2 +3 +4:05:06.7`, `-1 0:00:00`, `1-2`. iso_8601: `P1Y2M3DT4H5M6.7S`, `PT0S`.
//!   Infinite intervals: `infinity` / `-infinity`.
//!
//! Infinite values (`infinity`, `-infinity`) cannot be expressed in `Tm` / `Itm`; the caller (the
//! date/timestamp/interval I/O layer) prints them with the `EncodeSpecial*` strings. See
//! `EARLY` / `LATE` below for the spelling PG uses.
use super::calendar::{date2j, j2day};
use super::{Itm, Tm, MAX_INTERVAL_PRECISION, MAX_TIMESTAMP_PRECISION};
use crate::context::{DateOrder, DateStyle, IntervalStyle};
use pgarbor_common::{PgError, Result};
use std::fmt::Write as _;

/// PG `MAX_TIME_PRECISION` (utils/date.h).
const MAX_TIME_PRECISION: usize = 6;
/// PG `MAXTZLEN`: maximum time zone abbreviation length printed.
const MAXTZLEN: usize = 10;
/// Spelling of `-infinity` for special values (PG `EARLY`).
#[allow(dead_code)]
const EARLY: &str = "-infinity";
/// Spelling of `infinity` for special values (PG `LATE`).
#[allow(dead_code)]
const LATE: &str = "infinity";

const DAY_NAMES: [&str; 7] = ["Sun", "Mon", "Tue", "Wed", "Thu", "Fri", "Sat"];
const MONTH_NAMES: [&str; 12] = ["Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec"];

/// Format a date (no time part).
// CONTRACT
pub fn encode_date_only(tm: &Tm, style: DateStyle, order: DateOrder) -> Result<String> {
    let mut out = String::with_capacity(16);
    check_month(tm.mon)?;
    let year = display_year(tm.year);
    match style {
        DateStyle::Iso => {
            push_num(&mut out, year, 4);
            out.push('-');
            push_num(&mut out, tm.mon as i64, 2);
            out.push('-');
            push_num(&mut out, tm.mday as i64, 2);
        }
        DateStyle::Sql => {
            push_day_month(&mut out, tm, order == DateOrder::Dmy, '/');
            out.push('/');
            push_num(&mut out, year, 4);
        }
        DateStyle::German => {
            push_num(&mut out, tm.mday as i64, 2);
            out.push('.');
            push_num(&mut out, tm.mon as i64, 2);
            out.push('.');
            push_num(&mut out, year, 4);
        }
        DateStyle::Postgres => {
            push_day_month(&mut out, tm, order == DateOrder::Dmy, '-');
            out.push('-');
            push_num(&mut out, year, 4);
        }
    }
    push_bc(&mut out, tm.year);
    Ok(out)
}

/// Format a time; `tz_secs_west` = Some(zone) for timetz (printed with ISO sign: west 28800 → `-08`).
// CONTRACT
pub fn encode_time_only(tm: &Tm, tz_secs_west: Option<i32>, style: DateStyle) -> Result<String> {
    // PG's style only selects the XSD variant of the zone suffix, which DateStyle cannot express.
    let _ = style;
    let mut out = String::with_capacity(24);
    push_hms(&mut out, tm, MAX_TIME_PRECISION);
    if let Some(tz) = tz_secs_west {
        push_tz(&mut out, tz as i64);
    }
    Ok(out)
}

/// Format a timestamp; `tz_secs_west` / `tz_abbrev` present for timestamptz.
// CONTRACT
pub fn encode_date_time(
    tm: &Tm,
    tz_secs_west: Option<i32>,
    tz_abbrev: Option<&str>,
    style: DateStyle,
    order: DateOrder,
) -> Result<String> {
    let mon_idx = check_month(tm.mon)?;
    let mut out = String::with_capacity(40);
    let year = display_year(tm.year);
    match style {
        DateStyle::Iso => {
            push_num(&mut out, year, 4);
            out.push('-');
            push_num(&mut out, tm.mon as i64, 2);
            out.push('-');
            push_num(&mut out, tm.mday as i64, 2);
            out.push(' ');
            push_hms(&mut out, tm, MAX_TIMESTAMP_PRECISION as usize);
            if let Some(tz) = tz_secs_west {
                push_tz(&mut out, tz as i64);
            }
        }
        DateStyle::Sql => {
            push_day_month(&mut out, tm, order == DateOrder::Dmy, '/');
            out.push('/');
            push_num(&mut out, year, 4);
            out.push(' ');
            push_hms(&mut out, tm, MAX_TIMESTAMP_PRECISION as usize);
            push_tz_named(&mut out, tz_secs_west, tz_abbrev);
        }
        DateStyle::German => {
            push_num(&mut out, tm.mday as i64, 2);
            out.push('.');
            push_num(&mut out, tm.mon as i64, 2);
            out.push('.');
            push_num(&mut out, year, 4);
            out.push(' ');
            push_hms(&mut out, tm, MAX_TIMESTAMP_PRECISION as usize);
            push_tz_named(&mut out, tz_secs_west, tz_abbrev);
        }
        DateStyle::Postgres => {
            let jd = date2j(tm.year, tm.mon, tm.mday);
            let dow = j2day(jd).rem_euclid(7) as usize;
            out.push_str(DAY_NAMES[dow]);
            out.push(' ');
            if order == DateOrder::Dmy {
                push_num(&mut out, tm.mday as i64, 2);
                out.push(' ');
                out.push_str(MONTH_NAMES[mon_idx]);
            } else {
                out.push_str(MONTH_NAMES[mon_idx]);
                out.push(' ');
                push_num(&mut out, tm.mday as i64, 2);
            }
            out.push(' ');
            push_hms(&mut out, tm, MAX_TIMESTAMP_PRECISION as usize);
            out.push(' ');
            push_num(&mut out, year, 4);
            // Postgres style always separates the zone with a space (name or numeric form).
            if let Some(tz) = tz_secs_west {
                out.push(' ');
                match tz_abbrev {
                    Some(name) => push_tzname(&mut out, name),
                    None => push_tz(&mut out, tz as i64),
                }
            }
        }
    }
    push_bc(&mut out, tm.year);
    Ok(out)
}

/// Format an interval in the given IntervalStyle.
// CONTRACT
pub fn encode_interval(itm: &Itm, style: IntervalStyle) -> Result<String> {
    let f = Fields {
        year: itm.year as i64,
        mon: itm.mon as i64,
        mday: itm.mday as i64,
        hour: itm.hour,
        min: itm.min as i64,
        sec: itm.sec as i64,
        fsec: itm.usec as i64,
    };
    let mut out = String::with_capacity(48);
    match style {
        IntervalStyle::SqlStandard => encode_interval_sql_standard(&mut out, f),
        IntervalStyle::Iso8601 => encode_interval_iso8601(&mut out, f),
        IntervalStyle::Postgres => encode_interval_postgres(&mut out, f),
        IntervalStyle::PostgresVerbose => encode_interval_verbose(&mut out, f),
    }
    Ok(out)
}

/// Zone offset text as PG prints it in ISO style: east-positive `+hh[:mm[:ss]]`
/// (`tz_secs_west = -19800` → `+05:30`, `28800` → `-08`).
// CONTRACT
pub fn encode_timezone(tz_secs_west: i32) -> Result<String> {
    let mut out = String::with_capacity(9);
    push_tz(&mut out, tz_secs_west as i64);
    Ok(out)
}

// ---------------------------------------------------------------------------------------------
// Private helpers
// ---------------------------------------------------------------------------------------------

/// Interval fields widened to i64 (PG `EncodeInterval` locals).
#[derive(Clone, Copy)]
struct Fields {
    year: i64,
    mon: i64,
    mday: i64,
    hour: i64,
    min: i64,
    sec: i64,
    fsec: i64,
}

impl Fields {
    fn negated(self) -> Fields {
        Fields {
            year: self.year.wrapping_neg(),
            mon: self.mon.wrapping_neg(),
            mday: self.mday.wrapping_neg(),
            hour: self.hour.wrapping_neg(),
            min: self.min.wrapping_neg(),
            sec: self.sec.wrapping_neg(),
            fsec: self.fsec.wrapping_neg(),
        }
    }
}

/// Validate a 1-based month and return its 0-based index into the name tables.
fn check_month(mon: i32) -> Result<usize> {
    if (1..=12).contains(&mon) {
        Ok((mon - 1) as usize)
    } else {
        Err(PgError::internal(format!("invalid month {mon} in date output")))
    }
}

/// PG prints `tm_year > 0 ? tm_year : -(tm_year - 1)` (1 BC is astronomical year 0).
fn display_year(year: i32) -> i64 {
    if year > 0 {
        year as i64
    } else {
        1 - year as i64
    }
}

/// Append ` BC` for astronomical years <= 0.
fn push_bc(out: &mut String, year: i32) {
    if year <= 0 {
        out.push_str(" BC");
    }
}

/// Zero-padded decimal, PG `pg_ultostr_zeropad`.
fn push_num(out: &mut String, v: i64, width: usize) {
    let _ = write!(out, "{:0width$}", v, width = width);
}

/// `DD<sep>MM` when DMY, otherwise `MM<sep>DD` (PG date styles: SQL and Postgres).
fn push_day_month(out: &mut String, tm: &Tm, dmy: bool, sep: char) {
    if dmy {
        push_num(out, tm.mday as i64, 2);
        out.push(sep);
        push_num(out, tm.mon as i64, 2);
    } else {
        push_num(out, tm.mon as i64, 2);
        out.push(sep);
        push_num(out, tm.mday as i64, 2);
    }
}

/// `hh:mm:ss[.ffffff]` with trailing zeros trimmed from the fraction (PG `EncodeTimeOnly` body).
fn push_hms(out: &mut String, tm: &Tm, precision: usize) {
    push_num(out, tm.hour as i64, 2);
    out.push(':');
    push_num(out, tm.min as i64, 2);
    out.push(':');
    append_seconds(out, tm.sec as i64, tm.fsec as i64, precision, true);
}

/// Zone for the SQL and German timestamp styles: ` <abbrev>` when a name is known,
/// otherwise the numeric offset directly after the seconds (PG `EncodeDateTime`).
fn push_tz_named(out: &mut String, tz_secs_west: Option<i32>, tz_abbrev: Option<&str>) {
    if let Some(tz) = tz_secs_west {
        match tz_abbrev {
            Some(name) => {
                out.push(' ');
                push_tzname(out, name);
            }
            None => push_tz(out, tz as i64),
        }
    }
}

/// Zone abbreviation, truncated to PG `MAXTZLEN` characters (abbreviations are ASCII).
fn push_tzname(out: &mut String, name: &str) {
    out.extend(name.chars().take(MAXTZLEN));
}

/// PG `AppendSeconds`: `sec` (zero-padded to 2 digits when `fillzeros`), then `.` and the
/// fraction with trailing zeros removed. Signs are dropped (PG uses abs()).
fn append_seconds(out: &mut String, sec: i64, fsec: i64, precision: usize, fillzeros: bool) {
    let s = sec.unsigned_abs();
    if fillzeros {
        let _ = write!(out, "{:02}", s);
    } else {
        let _ = write!(out, "{}", s);
    }
    if fsec != 0 {
        let v = fsec.unsigned_abs();
        // PG falls back to printing the raw digits when the value does not fit the precision.
        let limit = 10u64.checked_pow(precision as u32).unwrap_or(u64::MAX);
        if v >= limit {
            let _ = write!(out, ".{}", v);
        } else {
            let digits = format!("{:0width$}", v, width = precision);
            let trimmed = digits.trim_end_matches('0');
            if !trimmed.is_empty() {
                out.push('.');
                out.push_str(trimmed);
            }
        }
    }
}

/// PG `EncodeTimezone`: `tz_secs_west` is negated relative to the printed sign. Prints `+hh`, `+hh:mm`
/// or `+hh:mm:ss`, using the shortest form that is exact.
fn push_tz(out: &mut String, tz_secs_west: i64) {
    let total = tz_secs_west.unsigned_abs();
    let sec = total % 60;
    let mins_total = total / 60;
    let min = mins_total % 60;
    let hour = mins_total / 60;
    out.push(if tz_secs_west <= 0 { '+' } else { '-' });
    if sec != 0 {
        let _ = write!(out, "{:02}:{:02}:{:02}", hour, min, sec);
    } else if min != 0 {
        let _ = write!(out, "{:02}:{:02}", hour, min);
    } else {
        let _ = write!(out, "{:02}", hour);
    }
}

/// PG `EncodeInterval`, `INTSTYLE_SQL_STANDARD`.
fn encode_interval_sql_standard(out: &mut String, f: Fields) {
    let has_negative = f.year < 0
        || f.mon < 0
        || f.mday < 0
        || f.hour < 0
        || f.min < 0
        || f.sec < 0
        || f.fsec < 0;
    let has_positive = f.year > 0
        || f.mon > 0
        || f.mday > 0
        || f.hour > 0
        || f.min > 0
        || f.sec > 0
        || f.fsec > 0;
    let has_year_month = f.year != 0 || f.mon != 0;
    let has_day_time = f.mday != 0 || f.hour != 0 || f.min != 0 || f.sec != 0 || f.fsec != 0;
    let has_day = f.mday != 0;
    let sql_standard_value = !(has_negative && has_positive) && !(has_year_month && has_day_time);

    // SQL standard wants a single leading sign for the whole interval.
    let f = if has_negative && sql_standard_value {
        out.push('-');
        f.negated()
    } else {
        f
    };

    if !has_negative && !has_positive {
        out.push('0');
    } else if !sql_standard_value {
        // Mixed signs: spell every component's sign so the value is unambiguous.
        let year_sign = if f.year < 0 || f.mon < 0 { '-' } else { '+' };
        let day_sign = if f.mday < 0 { '-' } else { '+' };
        let sec_sign = if f.hour < 0 || f.min < 0 || f.sec < 0 || f.fsec < 0 { '-' } else { '+' };
        let _ = write!(
            out,
            "{}{}-{} {}{} {}{}:{:02}:",
            year_sign,
            f.year.unsigned_abs(),
            f.mon.unsigned_abs(),
            day_sign,
            f.mday.unsigned_abs(),
            sec_sign,
            f.hour.unsigned_abs(),
            f.min.unsigned_abs()
        );
        append_seconds(out, f.sec, f.fsec, MAX_INTERVAL_PRECISION as usize, true);
    } else if has_year_month {
        let _ = write!(out, "{}-{}", f.year, f.mon);
    } else if has_day {
        let _ = write!(out, "{} {}:{:02}:", f.mday, f.hour, f.min);
        append_seconds(out, f.sec, f.fsec, MAX_INTERVAL_PRECISION as usize, true);
    } else {
        let _ = write!(out, "{}:{:02}:", f.hour, f.min);
        append_seconds(out, f.sec, f.fsec, MAX_INTERVAL_PRECISION as usize, true);
    }
}

/// PG `AddISO8601IntPart`: append `<value><unit>` only when nonzero.
fn push_iso_part(out: &mut String, value: i64, unit: char) {
    if value != 0 {
        let _ = write!(out, "{}{}", value, unit);
    }
}

/// PG `EncodeInterval`, `INTSTYLE_ISO_8601`.
fn encode_interval_iso8601(out: &mut String, f: Fields) {
    if f.year == 0 && f.mon == 0 && f.mday == 0 && f.hour == 0 && f.min == 0 && f.sec == 0 && f.fsec == 0 {
        out.push_str("PT0S");
        return;
    }
    out.push('P');
    push_iso_part(out, f.year, 'Y');
    push_iso_part(out, f.mon, 'M');
    push_iso_part(out, f.mday, 'D');
    if f.hour != 0 || f.min != 0 || f.sec != 0 || f.fsec != 0 {
        out.push('T');
    }
    push_iso_part(out, f.hour, 'H');
    push_iso_part(out, f.min, 'M');
    if f.sec != 0 || f.fsec != 0 {
        if f.sec < 0 || f.fsec < 0 {
            out.push('-');
        }
        append_seconds(out, f.sec, f.fsec, MAX_INTERVAL_PRECISION as usize, false);
        out.push('S');
    }
}

/// PG `AddPostgresIntPart`: one date field of the `postgres` style.
fn push_postgres_part(out: &mut String, value: i64, units: &str, is_zero: &mut bool, is_before: &mut bool) {
    if value == 0 {
        return;
    }
    if !*is_zero {
        out.push(' ');
    }
    if *is_before && value > 0 {
        out.push('+');
    }
    let _ = write!(out, "{} {}{}", value, units, if value != 1 { "s" } else { "" });
    // Each nonzero field sets is_before for (only) the next one.
    *is_before = value < 0;
    *is_zero = false;
}

/// PG `EncodeInterval`, `INTSTYLE_POSTGRES`.
fn encode_interval_postgres(out: &mut String, f: Fields) {
    let mut is_before = false;
    let mut is_zero = true;
    push_postgres_part(out, f.year, "year", &mut is_zero, &mut is_before);
    push_postgres_part(out, f.mon, "mon", &mut is_zero, &mut is_before);
    push_postgres_part(out, f.mday, "day", &mut is_zero, &mut is_before);
    if is_zero || f.hour != 0 || f.min != 0 || f.sec != 0 || f.fsec != 0 {
        let minus = f.hour < 0 || f.min < 0 || f.sec < 0 || f.fsec < 0;
        if !is_zero {
            out.push(' ');
        }
        if minus {
            out.push('-');
        } else if is_before {
            out.push('+');
        }
        let _ = write!(out, "{:02}:{:02}:", f.hour.unsigned_abs(), f.min.unsigned_abs());
        append_seconds(out, f.sec, f.fsec, MAX_INTERVAL_PRECISION as usize, true);
    }
}

/// PG `AddVerboseIntPart`: one field of the `postgres_verbose` style.
fn push_verbose_part(out: &mut String, value: i64, units: &str, is_zero: &mut bool, is_before: &mut bool) {
    if value == 0 {
        return;
    }
    let v = if *is_zero {
        // The first nonzero field decides the sign of the whole interval.
        *is_before = value < 0;
        value.wrapping_abs()
    } else if *is_before {
        value.wrapping_neg()
    } else {
        value
    };
    let _ = write!(out, " {} {}{}", v, units, if v == 1 { "" } else { "s" });
    *is_zero = false;
}

/// PG `EncodeInterval`, `INTSTYLE_POSTGRES_VERBOSE`.
fn encode_interval_verbose(out: &mut String, f: Fields) {
    let mut is_before = false;
    let mut is_zero = true;
    out.push('@');
    push_verbose_part(out, f.year, "year", &mut is_zero, &mut is_before);
    push_verbose_part(out, f.mon, "mon", &mut is_zero, &mut is_before);
    push_verbose_part(out, f.mday, "day", &mut is_zero, &mut is_before);
    push_verbose_part(out, f.hour, "hour", &mut is_zero, &mut is_before);
    push_verbose_part(out, f.min, "min", &mut is_zero, &mut is_before);
    if f.sec != 0 || f.fsec != 0 {
        out.push(' ');
        if f.sec < 0 || (f.sec == 0 && f.fsec < 0) {
            if is_zero {
                is_before = true;
            } else if !is_before {
                out.push('-');
            }
        } else if is_before {
            out.push('-');
        }
        append_seconds(out, f.sec, f.fsec, MAX_INTERVAL_PRECISION as usize, false);
        // The output says "ago" rather than negating, so the unit uses the absolute value.
        let _ = write!(out, " sec{}", if f.sec.unsigned_abs() != 1 || f.fsec != 0 { "s" } else { "" });
        is_zero = false;
    }
    // Identically zero: print a unitless zero.
    if is_zero {
        out.push_str(" 0");
    }
    if is_before {
        out.push_str(" ago");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ymd(year: i32, mon: i32, mday: i32) -> Tm {
        Tm { year, mon, mday, ..Default::default() }
    }

    fn hms(hour: i32, min: i32, sec: i32, fsec: i32) -> Tm {
        Tm { year: 2024, mon: 2, mday: 29, hour, min, sec, fsec, ..Default::default() }
    }

    fn iv(year: i32, mon: i32, mday: i32, hour: i64, min: i32, sec: i32, usec: i32) -> Itm {
        Itm { year, mon, mday, hour, min, sec, usec }
    }

    #[test]
    fn iso_dates() {
        assert_eq!(encode_date_only(&ymd(2024, 2, 29), DateStyle::Iso, DateOrder::Mdy).unwrap(), "2024-02-29");
        assert_eq!(encode_date_only(&ymd(10000, 1, 1), DateStyle::Iso, DateOrder::Mdy).unwrap(), "10000-01-01");
        // 44 BC is astronomical year -43.
        assert_eq!(encode_date_only(&ymd(-43, 3, 15), DateStyle::Iso, DateOrder::Mdy).unwrap(), "0044-03-15 BC");
        // Year 0 is 1 BC.
        assert_eq!(encode_date_only(&ymd(0, 1, 1), DateStyle::Iso, DateOrder::Mdy).unwrap(), "0001-01-01 BC");
    }

    #[test]
    fn other_date_styles() {
        let d = ymd(2024, 2, 29);
        assert_eq!(encode_date_only(&d, DateStyle::Postgres, DateOrder::Mdy).unwrap(), "02-29-2024");
        assert_eq!(encode_date_only(&d, DateStyle::Postgres, DateOrder::Dmy).unwrap(), "29-02-2024");
        assert_eq!(encode_date_only(&d, DateStyle::Sql, DateOrder::Mdy).unwrap(), "02/29/2024");
        assert_eq!(encode_date_only(&d, DateStyle::Sql, DateOrder::Dmy).unwrap(), "29/02/2024");
        assert_eq!(encode_date_only(&d, DateStyle::German, DateOrder::Mdy).unwrap(), "29.02.2024");
    }

    #[test]
    fn invalid_month_is_error_not_panic() {
        assert!(encode_date_only(&ymd(2024, 13, 1), DateStyle::Iso, DateOrder::Mdy).is_err());
    }

    #[test]
    fn timestamps_iso() {
        let t = Tm { year: 2024, mon: 2, mday: 29, hour: 7, min: 31, sec: 2, fsec: 500_000, ..Default::default() };
        assert_eq!(
            encode_date_time(&t, Some(0), Some("UTC"), DateStyle::Iso, DateOrder::Mdy).unwrap(),
            "2024-02-29 07:31:02.5+00"
        );
        let t2 = hms(13, 1, 2, 0);
        assert_eq!(
            encode_date_time(&t2, Some(19800), None, DateStyle::Iso, DateOrder::Mdy).unwrap(),
            "2024-02-29 13:01:02-05:30"
        );
        // Without a zone, no suffix.
        assert_eq!(encode_date_time(&t2, None, None, DateStyle::Iso, DateOrder::Mdy).unwrap(), "2024-02-29 13:01:02");
    }

    #[test]
    fn timestamp_postgres_and_sql_styles() {
        let t = hms(13, 1, 2, 500_000);
        assert_eq!(
            encode_date_time(&t, Some(0), Some("UTC"), DateStyle::Postgres, DateOrder::Mdy).unwrap(),
            "Thu Feb 29 13:01:02.5 2024 UTC"
        );
        assert_eq!(
            encode_date_time(&t, Some(0), None, DateStyle::Postgres, DateOrder::Mdy).unwrap(),
            "Thu Feb 29 13:01:02.5 2024 +00"
        );
        assert_eq!(
            encode_date_time(&t, None, None, DateStyle::Sql, DateOrder::Dmy).unwrap(),
            "29/02/2024 13:01:02.5"
        );
        assert_eq!(
            encode_date_time(&t, None, None, DateStyle::German, DateOrder::Mdy).unwrap(),
            "29.02.2024 13:01:02.5"
        );
    }

    #[test]
    fn bc_timestamp_postgres_style_suffix() {
        let t = Tm { year: -43, mon: 3, mday: 15, hour: 0, min: 0, sec: 0, fsec: 0, ..Default::default() };
        let s = encode_date_time(&t, None, None, DateStyle::Postgres, DateOrder::Mdy).unwrap();
        assert!(s.ends_with(" 0044 BC"), "{s}");
    }

    #[test]
    fn time_and_timetz() {
        assert_eq!(encode_time_only(&hms(13, 1, 2, 500_000), None, DateStyle::Iso).unwrap(), "13:01:02.5");
        assert_eq!(encode_time_only(&hms(0, 0, 0, 0), Some(-19815), DateStyle::Iso).unwrap(), "00:00:00+05:30:15");
        assert_eq!(encode_time_only(&hms(4, 5, 6, 0), Some(28800), DateStyle::Iso).unwrap(), "04:05:06-08");
        assert_eq!(encode_time_only(&hms(12, 34, 56, 789_000), Some(-19800), DateStyle::Iso).unwrap(), "12:34:56.789+05:30");
    }

    #[test]
    fn timezone_offsets() {
        assert_eq!(encode_timezone(0).unwrap(), "+00");
        assert_eq!(encode_timezone(-19800).unwrap(), "+05:30");
        assert_eq!(encode_timezone(28800).unwrap(), "-08");
        assert_eq!(encode_timezone(-19815).unwrap(), "+05:30:15");
    }

    #[test]
    fn append_seconds_fraction() {
        let mut s = String::new();
        append_seconds(&mut s, 6, 700_000, 6, false);
        assert_eq!(s, "6.7");
        s.clear();
        append_seconds(&mut s, 6, 0, 6, true);
        assert_eq!(s, "06");
        s.clear();
        append_seconds(&mut s, 6, 534_400, 6, true);
        assert_eq!(s, "06.5344");
    }

    #[test]
    fn interval_postgres_style() {
        let style = IntervalStyle::Postgres;
        assert_eq!(encode_interval(&iv(1, 2, 3, 4, 5, 6, 700_000), style).unwrap(), "1 year 2 mons 3 days 04:05:06.7");
        assert_eq!(encode_interval(&iv(0, 0, -1, -2, 0, 0, 0), style).unwrap(), "-1 days -02:00:00");
        assert_eq!(encode_interval(&iv(0, -1, 2, 0, 0, 0, 0), style).unwrap(), "-1 mons +2 days");
        assert_eq!(encode_interval(&iv(0, 0, 0, 0, 0, 0, 0), style).unwrap(), "00:00:00");
        assert_eq!(encode_interval(&iv(0, 0, 0, 36, 0, 0, 0), style).unwrap(), "36:00:00");
        assert_eq!(encode_interval(&iv(0, 1, 21, 10, 17, 8, 534_400), style).unwrap(), "1 mon 21 days 10:17:08.5344");
    }

    #[test]
    fn interval_sql_standard_style() {
        let style = IntervalStyle::SqlStandard;
        assert_eq!(encode_interval(&iv(1, 2, 3, 4, 5, 6, 700_000), style).unwrap(), "+1-2 +3 +4:05:06.7");
        assert_eq!(encode_interval(&iv(0, 0, 0, 0, 0, 0, 0), style).unwrap(), "0");
        assert_eq!(encode_interval(&iv(0, 0, -1, -2, 0, 0, 0), style).unwrap(), "-1 2:00:00");
        assert_eq!(encode_interval(&iv(1, 2, 0, 0, 0, 0, 0), style).unwrap(), "1-2");
    }

    #[test]
    fn interval_iso8601_style() {
        let style = IntervalStyle::Iso8601;
        assert_eq!(encode_interval(&iv(1, 2, 3, 4, 5, 6, 700_000), style).unwrap(), "P1Y2M3DT4H5M6.7S");
        assert_eq!(encode_interval(&iv(0, 0, 0, 0, 0, 0, 0), style).unwrap(), "PT0S");
    }

    #[test]
    fn interval_verbose_style() {
        let style = IntervalStyle::PostgresVerbose;
        assert_eq!(
            encode_interval(&iv(1, 2, 3, 4, 5, 6, 700_000), style).unwrap(),
            "@ 1 year 2 mons 3 days 4 hours 5 mins 6.7 secs"
        );
        assert_eq!(encode_interval(&iv(0, 0, -1, -2, 0, 0, 0), style).unwrap(), "@ 1 day 2 hours ago");
        assert_eq!(encode_interval(&iv(0, 0, 0, 0, 0, 0, 0), style).unwrap(), "@ 0");
    }
}
