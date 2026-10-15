//! Time zones: the `TimeZone` setting, zone names/abbreviations in input and `AT TIME ZONE`,
//! offsets for timestamptz I/O (TYPE-06). Lease TYP-25.
//!
//! Scope (v1): UTC-family names, fixed offsets, POSIX-style `UTC+3`, and the PG "Default" abbreviation
//! set (`src/timezone/tznames/Default`: e.g. `pst` -08 / `pdt` -07 (dst) / `est` -05 / `edt` -04 /
//! `cst` -06 / `cdt` -05 / `mst` -07 / `mdt` -06 / `cet` +01 / `cest` +02 / `gmt` / `utc` / `z` / `zulu`
//! / `jst` +09 / `ist` +02 (PG Default maps IST to Israel) ...; copy the whole Default list).
//! IANA region names (`America/New_York`, `Europe/Paris`) need tz rules PGArbor does not ship:
//! recognize them syntactically (contains `/` or is a known region-less IANA name like `Japan`)
//! and fail with 0A000 (`DtErr::UnsupportedTimezone` / `PgError::unimplemented("TYP-25", ..)`);
//! anything else unknown → 22023 `time zone "x" not recognized` (name lower-cased, oracle:
//! `'2020-01-01 00:00 Mars/Base'::timestamptz` → `time zone "mars/base" not recognized` — so a name
//! with `/` that is not a real IANA zone must still produce 22023: keep a list of IANA zone names
//! (from tzdata `zone1970.tab` / `backward`) to decide).
//!
//! `TimeZone` GUC parsing (`parse_timezone_setting`), oracle-verified semantics:
//! - `UTC`, `GMT`, `Etc/UTC`, `Z`, `zulu` (any case) → `TimeZoneSetting::Utc` (SHOW prints `UTC` / as given).
//! - POSIX offsets: `'+05:30'` means 5h30 WEST (POSIX sign!): SHOW `+05:30`, output `...-05:30`.
//!   `'UTC+3'` → 3h west (output `-03`). Number-only `'-8'`/`'8'`: hours, POSIX sign.
//! - `SET TIME ZONE INTERVAL '-08:00' HOUR TO MINUTE` / `SET TIME ZONE -8` (numeric literal) use ISO
//!   sign: 8h WEST, SHOW prints `<-08>+08` (the binder converts these forms before calling
//!   `fixed_offset_setting(secs_east)`).
//! - `LOCAL` / `DEFAULT` → the configured default (UTC).
use super::DtErr;
use crate::context::TimeZoneSetting;
use pgarbor_common::{PgError, Result};

/// A resolved zone for input / AT TIME ZONE.
// CONTRACT
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ZoneSpec {
    /// Fixed offset in seconds EAST of UTC (abbreviations, numeric offsets, UTC).
    Fixed { secs_east: i32, abbrev: Option<String> },
    /// IANA region name (unsupported in v1: conversions return 0A000).
    Region(String),
}

/// Parse a `SET TIME ZONE` / `TimeZone` GUC string (see module docs).
// CONTRACT
#[allow(unused_variables)]
pub fn parse_timezone_setting(value: &str) -> Result<TimeZoneSetting> {
    Err(PgError::unimplemented("TYP-25", "TimeZone setting"))
}

/// Setting for an ISO-signed fixed offset (`SET TIME ZONE INTERVAL '-08:00' HOUR TO MINUTE`):
/// name in PG's POSIX-bracket form (`-28800` → `<-08>+08`, `19800` → `<+0530>-0530`).
// CONTRACT
#[allow(unused_variables)]
pub fn fixed_offset_setting(secs_east: i32) -> Result<TimeZoneSetting> {
    Err(PgError::unimplemented("TYP-25", "fixed TimeZone setting"))
}

/// Resolve a zone word appearing in datetime input or `AT TIME ZONE` (already lower-cased).
/// Inner `Err`: `DtErr::BadTimezone(name)` / `DtErr::UnsupportedTimezone(name)`.
// CONTRACT
#[allow(unused_variables)]
pub fn lookup_zone(name: &str) -> Result<std::result::Result<ZoneSpec, DtErr>> {
    Err(PgError::unimplemented("TYP-25", "time zone names"))
}

/// Parse a numeric zone `+hh`, `+hhmm`, `+hh:mm`, `+hh:mm:ss`, `-h` (ISO sign) → seconds EAST.
/// Inner `Err(DtErr::TzDispOverflow)` when |offset| >= `TZDISP_LIMIT`, `BadFormat` when malformed.
// CONTRACT
#[allow(unused_variables)]
pub fn parse_numeric_offset(s: &str) -> Result<std::result::Result<i32, DtErr>> {
    Err(PgError::unimplemented("TYP-25", "numeric time zone offsets"))
}

/// UTC offset (seconds EAST) of the session zone at UTC instant `utc_ts` (timestamptz output).
// CONTRACT
#[allow(unused_variables)]
pub fn offset_at_utc(setting: &TimeZoneSetting, utc_ts: i64) -> Result<i32> {
    Err(PgError::unimplemented("TYP-25", "time zone offset"))
}

/// UTC offset (seconds EAST) of the session zone for LOCAL wall-clock time `local_ts` (input without
/// explicit zone; for DST gaps/overlaps follow PG `DetermineTimeZoneOffset`).
// CONTRACT
#[allow(unused_variables)]
pub fn offset_at_local(setting: &TimeZoneSetting, local_ts: i64) -> Result<i32> {
    Err(PgError::unimplemented("TYP-25", "time zone offset"))
}

/// Zone abbreviation printed by non-ISO DateStyles (`UTC` for UTC; numeric offsets for fixed zones,
/// e.g. `+0530`? Follow PG: abbreviation from the zone's tz rules, which for POSIX `<-08>+08` is `-08`).
// CONTRACT
#[allow(unused_variables)]
pub fn abbrev_at_utc(setting: &TimeZoneSetting, utc_ts: i64) -> Result<String> {
    Err(PgError::unimplemented("TYP-25", "time zone abbreviation"))
}

/// `SHOW timezone` text for a setting.
pub fn setting_display(setting: &TimeZoneSetting) -> String {
    match setting {
        TimeZoneSetting::Utc => "UTC".to_string(),
        TimeZoneSetting::Fixed { name, .. } => name.clone(),
        TimeZoneSetting::Named(n) => n.clone(),
    }
}
