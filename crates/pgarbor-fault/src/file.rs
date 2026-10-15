//! `FaultFile`: `FileIo` over a `FileState` with controller-decided faults. Lease FLT-03.
//!
//! Stub convention for `io::Result` trait methods: `Err(io::Error::other(PgError::unimplemented(..)))`.
//!
//! Per-op behaviour (CONTRACT): first `ctl.decide(kind, path, offset, len)`; a `decide` error is
//! returned as `io::Error::other(err)`. Then:
//! - `Proceed` → perform the op on the shared `FileState` (reads from `current`; writes/set_len
//!   pending; `sync_data`/`sync_all` → `FileState::sync`). Before a Write/SetLen that grows the
//!   file, ask `vfs_capacity_check(new_len - old_len)` (passed in at construction as a closure);
//!   over capacity → ENOSPC with no effect. Call the capacity closure BEFORE locking `state`
//!   (it locks the namespace and other files; holding `state` would deadlock).
//! - `Fail(code)` → `Err(from_raw_os_error(code))`, no effect.
//! - `ShortWrite(n)` → perform `write(offset, &buf[..n])` (if n > 0) and return `Ok(n)`.
//! - `FsyncFail` → `FileState::fsync_fail(sector_size, &mut ctl.crash_rng())`, return EIO.
//! - `Crash` → `Err(FaultController::crash_error())`, no effect.
//! Kinds: read_at=Read, write_at=Write, sync_data=SyncData, sync_all=SyncAll, len=Len,
//! set_len=SetLen, try_lock=Lock, unlock=Unlock. `len`, `try_lock`, `unlock` and `Read` never
//! mutate. Writes on a read-only handle → PermissionDenied (checked before `decide`, not counted).
//! Locks: emulate like `MemFile` using `FileState::{shared_locks, exclusive_lock}` + this
//! handle's `held`; `Drop` releases held locks (implemented below).
use crate::controller::FaultController;
use crate::disk::FileState;
use pgarbor_common::PgError;
use pgarbor_storage::io::{FileIo, LockMode};
use std::io;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

/// Capacity hook: given a growth in bytes, Ok if the simulated disk can hold it.
pub type CapacityCheck = Arc<dyn Fn(u64) -> bool + Send + Sync>;

// CONTRACT
pub struct FaultFile {
    pub(crate) state: Arc<Mutex<FileState>>,
    pub(crate) ctl: Arc<FaultController>,
    pub(crate) path: PathBuf,
    pub(crate) writable: bool,
    pub(crate) held: Mutex<Option<LockMode>>,
    pub(crate) capacity: CapacityCheck,
}

impl std::fmt::Debug for FaultFile {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("FaultFile").field("path", &self.path).field("writable", &self.writable).finish()
    }
}

impl FaultFile {
    pub fn new(
        state: Arc<Mutex<FileState>>,
        ctl: Arc<FaultController>,
        path: PathBuf,
        writable: bool,
        capacity: CapacityCheck,
    ) -> FaultFile {
        FaultFile { state, ctl, path, writable, held: Mutex::new(None), capacity }
    }
    /// A standalone file (own controller, no capacity limit) — handy for unit tests.
    pub fn standalone(bytes: Vec<u8>, ctl: Arc<FaultController>) -> FaultFile {
        FaultFile::new(Arc::new(Mutex::new(FileState::new(bytes))), ctl, PathBuf::from("standalone"), true, Arc::new(|_| true))
    }
    pub fn state(&self) -> &Arc<Mutex<FileState>> {
        &self.state
    }
}

fn stub(what: &str) -> io::Error {
    io::Error::other(PgError::unimplemented("FLT-03", what))
}

impl Drop for FaultFile {
    fn drop(&mut self) {
        let held = self.held.get_mut().map(|h| h.take()).unwrap_or(None);
        if let Some(m) = held {
            let mut st = self.state.lock().unwrap_or_else(|e| e.into_inner());
            match m {
                LockMode::Shared => st.shared_locks = st.shared_locks.saturating_sub(1),
                LockMode::Exclusive => st.exclusive_lock = false,
            }
        }
    }
}

impl FileIo for FaultFile {
    #[allow(unused_variables)]
    fn read_at(&self, offset: u64, buf: &mut [u8]) -> io::Result<usize> {
        Err(stub("fault file read"))
    }
    #[allow(unused_variables)]
    fn write_at(&self, offset: u64, buf: &[u8]) -> io::Result<usize> {
        Err(stub("fault file write"))
    }
    fn sync_data(&self) -> io::Result<()> {
        Err(stub("fault file sync_data"))
    }
    fn sync_all(&self) -> io::Result<()> {
        Err(stub("fault file sync_all"))
    }
    fn len(&self) -> io::Result<u64> {
        Err(stub("fault file len"))
    }
    #[allow(unused_variables)]
    fn set_len(&self, len: u64) -> io::Result<()> {
        Err(stub("fault file set_len"))
    }
    #[allow(unused_variables)]
    fn try_lock(&self, mode: LockMode) -> io::Result<()> {
        Err(stub("fault file lock"))
    }
    fn unlock(&self) -> io::Result<()> {
        Err(stub("fault file unlock"))
    }
}
