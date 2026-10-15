//! `pgarbor doctor` (lease CLI-16): explain problems; never modifies anything.
//!
//! Runs these checks in order, each producing a [`crate::reports::DoctorCheck`] (`ok`/`warn`/`fail`/
//! `skip`) with a one-line message and, for non-ok results, a concrete `hint`:
//! 1. `file`: exists, is a regular file (not a directory; symlink target resolved), readable;
//!    writable (`OpenOptions::write` probe is NOT done - check the permission bits instead:
//!    read-only file -> warn `file is read-only; serve --read-only only`).
//! 2. `directory`: parent directory writable (needed for `.compact-tmp`, `.export-tmp`, `.serve.json`
//!    files): probe by creating and removing `<db>.doctor-probe`; warn on failure.
//! 3. `leftovers`: `<db>.compact-tmp`, `<db>.export-tmp`, `*.detach-tmp` next to the file -> warn
//!    (`left over from an interrupted compact/export; safe to delete when no pgarbor is running`).
//! 4. `superblock`: read via `open_container_readonly(path, VerifyLevel::Roots)`: both slots' status
//!    (`Container::recovery_report()`: `chosen_slot`, `fell_back_to_older_slot`,
//!    `truncated_tail_bytes` > 0 -> warn `uncommitted tail will be truncated on next writable open`),
//!    unsupported format version -> fail with the exact version numbers and hint `upgrade pgarbor`.
//! 5. `integrity`: `verify_file(.., VerifyLevel::Full)` summary (fail with first error message when not ok).
//! 6. `lock`: file lock state; a live `serve_registry::read_info` -> ok `served by pgarbor pid N on
//!    host:port`; locked without info -> warn `locked by another process (not a pgarbor serve we can
//!    see)`.
//! 7. `listener`: when serve info exists, TCP-connect to each listener with a 500 ms timeout; failure
//!    -> fail `listener 127.0.0.1:5432 does not accept connections`.
//! 8. `environment`: `PGARBOR_DB` set but pointing to a missing file -> warn; `DATABASE_URL` set ->
//!    info that `branch exec` will refuse without `--force-env`.
//! 9. `filesystem`: report whether reflink cloning is available on the file's filesystem if
//!    `pgarbor_branches::reflink` exposes a probe (read `reflink.rs`); otherwise `skip` with
//!    `reflink probe unavailable`.
//! Exit: 0 when no `fail`, 3 when the failure is integrity/format, 4 when the only failure is a lock,
//! else 1. Human output: one line per check `[ok] name: message` / `[warn]` / `[FAIL]` / `[skip]`
//! followed by hints indented. `--json`: [`crate::reports::DoctorOutput`]. Without a resolvable
//! database path only checks 8 (and a `file` fail) run.
use pgarbor_common::PgError;
use crate::cli::FileArgs;
use crate::ctx::{CliError, CliResult, Ctx};
use crate::exit::Exit;

#[allow(unused_variables)]
pub fn run(ctx: &mut Ctx, args: &FileArgs) -> CliResult<Exit> {
    Err(CliError::from(PgError::unimplemented("CLI-16", "doctor (cmd::doctor::run)")))
}
