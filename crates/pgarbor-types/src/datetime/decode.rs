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
use super::lexer::Field;
use super::{DateTimeKind, DtErr, Tm};
use crate::context::IoCtx;
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

/// PG `DecodeDateTime`: fields of a date / timestamp / timestamptz literal.
/// Outer `Err` = engine error; inner `Err` = input error (see `dt_parse_error`).
// CONTRACT
#[allow(unused_variables)]
pub fn decode_date_time(fields: &[Field], ctx: &IoCtx) -> Result<std::result::Result<Decoded, DtErr>> {
    Err(PgError::unimplemented("TYP-23", "datetime decoding"))
}

/// PG `DecodeTimeOnly`: fields of a time / timetz literal (a date part is allowed and used only
/// to resolve the zone offset).
// CONTRACT
#[allow(unused_variables)]
pub fn decode_time_only(fields: &[Field], ctx: &IoCtx) -> Result<std::result::Result<Decoded, DtErr>> {
    Err(PgError::unimplemented("TYP-23", "time decoding"))
}

/// Convenience: lex + decode a full date/timestamp string, mapping failures to PG errors with
/// `type_name` (e.g. "timestamp with time zone").
// CONTRACT
#[allow(unused_variables)]
pub fn parse_date_time(input: &str, ctx: &IoCtx, type_name: &str) -> Result<Decoded> {
    Err(PgError::unimplemented("TYP-23", "datetime decoding"))
}

/// Convenience: lex + decode a time/timetz string.
// CONTRACT
#[allow(unused_variables)]
pub fn parse_time_only(input: &str, ctx: &IoCtx, type_name: &str) -> Result<Decoded> {
    Err(PgError::unimplemented("TYP-23", "time decoding"))
}
