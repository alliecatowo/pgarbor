//! `pgarbor verify` (lease CLI-14): integrity check, exit 3 on corruption.
//!
//! `pgarbor_storage::doctor::verify_file(Arc::new(OsVfs::default()), path, &BranchRootResolver, level)`
//! (`--level none|roots|full`, default full) reads the file without recovery side effects. Outcomes:
//! * `report.is_ok()` -> exit 0. Warnings and info issues are printed but do not fail.
//! * any `Error` issue -> exit 3 and stderr contains the word `corrupt` (e.g. `database file is
//!   corrupt: 2 errors (first: <message>)`).
//! * file missing -> exit 5; lock held by another process -> exit 4 (hint from `open.rs`).
//! Human output: `verified dev.pga: 1342 pages checked, 1201 reachable, 141 unreachable (garbage) - OK`
//! then one line per issue `error|warning|info: page N (tree T): message`. `--json`: the
//! `DoctorReport` itself (`report.to_json()` shape via serde), printed on stdout also when exit is 3.
use pgarbor_common::PgError;
use crate::cli::VerifyArgs;
use crate::ctx::{CliError, CliResult, Ctx};
use crate::exit::Exit;

#[allow(unused_variables)]
pub fn run(ctx: &mut Ctx, args: &VerifyArgs) -> CliResult<Exit> {
    Err(CliError::from(PgError::unimplemented("CLI-14", "verify (cmd::verify::run)")))
}
