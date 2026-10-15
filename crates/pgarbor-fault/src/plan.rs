//! Fault plans: what goes wrong, when. CONTRACT. Fully implemented (data only).
//!
//! Operation indexing: every I/O call through `FaultVfs`/`FaultFile` is an *op*. Ops whose kind
//! `is_mutating()` get a 0-based *mutating index*; `crash_at_op` and `ScheduledFault::at_op` refer
//! to that index, so read traffic (cache misses) never shifts crash points.

// CONTRACT
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum OpKind {
    /// `Vfs::open` of an existing file.
    Open,
    /// `Vfs::open` with CreateNew/CreateTruncate.
    Create,
    Read,
    Write,
    SyncData,
    SyncAll,
    Len,
    SetLen,
    Lock,
    Unlock,
    Exists,
    Rename,
    Remove,
    SyncDir,
}

impl OpKind {
    /// Ops that change (or make durable) on-disk state; these are crash points.
    pub fn is_mutating(self) -> bool {
        matches!(
            self,
            OpKind::Create
                | OpKind::Write
                | OpKind::SyncData
                | OpKind::SyncAll
                | OpKind::SetLen
                | OpKind::Rename
                | OpKind::Remove
                | OpKind::SyncDir
        )
    }
    pub fn is_sync(self) -> bool {
        matches!(self, OpKind::SyncData | OpKind::SyncAll | OpKind::SyncDir)
    }
}

// CONTRACT
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum FaultKind {
    /// The op fails with EIO (`from_raw_os_error(5)`) and has no effect.
    IoError,
    /// The op fails with ENOSPC (`from_raw_os_error(28)`) and has no effect. Only meaningful for
    /// Write/SetLen/Create; on other ops it degrades to `IoError`.
    Enospc,
    /// A Write persists only a random prefix `n < len` (possibly 0) and returns `Ok(n)`.
    /// On non-write ops it is ignored (op proceeds).
    ShortWrite,
    /// A SyncData/SyncAll/SyncDir returns EIO; Linux "fsyncgate" semantics: pending writes are
    /// randomly persisted or lost and are no longer pending (see `FileState::fsync_fail`).
    /// On non-sync ops it degrades to `IoError`.
    FsyncFail,
    /// Power loss: this op is not performed; it and every later op fail with the simulated-crash
    /// error until the scheduler takes `FaultVfs::crash_image`.
    Crash,
}

// CONTRACT
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ScheduledFault {
    /// Mutating-op index (0-based).
    pub at_op: u64,
    pub kind: FaultKind,
}

// CONTRACT
/// How unsynced writes survive a crash.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CrashMode {
    /// Lose every unsynced write and unsynced directory op (most pessimistic for durability).
    DropUnsynced,
    /// Keep every unsynced write and directory op (like a clean kill without power loss).
    KeepUnsynced,
    /// Each pending write independently: keep fully with `keep_permille`, tear (keep a random
    /// subset of its `sector_size`-aligned sectors) with `tear_permille`, else drop. This models
    /// reordering. Directory ops: a random prefix of the pending list survives.
    Random { keep_permille: u16, tear_permille: u16 },
}

// CONTRACT
#[derive(Clone, Debug, PartialEq)]
pub struct FaultPlan {
    pub seed: u64,
    /// Power loss at this mutating-op index.
    pub crash_at_op: Option<u64>,
    pub scheduled: Vec<ScheduledFault>,
    /// Random faults, per op of the applicable kind (0 = never).
    pub read_error_permille: u16,
    pub enospc_permille: u16,
    pub short_write_permille: u16,
    pub fsync_fail_permille: u16,
    /// Total bytes the simulated disk can hold (sum of live file lengths); a Write/SetLen that would
    /// exceed it fails with ENOSPC. `None` = unlimited.
    pub disk_capacity: Option<u64>,
    pub crash_mode: CrashMode,
    /// Torn-write granularity in bytes (default 512).
    pub sector_size: usize,
    /// Keep an op log (`FaultController::log`); off by default for speed.
    pub record_log: bool,
}

impl Default for FaultPlan {
    fn default() -> Self {
        FaultPlan {
            seed: 0,
            crash_at_op: None,
            scheduled: Vec::new(),
            read_error_permille: 0,
            enospc_permille: 0,
            short_write_permille: 0,
            fsync_fail_permille: 0,
            disk_capacity: None,
            crash_mode: CrashMode::Random { keep_permille: 500, tear_permille: 200 },
            sector_size: 512,
            record_log: false,
        }
    }
}

impl FaultPlan {
    /// No faults at all.
    pub fn none(seed: u64) -> FaultPlan {
        FaultPlan { seed, ..FaultPlan::default() }
    }
    /// Power loss at mutating op `op`.
    pub fn crash_at(seed: u64, op: u64) -> FaultPlan {
        FaultPlan { seed, crash_at_op: Some(op), ..FaultPlan::default() }
    }
    /// One scheduled fault.
    pub fn fault_at(seed: u64, op: u64, kind: FaultKind) -> FaultPlan {
        FaultPlan { seed, scheduled: vec![ScheduledFault { at_op: op, kind }], ..FaultPlan::default() }
    }
}
