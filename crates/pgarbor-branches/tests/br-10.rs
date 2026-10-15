//! BR-10 integration tests: copy-strategy standalone export (MemVfs, SyncMode::Off).
use pgarbor_branches::export::{DETACH_TEMP_SUFFIX, verify_and_publish};
use pgarbor_branches::ops;
use pgarbor_branches::{BranchManager, BranchRef, CreateOptions, CreateSource, DetachOptions, OpCtx, StrategyUsed};
use pgarbor_common::KvRead;
use pgarbor_storage::format::superblock::now_unix_ms;
use pgarbor_storage::io::sibling_path;
use pgarbor_storage::{Container, CreateOptions as StoCreate, MemVfs, OpenOptions, SyncMode, Vfs};
use std::path::Path;
use std::sync::Arc;

fn key(i: usize) -> Vec<u8> {
    format!("key{i:06}").into_bytes()
}
fn val(i: usize) -> Vec<u8> {
    format!("value-{i}").into_bytes()
}

fn new_manager(vfs: &Arc<MemVfs>, path: &str) -> BranchManager {
    let c = Container::create_with(
        vfs.clone(),
        Path::new(path),
        &StoCreate { sync: SyncMode::Off, ..StoCreate::default() },
    )
    .unwrap();
    BranchManager::open(c).unwrap()
}

/// Commit `range` of keys onto `branch` in one write transaction.
fn write_keys(mgr: &BranchManager, branch: &str, range: std::ops::Range<usize>) {
    let rec = mgr.get(&BranchRef::name(branch)).unwrap();
    let mut w = mgr.container().begin_write().unwrap();
    let mut root = rec.data_root;
    for i in range {
        root = w.put(root, &key(i), &val(i)).unwrap().root;
    }
    let ctx = OpCtx { commit_seq: w.commit_seq(), now_unix_ms: now_unix_ms() };
    let dir_root = w.directory_root();
    let new_dir = {
        let mut t = w.tree(dir_root);
        ops::advance_head(&mut t, &ctx, &rec.id, rec.generation, 5, root).unwrap();
        t.root()
    };
    w.set_directory_root(new_dir);
    w.commit().unwrap();
}

fn opts() -> DetachOptions {
    DetachOptions { sync: SyncMode::Off, ..DetachOptions::default() }
}

fn open_dest(vfs: &Arc<MemVfs>, dest: &str) -> BranchManager {
    let oo = OpenOptions { sync: SyncMode::Off, ..OpenOptions::default() };
    let c = Container::open_with(vfs.clone(), Path::new(dest), &oo).unwrap();
    BranchManager::open(c).unwrap()
}

fn seeded(vfs: &Arc<MemVfs>) -> BranchManager {
    let mgr = new_manager(vfs, "/src.pga");
    write_keys(&mgr, "main", 0..1000);
    mgr.create("feature", &CreateSource::branch("main"), &CreateOptions::default()).unwrap();
    write_keys(&mgr, "feature", 1000..1010);
    mgr
}

#[test]
fn export_roundtrip() {
    let vfs = Arc::new(MemVfs::new());
    let mgr = seeded(&vfs);
    let dest = Path::new("/out/feature.pga");
    let report = mgr.export(&BranchRef::name("feature"), dest, &opts()).unwrap();
    assert_eq!(report.strategy_used, StrategyUsed::Copy);
    assert!(report.verified);
    assert_eq!(report.garbage_pages, 0);
    assert!(!vfs.exists(&sibling_path(dest, DETACH_TEMP_SUFFIX)).unwrap());

    let out = open_dest(&vfs, "/out/feature.pga");
    assert_ne!(out.container().db_uuid(), mgr.container().db_uuid());
    assert_eq!(out.container().db_uuid(), report.new_db_uuid);
    let branches = out.list(true).unwrap();
    assert_eq!(branches.len(), 1);
    assert_eq!(branches[0].name, "main");
    assert!(branches[0].parent.is_none());
    assert_eq!(out.default_branch().unwrap().unwrap().id, branches[0].id);

    let snap = out.snapshot();
    let origin = pgarbor_branches::directory::origin(&snap.directory()).unwrap().unwrap();
    assert_eq!(origin.source_branch_name, "feature");
    assert_eq!(origin.strategy, "copy");
    assert_eq!(origin.source_db_uuid, mgr.container().db_uuid().to_hyphenated());

    let tree = snap.tree(branches[0].data_root).unwrap();
    for i in 0..1010 {
        assert_eq!(tree.get(&key(i)).unwrap(), Some(val(i)), "key {i}");
    }
    assert_eq!(tree.get(&key(1010)).unwrap(), None);

    let rep = out.verify(pgarbor_storage::VerifyLevel::Full).unwrap();
    assert!(rep.is_ok());
    assert!(snap.page_count() < mgr.snapshot().page_count());
}

#[test]
fn dest_exists_is_58p02_and_untouched() {
    let vfs = Arc::new(MemVfs::new());
    let mgr = seeded(&vfs);
    let dest = Path::new("/out/feature.pga");
    vfs.write_bytes(dest, b"keep me".to_vec());
    let e = mgr.export(&BranchRef::name("feature"), dest, &opts()).unwrap_err();
    assert_eq!(e.sqlstate.0, *b"58P02");
    assert_eq!(vfs.read_bytes(dest).unwrap(), b"keep me".to_vec());
    assert!(!vfs.exists(&sibling_path(dest, DETACH_TEMP_SUFFIX)).unwrap());

    let o = DetachOptions { overwrite: true, ..opts() };
    mgr.export(&BranchRef::name("feature"), dest, &o).unwrap();
    assert_ne!(vfs.read_bytes(dest).unwrap(), b"keep me".to_vec());
}

#[test]
fn stale_temp_is_removed() {
    let vfs = Arc::new(MemVfs::new());
    let mgr = seeded(&vfs);
    let dest = Path::new("/out/feature.pga");
    let tmp = sibling_path(dest, DETACH_TEMP_SUFFIX);
    vfs.write_bytes(&tmp, vec![9; 123]);
    mgr.export(&BranchRef::name("feature"), dest, &opts()).unwrap();
    assert!(!vfs.exists(&tmp).unwrap());
    assert!(vfs.exists(dest).unwrap());
}

#[test]
fn invalid_target_name_and_missing_branch_leave_nothing() {
    let vfs = Arc::new(MemVfs::new());
    let mgr = seeded(&vfs);
    let dest = Path::new("/out/x.pga");
    let o = DetachOptions { target_name: "bad name".into(), ..opts() };
    let e = mgr.export(&BranchRef::name("feature"), dest, &o).unwrap_err();
    assert_eq!(e.sqlstate.0, *b"42602");
    let e = mgr.export(&BranchRef::name("nope"), dest, &opts()).unwrap_err();
    assert_eq!(e.sqlstate.0, *b"42704");
    assert!(!vfs.exists(dest).unwrap());
    assert!(vfs.paths().iter().all(|p| !p.to_string_lossy().contains("tmp")));
}

#[test]
fn verify_failure_removes_temp_and_leaves_dest() {
    let vfs = Arc::new(MemVfs::new());
    let mgr = seeded(&vfs);
    let dest = Path::new("/out/x.pga");
    let tmp = sibling_path(dest, DETACH_TEMP_SUFFIX);
    vfs.write_bytes(&tmp, vec![0; 100]);
    let dyn_vfs: Arc<dyn Vfs> = vfs.clone();
    let rec = mgr.get(&BranchRef::name("feature")).unwrap();
    assert!(verify_and_publish(&dyn_vfs, &tmp, dest, &opts(), &rec).is_err());
    assert!(!vfs.exists(&tmp).unwrap());
    assert!(!vfs.exists(dest).unwrap());
}

#[test]
fn snapshots_are_carried_as_inherited_when_requested() {
    let vfs = Arc::new(MemVfs::new());
    let mgr = seeded(&vfs);
    mgr.create_snapshot(&BranchRef::name("feature"), "before").unwrap();
    write_keys(&mgr, "feature", 1010..1020);
    let o = DetachOptions { include_snapshots: true, ..opts() };
    mgr.export(&BranchRef::name("feature"), Path::new("/out/s.pga"), &o).unwrap();
    let out = open_dest(&vfs, "/out/s.pga");
    let rec = out.get(&BranchRef::name("main")).unwrap();
    assert_eq!(rec.snapshots.len(), 1);
    assert_eq!(rec.snapshots[0].label, "before");
    assert_eq!(rec.snapshots[0].kind, pgarbor_branches::SnapshotKind::Inherited);
    let snap = out.snapshot();
    let t = snap.tree(rec.snapshots[0].root).unwrap();
    assert_eq!(t.get(&key(1009)).unwrap(), Some(val(1009)));
    assert_eq!(t.get(&key(1015)).unwrap(), None);
}

#[test]
fn source_keeps_committing_during_export() {
    let vfs = Arc::new(MemVfs::new());
    let mgr = seeded(&vfs);
    let writer = mgr.clone();
    let handle = std::thread::spawn(move || {
        for i in 0..20 {
            write_keys(&writer, "feature", (2000 + i * 10)..(2010 + i * 10));
        }
    });
    let report = mgr.export(&BranchRef::name("feature"), Path::new("/out/live.pga"), &opts()).unwrap();
    handle.join().unwrap();
    assert!(report.verified);
    let out = open_dest(&vfs, "/out/live.pga");
    let rec = out.get(&BranchRef::name("main")).unwrap();
    let snap = out.snapshot();
    let tree = snap.tree(rec.data_root).unwrap();
    // The export is one consistent snapshot: the original 1010 keys are always present.
    for i in 0..1010 {
        assert_eq!(tree.get(&key(i)).unwrap(), Some(val(i)));
    }
    assert!(out.verify(pgarbor_storage::VerifyLevel::Full).unwrap().is_ok());
}
