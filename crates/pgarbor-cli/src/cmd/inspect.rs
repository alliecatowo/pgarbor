//! `pgarbor inspect` (lease CLI-13): structure and space of a database file (RFC 001 section 5.3).
//!
//! * Always: superblock facts (uuid, format version, page size, commit seq, page count, file length),
//!   branches (rows as in `branch list`).
//! * `--space`: `BranchManager::space()` -> `(Vec<BranchSpace>, SpaceReport)`. Human: totals block
//!   (`file`, `committed`, `reachable`, `garbage`, `reclaimable` via `humanize::bytes`) then a table
//!   `branch | pages | exclusive | bytes | exclusive bytes`. JSON: `InspectOutput.space = Some(report)`,
//!   per-branch numbers in `BranchRow.space`.
//! * `--page N`: `pgarbor_storage::doctor::inspect_page(&container.snapshot(), PageId(N))` ->
//!   `InspectOutput.page`; human: header fields, kind, level, entry count, first/last key hex, children.
//!   Page 0 or beyond `page_count`: exit 64 `page N is out of range (1..=M)`.
//! Opens the container read-only through `open::open_container_readonly` (works while no process holds
//! the exclusive lock; when `pgarbor serve` owns the file print the exit-4 error with the serve hint).
use pgarbor_common::PgError;
use crate::cli::InspectArgs;
use crate::ctx::{CliError, CliResult, Ctx};
use crate::exit::Exit;

#[allow(unused_variables)]
pub fn run(ctx: &mut Ctx, args: &InspectArgs) -> CliResult<Exit> {
    Err(CliError::from(PgError::unimplemented("CLI-13", "inspect (cmd::inspect::run)")))
}
