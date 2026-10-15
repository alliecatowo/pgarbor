//! FLT-10: interrupted compaction / export and lock tests (STORE-12, STORE-16, STORE-17).
//!
//! Lease FLT-10 writes this file. Required tests:
//! 1. `crash_during_compaction_every_op`: workload, then `compact()`; crash at every mutating op
//!    inside the compaction window (count ops before/after to find the window) → recovery yields
//!    the pre-compaction acked state (or the compacted equivalent: identical logical contents and
//!    commit_seq + 1); `<path>.compact-tmp` is removed by the next writable open.
//! 2. `rename_not_durable_without_dir_sync`: DropUnsynced crash right after the rename but before
//!    `sync_dir` → the old file is at the path and opens with the acked state.
//! 3. `export_crash_leaves_dest_absent_or_complete`: crash at every op of an ExportBuilder run →
//!    the destination either does not exist or opens with exactly the exported snapshot.
//! 4. `old_snapshot_survives_compaction`: a snapshot taken before compaction keeps returning its
//!    data afterwards (FaultVfs keeps the unlinked file object alive).
//! 5. `second_open_locked`: opening the same FaultVfs path twice → 55006.

use pgarbor_common::{CommitSeq, DatabaseUuid, KvRead, PageId, PgError, Result};
use pgarbor_fault::{
    read_state, CrashMode, DiskImage, Executor, FaultPlan, FaultVfs, OpKind, RefModel, RootsState, StepOutcome,
    TreeState, WorkOp, WorkloadSpec, ROOT_PREFIX,
};
use pgarbor_storage::{
    Container, ExportBuilder, ExportOptions, ExportReport, OpenOptions, PrefixU64Roots, Snapshot, VerifyLevel, Vfs,
};
use std::ops::Bound;
use std::path::PathBuf;
use std::sync::Arc;

const DB: &str = "/db/main.pga";
const DEST: &str = "/db/export.pga";

fn db_path() -> PathBuf {
    PathBuf::from(DB)
}

fn spec() -> WorkloadSpec {
    WorkloadSpec::small(7)
}

fn put(root: &str, i: u32, len: usize) -> WorkOp {
    WorkOp::Put {
        root: root.to_string(),
        key: format!("k{:06}", i).into_bytes(),
        value: vec![(i % 251) as u8 + 1; len],
    }
}

fn del(root: &str, i: u32) -> WorkOp {
    WorkOp::Delete { root: root.to_string(), key: format!("k{:06}", i).into_bytes() }
}

/// A fixed workload that leaves plenty of garbage pages behind (overwrites, deletes, overflow
/// values, a branch copy) so compaction has real work to do.
fn work_ops() -> Vec<WorkOp> {
    let mut v = vec![WorkOp::CreateRoot { name: "main".to_string(), from: None }];
    for i in 0..80u32 {
        v.push(put("main", i, 1 + (i as usize * 37) % 200));
    }
    for i in 0..4u32 {
        v.push(put("main", 1000 + i, 6000));
    }
    v.push(WorkOp::Commit);
    for i in (0..40u32).step_by(2) {
        v.push(del("main", i));
    }
    for i in 40..60u32 {
        v.push(put("main", i, 300 + i as usize));
    }
    v.push(WorkOp::Commit);
    v.push(WorkOp::CreateRoot { name: "feature".to_string(), from: Some("main".to_string()) });
    for i in 0..10u32 {
        v.push(put("feature", 2000 + i, 50));
    }
    v.push(WorkOp::Commit);
    v
}

fn io_err(ctx: &str, e: std::io::Error) -> PgError {
    PgError::io(ctx, &e)
}

fn expect_ok(out: StepOutcome, what: &str) -> Result<()> {
    match out {
        StepOutcome::Done | StepOutcome::CommitAcked(_) => Ok(()),
        other => Err(PgError::internal(format!("{what}: unexpected step outcome {other:?}"))),
    }
}

/// Fresh simulated disk + executor with the workload applied (no compaction yet).
fn prepare(plan: FaultPlan) -> Result<(FaultVfs, Executor)> {
    let vfs = FaultVfs::new(plan);
    let probe_vfs = vfs.clone();
    let probe: Arc<dyn Fn() -> bool + Send + Sync> = Arc::new(move || probe_vfs.controller().crashed());
    let dyn_vfs: Arc<dyn Vfs> = Arc::new(vfs.clone());
    let mut ex = Executor::create(dyn_vfs, db_path(), &spec().create_options(), probe)?;
    for op in work_ops() {
        let out = ex.step(&op)?;
        expect_ok(out, "workload")?;
    }
    Ok((vfs, ex))
}

fn plan_with(seed: u64, crash_at: Option<u64>, mode: CrashMode) -> FaultPlan {
    FaultPlan { seed, crash_at_op: crash_at, crash_mode: mode, ..FaultPlan::default() }
}

fn recover(img: &DiskImage, verify_full: bool) -> Result<(Container, FaultVfs)> {
    let rv = FaultVfs::from_image(img, FaultPlan::none(0));
    let dyn_vfs: Arc<dyn Vfs> = Arc::new(rv.clone());
    let mut oo: OpenOptions = spec().open_options();
    if verify_full {
        oo.verify = VerifyLevel::Full;
    }
    let c = Container::open_with(dyn_vfs, &db_path(), &oo)?;
    Ok((c, rv))
}

fn verify_clean(c: &Container, what: &str) -> Result<()> {
    let rep = c.verify(&PrefixU64Roots::new(ROOT_PREFIX), VerifyLevel::Full)?;
    if rep.is_ok() {
        Ok(())
    } else {
        Err(PgError::internal(format!("{what}: doctor reported errors: {:?}", rep.issues)))
    }
}

/// Mutating-op indexes that bound the compaction step in a fault-free run.
#[derive(Debug, Clone, Copy)]
struct Window {
    /// First mutating op of `compact()`.
    start: u64,
    /// One past the last mutating op of `compact()`.
    end: u64,
    /// Mutating index of the rename over the database path.
    rename: u64,
    /// Mutating index of the first `sync_dir` after the rename.
    sync_dir: u64,
}

fn compaction_window() -> Result<Window> {
    let plan = FaultPlan { record_log: true, ..FaultPlan::none(1) };
    let (vfs, mut ex) = prepare(plan)?;
    let start = vfs.controller().mutating_ops();
    expect_ok(ex.step(&WorkOp::Compact)?, "compact")?;
    let end = vfs.controller().mutating_ops();
    let log = vfs.controller().log();
    let rename = log
        .iter()
        .filter(|r| r.kind == OpKind::Rename)
        .filter_map(|r| r.mutating_index)
        .find(|m| *m >= start)
        .ok_or_else(|| PgError::internal("compaction performed no rename"))?;
    let sync_dir = log
        .iter()
        .filter(|r| r.kind == OpKind::SyncDir)
        .filter_map(|r| r.mutating_index)
        .find(|m| *m > rename)
        .ok_or_else(|| PgError::internal("compaction performed no sync_dir after the rename"))?;
    if end <= start {
        return Err(PgError::internal("compaction performed no mutating ops"));
    }
    Ok(Window { start, end, rename, sync_dir })
}

struct CompactionCrash {
    model: RefModel,
    seq: CommitSeq,
    state: RootsState,
    temp_left: bool,
}

/// Run the workload, crash at mutating op `k` (inside `compact()`), recover from the crash image
/// and return what was found.
fn crash_compaction(k: u64, mode: CrashMode) -> Result<CompactionCrash> {
    let (vfs, mut ex) = prepare(plan_with(k, Some(k), mode))?;
    if vfs.controller().crashed() {
        return Err(PgError::internal(format!("crash at op {k} fired before compaction started")));
    }
    let out = ex.step(&WorkOp::Compact)?;
    assert!(
        matches!(out, StepOutcome::Crashed(_)),
        "crash at op {k} mode {mode:?}: compaction should have observed the crash, got {out:?}"
    );
    assert!(vfs.controller().crashed(), "crash at op {k}: controller not crashed");
    let model = ex.model.clone();
    drop(ex);
    let img = vfs.crash_image()?;
    let (c, rv) = recover(&img, true)?;
    let (state, seq) = read_state(&c, ROOT_PREFIX)?;
    verify_clean(&c, &format!("crash at op {k} mode {mode:?}"))?;
    let temp = format!("{DB}.compact-tmp");
    let temp_left = rv.exists(std::path::Path::new(&temp)).map_err(|e| io_err("exists", e))?;
    Ok(CompactionCrash { model, seq, state, temp_left })
}

fn check_compaction_crash(k: u64, mode: CrashMode, r: &CompactionCrash) {
    let s = r.model.acked_seq.0;
    assert_eq!(r.state, r.model.acked, "crash at op {k} mode {mode:?}: recovered logical state differs");
    assert!(
        r.seq.0 == s || r.seq.0 == s + 1,
        "crash at op {k} mode {mode:?}: recovered commit_seq {} is neither {s} (old) nor {} (compacted)",
        r.seq.0,
        s + 1
    );
    assert!(!r.temp_left, "crash at op {k} mode {mode:?}: .compact-tmp survived a writable open");
}

#[test]
fn crash_during_compaction_every_op() -> Result<()> {
    let w = compaction_window()?;
    let modes = [
        CrashMode::DropUnsynced,
        CrashMode::KeepUnsynced,
        CrashMode::Random { keep_permille: 500, tear_permille: 200 },
    ];
    for mode in modes {
        for k in w.start..w.end {
            let r = crash_compaction(k, mode)?;
            check_compaction_crash(k, mode, &r);
            let s = r.model.acked_seq.0;
            match mode {
                CrashMode::DropUnsynced => {
                    // The rename is only durable after the directory sync, which never completed.
                    if k <= w.sync_dir {
                        assert_eq!(r.seq.0, s, "crash at op {k} DropUnsynced: rename must not be durable");
                    }
                }
                CrashMode::KeepUnsynced => {
                    if k <= w.rename {
                        assert_eq!(r.seq.0, s, "crash at op {k} KeepUnsynced: rename had not happened yet");
                    } else {
                        assert_eq!(r.seq.0, s + 1, "crash at op {k} KeepUnsynced: rename was already live");
                    }
                }
                CrashMode::Random { .. } => {}
            }
        }
    }
    Ok(())
}

#[test]
fn rename_not_durable_without_dir_sync() -> Result<()> {
    let w = compaction_window()?;
    // Crash at the directory sync that would have made the rename durable.
    let k = w.sync_dir;
    let r = crash_compaction(k, CrashMode::DropUnsynced)?;
    check_compaction_crash(k, CrashMode::DropUnsynced, &r);
    assert_eq!(r.seq, r.model.acked_seq, "old file must be at the path after a lost rename");
    assert_eq!(r.state, r.model.acked);

    // Same crash point but the directory ops survive: the compacted file is at the path.
    let r = crash_compaction(k, CrashMode::KeepUnsynced)?;
    check_compaction_crash(k, CrashMode::KeepUnsynced, &r);
    assert_eq!(r.seq.0, r.model.acked_seq.0 + 1, "compacted file must be at the path when the rename survived");
    Ok(())
}

fn export_options() -> ExportOptions {
    ExportOptions {
        page_size: spec().page_size,
        db_uuid: Some(DatabaseUuid([0x11; 16])),
        created_unix_ms: Some(0),
        // Small buffer so the export flushes several times (more crash points).
        buffer_bytes: 16 << 10,
        ..ExportOptions::default()
    }
}

/// Export the whole source database (directory + every root it references) to `DEST`.
fn run_export(c: &Container) -> Result<ExportReport> {
    let vfs = c.vfs().clone();
    let snap = c.snapshot();
    let mut b = ExportBuilder::create(vfs, std::path::Path::new(DEST), &export_options())?;
    let dir = b.copy_directory(&snap, snap.directory_root(), &PrefixU64Roots::new(ROOT_PREFIX))?;
    b.finish(dir)
}

fn source_container(ex: &Executor) -> Result<Container> {
    ex.container.clone().ok_or_else(|| PgError::internal("executor has no open container"))
}

#[test]
fn export_crash_leaves_dest_absent_or_complete() -> Result<()> {
    // Fault-free run: find the window and check the happy path.
    let (start, end) = {
        let (vfs, ex) = prepare(FaultPlan::none(1))?;
        let c = source_container(&ex)?;
        let start = vfs.controller().mutating_ops();
        let rep = run_export(&c)?;
        let end = vfs.controller().mutating_ops();
        assert_eq!(rep.path, PathBuf::from(DEST));
        assert!(vfs.exists(std::path::Path::new(DEST)).map_err(|e| io_err("exists", e))?);
        assert!(
            !vfs.exists(std::path::Path::new(&format!("{DEST}.export-tmp"))).map_err(|e| io_err("exists", e))?,
            "export temp file must be gone after a successful export"
        );
        assert!(end > start, "export performed no mutating ops");
        (start, end)
    };

    let modes = [
        CrashMode::DropUnsynced,
        CrashMode::KeepUnsynced,
        CrashMode::Random { keep_permille: 500, tear_permille: 200 },
    ];
    for mode in modes {
        for k in start..end {
            let (vfs, ex) = prepare(plan_with(k, Some(k), mode))?;
            let c = source_container(&ex)?;
            let res = run_export(&c);
            assert!(res.is_err(), "crash at op {k} mode {mode:?}: export reported success despite the crash");
            assert!(vfs.controller().crashed(), "crash at op {k}: controller not crashed");
            let acked = ex.model.acked.clone();
            let acked_seq = ex.model.acked_seq;
            drop(c);
            drop(ex);
            let img = vfs.crash_image()?;

            // The source database is unaffected by an interrupted export.
            let (src, _rv) = recover(&img, true)?;
            let (src_state, src_seq) = read_state(&src, ROOT_PREFIX)?;
            assert_eq!(src_state, acked, "crash at op {k} mode {mode:?}: source state changed");
            assert_eq!(src_seq, acked_seq, "crash at op {k} mode {mode:?}: source commit_seq changed");
            drop(src);

            // The destination is either absent or exactly the exported snapshot.
            let dest = PathBuf::from(DEST);
            if img.files.contains_key(&dest) {
                let rv = FaultVfs::from_image(&img, FaultPlan::none(0));
                let dyn_vfs: Arc<dyn Vfs> = Arc::new(rv);
                let mut oo = spec().open_options();
                oo.verify = VerifyLevel::Full;
                let d = Container::open_with(dyn_vfs, &dest, &oo).map_err(|e| {
                    PgError::internal(format!(
                        "crash at op {k} mode {mode:?}: destination exists but does not open: {e:?}"
                    ))
                })?;
                let (d_state, d_seq) = read_state(&d, ROOT_PREFIX)?;
                assert_eq!(d_state, acked, "crash at op {k} mode {mode:?}: destination is a partial export");
                assert_eq!(d_seq, CommitSeq(1), "crash at op {k} mode {mode:?}: destination commit_seq");
                verify_clean(&d, &format!("export dest, crash at op {k} mode {mode:?}"))?;
                assert_eq!(d.db_uuid(), DatabaseUuid([0x11; 16]));
            }
        }
    }
    Ok(())
}

fn snapshot_state(snap: &Snapshot) -> Result<RootsState> {
    let dir = snap.directory();
    let mut out = RootsState::new();
    for item in dir.scan_prefix(ROOT_PREFIX)? {
        let (k, v) = item?;
        let b: [u8; 8] = v
            .as_slice()
            .try_into()
            .map_err(|_| PgError::internal(format!("root reference has {} bytes, expected 8", v.len())))?;
        let root = PageId(u64::from_le_bytes(b));
        let name = String::from_utf8_lossy(&k[ROOT_PREFIX.len()..]).into_owned();
        let tree = snap.tree(root)?;
        let mut t = TreeState::new();
        for kv in tree.scan(Bound::Unbounded, Bound::Unbounded)? {
            let (key, value) = kv?;
            t.insert(key, value);
        }
        out.insert(name, t);
    }
    Ok(out)
}

#[test]
fn old_snapshot_survives_compaction() -> Result<()> {
    let (_vfs, mut ex) = prepare(FaultPlan::none(3))?;
    let c = source_container(&ex)?;
    let snap = c.snapshot();
    let seq0 = snap.commit_seq();
    let before = snapshot_state(&snap)?;
    assert!(!before.is_empty() && before.values().any(|t| !t.is_empty()), "workload produced no data");
    assert_eq!(before, ex.model.acked);

    expect_ok(ex.step(&WorkOp::Compact)?, "compact")?;
    assert_eq!(c.commit_seq(), CommitSeq(seq0.0 + 1), "compaction is a commit");
    assert_eq!(snap.commit_seq(), seq0);
    let after = snapshot_state(&snap)?;
    assert_eq!(after, before, "old snapshot changed after compaction");
    let (live, _) = read_state(&c, ROOT_PREFIX)?;
    assert_eq!(live, before, "compaction changed the logical contents");

    // The container keeps committing on the new file while the old snapshot is still alive.
    expect_ok(ex.step(&put("main", 5000, 77))?, "put after compact")?;
    expect_ok(ex.step(&WorkOp::Commit)?, "commit after compact")?;
    let after2 = snapshot_state(&snap)?;
    assert_eq!(after2, before, "old snapshot changed after a post-compaction commit");
    let (live2, _) = read_state(&c, ROOT_PREFIX)?;
    assert_eq!(live2, ex.model.acked);
    assert_ne!(live2, before);
    Ok(())
}

#[test]
fn second_open_locked() -> Result<()> {
    let vfs = FaultVfs::new(FaultPlan::none(5));
    let dyn_vfs: Arc<dyn Vfs> = Arc::new(vfs.clone());
    let path = db_path();
    let first = Container::create_with(dyn_vfs.clone(), &path, &spec().create_options())?;

    match Container::open_with(dyn_vfs.clone(), &path, &spec().open_options()) {
        Ok(_) => panic!("second writable open of a locked database succeeded"),
        Err(e) => assert_eq!(e.sqlstate.code(), "55006", "unexpected error: {e:?}"),
    }
    let mut ro = spec().open_options();
    ro.read_only = true;
    match Container::open_with(dyn_vfs.clone(), &path, &ro) {
        Ok(_) => panic!("read-only open of an exclusively locked database succeeded"),
        Err(e) => assert_eq!(e.sqlstate.code(), "55006", "unexpected error: {e:?}"),
    }

    // Releasing the first handle releases the lock.
    first.close()?;
    let again = Container::open_with(dyn_vfs, &path, &spec().open_options())?;
    drop(again);
    Ok(())
}
