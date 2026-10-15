//! `pgarbor branch snapshot create|list|drop` (lease CLI-18; BRANCH snapshots, RFC 001 section 9 reset
//! recovery). Named snapshots are cheap root pointers inside the branch record.
//!
//! * `create BRANCH LABEL`: `db.create_snapshot(branch, label)`; output `snapshot "L" created on
//!   branch "B" at commit <seq>`; label errors (42602) -> exit 64, exists -> exit 6, branch missing ->
//!   exit 5, too many (`MAX_SNAPSHOTS_PER_BRANCH`) -> exit 1.
//! * `list BRANCH`: `db.engine().branches().get(&BranchRef::name(b))` -> `BranchRecord.snapshots`
//!   -> [`crate::reports::SnapshotRow`] (`kind` = `format!("{:?}", kind)` lowercased with underscores:
//!   `User` -> `user`, `ResetRecovery` -> `reset_recovery`). Human table `LABEL | KIND | COMMIT |
//!   CREATED`. `--json`: an array under `{"branch": "...", "snapshots": [...]}`.
//! * `drop BRANCH LABEL`: `db.drop_snapshot(branch, label)`; not found -> exit 5.
//! All open the database like `basic.rs` and call `db.close()` at the end.
use pgarbor_common::PgError;
use crate::cli::DbFlag;
use crate::ctx::{CliError, CliResult, Ctx};
use crate::exit::Exit;

#[allow(unused_variables)]
pub fn create(ctx: &mut Ctx, branch: &str, label: &str, db: &DbFlag) -> CliResult<Exit> {
    Err(CliError::from(PgError::unimplemented("CLI-18", "snapshot create (branch::snapshot::create)")))
}

#[allow(unused_variables)]
pub fn list(ctx: &mut Ctx, branch: &str, db: &DbFlag) -> CliResult<Exit> {
    Err(CliError::from(PgError::unimplemented("CLI-18", "snapshot list (branch::snapshot::list)")))
}

#[allow(unused_variables)]
pub fn drop_snapshot(ctx: &mut Ctx, branch: &str, label: &str, db: &DbFlag) -> CliResult<Exit> {
    Err(CliError::from(PgError::unimplemented("CLI-18", "snapshot drop (branch::snapshot::drop_snapshot)")))
}
