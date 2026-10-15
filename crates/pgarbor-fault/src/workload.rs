//! Deterministic workloads and their executor. Lease FLT-06.
//!
//! Executor transaction model mirrors pgarbor-core: ops between commits are buffered (like a txn
//! overlay) and applied inside ONE `WriteTxn` at `Commit`, then `commit()`; so every commit is
//! one container commit, exercising `apply`, root creation (O(1) branch = copy root pointer),
//! root drop, overflow values and the directory update.
use crate::model::RefModel;
#[allow(unused_imports)]
use crate::rng::DetRng;
use pgarbor_common::{CommitSeq, PgError, Result};
use pgarbor_storage::{Container, CreateOptions, OpenOptions, Vfs};
use std::path::PathBuf;
use std::sync::Arc;

// CONTRACT
/// Directory key prefix for workload roots (`PrefixU64Roots::new(ROOT_PREFIX)`).
pub const ROOT_PREFIX: &[u8] = b"R";

// CONTRACT
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum WorkOp {
    Put { root: String, key: Vec<u8>, value: Vec<u8> },
    Delete { root: String, key: Vec<u8> },
    /// New empty root, or O(1) copy of `from`'s committed-or-pending contents.
    CreateRoot { name: String, from: Option<String> },
    DropRoot { name: String },
    Commit,
    Abort,
    /// `Container::compact(&PrefixU64Roots::new(ROOT_PREFIX), ..)` (only between txns).
    Compact,
    /// Drop the container and reopen it (clean restart, no crash).
    Reopen,
}

// CONTRACT
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WorkloadSpec {
    pub seed: u64,
    /// Number of ops to generate (a final `Commit` is always appended).
    pub ops: usize,
    pub max_roots: usize,
    /// Keys are `format!("k{:06}", rng.below(key_space))` bytes.
    pub key_space: u64,
    /// Values are `rng.bytes(len)` with len in `0..=max_value_len`; 1 in 10 values is forced
    /// above 2048 bytes (overflow) when `max_value_len > 2048`.
    pub max_value_len: usize,
    /// Per-op probabilities in permille for the non-put kinds; the rest are puts.
    pub delete_permille: u16,
    pub commit_permille: u16,
    pub abort_permille: u16,
    pub create_root_permille: u16,
    pub drop_root_permille: u16,
    pub compact_permille: u16,
    pub reopen_permille: u16,
    pub page_size: u32,
}

impl WorkloadSpec {
    /// ~200 ops, 4 roots, small keys, some overflow values, page_size 4096 (forces splits).
    pub fn small(seed: u64) -> WorkloadSpec {
        WorkloadSpec {
            seed,
            ops: 200,
            max_roots: 4,
            key_space: 300,
            max_value_len: 5000,
            delete_permille: 150,
            commit_permille: 80,
            abort_permille: 10,
            create_root_permille: 15,
            drop_root_permille: 5,
            compact_permille: 5,
            reopen_permille: 5,
            page_size: 4096,
        }
    }
    pub fn create_options(&self) -> CreateOptions {
        CreateOptions {
            page_size: self.page_size,
            db_uuid: Some(pgarbor_common::DatabaseUuid([0x5a; 16])),
            created_unix_ms: Some(0),
            // Tiny spill threshold so crash tests exercise the WriteTxn spill path.
            spill_threshold_bytes: 16 << 10,
            ..CreateOptions::default()
        }
    }
    /// Open options matching `create_options` (same spill threshold); used for Reopen and
    /// recovery opens.
    pub fn open_options(&self) -> OpenOptions {
        OpenOptions::from_create(&self.create_options())
    }
}

// CONTRACT
/// Generate the op list. The first op is always `CreateRoot{name:"main", from: None}`. The
/// generator simulates which roots exist (committed + pending) so Put/Delete/DropRoot/CreateRoot
/// `from` always name an existing root; root names are `"r1"`, `"r2"`, ... up to `max_roots`;
/// Compact/Reopen are only emitted right after a Commit/Abort (no open txn). Ends with `Commit`.
/// Pure function of the spec (uses `DetRng::new(spec.seed)`).
#[allow(unused_variables)]
pub fn generate(spec: &WorkloadSpec) -> Result<Vec<WorkOp>> {
    Err(PgError::unimplemented("FLT-06", "workload generation"))
}

// CONTRACT
#[derive(Clone, Debug, PartialEq)]
pub enum StepOutcome {
    /// Buffered or performed.
    Done,
    CommitAcked(CommitSeq),
    /// `commit()` failed; `poisoned` = `container.is_poisoned()` afterwards.
    CommitFailed { poisoned: bool, error: PgError },
    /// An op failed because the simulated disk crashed (the controller reports `crashed()`).
    Crashed(PgError),
    /// Any other error (e.g. ENOSPC during compaction). Recorded, run continues if possible.
    Failed(PgError),
}

// CONTRACT
#[derive(Clone, Debug, Default, PartialEq)]
pub struct RunSummary {
    pub steps: usize,
    pub acked_commits: u64,
    pub failed_commits: u64,
    pub crashed: bool,
    pub poisoned: bool,
    pub last_error: Option<PgError>,
}

// CONTRACT
pub struct Executor {
    pub vfs: Arc<dyn Vfs>,
    pub path: PathBuf,
    pub container: Option<Container>,
    pub open_options: OpenOptions,
    pub model: RefModel,
    /// Ops of the open txn, replayed inside one WriteTxn at Commit.
    pub txn_ops: Vec<WorkOp>,
    /// Returns true when the underlying disk has crashed (FaultVfs controller), so errors can be
    /// classified as `Crashed`.
    pub crashed_probe: Arc<dyn Fn() -> bool + Send + Sync>,
}

impl Executor {
    /// `Container::create_with(vfs, path, opts)`, `open_options = OpenOptions::from_create(opts)`,
    /// and an empty model.
    #[allow(unused_variables)]
    pub fn create(
        vfs: Arc<dyn Vfs>,
        path: PathBuf,
        opts: &CreateOptions,
        crashed_probe: Arc<dyn Fn() -> bool + Send + Sync>,
    ) -> Result<Executor> {
        Err(PgError::unimplemented("FLT-06", "executor create"))
    }

    /// Execute one op. Put/Delete/CreateRoot/DropRoot: push onto `txn_ops` and mirror into
    /// `model`. Abort: clear + `model.abort()`. Commit: `model.commit_started()`; `begin_write`;
    /// for each buffered op in order: resolve the root's current PageId from the txn's directory
    /// (`ROOT_PREFIX + name`; missing → NONE), `put`/`delete` on it, write the new root back with
    /// `w.put(dir, key, &root.0.to_le_bytes())`; CreateRoot copies the source's current root id
    /// (O(1) branch); DropRoot deletes the directory key. Then `set_directory_root`, `commit()`;
    /// Ok → `model.commit_acked(seq)`, Err → `model.commit_failed(container.is_poisoned())`.
    /// Compact: `container.compact(&PrefixU64Roots::new(ROOT_PREFIX), &Default::default())`.
    /// Reopen: drop container, `Container::open_with(vfs, path, open_options)`.
    /// Errors are classified into `StepOutcome` (never returned as Err unless the harness itself
    /// is broken, e.g. a lease stub).
    #[allow(unused_variables)]
    pub fn step(&mut self, op: &WorkOp) -> Result<StepOutcome> {
        Err(PgError::unimplemented("FLT-06", "executor step"))
    }

    /// Run ops in order; stop at the first `Crashed`, or when the container is poisoned.
    #[allow(unused_variables)]
    pub fn run(&mut self, ops: &[WorkOp]) -> Result<RunSummary> {
        Err(PgError::unimplemented("FLT-06", "executor run"))
    }
}
