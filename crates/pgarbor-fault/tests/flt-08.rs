//! FLT-08: crash-matrix integration tests (STORE-10, STORE-11; RFC 001 §14 Phase 1 gate).
//!
//! Lease FLT-08 writes this file. Required tests:
//! 1. `crash_at_every_op_small`: `crash_matrix(&WorkloadSpec::small(1), &FaultPlan::default(), 1)`
//!    passes (every mutating op is a crash point; default CrashMode::Random{500,200}).
//! 2. `crash_modes`: the same with `CrashMode::DropUnsynced` and `CrashMode::KeepUnsynced`
//!    (stride 3) pass.
//! 3. `explore_seeds`: `explore(&WorkloadSpec::small(0), 0..32, 8)` passes. Mark the larger
//!    variant (`0..512`) `#[ignore]` for the nightly campaign.
//! 4. `acknowledged_commit_survives_drop_unsynced`: with DropUnsynced, crash immediately after a
//!    `commit()` returned Ok (next mutating op) → recovered state == acked state exactly.
//! 5. `torn_superblock_falls_back`: with `sector_size: 16` (REQUIRED: at the default 512 the
//!    92 meaningful superblock bytes can never tear) and `CrashMode::Random{keep 0, tear 1000}`,
//!    crash right after a superblock write → recovery picks the older slot (MatchesAcked), never
//!    corruption. Assert the recovery report shows the torn slot as BadChecksum at least once
//!    across seeds 0..64.
//! 7. `explore_small_sectors`: `explore` variant with `sector_size: 16` (tears page headers and
//!    superblocks) over seeds 0..16 passes.
//! 6. `repro_from_env`: if `scheduler::env_repro()` is Some, run exactly that scenario and print
//!    its report (lets humans replay failures).
//! Every assertion message must include `report.repro`.

use pgarbor_common::{CommitSeq, Result};
use pgarbor_fault::scheduler::{
    count_mutating_ops, crash_matrix, env_repro, explore, repro_line, run_scenario, MatrixReport, Scenario,
};
use pgarbor_fault::workload::generate;
use pgarbor_fault::{
    read_state, CrashMode, DetRng, Executor, FaultPlan, FaultVfs, OpKind, StepOutcome, Verdict, WorkloadSpec,
    ROOT_PREFIX,
};
use pgarbor_storage::{Container, SlotStatus, Vfs};
use std::path::PathBuf;
use std::sync::Arc;

fn db_path() -> PathBuf {
    PathBuf::from("/sim/flt08.pgarbor")
}

fn describe(m: &MatrixReport) -> String {
    let mut s = format!("runs={} crashed={} failures={}", m.runs, m.crashed_runs, m.failures.len());
    for f in m.failures.iter().take(8) {
        s.push_str(&format!(
            "\n  repro: {} | verdict={:?} recovery_error={:?} doctor={:?} post={:?}",
            f.repro, f.verdict, f.recovery_error, f.doctor_errors, f.post_check_error
        ));
    }
    s
}

/// Outcome of a hand-driven crash scenario (needed to inspect the recovery report, which
/// `ScenarioReport` does not carry).
struct Manual {
    verdict: Verdict,
    slots: [SlotStatus; 2],
    acked_seq: CommitSeq,
    recovered_seq: CommitSeq,
    repro: String,
}

/// Run `spec` under `plan`, pull the plug, recover from the crash image. `None` when the crash
/// hit inside container creation (nothing promised yet).
fn manual(spec: &WorkloadSpec, plan: FaultPlan) -> Result<Option<Manual>> {
    let repro = repro_line(plan.seed, plan.crash_at_op);
    let vfs = FaultVfs::new(plan.clone());
    let ctl = vfs.controller().clone();
    let probe_ctl = ctl.clone();
    let mut exec = match Executor::create(
        Arc::new(vfs.clone()) as Arc<dyn Vfs>,
        db_path(),
        &spec.create_options(),
        Arc::new(move || probe_ctl.crashed()),
    ) {
        Ok(e) => e,
        Err(_) => return Ok(None),
    };
    let ops = generate(spec)?;
    exec.run(&ops)?;
    ctl.trigger_crash();
    let image = vfs.crash_image()?;
    let acked_seq = exec.model.acked_seq;
    let rvfs = FaultVfs::from_image(&image, FaultPlan::none(plan.seed));
    let c = Container::open_with(Arc::new(rvfs) as Arc<dyn Vfs>, &db_path(), &spec.open_options())?;
    let slots = c.recovery_report().slots.clone();
    let (state, seq) = read_state(&c, ROOT_PREFIX)?;
    let verdict = exec.model.check_recovered(&state, seq)?;
    Ok(Some(Manual { verdict, slots, acked_seq, recovered_seq: seq, repro }))
}

#[test]
fn crash_at_every_op_small() {
    let spec = WorkloadSpec::small(1);
    let total = count_mutating_ops(&spec).expect("count ops");
    assert!(total > 0, "workload performs no mutating ops");
    let rep = crash_matrix(&spec, &FaultPlan::default(), 1).expect("crash_matrix");
    assert_eq!(rep.runs, total, "matrix must cover every mutating op ({})", describe(&rep));
    assert!(rep.crashed_runs > 0, "no run crashed: {}", describe(&rep));
    assert!(rep.passed(), "{}", describe(&rep));
}

#[test]
fn crash_modes() {
    let spec = WorkloadSpec::small(1);
    for mode in [CrashMode::DropUnsynced, CrashMode::KeepUnsynced] {
        let plan = FaultPlan { crash_mode: mode, ..FaultPlan::default() };
        let rep = crash_matrix(&spec, &plan, 3).expect("crash_matrix");
        assert!(rep.runs > 0, "mode {mode:?}: no runs");
        assert!(rep.passed(), "mode {mode:?}: {}", describe(&rep));
    }
}

#[test]
fn explore_seeds() {
    let rep = explore(&WorkloadSpec::small(0), 0..32, 8).expect("explore");
    assert!(rep.runs > 0, "explore ran nothing");
    assert!(rep.passed(), "{}", describe(&rep));
}

#[test]
#[ignore = "nightly campaign"]
fn explore_seeds_large() {
    let rep = explore(&WorkloadSpec::small(0), 0..512, 8).expect("explore");
    assert!(rep.passed(), "{}", describe(&rep));
}

#[test]
fn acknowledged_commit_survives_drop_unsynced() {
    let spec = WorkloadSpec::small(1);
    let ops = generate(&spec).expect("generate");

    // Pass 1 (no faults): mutating-op counter right after each acknowledged commit.
    let mut points: Vec<u64> = Vec::new();
    {
        let vfs = FaultVfs::new(FaultPlan::none(spec.seed));
        let ctl = vfs.controller().clone();
        let probe = ctl.clone();
        let mut exec = Executor::create(
            Arc::new(vfs.clone()) as Arc<dyn Vfs>,
            db_path(),
            &spec.create_options(),
            Arc::new(move || probe.crashed()),
        )
        .expect("create");
        for op in &ops {
            if let StepOutcome::CommitAcked(_) = exec.step(op).expect("step") {
                points.push(ctl.mutating_ops());
            }
        }
    }
    assert!(!points.is_empty(), "workload acknowledged no commits");

    // Pass 2: crash at the very next mutating op after each ack, losing everything unsynced.
    for k in points {
        let plan = FaultPlan { crash_mode: CrashMode::DropUnsynced, ..FaultPlan::crash_at(spec.seed, k) };
        let repro = repro_line(spec.seed, Some(k));
        let m = manual(&spec, plan)
            .unwrap_or_else(|e| panic!("harness/recovery error {e:?}; {repro}"))
            .unwrap_or_else(|| panic!("crash hit container creation; {repro}"));
        assert!(
            m.verdict == Verdict::MatchesAcked,
            "verdict {:?} (acked seq {:?}, recovered {:?}); {}",
            m.verdict,
            m.acked_seq,
            m.recovered_seq,
            m.repro
        );
        assert!(
            m.recovered_seq >= m.acked_seq,
            "recovered seq {:?} older than acked {:?}; {}",
            m.recovered_seq,
            m.acked_seq,
            m.repro
        );
    }
}

#[test]
fn torn_superblock_falls_back() {
    let mut saw_bad_checksum = false;
    let mut attempts = 0u32;
    for seed in 0..64u64 {
        let spec = WorkloadSpec { ops: 40, ..WorkloadSpec::small(seed) };
        let base = FaultPlan {
            sector_size: 16,
            crash_mode: CrashMode::Random { keep_permille: 0, tear_permille: 1000 },
            record_log: true,
            ..FaultPlan::none(seed)
        };

        // Locate superblock writes (the two slots live in the first 8 KiB) in a fault-free run.
        let vfs = FaultVfs::new(base.clone());
        let ctl = vfs.controller().clone();
        let probe = ctl.clone();
        let mut exec = Executor::create(
            Arc::new(vfs.clone()) as Arc<dyn Vfs>,
            db_path(),
            &spec.create_options(),
            Arc::new(move || probe.crashed()),
        )
        .expect("create");
        exec.run(&generate(&spec).expect("generate")).expect("run");
        let sb_writes: Vec<u64> = ctl
            .log()
            .iter()
            .filter(|r| r.kind == OpKind::Write && r.offset < 8192)
            .filter_map(|r| r.mutating_index)
            .collect();
        assert!(!sb_writes.is_empty(), "seed {seed}: no superblock writes seen");

        // Crash right after a few of them (the next mutating op, normally the sync).
        let mut rng = DetRng::new(seed ^ 0x5B5B);
        for _ in 0..3 {
            let idx = sb_writes[rng.pick(sb_writes.len())];
            let plan = FaultPlan { record_log: false, crash_at_op: Some(idx + 1), ..base.clone() };
            let repro = repro_line(seed, plan.crash_at_op);
            let outcome = manual(&spec, plan).unwrap_or_else(|e| panic!("harness/recovery error {e:?}; {repro}"));
            let Some(m) = outcome else { continue };
            attempts += 1;
            assert!(m.verdict.ok(), "verdict {:?}; {}", m.verdict, m.repro);
            assert!(m.recovered_seq >= m.acked_seq, "older than acked; {}", m.repro);
            if m.slots.iter().any(|s| matches!(s, SlotStatus::BadChecksum { .. })) {
                saw_bad_checksum = true;
            }
        }
    }
    assert!(attempts > 0, "no torn-superblock scenario ran");
    assert!(saw_bad_checksum, "no recovery report showed a BadChecksum slot across seeds 0..64 ({attempts} runs)");
}

#[test]
fn explore_small_sectors() {
    for seed in 0..16u64 {
        let spec = WorkloadSpec::small(seed);
        let total = count_mutating_ops(&spec).expect("count ops");
        let mut rng = DetRng::new(seed ^ 0xC0FFEE);
        for _ in 0..8 {
            let k = rng.below(total.max(1));
            let plan = FaultPlan { sector_size: 16, ..FaultPlan::crash_at(seed, k) };
            let rep = run_scenario(&Scenario { workload: spec.clone(), plan }).expect("run_scenario");
            assert!(
                rep.passed(),
                "sector16 {}: verdict={:?} recovery={:?} doctor={:?} post={:?}",
                rep.repro,
                rep.verdict,
                rep.recovery_error,
                rep.doctor_errors,
                rep.post_check_error
            );
        }
    }
}

#[test]
fn repro_from_env() {
    let Some((seed, at)) = env_repro() else { return };
    let spec = WorkloadSpec::small(seed);
    let plan = match at {
        Some(k) => FaultPlan::crash_at(seed, k),
        None => FaultPlan::none(seed),
    };
    let rep = run_scenario(&Scenario { workload: spec, plan }).expect("run_scenario");
    println!("{rep:#?}");
    assert!(rep.passed(), "{}: {rep:?}", rep.repro);
}
