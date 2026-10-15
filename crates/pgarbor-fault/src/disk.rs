//! Per-file durability model. Lease FLT-02 (sonnet): `fsync_fail`, `crash_image`. The rest is
//! implemented.
//!
//! Model (CONTRACT): a file has `durable` bytes (what survives power loss), `current` bytes (what
//! reads see: durable + all pending ops applied in order) and an ordered `pending` list of
//! unsynced mutations. `sync()` makes current durable. Power loss produces a durable image from
//! `durable` plus a CrashMode-selected subset of `pending` (applied in order; tearing at
//! `sector_size`-aligned absolute offsets).
use crate::plan::CrashMode;
use crate::rng::DetRng;
use pgarbor_common::{PgError, Result};

// CONTRACT
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Pending {
    Write { offset: u64, data: Vec<u8> },
    SetLen(u64),
}

// CONTRACT
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct FileState {
    pub durable: Vec<u8>,
    pub current: Vec<u8>,
    pub pending: Vec<Pending>,
    /// Emulated advisory locks across handles: shared count / exclusive flag.
    pub shared_locks: usize,
    pub exclusive_lock: bool,
}

/// Apply one pending op to a byte vector (zero-filling gaps).
pub fn apply(buf: &mut Vec<u8>, op: &Pending) {
    match op {
        Pending::Write { offset, data } => {
            let start = *offset as usize;
            let end = start + data.len();
            if buf.len() < end {
                buf.resize(end, 0);
            }
            buf[start..end].copy_from_slice(data);
        }
        Pending::SetLen(len) => buf.resize(*len as usize, 0),
    }
}

impl FileState {
    /// A file whose durable and current contents are `bytes`.
    pub fn new(bytes: Vec<u8>) -> FileState {
        FileState { durable: bytes.clone(), current: bytes, ..FileState::default() }
    }
    pub fn len(&self) -> u64 {
        self.current.len() as u64
    }
    pub fn is_empty(&self) -> bool {
        self.current.is_empty()
    }
    /// Read from `current`; returns bytes copied (0 at/after EOF).
    pub fn read(&self, offset: u64, buf: &mut [u8]) -> usize {
        let len = self.current.len() as u64;
        if offset >= len {
            return 0;
        }
        let start = offset as usize;
        let n = buf.len().min(self.current.len() - start);
        buf[..n].copy_from_slice(&self.current[start..start + n]);
        n
    }
    /// Unsynced write.
    pub fn write(&mut self, offset: u64, data: &[u8]) {
        let op = Pending::Write { offset, data: data.to_vec() };
        apply(&mut self.current, &op);
        self.pending.push(op);
    }
    /// Unsynced truncate/extend.
    pub fn set_len(&mut self, len: u64) {
        let op = Pending::SetLen(len);
        apply(&mut self.current, &op);
        self.pending.push(op);
    }
    /// Successful fsync/fdatasync: everything current becomes durable.
    pub fn sync(&mut self) {
        self.durable = self.current.clone();
        self.pending.clear();
    }

    /// Failed fsync with Linux "fsyncgate" semantics: each pending op is independently persisted
    /// into `durable` (keep 1/2, tear 1/4 using `sector_size`, drop 1/4, decided by `rng`), then
    /// `pending` is CLEARED while `current` is left unchanged — the page cache still shows the
    /// data but it will never be retried, so a later successful sync does not make the dropped
    /// parts durable. (This is exactly why the storage layer poisons after an fsync error.)
    #[allow(unused_variables)]
    pub fn fsync_fail(&mut self, sector_size: usize, rng: &mut DetRng) -> Result<()> {
        Err(PgError::unimplemented("FLT-02", "fsyncgate simulation"))
    }

    /// Durable image after a power loss, without mutating self. `DropUnsynced` → `durable`;
    /// `KeepUnsynced` → `current`; `Random{keep, tear}`: start from `durable`, for each pending op
    /// in order draw `r = rng.below(1000)`: `r < keep` → apply fully; `r < keep + tear` → Write:
    /// apply only a random non-empty proper subset of its sector chunks (chunk boundaries at
    /// absolute offsets multiple of `sector_size`; the first/last chunk may be partial; a write
    /// inside one sector is kept or dropped whole), SetLen: apply; else drop.
    /// Example: durable "AAAA", pending [Write{0,"BB"}], KeepUnsynced → "BBAA".
    #[allow(unused_variables)]
    pub fn crash_image(&self, mode: CrashMode, sector_size: usize, rng: &mut DetRng) -> Result<Vec<u8>> {
        Err(PgError::unimplemented("FLT-02", "crash image construction"))
    }
}
