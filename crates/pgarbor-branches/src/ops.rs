//! Branch lifecycle mutations over the directory (BRANCH-01..05). Lease BR-03.
//!
//! Every function mutates ONLY the directory view it is given (`&mut dyn KvWrite`) and is
//! therefore atomic with whatever storage commit the caller performs (`BranchManager` does one
//! commit per operation; pgarbor-core composes `advance_head` into its data commits). Functions
//! validate everything BEFORE their first write, so an `Err` leaves the view unchanged (callers
//! also drop the uncommitted `WriteTxn` on error, so partial writes never become durable).
//!
//! All mutating functions stamp `ctx.commit_seq` / `ctx.now_unix_ms` into the records they touch.
use crate::directory::{self, DirectoryStatus};
use crate::error;
use crate::keys;
use crate::name;
use crate::options::{BranchRef, CreateOptions, CreateSource, DeleteOptions, PurgeFilter};
use crate::record::{BranchRecord, BranchState, NamedSnapshot, SnapshotKind, MAX_SNAPSHOTS_PER_BRANCH};
use pgarbor_common::{BranchId, CommitSeq, KvWrite, Oid, PageId, PgError, Result, SqlState};

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

/// Check that a record is structurally encodable BEFORE any write happens. Every record built in
/// this module goes through here, so a violated invariant surfaces as an error, not a half-write.
fn precheck(rec: &BranchRecord) -> Result<()> {
    match rec.validate() {
        None => Ok(()),
        Some(msg) => Err(PgError::internal(format!("invalid branch record: {msg}"))),
    }
}

/// Error for a `B` record that is required to be absent but is present.
fn branch_id_exists(id: &BranchId) -> PgError {
    PgError::new(SqlState::DUPLICATE_OBJECT, format!("branch with id {id} already exists"))
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
    match directory::check_format(&*dir)? {
        DirectoryStatus::Empty => {}
        DirectoryStatus::Ready => {
            return Err(PgError::new(
                SqlState::OBJECT_NOT_IN_PREREQUISITE_STATE,
                "database file already has a branch directory",
            ))
        }
    }
    name::validate_branch_name(name)?;
    let id = id.unwrap_or_else(new_branch_id);
    let rec = BranchRecord::new_active(id, name, None, data_root, ctx.commit_seq, ctx.now_unix_ms);
    precheck(&rec)?;
    directory::store_format(dir)?;
    directory::store_record(dir, &rec)?;
    directory::store_name(dir, name, &id)?;
    directory::store_default_branch(dir, Some(&id))?;
    Ok(rec)
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
    name::validate_branch_name(name)?;
    if directory::lookup_name(&*dir, name)?.is_some() {
        return Err(error::branch_exists(name));
    }
    let (src, data_root) = match source {
        CreateSource::Branch(b) => {
            let src = directory::resolve(&*dir, b)?;
            let root = src.data_root;
            (src, root)
        }
        CreateSource::Snapshot { branch, label } => {
            let src = directory::resolve(&*dir, branch)?;
            let root = match src.snapshot(label) {
                Some(s) => s.root,
                None => return Err(error::snapshot_not_found(&src.name, label)),
            };
            (src, root)
        }
    };
    let id = opts.id.unwrap_or_else(new_branch_id);
    if directory::load_record(&*dir, &id)?.is_some() {
        return Err(branch_id_exists(&id));
    }
    let mut rec = BranchRecord::new_active(id, name, Some(src.id), data_root, ctx.commit_seq, ctx.now_unix_ms);
    rec.extra_databases = src.extra_databases.clone();
    if opts.copy_snapshots {
        if src.snapshots.len() > MAX_SNAPSHOTS_PER_BRANCH {
            return Err(error::too_many_snapshots(name, MAX_SNAPSHOTS_PER_BRANCH));
        }
        rec.snapshots = src
            .snapshots
            .iter()
            .map(|s| NamedSnapshot { kind: SnapshotKind::Inherited, ..s.clone() })
            .collect();
    }
    precheck(&rec)?;
    directory::store_record(dir, &rec)?;
    directory::store_name(dir, name, &id)?;
    Ok(rec)
}

/// Rename (BRANCH-04): the id is unchanged, so `.pgabranch` handles and sessions stay valid.
/// Validate `new_name` (42602); same name → no-op returning the record (no write);
/// `N`+new_name exists → 42710; else delete `N`+old, put `N`+new, store record with the new
/// name and `updated_unix_ms = ctx.now_unix_ms` (generation unchanged).
#[allow(unused_variables)]
pub fn rename(dir: &mut dyn KvWrite, ctx: &OpCtx, branch: &BranchRef, new_name: &str) -> Result<BranchRecord> {
    name::validate_branch_name(new_name)?;
    let mut rec = directory::resolve(&*dir, branch)?;
    if rec.name == new_name {
        return Ok(rec);
    }
    if directory::lookup_name(&*dir, new_name)?.is_some() {
        return Err(error::branch_exists(new_name));
    }
    let old_name = std::mem::replace(&mut rec.name, new_name.to_string());
    rec.updated_unix_ms = ctx.now_unix_ms;
    precheck(&rec)?;
    directory::remove_name(dir, &old_name)?;
    directory::store_name(dir, new_name, &rec.id)?;
    directory::store_record(dir, &rec)?;
    Ok(rec)
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
    let mut rec = directory::resolve(&*dir, branch)?;
    let is_default = directory::default_branch(&*dir)? == Some(rec.id);
    if is_default && !opts.allow_default {
        return Err(error::cannot_delete_default(&rec.name));
    }
    let name = rec.name.clone();
    rec.state = BranchState::Deleted;
    rec.deleted_at_seq = ctx.commit_seq;
    rec.deleted_unix_ms = ctx.now_unix_ms;
    rec.generation += 1;
    rec.updated_unix_ms = ctx.now_unix_ms;
    if opts.retain {
        rec.roots_retained = true;
    } else {
        rec.roots_retained = false;
        rec.data_root = PageId::NONE;
        rec.snapshots.clear();
        rec.extra_databases.clear();
    }
    precheck(&rec)?;
    directory::remove_name(dir, &name)?;
    if is_default {
        directory::store_default_branch(dir, None)?;
    }
    directory::store_record(dir, &rec)?;
    Ok(rec)
}

/// Restore a deleted branch whose roots were retained. Load `B`+id (absent → 42704
/// `branch_id_not_found`; Active → `error::branch_not_deleted(name)`; `!roots_retained` →
/// `error::branch_purged(name)`). Name = `new_name.unwrap_or(record.name)`, validated; taken →
/// 42710. Record becomes Active: `deleted_at_seq = 0`, `deleted_unix_ms = 0`,
/// `roots_retained = false`, `generation += 1`, `updated_unix_ms = now`; write `B` and `N`.
#[allow(unused_variables)]
pub fn restore(dir: &mut dyn KvWrite, ctx: &OpCtx, id: &BranchId, new_name: Option<&str>) -> Result<BranchRecord> {
    let mut rec = directory::load_record(&*dir, id)?.ok_or_else(|| error::branch_id_not_found(id))?;
    if rec.is_active() {
        return Err(error::branch_not_deleted(&rec.name));
    }
    if !rec.roots_retained {
        return Err(error::branch_purged(&rec.name));
    }
    let name = match new_name {
        Some(n) => n.to_string(),
        None => rec.name.clone(),
    };
    name::validate_branch_name(&name)?;
    if directory::lookup_name(&*dir, &name)?.is_some() {
        return Err(error::branch_exists(&name));
    }
    rec.name = name.clone();
    rec.state = BranchState::Active;
    rec.deleted_at_seq = CommitSeq(0);
    rec.deleted_unix_ms = 0;
    rec.roots_retained = false;
    rec.generation += 1;
    rec.updated_unix_ms = ctx.now_unix_ms;
    precheck(&rec)?;
    directory::store_record(dir, &rec)?;
    directory::store_name(dir, &name, id)?;
    Ok(rec)
}

/// Remove tombstones matching `filter` entirely (their `B` keys); returns removed ids in
/// ascending id order. `PurgeFilter::Id` of an active branch → `error::branch_not_deleted`;
/// of an unknown id → 42704. Their pages are reclaimed by the next compaction (BRANCH-12).
#[allow(unused_variables)]
pub fn purge_deleted(dir: &mut dyn KvWrite, ctx: &OpCtx, filter: &PurgeFilter) -> Result<Vec<BranchId>> {
    let mut ids: Vec<BranchId> = match filter {
        PurgeFilter::Id(id) => {
            let rec = directory::load_record(&*dir, id)?.ok_or_else(|| error::branch_id_not_found(id))?;
            if rec.is_active() {
                return Err(error::branch_not_deleted(&rec.name));
            }
            vec![*id]
        }
        PurgeFilter::All => directory::list(&*dir, true)?
            .into_iter()
            .filter(|r| !r.is_active())
            .map(|r| r.id)
            .collect(),
        PurgeFilter::DeletedBefore(seq) => directory::list(&*dir, true)?
            .into_iter()
            .filter(|r| !r.is_active() && r.deleted_at_seq < *seq)
            .map(|r| r.id)
            .collect(),
    };
    ids.sort();
    ids.dedup();
    for id in &ids {
        dir.delete(&keys::branch_key(id))?;
    }
    Ok(ids)
}

/// Make an active branch the default (`Mdefault_branch`). Returns its record (unchanged).
#[allow(unused_variables)]
pub fn set_default(dir: &mut dyn KvWrite, ctx: &OpCtx, branch: &BranchRef) -> Result<BranchRecord> {
    let rec = directory::resolve(&*dir, branch)?;
    directory::store_default_branch(dir, Some(&rec.id))?;
    Ok(rec)
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
    let mut rec = directory::load_record(&*dir, id)?.ok_or_else(|| error::branch_id_not_found(id))?;
    if !rec.is_active() {
        return Err(error::branch_deleted(&rec.name, id));
    }
    if rec.generation != expected_generation {
        return Err(error::branch_generation_mismatch(&rec.name));
    }
    if !rec.set_database_root(db_oid, new_root) {
        return Err(PgError::new(
            SqlState::INVALID_CATALOG_NAME,
            format!("database with OID {db_oid} does not exist in branch \"{}\"", rec.name),
        ));
    }
    rec.head_seq = ctx.commit_seq;
    rec.updated_unix_ms = ctx.now_unix_ms;
    precheck(&rec)?;
    directory::store_record(dir, &rec)?;
    Ok(rec)
}

/// Re-exported for callers building error messages.
pub fn exists_error(name: &str) -> PgError {
    error::branch_exists(name)
}

/// Re-exported for callers that already hold a directory reader.
pub fn resolve_active(dir: &dyn pgarbor_common::KvRead, b: &BranchRef) -> Result<BranchRecord> {
    directory::resolve(dir, b)
}

#[cfg(test)]
mod tests {
    use super::*;
    use pgarbor_common::MemKv;
    use std::collections::BTreeMap;

    fn ctx(seq: u64) -> OpCtx {
        OpCtx { commit_seq: CommitSeq(seq), now_unix_ms: 1_000 + seq }
    }

    fn code(e: &PgError) -> &str {
        e.sqlstate.code()
    }

    fn snap(kv: &MemKv) -> BTreeMap<Vec<u8>, Vec<u8>> {
        kv.map.clone()
    }

    fn init_main(kv: &mut MemKv) -> BranchRecord {
        initialize(kv, &ctx(1), "main", PageId(7), None).expect("initialize")
    }

    #[test]
    fn initialize_twice_is_55000() {
        let mut kv = MemKv::default();
        let main = init_main(&mut kv);
        assert_eq!(main.generation, 1);
        assert_eq!(directory::default_branch(&kv).unwrap(), Some(main.id));
        let before = snap(&kv);
        let err = initialize(&mut kv, &ctx(2), "other", PageId::NONE, None).unwrap_err();
        assert_eq!(code(&err), "55000");
        assert_eq!(snap(&kv), before);
    }

    #[test]
    fn initialize_rejects_invalid_name_without_writing() {
        let mut kv = MemKv::default();
        let err = initialize(&mut kv, &ctx(1), "-bad", PageId::NONE, None).unwrap_err();
        assert_eq!(code(&err), "42602");
        assert!(kv.map.is_empty());
    }

    #[test]
    fn create_from_main_copies_root_and_sets_parent() {
        let mut kv = MemKv::default();
        let main = init_main(&mut kv);
        let feat = create(&mut kv, &ctx(2), "feature", &CreateSource::branch("main"), &CreateOptions::default())
            .expect("create");
        assert_eq!(feat.data_root, PageId(7));
        assert_eq!(feat.parent, Some(main.id));
        assert_eq!(feat.created_at_seq, CommitSeq(2));
        assert_eq!(feat.head_seq, CommitSeq(2));
        assert_eq!(directory::default_branch(&kv).unwrap(), Some(main.id));
        assert_eq!(directory::lookup_name(&kv, "feature").unwrap(), Some(feat.id));
    }

    #[test]
    fn create_duplicate_name_is_42710() {
        let mut kv = MemKv::default();
        init_main(&mut kv);
        let err = create(&mut kv, &ctx(2), "main", &CreateSource::branch("main"), &CreateOptions::default())
            .unwrap_err();
        assert_eq!(code(&err), "42710");
    }

    #[test]
    fn create_invalid_name_is_42602() {
        let mut kv = MemKv::default();
        init_main(&mut kv);
        let before = snap(&kv);
        let err = create(&mut kv, &ctx(2), "a b", &CreateSource::branch("main"), &CreateOptions::default())
            .unwrap_err();
        assert_eq!(code(&err), "42602");
        assert_eq!(snap(&kv), before);
    }

    #[test]
    fn create_from_missing_snapshot_is_42704() {
        let mut kv = MemKv::default();
        init_main(&mut kv);
        let src = CreateSource::Snapshot { branch: BranchRef::name("main"), label: "nope".into() };
        let before = snap(&kv);
        let err = create(&mut kv, &ctx(2), "f", &src, &CreateOptions::default()).unwrap_err();
        assert_eq!(code(&err), "42704");
        assert_eq!(snap(&kv), before);
    }

    #[test]
    fn rename_keeps_id_and_generation() {
        let mut kv = MemKv::default();
        init_main(&mut kv);
        let feat = create(&mut kv, &ctx(2), "feature", &CreateSource::branch("main"), &CreateOptions::default())
            .unwrap();
        let renamed = rename(&mut kv, &ctx(3), &BranchRef::name("feature"), "renamed").unwrap();
        assert_eq!(renamed.id, feat.id);
        assert_eq!(renamed.generation, feat.generation);
        assert_eq!(renamed.name, "renamed");
        assert_eq!(directory::lookup_name(&kv, "feature").unwrap(), None);
        assert_eq!(directory::lookup_name(&kv, "renamed").unwrap(), Some(feat.id));
    }

    #[test]
    fn rename_to_existing_is_42710() {
        let mut kv = MemKv::default();
        init_main(&mut kv);
        create(&mut kv, &ctx(2), "feature", &CreateSource::branch("main"), &CreateOptions::default()).unwrap();
        let before = snap(&kv);
        let err = rename(&mut kv, &ctx(3), &BranchRef::name("feature"), "main").unwrap_err();
        assert_eq!(code(&err), "42710");
        assert_eq!(snap(&kv), before);
    }

    #[test]
    fn delete_default_is_55000() {
        let mut kv = MemKv::default();
        init_main(&mut kv);
        let before = snap(&kv);
        let err = delete(&mut kv, &ctx(2), &BranchRef::name("main"), &DeleteOptions::default()).unwrap_err();
        assert_eq!(code(&err), "55000");
        assert_eq!(snap(&kv), before);
    }

    #[test]
    fn delete_retain_keeps_roots_and_frees_name() {
        let mut kv = MemKv::default();
        init_main(&mut kv);
        let feat = create(&mut kv, &ctx(2), "feature", &CreateSource::branch("main"), &CreateOptions::default())
            .unwrap();
        let tomb = delete(&mut kv, &ctx(3), &BranchRef::name("feature"), &DeleteOptions::default()).unwrap();
        assert_eq!(tomb.state, BranchState::Deleted);
        assert!(tomb.roots_retained);
        assert_eq!(tomb.generation, 2);
        assert!(!tomb.live_roots().is_empty());
        assert_eq!(directory::lookup_name(&kv, "feature").unwrap(), None);
        let again = create(&mut kv, &ctx(4), "feature", &CreateSource::branch("main"), &CreateOptions::default())
            .unwrap();
        assert_ne!(again.id, feat.id);
    }

    #[test]
    fn restore_after_retain_bumps_generation() {
        let mut kv = MemKv::default();
        init_main(&mut kv);
        let feat = create(&mut kv, &ctx(2), "feature", &CreateSource::branch("main"), &CreateOptions::default())
            .unwrap();
        delete(&mut kv, &ctx(3), &BranchRef::name("feature"), &DeleteOptions::default()).unwrap();
        let back = restore(&mut kv, &ctx(4), &feat.id, None).unwrap();
        assert!(back.is_active());
        assert_eq!(back.generation, 3);
        assert!(!back.roots_retained);
        assert_eq!(back.deleted_at_seq, CommitSeq(0));
        assert_eq!(directory::lookup_name(&kv, "feature").unwrap(), Some(feat.id));
    }

    #[test]
    fn restore_after_drop_roots_is_55000() {
        let mut kv = MemKv::default();
        init_main(&mut kv);
        let feat = create(&mut kv, &ctx(2), "feature", &CreateSource::branch("main"), &CreateOptions::default())
            .unwrap();
        let opts = DeleteOptions { retain: false, allow_default: false };
        delete(&mut kv, &ctx(3), &BranchRef::name("feature"), &opts).unwrap();
        let before = snap(&kv);
        let err = restore(&mut kv, &ctx(4), &feat.id, None).unwrap_err();
        assert_eq!(code(&err), "55000");
        assert_eq!(snap(&kv), before);
    }

    #[test]
    fn purge_removes_branch_key() {
        let mut kv = MemKv::default();
        init_main(&mut kv);
        let feat = create(&mut kv, &ctx(2), "feature", &CreateSource::branch("main"), &CreateOptions::default())
            .unwrap();
        delete(&mut kv, &ctx(3), &BranchRef::name("feature"), &DeleteOptions::default()).unwrap();
        let removed = purge_deleted(&mut kv, &ctx(4), &PurgeFilter::Id(feat.id)).unwrap();
        assert_eq!(removed, vec![feat.id]);
        assert!(kv.map.get(&keys::branch_key(&feat.id)).is_none());
    }

    #[test]
    fn purge_active_is_55000() {
        let mut kv = MemKv::default();
        let main = init_main(&mut kv);
        let before = snap(&kv);
        let err = purge_deleted(&mut kv, &ctx(2), &PurgeFilter::Id(main.id)).unwrap_err();
        assert_eq!(code(&err), "55000");
        assert_eq!(snap(&kv), before);
    }

    #[test]
    fn advance_head_stale_generation_is_55000() {
        let mut kv = MemKv::default();
        let main = init_main(&mut kv);
        let before = snap(&kv);
        let err = advance_head(&mut kv, &ctx(2), &main.id, 99, 5, PageId(9)).unwrap_err();
        assert_eq!(code(&err), "55000");
        assert_eq!(snap(&kv), before);
    }

    #[test]
    fn advance_head_updates_head_and_root() {
        let mut kv = MemKv::default();
        let main = init_main(&mut kv);
        let rec = advance_head(&mut kv, &ctx(5), &main.id, 1, 5, PageId(9)).unwrap();
        assert_eq!(rec.head_seq, CommitSeq(5));
        assert_eq!(rec.data_root, PageId(9));
        assert_eq!(rec.generation, 1);
        let stored = directory::load_record(&kv, &main.id).unwrap().unwrap();
        assert_eq!(stored.data_root, PageId(9));
    }

    #[test]
    fn advance_head_missing_database_is_3d000() {
        let mut kv = MemKv::default();
        let main = init_main(&mut kv);
        let before = snap(&kv);
        let err = advance_head(&mut kv, &ctx(2), &main.id, 1, 42, PageId(9)).unwrap_err();
        assert_eq!(code(&err), "3D000");
        assert_eq!(snap(&kv), before);
    }
}
