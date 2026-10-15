use std::{io, path::Path, sync::{Arc, Mutex, atomic::{AtomicBool, Ordering}}};
use pgarbor_branches::{BranchManager, BranchRef, DetachOptions};
use pgarbor_storage::{Container, FileIo, LockMode, MemVfs, OpenMode, Vfs};

#[derive(Debug)]
struct InterleavingVfs {
    inner: MemVfs,
    source: Mutex<Option<Container>>,
    nested: AtomicBool,
    late_dest: bool,
}
impl Vfs for InterleavingVfs {
    fn open(&self, p: &Path, m: OpenMode) -> io::Result<Box<dyn FileIo>> { self.inner.open(p,m) }
    fn exists(&self, p: &Path) -> io::Result<bool> { self.inner.exists(p) }
    fn rename(&self, a: &Path, b: &Path) -> io::Result<()> { self.inner.rename(a,b) }
    fn rename_noreplace(&self, a: &Path, b: &Path) -> io::Result<()> {
        if b == Path::new("out.pga.detach-tmp") && self.nested.swap(false, Ordering::Relaxed) {
            let c = self.source.lock().unwrap().as_ref().unwrap().clone();
            let m = BranchManager::open(c).unwrap();
            let err = m.export(&BranchRef::name("main"), Path::new("out.pga"), &DetachOptions::default()).unwrap_err();
            assert_eq!(err.sqlstate.code(), "55006");
        }
        if b == Path::new("out.pga") {
            let probe = self.inner.open(a, OpenMode::ReadWrite)?;
            assert_eq!(probe.try_lock(LockMode::Exclusive).unwrap_err().kind(), io::ErrorKind::WouldBlock,
                "verified intermediate must remain locked during final publication");
        }
        if b == Path::new("out.pga") && self.late_dest {
            self.inner.write_bytes(b, b"late destination".to_vec());
        }
        self.inner.rename_noreplace(a,b)
    }
    fn remove_file(&self, p: &Path) -> io::Result<()> { self.inner.remove_file(p) }
    fn sync_dir(&self, p: &Path) -> io::Result<()> { self.inner.sync_dir(p) }
}
fn setup(nested: bool, late_dest: bool) -> (Arc<InterleavingVfs>, BranchManager) {
    let v = Arc::new(InterleavingVfs { inner: MemVfs::new(), source: Mutex::new(None), nested: AtomicBool::new(nested), late_dest });
    let c = Container::create_with(v.clone(), Path::new("source.pga"), &Default::default()).unwrap();
    let m = BranchManager::open(c.clone()).unwrap();
    *v.source.lock().unwrap() = Some(c);
    (v,m)
}

#[test]
fn overlapping_export_cannot_recycle_first_builders_intermediate() {
    let (v,m) = setup(true,false);
    let report = m.export(&BranchRef::name("main"), Path::new("out.pga"), &Default::default()).unwrap();
    assert!(!v.nested.load(Ordering::Relaxed));
    let out = Container::open_with(v.clone(), Path::new("out.pga"), &Default::default()).unwrap();
    assert_eq!(out.db_uuid(), report.new_db_uuid);
    assert_eq!(BranchManager::open(out).unwrap().list(false).unwrap().len(), 1);
    assert!(!v.exists(Path::new("out.pga.detach-guard")).unwrap());
}

#[test]
fn final_publication_never_clobbers_destination_created_at_rename() {
    let (v,m) = setup(false,true);
    let err = m.export(&BranchRef::name("main"), Path::new("out.pga"), &Default::default()).unwrap_err();
    assert_eq!(err.sqlstate.code(), "58P02");
    assert_eq!(v.inner.read_bytes(Path::new("out.pga")).unwrap(), b"late destination");
    assert!(!v.exists(Path::new("out.pga.detach-tmp")).unwrap());
    assert!(!v.exists(Path::new("out.pga.detach-guard")).unwrap());
}

#[test]
fn live_intermediate_is_preserved() {
    let (v,m) = setup(false,false);
    let temp = Path::new("out.pga.detach-tmp");
    let f = v.open(temp, OpenMode::CreateNew).unwrap();
    f.try_lock(LockMode::Exclusive).unwrap();
    f.write_all_at(0,b"live owner").unwrap();
    let err = m.export(&BranchRef::name("main"), Path::new("out.pga"), &Default::default()).unwrap_err();
    assert_eq!(err.sqlstate.code(), "55006");
    assert_eq!(v.inner.read_bytes(temp).unwrap(), b"live owner");
}

#[test]
fn stale_guard_and_intermediate_are_reclaimed_safely() {
    let (v,m) = setup(false,false);
    v.inner.write_bytes(Path::new("out.pga.detach-guard"), Vec::new());
    v.inner.write_bytes(Path::new("out.pga.detach-tmp"), b"old incomplete file".to_vec());
    let report = m.export(&BranchRef::name("main"), Path::new("out.pga"), &Default::default()).unwrap();
    assert!(report.verified);
    assert!(!v.exists(Path::new("out.pga.detach-guard")).unwrap());
    assert!(!v.exists(Path::new("out.pga.detach-tmp")).unwrap());
    Container::open_with(v, Path::new("out.pga"), &Default::default()).unwrap();
}
