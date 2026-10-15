mod support;
use pgarbor::{Database, OpenOptions};
use pgarbor_branches::{BranchRef, DetachOptions, DetachStrategy, HandleOptions};
use support::*;

#[test]
fn sql_export_is_standalone_with_fresh_identity_and_independent_writes() {
    let dir = tempfile::tempdir().unwrap();
    let source = dir.path().join("source.pga");
    let dest = dir.path().join("export.pga");
    let db = Database::create(&source, OpenOptions::default()).unwrap();
    seed(&mut session(&db, "main"));
    db.create_branch("feature", "main").unwrap();
    execute(&mut session(&db, "feature"), "INSERT INTO t VALUES (3)");
    let report = db.export_branch("feature", &dest, &DetachOptions::default()).expect("export SQL branch");
    assert!(report.verified);
    assert_ne!(report.new_db_uuid, db.engine().container().db_uuid());
    let exported = Database::open(&dest, OpenOptions::default()).unwrap();
    assert_eq!(exported.list_branches(false).unwrap().len(), 1);
    assert_eq!(ids(&mut session(&exported, "main")), expected_ids(&[1, 2, 3]));
    execute(&mut session(&exported, "main"), "INSERT INTO t VALUES (4)");
    assert_eq!(ids(&mut session(&db, "feature")), expected_ids(&[1, 2, 3]));
    assert_eq!(ids(&mut session(&db, "main")), expected_ids(&[1, 2]));
}

#[test]
fn detach_preserves_named_snapshot_and_refuses_existing_destination() {
    let dir = tempfile::tempdir().unwrap();
    let db = Database::create(dir.path().join("source.pga"), OpenOptions::default()).unwrap();
    seed(&mut session(&db, "main"));
    db.create_snapshot("main", "before").unwrap();
    execute(&mut session(&db, "main"), "INSERT INTO t VALUES (3)");
    let dest = dir.path().join("detached.pga");
    let options = DetachOptions { strategy: DetachStrategy::Copy, include_snapshots: true, ..DetachOptions::default() };
    let report = db.detach_branch("main", &dest, &options).expect("detach");
    assert!(report.verified);
    let before = std::fs::read(&dest).unwrap();
    assert_eq!(db.export_branch("main", &dest, &options).unwrap_err().sqlstate.code(), "58P02");
    assert_eq!(std::fs::read(&dest).unwrap(), before, "refused export must preserve destination bytes");
    let detached = Database::open(&dest, OpenOptions::default()).unwrap();
    detached.create_branch_from_snapshot("old", "main", "before").unwrap();
    assert_eq!(ids(&mut session(&detached, "old")), expected_ids(&[1, 2]));
    assert_eq!(ids(&mut session(&detached, "main")), expected_ids(&[1, 2, 3]));
}

#[test]
fn sql_branch_handle_follows_rename_and_rejects_different_database() {
    let dir = tempfile::tempdir().unwrap();
    let source = dir.path().join("source.pga");
    let handle_path = dir.path().join("feature.pgabranch");
    let db = Database::create(&source, OpenOptions::default()).unwrap();
    seed(&mut session(&db, "main"));
    db.create_branch("feature", "main").unwrap();
    let handle = db.engine().branches().write_handle(&BranchRef::name("feature"), &handle_path, &HandleOptions::default()).unwrap();
    db.rename_branch("feature", "renamed").unwrap();
    let snapshot = db.engine().container().snapshot();
    let record = handle.check(&handle_path, &source, db.engine().container().db_uuid(), &snapshot.directory()).unwrap();
    assert_eq!(record.name, "renamed");
    assert_eq!(ids(&mut session(&db, &record.name)), expected_ids(&[1, 2]));
    let other_path = dir.path().join("other.pga");
    let other = Database::create(&other_path, OpenOptions::default()).unwrap();
    let other_snapshot = other.engine().container().snapshot();
    assert!(handle.check(&handle_path, &other_path, other.engine().container().db_uuid(), &other_snapshot.directory()).is_err());
}
