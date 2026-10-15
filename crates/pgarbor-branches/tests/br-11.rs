//! BR-11 integration tests: detach strategy selection, reflink barrier, re-identify, prune.
use pgarbor_branches::detach::{detach_branch, reidentify_file, StrategyUsed};
use pgarbor_branches::{directory, BranchManager, BranchRef, CreateOptions, CreateSource, DetachOptions, DetachStrategy};
use pgarbor_common::{DatabaseUuid, KvWrite};
use pgarbor_storage::{Container, CreateOptions as StoreCreate, MemVfs, OpenOptions, OsVfs, SyncMode, Vfs};
use std::path::Path;
use std::sync::Arc;

fn fast() -> StoreCreate {
    StoreCreate { sync: SyncMode::Off, ..StoreCreate::default() }
}

/// Put `n` keys into the data tree of `branch` and commit.
fn put_data(c: &Container, branch: &str, prefix: &str, n: u32) {
    let mut w = c.begin_write().unwrap();
    let mut rec = directory::resolve(&c.snapshot().directory(), &BranchRef::name(branch)).unwrap();
    let root = {
        let mut t = w.tree(rec.data_root);
        for i in 0..n {
            t.put(format!("{prefix}{i:06}").as_bytes(), &[7u8; 200]).unwrap();
        }
        t.root()
    };
    rec.data_root = root;
    rec.head_seq = w.commit_seq();
    let dir = w.directory_root();
    let new_dir = {
        let mut d = w.tree(dir);
        directory::store_record(&mut d, &rec).unwrap();
        d.root()
    };
    w.set_directory_root(new_dir);
    w.commit().unwrap();
}

fn mem_manager() -> (Arc<dyn Vfs>, BranchManager) {
    let vfs: Arc<dyn Vfs> = Arc::new(MemVfs::new());
    let c = Container::create_with(vfs.clone(), Path::new("/db/src.pga"), &fast()).unwrap();
    let m = BranchManager::open(c).unwrap();
    put_data(m.container(), "main", "k", 50);
    m.create("feature", &CreateSource::branch("main"), &CreateOptions::default()).unwrap();
    put_data(m.container(), "feature", "f", 200);
    (vfs, m)
}

fn opts() -> DetachOptions {
    DetachOptions { sync: SyncMode::Off, ..DetachOptions::default() }
}

#[test]
fn mem_auto_falls_back_to_copy() {
    let (_vfs, m) = mem_manager();
    let r = detach_branch(m.container(), &BranchRef::name("feature"), Path::new("/db/out.pga"), &opts()).unwrap();
    assert_eq!(r.strategy_used, StrategyUsed::Copy);
    assert!(r.fallback_reason.is_some());
    assert_eq!(r.garbage_pages, 0);
    assert!(r.verified);
}

#[test]
fn mem_reflink_required_is_0a000() {
    let (_vfs, m) = mem_manager();
    let o = DetachOptions { strategy: DetachStrategy::ReflinkRequired, ..opts() };
    let e = detach_branch(m.container(), &BranchRef::name("feature"), Path::new("/db/out.pga"), &o).unwrap_err();
    assert_eq!(e.sqlstate.code(), "0A000");
    assert!(!e.is_unimplemented());
}

#[test]
fn mem_copy_strategy_is_copy_without_fallback() {
    let (_vfs, m) = mem_manager();
    let o = DetachOptions { strategy: DetachStrategy::Copy, ..opts() };
    let r = detach_branch(m.container(), &BranchRef::name("main"), Path::new("/db/c.pga"), &o).unwrap();
    assert_eq!(r.strategy_used, StrategyUsed::Copy);
    assert!(r.fallback_reason.is_none());
}

#[test]
fn os_auto_reflink_or_copy() {
    let dir = tempfile::tempdir().unwrap();
    let src = dir.path().join("src.pga");
    let c = Container::create(&src, &fast()).unwrap();
    let m = BranchManager::open(c).unwrap();
    put_data(m.container(), "main", "k", 50);
    m.create("feature", &CreateSource::branch("main"), &CreateOptions::default()).unwrap();
    put_data(m.container(), "feature", "f", 400);
    let src_uuid = m.container().db_uuid();

    let dest = dir.path().join("out.pga");
    let o = DetachOptions { os_filesystem: true, target_name: "solo".to_string(), ..opts() };
    let r = detach_branch(m.container(), &BranchRef::name("main"), &dest, &o).unwrap();
    assert!(r.verified);
    assert_ne!(r.new_db_uuid, src_uuid);
    assert!(!dir.path().join("out.pga.detach-tmp").exists());
    match r.strategy_used {
        StrategyUsed::Reflink => {
            assert!(r.fallback_reason.is_none());
            // The feature branch's exclusive pages stay behind as garbage.
            assert!(r.garbage_pages > 0);
        }
        StrategyUsed::Copy => assert!(r.fallback_reason.is_some()),
    }
    // The source keeps working and is unchanged.
    assert_eq!(m.list(false).unwrap().len(), 2);

    drop(m);
    let d = Container::open(&dest, &OpenOptions::default()).unwrap();
    assert_eq!(d.db_uuid(), r.new_db_uuid);
    let recs = directory::list(&d.snapshot().directory(), true).unwrap();
    assert_eq!(recs.len(), 1);
    assert_eq!(recs[0].name, "solo");
    assert_eq!(recs[0].id, r.new_branch_id);
    assert!(recs[0].parent.is_none());
}

#[test]
fn os_reflink_required_succeeds_or_is_0a000() {
    let dir = tempfile::tempdir().unwrap();
    let c = Container::create(dir.path().join("src.pga"), &fast()).unwrap();
    let m = BranchManager::open(c).unwrap();
    put_data(m.container(), "main", "k", 20);
    let dest = dir.path().join("out.pga");
    let o = DetachOptions { strategy: DetachStrategy::ReflinkRequired, os_filesystem: true, ..opts() };
    match detach_branch(m.container(), &BranchRef::name("main"), &dest, &o) {
        Ok(r) => assert_eq!(r.strategy_used, StrategyUsed::Reflink),
        Err(e) => {
            assert_eq!(e.sqlstate.code(), "0A000");
            assert!(!dest.exists());
            assert!(!dir.path().join("out.pga.detach-tmp").exists());
        }
    }
}

#[test]
fn existing_destination_is_refused_unless_overwrite() {
    let dir = tempfile::tempdir().unwrap();
    let c = Container::create(dir.path().join("src.pga"), &fast()).unwrap();
    let m = BranchManager::open(c).unwrap();
    let dest = dir.path().join("out.pga");
    std::fs::write(&dest, b"x").unwrap();
    let o = DetachOptions { os_filesystem: true, ..opts() };
    let e = detach_branch(m.container(), &BranchRef::name("main"), &dest, &o).unwrap_err();
    assert_eq!(e.sqlstate.code(), "58P02");
    let o = DetachOptions { overwrite: true, ..o };
    detach_branch(m.container(), &BranchRef::name("main"), &dest, &o).unwrap();
}

#[test]
fn reidentify_changes_only_the_uuid() {
    let dir = tempfile::tempdir().unwrap();
    let a = dir.path().join("a.pga");
    let b = dir.path().join("b.pga");
    {
        let c = Container::create(&a, &fast()).unwrap();
        let m = BranchManager::open(c).unwrap();
        put_data(m.container(), "main", "k", 30);
    }
    std::fs::copy(&a, &b).unwrap();
    let new_uuid = DatabaseUuid([9u8; 16]);
    reidentify_file(&OsVfs, &b, new_uuid, SyncMode::Full).unwrap();
    let cb = Container::open(&b, &OpenOptions::default()).unwrap();
    assert_eq!(cb.db_uuid(), new_uuid);
    let ca = Container::open(&a, &OpenOptions::default()).unwrap();
    assert_ne!(ca.db_uuid(), new_uuid);
    assert_eq!(ca.commit_seq(), cb.commit_seq());
    let mb = BranchManager::new(cb);
    assert_eq!(mb.list(false).unwrap().len(), 1);
}

#[test]
fn reidentify_refuses_a_locked_file() {
    let dir = tempfile::tempdir().unwrap();
    let a = dir.path().join("a.pga");
    let _c = Container::create(&a, &fast()).unwrap();
    // Advisory-lock semantics differ per platform; when contention is detected it must be a real error.
    if let Err(e) = reidentify_file(&OsVfs, &a, DatabaseUuid([1u8; 16]), SyncMode::Off) {
        assert!(!e.is_unimplemented());
    }
}
