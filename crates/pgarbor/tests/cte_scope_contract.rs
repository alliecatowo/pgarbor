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

#[test]
fn infinite_recursion_is_stopped_lazily_by_limit() {
    let r = rows("WITH RECURSIVE t(n) AS (VALUES (1) UNION ALL SELECT n+1 FROM t) SELECT n FROM t LIMIT 10");
    assert_eq!(r.len(), 10);
    assert_eq!(r[9], vec![Datum::Int4(10)]);
}

#[test]
fn manual_cycle_detection_terminates() {
    let db = Database::in_memory().expect("create");
    let mut s = db.session("main", SessionOptions::default()).expect("session");
    s.simple_query("CREATE TABLE graph (id int, link int, data text); INSERT INTO graph VALUES (1, 2, 'x'), (2, 3, 'y'), (3, 1, 'z'), (3, 4, 'w')").expect("setup");
    let out = s.simple_query("WITH RECURSIVE sg(id, link, depth, is_cycle, path) AS (SELECT g.id, g.link, 0, false, ARRAY[g.id] FROM graph g UNION ALL SELECT g.id, g.link, sg.depth + 1, g.id = ANY(path), path || g.id FROM graph g, sg WHERE g.id = sg.link AND NOT is_cycle) SELECT count(*) FROM sg").expect("query");
    match &out[0].result {
        StatementResult::Rows { rows, .. } => assert_eq!(*rows, vec![vec![Datum::Int8(16)]]),
        other => panic!("{other:?}"),
    }
}

#[test]
fn recursion_only_evaluates_demanded_seed_rows() {
    let db = Database::in_memory().expect("db");
    let mut s = db.session("main", SessionOptions::default()).expect("session");
    s.simple_query("CREATE SEQUENCE seq").expect("sequence");
    s.simple_query("WITH RECURSIVE t(n) AS (SELECT nextval('seq') FROM generate_series(1,10) UNION ALL SELECT n+1 FROM t) SELECT n FROM t LIMIT 1").expect("one row");
    let out = s.simple_query("SELECT currval('seq')").expect("currval");
    match &out[0].result {
        StatementResult::Rows { rows, .. } => assert_eq!(*rows, vec![vec![Datum::Int8(1)]]),
        other => panic!("{other:?}"),
    }
}
