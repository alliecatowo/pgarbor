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
use pgarbor_common::PgError;
use crate::cli::CompactArgs;
use crate::ctx::{CliError, CliResult, Ctx};
use crate::exit::Exit;

#[allow(unused_variables)]
pub fn run(ctx: &mut Ctx, args: &CompactArgs) -> CliResult<Exit> {
    Err(CliError::from(PgError::unimplemented("CLI-15", "compact (cmd::compact::run)")))
}
