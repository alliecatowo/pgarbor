#[path="process_support/mod.rs"] mod support;
use support::*;
use serde_json::json;

#[test]
fn branch_lifecycle_is_durable_and_refuses_unconfirmed_delete() {
    let c=Cli::new(); c.init();
    json_ok(c.run(&["--json","branch","create","feature","--db",path(&c.db)]));
    ok(c.run(&["branch","rename","feature","renamed","--db",path(&c.db)]));
    let list=json_ok(c.run(&["--json","branch","list","--db",path(&c.db)]));
    assert!(list.to_string().contains("renamed"));
    code(&c.run(&["branch","delete","renamed","--db",path(&c.db)]),7);
    ok(c.run(&["branch","delete","renamed","--yes","--db",path(&c.db)]));
    ok(c.run(&["branch","restore","renamed","--db",path(&c.db)]));
    assert_eq!(c.rows("renamed","SELECT 8"),json!([[8]]));
}

#[test]
fn fork_inherits_sql_then_writes_diverge_from_main() {
    let c=Cli::new(); c.init();
    json_ok(c.sql("main","CREATE TABLE branch_data(id int PRIMARY KEY, value text); INSERT INTO branch_data VALUES (1, 'parent')"));
    ok(c.run(&["branch","create","feature","--from","main","--db",path(&c.db)]));
    assert_eq!(c.rows("feature","SELECT id, value FROM branch_data"),json!([[1,"parent"]]));
    json_ok(c.sql("feature","INSERT INTO branch_data VALUES (2, 'child')"));
    assert_eq!(c.rows("main","SELECT id, value FROM branch_data ORDER BY id"),json!([[1,"parent"]]));
    assert_eq!(c.rows("feature","SELECT id, value FROM branch_data ORDER BY id"),json!([[1,"parent"],[2,"child"]]));
}

#[test]
fn handle_tracks_rename_and_rejects_foreign_database_uuid() {
    let c=Cli::new(); c.init();
    ok(c.run(&["branch","create","feature","--db",path(&c.db)]));
    let handle=c.dir.path().join("feature.pgabranch");
    ok(c.run(&["branch","link","feature",path(&handle),"--db",path(&c.db)]));
    ok(c.run(&["branch","rename","feature","renamed","--db",path(&c.db)]));
    let report=json_ok(c.run(&["--json","exec","--handle",path(&handle),"-c","SELECT 7"]));
    assert_eq!(report["branch"],"renamed");
    assert_eq!(report["statements"][0]["rows"],json!([[7]]));
    let foreign=c.dir.path().join("foreign.pga");
    ok(c.run(&["init",path(&foreign)]));
    let error=c.run(&["exec","--handle",path(&handle),"--db",path(&foreign),"-c","SELECT 1"]);
    assert!(!error.status.success());
    assert!(String::from_utf8_lossy(&error.stderr).contains("database"));
}

#[test]
fn sql_snapshot_reset_restores_prior_rows() {
    let c=Cli::new(); c.init();
    json_ok(c.sql("main","CREATE TABLE saved(id int); INSERT INTO saved VALUES (1)"));
    ok(c.run(&["branch","snapshot","create","main","before","--db",path(&c.db)]));
    json_ok(c.sql("main","INSERT INTO saved VALUES (2)"));
    ok(c.run(&["branch","reset","main","--to-snapshot","before","--yes","--db",path(&c.db)]));
    assert_eq!(c.rows("main","SELECT id FROM saved ORDER BY id"),json!([[1]]));
}

#[test]
fn branch_export_reopens_as_standalone_sql_database() {
    let c=Cli::new(); c.init();
    json_ok(c.sql("main","CREATE TABLE exported(id int); INSERT INTO exported VALUES (99)"));
    let dest=c.dir.path().join("standalone.pga");
    ok(c.run(&["branch","export","main","--out",path(&dest),"--db",path(&c.db)]));
    let report=json_ok(c.run(&["--json","exec",path(&dest),"-c","SELECT id FROM exported"]));
    assert_eq!(report["statements"][0]["rows"],json!([[99]]));
    code(&c.run(&["branch","export","main","--out",path(&dest),"--db",path(&c.db)]),6);
}
