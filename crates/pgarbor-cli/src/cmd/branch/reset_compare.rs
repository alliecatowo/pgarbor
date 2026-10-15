//! `pgarbor branch reset` and `branch compare` (lease CLI-20; BRANCH-06/08, RFC 001 section 9).
//!
//! ## reset
//! Exactly one target flag (`--to-snapshot L`, `--to-branch B`, `--to-parent`), else exit 64
//! `specify exactly one of --to-snapshot, --to-branch, --to-parent`. Mapping to
//! `pgarbor_branches::ResetTarget::{Snapshot, Branch(BranchRef::name), Parent}`.
//! 1. `confirm::require(ctx, yes, "reset branch \"x\" to <target>: all changes since are discarded")`.
//! 2. `--recovery-snapshot L`: FIRST `db.create_snapshot(name, L)` (the Database API's `reset_branch`
//!    takes no `ResetOptions`; two commits, snapshot first so a failure never loses data). Without it
//!    the old head becomes garbage at the next compaction - print that as a note on stderr unless
//!    `--quiet`.
//! 3. `db.reset_branch(name, target, args.force)` (55006 when other sessions use the branch and no
//!    `--force`: exit 4 with hint `pass --force to end those sessions or stop the server`).
//! 4. Output: the resulting branch row; human `reset branch "x" to snapshot "s" (head commit N)`;
//!    `--json`: [`crate::reports::BranchRow`] plus `"recovery_snapshot"` as a wrapper
//!    `{"branch": <row>, "recovery_snapshot": "L" | null}`.
//!
//! ## compare
//! `db.engine().branches().compare(&BranchRef::name(a), &BranchRef::name(b), &CompareOptions{
//! max_differences, samples_per_space: samples, include_pages: !no_pages})` -> `CompareReport`.
//! `--json`: the report as-is. Human:
//! ```text
//! a: main (commit 42)   b: feature (commit 45)
//! identical: no   (b is a child of a)
//! SPACE       ONLY IN a   ONLY IN b   CHANGED
//! catalog             0          12         3
//! sequences           0           0         1
//! data (16384+)       0         100         7
//! pages: 120 shared, 0 exclusive to a, 14 exclusive to b
//! ```
//! (`SpaceDiff.space` names via Debug; counts from the report; sample keys listed below the table with
//! `-v`.) `truncated` -> note `difference limit reached; counts are lower bounds`. Exit 0 whether or
//! not they differ (use `--json` + `identical` for scripts). Unknown branch -> exit 5.
use pgarbor_common::PgError;
use crate::cli::{BranchCompareArgs, BranchResetArgs};
use crate::ctx::{CliError, CliResult, Ctx};
use crate::exit::Exit;

#[allow(unused_variables)]
pub fn reset(ctx: &mut Ctx, args: &BranchResetArgs) -> CliResult<Exit> {
    Err(CliError::from(PgError::unimplemented("CLI-20", "branch reset (branch::reset_compare::reset)")))
}

#[allow(unused_variables)]
pub fn compare(ctx: &mut Ctx, args: &BranchCompareArgs) -> CliResult<Exit> {
    Err(CliError::from(PgError::unimplemented("CLI-20", "branch compare (branch::reset_compare::compare)")))
}
