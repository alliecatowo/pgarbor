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
use super::{Itm, Tm};
use crate::context::{DateOrder, DateStyle, IntervalStyle};
use pgarbor_common::{PgError, Result};

/// Format a date (no time part).
// CONTRACT
#[allow(unused_variables)]
pub fn encode_date_only(tm: &Tm, style: DateStyle, order: DateOrder) -> Result<String> {
    Err(PgError::unimplemented("TYP-24", "date output"))
}

/// Format a time; `tz_secs_west` = Some(zone) for timetz (printed with ISO sign: west 28800 → `-08`).
// CONTRACT
#[allow(unused_variables)]
pub fn encode_time_only(tm: &Tm, tz_secs_west: Option<i32>, style: DateStyle) -> Result<String> {
    Err(PgError::unimplemented("TYP-24", "time output"))
}

/// Format a timestamp; `tz_secs_west` / `tz_abbrev` present for timestamptz.
// CONTRACT
#[allow(unused_variables)]
pub fn encode_date_time(
    tm: &Tm,
    tz_secs_west: Option<i32>,
    tz_abbrev: Option<&str>,
    style: DateStyle,
    order: DateOrder,
) -> Result<String> {
    Err(PgError::unimplemented("TYP-24", "timestamp output"))
}

/// Format an interval in the given IntervalStyle.
// CONTRACT
#[allow(unused_variables)]
pub fn encode_interval(itm: &Itm, style: IntervalStyle) -> Result<String> {
    Err(PgError::unimplemented("TYP-24", "interval output"))
}

/// Zone offset text as PG prints it in ISO style: east-positive `+hh[:mm[:ss]]`
/// (`tz_secs_west = -19800` → `+05:30`, `28800` → `-08`).
// CONTRACT
#[allow(unused_variables)]
pub fn encode_timezone(tz_secs_west: i32) -> Result<String> {
    Err(PgError::unimplemented("TYP-24", "time zone offset output"))
}
