//! Named snapshots and reset (BRANCH-06). Lease BR-04.
//!
//! A named snapshot pins the branch's "postgres"-database root at a commit (cheap: 8 bytes in the
//! record). Reset replaces a branch's head roots with another committed root — destructive, so the
//! CLI requires confirmation/`--force`; this layer only offers the optional recovery snapshot.
//! Like `ops`, every function validates before writing and touches only the given directory view.
use crate::directory;
use crate::error;
use crate::name;
use crate::options::{BranchRef, ResetOptions, ResetTarget};
use crate::ops::OpCtx;
use crate::record::{BranchRecord, DatabaseRoot, NamedSnapshot, SnapshotKind, MAX_SNAPSHOTS_PER_BRANCH};
use pgarbor_common::{KvRead, KvWrite, PageId, PgError, Result, SqlState};

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
pub fn create_snapshot(dir: &mut dyn KvWrite, ctx: &OpCtx, branch: &BranchRef, label: &str) -> Result<NamedSnapshot> {
    let mut rec = directory::resolve(&*dir, branch)?;
    name::validate_snapshot_label(label)?;
    check_snapshot_slot(&rec, label)?;
    let snap = NamedSnapshot {
        label: label.to_string(),
        root: rec.data_root,
        created_seq: ctx.commit_seq,
        created_unix_ms: ctx.now_unix_ms,
        kind: SnapshotKind::User,
    };
    rec.snapshots.push(snap.clone());
    rec.updated_unix_ms = ctx.now_unix_ms;
    directory::store_record(dir, &rec)?;
    Ok(snap)
}

/// Remove a named snapshot (any kind). Absent → `error::snapshot_not_found`. Returns it.
/// Stamps `updated_unix_ms = now` on the record; generation unchanged.
pub fn drop_snapshot(dir: &mut dyn KvWrite, ctx: &OpCtx, branch: &BranchRef, label: &str) -> Result<NamedSnapshot> {
    let mut rec = directory::resolve(&*dir, branch)?;
    let Some(pos) = rec.snapshots.iter().position(|s| s.label == label) else {
        return Err(error::snapshot_not_found(&rec.name, label));
    };
    let removed = rec.snapshots.remove(pos);
    rec.updated_unix_ms = ctx.now_unix_ms;
    directory::store_record(dir, &rec)?;
    Ok(removed)
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
pub fn reset(
    dir: &mut dyn KvWrite,
    ctx: &OpCtx,
    branch: &BranchRef,
    target: &ResetTarget,
    opts: &ResetOptions,
) -> Result<ResetReport> {
    // Everything that can fail is evaluated before the first write.
    let mut rec = directory::resolve(&*dir, branch)?;
    let (new_root, new_extra) = resolve_target(&*dir, &rec, target)?;
    let recovery_label = match opts.recovery_snapshot.as_deref() {
        Some(label) => {
            name::validate_snapshot_label(label)?;
            check_snapshot_slot(&rec, label)?;
            Some(label)
        }
        None => None,
    };
    let generation = rec.generation.checked_add(1).ok_or_else(|| PgError::internal("branch generation overflow"))?;

    let previous_root = rec.data_root;
    if let Some(label) = recovery_label {
        rec.snapshots.push(NamedSnapshot {
            label: label.to_string(),
            root: previous_root,
            created_seq: ctx.commit_seq,
            created_unix_ms: ctx.now_unix_ms,
            kind: SnapshotKind::ResetRecovery,
        });
    }
    rec.data_root = new_root;
    if let Some(extra) = new_extra {
        rec.extra_databases = extra;
    }
    rec.head_seq = ctx.commit_seq;
    rec.generation = generation;
    rec.updated_unix_ms = ctx.now_unix_ms;

    directory::store_record(dir, &rec)?;
    Ok(ResetReport {
        branch: rec,
        previous_root,
        new_root,
        recovery_snapshot: recovery_label.map(str::to_string),
    })
}

/// Shared check for a snapshot label being added to `rec`: the label must be unused and the
/// per-branch limit must not be reached. The label itself is validated by the caller.
fn check_snapshot_slot(rec: &BranchRecord, label: &str) -> Result<()> {
    if rec.snapshot(label).is_some() {
        return Err(error::snapshot_exists(&rec.name, label));
    }
    if rec.snapshots.len() >= MAX_SNAPSHOTS_PER_BRANCH {
        return Err(error::too_many_snapshots(&rec.name, MAX_SNAPSHOTS_PER_BRANCH));
    }
    Ok(())
}

/// Root and (optionally) extra databases a reset takes from `target`. `None` extra databases
/// means "keep the branch's own". Read-only: called before any write.
fn resolve_target(
    dir: &dyn KvRead,
    rec: &BranchRecord,
    target: &ResetTarget,
) -> Result<(PageId, Option<Vec<DatabaseRoot>>)> {
    match target {
        ResetTarget::Snapshot(label) => {
            let snap = rec.snapshot(label).ok_or_else(|| error::snapshot_not_found(&rec.name, label))?;
            Ok((snap.root, None))
        }
        ResetTarget::Branch(other) => {
            let src = directory::resolve(dir, other)?;
            Ok((src.data_root, Some(src.extra_databases)))
        }
        ResetTarget::Parent => {
            let Some(parent_id) = rec.parent.as_ref() else {
                return Err(PgError::new(
                    SqlState::OBJECT_NOT_IN_PREREQUISITE_STATE,
                    format!("branch \"{}\" has no parent", rec.name),
                ));
            };
            match directory::load_record(dir, parent_id)? {
                Some(parent) if parent.is_active() => Ok((parent.data_root, Some(parent.extra_databases))),
                _ => Err(PgError::new(
                    SqlState::OBJECT_NOT_IN_PREREQUISITE_STATE,
                    format!("parent of branch \"{}\" has been deleted", rec.name),
                )),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ops::{self, OpCtx};
    use crate::options::{CreateOptions, CreateSource};
    use crate::record::BranchState;
    use pgarbor_common::{BranchId, CommitSeq, MemKv};

    fn ctx(seq: u64) -> OpCtx {
        OpCtx { commit_seq: CommitSeq(seq), now_unix_ms: 1_000 + seq }
    }

    /// Fresh directory with a `main` branch whose data root is `PageId(10)`.
    fn setup() -> (MemKv, BranchId) {
        let mut kv = MemKv::default();
        let rec = ops::initialize(&mut kv, &ctx(1), "main", PageId(10), None).unwrap();
        (kv, rec.id)
    }

    fn main_ref() -> BranchRef {
        BranchRef::name("main")
    }

    fn load(kv: &MemKv, id: &BranchId) -> BranchRecord {
        directory::load_record(kv, id).unwrap().unwrap()
    }

    /// Move `main`'s data root (database oid 5) to `root` without bumping the generation.
    fn set_main_root(kv: &mut MemKv, id: &BranchId, seq: u64, root: u64) {
        let generation = load(kv, id).generation;
        ops::advance_head(kv, &ctx(seq), id, generation, 5, PageId(root)).unwrap();
    }

    #[test]
    fn snapshot_then_reset_restores_root() {
        let (mut kv, id) = setup();
        let snap = create_snapshot(&mut kv, &ctx(2), &main_ref(), "s1").unwrap();
        assert_eq!(snap.root, PageId(10));
        assert_eq!(snap.created_seq, CommitSeq(2));
        assert_eq!(snap.kind, SnapshotKind::User);
        set_main_root(&mut kv, &id, 3, 20);
        let rep = reset(&mut kv, &ctx(4), &main_ref(), &ResetTarget::Snapshot("s1".into()), &ResetOptions::default())
            .unwrap();
        assert_eq!(rep.previous_root, PageId(20));
        assert_eq!(rep.new_root, PageId(10));
        assert_eq!(rep.recovery_snapshot, None);
        let after = load(&kv, &id);
        assert_eq!(after.data_root, PageId(10));
        assert_eq!(after.head_seq, CommitSeq(4));
        assert_eq!(after.generation, 2);
        assert_eq!(after.snapshots.len(), 1);
    }

    #[test]
    fn reset_with_recovery_keeps_old_root() {
        let (mut kv, id) = setup();
        create_snapshot(&mut kv, &ctx(2), &main_ref(), "s1").unwrap();
        set_main_root(&mut kv, &id, 3, 20);
        let opts = ResetOptions { recovery_snapshot: Some("pre-reset".into()) };
        let rep = reset(&mut kv, &ctx(4), &main_ref(), &ResetTarget::Snapshot("s1".into()), &opts).unwrap();
        assert_eq!(rep.recovery_snapshot.as_deref(), Some("pre-reset"));
        let after = load(&kv, &id);
        let rec_snap = after.snapshot("pre-reset").unwrap();
        assert_eq!(rec_snap.root, PageId(20));
        assert_eq!(rec_snap.kind, SnapshotKind::ResetRecovery);
        assert_eq!(rec_snap.created_seq, CommitSeq(4));
        assert_eq!(after.data_root, PageId(10));
        assert_eq!(after.snapshots.len(), 2);
    }

    #[test]
    fn duplicate_label_is_42710() {
        let (mut kv, id) = setup();
        create_snapshot(&mut kv, &ctx(2), &main_ref(), "s1").unwrap();
        let err = create_snapshot(&mut kv, &ctx(3), &main_ref(), "s1").unwrap_err();
        assert_eq!(err.sqlstate.code(), "42710");
        assert_eq!(err.message, "snapshot \"s1\" already exists on branch \"main\"");
        // A recovery label colliding with an existing snapshot fails the same way, unchanged.
        let before = kv.clone();
        let opts = ResetOptions { recovery_snapshot: Some("s1".into()) };
        let err = reset(&mut kv, &ctx(4), &main_ref(), &ResetTarget::Snapshot("s1".into()), &opts).unwrap_err();
        assert_eq!(err.sqlstate.code(), "42710");
        assert_eq!(kv.map, before.map);
        assert_eq!(load(&kv, &id).generation, 1);
    }

    #[test]
    fn invalid_label_is_42602() {
        let (mut kv, _) = setup();
        let err = create_snapshot(&mut kv, &ctx(2), &main_ref(), "bad label").unwrap_err();
        assert_eq!(err.sqlstate.code(), "42602");
    }

    #[test]
    fn drop_snapshot_removes_and_reports_absence() {
        let (mut kv, id) = setup();
        create_snapshot(&mut kv, &ctx(2), &main_ref(), "s1").unwrap();
        let removed = drop_snapshot(&mut kv, &ctx(3), &main_ref(), "s1").unwrap();
        assert_eq!(removed.label, "s1");
        assert!(load(&kv, &id).snapshots.is_empty());
        let err = drop_snapshot(&mut kv, &ctx(4), &main_ref(), "s1").unwrap_err();
        assert_eq!(err.sqlstate.code(), "42704");
        assert_eq!(err.message, "snapshot \"s1\" of branch \"main\" does not exist");
    }

    #[test]
    fn reset_to_parent() {
        let (mut kv, id) = setup();
        let feat = ops::create(&mut kv, &ctx(2), "feat", &CreateSource::branch("main"), &CreateOptions::default())
            .unwrap();
        set_main_root(&mut kv, &id, 3, 30);
        let rep = reset(&mut kv, &ctx(4), &BranchRef::name("feat"), &ResetTarget::Parent, &ResetOptions::default())
            .unwrap();
        assert_eq!(rep.previous_root, PageId(10));
        assert_eq!(rep.new_root, PageId(30));
        let after = load(&kv, &feat.id);
        assert_eq!(after.data_root, PageId(30));
        assert_eq!(after.parent, Some(id.clone()));
    }

    #[test]
    fn reset_to_parent_errors() {
        let (mut kv, id) = setup();
        let err = reset(&mut kv, &ctx(2), &main_ref(), &ResetTarget::Parent, &ResetOptions::default()).unwrap_err();
        assert_eq!(err.sqlstate.code(), "55000");
        assert_eq!(err.message, "branch \"main\" has no parent");

        ops::create(&mut kv, &ctx(3), "feat", &CreateSource::branch("main"), &CreateOptions::default()).unwrap();
        // Tombstone the parent directly (its name index is left alone; only `feat` is resolved).
        let mut parent = load(&kv, &id);
        parent.state = BranchState::Deleted;
        parent.roots_retained = true;
        parent.deleted_at_seq = CommitSeq(4);
        directory::store_record(&mut kv, &parent).unwrap();
        let before = kv.clone();
        let err = reset(&mut kv, &ctx(5), &BranchRef::name("feat"), &ResetTarget::Parent, &ResetOptions::default())
            .unwrap_err();
        assert_eq!(err.sqlstate.code(), "55000");
        assert_eq!(err.message, "parent of branch \"feat\" has been deleted");
        assert_eq!(kv.map, before.map);
    }

    #[test]
    fn reset_to_another_branch_copies_root() {
        let (mut kv, id) = setup();
        let other = ops::create(&mut kv, &ctx(2), "other", &CreateSource::branch("main"), &CreateOptions::default())
            .unwrap();
        set_main_root(&mut kv, &id, 3, 40);
        ops::advance_head(&mut kv, &ctx(4), &other.id, 1, 5, PageId(50)).unwrap();
        let rep = reset(
            &mut kv,
            &ctx(5),
            &main_ref(),
            &ResetTarget::Branch(BranchRef::name("other")),
            &ResetOptions::default(),
        )
        .unwrap();
        assert_eq!(rep.new_root, PageId(50));
        assert_eq!(load(&kv, &id).data_root, PageId(50));
        // Resetting to itself only bumps the generation.
        let rep = reset(
            &mut kv,
            &ctx(6),
            &main_ref(),
            &ResetTarget::Branch(BranchRef::Id(id.clone())),
            &ResetOptions::default(),
        )
        .unwrap();
        assert_eq!(rep.previous_root, rep.new_root);
        assert_eq!(load(&kv, &id).generation, 3);
    }

    #[test]
    fn snapshot_limit_is_54000() {
        let (mut kv, _) = setup();
        for i in 0..MAX_SNAPSHOTS_PER_BRANCH {
            create_snapshot(&mut kv, &ctx(2), &main_ref(), &format!("s{i}")).unwrap();
        }
        let err = create_snapshot(&mut kv, &ctx(3), &main_ref(), "one-too-many").unwrap_err();
        assert_eq!(err.sqlstate.code(), "54000");
        assert_eq!(err.detail.as_deref(), Some("The limit is 1024."));
        let before = kv.clone();
        let opts = ResetOptions { recovery_snapshot: Some("recovery".into()) };
        let err = reset(&mut kv, &ctx(4), &main_ref(), &ResetTarget::Snapshot("s0".into()), &opts).unwrap_err();
        assert_eq!(err.sqlstate.code(), "54000");
        assert_eq!(kv.map, before.map);
    }

    #[test]
    fn failed_reset_leaves_directory_unchanged() {
        let (mut kv, _) = setup();
        let before = kv.clone();
        let err = reset(
            &mut kv,
            &ctx(2),
            &main_ref(),
            &ResetTarget::Snapshot("missing".into()),
            &ResetOptions { recovery_snapshot: Some("rec".into()) },
        )
        .unwrap_err();
        assert_eq!(err.sqlstate.code(), "42704");
        assert_eq!(err.message, "snapshot \"missing\" of branch \"main\" does not exist");
        assert_eq!(kv.map, before.map);
    }

    #[test]
    fn reset_unknown_branch_is_42704() {
        let (mut kv, _) = setup();
        let err = reset(
            &mut kv,
            &ctx(2),
            &BranchRef::name("nope"),
            &ResetTarget::Parent,
            &ResetOptions::default(),
        )
        .unwrap_err();
        assert_eq!(err.sqlstate.code(), "42704");
    }
}
