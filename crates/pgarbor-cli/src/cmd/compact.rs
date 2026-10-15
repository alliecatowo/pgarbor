//! `pgarbor compact` (lease CLI-15): rewrite the file with only reachable pages (ARCHITECTURE
//! section 5 "Space reuse in v1").
//!
//! Opens the database exclusively (`open_database`, writable). A running `pgarbor serve` holds the
//! lock: exit 4 with hint `stop the server (compaction needs exclusive access)`. Calls
//! `Database::compact()` -> `pgarbor_storage::CompactReport` (old/new length and page counts). Prints a
//! progress line to stderr before starting (`compacting dev.pga (120 MiB)...`, suppressed by
//! `--quiet`/`--json`), then `compacted dev.pga: 120 MiB -> 8.1 MiB (reclaimed 112 MiB)` to stdout.
//! `--json`: the `CompactReport` serialized as-is plus `"path"`: wrap as `{"path":..., "report":{...}}`.
//! Afterwards `db.close()`. After a successful compaction run `Container` verify at Roots level by
//! reopening is NOT needed (the storage layer verifies before the atomic rename).
use crate::cli::CompactArgs;
use crate::ctx::{CliError, CliResult, Ctx};
use crate::exit::Exit;
use crate::open::{open_database, OpenSpec};
use crate::paths::resolve_db;

pub fn run(ctx: &mut Ctx, args: &CompactArgs) -> CliResult<Exit> {
    let path = resolve_db(args.file.as_deref(), args.db.db.as_deref())?;
    let db = open_database(&path, &OpenSpec::existing()).map_err(|mut error| {
        if error.exit == Exit::LOCKED {
            error.hint = Some("stop the server (compaction needs exclusive access)".to_string());
        }
        error
    })?;
    let label = path.to_string_lossy().into_owned();
    let before_len = match std::fs::metadata(&path) {
        Ok(metadata) => metadata.len(),
        Err(error) => {
            let close_result = db.close();
            if let Err(close_error) = close_result {
                return Err(CliError::from(close_error));
            }
            return Err(CliError::io("could not read database file size", &error));
        }
    };

    ctx.info(&format!("compacting {label} ({})...", format_size(before_len)));
    let compacted = db.compact();
    let close_result = db.close();
    let report = match compacted {
        Ok(report) => report,
        Err(error) => return Err(CliError::from(error)),
    };
    close_result.map_err(CliError::from)?;

    if ctx.json {
        let value = serde_json::json!({ "path": label, "report": report });
        ctx.emit_json(&value)?;
    } else {
        let reclaimed = report.old_file_len.saturating_sub(report.new_file_len);
        ctx.out_line(&format!(
            "compacted {label}: {} -> {} (reclaimed {})",
            format_size(report.old_file_len),
            format_size(report.new_file_len),
            format_size(reclaimed)
        ));
    }
    Ok(Exit::OK)
}

fn format_size(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KiB", "MiB", "GiB", "TiB"];
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit + 1 < UNITS.len() {
        value /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{bytes} B")
    } else if value.fract() < 0.05 {
        format!("{value:.0} {}", UNITS[unit])
    } else {
        format!("{value:.1} {}", UNITS[unit])
    }
}

#[cfg(test)]
mod tests {
    use super::format_size;

    #[test]
    fn formats_compaction_sizes() {
        assert_eq!(format_size(0), "0 B");
        assert_eq!(format_size(1024), "1 KiB");
        assert_eq!(format_size(8 * 1024 * 1024 + 102_400), "8.1 MiB");
        assert_eq!(format_size(120 * 1024 * 1024), "120 MiB");
    }
}
