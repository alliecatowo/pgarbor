//! Crash scheduler and property-test driver. Lease FLT-07 (sonnet).
//!
//! One scenario = (workload spec, fault plan): run the workload on a fresh `FaultVfs`, stop at
//! the crash (or at the end, then pull the plug anyway), build the crash `DiskImage`, reopen it
//! on a fault-free `FaultVfs::from_image`, and check:
//! 1. `Container::open_with` succeeds (recovery never reports corruption for a crash image).
//!    Exception — crash inside creation: if `Executor::create` did NOT return Ok, accept NotFound
//!    (58P01), any XX001 open error, or a clean open of an empty database (commit 1). If
//!    `Executor::create` returned Ok, commit 1 was promised durable and open MUST succeed;
//! 2. `model::read_state` equals acked or in-flight (`RefModel::check_recovered`);
//! 3. `Container::verify(&PrefixU64Roots::new(ROOT_PREFIX), VerifyLevel::Full)` has no Error
//!    issues;
//! 4. the recovered container accepts a new commit (put + commit) and reopens again cleanly.
//! Any failure is a `ScenarioReport` with `passed() == false` and a `repro` line — never a panic.
use crate::model::Verdict;
use crate::plan::FaultPlan;
use crate::workload::WorkloadSpec;
use pgarbor_common::{CommitSeq, PgError, Result};

// CONTRACT
#[derive(Clone, Debug, PartialEq)]
pub struct Scenario {
    pub workload: WorkloadSpec,
    pub plan: FaultPlan,
}

impl Scenario {
    /// Power loss at mutating op `op`, crash mode/seed from the workload seed.
    pub fn crash_at(workload: WorkloadSpec, op: u64) -> Scenario {
        let plan = FaultPlan::crash_at(workload.seed, op);
        Scenario { workload, plan }
    }
}

// CONTRACT
#[derive(Clone, Debug, PartialEq)]
pub struct ScenarioReport {
    pub seed: u64,
    pub crash_at_op: Option<u64>,
    /// Mutating ops executed before the crash/end.
    pub ops_executed: u64,
    pub crashed: bool,
    pub acked_commits: u64,
    pub acked_seq: CommitSeq,
    pub recovered_seq: Option<CommitSeq>,
    pub verdict: Verdict,
    /// Open/recovery error, if recovery failed (always a failure).
    pub recovery_error: Option<PgError>,
    pub doctor_errors: Vec<String>,
    /// Failure of the post-recovery commit/reopen check.
    pub post_check_error: Option<PgError>,
    /// e.g. `PGARBOR_FAULT_SEED=42 PGARBOR_FAULT_CRASH_AT=17 cargo test -p pgarbor-fault --test flt-08`.
    pub repro: String,
}

impl ScenarioReport {
    pub fn passed(&self) -> bool {
        self.verdict.ok() && self.recovery_error.is_none() && self.doctor_errors.is_empty() && self.post_check_error.is_none()
    }
}

// CONTRACT
#[derive(Clone, Debug, Default, PartialEq)]
pub struct MatrixReport {
    pub runs: u64,
    pub crashed_runs: u64,
    pub failures: Vec<ScenarioReport>,
}

impl MatrixReport {
    pub fn passed(&self) -> bool {
        self.failures.is_empty()
    }
}

// CONTRACT
/// Repro line for a scenario. Fully implemented.
pub fn repro_line(seed: u64, crash_at: Option<u64>) -> String {
    match crash_at {
        Some(k) => format!("PGARBOR_FAULT_SEED={seed} PGARBOR_FAULT_CRASH_AT={k}"),
        None => format!("PGARBOR_FAULT_SEED={seed}"),
    }
}

// CONTRACT
/// Run the workload with no faults and return how many mutating ops it performs (the crash-point
/// space for `crash_matrix`).
#[allow(unused_variables)]
pub fn count_mutating_ops(spec: &WorkloadSpec) -> Result<u64> {
    Err(PgError::unimplemented("FLT-07", "mutating op count"))
}

// CONTRACT
/// Run one scenario (see module docs). `Err` only for harness bugs.
#[allow(unused_variables)]
pub fn run_scenario(s: &Scenario) -> Result<ScenarioReport> {
    Err(PgError::unimplemented("FLT-07", "crash scenario"))
}

// CONTRACT
/// Crash at every `stride`-th mutating op `0, stride, 2*stride, .. < total` (stride 0 → 1), each
/// with `base.crash_mode` etc. (seed fixed to `spec.seed`). Collects failures.
#[allow(unused_variables)]
pub fn crash_matrix(spec: &WorkloadSpec, base: &FaultPlan, stride: u64) -> Result<MatrixReport> {
    Err(PgError::unimplemented("FLT-07", "crash matrix"))
}

// CONTRACT
/// Property driver: for each seed in `seeds`, `WorkloadSpec { seed, ..template }`, count ops,
/// pick `points_per_seed` crash points with `DetRng::new(seed ^ 0xC0FFEE)`, run them with
/// `CrashMode::Random{500,200}`; additionally one run per seed with random faults
/// (`enospc_permille: 5, short_write_permille: 5, fsync_fail_permille: 2`) and no crash point.
#[allow(unused_variables)]
pub fn explore(template: &WorkloadSpec, seeds: std::ops::Range<u64>, points_per_seed: u64) -> Result<MatrixReport> {
    Err(PgError::unimplemented("FLT-07", "crash exploration"))
}

/// Read `PGARBOR_FAULT_SEED` / `PGARBOR_FAULT_CRASH_AT` env vars (for repro runs in tests).
pub fn env_repro() -> Option<(u64, Option<u64>)> {
    let seed = std::env::var("PGARBOR_FAULT_SEED").ok()?.parse().ok()?;
    let at = std::env::var("PGARBOR_FAULT_CRASH_AT").ok().and_then(|v| v.parse().ok());
    Some((seed, at))
}
