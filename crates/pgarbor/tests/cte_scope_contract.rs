use pgarbor::{Database, SessionOptions, StatementResult};
use pgarbor_types::Datum;

fn rows(sql: &str) -> Vec<Vec<Datum>> {
    let db = Database::in_memory().expect("create");
    let mut s = db.session("main", SessionOptions::default()).expect("session");
    let out = s.simple_query(sql).unwrap_or_else(|e| panic!("{sql}: {e:?}"));
    match &out[0].result {
        StatementResult::Rows { rows, .. } => rows.clone(),
        other => panic!("expected rows: {other:?}"),
    }
}

#[test]
fn recursive_series() {
    let r = rows("WITH RECURSIVE t(n) AS (VALUES (1) UNION ALL SELECT n+1 FROM t WHERE n < 100) SELECT sum(n), count(*) FROM t");
    assert_eq!(r.len(), 1);
}

#[test]
fn chained_ctes() {
    let r = rows("WITH a AS (SELECT 1 AS x), b AS (SELECT x FROM a WHERE x > (SELECT sum(x)/10 FROM a)) SELECT * FROM b");
    assert_eq!(r.len(), 1);
}

#[test]
fn simple_cte() {
    assert_eq!(rows("WITH a AS (SELECT 1 AS x) SELECT * FROM a").len(), 1);
}

#[test]
fn nested_with_inside_cte_body_and_sublink() {
    let r = rows("WITH a AS (SELECT 1 AS x), b AS (WITH c AS (SELECT x FROM a) SELECT x FROM c UNION ALL SELECT x FROM a) SELECT (SELECT count(*) FROM b), x FROM a");
    assert_eq!(r, vec![vec![Datum::Int8(2), Datum::Int4(1)]]);
}
