//! Human formatting helpers (lease CLI-05).
use crate::ctx::CliError;

/// `1536` -> `"1.5 KiB"`; below 1024 -> `"512 B"`. One decimal, binary units B, KiB, MiB, GiB, TiB.
/// `0` -> `"0 B"`. Never rounds up into the next unit incorrectly (1023.96 KiB prints as `1.0 MiB`).
pub fn bytes(n: u64) -> Result<String, CliError> {
    const UNITS: [&str; 5] = ["B", "KiB", "MiB", "GiB", "TiB"];
    if n < 1024 {
        return Ok(format!("{n} B"));
    }
    let mut value = n as f64;
    let mut unit = 0usize;
    while value >= 1024.0 && unit < UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }
    // Promote when rounding to one decimal would otherwise display `1024.0` in this unit.
    // The loop also handles promotion repeatedly, though the input range only reaches TiB.
    while unit < UNITS.len() - 1 && (value * 10.0).round() >= 10_240.0 {
        value /= 1024.0;
        unit += 1;
    }
    Ok(format!("{value:.1} {}", UNITS[unit]))
}

/// Unix milliseconds -> `YYYY-MM-DD HH:MM:SS UTC` (civil-from-days arithmetic, no time crate).
/// `0` -> `"-"`. Example: `1_700_000_000_000` -> `"2023-11-14 22:13:20 UTC"`.
pub fn timestamp(unix_ms: u64) -> Result<String, CliError> {
    if unix_ms == 0 {
        return Ok("-".to_owned());
    }
    let seconds = unix_ms / 1000;
    let days = seconds / 86_400;
    let day_seconds = seconds % 86_400;
    let (year, month, day) = civil_from_days(days as i64);
    let hour = day_seconds / 3600;
    let minute = (day_seconds % 3600) / 60;
    let second = day_seconds % 60;
    Ok(format!("{year:04}-{month:02}-{day:02} {hour:02}:{minute:02}:{second:02} UTC"))
}

/// Unix milliseconds -> RFC 3339 `YYYY-MM-DDTHH:MM:SS.mmmZ` (for JSON consumers who want strings).
pub fn rfc3339(unix_ms: u64) -> Result<String, CliError> {
    let seconds = unix_ms / 1000;
    let millis = unix_ms % 1000;
    let days = seconds / 86_400;
    let day_seconds = seconds % 86_400;
    let (year, month, day) = civil_from_days(days as i64);
    let hour = day_seconds / 3600;
    let minute = (day_seconds % 3600) / 60;
    let second = day_seconds % 60;
    Ok(format!("{year:04}-{month:02}-{day:02}T{hour:02}:{minute:02}:{second:02}.{millis:03}Z"))
}

/// `"1 row"` / `"2 rows"`, `"1 branch"` / `"3 branches"` style pluralization: appends `s` (or `es` for
/// words ending in `ch`/`s`/`x`).
pub fn plural(n: u64, noun: &str) -> Result<String, CliError> {
    let suffix = if n == 1 {
        ""
    } else if noun.ends_with("ch") || noun.ends_with('s') || noun.ends_with('x') {
        "es"
    } else {
        "s"
    };
    Ok(format!("{n} {noun}{suffix}"))
}

/// Convert days since 1970-01-01 to a proleptic Gregorian date.
fn civil_from_days(days: i64) -> (i64, u32, u32) {
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let mut year = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = mp + if mp < 10 { 3 } else { -9 };
    if month <= 2 {
        year += 1;
    }
    (year, month as u32, day as u32)
}

#[cfg(test)]
mod tests {
    use super::{bytes, plural, rfc3339, timestamp};

    #[test]
    fn byte_sizes() {
        assert_eq!(bytes(0).unwrap(), "0 B");
        assert_eq!(bytes(1023).unwrap(), "1023 B");
        assert_eq!(bytes(1024).unwrap(), "1.0 KiB");
        assert_eq!(bytes(1536).unwrap(), "1.5 KiB");
        assert_eq!(bytes(1_048_535).unwrap(), "1.0 MiB");
        assert_eq!(bytes(1_048_575).unwrap(), "1.0 MiB");
    }

    #[test]
    fn timestamps() {
        assert_eq!(timestamp(0).unwrap(), "-");
        assert_eq!(timestamp(1_700_000_000_000).unwrap(), "2023-11-14 22:13:20 UTC");
        assert_eq!(rfc3339(1_700_000_000_123).unwrap(), "2023-11-14T22:13:20.123Z");
    }

    #[test]
    fn plurals() {
        assert_eq!(plural(1, "row").unwrap(), "1 row");
        assert_eq!(plural(2, "branch").unwrap(), "2 branches");
        assert_eq!(plural(2, "class").unwrap(), "2 classes");
        assert_eq!(plural(2, "box").unwrap(), "2 boxes");
    }
}

/// Current time in unix milliseconds (`SystemTime`, 0 on clock error).
pub fn now_unix_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}
