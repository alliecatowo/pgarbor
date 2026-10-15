//! Deterministic workloads and their executor. Lease FLT-06.
//!
//! Executor transaction model mirrors pgarbor-core: ops between commits are buffered (like a txn
//! overlay) and applied inside ONE `WriteTxn` at `Commit`, then `commit()`; so every commit is
//! one container commit, exercising `apply`, root creation (O(1) branch = copy root pointer),
//! root drop, overflow values and the directory update.
use crate::model::RefModel;
use crate::rng::DetRng;
use pgarbor_common::{CommitSeq, PageId, PgError, Result};
use pgarbor_storage::{CompactOptions, Container, CreateOptions, OpenOptions, PrefixU64Roots, Vfs, WriteTxn};
use std::collections::BTreeSet;
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
    /// Kinds in threshold order (see `generate` docs).
    enum Kind {
        Delete,
        Commit,
        Abort,
        CreateRoot,
        DropRoot,
        Compact,
        Reopen,
        Put,
    }

    let mut rng = DetRng::new(spec.seed);
    let mut ops = Vec::with_capacity(spec.ops + 2);
    // Roots that exist as of the generator's current position (committed + pending).
    let mut roots: BTreeSet<String> = BTreeSet::new();
    roots.insert("main".to_string());
    ops.push(WorkOp::CreateRoot { name: "main".to_string(), from: None });
    // True while a transaction is open (buffered ops not yet committed or aborted).
    let mut in_txn = false;

    // Cumulative thresholds in permille, in the documented order.
    let t_delete = spec.delete_permille as u32;
    let t_commit = t_delete + spec.commit_permille as u32;
    let t_abort = t_commit + spec.abort_permille as u32;
    let t_create = t_abort + spec.create_root_permille as u32;
    let t_drop = t_create + spec.drop_root_permille as u32;
    let t_compact = t_drop + spec.compact_permille as u32;
    let t_reopen = t_compact + spec.reopen_permille as u32;

    for _ in 0..spec.ops {
        let r = rng.below(1000) as u32;
        let kind = if r < t_delete {
            Kind::Delete
        } else if r < t_commit {
            Kind::Commit
        } else if r < t_abort {
            Kind::Abort
        } else if r < t_create {
            Kind::CreateRoot
        } else if r < t_drop {
            Kind::DropRoot
        } else if r < t_compact {
            Kind::Compact
        } else if r < t_reopen {
            Kind::Reopen
        } else {
            Kind::Put
        };

        match kind {
            Kind::Delete => {
                let root = pick_root(&mut rng, &roots)?;
                let key = gen_key(&mut rng, spec);
                ops.push(WorkOp::Delete { root, key });
                in_txn = true;
            }
            Kind::Commit => {
                ops.push(WorkOp::Commit);
                in_txn = false;
            }
            Kind::Abort => {
                ops.push(WorkOp::Abort);
                in_txn = false;
            }
            Kind::CreateRoot => {
                let free: Vec<String> =
                    (1..=spec.max_roots).map(|i| format!("r{i}")).filter(|n| !roots.contains(n)).collect();
                if roots.len() < spec.max_roots && !free.is_empty() {
                    let name = free[rng.pick(free.len())].clone();
                    let from = if !roots.is_empty() && rng.chance(500) {
                        let existing: Vec<&String> = roots.iter().collect();
                        Some(existing[rng.pick(existing.len())].clone())
                    } else {
                        None
                    };
                    roots.insert(name.clone());
                    ops.push(WorkOp::CreateRoot { name, from });
                    in_txn = true;
                } else {
                    ops.push(gen_put(&mut rng, spec, &roots)?);
                    in_txn = true;
                }
            }
            Kind::DropRoot => {
                if roots.len() > 1 {
                    let name = pick_root(&mut rng, &roots)?;
                    roots.remove(&name);
                    ops.push(WorkOp::DropRoot { name });
                    in_txn = true;
                } else {
                    ops.push(gen_put(&mut rng, spec, &roots)?);
                    in_txn = true;
                }
            }
            Kind::Compact | Kind::Reopen => {
                if in_txn {
                    ops.push(WorkOp::Commit);
                    in_txn = false;
                } else if matches!(kind, Kind::Compact) {
                    ops.push(WorkOp::Compact);
                } else {
                    ops.push(WorkOp::Reopen);
                }
            }
            Kind::Put => {
                ops.push(gen_put(&mut rng, spec, &roots)?);
                in_txn = true;
            }
        }
    }
    // A final Commit is always appended (after the leading CreateRoot and the `spec.ops` ops).
    ops.push(WorkOp::Commit);
    Ok(ops)
}

/// Uniformly pick an existing root name.
fn pick_root(rng: &mut DetRng, roots: &BTreeSet<String>) -> Result<String> {
    if roots.is_empty() {
        return Err(PgError::internal("workload generator: no root exists"));
    }
    let idx = rng.pick(roots.len());
    roots
        .iter()
        .nth(idx)
        .cloned()
        .ok_or_else(|| PgError::internal("workload generator: root index out of range"))
}

/// `k{:06}` key drawn from `key_space`.
fn gen_key(rng: &mut DetRng, spec: &WorkloadSpec) -> Vec<u8> {
    format!("k{:06}", rng.below(spec.key_space)).into_bytes()
}

/// A Put on an existing root. One value in 10 is forced above 2048 bytes when
/// `max_value_len > 2048`; otherwise the length is uniform in `0..=max_value_len`.
fn gen_put(rng: &mut DetRng, spec: &WorkloadSpec, roots: &BTreeSet<String>) -> Result<WorkOp> {
    let root = pick_root(rng, roots)?;
    let key = gen_key(rng, spec);
    let max = spec.max_value_len as u64;
    let len = if spec.max_value_len > 2048 && rng.below(10) == 0 {
        rng.range_inclusive(2049, max)
    } else {
        rng.range_inclusive(0, max)
    };
    let value = rng.bytes(len as usize);
    Ok(WorkOp::Put { root, key, value })
}

/// Directory key of a workload root: `ROOT_PREFIX` followed by the root name.
fn root_key(name: &str) -> Vec<u8> {
    let mut k = Vec::with_capacity(ROOT_PREFIX.len() + name.len());
    k.extend_from_slice(ROOT_PREFIX);
    k.extend_from_slice(name.as_bytes());
    k
}

/// Current PageId of a named root in the txn's directory (missing → `PageId::NONE`).
fn resolve_root(w: &WriteTxn<'_>, dir: PageId, name: &str) -> Result<PageId> {
    match w.get(dir, &root_key(name))? {
        None => Ok(PageId::NONE),
        Some(v) => {
            let len = v.len();
            let bytes: [u8; 8] = v.as_slice().try_into().map_err(|_| {
                PgError::corruption(format!("root reference for \"{name}\" has {len} bytes, expected 8"))
            })?;
            Ok(PageId(u64::from_le_bytes(bytes)))
        }
    }
}

/// Apply the buffered ops of one transaction inside a single `WriteTxn` and commit it.
/// Any error (begin, op, or commit) drops the txn, which aborts it.
fn apply_txn(c: &Container, ops: &[WorkOp]) -> Result<CommitSeq> {
    let mut w = c.begin_write()?;
    let mut dir = w.directory_root();
    for op in ops {
        match op {
            WorkOp::Put { root, key, value } => {
                let cur = resolve_root(&w, dir, root)?;
                let new_root = w.put(cur, key, value)?.root;
                dir = w.put(dir, &root_key(root), &new_root.0.to_le_bytes())?.root;
            }
            WorkOp::Delete { root, key } => {
                let cur = resolve_root(&w, dir, root)?;
                let new_root = w.delete(cur, key)?.root;
                dir = w.put(dir, &root_key(root), &new_root.0.to_le_bytes())?.root;
            }
            WorkOp::CreateRoot { name, from } => {
                let src = match from {
                    Some(f) => resolve_root(&w, dir, f)?,
                    None => PageId::NONE,
                };
                dir = w.put(dir, &root_key(name), &src.0.to_le_bytes())?.root;
            }
            WorkOp::DropRoot { name } => {
                dir = w.delete(dir, &root_key(name))?.root;
            }
            WorkOp::Commit | WorkOp::Abort | WorkOp::Compact | WorkOp::Reopen => {
                return Err(PgError::internal("workload executor: control op inside a transaction"));
            }
        }
    }
    w.set_directory_root(dir);
    w.commit()
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
        let container = Container::create_with(vfs.clone(), &path, opts)?;
        Ok(Executor {
            vfs,
            path,
            container: Some(container),
            open_options: OpenOptions::from_create(opts),
            model: RefModel::new(),
            txn_ops: Vec::new(),
            crashed_probe,
        })
    }

    /// Classify an error from a non-commit op: `Crashed` when the disk has crashed, else `Failed`.
    fn classify(&self, err: PgError) -> StepOutcome {
        if (self.crashed_probe)() { StepOutcome::Crashed(err) } else { StepOutcome::Failed(err) }
    }

    fn is_poisoned(&self) -> bool {
        self.container.as_ref().is_some_and(Container::is_poisoned)
    }

    /// Commit the buffered ops (the `Commit` path of `step`).
    fn commit_txn(&mut self) -> StepOutcome {
        self.model.commit_started();
        let ops = std::mem::take(&mut self.txn_ops);
        let result = match self.container.as_ref() {
            None => Err(PgError::internal("workload executor: container is not open")),
            Some(c) => apply_txn(c, &ops),
        };
        match result {
            Ok(seq) => {
                self.model.commit_acked(seq);
                StepOutcome::CommitAcked(seq)
            }
            Err(err) => {
                if (self.crashed_probe)() {
                    return StepOutcome::Crashed(err);
                }
                let poisoned = self.is_poisoned();
                self.model.commit_failed(poisoned);
                StepOutcome::CommitFailed { poisoned, error: err }
            }
        }
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
        match op {
            WorkOp::Put { root, key, value } => {
                self.model.put(root, key, value);
                self.txn_ops.push(op.clone());
                Ok(StepOutcome::Done)
            }
            WorkOp::Delete { root, key } => {
                self.model.delete(root, key);
                self.txn_ops.push(op.clone());
                Ok(StepOutcome::Done)
            }
            WorkOp::CreateRoot { name, from } => {
                self.model.create_root(name, from.as_deref());
                self.txn_ops.push(op.clone());
                Ok(StepOutcome::Done)
            }
            WorkOp::DropRoot { name } => {
                self.model.drop_root(name);
                self.txn_ops.push(op.clone());
                Ok(StepOutcome::Done)
            }
            WorkOp::Commit => Ok(self.commit_txn()),
            WorkOp::Abort => {
                self.txn_ops.clear();
                self.model.abort();
                Ok(StepOutcome::Done)
            }
            WorkOp::Compact => {
                let Some(c) = self.container.as_ref() else {
                    return Ok(self.classify(PgError::internal("workload executor: container is not open")));
                };
                match c.compact(&PrefixU64Roots::new(ROOT_PREFIX), &CompactOptions::default()) {
                    Ok(report) => {
                        // Compaction is itself a commit (CommitSeq + 1).
                        self.model.acked_seq = report.new_commit_seq;
                        Ok(StepOutcome::Done)
                    }
                    Err(err) => Ok(self.classify(err)),
                }
            }
            WorkOp::Reopen => {
                // Clean restart: drop the handle first so the file lock is released.
                self.container = None;
                match Container::open_with(self.vfs.clone(), &self.path, &self.open_options) {
                    Ok(c) => {
                        self.container = Some(c);
                        Ok(StepOutcome::Done)
                    }
                    Err(err) => Ok(self.classify(err)),
                }
            }
        }
    }

    /// Run ops in order; stop at the first `Crashed`, or when the container is poisoned.
    pub fn run(&mut self, ops: &[WorkOp]) -> Result<RunSummary> {
        let mut summary = RunSummary::default();
        for op in ops {
            if self.is_poisoned() {
                summary.poisoned = true;
                break;
            }
            let outcome = self.step(op)?;
            summary.steps += 1;
            match outcome {
                StepOutcome::Done => {}
                StepOutcome::CommitAcked(_) => summary.acked_commits += 1,
                StepOutcome::CommitFailed { poisoned, error } => {
                    summary.failed_commits += 1;
                    summary.poisoned |= poisoned;
                    summary.last_error = Some(error);
                }
                StepOutcome::Crashed(error) => {
                    summary.crashed = true;
                    summary.last_error = Some(error);
                    break;
                }
                StepOutcome::Failed(error) => summary.last_error = Some(error),
            }
            if self.is_poisoned() {
                summary.poisoned = true;
                break;
            }
        }
        Ok(summary)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn generate_is_pure_and_well_formed() {
        let spec = WorkloadSpec::small(42);
        let a = generate(&spec).unwrap();
        assert_eq!(a, generate(&spec).unwrap());
        assert_ne!(a, generate(&WorkloadSpec::small(43)).unwrap());
        assert_eq!(a.first(), Some(&WorkOp::CreateRoot { name: "main".to_string(), from: None }));
        assert_eq!(a.last(), Some(&WorkOp::Commit));

        let mut open = false;
        let mut roots: BTreeSet<String> = BTreeSet::new();
        for op in &a {
            match op {
                WorkOp::Commit | WorkOp::Abort => open = false,
                WorkOp::Compact | WorkOp::Reopen => assert!(!open, "{op:?} inside a txn"),
                _ => open = true,
            }
            match op {
                WorkOp::CreateRoot { name, from } => {
                    if let Some(f) = from {
                        assert!(roots.contains(f));
                    }
                    roots.insert(name.clone());
                    assert!(roots.len() <= spec.max_roots);
                }
                WorkOp::DropRoot { name } => {
                    assert!(roots.remove(name));
                    assert!(!roots.is_empty());
                }
                WorkOp::Put { root, key, value } => {
                    assert!(roots.contains(root));
                    assert!(key.starts_with(b"k"));
                    assert!(value.len() <= spec.max_value_len);
                }
                WorkOp::Delete { root, .. } => assert!(roots.contains(root)),
                _ => {}
            }
        }
    }

    #[test]
    fn no_fault_run_acks_every_commit() {
        let spec = WorkloadSpec::small(7);
        let ops = generate(&spec).unwrap();
        let commits = ops.iter().filter(|o| matches!(o, WorkOp::Commit)).count();
        let vfs: Arc<dyn Vfs> = Arc::new(pgarbor_storage::MemVfs::new());
        let mut ex = Executor::create(vfs, PathBuf::from("wl.pga"), &spec.create_options(), Arc::new(|| false))
            .unwrap();
        let summary = ex.run(&ops).unwrap();
        assert!(!summary.crashed);
        assert!(!summary.poisoned);
        assert_eq!(summary.failed_commits, 0);
        assert_eq!(summary.acked_commits as usize, commits);
        assert!(ex.txn_ops.is_empty());
        assert!(!ex.model.in_txn);
        assert!(ex.model.inflight.is_none());
    }
}
