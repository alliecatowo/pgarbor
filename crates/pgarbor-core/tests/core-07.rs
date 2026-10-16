//! Integration tests for COMMIT (lease CORE-07). MemVfs + SyncMode::Off, several sessions on one
//! engine.
use pgarbor_branches::{BranchRef, CreateOptions, CreateSource, ResetOptions, ResetTarget};
use pgarbor_common::{KvRead, KvWrite};
use pgarbor_core::{
    BranchAdminOptions, CommitOutcome, Engine, EngineOptions, IsolationLevel, LockOutcome, SessionHandle,
    SessionRequest, Txn, TxnKind, TxnOptions, WaitLimits,
};
use pgarbor_storage::{MemVfs, SyncMode};
use std::path::Path;
use std::sync::{mpsc, Arc, Barrier};
use std::time::Duration;

const PATH: &str = "/core07.pga";

fn opts(vfs: &Arc<MemVfs>) -> EngineOptions {
    EngineOptions { vfs: Some(vfs.clone()), sync: SyncMode::Off, ..EngineOptions::default() }
}

fn new_engine() -> (Arc<MemVfs>, Arc<Engine>) {
    let vfs = Arc::new(MemVfs::new());
    let e = Engine::create(Path::new(PATH), &opts(&vfs)).expect("create");
    (vfs, e)
}

fn session(e: &Arc<Engine>, branch: &str) -> SessionHandle {
    e.attach_session(&SessionRequest::embedded(branch)).expect("attach")
}

fn begin(e: &Arc<Engine>, s: &SessionHandle, iso: IsolationLevel) -> Txn {
    let o = TxnOptions { kind: TxnKind::Explicit, isolation: iso, read_only: false, deferrable: false };
    e.begin(s, &o).expect("begin")
}

fn put(t: &mut Txn, k: &[u8], v: &[u8]) {
    t.begin_statement().unwrap();
    t.kv("INSERT").put(k, v).unwrap();
}

fn get(t: &mut Txn, k: &[u8]) -> Option<Vec<u8>> {
    t.begin_statement().unwrap();
    t.kv("SELECT").get(k).unwrap()
}

fn committed(o: CommitOutcome) -> bool {
    matches!(o, CommitOutcome::Committed { seq: Some(_) })
}

#[test]
fn north_star_survives_reopen() {
    let (vfs, e) = new_engine();
    {
        let s = session(&e, "main");
        let mut t = begin(&e, &s, IsolationLevel::ReadCommitted);
        put(&mut t, b"k1", b"v1");
        assert!(committed(t.commit().unwrap()));
    }
    e.close().unwrap();
    drop(e);
    let e2 = Engine::open(Path::new(PATH), &opts(&vfs)).expect("reopen");
    let s = session(&e2, "main");
    let mut t = begin(&e2, &s, IsolationLevel::ReadCommitted);
    assert_eq!(get(&mut t, b"k1"), Some(b"v1".to_vec()));
    t.rollback().unwrap();
}

#[test]
fn uncommitted_invisible_and_rollback_leaves_nothing() {
    let (_v, e) = new_engine();
    let (sa, sb) = (session(&e, "main"), session(&e, "main"));
    let mut a = begin(&e, &sa, IsolationLevel::ReadCommitted);
    put(&mut a, b"k1", b"v1");
    let mut b = begin(&e, &sb, IsolationLevel::ReadCommitted);
    assert_eq!(get(&mut b, b"k1"), None);
    a.rollback().unwrap();
    assert_eq!(get(&mut b, b"k1"), None);
    b.rollback().unwrap();
    let mut c = begin(&e, &sb, IsolationLevel::ReadCommitted);
    assert_eq!(get(&mut c, b"k1"), None);
}

#[test]
fn rc_sees_commit_rr_does_not() {
    let (_v, e) = new_engine();
    let (sa, sb, sc) = (session(&e, "main"), session(&e, "main"), session(&e, "main"));
    let mut rc = begin(&e, &sb, IsolationLevel::ReadCommitted);
    let mut rr = begin(&e, &sc, IsolationLevel::RepeatableRead);
    assert_eq!(get(&mut rc, b"k"), None);
    assert_eq!(get(&mut rr, b"k"), None);
    let mut a = begin(&e, &sa, IsolationLevel::ReadCommitted);
    put(&mut a, b"k", b"v");
    a.commit().unwrap();
    assert_eq!(get(&mut rc, b"k"), Some(b"v".to_vec()));
    assert_eq!(get(&mut rr, b"k"), None);
}

fn concurrent_update(iso: IsolationLevel) {
    let (_v, e) = new_engine();
    let (sa, sb) = (session(&e, "main"), session(&e, "main"));
    let mut seed = begin(&e, &sa, IsolationLevel::ReadCommitted);
    put(&mut seed, b"k", b"v0");
    seed.commit().unwrap();

    let mut a = begin(&e, &sa, IsolationLevel::ReadCommitted);
    a.begin_statement().unwrap();
    assert_eq!(a.lock_key_for_write(b"k").unwrap(), LockOutcome::Unchanged);
    a.kv("UPDATE").put(b"k", b"v1").unwrap();

    let (tx, rx) = mpsc::channel();
    let e2 = e.clone();
    let h = std::thread::spawn(move || {
        let mut b = begin(&e2, &sb, iso);
        b.begin_statement().unwrap();
        tx.send(()).unwrap();
        let r = b.lock_key_for_write(b"k");
        let _ = b.rollback();
        r
    });
    rx.recv().unwrap();
    std::thread::sleep(Duration::from_millis(150));
    assert!(committed(a.commit().unwrap()));
    let r = h.join().unwrap();
    match iso {
        IsolationLevel::ReadCommitted => assert_eq!(r.unwrap(), LockOutcome::Changed(Some(b"v1".to_vec()))),
        _ => assert_eq!(r.unwrap_err().sqlstate.code(), "40001"),
    }
}

#[test]
fn concurrent_update_rc() {
    concurrent_update(IsolationLevel::ReadCommitted);
}

#[test]
fn concurrent_update_rr() {
    concurrent_update(IsolationLevel::RepeatableRead);
}

#[test]
fn empty_commit_does_not_bump_seq() {
    let (_v, e) = new_engine();
    let s = session(&e, "main");
    let before = e.container().commit_seq();
    let mut t = begin(&e, &s, IsolationLevel::ReadCommitted);
    t.begin_statement().unwrap();
    assert_eq!(t.commit().unwrap(), CommitOutcome::Committed { seq: None });
    assert_eq!(e.container().commit_seq(), before);
}

#[test]
fn branch_isolation() {
    let (_v, e) = new_engine();
    e.create_branch("feature", &CreateSource::branch("main"), &CreateOptions::default()).unwrap();
    let sm = session(&e, "main");
    let sf = session(&e, "feature");
    let db = pgarbor_branches::POSTGRES_DATABASE_OID;
    let main_before = e.branch_head(&sm.branch(), db).unwrap().record.data_root;
    let mut t = begin(&e, &sf, IsolationLevel::ReadCommitted);
    put(&mut t, b"k", b"feature-only");
    t.commit().unwrap();
    let main_after = e.branch_head(&sm.branch(), db).unwrap().record.data_root;
    assert_eq!(main_before, main_after);
    let mut m = begin(&e, &sm, IsolationLevel::ReadCommitted);
    assert_eq!(get(&mut m, b"k"), None);
    let mut f = begin(&e, &sf, IsolationLevel::ReadCommitted);
    assert_eq!(get(&mut f, b"k"), Some(b"feature-only".to_vec()));
}

#[test]
fn commit_in_failed_state_rolls_back() {
    let (_v, e) = new_engine();
    let s = session(&e, "main");
    let before = e.container().commit_seq();
    let mut t = begin(&e, &s, IsolationLevel::ReadCommitted);
    put(&mut t, b"k", b"v");
    t.mark_failed();
    assert_eq!(t.commit().unwrap(), CommitOutcome::RolledBack);
    assert_eq!(e.container().commit_seq(), before);
    let mut r = begin(&e, &s, IsolationLevel::ReadCommitted);
    assert_eq!(get(&mut r, b"k"), None);
}

#[test]
fn forced_reset_under_open_txn_fails_commit() {
    let (_v, e) = new_engine();
    e.create_branch("feature", &CreateSource::branch("main"), &CreateOptions::default()).unwrap();
    let sf = session(&e, "feature");
    let mut t = begin(&e, &sf, IsolationLevel::ReadCommitted);
    put(&mut t, b"k", b"v");
    let admin = BranchAdminOptions { force: true, caller: None };
    e.reset_branch(&BranchRef::name("feature"), &ResetTarget::Parent, &ResetOptions::default(), &admin).unwrap();
    let before = e.container().commit_seq();
    let err = t.commit().unwrap_err();
    assert_eq!(err.sqlstate.code(), "55000");
    assert_eq!(e.container().commit_seq(), before);
    let sf2 = session(&e, "feature");
    let mut r = begin(&e, &sf2, IsolationLevel::ReadCommitted);
    assert_eq!(get(&mut r, b"k"), None);
}

fn crosswise(
    e: Arc<Engine>,
    s: SessionHandle,
    first: &'static [u8],
    second: &'static [u8],
    bar: Arc<Barrier>,
) -> std::thread::JoinHandle<Result<(), pgarbor_common::PgError>> {
    std::thread::spawn(move || {
        let mut t = begin(&e, &s, IsolationLevel::ReadCommitted);
        t.set_limits(WaitLimits { deadlock_timeout_ms: 50, ..WaitLimits::default() });
        t.begin_statement().unwrap();
        t.lock_key_for_write(first).unwrap();
        bar.wait();
        match t.lock_key_for_write(second) {
            Ok(_) => t.commit().map(|_| ()),
            Err(err) => {
                let _ = t.rollback();
                Err(err)
            }
        }
    })
}

#[test]
fn deadlock_detected_once() {
    let (_v, e) = new_engine();
    let (sa, sb) = (session(&e, "main"), session(&e, "main"));
    let mut seed = begin(&e, &sa, IsolationLevel::ReadCommitted);
    put(&mut seed, b"k1", b"a");
    put(&mut seed, b"k2", b"b");
    seed.commit().unwrap();

    let barrier = Arc::new(Barrier::new(2));
    let h1 = crosswise(e.clone(), sa, b"k1", b"k2", barrier.clone());
    let h2 = crosswise(e.clone(), sb, b"k2", b"k1", barrier.clone());
    let (r1, r2) = (h1.join().unwrap(), h2.join().unwrap());
    let errs: Vec<_> = [r1, r2].into_iter().filter_map(|r| r.err()).collect();
    assert_eq!(errs.len(), 1, "exactly one victim");
    assert_eq!(errs[0].sqlstate.code(), "40P01");
    assert!(errs[0].detail.as_deref().unwrap_or("").contains("ShareLock on transaction"));
}


#[test]
fn reused_key_lock_keeps_latest_committed_outcome() {
    use pgarbor_core::{LockMode, WaitPolicy};
    // A single frozen statement snapshot across concurrent insert, update and delete.
    for iso in [IsolationLevel::ReadCommitted, IsolationLevel::RepeatableRead] {
        for (old, new) in [(None, Some(&b"new"[..])), (Some(&b"old"[..]), Some(&b"new"[..])), (Some(&b"old"[..]), None)] {
            let (_v, e) = new_engine();
            let (sa, sb) = (session(&e, "main"), session(&e, "main"));
            if let Some(old) = old {
                let mut seed = begin(&e, &sb, IsolationLevel::ReadCommitted);
                put(&mut seed, b"k", old);
                seed.commit().unwrap();
            }
            let mut a = begin(&e, &sa, iso);
            a.begin_statement().unwrap();
            let original_seq = a.snapshot().unwrap().commit_seq;
            let mut b = begin(&e, &sb, IsolationLevel::ReadCommitted);
            b.begin_statement().unwrap();
            if let Some(new) = new {
                b.kv("UPDATE").put(b"k", new).unwrap();
            } else {
                b.kv("DELETE").delete(b"k").unwrap();
            }
            b.commit().unwrap();
            let changed = LockOutcome::Changed(new.map(<[u8]>::to_vec));
            for mode in [LockMode::ForKeyShare, LockMode::ForKeyShare, LockMode::ForUpdate, LockMode::ForUpdate] {
                assert_eq!(a.lock_key(b"k", mode, WaitPolicy::Block).unwrap(), changed);
                assert_eq!(a.kv("SELECT").get(b"k").unwrap(), old.map(<[u8]>::to_vec));
                assert_eq!(a.snapshot().unwrap().commit_seq, original_seq);
            }
            if iso == IsolationLevel::RepeatableRead {
                // A retry must not bypass serialization checking merely because we own the lock.
                for _ in 0..2 {
                    assert_eq!(a.lock_key_for_write(b"k").unwrap_err().sqlstate.code(), "40001");
                }
            } else {
                a.kv("UPDATE").put(b"k", b"own").unwrap();
                assert_eq!(a.kv("SELECT").get(b"k").unwrap(), Some(b"own".to_vec()));
                assert_eq!(a.lock_key_for_write(b"k").unwrap(), changed);
                assert_eq!(a.kv("SELECT").get(b"k").unwrap(), Some(b"own".to_vec()));
            }
            a.rollback().unwrap();
        }
    }
}


#[test]
fn reused_key_lock_preserves_own_writes_under_rc_and_rr() {
    for iso in [IsolationLevel::ReadCommitted, IsolationLevel::RepeatableRead] {
        let (_v, e) = new_engine();
        let sa = session(&e, "main");
        let mut a = begin(&e, &sa, iso);
        a.begin_statement().unwrap();
        a.kv("INSERT").put(b"k", b"own").unwrap();
        for _ in 0..2 {
            assert_eq!(a.lock_key_for_write(b"k").unwrap(), LockOutcome::Unchanged);
            assert_eq!(a.kv("SELECT").get(b"k").unwrap(), Some(b"own".to_vec()));
        }
        a.kv("DELETE").delete(b"k").unwrap();
        assert_eq!(a.lock_key_for_write(b"k").unwrap(), LockOutcome::Unchanged);
        assert_eq!(a.kv("SELECT").get(b"k").unwrap(), None);
        a.rollback().unwrap();
    }
}

/// Write skew between two SERIALIZABLE transactions (PG doc mvcc applevel-consistency): both read
/// both rows, each writes a different one; the first committer wins and the pivot fails 40001.
#[test]
fn serializable_write_skew_pivot_fails_at_second_commit() {
    let (_v, e) = new_engine();
    let (sseed, sa, sb) = (session(&e, "main"), session(&e, "main"), session(&e, "main"));
    {
        let mut seed = begin(&e, &sseed, IsolationLevel::ReadCommitted);
        put(&mut seed, b"r1", b"100");
        put(&mut seed, b"r2", b"200");
        assert!(committed(seed.commit().unwrap()));
    }
    let mut a = begin(&e, &sa, IsolationLevel::Serializable);
    let mut b = begin(&e, &sb, IsolationLevel::Serializable);
    assert_eq!(get(&mut a, b"r1"), Some(b"100".to_vec()));
    assert_eq!(get(&mut a, b"r2"), Some(b"200".to_vec()));
    assert_eq!(get(&mut b, b"r1"), Some(b"100".to_vec()));
    assert_eq!(get(&mut b, b"r2"), Some(b"200".to_vec()));
    put(&mut a, b"r1", b"110");
    put(&mut b, b"r2", b"220");
    assert!(committed(a.commit().unwrap()));
    let err = b.commit().err().expect("the pivot must be canceled at commit");
    assert_eq!(err.sqlstate.code(), "40001");
}
