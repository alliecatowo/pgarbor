#[path="process_support/mod.rs"] mod support;
use support::*;
use serde_json::json;

#[test]
fn exec_file_stops_at_first_error_and_reopen_sees_only_prior_writes() {
    let c=Cli::new(); c.init();
    json_ok(c.sql("main","CREATE TABLE script_data(id int)"));
    let script=c.dir.path().join("script.sql");
    std::fs::write(&script,"INSERT INTO script_data VALUES (1); SELECT FROM; INSERT INTO script_data VALUES (2);").unwrap();
    let report=c.run(&["--json","exec",path(&c.db),"-f",path(&script)]); code(&report,1);
    assert_eq!(c.rows("main","SELECT id FROM script_data ORDER BY id"),json!([[1]]));
}

#[test]
fn stdin_import_handles_semicolons_inside_values_and_persists() {
    let c=Cli::new(); c.init();
    let report=json_ok(c.input(&["--json","import","-","--db",path(&c.db)],"CREATE TABLE imported(value text);\nINSERT INTO imported VALUES ('a;b'), ('it''s fine');\n"));
    assert_eq!(report["statements_run"],2);
    assert_eq!(report["committed"],true);
    assert_eq!(c.rows("main","SELECT value FROM imported ORDER BY value"),json!([["a;b"],["it's fine"]]));
}

#[test]
fn single_transaction_import_rolls_back_schema_and_rows_on_error() {
    let c=Cli::new(); c.init();
    let output=c.input(&["--json","import","-","--single-transaction","--db",path(&c.db)],"CREATE TABLE rolled_back(id int);\nINSERT INTO rolled_back VALUES (1);\nSELECT FROM;\n");
    code(&output,1);
    sqlstate(c.sql("main","SELECT id FROM rolled_back"),"42P01");
}

#[test]
fn dump_import_roundtrip_preserves_schema_data_and_compaction() {
    let c=Cli::new(); c.init();
    json_ok(c.sql("main","CREATE TABLE roundtrip(id int PRIMARY KEY, value text); INSERT INTO roundtrip VALUES (1, 'a;b'), (2, 'quoted '' value')"));
    let dump=c.dir.path().join("dump.sql");
    ok(c.run(&["dump",path(&c.db),"--output",path(&dump)]));
    let text=std::fs::read_to_string(&dump).unwrap();
    assert!(text.contains("CREATE TABLE"));
    assert!(text.contains("INSERT INTO"));
    let dest=c.dir.path().join("imported.pga");
    ok(c.run(&["import",path(&dump),"--db",path(&dest),"--create"]));
    ok(c.run(&["compact",path(&dest)]));
    ok(c.run(&["verify",path(&dest)]));
    let report=json_ok(c.run(&["--json","exec",path(&dest),"-c","SELECT id, value FROM roundtrip ORDER BY id"]));
    assert_eq!(report["statements"][0]["rows"],json!([[1,"a;b"],[2,"quoted ' value"]]));
}

#[test]
fn copy_import_stream_preserves_rows() {
    let c=Cli::new(); c.init();
    let output=c.input(&["import","-","--db",path(&c.db)],"CREATE TABLE copied(id int, value text);\nCOPY copied FROM stdin;\n1\talpha\n2\tbeta\n\\.\n");
    ok(output);
    assert_eq!(c.rows("main","SELECT id, value FROM copied ORDER BY id"),json!([[1,"alpha"],[2,"beta"]]));
}
