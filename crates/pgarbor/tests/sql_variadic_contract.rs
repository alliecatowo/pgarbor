//! VARIADIC call contract (PG `parse_func.c` / `func_get_detail`): `f(VARIADIC arr)` on a
//! `VARIADIC "any"` builtin unpacks the array, a NULL array gives NULL for the json/concat/num_nulls
//! family, and VARIADIC is only accepted as the last argument.
use pgarbor::{Database, SessionOptions, StatementResult};
use pgarbor_types::Datum;

fn one_value(sql: &str) -> Datum {
    let db = Database::in_memory().expect("create");
    let mut s = db.session("main", SessionOptions::default()).expect("session");
    let out = s.simple_query(sql).unwrap_or_else(|e| panic!("{sql}: {e:?}"));
    match &out.last().expect("statement").result {
        StatementResult::Rows { rows, .. } => rows[0][0].clone(),
        other => panic!("expected rows for {sql}: {other:?}"),
    }
}

fn sqlstate(sql: &str) -> String {
    let db = Database::in_memory().expect("create");
    let mut s = db.session("main", SessionOptions::default()).expect("session");
    s.simple_query(sql).unwrap_err().sqlstate.code().to_string()
}

#[test]
fn variadic_any_positional_arguments_are_unpacked() {
    assert_eq!(one_value("SELECT concat(1, 'a', NULL, 2)"), Datum::Text("1a2".into()));
    assert_eq!(one_value("SELECT format('%s-%s', 'x', 2)"), Datum::Text("x-2".into()));
}

#[test]
fn variadic_keyword_unpacks_array_elements() {
    assert_eq!(one_value("SELECT concat(VARIADIC ARRAY['x', NULL, 'z']::text[])"), Datum::Text("xz".into()));
    assert_eq!(one_value("SELECT format('%s, %s', VARIADIC array[1, 2])"), Datum::Text("1, 2".into()));
    assert_eq!(one_value("SELECT num_nulls(VARIADIC '{1,2,NULL,3}'::int[])"), Datum::Int4(1));
}

#[test]
fn variadic_null_array_yields_null_for_build_and_concat_functions() {
    assert_eq!(one_value("SELECT concat(VARIADIC NULL::text[]) IS NULL"), Datum::Bool(true));
    assert_eq!(one_value("SELECT jsonb_build_array(VARIADIC NULL::text[]) IS NULL"), Datum::Bool(true));
    assert_eq!(one_value("SELECT num_nulls(VARIADIC NULL::text[]) IS NULL"), Datum::Bool(true));
}

#[test]
fn variadic_must_be_last_argument() {
    assert_eq!(sqlstate("SELECT concat(VARIADIC ARRAY['a'], 'b')"), "42601");
}

#[test]
fn variadic_calls_inside_ddl_expressions() {
    let db = Database::in_memory().expect("db");
    let mut s = db.session("main", SessionOptions::default()).expect("session");
    s.simple_query("CREATE TABLE t(v text DEFAULT concat(VARIADIC ARRAY['a','b'])); INSERT INTO t DEFAULT VALUES; CREATE VIEW v AS SELECT concat(VARIADIC ARRAY[v,'c']) AS x FROM t").expect("DDL expressions");
    let out = s.simple_query("SELECT x FROM v").expect("view");
    match &out[0].result {
        StatementResult::Rows { rows, .. } => assert_eq!(*rows, vec![vec![Datum::Text("abc".into())]]),
        other => panic!("{other:?}"),
    }
}
