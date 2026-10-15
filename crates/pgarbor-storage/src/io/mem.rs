//! `MemFile` / `MemVfs`: in-memory files for tests and wasm. CONTRACT. Fully implemented.
//!
//! MemFile models a perfect disk: every write is immediately "durable" and there are no crashes
//! (crash simulation lives in pgarbor-fault). Handles opened on the same path share one byte
//! buffer; a handle stays bound to its buffer after rename/remove (Unix inode semantics).
//! Locks are emulated per buffer: many `Shared` or one `Exclusive` across all handles.
use super::file_io::{FileIo, LockMode};
use super::vfs::{OpenMode, Vfs};
use std::collections::HashMap;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, RwLock};

#[derive(Debug, Default)]
struct LockState {
    shared: usize,
    exclusive: bool,
}

#[derive(Debug, Default)]
struct MemInner {
    data: RwLock<Vec<u8>>,
    locks: Mutex<LockState>,
}

// CONTRACT
/// One handle on an in-memory file.
#[derive(Debug)]
pub struct MemFile {
    inner: Arc<MemInner>,
    writable: bool,
    held: Mutex<Option<LockMode>>,
}

fn poisoned() -> io::Error {
    io::Error::other("MemFile lock poisoned")
}

impl MemFile {
    /// A new empty, writable, standalone file.
    pub fn new() -> MemFile {
        MemFile::from_bytes(Vec::new())
    }
    /// A standalone writable file with the given contents.
    pub fn from_bytes(bytes: Vec<u8>) -> MemFile {
        let inner = Arc::new(MemInner { data: RwLock::new(bytes), locks: Mutex::new(LockState::default()) });
        MemFile { inner, writable: true, held: Mutex::new(None) }
    }
    /// Copy of the current contents.
    pub fn to_bytes(&self) -> Vec<u8> {
        self.inner.data.read().map(|d| d.clone()).unwrap_or_default()
    }
    /// Another handle on the same buffer (unlocked).
    pub fn reopen(&self, writable: bool) -> MemFile {
        MemFile { inner: self.inner.clone(), writable, held: Mutex::new(None) }
    }
    fn check_writable(&self) -> io::Result<()> {
        if self.writable {
            Ok(())
        } else {
            Err(io::Error::new(io::ErrorKind::PermissionDenied, "file opened read-only"))
        }
    }
    fn release(&self) -> io::Result<()> {
        let mut held = self.held.lock().map_err(|_| poisoned())?;
        if let Some(m) = held.take() {
            let mut l = self.inner.locks.lock().map_err(|_| poisoned())?;
            match m {
                LockMode::Shared => l.shared = l.shared.saturating_sub(1),
                LockMode::Exclusive => l.exclusive = false,
            }
        }
        Ok(())
    }
}

impl Default for MemFile {
    fn default() -> Self {
        MemFile::new()
    }
}

impl Drop for MemFile {
    fn drop(&mut self) {
        let _ = self.release();
    }
}

impl FileIo for MemFile {
    fn identity(&self) -> io::Result<(u64, u64)> {
        Ok((1, Arc::as_ptr(&self.inner) as usize as u64))
    }
    fn read_at(&self, offset: u64, buf: &mut [u8]) -> io::Result<usize> {
        let d = self.inner.data.read().map_err(|_| poisoned())?;
        let len = d.len() as u64;
        if offset >= len {
            return Ok(0);
        }
        let start = offset as usize;
        let n = buf.len().min(d.len() - start);
        buf[..n].copy_from_slice(&d[start..start + n]);
        Ok(n)
    }
    fn write_at(&self, offset: u64, buf: &[u8]) -> io::Result<usize> {
        self.check_writable()?;
        let mut d = self.inner.data.write().map_err(|_| poisoned())?;
        let end = offset
            .checked_add(buf.len() as u64)
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "offset overflow"))?;
        let end = usize::try_from(end).map_err(|_| io::Error::from_raw_os_error(28))?;
        if d.len() < end {
            d.resize(end, 0);
        }
        d[offset as usize..end].copy_from_slice(buf);
        Ok(buf.len())
    }
    fn sync_data(&self) -> io::Result<()> {
        Ok(())
    }
    fn sync_all(&self) -> io::Result<()> {
        Ok(())
    }
    fn len(&self) -> io::Result<u64> {
        Ok(self.inner.data.read().map_err(|_| poisoned())?.len() as u64)
    }
    fn set_len(&self, len: u64) -> io::Result<()> {
        self.check_writable()?;
        let len = usize::try_from(len).map_err(|_| io::Error::from_raw_os_error(28))?;
        self.inner.data.write().map_err(|_| poisoned())?.resize(len, 0);
        Ok(())
    }
    fn try_lock(&self, mode: LockMode) -> io::Result<()> {
        let mut held = self.held.lock().map_err(|_| poisoned())?;
        if *held == Some(mode) {
            return Ok(());
        }
        let mut l = self.inner.locks.lock().map_err(|_| poisoned())?;
        // Drop what this handle holds before re-evaluating (lock conversion).
        match *held {
            Some(LockMode::Shared) => l.shared = l.shared.saturating_sub(1),
            Some(LockMode::Exclusive) => l.exclusive = false,
            None => {}
        }
        let ok = match mode {
            LockMode::Shared => !l.exclusive,
            LockMode::Exclusive => !l.exclusive && l.shared == 0,
        };
        if !ok {
            // restore previous hold
            match *held {
                Some(LockMode::Shared) => l.shared += 1,
                Some(LockMode::Exclusive) => l.exclusive = true,
                None => {}
            }
            return Err(io::Error::new(io::ErrorKind::WouldBlock, "file is locked by another handle"));
        }
        match mode {
            LockMode::Shared => l.shared += 1,
            LockMode::Exclusive => l.exclusive = true,
        }
        *held = Some(mode);
        Ok(())
    }
    fn unlock(&self) -> io::Result<()> {
        self.release()
    }
}

// CONTRACT
/// In-memory file system. Cloning shares the namespace.
#[derive(Clone, Debug, Default)]
pub struct MemVfs {
    files: Arc<Mutex<HashMap<PathBuf, Arc<MemInner>>>>,
}

impl MemVfs {
    pub fn new() -> MemVfs {
        MemVfs::default()
    }
    /// Current bytes of `path`, if it exists.
    pub fn read_bytes(&self, path: &Path) -> Option<Vec<u8>> {
        let files = self.files.lock().ok()?;
        let f = files.get(path)?;
        f.data.read().ok().map(|d| d.clone())
    }
    /// Create or replace `path` with `bytes`.
    pub fn write_bytes(&self, path: &Path, bytes: Vec<u8>) {
        if let Ok(mut files) = self.files.lock() {
            files.insert(
                path.to_path_buf(),
                Arc::new(MemInner { data: RwLock::new(bytes), locks: Mutex::new(LockState::default()) }),
            );
        }
    }
    /// All paths, sorted.
    pub fn paths(&self) -> Vec<PathBuf> {
        let mut v: Vec<PathBuf> = self.files.lock().map(|f| f.keys().cloned().collect()).unwrap_or_default();
        v.sort();
        v
    }
}

impl Vfs for MemVfs {
    fn open(&self, path: &Path, mode: OpenMode) -> io::Result<Box<dyn FileIo>> {
        let mut files = self.files.lock().map_err(|_| poisoned())?;
        let inner = match mode {
            OpenMode::ReadOnly | OpenMode::ReadWrite => files
                .get(path)
                .cloned()
                .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "no such file"))?,
            OpenMode::CreateNew => {
                if files.contains_key(path) {
                    return Err(io::Error::new(io::ErrorKind::AlreadyExists, "file exists"));
                }
                let i = Arc::new(MemInner::default());
                files.insert(path.to_path_buf(), i.clone());
                i
            }
            OpenMode::CreateTruncate => {
                let i = files.entry(path.to_path_buf()).or_insert_with(|| Arc::new(MemInner::default())).clone();
                i.data.write().map_err(|_| poisoned())?.clear();
                i
            }
        };
        Ok(Box::new(MemFile { inner, writable: mode.is_writable(), held: Mutex::new(None) }))
    }
    fn exists(&self, path: &Path) -> io::Result<bool> {
        Ok(self.files.lock().map_err(|_| poisoned())?.contains_key(path))
    }
    fn rename(&self, from: &Path, to: &Path) -> io::Result<()> {
        let mut files = self.files.lock().map_err(|_| poisoned())?;
        let f = files.remove(from).ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "no such file"))?;
        files.insert(to.to_path_buf(), f);
        Ok(())
    }
    fn rename_noreplace(&self, from: &Path, to: &Path) -> io::Result<()> {
        let mut files = self.files.lock().map_err(|_| poisoned())?;
        if files.contains_key(to) {
            return Err(io::Error::new(io::ErrorKind::AlreadyExists, "file exists"));
        }
        let f = files.remove(from).ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "no such file"))?;
        files.insert(to.to_path_buf(), f);
        Ok(())
    }
    fn remove_file(&self, path: &Path) -> io::Result<()> {
        let mut files = self.files.lock().map_err(|_| poisoned())?;
        files.remove(path).map(|_| ()).ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "no such file"))
    }
    fn sync_dir(&self, _dir: &Path) -> io::Result<()> {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn read_write_and_locks() {
        let vfs = MemVfs::new();
        let p = Path::new("x.pga");
        let a = vfs.open(p, OpenMode::CreateNew).unwrap();
        a.write_all_at(10, b"hi").unwrap();
        assert_eq!(a.len().unwrap(), 12);
        let mut buf = [9u8; 4];
        assert_eq!(a.read_at(9, &mut buf).unwrap(), 3);
        assert_eq!(&buf[..3], &[0, b'h', b'i']);
        let b = vfs.open(p, OpenMode::ReadWrite).unwrap();
        a.try_lock(LockMode::Exclusive).unwrap();
        assert_eq!(b.try_lock(LockMode::Shared).unwrap_err().kind(), io::ErrorKind::WouldBlock);
        drop(a);
        b.try_lock(LockMode::Exclusive).unwrap();
        assert!(vfs.open(p, OpenMode::CreateNew).is_err());
    }
}
