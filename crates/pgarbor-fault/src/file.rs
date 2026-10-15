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
use crate::controller::{Decision, FaultController};
use crate::disk::FileState;
use crate::plan::OpKind;
use pgarbor_storage::io::{FileIo, LockMode};
use std::io;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

/// Capacity hook: given a growth in bytes, Ok if the simulated disk can hold it.
pub type CapacityCheck = Arc<dyn Fn(u64) -> bool + Send + Sync>;

const EIO: i32 = 5;
const ENOSPC: i32 = 28;

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
        FaultFile::new(
            Arc::new(Mutex::new(FileState::new(bytes))),
            ctl,
            PathBuf::from("standalone"),
            true,
            Arc::new(|_: u64| true),
        )
    }
    pub fn state(&self) -> &Arc<Mutex<FileState>> {
        &self.state
    }
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

impl FaultFile {
    /// Ask the controller for this op's fate; a controller error becomes an `io::Error`.
    fn decide(&self, kind: OpKind, offset: u64, len: u64) -> io::Result<Decision> {
        self.ctl.decide(kind, &self.path, offset, len).map_err(io::Error::other)
    }

    fn lock_state(&self) -> std::sync::MutexGuard<'_, FileState> {
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn len_now(&self) -> u64 {
        self.lock_state().len()
    }

    /// Capacity hook for a growth of `growth` bytes. Must be called without holding `state`.
    fn check_grow(&self, growth: u64) -> io::Result<()> {
        if growth == 0 || (self.capacity)(growth) {
            Ok(())
        } else {
            Err(io::Error::from_raw_os_error(ENOSPC))
        }
    }

    fn check_writable(&self) -> io::Result<()> {
        if self.writable {
            Ok(())
        } else {
            Err(io::Error::new(io::ErrorKind::PermissionDenied, "file opened read-only"))
        }
    }

    /// Fate for an op where only errors matter: `Proceed` and `ShortWrite` (ignored on non-write
    /// ops) continue, every other decision becomes its error.
    fn proceed_or_err(&self, kind: OpKind, offset: u64, len: u64) -> io::Result<()> {
        match self.decide(kind, offset, len)? {
            Decision::Proceed | Decision::ShortWrite(_) => Ok(()),
            other => Err(FaultController::error_for(other)),
        }
    }

    fn sync_impl(&self, kind: OpKind) -> io::Result<()> {
        match self.decide(kind, 0, 0)? {
            Decision::Proceed | Decision::ShortWrite(_) => {
                self.lock_state().sync();
                Ok(())
            }
            Decision::FsyncFail => {
                let sector = self.ctl.plan().sector_size;
                let mut rng = self.ctl.crash_rng();
                self.lock_state().fsync_fail(sector, &mut rng).map_err(io::Error::other)?;
                Err(io::Error::from_raw_os_error(EIO))
            }
            other => Err(FaultController::error_for(other)),
        }
    }
}

impl FileIo for FaultFile {
    fn identity(&self) -> io::Result<(u64, u64)> {
        self.ctl.decide(OpKind::Len, &self.path, 0, 0).map_err(io::Error::other)
            .and_then(|d| match d {
                Decision::Proceed => Ok((2, Arc::as_ptr(&self.state) as usize as u64)),
                other => Err(FaultController::error_for(other)),
            })
    }
    fn read_at(&self, offset: u64, buf: &mut [u8]) -> io::Result<usize> {
        match self.decide(OpKind::Read, offset, buf.len() as u64)? {
            Decision::Proceed | Decision::ShortWrite(_) => Ok(self.lock_state().read(offset, buf)),
            other => Err(FaultController::error_for(other)),
        }
    }

    fn write_at(&self, offset: u64, buf: &[u8]) -> io::Result<usize> {
        self.check_writable()?;
        // Reject an overflowing range before consulting the controller.
        offset
            .checked_add(buf.len() as u64)
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "offset overflow"))?;
        let n = match self.decide(OpKind::Write, offset, buf.len() as u64)? {
            Decision::Proceed => buf.len(),
            Decision::ShortWrite(n) => n.min(buf.len()),
            other => return Err(FaultController::error_for(other)),
        };
        if n == 0 {
            return Ok(0);
        }
        // Capacity is checked before `state` is locked (the hook locks the namespace). The old
        // length is read in a separate critical section, so the growth figure is a snapshot.
        let old_len = self.len_now();
        let new_end = offset + n as u64;
        self.check_grow(new_end.saturating_sub(old_len))?;
        self.lock_state().write(offset, &buf[..n]);
        Ok(n)
    }

    fn sync_data(&self) -> io::Result<()> {
        self.sync_impl(OpKind::SyncData)
    }

    fn sync_all(&self) -> io::Result<()> {
        self.sync_impl(OpKind::SyncAll)
    }

    fn len(&self) -> io::Result<u64> {
        self.proceed_or_err(OpKind::Len, 0, 0)?;
        Ok(self.len_now())
    }

    fn set_len(&self, len: u64) -> io::Result<()> {
        self.check_writable()?;
        self.proceed_or_err(OpKind::SetLen, 0, len)?;
        let old_len = self.len_now();
        self.check_grow(len.saturating_sub(old_len))?;
        self.lock_state().set_len(len);
        Ok(())
    }

    fn try_lock(&self, mode: LockMode) -> io::Result<()> {
        self.proceed_or_err(OpKind::Lock, 0, 0)?;
        let mut held = self.held.lock().unwrap_or_else(|e| e.into_inner());
        if *held == Some(mode) {
            return Ok(());
        }
        let mut st = self.lock_state();
        // Drop this handle's current hold before re-evaluating (lock conversion).
        match *held {
            Some(LockMode::Shared) => st.shared_locks = st.shared_locks.saturating_sub(1),
            Some(LockMode::Exclusive) => st.exclusive_lock = false,
            None => {}
        }
        let ok = match mode {
            LockMode::Shared => !st.exclusive_lock,
            LockMode::Exclusive => !st.exclusive_lock && st.shared_locks == 0,
        };
        if !ok {
            // Restore the previous hold.
            match *held {
                Some(LockMode::Shared) => st.shared_locks += 1,
                Some(LockMode::Exclusive) => st.exclusive_lock = true,
                None => {}
            }
            return Err(io::Error::new(io::ErrorKind::WouldBlock, "file is locked by another handle"));
        }
        match mode {
            LockMode::Shared => st.shared_locks += 1,
            LockMode::Exclusive => st.exclusive_lock = true,
        }
        *held = Some(mode);
        Ok(())
    }

    fn unlock(&self) -> io::Result<()> {
        self.proceed_or_err(OpKind::Unlock, 0, 0)?;
        let mut held = self.held.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(m) = held.take() {
            let mut st = self.lock_state();
            match m {
                LockMode::Shared => st.shared_locks = st.shared_locks.saturating_sub(1),
                LockMode::Exclusive => st.exclusive_lock = false,
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::plan::{FaultKind, FaultPlan};

    fn ctl(plan: FaultPlan) -> Arc<FaultController> {
        FaultController::new(plan)
    }

    #[test]
    fn write_read_roundtrip() {
        let f = FaultFile::standalone(Vec::new(), ctl(FaultPlan::none(1)));
        f.write_all_at(4, b"abcd").unwrap();
        assert_eq!(f.len().unwrap(), 8);
        let mut buf = [9u8; 8];
        assert_eq!(f.read_at(0, &mut buf).unwrap(), 8);
        assert_eq!(buf, [0, 0, 0, 0, b'a', b'b', b'c', b'd']);
        assert_eq!(f.read_at(8, &mut buf).unwrap(), 0);
        f.set_len(2).unwrap();
        assert_eq!(f.len().unwrap(), 2);
    }

    #[test]
    fn short_write_then_proceed_completes() {
        // Mutating op 0 is a short write; the retry (op 1) proceeds.
        let f = FaultFile::standalone(Vec::new(), ctl(FaultPlan::fault_at(7, 0, FaultKind::ShortWrite)));
        let data = [0xABu8; 100];
        let n = f.write_at(0, &data).unwrap();
        assert!(n < data.len());
        assert_eq!(f.len().unwrap(), n as u64);
        f.write_all_at(0, &data).unwrap();
        assert_eq!(f.len().unwrap(), 100);
        let mut out = [0u8; 100];
        f.read_exact_at(0, &mut out).unwrap();
        assert_eq!(out, data);
    }

    #[test]
    fn enospc_leaves_file_unchanged() {
        let f = FaultFile::standalone(b"hello".to_vec(), ctl(FaultPlan::fault_at(1, 0, FaultKind::Enospc)));
        let err = f.write_at(0, b"XX").unwrap_err();
        assert_eq!(err.raw_os_error(), Some(28));
        assert_eq!(f.state().lock().unwrap().current, b"hello".to_vec());
        assert_eq!(f.len().unwrap(), 5);
    }

    #[test]
    fn capacity_refusal_is_enospc_without_effect() {
        let state = Arc::new(Mutex::new(FileState::new(b"abc".to_vec())));
        let f = FaultFile::new(state.clone(), ctl(FaultPlan::none(2)), PathBuf::from("c"), true, Arc::new(|_: u64| false));
        assert_eq!(f.write_at(0, b"zzzzzz").unwrap_err().raw_os_error(), Some(28));
        assert_eq!(state.lock().unwrap().current, b"abc".to_vec());
        // Overwrite without growth needs no capacity.
        assert_eq!(f.write_at(0, b"Z").unwrap(), 1);
        assert_eq!(state.lock().unwrap().current, b"Zbc".to_vec());
    }

    #[test]
    fn read_only_handle_rejects_writes() {
        let state = Arc::new(Mutex::new(FileState::new(b"abc".to_vec())));
        let f = FaultFile::new(state, ctl(FaultPlan::none(2)), PathBuf::from("r"), false, Arc::new(|_: u64| true));
        assert_eq!(f.write_at(0, b"x").unwrap_err().kind(), io::ErrorKind::PermissionDenied);
        assert_eq!(f.set_len(0).unwrap_err().kind(), io::ErrorKind::PermissionDenied);
    }

    #[test]
    fn exclusive_and_shared_locks_across_handles() {
        let state = Arc::new(Mutex::new(FileState::new(Vec::new())));
        let c = ctl(FaultPlan::none(3));
        let a = FaultFile::new(state.clone(), c.clone(), PathBuf::from("a"), true, Arc::new(|_: u64| true));
        let b = FaultFile::new(state.clone(), c.clone(), PathBuf::from("a"), false, Arc::new(|_: u64| true));

        a.try_lock(LockMode::Exclusive).unwrap();
        assert_eq!(b.try_lock(LockMode::Shared).unwrap_err().kind(), io::ErrorKind::WouldBlock);
        a.unlock().unwrap();

        b.try_lock(LockMode::Shared).unwrap();
        // Exclusive conversion fails while b shares; a holds nothing, so nothing is restored.
        assert_eq!(a.try_lock(LockMode::Exclusive).unwrap_err().kind(), io::ErrorKind::WouldBlock);
        // Re-locking with the same mode is fine.
        b.try_lock(LockMode::Shared).unwrap();
        drop(b);
        a.try_lock(LockMode::Exclusive).unwrap();
        assert!(state.lock().unwrap().exclusive_lock);
    }

    #[test]
    fn failed_conversion_restores_previous_shared_hold() {
        let state = Arc::new(Mutex::new(FileState::new(Vec::new())));
        let c = ctl(FaultPlan::none(4));
        let a = FaultFile::new(state.clone(), c.clone(), PathBuf::from("a"), true, Arc::new(|_: u64| true));
        let b = FaultFile::new(state.clone(), c.clone(), PathBuf::from("a"), true, Arc::new(|_: u64| true));
        a.try_lock(LockMode::Shared).unwrap();
        b.try_lock(LockMode::Shared).unwrap();
        assert_eq!(a.try_lock(LockMode::Exclusive).unwrap_err().kind(), io::ErrorKind::WouldBlock);
        assert_eq!(state.lock().unwrap().shared_locks, 2);
        drop(b);
        assert_eq!(state.lock().unwrap().shared_locks, 1);
        a.try_lock(LockMode::Exclusive).unwrap();
        assert_eq!(state.lock().unwrap().shared_locks, 0);
    }
}
