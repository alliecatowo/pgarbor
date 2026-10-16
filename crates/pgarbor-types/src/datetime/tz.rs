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
use super::{DtErr, TZDISP_LIMIT};
use crate::context::TimeZoneSetting;
use pgarbor_common::{PgError, Result, SqlState};
use std::cmp::Ordering;

type DtResult<T> = std::result::Result<T, DtErr>;
/// (abbreviation, seconds east of UTC, daylight flag, zone reference or "").
type AbbrevDef = (&'static str, i32, bool, &'static str);

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
pub fn parse_timezone_setting(value: &str) -> Result<TimeZoneSetting> {
    if is_zero_offset_word(value) {
        return Ok(TimeZoneSetting::Utc);
    }
    if value.len() >= 8 && value.as_bytes()[..8].eq_ignore_ascii_case(b"interval") {
        return interval_setting(value);
    }
    // PG check_timezone: a bare number is hours with ISO sign (`-8` = 8 hours west).
    if let Some(hours) = strtod_hours(value) {
        return fixed_setting_checked((hours * 3600.0) as i64, value);
    }
    // POSIX TZ string (`+05:30` = 5h30 west, `UTC+3` = 3h west); SHOW keeps the text as given.
    if let Some(east) = posix_east_seconds(value) {
        if east.abs() >= i64::from(TZDISP_LIMIT) {
            return Err(tz_range_error(value));
        }
        return Ok(TimeZoneSetting::Fixed { name: value.to_string(), secs_east: east as i32 });
    }
    match find_iana(value) {
        Some(canon) => match etc_gmt_offset(canon) {
            Some(secs_east) => Ok(TimeZoneSetting::Fixed { name: canon.to_string(), secs_east }),
            None => Ok(TimeZoneSetting::Named(canon.to_string())),
        },
        None => Err(tz_invalid(value)),
    }
}

/// Setting for an ISO-signed fixed offset (`SET TIME ZONE INTERVAL '-08:00' HOUR TO MINUTE`):
/// name in PG's POSIX-bracket form (`-28800` → `<-08>+08`, `19800` → `<+0530>-0530`).
// CONTRACT
pub fn fixed_offset_setting(secs_east: i32) -> Result<TimeZoneSetting> {
    fixed_setting_checked(i64::from(secs_east), &secs_east.to_string())
}

/// Resolve a zone word appearing in datetime input or `AT TIME ZONE` (already lower-cased).
/// Inner `Err`: `DtErr::BadTimezone(name)` / `DtErr::UnsupportedTimezone(name)`.
// CONTRACT
pub fn lookup_zone(name: &str) -> Result<std::result::Result<ZoneSpec, DtErr>> {
    if let Some((abbrev, secs_east, _dst, zone)) = find_abbrev(name) {
        if zone.is_empty() {
            return Ok(Ok(ZoneSpec::Fixed { secs_east, abbrev: Some(abbrev.to_string()) }));
        }
        return Ok(Err(DtErr::UnsupportedTimezone(name.to_string())));
    }
    if is_zero_offset_word(name) {
        return Ok(Ok(ZoneSpec::Fixed { secs_east: 0, abbrev: Some("UTC".to_string()) }));
    }
    if let Some(canon) = find_iana(name) {
        return Ok(match etc_gmt_offset(canon) {
            Some(secs_east) => Ok(ZoneSpec::Fixed { secs_east, abbrev: Some(abbrev_text(i64::from(secs_east))) }),
            None => Err(DtErr::UnsupportedTimezone(name.to_string())),
        });
    }
    Ok(Err(DtErr::BadTimezone(name.to_string())))
}

/// Parse a numeric zone `+hh`, `+hhmm`, `+hh:mm`, `+hh:mm:ss`, `-h` (ISO sign) → seconds EAST.
/// Inner `Err(DtErr::TzDispOverflow)` when |offset| >= `TZDISP_LIMIT`, `BadFormat` when malformed.
// CONTRACT
pub fn parse_numeric_offset(s: &str) -> Result<std::result::Result<i32, DtErr>> {
    Ok(numeric_offset(s))
}

/// UTC offset (seconds EAST) of the session zone at UTC instant `utc_ts` (timestamptz output).
// CONTRACT
pub fn offset_at_utc(setting: &TimeZoneSetting, utc_ts: i64) -> Result<i32> {
    let _ = utc_ts;
    fixed_or_utc_offset(setting)
}

/// UTC offset (seconds EAST) of the session zone for LOCAL wall-clock time `local_ts` (input without
/// explicit zone; for DST gaps/overlaps follow PG `DetermineTimeZoneOffset`).
// CONTRACT
pub fn offset_at_local(setting: &TimeZoneSetting, local_ts: i64) -> Result<i32> {
    let _ = local_ts;
    fixed_or_utc_offset(setting)
}

/// Zone abbreviation printed by non-ISO DateStyles (`UTC` for UTC; numeric offsets for fixed zones,
/// e.g. `+0530`? Follow PG: abbreviation from the zone's tz rules, which for POSIX `<-08>+08` is `-08`).
// CONTRACT
pub fn abbrev_at_utc(setting: &TimeZoneSetting, utc_ts: i64) -> Result<String> {
    let _ = utc_ts;
    match setting {
        TimeZoneSetting::Utc => Ok("UTC".to_string()),
        TimeZoneSetting::Fixed { secs_east, .. } => Ok(abbrev_text(i64::from(*secs_east))),
        TimeZoneSetting::Named(n) => Err(named_unsupported(n)),
    }
}

/// `SHOW timezone` text for a setting.
pub fn setting_display(setting: &TimeZoneSetting) -> String {
    match setting {
        TimeZoneSetting::Utc => "UTC".to_string(),
        TimeZoneSetting::Fixed { name, .. } => name.clone(),
        TimeZoneSetting::Named(n) => n.clone(),
    }
}

// ---------------------------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------------------------

/// Names PG resolves to a zero offset (abbreviations and zero-offset links), compared ASCII case-insensitively.
const ZERO_OFFSET_NAMES: &[&str] = &[
    "utc", "gmt", "z", "zulu", "uct", "universal", "greenwich", "gmt0", "gmt+0", "gmt-0", "etc/utc", "etc/gmt",
    "etc/uct", "etc/zulu", "etc/universal", "etc/greenwich", "etc/gmt0", "etc/gmt+0", "etc/gmt-0",
];

fn is_zero_offset_word(name: &str) -> bool {
    ZERO_OFFSET_NAMES.iter().any(|w| w.eq_ignore_ascii_case(name))
}

/// ASCII case-insensitive ordering; both tables are sorted by this order.
fn fold_cmp(a: &str, b: &str) -> Ordering {
    a.bytes().map(|c| c.to_ascii_lowercase()).cmp(b.bytes().map(|c| c.to_ascii_lowercase()))
}

fn find_abbrev(name: &str) -> Option<AbbrevDef> {
    DEFAULT_ABBREVS.binary_search_by(|e| fold_cmp(e.0, name)).ok().map(|i| DEFAULT_ABBREVS[i])
}

fn find_iana(name: &str) -> Option<&'static str> {
    IANA_ZONES.binary_search_by(|z| fold_cmp(z, name)).ok().map(|i| IANA_ZONES[i])
}

/// `Etc/GMT+3` is UTC-3, so the sign is inverted. Returns seconds east for the `Etc/GMT±n` zones.
fn etc_gmt_offset(canon: &str) -> Option<i32> {
    let rest = canon.strip_prefix("Etc/GMT")?;
    let (sign, digits) = match rest.as_bytes().first()? {
        b'+' => (-1, &rest[1..]),
        b'-' => (1, &rest[1..]),
        _ => return None,
    };
    let hours = digits_in(digits.as_bytes(), 1, 2).ok()?;
    Some(sign * hours * 3600)
}

fn tz_invalid(value: &str) -> PgError {
    PgError::new(SqlState::INVALID_PARAMETER_VALUE, format!("invalid value for parameter \"TimeZone\": \"{value}\""))
}

fn tz_range_error(value: &str) -> PgError {
    tz_invalid(value).with_detail("UTC timezone offset is out of range.")
}

fn named_unsupported(name: &str) -> PgError {
    PgError::unimplemented("TYP-25", format!("time zone \"{name}\""))
}

fn fixed_or_utc_offset(setting: &TimeZoneSetting) -> Result<i32> {
    match setting {
        TimeZoneSetting::Utc => Ok(0),
        TimeZoneSetting::Fixed { secs_east, .. } => Ok(*secs_east),
        TimeZoneSetting::Named(n) => Err(named_unsupported(n)),
    }
}

fn fixed_setting_checked(secs_east: i64, value: &str) -> Result<TimeZoneSetting> {
    if secs_east.abs() >= i64::from(TZDISP_LIMIT) {
        return Err(tz_range_error(value));
    }
    let secs = secs_east as i32;
    Ok(TimeZoneSetting::Fixed { name: posix_bracket_name(i64::from(secs)), secs_east: secs })
}

/// `SET TIME ZONE INTERVAL '...'`-style values: the body is ISO-signed `[+-]hh[:mm[:ss]]`.
fn interval_setting(value: &str) -> Result<TimeZoneSetting> {
    let inner = value
        .get(8..)
        .unwrap_or("")
        .trim_start()
        .strip_prefix('\'')
        .and_then(|r| r.strip_suffix('\''))
        .ok_or_else(|| tz_invalid(value))?;
    if inner.bytes().any(|c| c.is_ascii_alphabetic()) {
        return Err(PgError::unimplemented("TYP-25", "INTERVAL-form TimeZone values with units"));
    }
    let east = match inner.as_bytes().first() {
        Some(b'-') => signed_hms(&inner[1..], true),
        Some(b'+') => signed_hms(&inner[1..], false),
        _ => signed_hms(inner, false),
    };
    match east {
        Ok(secs) => fixed_setting_checked(i64::from(secs), value),
        Err(DtErr::TzDispOverflow) => Err(tz_range_error(value)),
        Err(_) => Err(tz_invalid(value)),
    }
}

/// strtod-style decimal number (hours) for the numeric GUC form; `None` if the whole string is not one.
fn strtod_hours(value: &str) -> Option<f64> {
    if value.is_empty() || !value.bytes().all(|c| c.is_ascii_digit() || matches!(c, b'+' | b'-' | b'.' | b'e' | b'E')) {
        return None;
    }
    value.parse::<f64>().ok()
}

/// POSIX TZ offset text: optional alphabetic std name (3+ letters), optional sign, `hh[:mm[:ss]]`.
/// Returns seconds EAST (POSIX sign: `+` is west, no sign after a name is west too, `-` is east).
fn posix_east_seconds(value: &str) -> Option<i64> {
    let alpha = value.bytes().take_while(|c| c.is_ascii_alphabetic()).count();
    if alpha == 1 || alpha == 2 {
        return None;
    }
    let rest = &value[alpha..];
    let (neg, digits, signed) = match rest.as_bytes().first() {
        Some(b'+') => (false, &rest[1..], true),
        Some(b'-') => (true, &rest[1..], true),
        _ => (false, rest, false),
    };
    if alpha == 0 && !signed {
        return None;
    }
    let (h, m, s) = split_hms(digits.as_bytes()).ok()?;
    let total = i64::from(h) * 3600 + i64::from(m) * 60 + i64::from(s);
    Some(if neg { total } else { -total })
}

/// `hh`, `h`, `hhmm`, `hhmmss`, or `h[h]:mm[:ss]` (PG DecodeTimezone digit forms). Returns (h, m, s).
fn split_hms(b: &[u8]) -> DtResult<(i32, i32, i32)> {
    if let Some(c1) = b.iter().position(|&c| c == b':') {
        let h = digits_in(&b[..c1], 1, 2)?;
        let rest = &b[c1 + 1..];
        let (m, s) = match rest.iter().position(|&c| c == b':') {
            Some(c2) => (digits_in(&rest[..c2], 2, 2)?, digits_in(&rest[c2 + 1..], 2, 2)?),
            None => (digits_in(rest, 2, 2)?, 0),
        };
        return Ok((h, m, s));
    }
    match b.len() {
        1 | 2 => Ok((digits_in(b, 1, 2)?, 0, 0)),
        4 => Ok((digits_in(&b[..2], 2, 2)?, digits_in(&b[2..], 2, 2)?, 0)),
        6 => Ok((digits_in(&b[..2], 2, 2)?, digits_in(&b[2..4], 2, 2)?, digits_in(&b[4..], 2, 2)?)),
        _ => Err(DtErr::BadFormat),
    }
}

fn digits_in(b: &[u8], min: usize, max: usize) -> DtResult<i32> {
    if b.len() < min || b.len() > max || !b.iter().all(u8::is_ascii_digit) {
        return Err(DtErr::BadFormat);
    }
    Ok(b.iter().fold(0i32, |acc, &c| acc * 10 + i32::from(c - b'0')))
}

/// Datetime numeric zone `[+-]hh[:mm[:ss]]` (ISO sign) → seconds east.
fn numeric_offset(s: &str) -> DtResult<i32> {
    match s.as_bytes().first() {
        Some(b'+') => signed_hms(&s[1..], false),
        Some(b'-') => signed_hms(&s[1..], true),
        _ => Err(DtErr::BadFormat),
    }
}

/// Digits after the sign → seconds east, with PG's displacement limits (`TzDispOverflow`).
fn signed_hms(body: &str, neg: bool) -> DtResult<i32> {
    let (h, m, s) = split_hms(body.as_bytes())?;
    if m >= 60 || s >= 60 {
        return Err(DtErr::TzDispOverflow);
    }
    let total = h * 3600 + m * 60 + s;
    if total >= TZDISP_LIMIT {
        return Err(DtErr::TzDispOverflow);
    }
    Ok(if neg { -total } else { total })
}

/// `hh[:mm[:ss]]` for an absolute offset, as PG's pg_tzset_offset prints it (minutes/seconds only when nonzero).
fn hms_text(abs: i64) -> String {
    let mut s = format!("{:02}", abs / 3600);
    let rem = abs % 3600;
    if rem != 0 {
        s.push_str(&format!(":{:02}", rem / 60));
        if rem % 60 != 0 {
            s.push_str(&format!(":{:02}", rem % 60));
        }
    }
    s
}

fn east_sign(secs_east: i64) -> char {
    if secs_east < 0 {
        '-'
    } else {
        '+'
    }
}

/// PG's POSIX-bracket name for a fixed offset (`pg_tzset_offset`): `-28800` → `<-08>+08`, `19800` → `<+05:30>-05:30`.
fn posix_bracket_name(secs_east: i64) -> String {
    let sign = east_sign(secs_east);
    let other = if sign == '-' { '+' } else { '-' };
    let digits = hms_text(secs_east.abs());
    format!("<{sign}{digits}>{other}{digits}")
}

/// Abbreviation PG prints for a numeric zone: the bracket content, e.g. `-08`, `+05:30`.
fn abbrev_text(secs_east: i64) -> String {
    format!("{}{}", east_sign(secs_east), hms_text(secs_east.abs()))
}

/// PG 18.6 "Default" abbreviation set (`src/timezone/tznames/Default`), sorted case-insensitively.
/// Fields: abbreviation, seconds EAST of UTC, daylight flag, zone reference (non-empty = the abbreviation
/// maps through that IANA zone's history, which PGArbor cannot evaluate: 0A000).
const DEFAULT_ABBREVS: &[AbbrevDef] = &[
    ("ACDT", 37800, true, ""),
    ("ACSST", 37800, true, ""),
    ("ACST", 34200, false, ""),
    ("ACT", -18000, false, ""),
    ("ACWST", 31500, false, ""),
    ("ADT", -10800, true, ""),
    ("AEDT", 39600, true, ""),
    ("AESST", 39600, true, ""),
    ("AEST", 36000, false, ""),
    ("AFT", 16200, false, ""),
    ("AKDT", -28800, true, ""),
    ("AKST", -32400, false, ""),
    ("ALMST", 25200, true, ""),
    ("ALMT", 21600, false, ""),
    ("AMST", 0, false, "Asia/Yerevan"),
    ("AMT", -14400, false, ""),
    ("ANAST", 0, false, "Asia/Anadyr"),
    ("ANAT", 0, false, "Asia/Anadyr"),
    ("ARST", 0, false, "America/Argentina/Buenos_Aires"),
    ("ART", 0, false, "America/Argentina/Buenos_Aires"),
    ("AST", -14400, false, ""),
    ("AWSST", 32400, true, ""),
    ("AWST", 28800, false, ""),
    ("AZOST", 0, true, ""),
    ("AZOT", -3600, false, ""),
    ("AZST", 0, false, "Asia/Baku"),
    ("AZT", 0, false, "Asia/Baku"),
    ("BDST", 7200, true, ""),
    ("BDT", 21600, false, ""),
    ("BNT", 28800, false, ""),
    ("BORT", 28800, false, ""),
    ("BOT", -14400, false, ""),
    ("BRA", -10800, false, ""),
    ("BRST", -7200, true, ""),
    ("BRT", -10800, false, ""),
    ("BST", 3600, true, ""),
    ("BTT", 21600, false, ""),
    ("CADT", 37800, true, ""),
    ("CAST", 34200, false, ""),
    ("CCT", 28800, false, ""),
    ("CDT", -18000, true, ""),
    ("CEST", 7200, true, ""),
    ("CET", 3600, false, ""),
    ("CETDST", 7200, true, ""),
    ("CHADT", 49500, true, ""),
    ("CHAST", 45900, false, ""),
    ("CHUT", 36000, false, ""),
    ("CKT", 0, false, "Pacific/Rarotonga"),
    ("CLST", -10800, true, ""),
    ("CLT", 0, false, "America/Santiago"),
    ("COT", -18000, false, ""),
    ("CST", -21600, false, ""),
    ("CXT", 25200, false, ""),
    ("DAVT", 0, false, "Antarctica/Davis"),
    ("DDUT", 36000, false, ""),
    ("EASST", 0, false, "Pacific/Easter"),
    ("EAST", 0, false, "Pacific/Easter"),
    ("EAT", 10800, false, ""),
    ("EDT", -14400, true, ""),
    ("EEST", 10800, true, ""),
    ("EET", 7200, false, ""),
    ("EETDST", 10800, true, ""),
    ("EGST", 0, true, ""),
    ("EGT", -3600, false, ""),
    ("EST", -18000, false, ""),
    ("FET", 10800, false, ""),
    ("FJST", 46800, true, ""),
    ("FJT", 43200, false, ""),
    ("FKST", 0, false, "Atlantic/Stanley"),
    ("FKT", 0, false, "Atlantic/Stanley"),
    ("FNST", -3600, true, ""),
    ("FNT", -7200, false, ""),
    ("GALT", -21600, false, ""),
    ("GAMT", -32400, false, ""),
    ("GEST", 0, false, "Asia/Tbilisi"),
    ("GET", 0, false, "Asia/Tbilisi"),
    ("GFT", -10800, false, ""),
    ("GILT", 43200, false, ""),
    ("GMT", 0, false, ""),
    ("GYT", 0, false, "America/Guyana"),
    ("HKT", 28800, false, ""),
    ("HST", -36000, false, ""),
    ("ICT", 25200, false, ""),
    ("IDT", 10800, true, ""),
    ("IOT", 0, false, "Indian/Chagos"),
    ("IRKST", 0, false, "Asia/Irkutsk"),
    ("IRKT", 0, false, "Asia/Irkutsk"),
    ("IRT", 12600, false, ""),
    ("IST", 7200, false, ""),
    ("JAYT", 32400, false, ""),
    ("JST", 32400, false, ""),
    ("KDT", 36000, true, ""),
    ("KGST", 21600, true, ""),
    ("KGT", 0, false, "Asia/Bishkek"),
    ("KOST", 0, false, "Pacific/Kosrae"),
    ("KRAST", 0, false, "Asia/Krasnoyarsk"),
    ("KRAT", 0, false, "Asia/Krasnoyarsk"),
    ("KST", 32400, false, ""),
    ("LHDT", 0, false, "Australia/Lord_Howe"),
    ("LHST", 37800, false, ""),
    ("LIGT", 36000, false, ""),
    ("LINT", 0, false, "Pacific/Kiritimati"),
    ("LKT", 0, false, "Asia/Colombo"),
    ("MAGST", 0, false, "Asia/Magadan"),
    ("MAGT", 0, false, "Asia/Magadan"),
    ("MART", -34200, false, ""),
    ("MAWT", 0, false, "Antarctica/Mawson"),
    ("MDT", -21600, true, ""),
    ("MEST", 7200, true, ""),
    ("MESZ", 7200, true, ""),
    ("MET", 3600, false, ""),
    ("METDST", 7200, true, ""),
    ("MEZ", 3600, false, ""),
    ("MHT", 43200, false, ""),
    ("MMT", 23400, false, ""),
    ("MPT", 36000, false, ""),
    ("MSD", 14400, true, ""),
    ("MSK", 0, false, "Europe/Moscow"),
    ("MST", -25200, false, ""),
    ("MUST", 18000, true, ""),
    ("MUT", 14400, false, ""),
    ("MVT", 18000, false, ""),
    ("MYT", 28800, false, ""),
    ("NDT", -9000, true, ""),
    ("NFT", -12600, false, ""),
    ("NOVST", 0, false, "Asia/Novosibirsk"),
    ("NOVT", 0, false, "Asia/Novosibirsk"),
    ("NPT", 20700, false, ""),
    ("NST", -12600, false, ""),
    ("NUT", 0, false, "Pacific/Niue"),
    ("NZDT", 46800, true, ""),
    ("NZST", 43200, false, ""),
    ("NZT", 43200, false, ""),
    ("OMSST", 0, false, "Asia/Omsk"),
    ("OMST", 0, false, "Asia/Omsk"),
    ("PDT", -25200, true, ""),
    ("PET", -18000, false, ""),
    ("PETST", 0, false, "Asia/Kamchatka"),
    ("PETT", 0, false, "Asia/Kamchatka"),
    ("PGT", 36000, false, ""),
    ("PHT", 28800, false, ""),
    ("PKST", 21600, true, ""),
    ("PKT", 18000, false, ""),
    ("PMDT", -7200, true, ""),
    ("PMST", -10800, false, ""),
    ("PONT", 39600, false, ""),
    ("PST", -28800, false, ""),
    ("PWT", 32400, false, ""),
    ("PYST", -10800, true, ""),
    ("PYT", 0, false, "America/Asuncion"),
    ("RET", 14400, false, ""),
    ("SADT", 37800, true, ""),
    ("SAST", 7200, false, ""),
    ("SCT", 14400, false, ""),
    ("SGT", 0, false, "Asia/Singapore"),
    ("TAHT", -36000, false, ""),
    ("TFT", 18000, false, ""),
    ("TJT", 18000, false, ""),
    ("TKT", 0, false, "Pacific/Fakaofo"),
    ("TMT", 0, false, "Asia/Ashgabat"),
    ("TOT", 46800, false, ""),
    ("TRUT", 36000, false, ""),
    ("TVT", 43200, false, ""),
    ("UCT", 0, false, ""),
    ("ULAST", 32400, true, ""),
    ("ULAT", 0, false, "Asia/Ulaanbaatar"),
    ("UT", 0, false, ""),
    ("UTC", 0, false, ""),
    ("UYST", -7200, true, ""),
    ("UYT", -10800, false, ""),
    ("UZST", 21600, true, ""),
    ("UZT", 18000, false, ""),
    ("VET", 0, false, "America/Caracas"),
    ("VLAST", 0, false, "Asia/Vladivostok"),
    ("VLAT", 0, false, "Asia/Vladivostok"),
    ("VOLT", 0, false, "Europe/Volgograd"),
    ("VUT", 39600, false, ""),
    ("WADT", 28800, true, ""),
    ("WAKT", 43200, false, ""),
    ("WAST", 25200, false, ""),
    ("WAT", 3600, false, ""),
    ("WDT", 32400, true, ""),
    ("WET", 0, false, ""),
    ("WETDST", 3600, true, ""),
    ("WFT", 43200, false, ""),
    ("WGST", -7200, true, ""),
    ("WGT", -10800, false, ""),
    ("XJT", 21600, false, ""),
    ("YAKST", 0, false, "Asia/Yakutsk"),
    ("YAKT", 0, false, "Asia/Yakutsk"),
    ("YAPT", 36000, false, ""),
    ("YEKST", 21600, true, ""),
    ("YEKT", 0, false, "Asia/Yekaterinburg"),
    ("Z", 0, false, ""),
    ("ZULU", 0, false, ""),
];

/// IANA zone names (tzdata 2026c `Zone` and `Link` names, incl. backward links), sorted case-insensitively.
const IANA_ZONES: &[&str] = &[
    "Africa/Abidjan", "Africa/Accra", "Africa/Addis_Ababa", "Africa/Algiers", "Africa/Asmara", "Africa/Asmera",
    "Africa/Bamako", "Africa/Bangui", "Africa/Banjul", "Africa/Bissau", "Africa/Blantyre", "Africa/Brazzaville",
    "Africa/Bujumbura", "Africa/Cairo", "Africa/Casablanca", "Africa/Ceuta", "Africa/Conakry", "Africa/Dakar",
    "Africa/Dar_es_Salaam", "Africa/Djibouti", "Africa/Douala", "Africa/El_Aaiun", "Africa/Freetown", "Africa/Gaborone",
    "Africa/Harare", "Africa/Johannesburg", "Africa/Juba", "Africa/Kampala", "Africa/Khartoum", "Africa/Kigali",
    "Africa/Kinshasa", "Africa/Lagos", "Africa/Libreville", "Africa/Lome", "Africa/Luanda", "Africa/Lubumbashi",
    "Africa/Lusaka", "Africa/Malabo", "Africa/Maputo", "Africa/Maseru", "Africa/Mbabane", "Africa/Mogadishu",
    "Africa/Monrovia", "Africa/Nairobi", "Africa/Ndjamena", "Africa/Niamey", "Africa/Nouakchott", "Africa/Ouagadougou",
    "Africa/Porto-Novo", "Africa/Sao_Tome", "Africa/Timbuktu", "Africa/Tripoli", "Africa/Tunis", "Africa/Windhoek",
    "America/Adak", "America/Anchorage", "America/Anguilla", "America/Antigua", "America/Araguaina", "America/Argentina/Buenos_Aires",
    "America/Argentina/Catamarca", "America/Argentina/ComodRivadavia", "America/Argentina/Cordoba", "America/Argentina/Jujuy", "America/Argentina/La_Rioja", "America/Argentina/Mendoza",
    "America/Argentina/Rio_Gallegos", "America/Argentina/Salta", "America/Argentina/San_Juan", "America/Argentina/San_Luis", "America/Argentina/Tucuman", "America/Argentina/Ushuaia",
    "America/Aruba", "America/Asuncion", "America/Atikokan", "America/Atka", "America/Bahia", "America/Bahia_Banderas",
    "America/Barbados", "America/Belem", "America/Belize", "America/Blanc-Sablon", "America/Boa_Vista", "America/Bogota",
    "America/Boise", "America/Buenos_Aires", "America/Cambridge_Bay", "America/Campo_Grande", "America/Cancun", "America/Caracas",
    "America/Catamarca", "America/Cayenne", "America/Cayman", "America/Chicago", "America/Chihuahua", "America/Ciudad_Juarez",
    "America/Coral_Harbour", "America/Cordoba", "America/Costa_Rica", "America/Coyhaique", "America/Creston", "America/Cuiaba",
    "America/Curacao", "America/Danmarkshavn", "America/Dawson", "America/Dawson_Creek", "America/Denver", "America/Detroit",
    "America/Dominica", "America/Edmonton", "America/Eirunepe", "America/El_Salvador", "America/Ensenada", "America/Fort_Nelson",
    "America/Fort_Wayne", "America/Fortaleza", "America/Glace_Bay", "America/Godthab", "America/Goose_Bay", "America/Grand_Turk",
    "America/Grenada", "America/Guadeloupe", "America/Guatemala", "America/Guayaquil", "America/Guyana", "America/Halifax",
    "America/Havana", "America/Hermosillo", "America/Indiana/Indianapolis", "America/Indiana/Knox", "America/Indiana/Marengo", "America/Indiana/Petersburg",
    "America/Indiana/Tell_City", "America/Indiana/Vevay", "America/Indiana/Vincennes", "America/Indiana/Winamac", "America/Indianapolis", "America/Inuvik",
    "America/Iqaluit", "America/Jamaica", "America/Jujuy", "America/Juneau", "America/Kentucky/Louisville", "America/Kentucky/Monticello",
    "America/Knox_IN", "America/Kralendijk", "America/La_Paz", "America/Lima", "America/Los_Angeles", "America/Louisville",
    "America/Lower_Princes", "America/Maceio", "America/Managua", "America/Manaus", "America/Marigot", "America/Martinique",
    "America/Matamoros", "America/Mazatlan", "America/Mendoza", "America/Menominee", "America/Merida", "America/Metlakatla",
    "America/Mexico_City", "America/Miquelon", "America/Moncton", "America/Monterrey", "America/Montevideo", "America/Montreal",
    "America/Montserrat", "America/Nassau", "America/New_York", "America/Nipigon", "America/Nome", "America/Noronha",
    "America/North_Dakota/Beulah", "America/North_Dakota/Center", "America/North_Dakota/New_Salem", "America/Nuuk", "America/Ojinaga", "America/Panama",
    "America/Pangnirtung", "America/Paramaribo", "America/Phoenix", "America/Port-au-Prince", "America/Port_of_Spain", "America/Porto_Acre",
    "America/Porto_Velho", "America/Puerto_Rico", "America/Punta_Arenas", "America/Rainy_River", "America/Rankin_Inlet", "America/Recife",
    "America/Regina", "America/Resolute", "America/Rio_Branco", "America/Rosario", "America/Santa_Isabel", "America/Santarem",
    "America/Santiago", "America/Santo_Domingo", "America/Sao_Paulo", "America/Scoresbysund", "America/Shiprock", "America/Sitka",
    "America/St_Barthelemy", "America/St_Johns", "America/St_Kitts", "America/St_Lucia", "America/St_Thomas", "America/St_Vincent",
    "America/Swift_Current", "America/Tegucigalpa", "America/Thule", "America/Thunder_Bay", "America/Tijuana", "America/Toronto",
    "America/Tortola", "America/Vancouver", "America/Virgin", "America/Whitehorse", "America/Winnipeg", "America/Yakutat",
    "America/Yellowknife", "Antarctica/Casey", "Antarctica/Davis", "Antarctica/DumontDUrville", "Antarctica/Macquarie", "Antarctica/Mawson",
    "Antarctica/McMurdo", "Antarctica/Palmer", "Antarctica/Rothera", "Antarctica/South_Pole", "Antarctica/Syowa", "Antarctica/Troll",
    "Antarctica/Vostok", "Arctic/Longyearbyen", "Asia/Aden", "Asia/Almaty", "Asia/Amman", "Asia/Anadyr",
    "Asia/Aqtau", "Asia/Aqtobe", "Asia/Ashgabat", "Asia/Ashkhabad", "Asia/Atyrau", "Asia/Baghdad",
    "Asia/Bahrain", "Asia/Baku", "Asia/Bangkok", "Asia/Barnaul", "Asia/Beirut", "Asia/Bishkek",
    "Asia/Brunei", "Asia/Calcutta", "Asia/Chita", "Asia/Choibalsan", "Asia/Chongqing", "Asia/Chungking",
    "Asia/Colombo", "Asia/Dacca", "Asia/Damascus", "Asia/Dhaka", "Asia/Dili", "Asia/Dubai",
    "Asia/Dushanbe", "Asia/Famagusta", "Asia/Gaza", "Asia/Harbin", "Asia/Hebron", "Asia/Ho_Chi_Minh",
    "Asia/Hong_Kong", "Asia/Hovd", "Asia/Irkutsk", "Asia/Istanbul", "Asia/Jakarta", "Asia/Jayapura",
    "Asia/Jerusalem", "Asia/Kabul", "Asia/Kamchatka", "Asia/Karachi", "Asia/Kashgar", "Asia/Kathmandu",
    "Asia/Katmandu", "Asia/Khandyga", "Asia/Kolkata", "Asia/Krasnoyarsk", "Asia/Kuala_Lumpur", "Asia/Kuching",
    "Asia/Kuwait", "Asia/Macao", "Asia/Macau", "Asia/Magadan", "Asia/Makassar", "Asia/Manila",
    "Asia/Muscat", "Asia/Nicosia", "Asia/Novokuznetsk", "Asia/Novosibirsk", "Asia/Omsk", "Asia/Oral",
    "Asia/Phnom_Penh", "Asia/Pontianak", "Asia/Pyongyang", "Asia/Qatar", "Asia/Qostanay", "Asia/Qyzylorda",
    "Asia/Rangoon", "Asia/Riyadh", "Asia/Saigon", "Asia/Sakhalin", "Asia/Samarkand", "Asia/Seoul",
    "Asia/Shanghai", "Asia/Singapore", "Asia/Srednekolymsk", "Asia/Taipei", "Asia/Tashkent", "Asia/Tbilisi",
    "Asia/Tehran", "Asia/Tel_Aviv", "Asia/Thimbu", "Asia/Thimphu", "Asia/Tokyo", "Asia/Tomsk",
    "Asia/Ujung_Pandang", "Asia/Ulaanbaatar", "Asia/Ulan_Bator", "Asia/Urumqi", "Asia/Ust-Nera", "Asia/Vientiane",
    "Asia/Vladivostok", "Asia/Yakutsk", "Asia/Yangon", "Asia/Yekaterinburg", "Asia/Yerevan", "Atlantic/Azores",
    "Atlantic/Bermuda", "Atlantic/Canary", "Atlantic/Cape_Verde", "Atlantic/Faeroe", "Atlantic/Faroe", "Atlantic/Jan_Mayen",
    "Atlantic/Madeira", "Atlantic/Reykjavik", "Atlantic/South_Georgia", "Atlantic/St_Helena", "Atlantic/Stanley", "Australia/ACT",
    "Australia/Adelaide", "Australia/Brisbane", "Australia/Broken_Hill", "Australia/Canberra", "Australia/Currie", "Australia/Darwin",
    "Australia/Eucla", "Australia/Hobart", "Australia/LHI", "Australia/Lindeman", "Australia/Lord_Howe", "Australia/Melbourne",
    "Australia/North", "Australia/NSW", "Australia/Perth", "Australia/Queensland", "Australia/South", "Australia/Sydney",
    "Australia/Tasmania", "Australia/Victoria", "Australia/West", "Australia/Yancowinna", "Brazil/Acre", "Brazil/DeNoronha",
    "Brazil/East", "Brazil/West", "Canada/Atlantic", "Canada/Central", "Canada/Eastern", "Canada/Mountain",
    "Canada/Newfoundland", "Canada/Pacific", "Canada/Saskatchewan", "Canada/Yukon", "CET", "Chile/Continental",
    "Chile/EasterIsland", "CST6CDT", "Cuba", "EET", "Egypt", "Eire",
    "EST5EDT", "Etc/GMT", "Etc/GMT+0", "Etc/GMT+1", "Etc/GMT+10",
    "Etc/GMT+11", "Etc/GMT+12", "Etc/GMT+2", "Etc/GMT+3", "Etc/GMT+4", "Etc/GMT+5",
    "Etc/GMT+6", "Etc/GMT+7", "Etc/GMT+8", "Etc/GMT+9", "Etc/GMT-0", "Etc/GMT-1",
    "Etc/GMT-10", "Etc/GMT-11", "Etc/GMT-12", "Etc/GMT-13", "Etc/GMT-14", "Etc/GMT-2",
    "Etc/GMT-3", "Etc/GMT-4", "Etc/GMT-5", "Etc/GMT-6", "Etc/GMT-7", "Etc/GMT-8",
    "Etc/GMT-9", "Etc/GMT0", "Etc/Greenwich", "Etc/UCT", "Etc/Universal", "Etc/UTC",
    "Etc/Zulu", "Europe/Amsterdam", "Europe/Andorra", "Europe/Astrakhan", "Europe/Athens", "Europe/Belfast",
    "Europe/Belgrade", "Europe/Berlin", "Europe/Bratislava", "Europe/Brussels", "Europe/Bucharest", "Europe/Budapest",
    "Europe/Busingen", "Europe/Chisinau", "Europe/Copenhagen", "Europe/Dublin", "Europe/Gibraltar", "Europe/Guernsey",
    "Europe/Helsinki", "Europe/Isle_of_Man", "Europe/Istanbul", "Europe/Jersey", "Europe/Kaliningrad", "Europe/Kiev",
    "Europe/Kirov", "Europe/Kyiv", "Europe/Lisbon", "Europe/Ljubljana", "Europe/London", "Europe/Luxembourg",
    "Europe/Madrid", "Europe/Malta", "Europe/Mariehamn", "Europe/Minsk", "Europe/Monaco", "Europe/Moscow",
    "Europe/Nicosia", "Europe/Oslo", "Europe/Paris", "Europe/Podgorica", "Europe/Prague", "Europe/Riga",
    "Europe/Rome", "Europe/Samara", "Europe/San_Marino", "Europe/Sarajevo", "Europe/Saratov", "Europe/Simferopol",
    "Europe/Skopje", "Europe/Sofia", "Europe/Stockholm", "Europe/Tallinn", "Europe/Tirane", "Europe/Tiraspol",
    "Europe/Ulyanovsk", "Europe/Uzhgorod", "Europe/Vaduz", "Europe/Vatican", "Europe/Vienna", "Europe/Vilnius",
    "Europe/Volgograd", "Europe/Warsaw", "Europe/Zagreb", "Europe/Zaporozhye", "Europe/Zurich", "Factory",
    "GB", "GB-Eire", "GMT", "GMT+0", "GMT-0", "GMT0",
    "Greenwich", "Hongkong", "Iceland", "Indian/Antananarivo", "Indian/Chagos",
    "Indian/Christmas", "Indian/Cocos", "Indian/Comoro", "Indian/Kerguelen", "Indian/Mahe", "Indian/Maldives",
    "Indian/Mauritius", "Indian/Mayotte", "Indian/Reunion", "Iran", "Israel", "Jamaica",
    "Japan", "Kwajalein", "Libya", "MET", "Mexico/BajaNorte", "Mexico/BajaSur",
    "Mexico/General", "MST7MDT", "Navajo", "NZ", "NZ-CHAT",
    "Pacific/Apia", "Pacific/Auckland", "Pacific/Bougainville", "Pacific/Chatham", "Pacific/Chuuk", "Pacific/Easter",
    "Pacific/Efate", "Pacific/Enderbury", "Pacific/Fakaofo", "Pacific/Fiji", "Pacific/Funafuti", "Pacific/Galapagos",
    "Pacific/Gambier", "Pacific/Guadalcanal", "Pacific/Guam", "Pacific/Honolulu", "Pacific/Johnston", "Pacific/Kanton",
    "Pacific/Kiritimati", "Pacific/Kosrae", "Pacific/Kwajalein", "Pacific/Majuro", "Pacific/Marquesas", "Pacific/Midway",
    "Pacific/Nauru", "Pacific/Niue", "Pacific/Norfolk", "Pacific/Noumea", "Pacific/Pago_Pago", "Pacific/Palau",
    "Pacific/Pitcairn", "Pacific/Pohnpei", "Pacific/Ponape", "Pacific/Port_Moresby", "Pacific/Rarotonga", "Pacific/Saipan",
    "Pacific/Samoa", "Pacific/Tahiti", "Pacific/Tarawa", "Pacific/Tongatapu", "Pacific/Truk", "Pacific/Wake",
    "Pacific/Wallis", "Pacific/Yap", "Poland", "Portugal", "PRC", "PST8PDT",
    "ROC", "ROK", "Singapore", "Turkey", "UCT", "Universal",
    "US/Alaska", "US/Aleutian", "US/Arizona", "US/Central", "US/East-Indiana", "US/Eastern",
    "US/Hawaii", "US/Indiana-Starke", "US/Michigan", "US/Mountain", "US/Pacific", "US/Samoa",
    "UTC", "W-SU", "WET", "Zulu",
];

#[cfg(test)]
mod tests {
    use super::{
        abbrev_at_utc, fixed_offset_setting, lookup_zone, offset_at_utc, parse_numeric_offset, parse_timezone_setting,
        DtErr, TimeZoneSetting, ZoneSpec, DEFAULT_ABBREVS, IANA_ZONES, fold_cmp,
    };
    use pgarbor_common::SqlState;
    use std::cmp::Ordering;

    fn fixed(name: &str, secs: i32) -> TimeZoneSetting {
        TimeZoneSetting::Fixed { name: name.to_string(), secs_east: secs }
    }

    #[test]
    fn utc_family_settings() {
        for w in ["UTC", "utc", "GMT", "Etc/UTC", "Z", "zulu"] {
            assert_eq!(parse_timezone_setting(w).expect("utc"), TimeZoneSetting::Utc);
        }
    }

    #[test]
    fn posix_numeric_and_interval_settings() {
        assert_eq!(parse_timezone_setting("+05:30").expect("posix"), fixed("+05:30", -19800));
        assert_eq!(parse_timezone_setting("UTC+3").expect("posix"), fixed("UTC+3", -10800));
        assert_eq!(parse_timezone_setting("UTC-3").expect("posix"), fixed("UTC-3", 10800));
        assert_eq!(parse_timezone_setting("-8").expect("numeric"), fixed("<-08>+08", -28800));
        assert_eq!(parse_timezone_setting("8").expect("numeric"), fixed("<+08>-08", 28800));
        assert_eq!(parse_timezone_setting("interval '-08:00'").expect("interval"), fixed("<-08>+08", -28800));
    }

    #[test]
    fn fixed_offset_names() {
        assert_eq!(fixed_offset_setting(-28800).expect("fixed"), fixed("<-08>+08", -28800));
        assert_eq!(fixed_offset_setting(19800).expect("fixed"), fixed("<+05:30>-05:30", 19800));
        assert_eq!(offset_at_utc(&fixed("x", -28800), 0).expect("offset"), -28800);
        assert_eq!(abbrev_at_utc(&fixed("x", -28800), 0).expect("abbrev"), "-08");
        assert_eq!(abbrev_at_utc(&TimeZoneSetting::Utc, 0).expect("abbrev"), "UTC");
    }

    #[test]
    fn iana_names_and_errors() {
        assert_eq!(
            parse_timezone_setting("america/new_york").expect("iana"),
            TimeZoneSetting::Named("America/New_York".to_string())
        );
        assert_eq!(parse_timezone_setting("Etc/GMT+3").expect("iana"), fixed("Etc/GMT+3", -10800));
        let err = parse_timezone_setting("Mars/Base").expect_err("unknown zone");
        assert_eq!(err.sqlstate, SqlState::INVALID_PARAMETER_VALUE);
        assert_eq!(err.message, "invalid value for parameter \"TimeZone\": \"Mars/Base\"");
        let range = parse_timezone_setting("+16").expect_err("out of range");
        assert_eq!(range.sqlstate, SqlState::INVALID_PARAMETER_VALUE);
    }

    #[test]
    fn abbreviations_rejected_as_session_zone() {
        // Oracle (PG 18.6): SET TIME ZONE 'PST' / 'EST' -> 22023; abbreviations stay valid in input.
        for name in ["PST", "EST", "MST", "HST"] {
            let err = parse_timezone_setting(name).expect_err("abbreviation is not a session zone");
            assert_eq!(err.sqlstate, SqlState::INVALID_PARAMETER_VALUE);
            assert_eq!(err.message, format!("invalid value for parameter \"TimeZone\": \"{name}\""));
        }
        assert_eq!(
            parse_timezone_setting("PST8PDT").expect("posix-style zone"),
            TimeZoneSetting::Named("PST8PDT".to_string())
        );
        assert_eq!(
            lookup_zone("est").expect("lookup"),
            Ok(ZoneSpec::Fixed { secs_east: -18000, abbrev: Some("EST".to_string()) })
        );
    }

    #[test]
    fn zone_lookup() {
        assert_eq!(
            lookup_zone("pst").expect("lookup"),
            Ok(ZoneSpec::Fixed { secs_east: -28800, abbrev: Some("PST".to_string()) })
        );
        assert_eq!(
            lookup_zone("gmt").expect("lookup"),
            Ok(ZoneSpec::Fixed { secs_east: 0, abbrev: Some("GMT".to_string()) })
        );
        assert_eq!(
            lookup_zone("america/new_york").expect("lookup"),
            Err(DtErr::UnsupportedTimezone("america/new_york".to_string()))
        );
        assert_eq!(lookup_zone("art").expect("lookup"), Err(DtErr::UnsupportedTimezone("art".to_string())));
        assert_eq!(lookup_zone("mars/base").expect("lookup"), Err(DtErr::BadTimezone("mars/base".to_string())));
    }

    #[test]
    fn numeric_offsets() {
        assert_eq!(parse_numeric_offset("+05:30").expect("offset"), Ok(19800));
        assert_eq!(parse_numeric_offset("+0530").expect("offset"), Ok(19800));
        assert_eq!(parse_numeric_offset("-8").expect("offset"), Ok(-28800));
        assert_eq!(parse_numeric_offset("+16:00").expect("offset"), Err(DtErr::TzDispOverflow));
        assert_eq!(parse_numeric_offset("+05:60").expect("offset"), Err(DtErr::TzDispOverflow));
        assert_eq!(parse_numeric_offset("05:30").expect("offset"), Err(DtErr::BadFormat));
    }

    #[test]
    fn tables_are_sorted_and_unique() {
        assert!(DEFAULT_ABBREVS.windows(2).all(|w| fold_cmp(w[0].0, w[1].0) == Ordering::Less));
        assert!(IANA_ZONES.windows(2).all(|w| fold_cmp(w[0], w[1]) == Ordering::Less));
    }
}
