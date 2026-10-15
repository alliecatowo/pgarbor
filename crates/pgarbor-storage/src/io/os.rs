//! `OsFile` / `OsVfs`: real files via `std::fs` + `fs2` advisory locks. CONTRACT. Fully implemented.
//!
//! Positional I/O uses `std::os::unix::fs::FileExt` (pread/pwrite) on Unix and
//! `std::os::windows::fs::FileExt` (seek_read/seek_write) on Windows. Other targets (wasm) get
//! `ErrorKind::Unsupported`; use `MemVfs` there.
//!
//! Locking uses `fs2::FileExt` called with fully-qualified syntax, because `std::fs::File` (Rust
//! >= 1.89) has inherent `try_lock_shared`/`unlock` methods with different signatures that would
//! otherwise win method resolution. fs2 uses `flock(2)` on Unix (lock follows the open file
//! description; it survives `rename` of the path, which compaction relies on) and `LockFileEx` on
//! Windows.
use super::file_io::{FileIo, LockMode};
use super::vfs::{OpenMode, Vfs};
use std::fs::{File, OpenOptions as FsOpenOptions};
use std::io;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

// CONTRACT
/// A real OS file.
#[derive(Debug)]
pub struct OsFile {
    file: File,
    path: PathBuf,
    writable: bool,
    held: Mutex<Option<LockMode>>,
}

impl OsFile {
    /// Wrap an already-open `std::fs::File`.
    pub fn from_std(file: File, path: PathBuf, writable: bool) -> OsFile {
        OsFile { file, path, writable, held: Mutex::new(None) }
    }
    /// Open `path` with `mode` (see [`OpenMode`]).
    pub fn open(path: &Path, mode: OpenMode) -> io::Result<OsFile> {
        let mut o = FsOpenOptions::new();
        match mode {
            OpenMode::ReadOnly => {
                o.read(true);
            }
            OpenMode::ReadWrite => {
                o.read(true).write(true);
            }
            OpenMode::CreateNew => {
                o.read(true).write(true).create_new(true);
            }
            OpenMode::CreateTruncate => {
                o.read(true).write(true).create(true).truncate(true);
            }
        }
        let file = o.open(path)?;
        Ok(OsFile::from_std(file, path.to_path_buf(), mode.is_writable()))
    }
    pub fn path(&self) -> &Path {
        &self.path
    }
    fn check_writable(&self) -> io::Result<()> {
        if self.writable {
            Ok(())
        } else {
            Err(io::Error::new(io::ErrorKind::PermissionDenied, "file opened read-only"))
        }
    }
}

impl FileIo for OsFile {
    fn identity(&self) -> io::Result<(u64, u64)> {
        #[cfg(unix)] {
            use std::os::unix::fs::MetadataExt;
            let m = self.file.metadata()?;
            Ok((m.dev(), m.ino()))
        }
        #[cfg(not(unix))] {
            Err(io::Error::new(io::ErrorKind::Unsupported, "file identity unsupported on this platform"))
        }
    }
    fn read_at(&self, offset: u64, buf: &mut [u8]) -> io::Result<usize> {
        #[cfg(unix)]
        {
            std::os::unix::fs::FileExt::read_at(&self.file, buf, offset)
        }
        #[cfg(windows)]
        {
            // seek_read returns Ok(0) at EOF like pread.
            std::os::windows::fs::FileExt::seek_read(&self.file, buf, offset)
        }
        #[cfg(not(any(unix, windows)))]
        {
            let _ = (offset, buf);
            Err(io::Error::new(io::ErrorKind::Unsupported, "positional I/O unsupported on this target"))
        }
    }

    fn write_at(&self, offset: u64, buf: &[u8]) -> io::Result<usize> {
        self.check_writable()?;
        #[cfg(unix)]
        {
            std::os::unix::fs::FileExt::write_at(&self.file, buf, offset)
        }
        #[cfg(windows)]
        {
            std::os::windows::fs::FileExt::seek_write(&self.file, buf, offset)
        }
        #[cfg(not(any(unix, windows)))]
        {
            let _ = (offset, buf);
            Err(io::Error::new(io::ErrorKind::Unsupported, "positional I/O unsupported on this target"))
        }
    }

    fn sync_data(&self) -> io::Result<()> {
        self.file.sync_data()
    }

    fn sync_all(&self) -> io::Result<()> {
        self.file.sync_all()
    }

    fn len(&self) -> io::Result<u64> {
        Ok(self.file.metadata()?.len())
    }

    fn set_len(&self, len: u64) -> io::Result<()> {
        self.check_writable()?;
        self.file.set_len(len)
    }

    fn try_lock(&self, mode: LockMode) -> io::Result<()> {
        let mut held = self.held.lock().map_err(|_| io::Error::other("lock state poisoned"))?;
        if *held == Some(mode) {
            return Ok(());
        }
        let r = match mode {
            LockMode::Shared => fs2::FileExt::try_lock_shared(&self.file),
            LockMode::Exclusive => fs2::FileExt::try_lock_exclusive(&self.file),
        };
        match r {
            Ok(()) => {
                *held = Some(mode);
                Ok(())
            }
            Err(e) => {
                // fs2 reports contention as EWOULDBLOCK (Unix) / ERROR_LOCK_VIOLATION (Windows).
                let contended = e.kind() == io::ErrorKind::WouldBlock
                    || e.raw_os_error() == fs2::lock_contended_error().raw_os_error();
                if contended {
                    Err(io::Error::new(io::ErrorKind::WouldBlock, "file is locked by another process"))
                } else {
                    Err(e)
                }
            }
        }
    }

    fn unlock(&self) -> io::Result<()> {
        let mut held = self.held.lock().map_err(|_| io::Error::other("lock state poisoned"))?;
        if held.is_some() {
            fs2::FileExt::unlock(&self.file)?;
            *held = None;
        }
        Ok(())
    }
}

// CONTRACT
/// The real file system.
#[derive(Clone, Copy, Debug, Default)]
pub struct OsVfs;

impl Vfs for OsVfs {
    fn open(&self, path: &Path, mode: OpenMode) -> io::Result<Box<dyn FileIo>> {
        Ok(Box::new(OsFile::open(path, mode)?))
    }
    fn exists(&self, path: &Path) -> io::Result<bool> {
        path.try_exists()
    }
    fn rename(&self, from: &Path, to: &Path) -> io::Result<()> {
        std::fs::rename(from, to)
    }
    fn rename_noreplace(&self, from: &Path, to: &Path) -> io::Result<()> {
        #[cfg(any(target_os = "linux", target_os = "android"))] {
            use std::os::unix::ffi::OsStrExt;
            let src = std::ffi::CString::new(from.as_os_str().as_bytes())
                .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "NUL in path"))?;
            let dst = std::ffi::CString::new(to.as_os_str().as_bytes())
                .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "NUL in path"))?;
            // SAFETY: both paths are valid NUL-terminated strings for the call duration.
            let rc = unsafe { libc::renameat2(libc::AT_FDCWD, src.as_ptr(), libc::AT_FDCWD, dst.as_ptr(), libc::RENAME_NOREPLACE) };
            if rc == 0 { Ok(()) } else { Err(io::Error::last_os_error()) }
        }
        #[cfg(not(any(target_os = "linux", target_os = "android")))] {
            let _ = (from, to);
            Err(io::Error::new(io::ErrorKind::Unsupported, "atomic no-replace rename unsupported on this platform"))
        }
    }
    fn remove_file(&self, path: &Path) -> io::Result<()> {
        std::fs::remove_file(path)
    }
    fn sync_dir(&self, dir: &Path) -> io::Result<()> {
        #[cfg(unix)]
        {
            File::open(dir)?.sync_all()
        }
        #[cfg(not(unix))]
        {
            // KNOWN GAP: Windows has no portable directory fsync; NTFS journals metadata.
            let _ = dir;
            Ok(())
        }
    }
}
