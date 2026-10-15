#[path="process_support/mod.rs"] mod support;
use support::*;
use serde_json::json;

#[test]
fn init_json_reopen_and_select_one_are_real_processes() {
    let c = Cli::new();
    let report = c.init();
    assert_eq!(report["created"], true);
    assert_eq!(report["branch"], "main");
    assert_eq!(report["page_size"], 8192);
    assert!(c.db.is_file());
    let reopened = json_ok(c.run(&["--json","init",path(&c.db),"--if-not-exists"]));
    assert_eq!(reopened["created"], false);
    assert_eq!(reopened["db_uuid"], report["db_uuid"]);
    assert_eq!(c.rows("main","SELECT 1 AS answer"), json!([[1]]));
}

#[test]
fn committed_sql_survives_separate_process_reopen() {
    let c = Cli::new(); c.init();
    json_ok(c.sql("main","CREATE TABLE durable(id int PRIMARY KEY, value text); INSERT INTO durable VALUES (1, 'persisted')"));
    assert_eq!(c.rows("main","SELECT id, value FROM durable ORDER BY id"), json!([[1,"persisted"]]));
    ok(c.run(&["verify",path(&c.db)]));
}

#[test]
fn create_refuses_existing_and_errors_are_single_json_documents() {
    let c = Cli::new(); c.init();
    let before = std::fs::read(&c.db).unwrap();
    let duplicate = c.run(&["--json","init",path(&c.db)]);
    code(&duplicate,6);
    assert!(duplicate.stdout.is_empty());
    let error: serde_json::Value = serde_json::from_slice(&duplicate.stderr).unwrap();
    assert_eq!(error["error"]["exit_code"],6);
    assert_eq!(std::fs::read(&c.db).unwrap(),before);
    let missing = c.run(&["--json","status","missing.pga"]); code(&missing,5);
    let bad = c.run(&["init","bad.pga","--page-size","1000"]); code(&bad,64);
    assert!(!c.dir.path().join("bad.pga").exists());
}

#[test]
fn os_file_lock_conflict_is_exit_four() {
    let c = Cli::new(); c.init();
    let _held = pgarbor_storage::Container::open(&c.db,&Default::default()).unwrap();
    let output = c.run(&["--json","status",path(&c.db)]); code(&output,4);
    let error: serde_json::Value = serde_json::from_slice(&output.stderr).unwrap();
    assert_eq!(error["error"]["exit_code"],4);
}

#[test]
fn sql_error_is_nonzero_and_success_human_output_uses_stdout() {
    let c=Cli::new(); c.init();
    sqlstate(c.sql("main","SELECT FROM"),"42601");
    let output=ok(c.run(&["exec",path(&c.db),"-c","SELECT 42 AS answer"]));
    assert!(String::from_utf8_lossy(&output.stdout).contains("42"));
    assert!(String::from_utf8_lossy(&output.stdout).contains("answer"));
    assert!(!String::from_utf8_lossy(&output.stderr).contains("ERROR"));
}
