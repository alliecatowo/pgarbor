//! Calendar arithmetic and broken-down time conversions (TYPE-06). Lease TYP-21.
//!
//! PG18 reference: `utils/adt/datetime.c` (`date2j`, `j2date`, `j2day`, `ValidateDate`,
//! `day_tab`), `utils/adt/timestamp.c` (`tm2timestamp`, `timestamp2tm`, `dt2time`, `time2t`,
//! `interval2itm`, `itm2interval`, `itmin2interval`), `utils/adt/date.c` (`time2tm`, `tm2time`,
//! `timetz2tm`, `tm2timetz`), `timestamp.c` ISO week helpers (`date2isoweek`, `isoweek2j`,
//! `isoweek2date`, `isoweekdate2date`, `date2isoyear`, `date2isoyearday`).
//!
//! Conventions: proleptic Gregorian calendar; astronomical years in `Tm.year` (1 BC = 0);
//! Julian day numbers as in PG (`date2j(2000,1,1)` = 2451545, `date2j(1970,1,1)` = 2440588,
//! `date2j(-4713,11,24)` = 0). `j2day(j)`: 0 = Sunday.
//! `timestamp2tm` for timestamptz takes the zone offset into account (`tz_secs_west` = zone in PG
//! sign convention) and fills `tm.tz`; range errors → 22008 `timestamp out of range`.
//! `tm2timestamp` errors → 22008 `timestamp out of range` (callers re-raise with the input string,
//! e.g. `timestamp out of range: "294277-01-01"` from input routines).
use super::{Itm, Tm};
use crate::datum::Interval;
use pgarbor_common::{PgError, Result};

/// Gregorian date → Julian day number (PG `date2j`). Pure arithmetic, never fails for valid ranges.
pub fn date2j(year: i32, month: i32, day: i32) -> i32 {
    // PostgreSQL's date2j (datetime.c); wrapping ops mirror C int arithmetic (no debug panics).
    let (mut y, mut m) = (year, month);
    if m > 2 {
        m = m.wrapping_add(1);
        y = y.wrapping_add(4800);
    } else {
        m = m.wrapping_add(13);
        y = y.wrapping_add(4799);
    }
    let century = y / 100;
    let mut julian = y.wrapping_mul(365).wrapping_sub(32167);
    julian = julian.wrapping_add(y / 4 - century + century / 4);
    julian = julian.wrapping_add(7834i32.wrapping_mul(m) / 256 + day);
    julian
}

/// Julian day number → (year, month, day) (PG `j2date`, unsigned wrapping arithmetic as in C).
pub fn j2date(jd: i32) -> (i32, i32, i32) {
    let mut julian: u32 = (jd as u32).wrapping_add(32044);
    let mut quad: u32 = julian / 146097;
    let extra: u32 = julian.wrapping_sub(quad.wrapping_mul(146097)).wrapping_mul(4).wrapping_add(3);
    julian = julian.wrapping_add(60u32.wrapping_add(quad.wrapping_mul(3)).wrapping_add(extra / 146097));
    quad = julian / 1461;
    julian = julian.wrapping_sub(quad.wrapping_mul(1461));
    let mut y: i32 = (julian.wrapping_mul(4) / 1461) as i32;
    julian = if y != 0 { julian.wrapping_add(305) % 365 } else { julian.wrapping_add(306) % 366 } + 123;
    y = y.wrapping_add(quad.wrapping_mul(4) as i32);
    let year = y.wrapping_sub(4800);
    let q = julian.wrapping_mul(2141) / 65536;
    let day = julian.wrapping_sub(7834u32.wrapping_mul(q) / 256) as i32;
    let month = ((q + 10) % 12 + 1) as i32;
    (year, month, day)
}

/// Day of week of a Julian day, 0 = Sunday (PG `j2day`).
pub fn j2day(jd: i32) -> i32 {
    let d = (jd as i64 + 1) % 7;
    if d < 0 { (d + 7) as i32 } else { d as i32 }
}

/// Leap year test on astronomical years.
pub fn is_leap(year: i32) -> bool {
    year % 4 == 0 && (year % 100 != 0 || year % 400 == 0)
}

/// Days in month (1..=12) for an astronomical year.
pub fn days_in_month(year: i32, month: i32) -> i32 {
    const DAYS: [i32; 12] = [31, 28, 31, 30, 31, 30, 31, 31, 30, 31, 30, 31];
    if month == 2 && is_leap(year) { 29 } else { DAYS[((month - 1).clamp(0, 11)) as usize] }
}

/// Broken-down fields → timestamp µs since 2000-01-01, interpreting the fields as local time at
/// `tz_secs_west` (None = UTC / timestamp without tz). PG `tm2timestamp`.
// CONTRACT
#[allow(unused_variables)]
pub fn tm2timestamp(tm: &Tm, tz_secs_west: Option<i32>) -> Result<i64> {
    Err(PgError::unimplemented("TYP-21", "timestamp construction"))
}

/// Timestamp → broken-down fields, shifted by `-tz_secs_west` when given (PG `timestamp2tm`).
/// Fills `wday`, `yday`, `tz`.
// CONTRACT
#[allow(unused_variables)]
pub fn timestamp2tm(ts: i64, tz_secs_west: Option<i32>) -> Result<Tm> {
    Err(PgError::unimplemented("TYP-21", "timestamp decomposition"))
}

/// µs since midnight → (hour, min, sec, fsec) (PG `time2tm`).
// CONTRACT
#[allow(unused_variables)]
pub fn time2tm(t: i64) -> Result<Tm> {
    Err(PgError::unimplemented("TYP-21", "time decomposition"))
}

/// (hour, min, sec, fsec) → µs since midnight (PG `tm2time`).
// CONTRACT
#[allow(unused_variables)]
pub fn tm2time(tm: &Tm) -> Result<i64> {
    Err(PgError::unimplemented("TYP-21", "time construction"))
}

/// Interval → broken-down interval (PG `interval2itm`).
// CONTRACT
#[allow(unused_variables)]
pub fn interval2itm(iv: &Interval) -> Result<Itm> {
    Err(PgError::unimplemented("TYP-21", "interval decomposition"))
}

/// Broken-down interval → interval; overflow → 22008 `interval out of range` (PG `itm2interval`).
// CONTRACT
#[allow(unused_variables)]
pub fn itm2interval(itm: &Itm) -> Result<Interval> {
    Err(PgError::unimplemented("TYP-21", "interval construction"))
}

/// ISO 8601 week number of a date (PG `date2isoweek`).
// CONTRACT
#[allow(unused_variables)]
pub fn date2isoweek(year: i32, mon: i32, mday: i32) -> Result<i32> {
    Err(PgError::unimplemented("TYP-21", "ISO week"))
}

/// ISO 8601 week-numbering year of a date (PG `date2isoyear`).
// CONTRACT
#[allow(unused_variables)]
pub fn date2isoyear(year: i32, mon: i32, mday: i32) -> Result<i32> {
    Err(PgError::unimplemented("TYP-21", "ISO year"))
}

/// Day of the ISO year, 1..=371 (PG `date2isoyearday`).
// CONTRACT
#[allow(unused_variables)]
pub fn date2isoyearday(year: i32, mon: i32, mday: i32) -> Result<i32> {
    Err(PgError::unimplemented("TYP-21", "ISO year day"))
}

/// Julian day of Monday of ISO week `week` of ISO year `year` (PG `isoweek2j`).
// CONTRACT
#[allow(unused_variables)]
pub fn isoweek2j(year: i32, week: i32) -> Result<i32> {
    Err(PgError::unimplemented("TYP-21", "ISO week to date"))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn julian_roundtrip() {
        assert_eq!(date2j(2000, 1, 1), crate::datetime::POSTGRES_EPOCH_JDATE);
        assert_eq!(date2j(1970, 1, 1), crate::datetime::UNIX_EPOCH_JDATE);
        assert_eq!(date2j(-4713, 11, 24), 0);
        assert_eq!(j2date(2451545), (2000, 1, 1));
        assert_eq!(j2date(date2j(2024, 2, 29)), (2024, 2, 29));
        assert_eq!(j2day(date2j(2000, 1, 1)), 6); // Saturday
    }
}
