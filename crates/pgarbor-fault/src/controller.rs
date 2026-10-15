//! Shared fault decision point for one simulated disk. Lease FLT-01 (`decide`).
//!
//! Every `FaultVfs`/`FaultFile` op calls `decide` BEFORE doing anything; the decision fully
//! determines the op's fate. One controller per `FaultVfs` (shared by all its files), so op
//! indexing is global across files (compaction temp file + main file).
use crate::plan::{FaultKind, FaultPlan, OpKind};
use crate::rng::DetRng;
use pgarbor_common::Result;
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
    pub fn decide(&self, kind: OpKind, path: &Path, offset: u64, len: u64) -> Result<Decision> {
        let mut st = self.st();
        // 1. A crashed disk fails every op; nothing is counted or logged.
        if st.crashed {
            return Ok(Decision::Crash);
        }
        // 2. Global op index, and the mutating index for mutating kinds.
        let index = st.ops;
        st.ops += 1;
        let mutating_index = if kind.is_mutating() {
            let m = st.mutating_ops;
            st.mutating_ops += 1;
            Some(m)
        } else {
            None
        };
        // 3-6. Crash point, scheduled faults, random faults, then Proceed.
        let decision = decide_locked(&mut *st, kind, len, mutating_index);
        if st.plan.record_log {
            st.log.push(OpRecord {
                index,
                mutating_index,
                kind,
                path: path.to_path_buf(),
                offset,
                len,
                decision,
            });
        }
        Ok(decision)
    }
}

/// Steps 3-6 of `FaultController::decide`, run with the controller state locked.
fn decide_locked(s: &mut CtlState, kind: OpKind, len: u64, mutating_index: Option<u64>) -> Decision {
    if let Some(m) = mutating_index {
        // 3. Power loss at a fixed crash point.
        if s.plan.crash_at_op == Some(m) {
            s.crashed = true;
            return Decision::Crash;
        }
        // 4. First scheduled fault for this mutating index.
        let scheduled = s.plan.scheduled.iter().find(|f| f.at_op == m).map(|f| f.kind);
        if let Some(fk) = scheduled {
            return match fk {
                FaultKind::Crash => {
                    s.crashed = true;
                    Decision::Crash
                }
                FaultKind::IoError => Decision::Fail(5),
                FaultKind::Enospc => match kind {
                    OpKind::Write | OpKind::SetLen | OpKind::Create => Decision::Fail(28),
                    _ => Decision::Fail(5),
                },
                FaultKind::ShortWrite => {
                    if kind == OpKind::Write {
                        short_prefix(s, len)
                    } else {
                        Decision::Proceed
                    }
                }
                FaultKind::FsyncFail => {
                    if kind.is_sync() {
                        Decision::FsyncFail
                    } else {
                        Decision::Fail(5)
                    }
                }
            };
        }
    }
    // 5. Random faults. Each draws from the stream only when its permille is non-zero.
    if kind == OpKind::Read && s.plan.read_error_permille > 0 && s.rng.chance(s.plan.read_error_permille) {
        return Decision::Fail(5);
    }
    let write_like = matches!(kind, OpKind::Write | OpKind::SetLen | OpKind::Create);
    if write_like && s.plan.enospc_permille > 0 && s.rng.chance(s.plan.enospc_permille) {
        return Decision::Fail(28);
    }
    if kind == OpKind::Write && s.plan.short_write_permille > 0 && s.rng.chance(s.plan.short_write_permille) {
        return short_prefix(s, len);
    }
    if kind.is_sync() && s.plan.fsync_fail_permille > 0 && s.rng.chance(s.plan.fsync_fail_permille) {
        return Decision::FsyncFail;
    }
    // 6. Otherwise the op proceeds.
    Decision::Proceed
}

/// A short write of `n = rng.below(len)` bytes (n < len); zero-length writes proceed.
fn short_prefix(s: &mut CtlState, len: u64) -> Decision {
    if len == 0 {
        Decision::Proceed
    } else {
        Decision::ShortWrite(s.rng.below(len) as usize)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::plan::{FaultKind, ScheduledFault};

    const P: &str = "/db/test.pga";

    fn decide_all(c: &FaultController, kinds: &[(OpKind, u64)]) -> Vec<Decision> {
        kinds.iter().map(|(k, len)| c.decide(*k, Path::new(P), 0, *len).unwrap()).collect()
    }

    fn one(kind: FaultKind, at: u64, op: OpKind, len: u64) -> Decision {
        let c = FaultController::new(FaultPlan::fault_at(7, at, kind));
        c.decide(op, Path::new(P), 0, len).unwrap()
    }

    fn random_plan(seed: u64) -> FaultPlan {
        FaultPlan {
            seed,
            read_error_permille: 120,
            enospc_permille: 80,
            short_write_permille: 150,
            fsync_fail_permille: 200,
            ..FaultPlan::default()
        }
    }

    fn stream(n: usize) -> Vec<(OpKind, u64)> {
        const KINDS: [OpKind; 8] = [
            OpKind::Read,
            OpKind::Write,
            OpKind::SyncData,
            OpKind::SetLen,
            OpKind::Create,
            OpKind::Read,
            OpKind::SyncAll,
            OpKind::Len,
        ];
        (0..n).map(|i| (KINDS[i % KINDS.len()], (i as u64 * 37) % 97)).collect()
    }

    fn run_stream(plan: FaultPlan, n: usize) -> Vec<Decision> {
        let c = FaultController::new(plan);
        decide_all(&c, &stream(n))
    }

    #[test]
    fn crash_at_op_3_crashes_the_fourth_mutating_op_and_every_later_op() {
        let c = FaultController::new(FaultPlan::crash_at(1, 3));
        let kinds = [
            (OpKind::Write, 8),
            (OpKind::Read, 8),
            (OpKind::SyncData, 0),
            (OpKind::Write, 8),
            (OpKind::Read, 8),
            (OpKind::Create, 0),
            (OpKind::Read, 8),
            (OpKind::Write, 8),
            (OpKind::SyncData, 0),
        ];
        let d = decide_all(&c, &kinds);
        assert!(d[..5].iter().all(|x| *x == Decision::Proceed), "{d:?}");
        assert!(d[5..].iter().all(|x| *x == Decision::Crash), "{d:?}");
        assert!(c.crashed());
        assert_eq!(c.mutating_ops(), 4);
        // Ops after the crash are not counted.
        assert_eq!(c.ops(), 6);
    }

    #[test]
    fn reads_do_not_advance_the_mutating_index() {
        let plan = FaultPlan { record_log: true, ..FaultPlan::crash_at(1, 1) };
        let c = FaultController::new(plan);
        let kinds = [(OpKind::Read, 4), (OpKind::Read, 4), (OpKind::Write, 4), (OpKind::Read, 4), (OpKind::Write, 4), (OpKind::Read, 4)];
        let d = decide_all(&c, &kinds);
        assert_eq!(
            d,
            vec![Decision::Proceed, Decision::Proceed, Decision::Proceed, Decision::Proceed, Decision::Crash, Decision::Crash]
        );
        let log = c.log();
        assert_eq!(log.len(), 5);
        assert_eq!(log.iter().map(|r| r.mutating_index).collect::<Vec<_>>(), vec![None, None, Some(0), None, Some(1)]);
        assert_eq!(log[4].decision, Decision::Crash);
        assert_eq!(c.mutating_ops(), 2);
        assert_eq!(c.ops(), 5);
    }

    #[test]
    fn scheduled_fault_targets_the_mutating_index_not_the_global_index() {
        let c = FaultController::new(FaultPlan::fault_at(7, 1, FaultKind::IoError));
        let kinds = [(OpKind::Read, 4), (OpKind::Write, 4), (OpKind::Read, 4), (OpKind::Write, 4)];
        assert_eq!(
            decide_all(&c, &kinds),
            vec![Decision::Proceed, Decision::Proceed, Decision::Proceed, Decision::Fail(5)]
        );
    }

    #[test]
    fn scheduled_enospc_on_write_is_28_and_on_sync_is_5() {
        assert_eq!(one(FaultKind::Enospc, 0, OpKind::Write, 10), Decision::Fail(28));
        assert_eq!(one(FaultKind::Enospc, 0, OpKind::SetLen, 0), Decision::Fail(28));
        assert_eq!(one(FaultKind::Enospc, 0, OpKind::Create, 0), Decision::Fail(28));
        assert_eq!(one(FaultKind::Enospc, 0, OpKind::SyncData, 0), Decision::Fail(5));
        assert_eq!(one(FaultKind::Enospc, 0, OpKind::Rename, 0), Decision::Fail(5));
    }

    #[test]
    fn scheduled_kinds_map_per_op() {
        assert_eq!(one(FaultKind::IoError, 0, OpKind::Write, 10), Decision::Fail(5));
        assert_eq!(one(FaultKind::FsyncFail, 0, OpKind::SyncData, 0), Decision::FsyncFail);
        assert_eq!(one(FaultKind::FsyncFail, 0, OpKind::SyncDir, 0), Decision::FsyncFail);
        assert_eq!(one(FaultKind::FsyncFail, 0, OpKind::Write, 10), Decision::Fail(5));
        assert_eq!(one(FaultKind::ShortWrite, 0, OpKind::Rename, 0), Decision::Proceed);
        assert_eq!(one(FaultKind::ShortWrite, 0, OpKind::Write, 0), Decision::Proceed);
        match one(FaultKind::ShortWrite, 0, OpKind::Write, 10) {
            Decision::ShortWrite(n) => assert!(n < 10, "n = {n}"),
            other => panic!("expected ShortWrite, got {other:?}"),
        }
        assert_eq!(one(FaultKind::Crash, 0, OpKind::Remove, 0), Decision::Crash);
    }

    #[test]
    fn scheduled_crash_is_sticky_and_trigger_crash_freezes_the_disk() {
        let c = FaultController::new(FaultPlan { scheduled: vec![ScheduledFault { at_op: 0, kind: FaultKind::Crash }], ..FaultPlan::none(3) });
        assert_eq!(c.decide(OpKind::Write, Path::new(P), 0, 4).unwrap(), Decision::Crash);
        assert!(c.crashed());
        assert_eq!(c.decide(OpKind::Read, Path::new(P), 0, 4).unwrap(), Decision::Crash);
        assert_eq!(c.decide(OpKind::SyncAll, Path::new(P), 0, 0).unwrap(), Decision::Crash);

        let t = FaultController::new(FaultPlan::none(3));
        assert_eq!(t.decide(OpKind::Read, Path::new(P), 0, 4).unwrap(), Decision::Proceed);
        t.trigger_crash();
        assert_eq!(t.decide(OpKind::Read, Path::new(P), 0, 4).unwrap(), Decision::Crash);
        assert_eq!(t.ops(), 1);
    }

    #[test]
    fn no_faults_plan_always_proceeds() {
        let d = run_stream(FaultPlan::none(42), 200);
        assert!(d.iter().all(|x| *x == Decision::Proceed));
    }

    #[test]
    fn full_rate_random_faults_hit_every_applicable_op() {
        let plan = FaultPlan {
            seed: 5,
            read_error_permille: 1000,
            enospc_permille: 1000,
            short_write_permille: 1000,
            fsync_fail_permille: 1000,
            ..FaultPlan::default()
        };
        let c = FaultController::new(plan);
        let p = Path::new(P);
        assert_eq!(c.decide(OpKind::Read, p, 0, 4).unwrap(), Decision::Fail(5));
        assert_eq!(c.decide(OpKind::Open, p, 0, 0).unwrap(), Decision::Proceed);
        // ENOSPC is checked before the short-write draw on Write.
        assert_eq!(c.decide(OpKind::Write, p, 0, 4).unwrap(), Decision::Fail(28));
        assert_eq!(c.decide(OpKind::SetLen, p, 0, 4).unwrap(), Decision::Fail(28));
        assert_eq!(c.decide(OpKind::SyncData, p, 0, 0).unwrap(), Decision::FsyncFail);
        assert_eq!(c.decide(OpKind::Remove, p, 0, 0).unwrap(), Decision::Proceed);
    }

    #[test]
    fn full_rate_short_write_returns_a_strict_prefix() {
        let plan = FaultPlan { seed: 9, short_write_permille: 1000, ..FaultPlan::default() };
        let c = FaultController::new(plan);
        let p = Path::new(P);
        for _ in 0..50 {
            match c.decide(OpKind::Write, p, 0, 16).unwrap() {
                Decision::ShortWrite(n) => assert!(n < 16),
                other => panic!("expected ShortWrite, got {other:?}"),
            }
        }
        assert_eq!(c.decide(OpKind::Write, p, 0, 0).unwrap(), Decision::Proceed);
    }

    #[test]
    fn same_seed_gives_identical_decision_sequences() {
        let a = run_stream(random_plan(99), 400);
        let b = run_stream(random_plan(99), 400);
        assert_eq!(a, b);
        assert!(a.iter().any(|d| *d != Decision::Proceed));
        let other = run_stream(random_plan(100), 400);
        assert_ne!(a, other);
    }
}
