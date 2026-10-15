//! Shared fault decision point for one simulated disk. Lease FLT-01 (`decide`).
//!
//! Every `FaultVfs`/`FaultFile` op calls `decide` BEFORE doing anything; the decision fully
//! determines the op's fate. One controller per `FaultVfs` (shared by all its files), so op
//! indexing is global across files (compaction temp file + main file).
#[allow(unused_imports)]
use crate::plan::{FaultKind, FaultPlan, OpKind};
use crate::rng::DetRng;
use pgarbor_common::{PgError, Result};
use std::io;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

// CONTRACT
/// Fate of one op.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Decision {
    Proceed,
    /// Fail with this raw OS error (5 = EIO, 28 = ENOSPC); no effect.
    Fail(i32),
    /// Write only the first `n` bytes and return `Ok(n)`.
    ShortWrite(usize),
    /// Sync fails with EIO after fsyncgate processing of pending writes.
    FsyncFail,
    /// Power loss: fail with `crash_error()`; the disk is frozen.
    Crash,
}

// CONTRACT
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OpRecord {
    /// Global op index (all kinds).
    pub index: u64,
    /// Mutating-op index when `kind.is_mutating()`.
    pub mutating_index: Option<u64>,
    pub kind: OpKind,
    pub path: PathBuf,
    pub offset: u64,
    pub len: u64,
    pub decision: Decision,
}

#[derive(Debug)]
pub(crate) struct CtlState {
    pub(crate) plan: FaultPlan,
    /// Stream for random fault decisions (seeded from plan.seed).
    pub(crate) rng: DetRng,
    /// Separate stream for crash-image / fsyncgate randomness (seed ^ 0xC4A5_5EED).
    pub(crate) crash_rng: DetRng,
    pub(crate) ops: u64,
    pub(crate) mutating_ops: u64,
    pub(crate) crashed: bool,
    pub(crate) log: Vec<OpRecord>,
}

// CONTRACT
#[derive(Debug)]
pub struct FaultController {
    pub(crate) state: Mutex<CtlState>,
}

impl FaultController {
    pub fn new(plan: FaultPlan) -> Arc<FaultController> {
        let rng = DetRng::new(plan.seed);
        let crash_rng = DetRng::new(plan.seed ^ 0xC4A5_5EED);
        Arc::new(FaultController {
            state: Mutex::new(CtlState { plan, rng, crash_rng, ops: 0, mutating_ops: 0, crashed: false, log: Vec::new() }),
        })
    }
    fn st(&self) -> std::sync::MutexGuard<'_, CtlState> {
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }
    pub fn plan(&self) -> FaultPlan {
        self.st().plan.clone()
    }
    pub fn crashed(&self) -> bool {
        self.st().crashed
    }
    /// Force a crash now (scheduler "pull the plug" after a workload finishes).
    pub fn trigger_crash(&self) {
        self.st().crashed = true;
    }
    pub fn ops(&self) -> u64 {
        self.st().ops
    }
    pub fn mutating_ops(&self) -> u64 {
        self.st().mutating_ops
    }
    pub fn log(&self) -> Vec<OpRecord> {
        self.st().log.clone()
    }
    /// Draw from the crash/fsyncgate randomness stream.
    pub fn crash_rng(&self) -> DetRng {
        let mut st = self.st();
        st.crash_rng.fork(0)
    }
    /// The error every op returns after a simulated power loss:
    /// `io::Error::other("pgarbor-fault: simulated crash")`.
    pub fn crash_error() -> io::Error {
        io::Error::other("pgarbor-fault: simulated crash")
    }
    /// `Decision::Fail(code)` → `io::Error::from_raw_os_error(code)`; Crash → `crash_error()`.
    pub fn error_for(d: Decision) -> io::Error {
        match d {
            Decision::Fail(code) => io::Error::from_raw_os_error(code),
            Decision::FsyncFail => io::Error::from_raw_os_error(5),
            _ => FaultController::crash_error(),
        }
    }

    /// Decide the fate of one op. Order (deterministic):
    /// 1. If already crashed → `Crash` (not counted).
    /// 2. `index = ops++`; if `kind.is_mutating()`: `m = mutating_ops++`.
    /// 3. Mutating and `plan.crash_at_op == Some(m)` → set crashed, `Crash`.
    /// 4. Mutating and a `plan.scheduled` entry has `at_op == m` → map its kind: Crash → as 3;
    ///    IoError → `Fail(5)`; Enospc → `Fail(28)` for Write/SetLen/Create else `Fail(5)`;
    ///    ShortWrite → for Write `ShortWrite(rng.below(len))` else `Proceed`; FsyncFail → for
    ///    sync kinds `FsyncFail` else `Fail(5)`.
    /// 5. Random faults (each draws from `rng` ONLY when its permille > 0, to keep streams stable):
    ///    Read with `read_error_permille` → `Fail(5)`; Write/SetLen/Create with `enospc_permille`
    ///    → `Fail(28)`; Write with `short_write_permille` → `ShortWrite(rng.below(len))`;
    ///    SyncData/SyncAll/SyncDir with `fsync_fail_permille` → `FsyncFail`.
    /// 6. Else `Proceed`.
    /// When `plan.record_log`, push an `OpRecord`. `disk_capacity` is enforced by `FaultVfs`
    /// (it knows file sizes), not here. `Err` only for the lease stub.
    #[allow(unused_variables)]
    pub fn decide(&self, kind: OpKind, path: &Path, offset: u64, len: u64) -> Result<Decision> {
        Err(PgError::unimplemented("FLT-01", "fault decision"))
    }
}
