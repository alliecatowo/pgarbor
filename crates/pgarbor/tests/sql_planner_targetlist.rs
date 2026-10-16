//! Regression tests for planner target-list resolution (setrefs): whole-row references above a
//! scan, simplified aggregate calls, functionally dependent GROUP BY columns, `tableoid`, and
//! ALTER COLUMN TYPE ... USING.
use pgarbor::{Database, Session, SessionOptions, StatementResult};
use pgarbor_types::Datum;

fn session() -> (Database, Session) {
    let db = Database::in_memory().expect("create");
    let s = db.session("main", SessionOptions::default()).expect("session");
    (db, s)
}

fn rows(s: &mut Session, sql: &str) -> Vec<Vec<Datum>> {
    let out = s.simple_query(sql).unwrap_or_else(|e| panic!("{sql}: {} {}", e.sqlstate.code(), e.message));
    match &out.last().expect("a statement").result {
        StatementResult::Rows { rows, .. } => rows.clone(),
        other => panic!("expected rows: {other:?}"),
    }
}

#[test]
fn whole_row_above_scan_and_join() {
    let (_db, mut s) = session();
    s.simple_query("CREATE TABLE t(a int, b text); INSERT INTO t VALUES (1,'x'),(2,'y')").expect("setup");
    assert_eq!(rows(&mut s, "SELECT t FROM t ORDER BY t.a").len(), 2);
    assert_eq!(rows(&mut s, "SELECT u FROM t JOIN t u USING (a) ORDER BY a").len(), 2);
    assert_eq!(rows(&mut s, "SELECT t, count(*) FROM t GROUP BY t").len(), 2);
    assert_eq!(rows(&mut s, "SELECT length(t::text) FROM t ORDER BY t")[0], vec![Datum::Int4(5)]);
}

#[test]
fn aggregates_with_constant_arguments_match_their_simplified_form() {
    let (_db, mut s) = session();
    assert_eq!(rows(&mut s, "SELECT regr_count(1.0::float8, 2.0::float8)"), vec![vec![Datum::Int8(1)]]);
    assert_eq!(rows(&mut s, "SELECT var_pop(1.0::float8)"), vec![vec![Datum::Float8(0.0)]]);
    assert_eq!(
        rows(&mut s, "SELECT percentile_cont(0.5) WITHIN GROUP (ORDER BY x) FROM (VALUES (1.0::float8), (3.0::float8)) v(x)"),
        vec![vec![Datum::Float8(2.0)]]
    );
}

#[test]
fn group_by_primary_key_exposes_dependent_columns() {
    let (_db, mut s) = session();
    s.simple_query("CREATE TABLE p(id int PRIMARY KEY, b text); INSERT INTO p VALUES (1,'x'),(2,'y')").expect("setup");
    let got = rows(&mut s, "SELECT id, b, count(*) FROM p GROUP BY id ORDER BY id");
    assert_eq!(got.len(), 2);
    assert_eq!(got[1][1], Datum::Text("y".into()));
    assert_eq!(rows(&mut s, "SELECT id + 1, b FROM p GROUP BY id HAVING b <> 'x'").len(), 1);
}

#[test]
fn tableoid_in_joins_and_views() {
    let (_db, mut s) = session();
    s.simple_query("CREATE TABLE p(id int PRIMARY KEY); INSERT INTO p VALUES (1); CREATE VIEW v AS SELECT tableoid::regclass AS t, id FROM p").expect("setup");
    assert_eq!(rows(&mut s, "SELECT a.id FROM p a JOIN p b ON a.tableoid = b.tableoid").len(), 1);
    assert_eq!(rows(&mut s, "SELECT t::text FROM v"), vec![vec![Datum::Text("p".into())]]);
}

#[test]
fn alter_column_type_using_rewrites_rows() {
    let (_db, mut s) = session();
    s.simple_query("CREATE TABLE t(a int, b text); INSERT INTO t VALUES (1,'1'),(2,'2')").expect("setup");
    s.simple_query("ALTER TABLE t ALTER b TYPE int USING b::int + a").expect("alter using");
    assert_eq!(rows(&mut s, "SELECT b FROM t ORDER BY a"), vec![vec![Datum::Int4(2)], vec![Datum::Int4(4)]]);
    s.simple_query("ALTER TABLE t ALTER a TYPE bigint").expect("alter implicit cast");
    assert_eq!(rows(&mut s, "SELECT a FROM t ORDER BY a"), vec![vec![Datum::Int8(1)], vec![Datum::Int8(2)]]);
}

#[test]
fn outer_join_null_extends_tableoid_and_whole_row() {
    let (_db, mut s) = session();
    s.simple_query("CREATE TABLE a(x int); CREATE TABLE b(x int); INSERT INTO a VALUES (1); INSERT INTO b VALUES (NULL)").expect("setup");
    assert_eq!(rows(&mut s, "SELECT b.tableoid, b::text FROM a LEFT JOIN b ON false"), vec![vec![Datum::Null, Datum::Null]]);
    assert_eq!(rows(&mut s, "SELECT b.tableoid IS NOT NULL, b::text FROM a LEFT JOIN b ON true"), vec![vec![Datum::Bool(true), Datum::Text("()".into())]]);
    assert_eq!(rows(&mut s, "SELECT a.tableoid, a::text FROM a RIGHT JOIN b ON false"), vec![vec![Datum::Null, Datum::Null]]);
}

#[test]
fn grouping_sets_keep_functionally_dependent_values() {
    let (_db, mut s) = session();
    s.simple_query("CREATE TABLE gs(a int PRIMARY KEY, b int, c int, d int); INSERT INTO gs VALUES (1,1,1,10),(2,2,2,20)").expect("setup");
    assert_eq!(rows(&mut s, "SELECT a, d, grouping(a,b,c) FROM gs GROUP BY GROUPING SETS ((a,b),(a,c)) ORDER BY a, grouping(a,b,c)"), vec![
        vec![Datum::Int4(1),Datum::Int4(10),Datum::Int4(1)], vec![Datum::Int4(1),Datum::Int4(10),Datum::Int4(2)],
        vec![Datum::Int4(2),Datum::Int4(20),Datum::Int4(1)], vec![Datum::Int4(2),Datum::Int4(20),Datum::Int4(2)],
    ]);
}

#[test]
fn whole_row_preserves_dropped_attribute_positions() {
    let (_db, mut s) = session();
    s.simple_query("CREATE TABLE t(a int,b text,c int); INSERT INTO t VALUES (1,'x',2); ALTER TABLE t DROP b").expect("setup");
    assert_eq!(rows(&mut s, "SELECT t::text FROM t"), vec![vec![Datum::Text("(1,2)".into())]]);
}
