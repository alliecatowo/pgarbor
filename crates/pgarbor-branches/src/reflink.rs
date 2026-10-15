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
#[allow(unused_variables)]
pub fn clone_file(src: &Path, dst: &Path) -> Result<CloneOutcome> {
    Err(PgError::unimplemented("BR-09", "reflink file clone"))
}

/// Whether this build has a reflink implementation at all (Linux or macOS).
pub fn platform_has_reflink() -> bool {
    cfg!(any(target_os = "linux", target_os = "macos"))
}
