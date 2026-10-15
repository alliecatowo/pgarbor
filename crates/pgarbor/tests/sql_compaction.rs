mod support;
use pgarbor::{Database, OpenOptions};
use pgarbor_branches::ResetTarget;
use support::*;

#[test]
fn compact_preserves_live_branch_catalog_index_and_named_snapshot() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("compact.pga");
    let db = Database::create(&path, OpenOptions::default()).unwrap();
    seed(&mut session(&db, "main"));
    db.create_snapshot("main", "before").unwrap();
    db.create_branch("feature", "main").unwrap();
    execute(&mut session(&db, "feature"), "INSERT INTO t VALUES (9)");
    for value in 3..15 {
        execute(&mut session(&db, "main"), &format!("INSERT INTO t VALUES ({value})"));
    }
    execute(&mut session(&db, "main"), "DELETE FROM t WHERE id > 2");
    let report = db.compact().expect("compact SQL roots");
    assert!(report.new_commit_seq > report.old_commit_seq);
    assert!(report.new_page_count < report.old_page_count, "compaction must reclaim this generated garbage");
    assert_eq!(ids(&mut session(&db, "main")), expected_ids(&[1, 2]));
    assert_eq!(ids(&mut session(&db, "feature")), expected_ids(&[1, 2, 9]));
    assert_eq!(session(&db, "main").simple_query("INSERT INTO t VALUES (1)").unwrap_err().sqlstate.code(), "23505");
    db.reset_branch("main", ResetTarget::Snapshot("before".into()), false).unwrap();
    assert_eq!(ids(&mut session(&db, "main")), expected_ids(&[1, 2]));
    execute(&mut session(&db, "main"), "INSERT INTO t VALUES (20)");
    db.close().unwrap();
    drop(db);
    let reopened = Database::open(path, OpenOptions::default()).unwrap();
    assert_eq!(ids(&mut session(&reopened, "main")), expected_ids(&[1, 2, 20]));
    assert_eq!(ids(&mut session(&reopened, "feature")), expected_ids(&[1, 2, 9]));
}

#[test]
fn compact_keeps_an_open_repeatable_read_snapshot_usable() {
    let db = Database::in_memory().unwrap();
    let mut reader = session(&db, "main");
    seed(&mut reader);
    execute(&mut reader, "BEGIN ISOLATION LEVEL REPEATABLE READ");
    assert_eq!(ids(&mut reader), expected_ids(&[1, 2]));
    execute(&mut session(&db, "main"), "INSERT INTO t VALUES (3)");
    db.compact().unwrap();
    assert_eq!(ids(&mut reader), expected_ids(&[1, 2]));
    execute(&mut reader, "COMMIT");
    assert_eq!(ids(&mut reader), expected_ids(&[1, 2, 3]));
}
