//! Human formatting helpers (lease CLI-05).
use pgarbor_common::PgError;
use crate::ctx::CliError;

/// `1536` -> `"1.5 KiB"`; below 1024 -> `"512 B"`. One decimal, binary units B, KiB, MiB, GiB, TiB.
/// `0` -> `"0 B"`. Never rounds up into the next unit incorrectly (1023.96 KiB prints as `1.0 MiB`).
#[allow(unused_variables)]
pub fn bytes(n: u64) -> Result<String, CliError> {
    Err(CliError::from(PgError::unimplemented("CLI-05", "format byte sizes (humanize::bytes)")))
}

/// Unix milliseconds -> `YYYY-MM-DD HH:MM:SS UTC` (civil-from-days arithmetic, no time crate).
/// `0` -> `"-"`. Example: `1_700_000_000_000` -> `"2023-11-14 22:13:20 UTC"`.
#[allow(unused_variables)]
pub fn timestamp(unix_ms: u64) -> Result<String, CliError> {
    Err(CliError::from(PgError::unimplemented("CLI-05", "format timestamps (humanize::timestamp)")))
}

/// Unix milliseconds -> RFC 3339 `YYYY-MM-DDTHH:MM:SS.mmmZ` (for JSON consumers who want strings).
#[allow(unused_variables)]
pub fn rfc3339(unix_ms: u64) -> Result<String, CliError> {
    Err(CliError::from(PgError::unimplemented("CLI-05", "RFC 3339 timestamps (humanize::rfc3339)")))
}

/// `"1 row"` / `"2 rows"`, `"1 branch"` / `"3 branches"` style pluralization: appends `s` (or `es` for
/// words ending in `ch`/`s`/`x`).
#[allow(unused_variables)]
pub fn plural(n: u64, noun: &str) -> Result<String, CliError> {
    Err(CliError::from(PgError::unimplemented("CLI-05", "pluralize (humanize::plural)")))
}

/// Current time in unix milliseconds (`SystemTime`, 0 on clock error).
pub fn now_unix_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}
