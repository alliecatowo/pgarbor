//! `pgarbor init FILE` / `pgarbor create FILE` (lease CLI-06; CLI-01, harness-02 item 1).
//!
//! ## Behavior
//! 1. `FILE` exists:
//!    * with `--if-not-exists`: open it read-only (`open::open_container_readonly`, Roots verify); a
//!      valid PGArbor file -> success with `created = false`; anything else -> exit 3 (not a PGArbor
//!      database) so a typo never "succeeds";
//!    * otherwise exit 6 (`EXISTS`): `cannot create "<file>": file already exists` (stderr, non-zero as
//!      harness-02 requires). Never truncate or modify the existing file.
//! 2. Parent directory missing: exit 5 `directory "<dir>" does not exist`.
//! 3. Validate flags: `--page-size` a power of two in 4096..=65536 else exit 64; `--branch` must pass
//!    `pgarbor_branches::name::validate_branch_name` (error -> exit 64 with the validator's message).
//! 4. `Database::create(file, OpenOptions { page_size, initial_branch, sync: SyncMode::Full, ..})`
//!    creates the container with branch `main` (or `--branch`) and bootstraps the PG catalog (built-in
//!    `postgres` database and role). On ANY failure after the call started, delete the file if this
//!    invocation created it (never leave a half-initialized file), then report the error.
//! 5. `db.close()` (flush), then report.
//! 6. Unix: leave the file mode to the process umask (typically 0644); do not chmod.
//!
//! ## Output
//! Human (stdout): `created dev.pga (branch main, page size 8192, id <db uuid>)`; with `--quiet` nothing.
//! `--json`: [`crate::reports::InitReport`]. `db_uuid` from `db.engine().container().db_uuid().to_hyphenated()`.
use pgarbor_common::PgError;
use crate::cli::InitArgs;
use crate::ctx::{CliError, CliResult, Ctx};
use crate::exit::Exit;

#[allow(unused_variables)]
pub fn run(ctx: &mut Ctx, args: &InitArgs) -> CliResult<Exit> {
    Err(CliError::from(PgError::unimplemented("CLI-06", "init (cmd::init::run)")))
}
