use std::{io, path::Path, sync::{Arc, Mutex, atomic::{AtomicUsize, Ordering}}};
use pgarbor_common::KvRead;
use pgarbor_storage::{Container, FileIo, LockMode, MemVfs, NoNestedRoots, OpenMode, Vfs};

/// Pause an opener after obtaining the old fd and publish a real compaction before
/// it locks that fd. This reproduces the old-inode race without scheduler timing.
#[derive(Debug)]
struct CompactDuringOpen {
    inner: Arc<MemVfs>,
    owner: Mutex<Option<Container>>,
    opens: AtomicUsize,
}
impl Vfs for CompactDuringOpen {
    fn open(&self, path: &Path, mode: OpenMode) -> io::Result<Box<dyn FileIo>> {
        let file = self.inner.open(path, mode)?;
        if mode == OpenMode::ReadWrite && path == Path::new("db.pga") {
            self.opens.fetch_add(1, Ordering::Relaxed);
            if let Some(owner) = self.owner.lock().unwrap().take() {
                owner.compact(&NoNestedRoots, &Default::default()).unwrap();
                drop(owner);
            }
        }
        Ok(file)
    }
    fn exists(&self, path: &Path) -> io::Result<bool> { self.inner.exists(path) }
    fn rename(&self, from: &Path, to: &Path) -> io::Result<()> { self.inner.rename(from, to) }
    fn rename_noreplace(&self, from: &Path, to: &Path) -> io::Result<()> { self.inner.rename_noreplace(from, to) }
    fn remove_file(&self, path: &Path) -> io::Result<()> { self.inner.remove_file(path) }
    fn sync_dir(&self, path: &Path) -> io::Result<()> { self.inner.sync_dir(path) }
}

#[test]
fn opener_retries_retired_inode_and_commits_to_published_generation() {
    let inner = Arc::new(MemVfs::new());
    let owner = Container::create_with(inner.clone(), Path::new("db.pga"), &Default::default()).unwrap();
    let seq = owner.commit_seq();
    let vfs = Arc::new(CompactDuringOpen { inner: inner.clone(), owner: Mutex::new(Some(owner)), opens: AtomicUsize::new(0) });
    let opened = Container::open_with(vfs.clone(), Path::new("db.pga"), &Default::default()).unwrap();
    assert_eq!(opened.commit_seq().0, seq.0 + 1);
    assert_eq!(vfs.opens.load(Ordering::Relaxed), 2);
    let mut w = opened.begin_write().unwrap();
    let dir = w.put(w.directory_root(), b"after", b"compaction").unwrap().root;
    w.set_directory_root(dir);
    let committed = w.commit().unwrap();
    drop(opened);
    let reopened = Container::open_with(inner, Path::new("db.pga"), &Default::default()).unwrap();
    assert_eq!(reopened.commit_seq(), committed);
    assert_eq!(reopened.snapshot().directory().get(b"after").unwrap(), Some(b"compaction".to_vec()));
}

#[test]
fn compaction_refusal_preserves_unowned_temporary() {
    let vfs = Arc::new(MemVfs::new());
    let c = Container::create_with(vfs.clone(), Path::new("db.pga"), &Default::default()).unwrap();
    let temp = Path::new("db.pga.compact-tmp");
    let held = vfs.open(temp, OpenMode::CreateNew).unwrap();
    held.try_lock(LockMode::Exclusive).unwrap();
    held.write_all_at(0, b"owned elsewhere").unwrap();
    assert_eq!(c.compact(&NoNestedRoots, &Default::default()).unwrap_err().sqlstate.code(), "58P02");
    assert_eq!(vfs.read_bytes(temp).unwrap(), b"owned elsewhere");
}
