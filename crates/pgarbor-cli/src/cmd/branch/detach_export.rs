//! `pgarbor branch detach` and `branch export` (lease CLI-21; BRANCH-09, CLI-06/12 in the seed DAG,
//! RFC 001 sections 4.2-4.3): produce a truly independent standalone `.pga` containing exactly one
//! branch.
//!
//! * `detach NAME --out FILE [--strategy auto|reflink|copy] [--target-name main] [--include-snapshots]
//!   [--overwrite]` -> `db.detach_branch(name, out, &DetachOptions{strategy, target_name,
//!   include_snapshots, overwrite, os_filesystem: true, ..Default::default()})`.
//! * `export NAME --out FILE [...]` -> `db.export_branch(name, out, &DetachOptions{strategy:
//!   DetachStrategy::Copy, os_filesystem: true, ..})` (always the streaming compact copy).
//! `StrategyArg` maps `Auto -> DetachStrategy::Auto`, `Reflink -> ReflinkRequired`, `Copy -> Copy`.
//!
//! Validation before calling: `--out` parent directory must exist (exit 5); `--out` equal to the
//! source database path -> exit 64 (never write over the master); `--out` exists without `--overwrite`
//! -> exit 6. The engine verifies the new file by reopening it before reporting success
//! (`DetachReport.verified`). Progress: before a copy export print `exporting branch "x" ...` on stderr
//! (not with `--quiet`/`--json`).
//! Output: human `detached branch "x" -> auth.pga (reflink, 1.2 MiB, id <new db uuid>)` (strategy
//! words `reflink`/`copy`; mention `fallback_reason` when set); `--json`: the `DetachReport` as-is.
//! Exit 8 when `--strategy reflink` is impossible (0A000 from the engine).
use pgarbor_common::PgError;
use crate::cli::{BranchDetachArgs, BranchExportArgs};
use crate::ctx::{CliError, CliResult, Ctx};
use crate::exit::Exit;

#[allow(unused_variables)]
pub fn detach(ctx: &mut Ctx, args: &BranchDetachArgs) -> CliResult<Exit> {
    Err(CliError::from(PgError::unimplemented("CLI-21", "branch detach (branch::detach_export::detach)")))
}

#[allow(unused_variables)]
pub fn export(ctx: &mut Ctx, args: &BranchExportArgs) -> CliResult<Exit> {
    Err(CliError::from(PgError::unimplemented("CLI-21", "branch export (branch::detach_export::export)")))
}
