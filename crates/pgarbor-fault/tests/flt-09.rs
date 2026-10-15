//! FLT-09: write-failure behaviour (STORE-18): no success acknowledgment after ENOSPC, short
//! write or fsync failure; poisoning semantics.
//!
//! Lease FLT-09 writes this file. Required tests (FaultVfs + Container directly):
//! 1. `enospc_during_page_write_is_not_acked`: `FaultKind::Enospc` at the first page write of a
//!    commit → `commit()` Err with SQLSTATE 53100 and hint "Check free disk space."; container NOT
//!    poisoned; a retry commit after clearing faults succeeds; reopen shows only acked data.
//! 2. `short_write_is_not_acked`: ShortWrite on a page write → Err (53100 via WriteZero→ENOSPC or
//!    the following ENOSPC) and no ack.
//! 3. `fsync_failure_poisons`: FsyncFail on the data sync of a commit → Err 58030; afterwards
//!    `begin_write()` fails with "is unavailable for writing after an earlier I/O failure";
//!    snapshots still read the last acked state; crash image recovers to acked or in-flight.
//! 4. `superblock_write_failure_poisons`: IoError at the superblock write → poisoned; recovery
//!    from the crash image is acked-or-inflight.
//! 5. `disk_capacity_limit`: `disk_capacity: Some(64 KiB)` → inserting until full yields 53100,
//!    never a panic, never corruption; after reopen data == acked.
//! 6. `random_faults_property`: 64 seeds of `explore` with random enospc/short/fsync faults pass.

use pgarbor_common::{CommitSeq, KvRead, PgError};
use pgarbor_fault::plan::ScheduledFault;
use pgarbor_fault::scheduler::explore;
use pgarbor_fault::{CrashMode, DiskImage, FaultKind, FaultPlan, FaultVfs, OpKind, WorkloadSpec};
use pgarbor_storage::{Container, CreateOptions, OpenOptions, Vfs};
use std::collections::BTreeMap;
use std::ops::Bound;
use std::path::PathBuf;
use std::sync::Arc;

type State = BTreeMap<Vec<u8>, Vec<u8>>;

const HINT_DISK_FULL: &str = "Check free disk space.";
const POISON_TEXT: &str = "is unavailable for writing after an earlier I/O failure";

fn db_path() -> PathBuf {
    PathBuf::from("/fault/db.pga")
}

fn create_opts() -> CreateOptions {
    CreateOptions {
        page_size: 4096,
        db_uuid: Some(pgarbor_common::DatabaseUuid([0x5a; 16])),
        created_unix_ms: Some(0),
        ..CreateOptions::default()
    }
}

/// One commit that stores `key -> value` directly in the directory tree.
fn put_kv(c: &Container, key: &[u8], value: &[u8]) -> Result<CommitSeq, PgError> {
    let mut w = c.begin_write()?;
    let root = w.directory_root();
    let new_root = w.put(root, key, value)?.root;
    w.set_directory_root(new_root);
    w.commit()
}

fn read_all(c: &Container) -> State {
    let snap = c.snapshot();
    let dir = snap.directory();
    let mut out = State::new();
    let it = dir.scan(Bound::Unbounded, Bound::Unbounded).expect("scan directory");
    for item in it {
        let (k, v) = item.expect("scan item");
        out.insert(k, v);
    }
    out
}

fn state_a() -> State {
    let mut s = State::new();
    s.insert(b"a-key".to_vec(), b"a-val".to_vec());
    s
}

fn b_value() -> Vec<u8> {
    vec![0xB7; 100]
}

fn state_ab() -> State {
    let mut s = state_a();
    s.insert(b"b-key".to_vec(), b_value());
    s
}

fn commit_b(c: &Container) -> Result<CommitSeq, PgError> {
    put_kv(c, b"b-key", &b_value())
}

struct Run {
    vfs: FaultVfs,
    c: Container,
    /// Mutating-op count after commit A (the first op of commit B has this index).
    m0: u64,
}

/// Create the container and acknowledge commit A on a disk with `plan`.
fn setup(plan: FaultPlan) -> Run {
    let vfs = FaultVfs::new(plan);
    let dynvfs: Arc<dyn Vfs> = Arc::new(vfs.clone());
    let c = Container::create_with(dynvfs, &db_path(), &create_opts()).expect("create");
    put_kv(&c, b"a-key", b"a-val").expect("commit A");
    let m0 = vfs.controller().mutating_ops();
    Run { vfs, c, m0 }
}

/// Mutating-op indexes of interest inside commit B.
struct Probe {
    m0: u64,
    page_write: u64,
    data_sync: u64,
    sb_write: u64,
    sb_sync: u64,
}

/// Run commit B fault-free with the op log on and locate its page write, data sync, superblock
/// write and superblock sync. Runs are deterministic, so the indexes carry over to faulted runs.
fn probe() -> Probe {
    let plan = FaultPlan { record_log: true, ..FaultPlan::none(9) };
    let run = setup(plan);
    commit_b(&run.c).expect("fault-free commit B");
    let log = run.vfs.controller().log();
    let ops: Vec<(u64, OpKind)> = log
        .iter()
        .filter_map(|r| r.mutating_index.map(|i| (i, r.kind)))
        .filter(|(i, _)| *i >= run.m0)
        .collect();
    let first = |from: u64, kind: OpKind| -> u64 {
        ops.iter()
            .find(|(i, k)| *i >= from && *k == kind)
            .map(|(i, _)| *i)
            .unwrap_or_else(|| panic!("no {kind:?} at or after mutating op {from} in commit B: {ops:?}"))
    };
    let page_write = first(run.m0, OpKind::Write);
    let data_sync = first(page_write, OpKind::SyncData);
    let sb_write = first(data_sync, OpKind::Write);
    let sb_sync = first(sb_write, OpKind::SyncData);
    assert!(page_write < data_sync && data_sync < sb_write && sb_write < sb_sync);
    Probe { m0: run.m0, page_write, data_sync, sb_write, sb_sync }
}

fn faulted(sched: Vec<ScheduledFault>, mode: CrashMode) -> FaultPlan {
    FaultPlan { scheduled: sched, crash_mode: mode, ..FaultPlan::none(21) }
}

fn fault(at_op: u64, kind: FaultKind) -> ScheduledFault {
    ScheduledFault { at_op, kind }
}

const MODES: [CrashMode; 3] = [
    CrashMode::DropUnsynced,
    CrashMode::KeepUnsynced,
    CrashMode::Random { keep_permille: 500, tear_permille: 200 },
];

fn reopen(img: &DiskImage) -> Result<Container, PgError> {
    let vfs: Arc<dyn Vfs> = Arc::new(FaultVfs::from_image(img, FaultPlan::none(1)));
    Container::open_with(vfs, &db_path(), &OpenOptions::from_create(&create_opts()))
}

/// Recovery of `img` must succeed, equal one of `allowed`, and accept a new commit.
fn assert_recovers_to(img: &DiskImage, allowed: &[State], ctx: &str) {
    let c = match reopen(img) {
        Ok(c) => c,
        Err(e) => panic!("{ctx}: recovery failed: {} {}", e.sqlstate.code(), e.message),
    };
    let st = read_all(&c);
    assert!(allowed.contains(&st), "{ctx}: recovered state {st:?} is neither acked nor in-flight");
    if let Err(e) = put_kv(&c, b"post-recovery", b"x") {
        panic!("{ctx}: post-recovery commit failed: {} {}", e.sqlstate.code(), e.message);
    }
}

fn expect_err(r: Result<CommitSeq, PgError>, ctx: &str) -> PgError {
    match r {
        Ok(seq) => panic!("{ctx}: commit was acknowledged (seq {seq:?}) despite an injected failure"),
        Err(e) => e,
    }
}

fn assert_poisoned_refuses(c: &Container, ctx: &str) {
    assert!(c.is_poisoned(), "{ctx}: container should be poisoned");
    match c.begin_write() {
        Ok(_) => panic!("{ctx}: begin_write succeeded on a poisoned container"),
        Err(e) => {
            assert_eq!(e.sqlstate.code(), "58030", "{ctx}: {}", e.message);
            assert!(e.message.contains(POISON_TEXT), "{ctx}: {}", e.message);
        }
    }
}

#[test]
fn enospc_during_page_write_is_not_acked() {
    let p = probe();
    for mode in MODES {
        let run = setup(faulted(vec![fault(p.page_write, FaultKind::Enospc)], mode));
        assert_eq!(run.m0, p.m0);
        let e = expect_err(commit_b(&run.c), "enospc page write");
        assert_eq!(e.sqlstate.code(), "53100", "{}", e.message);
        assert_eq!(e.hint.as_deref(), Some(HINT_DISK_FULL));
        assert!(!run.c.is_poisoned(), "ENOSPC on a page write must not poison");
        assert_eq!(read_all(&run.c), state_a());

        // The fault was one-shot: the retry succeeds and is durable.
        let seq = commit_b(&run.c).expect("retry commit");
        assert!(seq.0 > 1);
        assert_eq!(read_all(&run.c), state_ab());

        let live = run.vfs.live_image().expect("live image");
        drop(run.c);
        assert_recovers_to(&live, &[state_ab()], "reopen after retry");
    }
}

#[test]
fn enospc_page_write_reopen_shows_only_acked() {
    let p = probe();
    for mode in MODES {
        let run = setup(faulted(vec![fault(p.page_write, FaultKind::Enospc)], mode));
        let _ = expect_err(commit_b(&run.c), "enospc page write");
        let live = run.vfs.live_image().expect("live image");
        let crash = run.vfs.crash_image().expect("crash image");
        drop(run.c);
        assert_recovers_to(&live, &[state_a()], "live image");
        assert_recovers_to(&crash, &[state_a()], "crash image");
    }
}

#[test]
fn short_write_is_not_acked() {
    let p = probe();
    // A short write followed by ENOSPC on the continuation write (or a zero-progress short write,
    // which write_all_at turns into ENOSPC itself) must surface as 53100 and never as success.
    let sched = vec![fault(p.page_write, FaultKind::ShortWrite), fault(p.page_write + 1, FaultKind::Enospc)];
    for mode in MODES {
        let run = setup(faulted(sched.clone(), mode));
        let e = expect_err(commit_b(&run.c), "short write + enospc");
        assert_eq!(e.sqlstate.code(), "53100", "{}", e.message);
        assert!(!run.c.is_poisoned());
        assert_eq!(read_all(&run.c), state_a());
        let live = run.vfs.live_image().expect("live image");
        let crash = run.vfs.crash_image().expect("crash image");
        drop(run.c);
        assert_recovers_to(&live, &[state_a()], "live image");
        assert_recovers_to(&crash, &[state_a()], "crash image");
    }
}

#[test]
fn short_write_alone_is_completed_or_failed() {
    let p = probe();
    let run = setup(faulted(vec![fault(p.page_write, FaultKind::ShortWrite)], CrashMode::DropUnsynced));
    match commit_b(&run.c) {
        Ok(_) => {
            // The storage layer wrote the remainder: the commit is genuinely durable.
            assert_eq!(read_all(&run.c), state_ab());
            let crash = run.vfs.crash_image().expect("crash image");
            drop(run.c);
            assert_recovers_to(&crash, &[state_ab()], "acked commit after short write");
        }
        Err(e) => {
            assert!(matches!(e.sqlstate.code(), "53100" | "58030"), "{}", e.message);
            let crash = run.vfs.crash_image().expect("crash image");
            drop(run.c);
            assert_recovers_to(&crash, &[state_a(), state_ab()], "failed commit after short write");
        }
    }
}

#[test]
fn fsync_failure_poisons() {
    let p = probe();
    for mode in MODES {
        let run = setup(faulted(vec![fault(p.data_sync, FaultKind::FsyncFail)], mode));
        let e = expect_err(commit_b(&run.c), "data fsync failure");
        assert_eq!(e.sqlstate.code(), "58030", "{}", e.message);
        assert_poisoned_refuses(&run.c, "after data fsync failure");
        // Snapshots still serve the last acknowledged state.
        assert_eq!(read_all(&run.c), state_a());
        let crash = run.vfs.crash_image().expect("crash image");
        let live = run.vfs.live_image().expect("live image");
        drop(run.c);
        assert_recovers_to(&crash, &[state_a(), state_ab()], "crash image");
        assert_recovers_to(&live, &[state_a(), state_ab()], "live image");
    }
}

#[test]
fn superblock_sync_failure_poisons() {
    let p = probe();
    for mode in MODES {
        let run = setup(faulted(vec![fault(p.sb_sync, FaultKind::FsyncFail)], mode));
        let e = expect_err(commit_b(&run.c), "superblock fsync failure");
        assert_eq!(e.sqlstate.code(), "58030", "{}", e.message);
        assert_poisoned_refuses(&run.c, "after superblock fsync failure");
        assert_eq!(read_all(&run.c), state_a());
        let crash = run.vfs.crash_image().expect("crash image");
        drop(run.c);
        assert_recovers_to(&crash, &[state_a(), state_ab()], "crash image");
    }
}

#[test]
fn superblock_write_failure_poisons() {
    let p = probe();
    for mode in MODES {
        let run = setup(faulted(vec![fault(p.sb_write, FaultKind::IoError)], mode));
        let e = expect_err(commit_b(&run.c), "superblock write failure");
        assert_eq!(e.sqlstate.code(), "58030", "{}", e.message);
        assert_poisoned_refuses(&run.c, "after superblock write failure");
        assert_eq!(read_all(&run.c), state_a());
        let crash = run.vfs.crash_image().expect("crash image");
        drop(run.c);
        assert_recovers_to(&crash, &[state_a(), state_ab()], "crash image");
    }
}

#[test]
fn superblock_enospc_is_not_acked() {
    let p = probe();
    let run = setup(faulted(vec![fault(p.sb_write, FaultKind::Enospc)], CrashMode::DropUnsynced));
    let e = expect_err(commit_b(&run.c), "superblock enospc");
    assert!(matches!(e.sqlstate.code(), "53100" | "58030"), "{}", e.message);
    assert_eq!(read_all(&run.c), state_a());
    let crash = run.vfs.crash_image().expect("crash image");
    drop(run.c);
    assert_recovers_to(&crash, &[state_a(), state_ab()], "crash image");
}

#[test]
fn disk_capacity_limit() {
    for mode in MODES {
        let plan = FaultPlan { disk_capacity: Some(64 * 1024), crash_mode: mode, ..FaultPlan::none(5) };
        let vfs = FaultVfs::new(plan);
        let dynvfs: Arc<dyn Vfs> = Arc::new(vfs.clone());
        let c = Container::create_with(dynvfs, &db_path(), &create_opts()).expect("create fits in 64 KiB");

        let mut acked = State::new();
        let mut inflight: Option<State> = None;
        let mut failed = false;
        for i in 0..2000u32 {
            let key = format!("key-{i:06}").into_bytes();
            let value = vec![(i % 251) as u8; 200];
            match put_kv(&c, &key, &value) {
                Ok(_) => {
                    acked.insert(key, value);
                }
                Err(e) => {
                    assert_eq!(e.sqlstate.code(), "53100", "{}", e.message);
                    let mut s = acked.clone();
                    s.insert(key, value);
                    inflight = Some(s);
                    failed = true;
                    break;
                }
            }
        }
        assert!(failed, "a 64 KiB disk must fill up");
        assert_eq!(read_all(&c), acked, "live snapshot must equal acked data");

        let mut allowed = vec![acked.clone()];
        if let Some(s) = inflight {
            allowed.push(s);
        }
        let crash = vfs.crash_image().expect("crash image");
        let live = vfs.live_image().expect("live image");
        drop(c);
        assert_recovers_to(&crash, &allowed, "capacity crash image");
        assert_recovers_to(&live, &allowed, "capacity live image");
    }
}

#[test]
fn random_faults_property() {
    // Each seed runs crash points plus one run with random enospc/short/fsync faults and no crash.
    let report = explore(&WorkloadSpec::small(0), 0..64, 1).expect("harness");
    let first = report.failures.first().map(|f| f.repro.clone());
    assert!(report.passed(), "{} failures, first: {:?}", report.failures.len(), first);
    assert!(report.runs >= 64, "only {} runs", report.runs);
}
