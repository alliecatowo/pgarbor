//! The `Vfs` trait: directory-level operations needed by create/open/compaction/export.
//! CONTRACT. Fully specified here; no lease.
use super::file_io::FileIo;
use std::fmt;
use std::io;
use std::path::{Path, PathBuf};

// CONTRACT
/// How [`Vfs::open`] opens a path.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum OpenMode {
    /// Existing file, read only. NotFound if missing.
    ReadOnly,
    /// Existing file, read-write. NotFound if missing.
    ReadWrite,
    /// Create a new empty file read-write; `ErrorKind::AlreadyExists` if the path exists.
    CreateNew,
    /// Create or truncate to empty, read-write.
    CreateTruncate,
}

impl OpenMode {
    pub fn is_writable(self) -> bool {
        !matches!(self, OpenMode::ReadOnly)
    }
}

// CONTRACT
/// File-system abstraction. Directory operations are NOT durable until `sync_dir(parent)` succeeds
/// (POSIX: a created/renamed/removed name may vanish on power loss without a directory fsync).
///
/// - `open`: see [`OpenMode`]. Returned handle is unlocked.
/// - `exists`: whether a name exists (live view, including non-durable names).
/// - `rename(from, to)`: atomic replace of `to` (POSIX rename(2)). Open handles to the old `to`
///   inode keep reading the old bytes (Unix semantics; MemVfs/FaultVfs emulate this: a handle is
///   bound to its file object, not to its name).
/// - `remove_file`: unlink; open handles stay valid.
/// - `sync_dir(dir)`: make preceding create/rename/remove under `dir` durable. On platforms
///   without directory fsync (Windows) `OsVfs` returns `Ok(())` and documents the gap.
pub trait Vfs: Send + Sync + fmt::Debug {
    fn open(&self, path: &Path, mode: OpenMode) -> io::Result<Box<dyn FileIo>>;
    fn exists(&self, path: &Path) -> io::Result<bool>;
    fn rename(&self, from: &Path, to: &Path) -> io::Result<()>;
    fn remove_file(&self, path: &Path) -> io::Result<()>;
    fn sync_dir(&self, dir: &Path) -> io::Result<()>;
}

// CONTRACT
/// Parent directory of `path` for `sync_dir`; an empty parent (bare file name) becomes `"."`.
pub fn parent_dir(path: &Path) -> PathBuf {
    match path.parent() {
        Some(p) if !p.as_os_str().is_empty() => p.to_path_buf(),
        _ => PathBuf::from("."),
    }
}

// CONTRACT
/// `path` with `suffix` appended to the file name, in the same directory
/// (e.g. `dev.pga` + `.compact-tmp` → `dev.pga.compact-tmp`). Used for temp files that are later
/// renamed over `path`, which must live in the same directory for rename atomicity.
pub fn sibling_path(path: &Path, suffix: &str) -> PathBuf {
    let mut s = path.as_os_str().to_os_string();
    s.push(suffix);
    PathBuf::from(s)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn paths() {
        assert_eq!(parent_dir(Path::new("dev.pga")), PathBuf::from("."));
        assert_eq!(parent_dir(Path::new("/a/dev.pga")), PathBuf::from("/a"));
        assert_eq!(sibling_path(Path::new("/a/dev.pga"), ".tmp"), PathBuf::from("/a/dev.pga.tmp"));
    }
}
