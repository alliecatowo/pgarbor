//! Regression coverage for semantic fixes found by embedded differential review 02.
use pgarbor::{Database, SessionOptions, StatementResult};
use pgarbor_types::Datum;

#[test]
fn view_dependency_survives_expansion() {
    let db = Database::in_memory().expect("db");
    let mut s = db.session("main", SessionOptions::default()).expect("session");
    s.simple_query("CREATE TABLE t(x int); CREATE VIEW v1 AS SELECT x FROM t; CREATE VIEW v2 AS SELECT x FROM v1").expect("setup");
    let e = s.simple_query("DROP VIEW v1").expect_err("dependent view");
    assert_eq!(e.sqlstate.code(), "2BP01");
    s.simple_query("DROP VIEW v1 CASCADE").expect("cascade");
    assert_eq!(s.simple_query("SELECT * FROM v2").expect_err("gone").sqlstate.code(), "42P01");
}

#[test]
fn system_columns_cannot_be_index_keys_expressions_or_predicates() {
    let db = Database::in_memory().expect("db");
    let mut s = db.session("main", SessionOptions::default()).expect("session");
    s.simple_query("CREATE TABLE t(x int)").expect("setup");
    for sql in ["CREATE INDEX ON t(tableoid)", "CREATE INDEX ON t((tableoid::oid))", "CREATE INDEX ON t(x) INCLUDE (xmin)", "CREATE INDEX ON t(x) WHERE tableoid > 0"] {
        let e = s.simple_query(sql).expect_err(sql);
        assert_eq!(e.sqlstate.code(), "0A000");
        assert_eq!(e.message, "index creation on system columns is not supported");
    }
}

#[test]
fn alter_add_volatile_default_uses_column_type() {
    let db = Database::in_memory().expect("db");
    let mut s = db.session("main", SessionOptions::default()).expect("session");
    s.simple_query("CREATE SEQUENCE seq; CREATE TABLE t(x int); INSERT INTO t VALUES (1),(2); ALTER TABLE t ADD COLUMN n int DEFAULT nextval('seq')").expect("rewrite");
    let out = s.simple_query("SELECT n FROM t ORDER BY x").expect("read");
    match &out[0].result {
        StatementResult::Rows { rows, .. } => assert_eq!(*rows, vec![vec![Datum::Int4(1)], vec![Datum::Int4(2)]]),
        other => panic!("{other:?}"),
    }
}

#[test]
fn oid_binary_relabels_change_the_datum_variant() {
    let db = Database::in_memory().expect("db");
    let mut s = db.session("main", SessionOptions::default()).expect("session");
    let out = s.simple_query("SELECT 42::oid, 42::oid::int, (-1)::oid::int").expect("casts");
    match &out[0].result {
        StatementResult::Rows { rows, .. } => assert_eq!(*rows, vec![vec![Datum::Oid(42), Datum::Int4(42), Datum::Int4(-1)]]),
        other => panic!("{other:?}"),
    }
}

#[test]
fn pg_notify_null_channel_reports_parameter_error() {
    let db = Database::in_memory().expect("db");
    let mut s = db.session("main", SessionOptions::default()).expect("session");
    let e = s.simple_query("SELECT pg_notify(NULL, NULL)").expect_err("empty channel");
    assert_eq!(e.sqlstate.code(), "22023");
    assert_eq!(e.message, "channel name cannot be empty");
}

#[test]
fn mixed_integer_distinct_and_scalar_limit() {
    let db = Database::in_memory().expect("db");
    let mut s = db.session("main", SessionOptions::default()).expect("session");
    let out = s.simple_query("SELECT 5::bigint IS DISTINCT FROM 5::integer, NULL::bigint IS DISTINCT FROM 5, NULL::bigint IS NOT DISTINCT FROM NULL::integer").expect("distinct");
    match &out[0].result {
        StatementResult::Rows { rows, .. } => assert_eq!(*rows, vec![vec![Datum::Bool(false), Datum::Bool(true), Datum::Bool(true)]]),
        other => panic!("{other:?}"),
    }
    let out = s.simple_query("SELECT x FROM (VALUES (1),(2),(3)) v(x) ORDER BY x LIMIT (SELECT 1) OFFSET (SELECT 1)").expect("limit");
    match &out[0].result {
        StatementResult::Rows { rows, .. } => assert_eq!(*rows, vec![vec![Datum::Int4(2)]]),
        other => panic!("{other:?}"),
    }
}

#[test]
fn lateral_subquery_preserves_local_and_outer_columns() {
    let db = Database::in_memory().expect("db");
    let mut s = db.session("main", SessionOptions::default()).expect("session");
    s.simple_query("CREATE TABLE a(x int); CREATE TABLE b(y int); INSERT INTO a VALUES (1),(2); INSERT INTO b VALUES (1),(3)").expect("setup");
    let e = s.simple_query("SELECT * FROM a, (SELECT * FROM b WHERE y = a.x) q").expect_err("LATERAL required");
    assert_eq!(e.sqlstate.code(), "42P01");
    let out = s.simple_query("SELECT a.x, q.y FROM a LEFT JOIN LATERAL (SELECT * FROM b WHERE y = a.x) q ON true ORDER BY a.x").expect("lateral");
    match &out[0].result {
        StatementResult::Rows { rows, .. } => assert_eq!(*rows, vec![vec![Datum::Int4(1), Datum::Int4(1)],vec![Datum::Int4(2), Datum::Null]]),
        other => panic!("{other:?}"),
    }
}

#[test]
fn domain_constraint_validation_rejects_array_containers() {
    let db = Database::in_memory().expect("db");
    let mut s = db.session("main", SessionOptions::default()).expect("session");
    s.simple_query("CREATE DOMAIN pos AS int; CREATE DOMAIN pos2 AS pos; CREATE TABLE t(a pos2[])").expect("setup");
    for sql in ["ALTER DOMAIN pos SET NOT NULL", "ALTER DOMAIN pos ADD CHECK (VALUE > 0)"] {
        let e = s.simple_query(sql).expect_err("container");
        assert_eq!(e.sqlstate.code(), "0A000");
        assert_eq!(e.message, "cannot alter type \"pos2\" because column \"t.a\" uses it");
    }
    s.simple_query("ALTER DOMAIN pos SET DEFAULT 1; DROP TABLE t; ALTER DOMAIN pos SET NOT NULL").expect("after drop");
}

#[test]
fn utility_and_materialized_view_completion_tags() {
    let db = Database::in_memory().expect("db");
    let mut s = db.session("main", SessionOptions::default()).expect("session");
    for (sql, tag) in [
        ("START TRANSACTION", "START TRANSACTION"), ("ROLLBACK", "ROLLBACK"),
        ("RESET ROLE", "RESET"), ("RESET SESSION AUTHORIZATION", "RESET"),
        ("CREATE MATERIALIZED VIEW m AS SELECT 1 AS n", "SELECT 1"),
        ("CREATE MATERIALIZED VIEW empty AS SELECT 1 AS n WHERE false", "SELECT 0"),
        ("CREATE MATERIALIZED VIEW IF NOT EXISTS m AS SELECT 2 AS n", "CREATE MATERIALIZED VIEW"),
    ] {
        assert_eq!(s.simple_query(sql).expect(sql)[0].result.tag(), tag, "{sql}");
    }
}

#[test]
fn correlated_catalog_default_expression() {
    let db = Database::in_memory().expect("db");
    let mut s = db.session("main", SessionOptions::default()).expect("session");
    s.simple_query("CREATE TABLE nd (a int, b text DEFAULT NULL, c text DEFAULT '')").expect("setup");
    let out = s.simple_query("SELECT a.attname, (SELECT pg_catalog.pg_get_expr(d.adbin, d.adrelid, true) FROM pg_catalog.pg_attrdef d WHERE d.adrelid = a.attrelid AND d.adnum = a.attnum AND a.atthasdef) IS NULL FROM pg_catalog.pg_attribute a WHERE a.attrelid = 'nd'::regclass AND a.attnum > 0 AND NOT a.attisdropped ORDER BY a.attnum").expect("describe");
    match &out[0].result {
        StatementResult::Rows { rows, .. } => assert_eq!(rows.len(), 3),
        other => panic!("{other:?}"),
    }
}

#[test]
fn nested_set_expressions_advance_together_and_pad_with_nulls() {
    let db = Database::in_memory().expect("db");
    let mut s = db.session("main", SessionOptions::default()).expect("session");
    let out = s.simple_query("SELECT generate_series(1,3)+10, generate_series(1,2)*2").expect("sets");
    match &out[0].result {
        StatementResult::Rows { rows, .. } => assert_eq!(*rows, vec![
            vec![Datum::Int4(11),Datum::Int4(2)],vec![Datum::Int4(12),Datum::Int4(4)],vec![Datum::Int4(13),Datum::Null],
        ]),
        other => panic!("{other:?}"),
    }
    let out = s.simple_query("SELECT generate_series(1,3)+generate_series(1,2)").expect("two sets in one expression");
    match &out[0].result {
        StatementResult::Rows { rows, .. } => assert_eq!(*rows, vec![vec![Datum::Int4(2)],vec![Datum::Int4(4)],vec![Datum::Null]]),
        other => panic!("{other:?}"),
    }
}

#[test]
fn unsupported_builtins_and_outer_aggregates_preserve_leased_errors() {
    let db = Database::in_memory().expect("db");
    let mut s = db.session("main", SessionOptions::default()).expect("session");
    for sql in ["GRANT EXECUTE ON FUNCTION abs(int) TO PUBLIC", "SELECT (SELECT count(t.x)) FROM (VALUES (1)) t(x)"] {
        let e = s.simple_query(sql).expect_err(sql);
        assert_eq!(e.sqlstate.code(), "0A000", "{sql}");
        assert!(e.detail.as_deref().unwrap_or("").starts_with("pgarbor-unimplemented:"));
    }
}

#[test]
fn single_field_composite_functions_and_old_record_output() {
    let db = Database::in_memory().expect("db");
    let mut s = db.session("main", SessionOptions::default()).expect("session");
    s.simple_query("CREATE TYPE char2 AS (a char(2))").expect("type");
    let out = s.simple_query("SELECT * FROM jsonb_populate_record(NULL::char2, '{\"a\":\"aa\"}') q").expect("populate");
    match &out[0].result {
        StatementResult::Rows { rows, .. } => assert_eq!(*rows, vec![vec![Datum::Text("aa".into())]]),
        other => panic!("{other:?}"),
    }
    s.simple_query("CREATE TABLE fullname(first text,last text); CREATE TABLE people(fn fullname); INSERT INTO people VALUES ('(Joe,Blow)')").expect("setup composite dependency");
    assert_eq!(s.simple_query("ALTER TABLE fullname ADD suffix text DEFAULT ''").expect_err("non-NULL default changes dependent tuples").sqlstate.code(), "0A000");
    s.simple_query("ALTER TABLE fullname ADD suffix text DEFAULT NULL").expect("metadata change");
    let out = s.simple_query("SELECT fn::text FROM people").expect("old record");
    match &out[0].result {
        StatementResult::Rows { rows, .. } => assert_eq!(*rows, vec![vec![Datum::Text("(Joe,Blow,)".into())]]),
        other => panic!("{other:?}"),
    }
}

#[test]
fn newly_added_composite_fields_read_as_null_in_old_tuples() {
    let db = Database::in_memory().expect("db");
    let mut s = db.session("main", SessionOptions::default()).expect("session");
    s.simple_query("CREATE TABLE rt(a int); CREATE TABLE t(v rt); INSERT INTO t VALUES ('(1)'); ALTER TABLE rt ADD b int DEFAULT NULL").expect("setup");
    let out = s.simple_query("SELECT (v).a, (v).b FROM t").expect("field access");
    match &out[0].result {
        StatementResult::Rows { rows, .. } => assert_eq!(*rows, vec![vec![Datum::Int4(1),Datum::Null]]),
        other => panic!("{other:?}"),
    }
}
