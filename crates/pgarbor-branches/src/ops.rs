//! Branch lifecycle mutations over the directory (BRANCH-01..05). Lease BR-03.
//!
//! Every function mutates ONLY the directory view it is given (`&mut dyn KvWrite`) and is
//! therefore atomic with whatever storage commit the caller performs (`BranchManager` does one
//! commit per operation; pgarbor-core composes `advance_head` into its data commits). Functions
//! validate everything BEFORE their first write, so an `Err` leaves the view unchanged (callers
//! also drop the uncommitted `WriteTxn` on error, so partial writes never become durable).
//!
//! All mutating functions stamp `ctx.commit_seq` / `ctx.now_unix_ms` into the records they touch.
use crate::directory;
use crate::error;
use crate::options::{BranchRef, CreateOptions, CreateSource, DeleteOptions, PurgeFilter};
use crate::record::BranchRecord;
use pgarbor_common::{BranchId, CommitSeq, KvWrite, Oid, PageId, PgError, Result};

// CONTRACT
/// Context of the commit an operation will be part of.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct OpCtx {
    /// `WriteTxn::commit_seq()` of the enclosing storage write (the seq this commit will get).
    pub commit_seq: CommitSeq,
    pub now_unix_ms: u64,
}

/// Fresh random branch id (UUIDv4 via the `uuid` crate: `uuid::Uuid::new_v4().into_bytes()`).
pub fn new_branch_id() -> BranchId {
    BranchId(uuid::Uuid::new_v4().into_bytes())
}

/// Initialize an EMPTY directory (BRANCH-01): writes `Mformat`, the first branch `name` (id =
/// `id.unwrap_or_else(new_branch_id)`, parent None, `data_root` as given — `PageId::NONE` for a
/// new database), its `N` entry and `Mdefault_branch` = its id. Returns the record.
/// Errors: `directory::check_format(dir)` is not `Empty` → 55000
/// `database file already has a branch directory`; invalid name → 42602.
#[allow(unused_variables)]
pub fn initialize(
    dir: &mut dyn KvWrite,
    ctx: &OpCtx,
    name: &str,
    data_root: PageId,
    id: Option<BranchId>,
) -> Result<BranchRecord> {
    Err(PgError::unimplemented("BR-03", "branch directory initialization"))
}

/// Create a branch (BRANCH-02): O(1) — the new record copies root pointers, no page is copied.
/// Steps: validate `name` (42602); `N`+name exists → `error::branch_exists` (42710); resolve the
/// source (`directory::resolve`, active only):
/// - `CreateSource::Branch(b)` → `data_root` and `extra_databases` = source's; parent = source id;
///   if `opts.copy_snapshots`, copy the source's snapshots with kind `Inherited` (labels kept).
/// - `CreateSource::Snapshot{branch, label}` → `data_root` = that snapshot's root (absent →
///   `error::snapshot_not_found(branch name, label)`); `extra_databases` = source's current
///   ones (v1 has none); parent = branch id; `copy_snapshots` copies as above.
/// Record = `BranchRecord::new_active(id, name, Some(parent), root, ctx.commit_seq, ctx.now_unix_ms)`
/// (`created_at_seq == head_seq == ctx.commit_seq`); `id = opts.id.unwrap_or_else(new_branch_id)`
/// (an existing `B`+id → 42710 `branch with id <id> already exists`). Writes `B` and `N`.
/// The default branch is unchanged.
#[allow(unused_variables)]
pub fn create(
    dir: &mut dyn KvWrite,
    ctx: &OpCtx,
    name: &str,
    source: &CreateSource,
    opts: &CreateOptions,
) -> Result<BranchRecord> {
    Err(PgError::unimplemented("BR-03", "branch create"))
}

/// Rename (BRANCH-04): the id is unchanged, so `.pgabranch` handles and sessions stay valid.
/// Validate `new_name` (42602); same name → no-op returning the record (no write);
/// `N`+new_name exists → 42710; else delete `N`+old, put `N`+new, store record with the new
/// name and `updated_unix_ms = ctx.now_unix_ms` (generation unchanged).
#[allow(unused_variables)]
pub fn rename(dir: &mut dyn KvWrite, ctx: &OpCtx, branch: &BranchRef, new_name: &str) -> Result<BranchRecord> {
    Err(PgError::unimplemented("BR-03", "branch rename"))
}

/// Logical delete (BRANCH-05). Resolve (active only). If it is the default branch and
/// `!opts.allow_default` → `error::cannot_delete_default(name)`; with `allow_default` the
/// `Mdefault_branch` key is removed. Then: remove `N`+name; record.state = Deleted,
/// `deleted_at_seq = ctx.commit_seq`, `deleted_unix_ms = ctx.now_unix_ms`, `generation += 1`,
/// `updated_unix_ms = now`; if `opts.retain` → `roots_retained = true` (roots kept, restorable);
/// else `roots_retained = false`, `data_root = NONE`, `snapshots` and `extra_databases` cleared.
/// Returns the tombstone. Active-session checks are pgarbor-core's job (Engine::delete_branch).
///
/// Reclaim is deferred: pages become unreachable only once no record references them, and are
/// returned to the file system by the next `Container::compact` (which never disturbs open
/// snapshots: they keep reading the pre-compaction file).
#[allow(unused_variables)]
pub fn delete(dir: &mut dyn KvWrite, ctx: &OpCtx, branch: &BranchRef, opts: &DeleteOptions) -> Result<BranchRecord> {
    Err(PgError::unimplemented("BR-03", "branch delete"))
}

/// Restore a deleted branch whose roots were retained. Load `B`+id (absent → 42704
/// `branch_id_not_found`; Active → `error::branch_not_deleted(name)`; `!roots_retained` →
/// `error::branch_purged(name)`). Name = `new_name.unwrap_or(record.name)`, validated; taken →
/// 42710. Record becomes Active: `deleted_at_seq = 0`, `deleted_unix_ms = 0`,
/// `roots_retained = false`, `generation += 1`, `updated_unix_ms = now`; write `B` and `N`.
#[allow(unused_variables)]
pub fn restore(dir: &mut dyn KvWrite, ctx: &OpCtx, id: &BranchId, new_name: Option<&str>) -> Result<BranchRecord> {
    Err(PgError::unimplemented("BR-03", "branch restore"))
}

/// Remove tombstones matching `filter` entirely (their `B` keys); returns removed ids in
/// ascending id order. `PurgeFilter::Id` of an active branch → `error::branch_not_deleted`;
/// of an unknown id → 42704. Their pages are reclaimed by the next compaction (BRANCH-12).
#[allow(unused_variables)]
pub fn purge_deleted(dir: &mut dyn KvWrite, ctx: &OpCtx, filter: &PurgeFilter) -> Result<Vec<BranchId>> {
    Err(PgError::unimplemented("BR-03", "deleted branch purge"))
}

/// Make an active branch the default (`Mdefault_branch`). Returns its record (unchanged).
#[allow(unused_variables)]
pub fn set_default(dir: &mut dyn KvWrite, ctx: &OpCtx, branch: &BranchRef) -> Result<BranchRecord> {
    Err(PgError::unimplemented("BR-03", "default branch change"))
}

/// Publish a new head for one database of a branch — the directory half of a data commit
/// (pgarbor-core commit, sequence/id mini-commits). Load `B`+id; absent → 42704; Deleted →
/// `error::branch_deleted`; `record.generation != expected_generation` →
/// `error::branch_generation_mismatch(name)`; `set_database_root(db_oid, new_root)` false →
/// 3D000 `database with OID <oid> does not exist in branch "<name>"`. Then `head_seq =
/// ctx.commit_seq`, `updated_unix_ms = now`, store. Generation is NOT bumped (ordinary commits
/// never invalidate other transactions' snapshots).
#[allow(unused_variables)]
pub fn advance_head(
    dir: &mut dyn KvWrite,
    ctx: &OpCtx,
    id: &BranchId,
    expected_generation: u64,
    db_oid: Oid,
    new_root: PageId,
) -> Result<BranchRecord> {
    Err(PgError::unimplemented("BR-03", "branch head advance"))
}

/// Re-exported for callers building error messages.
pub fn exists_error(name: &str) -> PgError {
    error::branch_exists(name)
}

/// Re-exported for callers that already hold a directory reader.
pub fn resolve_active(dir: &dyn pgarbor_common::KvRead, b: &BranchRef) -> Result<BranchRecord> {
    directory::resolve(dir, b)
}
