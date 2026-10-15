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
use crate::model::{read_state, RefModel, Verdict};
use crate::plan::{CrashMode, FaultPlan};
use crate::rng::DetRng;
use crate::vfs::{DiskImage, FaultVfs};
use crate::workload::{generate, Executor, RunSummary, WorkloadSpec, ROOT_PREFIX};
use pgarbor_common::{CommitSeq, PageId, PgError, Result, SqlState};
use pgarbor_storage::{Container, IssueSeverity, PrefixU64Roots, VerifyLevel};
use std::path::{Path, PathBuf};
use std::sync::Arc;

const DB_PATH: &str = "/db/test.pga";
const MARKER_ROOT: &str = "zz_marker";

/// True when an error is a lease stub (a harness/build problem, not a storage verdict).
fn is_stub(e: &PgError) -> bool {
    e.detail.as_deref().is_some_and(|d| d.contains("pgarbor-unimplemented"))
}

fn crash_probe(vfs: &FaultVfs) -> Arc<dyn Fn() -> bool + Send + Sync> {
    let ctl = vfs.controller().clone();
    Arc::new(move || ctl.crashed())
}

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
pub fn count_mutating_ops(spec: &WorkloadSpec) -> Result<u64> {
    let vfs = FaultVfs::new(FaultPlan::none(spec.seed));
    let mut exec = Executor::create(Arc::new(vfs.clone()), PathBuf::from(DB_PATH), &spec.create_options(), crash_probe(&vfs))?;
    let ops = generate(spec)?;
    let summary: RunSummary = exec.run(&ops)?;
    if summary.crashed || summary.failed_commits > 0 || summary.last_error.is_some() {
        return Err(PgError::internal(format!(
            "fault-free workload (seed {}) did not run cleanly: {:?}",
            spec.seed, summary.last_error
        )));
    }
    Ok(vfs.controller().mutating_ops())
}

// CONTRACT
/// Run one scenario (see module docs). `Err` only for harness bugs.
pub fn run_scenario(s: &Scenario) -> Result<ScenarioReport> {
    let spec = &s.workload;
    let path = PathBuf::from(DB_PATH);
    let vfs = FaultVfs::new(s.plan.clone());

    // Phase 1: create + run the workload, then always pull the plug.
    let created = Executor::create(Arc::new(vfs.clone()), path.clone(), &spec.create_options(), crash_probe(&vfs));
    let mut exec = match created {
        Ok(e) => Some(e),
        Err(e) => {
            if is_stub(&e) {
                return Err(e);
            }
            None
        }
    };
    let create_ok = exec.is_some();
    let (model, summary) = match exec.as_mut() {
        Some(ex) => {
            let ops = generate(spec)?;
            let summary = ex.run(&ops)?;
            (ex.model.clone(), summary)
        }
        None => (RefModel::new(), RunSummary::default()),
    };
    let ctl = vfs.controller();
    let crashed = ctl.crashed();
    let ops_executed = ctl.mutating_ops();
    ctl.trigger_crash();
    let image = vfs.crash_image()?;
    // Dropping the executor after the image is taken: its drop cannot affect the image.
    drop(exec);

    recover_and_check(s, &path, create_ok, model, summary, crashed, ops_executed, &image)
}

#[allow(clippy::too_many_arguments)]
fn recover_and_check(
    s: &Scenario,
    path: &Path,
    create_ok: bool,
    mut model: RefModel,
    summary: RunSummary,
    crashed: bool,
    ops_executed: u64,
    image: &DiskImage,
) -> Result<ScenarioReport> {
    let spec = &s.workload;
    let mut report = ScenarioReport {
        seed: s.plan.seed,
        crash_at_op: s.plan.crash_at_op,
        ops_executed,
        crashed,
        acked_commits: summary.acked_commits,
        acked_seq: model.acked_seq,
        recovered_seq: None,
        verdict: Verdict::MatchesAcked,
        recovery_error: None,
        doctor_errors: Vec::new(),
        post_check_error: None,
        repro: repro_line(s.plan.seed, s.plan.crash_at_op),
    };

    // Phase 2: recover on a fault-free disk holding the crash image.
    let vfs2 = FaultVfs::from_image(image, FaultPlan::none(s.plan.seed));
    let opened = Container::open_with(Arc::new(vfs2.clone()), path, &spec.open_options());
    let c = match opened {
        Ok(c) => c,
        Err(e) => {
            if is_stub(&e) {
                return Err(e);
            }
            // Crash inside creation: no durable database yet is acceptable.
            let tolerated = !create_ok && (e.sqlstate == SqlState::UNDEFINED_FILE || e.sqlstate == SqlState::DATA_CORRUPTED);
            if !tolerated {
                report.verdict = Verdict::Violation(format!("recovery open failed: {}", e.message));
                report.recovery_error = Some(e);
            }
            return Ok(report);
        }
    };

    // Phase 3: state oracle.
    match read_state(&c, ROOT_PREFIX) {
        Ok((state, seq)) => {
            report.recovered_seq = Some(seq);
            report.verdict = model.check_recovered(&state, seq)?;
            report.acked_seq = model.acked_seq;
        }
        Err(e) => {
            if is_stub(&e) {
                return Err(e);
            }
            report.verdict = Verdict::Violation(format!("reading recovered state failed: {}", e.message));
            report.recovery_error = Some(e);
            return Ok(report);
        }
    }

    // Phase 4: doctor.
    match c.verify(&PrefixU64Roots::new(ROOT_PREFIX), VerifyLevel::Full) {
        Ok(rep) => {
            report.doctor_errors = rep
                .issues
                .iter()
                .filter(|i| i.severity == IssueSeverity::Error)
                .map(|i| i.message.clone())
                .collect();
        }
        Err(e) => {
            if is_stub(&e) {
                return Err(e);
            }
            report.doctor_errors.push(format!("verify failed: {}", e.message));
        }
    }

    // Phase 5: the recovered container must accept a new commit and reopen cleanly.
    if let Err(e) = post_check(c, &vfs2, path, spec) {
        if is_stub(&e) {
            return Err(e);
        }
        report.post_check_error = Some(e);
    }
    Ok(report)
}

/// Put a marker root, commit, close, reopen and check the marker is visible.
fn post_check(c: Container, vfs2: &FaultVfs, path: &Path, spec: &WorkloadSpec) -> Result<()> {
    let mut key = ROOT_PREFIX.to_vec();
    key.extend_from_slice(MARKER_ROOT.as_bytes());
    {
        let mut w = c.begin_write()?;
        let root = w.put(PageId::NONE, b"marker", b"1")?.root;
        let dir = w.directory_root();
        let new_dir = w.put(dir, &key, &root.0.to_le_bytes())?.root;
        w.set_directory_root(new_dir);
        w.commit()?;
    }
    drop(c);
    let c2 = Container::open_with(Arc::new(vfs2.clone()), path, &spec.open_options())?;
    let (state, _) = read_state(&c2, ROOT_PREFIX)?;
    let ok = state
        .get(MARKER_ROOT)
        .and_then(|t| t.get(b"marker".as_slice()))
        .is_some_and(|v| v.as_slice() == b"1");
    if ok {
        Ok(())
    } else {
        Err(PgError::corruption("post-recovery marker commit not visible after reopen"))
    }
}

fn record(out: &mut MatrixReport, r: ScenarioReport) {
    out.runs += 1;
    if r.crashed {
        out.crashed_runs += 1;
    }
    if !r.passed() {
        out.failures.push(r);
    }
}

// CONTRACT
/// Crash at every `stride`-th mutating op `0, stride, 2*stride, .. < total` (stride 0 → 1), each
/// with `base.crash_mode` etc. (seed fixed to `spec.seed`). Collects failures.
pub fn crash_matrix(spec: &WorkloadSpec, base: &FaultPlan, stride: u64) -> Result<MatrixReport> {
    let total = count_mutating_ops(spec)?;
    let stride = stride.max(1);
    let mut out = MatrixReport::default();
    let mut k = 0u64;
    while k < total {
        let plan = FaultPlan { seed: spec.seed, crash_at_op: Some(k), ..base.clone() };
        let r = run_scenario(&Scenario { workload: spec.clone(), plan })?;
        record(&mut out, r);
        k += stride;
    }
    Ok(out)
}

// CONTRACT
/// Property driver: for each seed in `seeds`, `WorkloadSpec { seed, ..template }`, count ops,
/// pick `points_per_seed` crash points with `DetRng::new(seed ^ 0xC0FFEE)`, run them with
/// `CrashMode::Random{500,200}`; additionally one run per seed with random faults
/// (`enospc_permille: 5, short_write_permille: 5, fsync_fail_permille: 2`) and no crash point.
pub fn explore(template: &WorkloadSpec, seeds: std::ops::Range<u64>, points_per_seed: u64) -> Result<MatrixReport> {
    let mut out = MatrixReport::default();
    for seed in seeds {
        let spec = WorkloadSpec { seed, ..template.clone() };
        let total = count_mutating_ops(&spec)?;
        let mut rng = DetRng::new(seed ^ 0xC0FFEE);
        for _ in 0..points_per_seed {
            let k = rng.below(total.max(1));
            let plan = FaultPlan {
                seed,
                crash_at_op: Some(k),
                crash_mode: CrashMode::Random { keep_permille: 500, tear_permille: 200 },
                ..FaultPlan::default()
            };
            let r = run_scenario(&Scenario { workload: spec.clone(), plan })?;
            record(&mut out, r);
        }
        let plan = FaultPlan {
            seed,
            enospc_permille: 5,
            short_write_permille: 5,
            fsync_fail_permille: 2,
            ..FaultPlan::default()
        };
        let r = run_scenario(&Scenario { workload: spec, plan })?;
        record(&mut out, r);
    }
    Ok(out)
}

/// Read `PGARBOR_FAULT_SEED` / `PGARBOR_FAULT_CRASH_AT` env vars (for repro runs in tests).
pub fn env_repro() -> Option<(u64, Option<u64>)> {
    let seed = std::env::var("PGARBOR_FAULT_SEED").ok()?.parse().ok()?;
    let at = std::env::var("PGARBOR_FAULT_CRASH_AT").ok().and_then(|v| v.parse().ok());
    Some((seed, at))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn repro_lines() {
        assert_eq!(repro_line(42, Some(17)), "PGARBOR_FAULT_SEED=42 PGARBOR_FAULT_CRASH_AT=17");
        assert_eq!(repro_line(42, None), "PGARBOR_FAULT_SEED=42");
    }

    #[test]
    fn scenario_crash_at_uses_workload_seed() {
        let s = Scenario::crash_at(WorkloadSpec::small(9), 3);
        assert_eq!(s.plan.seed, 9);
        assert_eq!(s.plan.crash_at_op, Some(3));
    }

    #[test]
    fn empty_matrix_passes() {
        assert!(MatrixReport::default().passed());
    }

    #[test]
    fn crash_at_first_op_is_tolerated() {
        let r = run_scenario(&Scenario::crash_at(WorkloadSpec::small(2), 0)).unwrap();
        assert!(r.passed(), "{r:?}");
        assert!(r.crashed);
    }

    #[test]
    fn small_matrix_passes() {
        let spec = WorkloadSpec::small(1);
        let rep = crash_matrix(&spec, &FaultPlan::none(1), 7).unwrap();
        assert!(rep.runs > 0);
        assert!(rep.passed(), "{:?}", rep.failures.iter().map(|f| (&f.repro, &f.verdict, &f.recovery_error)).collect::<Vec<_>>());
    }

    #[test]
    fn explore_smoke() {
        let rep = explore(&WorkloadSpec::small(0), 0..2, 3).unwrap();
        assert_eq!(rep.runs, 8);
    }
}
