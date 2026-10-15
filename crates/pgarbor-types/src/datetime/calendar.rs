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
use super::{
    Itm, Tm, DATETIME_MIN_JULIAN, DATE_END_JULIAN, END_TIMESTAMP, MIN_TIMESTAMP, MONTHS_PER_YEAR,
    POSTGRES_EPOCH_JDATE, USECS_PER_DAY, USECS_PER_HOUR, USECS_PER_MINUTE, USECS_PER_SEC,
};
use super::out_of_range;
use crate::datum::Interval;
use pgarbor_common::Result;

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

// ---------------------------------------------------------------------------------------------
// Private helpers (PGArbor port of the PG 18 macros and static helpers used above).
// ---------------------------------------------------------------------------------------------

const JULIAN_MINYEAR: i32 = -4713;
const JULIAN_MINMONTH: i32 = 11;
const JULIAN_MAXYEAR: i32 = 5874898;
const JULIAN_MAXMONTH: i32 = 6;

/// PG `IS_VALID_JULIAN`. PG 18 tests the year and month only; the final timestamp range check
/// in the callers catches the remaining out-of-range values.
fn is_valid_julian(year: i32, mon: i32) -> bool {
    (year > JULIAN_MINYEAR || (year == JULIAN_MINYEAR && mon >= JULIAN_MINMONTH))
        && (year < JULIAN_MAXYEAR || (year == JULIAN_MAXYEAR && mon < JULIAN_MAXMONTH))
}

/// Full field validity for a Gregorian date (used by the ISO helpers and `tm2timestamp`).
fn valid_ymd(year: i32, mon: i32, mday: i32) -> bool {
    is_valid_julian(year, mon) && (1..=12).contains(&mon) && mday >= 1 && mday <= days_in_month(year, mon)
}

fn oor_timestamp() -> pgarbor_common::PgError {
    out_of_range("timestamp")
}

/// Julian day of a Gregorian date in i64 (same formula as `date2j`, no wrapping).
/// Callers pass values bounded by i32 magnitude, so the intermediate products fit in i64.
fn jd64(year: i64, month: i64, day: i64) -> i64 {
    let (y, m) = if month > 2 { (year + 4800, month + 1) } else { (year + 4799, month + 13) };
    let century = y / 100;
    let mut julian = y * 365 - 32167;
    julian += y / 4 - century + century / 4;
    julian += 7834 * m / 256 + day;
    julian
}

/// Day of week of a Julian day in i64, 0 = Sunday.
fn wday64(jd: i64) -> i64 {
    (jd + 1).rem_euclid(7)
}

/// Julian day of the Monday of week 1 of an ISO year (PG `isoweek2j(year, 1)`).
/// `day4` is the fourth day of the year; `day4 - wday(day4 - 1)` is the Monday on or before it.
fn iso_week1_monday(iso_year: i64) -> i64 {
    let day4 = jd64(iso_year, 1, 4);
    day4 - wday64(day4 - 1)
}

/// Returns (ISO year, ISO week) of the day with Julian number `dayn` falling in calendar `year`.
/// Mirrors the PG `date2isoweek` / `date2isoyear` control flow exactly.
fn iso_year_and_week(year: i64, dayn: i64) -> (i64, i64) {
    let mut iso_year = year;
    let mut monday = iso_week1_monday(year);
    if dayn < monday {
        iso_year = year - 1;
        monday = iso_week1_monday(iso_year);
    }
    let mut week = (dayn - monday) / 7 + 1;
    if week >= 52 {
        let next = iso_week1_monday(iso_year + 1);
        if dayn >= next {
            iso_year += 1;
            week = (dayn - next) / 7 + 1;
        }
    }
    (iso_year, week)
}

/// Splits µs since midnight (0..=USECS_PER_DAY) into (hour, min, sec, fsec).
fn split_time(time: i64) -> (i32, i32, i32, i32) {
    let hour = time / USECS_PER_HOUR;
    let rem = time - hour * USECS_PER_HOUR;
    let min = rem / USECS_PER_MINUTE;
    let rem = rem - min * USECS_PER_MINUTE;
    let sec = rem / USECS_PER_SEC;
    let fsec = rem - sec * USECS_PER_SEC;
    (hour as i32, min as i32, sec as i32, fsec as i32)
}

/// PG `time2t`: (hour, min, sec, fsec) → µs, with overflow reported as `None`.
fn time2t(hour: i32, min: i32, sec: i32, fsec: i32) -> Option<i64> {
    let secs = i64::from(hour)
        .checked_mul(60)?
        .checked_add(i64::from(min))?
        .checked_mul(60)?
        .checked_add(i64::from(sec))?;
    secs.checked_mul(USECS_PER_SEC)?.checked_add(i64::from(fsec))
}

/// µs contribution of every field of an `Itm`, with overflow reported as `None`.
fn itm_micros(itm: &Itm) -> Option<i64> {
    let hours = itm.hour.checked_mul(USECS_PER_HOUR)?;
    let mins = i64::from(itm.min).checked_mul(USECS_PER_MINUTE)?;
    let secs = i64::from(itm.sec).checked_mul(USECS_PER_SEC)?;
    hours.checked_add(mins)?.checked_add(secs)?.checked_add(i64::from(itm.usec))
}

// ---------------------------------------------------------------------------------------------
// Public conversions.
// ---------------------------------------------------------------------------------------------

/// Broken-down fields → timestamp µs since 2000-01-01, interpreting the fields as local time at
/// `tz_secs_west` (None = UTC / timestamp without tz). PG `tm2timestamp`.
// CONTRACT
pub fn tm2timestamp(tm: &Tm, tz_secs_west: Option<i32>) -> Result<i64> {
    if !valid_ymd(tm.year, tm.mon, tm.mday) {
        return Err(oor_timestamp());
    }
    let date = jd64(i64::from(tm.year), i64::from(tm.mon), i64::from(tm.mday)) - i64::from(POSTGRES_EPOCH_JDATE);
    let time = time2t(tm.hour, tm.min, tm.sec, tm.fsec).ok_or_else(oor_timestamp)?;
    let mut result = date
        .checked_mul(USECS_PER_DAY)
        .and_then(|d| d.checked_add(time))
        .ok_or_else(oor_timestamp)?;
    if let Some(west) = tz_secs_west {
        // local = utc - west, so utc = local + west.
        let shift = i64::from(west).checked_mul(USECS_PER_SEC).ok_or_else(oor_timestamp)?;
        result = result.checked_add(shift).ok_or_else(oor_timestamp)?;
    }
    if !(MIN_TIMESTAMP..END_TIMESTAMP).contains(&result) {
        return Err(oor_timestamp());
    }
    Ok(result)
}

/// Timestamp → broken-down fields, shifted by `-tz_secs_west` when given (PG `timestamp2tm`).
/// Fills `wday`, `yday`, `tz`.
// CONTRACT
pub fn timestamp2tm(ts: i64, tz_secs_west: Option<i32>) -> Result<Tm> {
    // The infinities have no calendar fields; callers must handle them before decomposing.
    if ts == i64::MIN || ts == i64::MAX {
        return Err(oor_timestamp());
    }
    let local = match tz_secs_west {
        Some(west) => {
            // local = utc - west.
            let shift = i64::from(west).checked_mul(USECS_PER_SEC).ok_or_else(oor_timestamp)?;
            ts.checked_sub(shift).ok_or_else(oor_timestamp)?
        }
        None => ts,
    };
    let date = local.div_euclid(USECS_PER_DAY);
    let time = local.rem_euclid(USECS_PER_DAY);
    // Julian day number (PG date0); the Julian routines need 0 <= jd <= i32::MAX.
    let jd = date.checked_add(i64::from(POSTGRES_EPOCH_JDATE)).ok_or_else(oor_timestamp)?;
    if jd < 0 || jd > i64::from(i32::MAX) {
        return Err(oor_timestamp());
    }
    let jd = jd as i32;
    let (year, mon, mday) = j2date(jd);
    let (hour, min, sec, fsec) = split_time(time);
    let yday = i64::from(jd) - jd64(i64::from(year), 1, 1) + 1;
    Ok(Tm {
        year,
        mon,
        mday,
        hour,
        min,
        sec,
        fsec,
        wday: j2day(jd),
        yday: yday as i32,
        tz: tz_secs_west,
        tz_abbrev: None,
    })
}

/// µs since midnight → (hour, min, sec, fsec) (PG `time2tm`).
// CONTRACT
pub fn time2tm(t: i64) -> Result<Tm> {
    if !(0..=USECS_PER_DAY).contains(&t) {
        return Err(out_of_range("time"));
    }
    let (hour, min, sec, fsec) = split_time(t);
    Ok(Tm { hour, min, sec, fsec, ..Tm::default() })
}

/// (hour, min, sec, fsec) → µs since midnight (PG `tm2time`).
// CONTRACT
pub fn tm2time(tm: &Tm) -> Result<i64> {
    // PG `time_overflows`: hour may be 24 (and then nothing else), sec may be 60 (leap second).
    let fields_ok = (0..=24).contains(&tm.hour)
        && (0..60).contains(&tm.min)
        && (0..=60).contains(&tm.sec)
        && (0..=1_000_000).contains(&tm.fsec);
    if !fields_ok {
        return Err(out_of_range("time"));
    }
    let t = time2t(tm.hour, tm.min, tm.sec, tm.fsec).ok_or_else(|| out_of_range("time"))?;
    if t > USECS_PER_DAY {
        return Err(out_of_range("time"));
    }
    Ok(t)
}

/// Interval → broken-down interval (PG `interval2itm`).
// CONTRACT
pub fn interval2itm(iv: &Interval) -> Result<Itm> {
    // Infinite intervals have no finite field decomposition; callers must test them first.
    if iv.is_infinite() {
        return Err(out_of_range("interval"));
    }
    let mut time = iv.micros;
    let hour = time / USECS_PER_HOUR;
    time -= hour * USECS_PER_HOUR;
    let min = time / USECS_PER_MINUTE;
    time -= min * USECS_PER_MINUTE;
    let sec = time / USECS_PER_SEC;
    time -= sec * USECS_PER_SEC;
    Ok(Itm {
        year: iv.months / MONTHS_PER_YEAR,
        mon: iv.months % MONTHS_PER_YEAR,
        mday: iv.days,
        hour,
        min: min as i32,
        sec: sec as i32,
        usec: time as i32,
    })
}

/// Broken-down interval → interval; overflow → 22008 `interval out of range` (PG `itm2interval`).
// CONTRACT
pub fn itm2interval(itm: &Itm) -> Result<Interval> {
    let overflow = || out_of_range("interval");
    let total_months = i64::from(itm.year)
        .checked_mul(i64::from(MONTHS_PER_YEAR))
        .and_then(|y| y.checked_add(i64::from(itm.mon)));
    let months = total_months.and_then(|m| i32::try_from(m).ok());
    let micros = itm_micros(itm);
    match (months, micros) {
        (Some(months), Some(micros)) => {
            let span = Interval::new(months, itm.mday, micros);
            // Infinite results are treated as overflow, as in PG (INTERVAL_NOT_FINITE).
            if span.is_infinite() {
                Err(overflow())
            } else {
                Ok(span)
            }
        }
        _ => Err(overflow()),
    }
}

/// ISO 8601 week number of a date (PG `date2isoweek`).
// CONTRACT
pub fn date2isoweek(year: i32, mon: i32, mday: i32) -> Result<i32> {
    if !valid_ymd(year, mon, mday) {
        return Err(out_of_range("date"));
    }
    let dayn = jd64(i64::from(year), i64::from(mon), i64::from(mday));
    let (_, week) = iso_year_and_week(i64::from(year), dayn);
    i32::try_from(week).map_err(|_| out_of_range("date"))
}

/// ISO 8601 week-numbering year of a date (PG `date2isoyear`).
// CONTRACT
pub fn date2isoyear(year: i32, mon: i32, mday: i32) -> Result<i32> {
    if !valid_ymd(year, mon, mday) {
        return Err(out_of_range("date"));
    }
    let dayn = jd64(i64::from(year), i64::from(mon), i64::from(mday));
    let (iso_year, _) = iso_year_and_week(i64::from(year), dayn);
    i32::try_from(iso_year).map_err(|_| out_of_range("date"))
}

/// Day of the ISO year, 1..=371 (PG `date2isoyearday`).
// CONTRACT
pub fn date2isoyearday(year: i32, mon: i32, mday: i32) -> Result<i32> {
    if !valid_ymd(year, mon, mday) {
        return Err(out_of_range("date"));
    }
    let dayn = jd64(i64::from(year), i64::from(mon), i64::from(mday));
    let (iso_year, _) = iso_year_and_week(i64::from(year), dayn);
    let doy = dayn - iso_week1_monday(iso_year) + 1;
    i32::try_from(doy).map_err(|_| out_of_range("date"))
}

/// Julian day of Monday of ISO week `week` of ISO year `year` (PG `isoweek2j`).
// CONTRACT
pub fn isoweek2j(year: i32, week: i32) -> Result<i32> {
    let jd = iso_week1_monday(i64::from(year)) + (i64::from(week) - 1) * 7;
    if jd < i64::from(DATETIME_MIN_JULIAN) || jd >= i64::from(DATE_END_JULIAN) {
        return Err(out_of_range("date"));
    }
    Ok(jd as i32)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::datetime::{MIN_TIMESTAMP, POSTGRES_EPOCH_JDATE};
    #[test]
    fn julian_roundtrip() {
        assert_eq!(date2j(2000, 1, 1), crate::datetime::POSTGRES_EPOCH_JDATE);
        assert_eq!(date2j(1970, 1, 1), crate::datetime::UNIX_EPOCH_JDATE);
        assert_eq!(date2j(-4713, 11, 24), 0);
        assert_eq!(j2date(2451545), (2000, 1, 1));
        assert_eq!(j2date(date2j(2024, 2, 29)), (2024, 2, 29));
        assert_eq!(j2day(date2j(2000, 1, 1)), 6); // Saturday
    }

    fn ymd(year: i32, mon: i32, mday: i32) -> Tm {
        Tm { year, mon, mday, ..Tm::default() }
    }

    #[test]
    fn tm2timestamp_epoch_and_range() {
        let mut tm = ymd(2000, 1, 1);
        assert_eq!(tm2timestamp(&tm, None), Ok(0));
        // 294277-01-01 is the first invalid timestamp.
        tm = ymd(294277, 1, 1);
        let err = tm2timestamp(&tm, None).unwrap_err();
        assert_eq!(err.sqlstate.code(), "22008");
        // 4714-11-24 BC is the minimum timestamp.
        assert_eq!(tm2timestamp(&ymd(-4713, 11, 24), None), Ok(MIN_TIMESTAMP));
        assert!(tm2timestamp(&ymd(-4713, 11, 23), None).is_err());
        assert!(tm2timestamp(&ymd(2000, 2, 30), None).is_err());
    }

    #[test]
    fn timestamp2tm_epoch_fields() {
        let tm = timestamp2tm(0, None).unwrap();
        assert_eq!((tm.year, tm.mon, tm.mday), (2000, 1, 1));
        assert_eq!((tm.hour, tm.min, tm.sec, tm.fsec), (0, 0, 0, 0));
        assert_eq!(tm.wday, 6);
        assert_eq!(tm.yday, 1);
        assert_eq!(tm.tz, None);
        // Before the epoch: 1999-12-31 23:59:59.999999.
        let tm = timestamp2tm(-1, None).unwrap();
        assert_eq!((tm.year, tm.mon, tm.mday), (1999, 12, 31));
        assert_eq!((tm.hour, tm.min, tm.sec, tm.fsec), (23, 59, 59, 999_999));
        assert_eq!(tm.yday, 365);
    }

    #[test]
    fn timestamp_tz_roundtrip() {
        // +05:00 is stored as west = -18000.
        let tm = timestamp2tm(0, Some(-18000)).unwrap();
        assert_eq!((tm.year, tm.mon, tm.mday, tm.hour), (2000, 1, 1, 5));
        assert_eq!(tm.tz, Some(-18000));
        assert_eq!(tm2timestamp(&tm, Some(-18000)), Ok(0));
        // Infinities have no fields.
        assert!(timestamp2tm(i64::MAX, None).is_err());
        assert!(timestamp2tm(i64::MIN, None).is_err());
    }

    #[test]
    fn time_conversions() {
        assert_eq!(time2tm(USECS_PER_DAY).unwrap().hour, 24);
        let tm = time2tm(3_723_500_000).unwrap();
        assert_eq!((tm.hour, tm.min, tm.sec, tm.fsec), (1, 2, 3, 500_000));
        assert!(time2tm(-1).is_err());
        let t = Tm { hour: 24, ..Tm::default() };
        assert_eq!(tm2time(&t), Ok(USECS_PER_DAY));
        let bad = Tm { hour: 24, min: 0, sec: 1, ..Tm::default() };
        assert!(tm2time(&bad).is_err());
        assert_eq!(tm2time(&Tm { hour: 1, min: 2, sec: 3, fsec: 500_000, ..Tm::default() }), Ok(3_723_500_000));
    }

    #[test]
    fn interval_decomposition() {
        // 1 year 2 mons 3 days 04:05:06.7
        let iv = Interval::new(14, 3, 14_706_700_000);
        let itm = interval2itm(&iv).unwrap();
        assert_eq!(
            (itm.year, itm.mon, itm.mday, itm.hour, itm.min, itm.sec, itm.usec),
            (1, 2, 3, 4, 5, 6, 700_000)
        );
        assert_eq!(itm2interval(&itm), Ok(iv));
        assert!(interval2itm(&Interval::INFINITY).is_err());
        let huge = Itm { year: i32::MAX, mon: 11, ..Itm::default() };
        assert!(itm2interval(&huge).is_err());
        let hours = Itm { hour: i64::MAX, ..Itm::default() };
        assert!(itm2interval(&hours).is_err());
    }

    #[test]
    fn iso_week_helpers() {
        assert_eq!(date2isoweek(2024, 12, 30), Ok(1));
        assert_eq!(date2isoyear(2024, 12, 30), Ok(2025));
        assert_eq!(date2isoyearday(2024, 12, 30), Ok(1));
        assert_eq!(date2isoweek(2021, 1, 1), Ok(53));
        assert_eq!(date2isoyear(2021, 1, 1), Ok(2020));
        assert_eq!(date2isoweek(2024, 1, 1), Ok(1));
        assert_eq!(date2isoyear(2024, 1, 1), Ok(2024));
        assert_eq!(date2isoyearday(2024, 1, 1), Ok(1));
        assert_eq!(date2isoweek(2000, 1, 1), Ok(52));
        assert_eq!(date2isoyear(2000, 1, 1), Ok(1999));
        assert!(date2isoweek(2023, 2, 30).is_err());
        assert_eq!(isoweek2j(2025, 1).map(j2date), Ok((2024, 12, 30)));
        assert_eq!(date2isoyearday(2024, 12, 31), Ok(2));
    }

    #[test]
    fn isoweek_julian_matches_epoch() {
        // 2000-01-01 is a Saturday in ISO week 52 of 1999, so ISO week 1 of 2000 starts Monday 2000-01-03.
        let jd = isoweek2j(2000, 1).unwrap();
        assert_eq!(j2date(jd), (2000, 1, 3));
        assert_eq!(jd, POSTGRES_EPOCH_JDATE + 2);
    }
}
