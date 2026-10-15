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
