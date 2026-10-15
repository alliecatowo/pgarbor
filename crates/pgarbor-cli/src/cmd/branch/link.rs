//! `pgarbor branch link` and `branch resolve` (lease CLI-19; BRANCH-07, RFC 001 section 4.1, CLI-05).
//! A `.pgabranch` handle is a tiny JSON file naming a branch by IDENTITY (`format:"pgabranch/1"`,
//! `db_uuid`, `branch_id`, name/path hints, crc32c); see `pgarbor_branches::handle`.
//!
//! ## link
//! `branch link BRANCH HANDLE [--overwrite] [--absolute] [--db FILE]`: resolve the db (`paths::resolve_db`),
//! open it (`open::open_database`, writable not required but the shared helper is fine), then
//! `db.engine().branches().write_handle(&BranchRef::name(branch), handle_path, &HandleOptions{overwrite,
//! absolute_db_path})`. Existing handle without `--overwrite`: exit 6 (58P02). The handle's directory
//! must exist (exit 5). Output [`crate::reports::LinkReport`]; human `linked branch "x" -> ./x.pgabranch
//! (database dev.pga)`.
//!
//! ## resolve
//! `branch resolve HANDLE [--db FILE] [--rewrite-hint]`: validate WITHOUT starting anything.
//! 1. `pgarbor_branches::handle::resolve_db_path(&OsVfs::default(), handle, explicit_db)` finds the master
//!    (hint path relative to the handle, then absolute, then `--db`); not found -> exit 5 listing the
//!    tried paths; malformed/checksum failure -> exit 65.
//! 2. Open the master READ-ONLY (`OpenSpec::read_only()`), compare `Container::db_uuid` with the handle's
//!    `db_uuid`: mismatch -> exit 65 `handle "H" belongs to database <uuid>, but "<path>" is database
//!    <uuid2>` (loud, per RFC; use `HandleFile::check`).
//! 3. Find the branch by id among `list_branches(true)`: present+active -> `branch_exists = true` with
//!    its CURRENT name (may differ from the hint after a rename); deleted -> `branch_exists = false`
//!    plus note `branch was deleted`; absent -> `branch_exists = false`.
//! 4. `relocated` = found path differs (after lexical normalization) from the handle's resolved hint
//!    path. With `--rewrite-hint` and `relocated`, rewrite the handle via `write_handle(.., overwrite:
//!    true, ..)` so the new relative path is stored (`hint_rewritten = true`).
//! Exit: 0 when the branch exists; 5 when the branch is gone. Output [`crate::reports::ResolveReport`].
use pgarbor_common::PgError;
use crate::cli::{BranchLinkArgs, BranchResolveArgs};
use crate::ctx::{CliError, CliResult, Ctx};
use crate::exit::Exit;

#[allow(unused_variables)]
pub fn link(ctx: &mut Ctx, args: &BranchLinkArgs) -> CliResult<Exit> {
    Err(CliError::from(PgError::unimplemented("CLI-19", "branch link (branch::link::link)")))
}

#[allow(unused_variables)]
pub fn resolve(ctx: &mut Ctx, args: &BranchResolveArgs) -> CliResult<Exit> {
    Err(CliError::from(PgError::unimplemented("CLI-19", "branch resolve (branch::link::resolve)")))
}
