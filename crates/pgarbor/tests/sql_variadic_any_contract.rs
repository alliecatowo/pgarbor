//! `VARIADIC "any"` builtins (concat, concat_ws, format, json_build_array/object, num_nulls,
//! num_nonnulls, ...) take any number of trailing arguments, unpacked, with no array packing
//! (PG `parse_func.c` / `FuncnameGetCandidates`). The binder must accept more call arguments than
//! declared parameters for these functions instead of failing with an internal error.
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
fn concat_accepts_many_unpacked_arguments() {
    assert_eq!(one_value("SELECT concat('a', 'b', 'c')"), Datum::Text("abc".into()));
    assert_eq!(one_value("SELECT concat('a', NULL, 'c', 'd', 'e')"), Datum::Text("acde".into()));
    assert_eq!(one_value("SELECT concat(1, 2.5, true, NULL, 'x')"), Datum::Text("12.5tx".into()));
}

#[test]
fn concat_ws_accepts_many_unpacked_arguments() {
    assert_eq!(one_value("SELECT concat_ws(',', 'a', NULL, 'b', 'c')"), Datum::Text("a,b,c".into()));
    assert_eq!(one_value("SELECT concat_ws(',', 'a')"), Datum::Text("a".into()));
}

#[test]
fn format_accepts_extra_trailing_arguments() {
    assert_eq!(one_value("SELECT format('%s %s', 'a', 'b', 'c')"), Datum::Text("a b".into()));
    assert_eq!(one_value("SELECT format('x')"), Datum::Text("x".into()));
    assert_eq!(one_value("SELECT format('%1$s', 'a')"), Datum::Text("a".into()));
}

#[test]
fn json_and_null_counting_builtins_accept_many_arguments() {
    assert_eq!(one_value("SELECT json_build_array(1, NULL, 'x')::text"), Datum::Text("[1, null, \"x\"]".into()));
    assert_eq!(one_value("SELECT num_nulls(1, NULL, 3, NULL)"), Datum::Int4(2));
    assert_eq!(one_value("SELECT num_nonnulls(1, NULL, 3, NULL)"), Datum::Int4(2));
    assert_eq!(one_value("SELECT jsonb_build_object('a', 1, 'b', 'x')::text"), Datum::Text("{\"a\": 1, \"b\": \"x\"}".into()));
}

#[test]
fn zero_variadic_arguments_follow_pg() {
    assert_eq!(one_value("SELECT json_build_array()::text"), Datum::Text("[]".into()));
    assert_eq!(sqlstate("SELECT format('%s')"), "22023");
    assert_eq!(sqlstate("SELECT concat()"), "42883");
}
