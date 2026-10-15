mod support;
use pgarbor::{Database, OpenOptions, SessionOptions};
use pgarbor_branches::{BranchRef, ResetTarget};
use support::*;

#[test]
fn fork_inherits_sql_data_and_isolates_writes_and_ddl() {
    let db = Database::in_memory().unwrap();
    let mut main = session(&db, "main");
    seed(&mut main);
    db.create_branch("feature", "main").unwrap();
    let mut feature = session(&db, "feature");
    assert_eq!(ids(&mut feature), expected_ids(&[1, 2]));
    execute(&mut feature, "INSERT INTO t VALUES (3)");
    execute(&mut feature, "CREATE TABLE feature_only(id int)");
    assert_eq!(ids(&mut main), expected_ids(&[1, 2]));
    assert_eq!(ids(&mut feature), expected_ids(&[1, 2, 3]));
    assert_eq!(main.simple_query("SELECT * FROM feature_only").unwrap_err().sqlstate.code(), "42P01");
}

#[test]
fn rename_preserves_identity_and_reopen_preserves_default() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("branches.pga");
    let identity;
    {
        let db = Database::create(&path, OpenOptions::default()).unwrap();
        seed(&mut session(&db, "main"));
        identity = db.create_branch("feature", "main").unwrap().id;
        assert_eq!(db.rename_branch("feature", "renamed").unwrap().id, identity);
        db.set_default_branch("renamed").unwrap();
        db.close().unwrap();
    }
    let db = Database::open(path, OpenOptions::default()).unwrap();
    let branches = db.list_branches(false).unwrap();
    assert!(branches.iter().any(|b| b.id == identity && b.name == "renamed" && b.is_default));
    assert_eq!(ids(&mut db.connect(None, SessionOptions::default()).unwrap()), expected_ids(&[1, 2]));
}

#[test]
fn snapshot_fork_and_reset_restore_sql_state() {
    let db = Database::in_memory().unwrap();
    seed(&mut session(&db, "main"));
    db.create_snapshot("main", "before").unwrap();
    execute(&mut session(&db, "main"), "INSERT INTO t VALUES (3)");
    db.create_branch_from_snapshot("snapshot", "main", "before").unwrap();
    assert_eq!(ids(&mut session(&db, "snapshot")), expected_ids(&[1, 2]));
    db.reset_branch("main", ResetTarget::Branch(BranchRef::name("snapshot")), false).unwrap();
    assert_eq!(ids(&mut session(&db, "main")), expected_ids(&[1, 2]));
}

#[test]
fn active_sql_session_blocks_destructive_branch_admin() {
    let db = Database::in_memory().unwrap();
    db.create_branch("busy", "main").unwrap();
    let mut busy = session(&db, "busy");
    execute(&mut busy, "SELECT 1");
    assert_eq!(db.delete_branch("busy", false).unwrap_err().sqlstate.code(), "55006");
    assert_eq!(db.reset_branch("busy", ResetTarget::Branch(BranchRef::name("main")), false).unwrap_err().sqlstate.code(), "55006");
    drop(busy);
    assert!(db.delete_branch("busy", false).unwrap().deleted);
    db.restore_branch("busy", None).unwrap();
    assert_eq!(rows(&mut session(&db, "busy"), "SELECT 1"), expected_ids(&[1]));
}
