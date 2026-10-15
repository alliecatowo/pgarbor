//! PGArbor fault injection: a crash-simulating disk (`FaultVfs`/`FaultFile` implementing the
//! storage `Vfs`/`FileIo` traits), a reference model of committed state, a deterministic workload
//! generator and a crash scheduler (RFC 001 §5.2, §11.2; STORE-10/11/16/18 verification).
//!
//! CONTRACT: public items are frozen at `contracts-v1` (see `contracts/crates/pgarbor-fault.md`).
//!
//! Everything is deterministic from a `u64` seed (own SplitMix64 RNG, no `rand` dependency in the
//! simulation path), so any failing scenario is reproducible from `(seed, crash_at_op)`.
//!
//! ```ignore
//! use pgarbor_fault::*;
//! let spec = WorkloadSpec::small(42);
//! let total = scheduler::count_mutating_ops(&spec)?;
//! for k in 0..total {
//!     let r = scheduler::run_scenario(&Scenario::crash_at(spec.clone(), k))?;
//!     assert!(r.passed(), "{}", r.repro);
//! }
//! ```
pub mod controller;
pub mod disk;
pub mod file;
pub mod model;
pub mod plan;
pub mod rng;
pub mod scheduler;
pub mod vfs;
pub mod workload;

pub use controller::{Decision, FaultController, OpRecord};
pub use disk::{FileState, Pending};
pub use file::FaultFile;
pub use model::{read_state, RefModel, RootsState, TreeState, Verdict};
pub use plan::{CrashMode, FaultKind, FaultPlan, OpKind, ScheduledFault};
pub use rng::DetRng;
pub use scheduler::{MatrixReport, Scenario, ScenarioReport};
pub use vfs::{DiskImage, FaultVfs};
pub use workload::{Executor, RunSummary, StepOutcome, WorkOp, WorkloadSpec, ROOT_PREFIX};
