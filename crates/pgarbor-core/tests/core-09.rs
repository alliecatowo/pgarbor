//! Integration tests for the lock manager (lease CORE-09).
use pgarbor_common::{BranchId, TxnId};
use pgarbor_core::locks::table::LockScope;
use pgarbor_core::session::{Activity, BackendState, Interrupts, SessionShared};
use pgarbor_core::{AcquireResult, LockManager, LockMode, LockTag, SessionId, SubXactId, WaitLimits, WaitPolicy};
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

fn sess(id: i32) -> Arc<SessionShared> {
    Arc::new(SessionShared {
        id: SessionId(id),
        branch: BranchId([1; 16]),
        branch_name: Mutex::new("main".into()),
        database_oid: 5,
        database_name: "postgres".into(),
        user: "postgres".into(),
        application_name: Mutex::new(String::new()),
        client_addr: None,
        client_port: None,
        backend_start_ms: 0,
        cancel_key: vec![0; 4],
        interrupts: Interrupts::new(),
        activity: Mutex::new(Activity {
            state: BackendState::Idle,
            query: String::new(),
            query_start_ms: None,
            xact_start_ms: None,
            state_change_ms: 0,
            backend_xid: None,
            txn: None,
            wait: None,
        }),
        sequences: Mutex::new(Default::default()),
    })
}

fn rel(n: u32) -> LockTag {
    LockTag::Relation { branch: BranchId([1; 16]), database: 5, relid: n }
}

fn tx(t: u64, s: u32) -> LockScope {
    LockScope::Transaction { txn: TxnId(t), subxact: SubXactId(s) }
}

fn patient() -> WaitLimits {
    WaitLimits { lock_timeout_ms: 0, deadlock_timeout_ms: 600_000, statement_deadline: None }
}

fn acq(m: &LockManager, s: &SessionShared, scope: LockScope, tag: &LockTag, mode: LockMode, limits: &WaitLimits) -> pgarbor_common::Result<AcquireResult> {
    m.acquire(s, scope, tag, mode, WaitPolicy::Block, limits)
}

fn wait_until_waiting(m: &LockManager, sid: SessionId) {
    let end = Instant::now() + Duration::from_secs(5);
    loop {
        if m.snapshot().unwrap().iter().any(|r| r.session == sid && !r.granted) {
            return;
        }
        assert!(Instant::now() < end, "session {sid} never queued");
        thread::sleep(Duration::from_millis(2));
    }
}

fn spawn_acquire(
    m: &Arc<LockManager>,
    s: &Arc<SessionShared>,
    scope: LockScope,
    tag: LockTag,
    mode: LockMode,
    limits: WaitLimits,
) -> mpsc::Receiver<pgarbor_common::Result<AcquireResult>> {
    let (tx_, rx) = mpsc::channel();
    let (m, s) = (Arc::clone(m), Arc::clone(s));
    thread::spawn(move || {
        let r = m.acquire(&s, scope, &tag, mode, WaitPolicy::Block, &limits);
        let _ = tx_.send(r);
    });
    rx
}

#[test]
fn exclusive_blocks_until_release() {
    let m = Arc::new(LockManager::new());
    let (s1, s2) = (sess(1), sess(2));
    let t = rel(16384);
    assert_eq!(acq(&m, &s1, tx(1, 0), &t, LockMode::Exclusive, &patient()).unwrap(), AcquireResult::Acquired { newly: true });
    let rx = spawn_acquire(&m, &s2, tx(2, 0), t.clone(), LockMode::Exclusive, patient());
    wait_until_waiting(&m, SessionId(2));
    assert!(rx.recv_timeout(Duration::from_millis(100)).is_err());
    assert_eq!(m.blocking_sessions(SessionId(2)).unwrap(), vec![SessionId(1)]);
    assert!(s2.activity().wait.is_some());
    m.release_txn(SessionId(1), TxnId(1)).unwrap();
    let r = rx.recv_timeout(Duration::from_secs(5)).unwrap().unwrap();
    assert_eq!(r, AcquireResult::Acquired { newly: true });
    assert!(m.holds(SessionId(2), &t, LockMode::Exclusive).unwrap());
    assert!(s2.activity().wait.is_none());
    assert!(m.blocking_sessions(SessionId(2)).unwrap().is_empty());
}

#[test]
fn share_share_compatible_and_reentrant() {
    let m = LockManager::new();
    let (s1, s2) = (sess(1), sess(2));
    let t = rel(16384);
    assert_eq!(acq(&m, &s1, tx(1, 0), &t, LockMode::Share, &patient()).unwrap(), AcquireResult::Acquired { newly: true });
    assert_eq!(acq(&m, &s2, tx(2, 0), &t, LockMode::Share, &patient()).unwrap(), AcquireResult::Acquired { newly: true });
    assert_eq!(acq(&m, &s1, tx(1, 0), &t, LockMode::Share, &patient()).unwrap(), AcquireResult::Acquired { newly: false });
}

#[test]
fn same_session_never_blocks() {
    let m = LockManager::new();
    let s1 = sess(1);
    let t = rel(16384);
    acq(&m, &s1, tx(1, 0), &t, LockMode::AccessExclusive, &patient()).unwrap();
    assert_eq!(acq(&m, &s1, tx(1, 0), &t, LockMode::AccessExclusive, &patient()).unwrap(), AcquireResult::Acquired { newly: false });
    assert_eq!(acq(&m, &s1, tx(1, 0), &t, LockMode::AccessShare, &patient()).unwrap(), AcquireResult::Acquired { newly: true });
    assert_eq!(acq(&m, &s1, LockScope::Session, &t, LockMode::Exclusive, &patient()).unwrap(), AcquireResult::Acquired { newly: true });
}

#[test]
fn fifo_third_request_queues_behind_waiting_exclusive() {
    let m = Arc::new(LockManager::new());
    let (s1, s2, s3) = (sess(1), sess(2), sess(3));
    let t = rel(16384);
    acq(&m, &s1, tx(1, 0), &t, LockMode::Share, &patient()).unwrap();
    let rx2 = spawn_acquire(&m, &s2, tx(2, 0), t.clone(), LockMode::Exclusive, patient());
    wait_until_waiting(&m, SessionId(2));
    // Share is compatible with the holder but not with the queued Exclusive: NoWait must refuse.
    assert_eq!(
        m.acquire(&s3, tx(3, 0), &t, LockMode::Share, WaitPolicy::NoWait, &patient()).unwrap(),
        AcquireResult::NotAvailable
    );
    let rx3 = spawn_acquire(&m, &s3, tx(3, 0), t.clone(), LockMode::Share, patient());
    wait_until_waiting(&m, SessionId(3));
    assert_eq!(m.blocking_sessions(SessionId(3)).unwrap(), vec![SessionId(2)]);
    m.release_txn(SessionId(1), TxnId(1)).unwrap();
    assert!(rx2.recv_timeout(Duration::from_secs(5)).unwrap().is_ok());
    assert!(rx3.recv_timeout(Duration::from_millis(100)).is_err());
    m.release_txn(SessionId(2), TxnId(2)).unwrap();
    assert!(rx3.recv_timeout(Duration::from_secs(5)).unwrap().is_ok());
}

#[test]
fn nowait_and_skip_locked_do_not_queue() {
    let m = LockManager::new();
    let (s1, s2) = (sess(1), sess(2));
    let t = rel(16384);
    acq(&m, &s1, tx(1, 0), &t, LockMode::Exclusive, &patient()).unwrap();
    for p in [WaitPolicy::NoWait, WaitPolicy::SkipLocked] {
        assert_eq!(m.acquire(&s2, tx(2, 0), &t, LockMode::Exclusive, p, &patient()).unwrap(), AcquireResult::NotAvailable);
    }
    assert!(m.snapshot().unwrap().iter().all(|r| r.granted));
}

#[test]
fn lock_timeout_removes_waiter() {
    let m = LockManager::new();
    let (s1, s2) = (sess(1), sess(2));
    let t = rel(16384);
    acq(&m, &s1, tx(1, 0), &t, LockMode::Exclusive, &patient()).unwrap();
    let limits = WaitLimits { lock_timeout_ms: 50, ..patient() };
    let start = Instant::now();
    let e = acq(&m, &s2, tx(2, 0), &t, LockMode::Exclusive, &limits).unwrap_err();
    assert_eq!(e.sqlstate.code(), "55P03");
    assert!(start.elapsed() >= Duration::from_millis(50));
    assert!(m.snapshot().unwrap().iter().all(|r| r.granted));
    assert!(s2.activity().wait.is_none());
    // The slot is free again once the holder leaves.
    m.release_txn(SessionId(1), TxnId(1)).unwrap();
    assert_eq!(acq(&m, &s2, tx(2, 0), &t, LockMode::Exclusive, &patient()).unwrap(), AcquireResult::Acquired { newly: true });
}

#[test]
fn statement_deadline_cancels_wait() {
    let m = LockManager::new();
    let (s1, s2) = (sess(1), sess(2));
    let t = rel(16384);
    acq(&m, &s1, tx(1, 0), &t, LockMode::Exclusive, &patient()).unwrap();
    let limits = WaitLimits { statement_deadline: Some(Instant::now() + Duration::from_millis(40)), ..patient() };
    let e = acq(&m, &s2, tx(2, 0), &t, LockMode::Exclusive, &limits).unwrap_err();
    assert_eq!(e.sqlstate.code(), "57014");
    assert!(m.snapshot().unwrap().iter().all(|r| r.granted));
}

#[test]
fn cancel_during_wait() {
    let m = Arc::new(LockManager::new());
    let (s1, s2) = (sess(1), sess(2));
    let t = rel(16384);
    acq(&m, &s1, tx(1, 0), &t, LockMode::Exclusive, &patient()).unwrap();
    let rx = spawn_acquire(&m, &s2, tx(2, 0), t.clone(), LockMode::Exclusive, patient());
    wait_until_waiting(&m, SessionId(2));
    s2.interrupts.request_cancel();
    m.wake(SessionId(2)).unwrap();
    let e = rx.recv_timeout(Duration::from_secs(2)).unwrap().unwrap_err();
    assert_eq!(e.sqlstate.code(), "57014");
    assert!(m.snapshot().unwrap().iter().all(|r| r.granted));
}

#[test]
fn terminate_during_wait() {
    let m = Arc::new(LockManager::new());
    let (s1, s2) = (sess(1), sess(2));
    let t = rel(16384);
    acq(&m, &s1, tx(1, 0), &t, LockMode::Exclusive, &patient()).unwrap();
    let rx = spawn_acquire(&m, &s2, tx(2, 0), t.clone(), LockMode::Exclusive, patient());
    wait_until_waiting(&m, SessionId(2));
    s2.interrupts.request_terminate();
    m.wake(SessionId(2)).unwrap();
    let e = rx.recv_timeout(Duration::from_secs(2)).unwrap().unwrap_err();
    assert_eq!(e.sqlstate.code(), "57P01");
}

#[test]
fn two_session_deadlock_has_one_victim() {
    let m = Arc::new(LockManager::new());
    let (s1, s2) = (sess(1), sess(2));
    let (a, b) = (rel(16384), rel(16385));
    acq(&m, &s1, tx(1, 0), &a, LockMode::Exclusive, &patient()).unwrap();
    acq(&m, &s2, tx(2, 0), &b, LockMode::Exclusive, &patient()).unwrap();
    let limits = WaitLimits { lock_timeout_ms: 0, deadlock_timeout_ms: 30, statement_deadline: None };
    let (rtx, rrx) = mpsc::channel();
    let mut handles = Vec::new();
    for (s, txn, want) in [(Arc::clone(&s1), 1u64, b.clone()), (Arc::clone(&s2), 2u64, a.clone())] {
        let (m2, rtx) = (Arc::clone(&m), rtx.clone());
        handles.push(thread::spawn(move || {
            let r = m2.acquire(&s, tx(txn, 0), &want, LockMode::Exclusive, WaitPolicy::Block, &limits);
            if r.is_err() {
                m2.release_txn(s.id, TxnId(txn)).unwrap();
            }
            let _ = rtx.send((txn, r));
        }));
        if txn == 1 {
            wait_until_waiting(&m, SessionId(1));
        }
    }
    drop(rtx);
    let mut results: Vec<(u64, pgarbor_common::Result<AcquireResult>)> = Vec::new();
    for _ in 0..2 {
        results.push(rrx.recv_timeout(Duration::from_secs(10)).unwrap());
    }
    for h in handles {
        h.join().unwrap();
    }
    let errs: Vec<_> = results.iter().filter_map(|(_, r)| r.as_ref().err()).collect();
    assert_eq!(errs.len(), 1);
    assert_eq!(errs[0].sqlstate.code(), "40P01");
    assert!(results.iter().any(|(_, r)| matches!(r, Ok(AcquireResult::Acquired { newly: true }))));
}

#[test]
fn release_subxacts_releases_only_newer() {
    let m = LockManager::new();
    let s1 = sess(1);
    let id = SessionId(1);
    for (i, n) in [(0u32, 16384u32), (1, 16385), (2, 16386)] {
        acq(&m, &s1, tx(9, i), &rel(n), LockMode::Share, &patient()).unwrap();
    }
    m.release_subxacts(id, TxnId(9), SubXactId(1)).unwrap();
    assert!(m.holds(id, &rel(16384), LockMode::Share).unwrap());
    assert!(!m.holds(id, &rel(16385), LockMode::Share).unwrap());
    assert!(!m.holds(id, &rel(16386), LockMode::Share).unwrap());
}

#[test]
fn reassign_subxacts_merges() {
    let m = LockManager::new();
    let s1 = sess(1);
    let id = SessionId(1);
    let t = rel(16384);
    acq(&m, &s1, tx(9, 0), &t, LockMode::Share, &patient()).unwrap();
    acq(&m, &s1, tx(9, 1), &t, LockMode::Share, &patient()).unwrap();
    acq(&m, &s1, tx(9, 2), &t, LockMode::Share, &patient()).unwrap();
    acq(&m, &s1, tx(9, 2), &rel(16385), LockMode::Share, &patient()).unwrap();
    m.reassign_subxacts(id, TxnId(9), SubXactId(1), SubXactId(0)).unwrap();
    assert_eq!(m.snapshot().unwrap().len(), 2);
    // Everything now lives at subxact 0, so rolling back to 1 releases nothing.
    m.release_subxacts(id, TxnId(9), SubXactId(1)).unwrap();
    assert!(m.holds(id, &t, LockMode::Share).unwrap());
    assert!(m.holds(id, &rel(16385), LockMode::Share).unwrap());
    // Merged count: three acquisitions, one release_txn drops them all.
    m.release_txn(id, TxnId(9)).unwrap();
    assert!(m.snapshot().unwrap().is_empty());
}

#[test]
fn release_session_keeps_txn_locks() {
    let m = LockManager::new();
    let s1 = sess(1);
    let id = SessionId(1);
    acq(&m, &s1, LockScope::Session, &rel(16384), LockMode::Exclusive, &patient()).unwrap();
    acq(&m, &s1, tx(3, 0), &rel(16385), LockMode::Exclusive, &patient()).unwrap();
    assert_eq!(m.release_session(id).unwrap(), 1);
    assert!(!m.holds(id, &rel(16384), LockMode::Exclusive).unwrap());
    assert!(m.holds(id, &rel(16385), LockMode::Exclusive).unwrap());
}

#[test]
fn release_one_and_filtered_release() {
    let m = LockManager::new();
    let s1 = sess(1);
    let id = SessionId(1);
    let t = rel(16384);
    acq(&m, &s1, LockScope::Session, &t, LockMode::Exclusive, &patient()).unwrap();
    acq(&m, &s1, LockScope::Session, &t, LockMode::Exclusive, &patient()).unwrap();
    assert!(m.release_one(id, LockScope::Session, &t, LockMode::Exclusive).unwrap());
    assert!(m.holds(id, &t, LockMode::Exclusive).unwrap());
    assert!(m.release_one(id, LockScope::Session, &t, LockMode::Exclusive).unwrap());
    assert!(!m.release_one(id, LockScope::Session, &t, LockMode::Exclusive).unwrap());
    acq(&m, &s1, LockScope::Session, &t, LockMode::Share, &patient()).unwrap();
    assert_eq!(m.release_session_where(id, &|_| false).unwrap(), 0);
    assert_eq!(m.release_session_where(id, &|_| true).unwrap(), 1);
}

#[test]
fn snapshot_lists_granted_and_waiting() {
    let m = Arc::new(LockManager::new());
    let (s1, s2) = (sess(1), sess(2));
    let t = rel(16384);
    acq(&m, &s1, tx(1, 0), &t, LockMode::Exclusive, &patient()).unwrap();
    let rx = spawn_acquire(&m, &s2, tx(2, 0), t.clone(), LockMode::Share, patient());
    wait_until_waiting(&m, SessionId(2));
    let snap = m.snapshot().unwrap();
    assert_eq!(snap.len(), 2);
    assert!(snap[0].granted && snap[0].session == SessionId(1) && snap[0].wait_start_unix_ms.is_none());
    assert!(!snap[1].granted && snap[1].session == SessionId(2) && snap[1].wait_start_unix_ms.is_some());
    assert_eq!(snap[0].tag.locktype, "relation");
    assert_eq!(snap[0].tag.relation, Some(16384));
    m.release_txn(SessionId(1), TxnId(1)).unwrap();
    assert!(rx.recv_timeout(Duration::from_secs(5)).unwrap().is_ok());
}

#[test]
fn row_mode_on_relation_tag_is_internal_error() {
    let m = LockManager::new();
    let s1 = sess(1);
    let e = acq(&m, &s1, tx(1, 0), &rel(16384), LockMode::ForUpdate, &patient()).unwrap_err();
    assert_eq!(e.sqlstate.code(), "XX000");
}
