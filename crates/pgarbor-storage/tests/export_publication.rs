use std::{path::Path, sync::Arc};
use pgarbor_common::{KvRead, PageId};
use pgarbor_storage::{Container, ExportBuilder, ExportOptions, MemVfs, OpenOptions, Vfs};

#[test]
fn overlapping_builder_cannot_remove_or_publish_anothers_file() {
    let vfs = Arc::new(MemVfs::new());
    let dest = Path::new("out.pga");
    let mut a = ExportBuilder::create(vfs.clone(), dest, &ExportOptions::default()).unwrap();
    let root = a.put(PageId::NONE, b"owner", b"A").unwrap();
    let error = ExportBuilder::create(vfs.clone(), dest, &ExportOptions::default()).unwrap_err();
    assert_eq!(error.sqlstate.code(), "58P02");
    a.finish(root).unwrap();
    let opened = Container::open_with(vfs.clone(), dest, &OpenOptions::default()).unwrap();
    assert_eq!(opened.snapshot().directory().get(b"owner").unwrap(), Some(b"A".to_vec()));
}

#[test]
fn destination_created_after_builder_start_is_never_replaced() {
    let vfs = Arc::new(MemVfs::new());
    let dest = Path::new("out.pga");
    let a = ExportBuilder::create(vfs.clone(), dest, &ExportOptions::default()).unwrap();
    let other = Container::create_with(vfs.clone(), dest, &Default::default()).unwrap();
    let uuid = other.db_uuid();
    drop(other);
    let error = a.finish(PageId::NONE).unwrap_err();
    assert_eq!(error.sqlstate.code(), "58P02");
    assert_eq!(Container::open_with(vfs.clone(), dest, &Default::default()).unwrap().db_uuid(), uuid);
    assert!(!vfs.exists(Path::new("out.pga.export-tmp")).unwrap());
}

#[test]
fn failed_contender_does_not_affect_owner_drop_or_abort() {
    for abort in [false, true] {
        let vfs = Arc::new(MemVfs::new());
        let dest = Path::new("out.pga");
        let a = ExportBuilder::create(vfs.clone(), dest, &Default::default()).unwrap();
        assert!(ExportBuilder::create(vfs.clone(), dest, &Default::default()).is_err());
        assert!(vfs.exists(Path::new("out.pga.export-tmp")).unwrap());
        if abort { a.abort().unwrap(); } else { drop(a); }
        let b = ExportBuilder::create(vfs.clone(), dest, &Default::default()).unwrap();
        b.finish(PageId::NONE).unwrap();
        Container::open_with(vfs, dest, &Default::default()).unwrap();
    }
}

#[test]
fn recovery_preserves_live_export_temporary() {
    let vfs = Arc::new(MemVfs::new());
    let dest = Path::new("out.pga");
    let a = ExportBuilder::create(vfs.clone(), dest, &ExportOptions { overwrite: true, ..Default::default() }).unwrap();
    drop(Container::create_with(vfs.clone(), dest, &Default::default()).unwrap());
    drop(Container::open_with(vfs.clone(), dest, &Default::default()).unwrap());
    assert!(vfs.exists(Path::new("out.pga.export-tmp")).unwrap());
    a.finish(PageId::NONE).unwrap();
    Container::open_with(vfs, dest, &Default::default()).unwrap();
}

#[test]
fn os_publication_refuses_late_destination() {
    let temp = tempfile::tempdir().unwrap();
    let dest = temp.path().join("out.pga");
    let a = ExportBuilder::create(Arc::new(pgarbor_storage::OsVfs), &dest, &Default::default()).unwrap();
    std::fs::write(&dest, b"original").unwrap();
    assert_eq!(a.finish(PageId::NONE).unwrap_err().sqlstate.code(), "58P02");
    assert_eq!(std::fs::read(&dest).unwrap(), b"original");
}

#[derive(Debug)]
struct CheckCleanupLock { inner: MemVfs }
impl Vfs for CheckCleanupLock {
    fn open(&self, p: &Path, mode: pgarbor_storage::OpenMode) -> std::io::Result<Box<dyn pgarbor_storage::FileIo>> {
        self.inner.open(p, mode)
    }
    fn exists(&self, p: &Path) -> std::io::Result<bool> { self.inner.exists(p) }
    fn rename(&self, a: &Path, b: &Path) -> std::io::Result<()> { self.inner.rename(a,b) }
    fn rename_noreplace(&self, a: &Path, b: &Path) -> std::io::Result<()> { self.inner.rename_noreplace(a,b) }
    fn remove_file(&self, p: &Path) -> std::io::Result<()> {
        let contender = self.inner.open(p, pgarbor_storage::OpenMode::ReadWrite)?;
        assert_eq!(contender.try_lock(pgarbor_storage::LockMode::Exclusive).unwrap_err().kind(), std::io::ErrorKind::WouldBlock,
            "temporary cleanup must retain its lock until unlink completes");
        self.inner.remove_file(p)
    }
    fn sync_dir(&self, p: &Path) -> std::io::Result<()> { self.inner.sync_dir(p) }
}

#[test]
fn cleanup_holds_ownership_until_unlink_for_drop_abort_and_failed_publish() {
    let vfs = Arc::new(CheckCleanupLock { inner: MemVfs::new() });
    let dest = Path::new("out.pga");
    drop(ExportBuilder::create(vfs.clone(), dest, &Default::default()).unwrap());
    ExportBuilder::create(vfs.clone(), dest, &Default::default()).unwrap().abort().unwrap();
    let b = ExportBuilder::create(vfs.clone(), dest, &Default::default()).unwrap();
    vfs.inner.write_bytes(dest, b"other owner".to_vec());
    assert_eq!(b.finish(PageId::NONE).unwrap_err().sqlstate.code(), "58P02");
    assert_eq!(vfs.inner.read_bytes(dest).unwrap(), b"other owner");
}
