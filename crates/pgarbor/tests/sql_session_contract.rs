use pgarbor::{Database, SessionOptions, StatementResult};
use pgarbor_types::Datum;

#[test]
fn select_one_and_ddl_dml_metadata() {
    let db = Database::in_memory().expect("create");
    let mut s = db.session("main", SessionOptions::default()).expect("session");
    let out = s.simple_query("SELECT 1 AS answer").expect("select");
    match &out[0].result {
        StatementResult::Rows { columns, rows, tag } => {
            assert_eq!(columns[0].name, "answer");
            assert_eq!(columns[0].type_oid, 23);
            assert_eq!(rows, &vec![vec![Datum::Int4(1)]]);
            assert_eq!(tag, "SELECT 1");
        }
        other => panic!("expected rows: {other:?}"),
    }
    let out = s.simple_query("CREATE TABLE t(id int primary key); INSERT INTO t VALUES (2),(1); SELECT id FROM t ORDER BY id").expect("DDL/DML");
    assert_eq!(out.iter().map(|x| x.result.tag()).collect::<Vec<_>>(), ["CREATE TABLE", "INSERT 0 2", "SELECT 2"]);
    match &out[2].result {
        StatementResult::Rows { rows, .. } => assert_eq!(rows, &vec![vec![Datum::Int4(1)], vec![Datum::Int4(2)]]),
        other => panic!("expected rows: {other:?}"),
    }
    assert_eq!(s.simple_query("INSERT INTO t VALUES (1)").unwrap_err().sqlstate.code(), "23505");
}

#[test]
fn whole_string_parse_and_implicit_rollback() {
    let db = Database::in_memory().expect("create");
    let mut s = db.session("main", SessionOptions::default()).expect("session");
    assert_eq!(s.simple_query("CREATE TABLE must_not_exist(id int); SELEC 1").unwrap_err().sqlstate.code(), "42601");
    assert_eq!(s.simple_query("SELECT * FROM must_not_exist").unwrap_err().sqlstate.code(), "42P01");
    s.simple_query("CREATE TABLE t(id int)").expect("create table");
    assert_eq!(s.simple_query("INSERT INTO t VALUES (1); SELECT 1/0").unwrap_err().sqlstate.code(), "22012");
    let out = s.simple_query("SELECT id FROM t").expect("read after failure");
    match &out[0].result {
        StatementResult::Rows { rows, .. } => assert!(rows.is_empty()),
        other => panic!("expected rows: {other:?}"),
    }
}

#[test]
fn failed_explicit_transaction_and_savepoint() {
    let db = Database::in_memory().expect("create");
    let mut s = db.session("main", SessionOptions::default()).expect("session");
    s.simple_query("CREATE TABLE t(id int primary key)").expect("create");
    s.simple_query("BEGIN; INSERT INTO t VALUES (1)").expect("begin");
    assert_eq!(s.simple_query("SELECT 1/0").unwrap_err().sqlstate.code(), "22012");
    assert_eq!(s.txn_status(), pgarbor::TxnStatus::Failed);
    assert_eq!(s.simple_query("SELECT 1").unwrap_err().sqlstate.code(), "25P02");
    assert_eq!(s.simple_query("COMMIT").expect("failed commit")[0].result.tag(), "ROLLBACK");
    s.simple_query("BEGIN; SAVEPOINT a; CREATE TABLE rolled_back(id int); ROLLBACK TO a").expect("savepoint");
    assert_eq!(s.simple_query("SELECT * FROM rolled_back").unwrap_err().sqlstate.code(), "42P01");
    s.simple_query("ROLLBACK").expect("rollback");
    let out = s.simple_query("SELECT id FROM t").expect("rows");
    assert!(matches!(&out[0].result, StatementResult::Rows { rows, .. } if rows.is_empty()));
}

#[test]
fn unique_nulls_and_mixed_integer_join() {
    let db = Database::in_memory().expect("create");
    let mut s = db.session("main", SessionOptions::default()).expect("session");
    s.simple_query("CREATE TABLE t(id int unique); INSERT INTO t VALUES (NULL),(NULL),(1)").expect("NULL distinct");
    let out = s.simple_query("SELECT a.id FROM t a JOIN t b ON a.id::bigint = b.id WHERE a.id IS NOT NULL").expect("cross-type join");
    assert!(matches!(&out[0].result, StatementResult::Rows { rows, .. } if rows == &vec![vec![Datum::Int4(1)]]));
    assert_eq!(s.simple_query("SET CONSTRAINTS ALL DEFERRED").unwrap_err().sqlstate.code(), "0A000");
}
