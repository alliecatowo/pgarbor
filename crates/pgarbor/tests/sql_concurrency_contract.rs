use pgarbor::{Database, Datum, Session, SessionOptions, StatementResult};

fn value(s: &mut Session) -> i32 {
    let output=s.simple_query("SELECT value FROM t WHERE id=1").expect("read");
    match &output[0].result {
        StatementResult::Rows {rows,..} if rows.len()==1 => match rows[0][0] {Datum::Int4(v)=>v,_=>panic!("wrong type")},
        other=>panic!("wrong rows: {other:?}"),
    }
}
#[test]
fn read_committed_refreshes_between_statements() {
    let db=Database::in_memory().expect("database");
    let mut a=db.session("main",SessionOptions::default()).expect("session a");
    let mut b=db.session("main",SessionOptions::default()).expect("session b");
    a.simple_query("CREATE TABLE t(id int primary key,value int); INSERT INTO t VALUES(1,10)").expect("setup");
    a.simple_query("BEGIN ISOLATION LEVEL READ COMMITTED").expect("begin");
    assert_eq!(value(&mut a),10);
    b.simple_query("UPDATE t SET value=20 WHERE id=1").expect("concurrent commit");
    assert_eq!(value(&mut a),20);
    a.simple_query("ROLLBACK").expect("rollback");
}
#[test]
fn repeatable_read_retains_first_statement_snapshot() {
    let db=Database::in_memory().expect("database");
    let mut a=db.session("main",SessionOptions::default()).expect("session a");
    let mut b=db.session("main",SessionOptions::default()).expect("session b");
    a.simple_query("CREATE TABLE t(id int primary key,value int); INSERT INTO t VALUES(1,10)").expect("setup");
    a.simple_query("BEGIN ISOLATION LEVEL REPEATABLE READ").expect("begin");
    assert_eq!(value(&mut a),10);
    b.simple_query("UPDATE t SET value=20 WHERE id=1").expect("concurrent commit");
    assert_eq!(value(&mut a),10);
    a.simple_query("ROLLBACK").expect("rollback");
    assert_eq!(value(&mut a),20);
}
#[test]
fn failed_multirow_unique_statement_leaves_no_partial_rows() {
    let db=Database::in_memory().expect("database");
    let mut s=db.session("main",SessionOptions::default()).expect("session");
    s.simple_query("CREATE TABLE t(id int primary key,value int)").expect("setup");
    assert_eq!(s.simple_query("INSERT INTO t VALUES(1,10),(1,20)").unwrap_err().sqlstate.code(),"23505");
    let output=s.simple_query("SELECT id FROM t").expect("recover");
    assert!(matches!(&output[0].result,StatementResult::Rows {rows,..} if rows.is_empty()));
}
