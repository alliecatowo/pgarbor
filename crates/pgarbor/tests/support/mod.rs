#![allow(dead_code)]
use pgarbor::{Database, Session, SessionOptions, StatementResult};
use pgarbor_types::Datum;

pub fn session(db: &Database, branch: &str) -> Session {
    db.session(branch, SessionOptions::default()).expect("connect SQL session")
}

pub fn execute(s: &mut Session, sql: &str) {
    s.simple_query(sql).unwrap_or_else(|e| panic!("SQL {sql:?} failed: {e:?}"));
}

pub fn rows(s: &mut Session, sql: &str) -> Vec<Vec<Datum>> {
    let output = s.simple_query(sql).unwrap_or_else(|e| panic!("SQL {sql:?} failed: {e:?}"));
    assert_eq!(output.len(), 1, "one result required for {sql}");
    match output.into_iter().next().expect("one output").result {
        StatementResult::Rows { rows, .. } => rows,
        other => panic!("expected rows for {sql}: {other:?}"),
    }
}

pub fn ids(s: &mut Session) -> Vec<Vec<Datum>> {
    rows(s, "SELECT id FROM t ORDER BY id")
}

pub fn expected_ids(values: &[i32]) -> Vec<Vec<Datum>> {
    values.iter().map(|&n| vec![Datum::Int4(n)]).collect()
}

pub fn seed(s: &mut Session) {
    execute(s, "CREATE TABLE t(id int primary key)");
    execute(s, "INSERT INTO t VALUES (1),(2)");
}
