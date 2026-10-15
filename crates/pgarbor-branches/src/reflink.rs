//! Filesystem copy-on-write file cloning (BRANCH-10 macOS `clonefile`, BRANCH-11 Linux `FICLONE`).
//! Lease BR-09 (sonnet).
//!
//! Reflink is an OPTIMIZATION of detach, never the semantic basis of branching (RFC 001 §4.2).
//! It only works on real OS paths; callers pass paths, not `Vfs` handles (the `FileIo` trait
//! exposes no file descriptor). The caller (detach) is responsible for the snapshot barrier
//! (holding the container's writer so no commit is mid-flight while the clone is taken).
//!
//! # Linux (`#[cfg(target_os = "linux")]`)
//! 1. `src_f = std::fs::File::open(src)` (error → `pgarbor_storage::error::open_failed(src, &e)`).
//! 2. `dst_f = std::fs::OpenOptions::new().write(true).create_new(true).open(dst)`
//!    (AlreadyExists → `error::destination_exists(dst)`; other → `open_failed`).
//! 3. `let rc = unsafe { libc::ioctl(dst_f.as_raw_fd(), libc::FICLONE, src_f.as_raw_fd()) };`
//!    Pass `libc::FICLONE` directly as the request argument (its type is `libc::Ioctl`, which is
//!    `c_ulong` on glibc and `c_int` on musl — never cast it yourself).
//! 4. `rc == -1`: `errno = std::io::Error::last_os_error().raw_os_error()`. If errno is one of
//!    `EOPNOTSUPP`, `ENOTSUP`, `EXDEV`, `EINVAL`, `ENOTTY`, `ENOSYS` or `EPERM` (some FUSE/overlay
//!    filesystems report EPERM) → drop `dst_f`, `std::fs::remove_file(dst)` best-effort,
//!    return `Ok(CloneOutcome::Unsupported { reason: "<syscall> failed: <io error text>" })`.
//!    Any other errno (e.g. ENOSPC, EIO) → remove dst best-effort and return
//!    `Err(PgError::io(&format!("could not clone file \"{}\" to \"{}\"", src, dst), &err))`.
//! 5. Success: `dst_f.sync_all()` (error → `pgarbor_storage::error::fsync_failed(dst, &e)`), then
//!    `Ok(CloneOutcome::Cloned)`. The caller syncs the directory after the final rename.
//!
//! # macOS (`#[cfg(target_os = "macos")]`)
//! `libc::clonefile(src_c.as_ptr(), dst_c.as_ptr(), 0)` with `CString`s from
//! `std::os::unix::ffi::OsStrExt::as_bytes` (interior NUL → Unsupported). `dst` must not exist
//! (check first → `destination_exists`). errno `ENOTSUP`/`EXDEV`/`EINVAL` → Unsupported; others →
//! `Err(PgError::io(..))`. Success → open dst and `sync_all`. NOTE: this block is never compiled by
//! the Linux CI's `cargo check`; keep it minimal and mirror the Linux structure exactly.
//!
//! # Other platforms
//! `Ok(CloneOutcome::Unsupported { reason: "reflink cloning is not available on this platform" })`.
//!
//! Required tests (Linux, `tempfile` dev-dep): cloning on tmpfs/ext4 returns `Unsupported` and
//! leaves no `dst` behind; existing dst → 58P02; on btrfs/xfs-reflink (skip when unsupported)
//! the clone has identical bytes.
use pgarbor_common::{PgError, Result};
use std::path::Path;

// CONTRACT
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CloneOutcome {
    /// `dst` now exists, shares extents with `src`, and is fsynced.
    Cloned,
    /// The file system/platform cannot reflink; `dst` does not exist.
    Unsupported { reason: String },
}

/// Clone `src` to the new path `dst` (see module docs).
pub fn clone_file(src: &Path, dst: &Path) -> Result<CloneOutcome> {
    clone_file_impl(src, dst)
}

#[cfg(target_os = "linux")]
fn clone_file_impl(src: &Path, dst: &Path) -> Result<CloneOutcome> {
    use std::os::fd::AsRawFd;

    let src_f = std::fs::File::open(src)
        .map_err(|e| pgarbor_storage::error::open_failed(src, &e))?;
    let dst_f = match std::fs::OpenOptions::new().write(true).create_new(true).open(dst) {
        Ok(f) => f,
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
            return Err(crate::error::destination_exists(dst));
        }
        Err(e) => return Err(pgarbor_storage::error::open_failed(dst, &e)),
    };
    // SAFETY: both descriptors are valid and owned by live `File`s for the duration of the
    // call; FICLONE takes the source fd as its integer argument and touches no user memory.
    let rc = unsafe { libc::ioctl(dst_f.as_raw_fd(), libc::FICLONE, src_f.as_raw_fd()) };
    if rc == -1 {
        let err = std::io::Error::last_os_error();
        drop(dst_f);
        let _ = std::fs::remove_file(dst);
        let unsupported = matches!(
            err.raw_os_error(),
            Some(e) if e == libc::EOPNOTSUPP
                || e == libc::ENOTSUP
                || e == libc::EXDEV
                || e == libc::EINVAL
                || e == libc::ENOTTY
                || e == libc::ENOSYS
                || e == libc::EPERM
        );
        if unsupported {
            return Ok(CloneOutcome::Unsupported {
                reason: format!("ioctl(FICLONE) failed: {err}"),
            });
        }
        return Err(PgError::io(
            &format!(
                "could not clone file \"{}\" to \"{}\"",
                src.display(),
                dst.display()
            ),
            &err,
        ));
    }
    if let Err(e) = dst_f.sync_all() {
        drop(dst_f);
        let _ = std::fs::remove_file(dst);
        return Err(pgarbor_storage::error::fsync_failed(dst, &e));
    }
    Ok(CloneOutcome::Cloned)
}

#[cfg(target_os = "macos")]
fn clone_file_impl(src: &Path, dst: &Path) -> Result<CloneOutcome> {
    use std::ffi::CString;
    use std::os::unix::ffi::OsStrExt;

    let (src_c, dst_c) = match (
        CString::new(src.as_os_str().as_bytes()),
        CString::new(dst.as_os_str().as_bytes()),
    ) {
        (Ok(a), Ok(b)) => (a, b),
        _ => {
            return Ok(CloneOutcome::Unsupported {
                reason: "path contains an interior NUL byte".to_string(),
            })
        }
    };
    if dst.exists() {
        return Err(crate::error::destination_exists(dst));
    }
    // SAFETY: both pointers reference valid NUL-terminated strings that outlive the call.
    let rc = unsafe { libc::clonefile(src_c.as_ptr(), dst_c.as_ptr(), 0) };
    if rc == -1 {
        let err = std::io::Error::last_os_error();
        match err.raw_os_error() {
            Some(e) if e == libc::ENOTSUP || e == libc::EXDEV || e == libc::EINVAL => {
                let _ = std::fs::remove_file(dst);
                return Ok(CloneOutcome::Unsupported {
                    reason: format!("clonefile failed: {err}"),
                });
            }
            Some(e) if e == libc::ENOENT && !src.exists() => {
                return Err(pgarbor_storage::error::open_failed(src, &err));
            }
            Some(e) if e == libc::EEXIST => {
                return Err(crate::error::destination_exists(dst));
            }
            _ => {
                let _ = std::fs::remove_file(dst);
                return Err(PgError::io(
                    &format!(
                        "could not clone file \"{}\" to \"{}\"",
                        src.display(),
                        dst.display()
                    ),
                    &err,
                ));
            }
        }
    }
    let f = std::fs::File::open(dst).map_err(|e| pgarbor_storage::error::open_failed(dst, &e))?;
    f.sync_all()
        .map_err(|e| pgarbor_storage::error::fsync_failed(dst, &e))?;
    Ok(CloneOutcome::Cloned)
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
fn clone_file_impl(_src: &Path, _dst: &Path) -> Result<CloneOutcome> {
    Ok(CloneOutcome::Unsupported {
        reason: "reflink cloning is not available on this platform".to_string(),
    })
}

/// Whether this build has a reflink implementation at all (Linux or macOS).
pub fn platform_has_reflink() -> bool {
    cfg!(any(target_os = "linux", target_os = "macos"))
}

#[cfg(all(test, target_os = "linux"))]
mod tests {
    use super::*;

    #[test]
    fn clone_or_unsupported_leaves_no_stray_dst() {
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("src.bin");
        let dst = dir.path().join("dst.bin");
        std::fs::write(&src, b"hello reflink").unwrap();
        match clone_file(&src, &dst).unwrap() {
            CloneOutcome::Cloned => assert_eq!(std::fs::read(&dst).unwrap(), b"hello reflink"),
            CloneOutcome::Unsupported { reason } => {
                assert!(!reason.is_empty());
                assert!(!dst.exists());
            }
        }
    }

    #[test]
    fn existing_destination_is_58p02() {
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("src.bin");
        let dst = dir.path().join("dst.bin");
        std::fs::write(&src, b"a").unwrap();
        std::fs::write(&dst, b"keep").unwrap();
        let err = clone_file(&src, &dst).unwrap_err();
        assert_eq!(err.sqlstate.0, *b"58P02");
        assert_eq!(std::fs::read(&dst).unwrap(), b"keep");
    }

    #[test]
    fn missing_source_is_58p01() {
        let dir = tempfile::tempdir().unwrap();
        let err = clone_file(&dir.path().join("nope"), &dir.path().join("dst")).unwrap_err();
        assert_eq!(err.sqlstate.0, *b"58P01");
        assert!(!dir.path().join("dst").exists());
    }

    #[test]
    fn platform_flag() {
        assert!(platform_has_reflink());
    }
}
