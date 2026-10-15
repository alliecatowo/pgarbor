mod support;
use pgarbor::{Database, OpenOptions};
use support::*;

#[test]
fn committed_schema_rows_and_unique_index_survive_reopen() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("db.pga");
    {
        let db = Database::create(&path, OpenOptions::default()).expect("durable create");
        let mut s = session(&db, "main");
        seed(&mut s);
        execute(&mut s, "BEGIN; INSERT INTO t VALUES (3); ROLLBACK");
        drop(s);
        db.close().expect("close");
    }
    let db = Database::open(&path, OpenOptions::default()).expect("reopen");
    let mut s = session(&db, "main");
    assert_eq!(ids(&mut s), expected_ids(&[1, 2]));
    assert_eq!(s.simple_query("INSERT INTO t VALUES (1)").unwrap_err().sqlstate.code(), "23505");
    execute(&mut s, "INSERT INTO t VALUES (4)");
    assert_eq!(ids(&mut s), expected_ids(&[1, 2, 4]));
}

#[test]
fn sole_pga_copy_is_portable_to_empty_directory() {
    let source = tempfile::tempdir().unwrap();
    let target = tempfile::tempdir().unwrap();
    let path = source.path().join("source.pga");
    {
        let db = Database::create(&path, OpenOptions::default()).unwrap();
        seed(&mut session(&db, "main"));
        db.close().unwrap();
    }
    let copied = target.path().join("only.pga");
    std::fs::copy(path, &copied).unwrap();
    assert_eq!(std::fs::read_dir(target.path()).unwrap().count(), 1);
    let db = Database::open(copied, OpenOptions::default()).unwrap();
    assert_eq!(ids(&mut session(&db, "main")), expected_ids(&[1, 2]));
}

#[test]
fn separate_process_reads_committed_sql_file() {
    const CHILD: &str = "PGARBOR_SQL_REOPEN_PATH";
    if let Some(path) = std::env::var_os(CHILD) {
        let db = Database::open(path, OpenOptions::default()).expect("child reopen");
        assert_eq!(ids(&mut session(&db, "main")), expected_ids(&[1, 2]));
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("process.pga");
    {
        let db = Database::create(&path, OpenOptions::default()).unwrap();
        seed(&mut session(&db, "main"));
        db.close().unwrap();
    }
    let status = std::process::Command::new(std::env::current_exe().unwrap())
        .args(["--exact", "separate_process_reads_committed_sql_file", "--nocapture"])
        .env(CHILD, &path).status().expect("spawn reopen child");
    assert!(status.success(), "reopen process failed: {status}");
}

#[test]
fn exclusive_file_lock_is_released_after_handles_drop() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("locked.pga");
    let db = Database::create(&path, OpenOptions::default()).unwrap();
    let error = Database::open(&path, OpenOptions::default()).unwrap_err();
    assert_eq!(error.sqlstate.code(), "55006");
    db.close().unwrap();
    drop(db);
    let reopened = Database::open(&path, OpenOptions::default()).expect("lock released");
    assert_eq!(rows(&mut session(&reopened, "main"), "SELECT 1"), expected_ids(&[1]));
}

#[test]
fn sequence_state_advances_across_clean_reopen() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("sequence.pga");
    {
        let db = Database::create(&path, OpenOptions::default()).unwrap();
        let mut s = session(&db, "main");
        execute(&mut s, "CREATE SEQUENCE durable_seq");
        assert_eq!(rows(&mut s, "SELECT nextval('durable_seq')"), vec![vec![pgarbor_types::Datum::Int8(1)]]);
        drop(s);
        db.close().unwrap();
    }
    let db = Database::open(&path, OpenOptions::default()).unwrap();
    assert_eq!(rows(&mut session(&db, "main"), "SELECT nextval('durable_seq')"), vec![vec![pgarbor_types::Datum::Int8(2)]]);
}
