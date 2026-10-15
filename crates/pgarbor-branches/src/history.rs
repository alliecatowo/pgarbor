//! Named snapshots and reset (BRANCH-06). Lease BR-04.
//!
//! A named snapshot pins the branch's "postgres"-database root at a commit (cheap: 8 bytes in the
//! record). Reset replaces a branch's head roots with another committed root — destructive, so the
//! CLI requires confirmation/`--force`; this layer only offers the optional recovery snapshot.
//! Like `ops`, every function validates before writing and touches only the given directory view.
use crate::options::{BranchRef, ResetOptions, ResetTarget};
use crate::ops::OpCtx;
use crate::record::{BranchRecord, NamedSnapshot};
use pgarbor_common::{KvWrite, PageId, PgError, Result};

// CONTRACT
/// Outcome of a reset.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize)]
pub struct ResetReport {
    pub branch: BranchRecord,
    pub previous_root: PageId,
    pub new_root: PageId,
    /// Label of the recovery snapshot that now holds `previous_root`, if requested.
    pub recovery_snapshot: Option<String>,
}

/// Snapshot the branch's current `data_root` under `label` (kind `User`).
/// Errors: resolve (active only); invalid label → 42602 `invalid snapshot name`; label exists on
/// the branch → `error::snapshot_exists(branch, label)`; `snapshots.len() >=
/// MAX_SNAPSHOTS_PER_BRANCH` → `error::too_many_snapshots`. Appends
/// `NamedSnapshot { label, root: data_root, created_seq: ctx.commit_seq, created_unix_ms: now,
/// kind: User }`, `updated_unix_ms = now`, stores the record. Generation unchanged.
#[allow(unused_variables)]
pub fn create_snapshot(dir: &mut dyn KvWrite, ctx: &OpCtx, branch: &BranchRef, label: &str) -> Result<NamedSnapshot> {
    Err(PgError::unimplemented("BR-04", "named snapshot create"))
}

/// Remove a named snapshot (any kind). Absent → `error::snapshot_not_found`. Returns it.
#[allow(unused_variables)]
pub fn drop_snapshot(dir: &mut dyn KvWrite, ctx: &OpCtx, branch: &BranchRef, label: &str) -> Result<NamedSnapshot> {
    Err(PgError::unimplemented("BR-04", "named snapshot drop"))
}

/// Reset `branch` to `target`:
/// - `Snapshot(label)` → that snapshot of the SAME branch (absent → `snapshot_not_found`);
///   extra databases unchanged.
/// - `Branch(other)` → other's current `data_root` and `extra_databases` (resolving `other` to
///   the same branch is allowed and only bumps the generation).
/// - `Parent` → parent's current head; no parent / parent deleted → 55000
///   `branch "<name>" has no parent` / `parent of branch "<name>" has been deleted`.
/// If `opts.recovery_snapshot = Some(l)`: validate `l`, it must not already exist
/// (`snapshot_exists`), limit check, and append `NamedSnapshot{label: l, root: previous data_root,
/// kind: ResetRecovery, created_seq: ctx.commit_seq, ..}` BEFORE switching roots.
/// Then: `data_root = new_root`, `head_seq = ctx.commit_seq`, `generation += 1`,
/// `updated_unix_ms = now`; store. Parent pointer is unchanged.
/// Active-session checks and in-memory cache invalidation (sequences, id allocators) are done by
/// pgarbor-core's `Engine::reset_branch` around this call.
#[allow(unused_variables)]
pub fn reset(
    dir: &mut dyn KvWrite,
    ctx: &OpCtx,
    branch: &BranchRef,
    target: &ResetTarget,
    opts: &ResetOptions,
) -> Result<ResetReport> {
    Err(PgError::unimplemented("BR-04", "branch reset"))
}
