//! SQL-level durability checks against deterministic simulated disk failures.
mod support;
use pgarbor::{Database, OpenOptions};
use pgarbor_fault::{CrashMode, DiskImage, FaultKind, FaultPlan, FaultVfs, OpKind};
use pgarbor_types::Datum;
use std::sync::Arc;
use support::*;

const PATH: &str = "/sql-fault.pga";
const SEED: u64 = 7301;

fn options(vfs: &FaultVfs) -> OpenOptions {
    OpenOptions { vfs: Some(Arc::new(vfs.clone())), ..OpenOptions::default() }
}

fn baseline() -> DiskImage {
    let vfs = FaultVfs::new(FaultPlan::none(SEED));
    let db = Database::create(PATH, options(&vfs)).expect("create SQL fault baseline");
    seed(&mut session(&db, "main"));
    db.close().unwrap();
    vfs.live_image().unwrap()
}

fn pending_transaction(db: &Database) -> pgarbor::Session {
    let mut s = session(db, "main");
    execute(&mut s, "BEGIN");
    execute(&mut s, "INSERT INTO t VALUES (3)");
    execute(&mut s, "CREATE TABLE committed_together(id int)");
    s
}

fn commit_trace(image: &DiskImage) -> Vec<(u64, OpKind)> {
    let mut plan = FaultPlan::none(SEED);
    plan.record_log = true;
    let vfs = FaultVfs::from_image(image, plan);
    let db = Database::open(PATH, options(&vfs)).unwrap();
    let mut s = pending_transaction(&db);
    let start = vfs.controller().mutating_ops();
    execute(&mut s, "COMMIT");
    let trace: Vec<_> = vfs.controller().log().into_iter().filter_map(|op| {
        op.mutating_index.filter(|&index| index >= start).map(|index| (index, op.kind))
    }).collect();
    assert!(!trace.is_empty(), "SQL commit performed no disk operations");
    assert!(trace.iter().any(|(_, kind)| kind.is_sync()), "SQL commit performed no sync");
    trace
}

fn assert_recovered(image: &DiskImage, acknowledged: bool, repro: &str) {
    let vfs = FaultVfs::from_image(image, FaultPlan::none(SEED));
    let db = Database::open(PATH, options(&vfs)).unwrap_or_else(|e| panic!("{repro}: recovery failed: {e:?}"));
    let mut s = session(&db, "main");
    let actual = ids(&mut s);
    let old = expected_ids(&[1, 2]);
    let new = expected_ids(&[1, 2, 3]);
    assert!(actual == old || actual == new, "{repro}: torn SQL transaction: {actual:?}");
    if acknowledged {
        assert_eq!(actual, new, "{repro}: lost acknowledged SQL commit");
    }
    if actual == new {
        assert!(rows(&mut s, "SELECT id FROM committed_together").is_empty(), "{repro}");
    } else {
        assert_eq!(s.simple_query("SELECT id FROM committed_together").unwrap_err().sqlstate.code(), "42P01", "{repro}: catalog/data commit split");
    }
    assert_eq!(s.simple_query("INSERT INTO t VALUES (1)").unwrap_err().sqlstate.code(), "23505", "{repro}: unique constraint lost");
    execute(&mut s, "INSERT INTO t VALUES (4)");
    assert!(ids(&mut s).contains(&vec![Datum::Int4(4)]), "{repro}: recovered database cannot commit");
    assert!(db.engine().branches().verify(pgarbor::VerifyLevel::Full).unwrap().is_ok(), "{repro}: doctor errors");
}

#[test]
fn every_sql_commit_cutpoint_recovers_atomic_catalog_and_rows() {
    let image = baseline();
    let trace = commit_trace(&image);
    for mode in [CrashMode::DropUnsynced, CrashMode::KeepUnsynced] {
        for &(cut, _) in &trace {
            let mut plan = FaultPlan::crash_at(SEED, cut);
            plan.crash_mode = mode;
            let vfs = FaultVfs::from_image(&image, plan);
            let db = Database::open(PATH, options(&vfs)).unwrap();
            let mut s = pending_transaction(&db);
            let result = s.simple_query("COMMIT");
            assert!(vfs.controller().crashed(), "seed={SEED} cut={cut}: crash point not reached");
            assert!(result.is_err(), "seed={SEED} cut={cut}: crashed commit acknowledged");
            let crash_image = vfs.crash_image().unwrap();
            assert_recovered(&crash_image, false, &format!("seed={SEED} cut={cut} mode={mode:?}"));
        }
    }
}

#[test]
fn acknowledged_sql_commit_survives_power_loss() {
    let image = baseline();
    let mut plan = FaultPlan::none(SEED);
    plan.crash_mode = CrashMode::DropUnsynced;
    let vfs = FaultVfs::from_image(&image, plan);
    let db = Database::open(PATH, options(&vfs)).unwrap();
    execute(&mut pending_transaction(&db), "COMMIT");
    vfs.controller().trigger_crash();
    assert_recovered(&vfs.crash_image().unwrap(), true, "acknowledged SQL commit");
}

#[test]
fn sql_enospc_and_fsync_failure_never_acknowledge_failed_commit() {
    let image = baseline();
    let trace = commit_trace(&image);
    for (kind, operation) in [(FaultKind::Enospc, OpKind::Write), (FaultKind::FsyncFail, OpKind::SyncData)] {
        let &(cut, _) = trace.iter().find(|(_, op)| *op == operation).expect("required write/sync cut point");
        let mut plan = FaultPlan::fault_at(SEED, cut, kind);
        plan.crash_mode = CrashMode::DropUnsynced;
        let vfs = FaultVfs::from_image(&image, plan);
        let db = Database::open(PATH, options(&vfs)).unwrap();
        let mut s = pending_transaction(&db);
        let error = s.simple_query("COMMIT").expect_err("faulted commit must not acknowledge");
        if kind == FaultKind::Enospc {
            assert_eq!(error.sqlstate.code(), "53100");
        } else {
            assert!(s.simple_query("INSERT INTO t VALUES (5)").is_err(), "fsync failure must poison further writes");
        }
        vfs.controller().trigger_crash();
        assert_recovered(&vfs.crash_image().unwrap(), false, &format!("seed={SEED} cut={cut} fault={kind:?}"));
    }
}

#[test]
fn snapshot_free_statement_works_as_first_statement_of_fresh_and_reopened_database() {
    let vfs = FaultVfs::new(FaultPlan::none(SEED));
    let db = Database::create(PATH, options(&vfs)).unwrap();
    let mut s = session(&db, "main");
    execute(&mut s, "BEGIN");
    execute(&mut s, "CREATE TABLE t(id int primary key)");
    execute(&mut s, "INSERT INTO t VALUES (1)");
    execute(&mut s, "COMMIT");
    drop(s);
    db.close().unwrap();
    let image = vfs.live_image().unwrap();
    let vfs = FaultVfs::from_image(&image, FaultPlan::none(SEED));
    let db = Database::open(PATH, options(&vfs)).unwrap();
    let mut s = session(&db, "main");
    execute(&mut s, "BEGIN");
    execute(&mut s, "INSERT INTO t VALUES (2)");
    execute(&mut s, "COMMIT");
    assert_eq!(ids(&mut s), expected_ids(&[1, 2]));
}
