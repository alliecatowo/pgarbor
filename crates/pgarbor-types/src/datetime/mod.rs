//! Date/time types (TYPE-06, TYPE-14): date, time, timetz, timestamp, timestamptz, interval.
//!
//! Internal representation (ARCHITECTURE §8, identical to PostgreSQL's integer datetimes):
//! - date: `i32` days since 2000-01-01 (`POSTGRES_EPOCH_JDATE`); `i32::MAX`/`i32::MIN` = ±infinity.
//!   Valid finite range: Julian day 0 (4714-11-24 BC) ..= `DATE_END_JULIAN - 1` (5874897-12-31).
//! - time: `i64` µs since midnight, `0..=USECS_PER_DAY` (24:00:00 is allowed).
//! - timetz: (µs, zone) with zone in seconds WEST of UTC (PG `TimeTzADT.zone`), |zone| <= `TZDISP_LIMIT`.
//! - timestamp/timestamptz: `i64` µs since 2000-01-01 00:00:00 (UTC for tz), finite range
//!   `MIN_TIMESTAMP ..< END_TIMESTAMP`, `i64::MIN`/`i64::MAX` = ±infinity.
//! - interval: `Interval { months, days, micros }`.
//!
//! Files:
//! - `calendar.rs`: Julian-day arithmetic and struct-tm conversions (PG `date2j`, `j2date`, `timestamp2tm`, ...).
//! - `lexer.rs`: `ParseDateTime` (split input into typed fields).
//! - `decode.rs`: `DecodeDateTime` / `DecodeTimeOnly` / `DecodeSpecial` keyword tables.
//! - `encode.rs`: `EncodeDateOnly` / `EncodeTimeOnly` / `EncodeDateTime` / `EncodeInterval` for all styles.
//! - `tz.rs`: TimeZone setting + zone names/abbreviations/offset parsing.
//! - `date.rs`, `time.rs`, `timestamp.rs`, `interval.rs`: per-type I/O + operators.
//! - `funcs.rs`: extract/date_part/date_trunc/age/make_*/now/AT TIME ZONE/date_bin/overlaps.

pub mod calendar;
pub mod date;
pub mod decode;
pub mod encode;
pub mod funcs;
pub mod interval;
pub mod lexer;
pub mod time;
pub mod timestamp;
pub mod tz;

/// Julian day of 2000-01-01 (`date2j(2000,1,1)`).
pub const POSTGRES_EPOCH_JDATE: i32 = 2451545;
/// Julian day of 1970-01-01.
pub const UNIX_EPOCH_JDATE: i32 = 2440588;
/// First valid Julian day (4714-11-24 BC).
pub const DATETIME_MIN_JULIAN: i32 = 0;
/// `date2j(5874898, 1, 1)`: first INVALID Julian day for dates.
pub const DATE_END_JULIAN: i32 = 2147483494;
/// `date2j(294277, 1, 1)`: first INVALID Julian day for timestamps.
pub const TIMESTAMP_END_JULIAN: i32 = 109203528;
/// Smallest valid timestamp (4714-11-24 00:00:00 BC).
pub const MIN_TIMESTAMP: i64 = -211813488000000000;
/// First invalid timestamp (294277-01-01 00:00:00).
pub const END_TIMESTAMP: i64 = 9223371331200000000;
/// Max fractional-second precision for time/timestamp/interval typmods.
pub const MAX_TIMESTAMP_PRECISION: i32 = 6;
pub const MAX_INTERVAL_PRECISION: i32 = 6;
/// Max |zone displacement| in seconds (15:59:59 → (15+1)*3600).
pub const TZDISP_LIMIT: i32 = 16 * 3600;

pub const USECS_PER_SEC: i64 = 1_000_000;
pub const USECS_PER_MINUTE: i64 = 60_000_000;
pub const USECS_PER_HOUR: i64 = 3_600_000_000;
pub const USECS_PER_DAY: i64 = 86_400_000_000;
pub const SECS_PER_MINUTE: i32 = 60;
pub const SECS_PER_HOUR: i32 = 3600;
pub const SECS_PER_DAY: i32 = 86400;
pub const DAYS_PER_MONTH: i32 = 30;
pub const MONTHS_PER_YEAR: i32 = 12;
pub const HOURS_PER_DAY: i32 = 24;

/// Interval typmod range bits (PG `INTERVAL_MASK(b)` values, see `utils/datetime.h`).
pub mod interval_fields {
    pub const MONTH: i32 = 1 << 1;
    pub const YEAR: i32 = 1 << 2;
    pub const DAY: i32 = 1 << 3;
    pub const HOUR: i32 = 1 << 10;
    pub const MINUTE: i32 = 1 << 11;
    pub const SECOND: i32 = 1 << 12;
    /// `INTERVAL_FULL_RANGE`.
    pub const FULL_RANGE: i32 = 0x7FFF;
    /// `INTERVAL_FULL_PRECISION`.
    pub const FULL_PRECISION: i32 = 0xFFFF;
}

/// Broken-down date/time (PG `struct pg_tm` + fsec). `year` is astronomical in calculations
/// (1 BC = 0, 2 BC = -1); the encoders/decoders convert to/from the "BC" display form.
// CONTRACT
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Tm {
    pub year: i32,
    /// 1..=12
    pub mon: i32,
    /// 1..=31
    pub mday: i32,
    pub hour: i32,
    pub min: i32,
    pub sec: i32,
    /// Fractional second in µs, 0..=999_999.
    pub fsec: i32,
    /// Day of week 0 = Sunday (filled by conversions that compute it).
    pub wday: i32,
    /// Day of year 1..=366 (filled by conversions that compute it).
    pub yday: i32,
    /// Zone in seconds WEST of UTC (PG `tm_gmtoff` negated), if known.
    pub tz: Option<i32>,
    /// Zone abbreviation for output (e.g. "UTC", "+05"), if known.
    pub tz_abbrev: Option<&'static str>,
}

/// Broken-down interval (PG `struct pg_itm`).
// CONTRACT
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Itm {
    pub year: i32,
    pub mon: i32,
    pub mday: i32,
    pub hour: i64,
    pub min: i32,
    pub sec: i32,
    pub usec: i32,
}

/// Result class of decoding a datetime string (PG `DTK_*` for special values).
// CONTRACT
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DateTimeKind {
    /// Ordinary date/time in the `Tm`.
    Date,
    /// 'epoch' (1970-01-01 00:00:00 UTC).
    Epoch,
    /// 'infinity' / '+infinity'.
    Late,
    /// '-infinity'.
    Early,
    /// 'now' (transaction timestamp). Note: 'today', 'tomorrow', 'yesterday' and 'allballs'
    /// are resolved by the decoder into `Date` (using the transaction timestamp for the first three).
    Now,
}

/// Datetime decoding failure classes (PG `DTERR_*`). Converted to a `PgError` with `dt_parse_error`.
// CONTRACT
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DtErr {
    /// `DTERR_BAD_FORMAT` → 22007 `invalid input syntax for type <type>: "<input>"`.
    BadFormat,
    /// `DTERR_FIELD_OVERFLOW` → 22008 `date/time field value out of range: "<input>"`.
    FieldOverflow,
    /// `DTERR_MD_FIELD_OVERFLOW` → as FieldOverflow + HINT `Perhaps you need a different "DateStyle" setting.`
    MdFieldOverflow,
    /// `DTERR_INTERVAL_OVERFLOW` → 22015 `interval field value out of range: "<input>"`.
    IntervalOverflow,
    /// `DTERR_TZDISP_OVERFLOW` → 22009 `time zone displacement out of range: "<input>"`.
    TzDispOverflow,
    /// `DTERR_BAD_TIMEZONE` → 22023 `time zone "<name>" not recognized` (name lower-cased as PG prints it).
    BadTimezone(String),
    /// A recognized but unsupported zone (IANA rules not shipped): → 0A000 (PGArbor-specific).
    UnsupportedTimezone(String),
}

/// Build the PostgreSQL error for a datetime decoding failure (PG `DateTimeParseError`).
/// `type_name` is the SQL display name used in messages: "date", "time", "time with time zone",
/// "timestamp", "timestamp with time zone", "interval".
pub fn dt_parse_error(err: &DtErr, input: &str, type_name: &str) -> pgarbor_common::PgError {
    use pgarbor_common::{PgError, SqlState};
    match err {
        DtErr::BadFormat => PgError::new(
            SqlState::INVALID_DATETIME_FORMAT,
            format!("invalid input syntax for type {type_name}: \"{input}\""),
        ),
        DtErr::FieldOverflow => PgError::new(
            SqlState::DATETIME_FIELD_OVERFLOW,
            format!("date/time field value out of range: \"{input}\""),
        ),
        DtErr::MdFieldOverflow => PgError::new(
            SqlState::DATETIME_FIELD_OVERFLOW,
            format!("date/time field value out of range: \"{input}\""),
        )
        .with_hint("Perhaps you need a different \"DateStyle\" setting."),
        DtErr::IntervalOverflow => PgError::new(
            SqlState::INTERVAL_FIELD_OVERFLOW,
            format!("interval field value out of range: \"{input}\""),
        ),
        DtErr::TzDispOverflow => PgError::new(
            SqlState::INVALID_TIME_ZONE_DISPLACEMENT_VALUE,
            format!("time zone displacement out of range: \"{input}\""),
        ),
        DtErr::BadTimezone(name) => {
            PgError::new(SqlState::INVALID_PARAMETER_VALUE, format!("time zone \"{name}\" not recognized"))
        }
        DtErr::UnsupportedTimezone(name) => PgError::new(
            SqlState::FEATURE_NOT_SUPPORTED,
            format!("time zone \"{name}\" is not supported by PGArbor yet"),
        )
        .with_detail("pgarbor-unimplemented: TYP-25"),
    }
}

/// Generic 22008 `<what> out of range` (e.g. "timestamp out of range", "date out of range").
pub fn out_of_range(what: &str) -> pgarbor_common::PgError {
    pgarbor_common::PgError::new(pgarbor_common::SqlState::DATETIME_VALUE_OUT_OF_RANGE, format!("{what} out of range"))
}
