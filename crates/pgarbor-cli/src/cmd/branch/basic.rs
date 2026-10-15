//! Branch lifecycle commands (lease CLI-17; CLI-03, BRANCH-03/05): create, list, rename, delete,
//! restore, purge, set-default. Each function opens the database writable (list: read-only is fine
//! but still needs the exclusive/shared lock, so same `open_database`), runs one engine call and
//! closes the database (`db.close()`).
//!
//! ## create
//! `branch create NAME [--from B] [--snapshot L] [--copy-snapshots]`. Source = `--from` or the default
//! branch (`list_branches` `is_default`). Plain: `db.create_branch(name, from)`; with `--snapshot`:
//! `db.create_branch_from_snapshot(name, from, label)`; with `--copy-snapshots`:
//! `db.engine().create_branch(name, &CreateSource::branch(from), &CreateOptions{copy_snapshots: true,
//! id: None})` (needs `pgarbor_branches::{CreateSource, CreateOptions}`; then re-read the row with
//! `list_branches`). Output: human `created branch feature/auth from main (id <uuid>)`; `--json`: one
//! [`crate::reports::BranchRow`]. Errors: exists -> exit 6 (42710), bad name -> exit 64 (42602), unknown
//! source -> exit 5.
//!
//! ## list
//! `db.list_branches(args.all)`; `--space`: `db.engine().branches().space()` -> match rows by branch id
//! (`BranchSpace.id.to_hyphenated()`). Human: aligned table `NAME | PARENT | DEFAULT | STATE | HEAD |
//! SNAPSHOTS | UPDATED` (`*` in DEFAULT; UPDATED via `humanize::timestamp`; with `--space` add
//! `SIZE | EXCLUSIVE` via `humanize::bytes`). `--json`: [`crate::reports::BranchListReport`]. Sorted as
//! returned (active by name, then deleted).
//!
//! ## rename / set-default
//! `db.rename_branch(old, new)` / `db.set_default_branch(name)`; print the resulting row. A handle file
//! pointing at the renamed branch keeps working (identified by id).
//!
//! ## delete
//! `confirm::require(ctx, args.yes, "delete branch \"x\"")` BEFORE any change (exit 7 when not
//! confirmed). Then `db.delete_branch(name, args.force)`; the embedded API deletes with the default
//! `DeleteOptions`, so the data is RETAINED and `restore` works. With `--purge`: after the delete, parse
//! the returned `BranchInfo.id` with `pgarbor_common::BranchId::parse` and call
//! `db.engine().branches().purge_deleted(&pgarbor_branches::PurgeFilter::Id(id))` (exactly that
//! branch; space is reclaimed by `compact`). `--allow-default` cannot be honored through the embedded
//! API: when given, fail with exit 8 `deleting the default branch is not supported yet; use
//! set-default first`. Deleting the default branch without the flag surfaces the engine error
//! (`cannot delete the default branch`) as exit 7. Sessions using the branch without `--force`: 55006 ->
//! exit 4 with hint `pass --force to end those sessions`.
//! Output: [`crate::reports::DeleteReport`]; human `deleted branch "x" (data retained; restore with:
//! pgarbor branch restore x)`, or `... (purged)` with `--purge`.
//!
//! ## restore / purge
//! `restore NAME_OR_ID [--as-name N]` -> `db.restore_branch(name_or_id, new_name)`.
//! `purge`: `confirm::require` then `db.purge_deleted()`; [`crate::reports::PurgeReport`].
use pgarbor_common::PgError;
use crate::cli::{
    BranchCreateArgs, BranchDeleteArgs, BranchListArgs, BranchPurgeArgs, BranchRenameArgs, BranchRestoreArgs,
    BranchSetDefaultArgs,
};
use crate::ctx::{CliError, CliResult, Ctx};
use crate::exit::Exit;

#[allow(unused_variables)]
pub fn create(ctx: &mut Ctx, args: &BranchCreateArgs) -> CliResult<Exit> {
    Err(CliError::from(PgError::unimplemented("CLI-17", "branch create (branch::basic::create)")))
}

#[allow(unused_variables)]
pub fn list(ctx: &mut Ctx, args: &BranchListArgs) -> CliResult<Exit> {
    Err(CliError::from(PgError::unimplemented("CLI-17", "branch list (branch::basic::list)")))
}

#[allow(unused_variables)]
pub fn rename(ctx: &mut Ctx, args: &BranchRenameArgs) -> CliResult<Exit> {
    Err(CliError::from(PgError::unimplemented("CLI-17", "branch rename (branch::basic::rename)")))
}

#[allow(unused_variables)]
pub fn delete(ctx: &mut Ctx, args: &BranchDeleteArgs) -> CliResult<Exit> {
    Err(CliError::from(PgError::unimplemented("CLI-17", "branch delete (branch::basic::delete)")))
}

#[allow(unused_variables)]
pub fn restore(ctx: &mut Ctx, args: &BranchRestoreArgs) -> CliResult<Exit> {
    Err(CliError::from(PgError::unimplemented("CLI-17", "branch restore (branch::basic::restore)")))
}

#[allow(unused_variables)]
pub fn purge(ctx: &mut Ctx, args: &BranchPurgeArgs) -> CliResult<Exit> {
    Err(CliError::from(PgError::unimplemented("CLI-17", "branch purge (branch::basic::purge)")))
}

#[allow(unused_variables)]
pub fn set_default(ctx: &mut Ctx, args: &BranchSetDefaultArgs) -> CliResult<Exit> {
    Err(CliError::from(PgError::unimplemented("CLI-17", "branch set-default (branch::basic::set_default)")))
}
