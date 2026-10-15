//! The `FileIo` trait. CONTRACT (ARCHITECTURE §5). Fully specified here; no lease.
use std::fmt;
use std::io;

// CONTRACT
/// Advisory lock mode taken on a database file (STORE-12, ADR-004).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum LockMode {
    /// Many readers (read-only opens, `pgarbor doctor`).
    Shared,
    /// One read-write owner. A read-write `Container` always holds this.
    Exclusive,
}

// CONTRACT
/// Positional byte I/O on one open file. All methods take `&self` (implementations use interior
/// mutability or positional syscalls) so one handle is shared by concurrent reader threads.
///
/// Semantics every implementation MUST follow (pgarbor-fault relies on them):
/// - `read_at(offset, buf)`: reads up to `buf.len()` bytes at `offset`; returns the count read.
///   Reading at or past EOF returns `Ok(0)`; a read crossing EOF returns the short count. Never
///   extends the file. Bytes are the latest written bytes (including unsynced ones).
/// - `write_at(offset, buf)`: writes up to `buf.len()` bytes; may return a short count `n < len`
///   (callers loop via [`FileIo::write_all_at`]). Writing past EOF extends the file; any gap is
///   zero-filled. `Ok(0)` for a non-empty buffer means "no progress" (treated as ENOSPC-like).
/// - `sync_data()`: durably persists all previously written bytes AND the file length
///   (fdatasync semantics). After `Ok`, a power loss cannot lose those bytes.
/// - `sync_all()`: like `sync_data` plus all metadata (fsync).
/// - `len()`: current logical length (including unsynced extension).
/// - `set_len(len)`: truncate or zero-extend. Not durable until `sync_data`/`sync_all`.
/// - `try_lock(mode)`: non-blocking advisory lock for this handle. Contention returns
///   `Err(io::ErrorKind::WouldBlock)`. Re-locking the same handle with the same mode is `Ok`.
///   Locks are released by `unlock()` or when the handle is dropped.
/// - Errors are plain `io::Error`s. ENOSPC MUST be `io::Error::from_raw_os_error(28)` so that
///   `PgError::io` maps it to SQLSTATE 53100 (fault injectors construct it that way too).
pub trait FileIo: Send + Sync + fmt::Debug {
    fn read_at(&self, offset: u64, buf: &mut [u8]) -> io::Result<usize>;
    fn write_at(&self, offset: u64, buf: &[u8]) -> io::Result<usize>;
    fn sync_data(&self) -> io::Result<()>;
    fn sync_all(&self) -> io::Result<()>;
    fn len(&self) -> io::Result<u64>;
    fn set_len(&self, len: u64) -> io::Result<()>;
    fn try_lock(&self, mode: LockMode) -> io::Result<()>;
    fn unlock(&self) -> io::Result<()>;

    /// Read exactly `buf.len()` bytes or fail with `ErrorKind::UnexpectedEof`.
    /// Retries `ErrorKind::Interrupted`.
    fn read_exact_at(&self, offset: u64, buf: &mut [u8]) -> io::Result<()> {
        let mut done = 0usize;
        while done < buf.len() {
            match self.read_at(offset + done as u64, &mut buf[done..]) {
                Ok(0) => {
                    return Err(io::Error::new(
                        io::ErrorKind::UnexpectedEof,
                        format!("read only {} of {} bytes", done, buf.len()),
                    ));
                }
                Ok(n) => done += n,
                Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
                Err(e) => return Err(e),
            }
        }
        Ok(())
    }

    /// Write all of `buf` or fail. A zero-progress write becomes ENOSPC
    /// (`from_raw_os_error(28)`), mirroring PG md.c which assumes a short write means disk full.
    /// Retries `ErrorKind::Interrupted`.
    fn write_all_at(&self, offset: u64, buf: &[u8]) -> io::Result<()> {
        let mut done = 0usize;
        while done < buf.len() {
            match self.write_at(offset + done as u64, &buf[done..]) {
                Ok(0) => return Err(io::Error::from_raw_os_error(28)),
                Ok(n) => done += n,
                Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
                Err(e) => return Err(e),
            }
        }
        Ok(())
    }
}
