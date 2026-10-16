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

#[test]
fn show_and_txn_control_take_no_snapshot_and_rollback_discards() {
    let db = Database::in_memory().expect("create");
    let mut s = db.session("main", SessionOptions::default()).expect("session");
    s.simple_query("SELECT 1").expect("warm up");
    let out = s.simple_query("SHOW server_version").expect("SHOW needs no snapshot");
    assert_eq!(out[0].result.tag(), "SHOW");
    s.simple_query("CREATE TABLE t(a int primary key)").expect("create");
    s.simple_query("INSERT INTO t VALUES (1)").expect("insert");
    assert_eq!(s.simple_query("BEGIN").expect("begin")[0].result.tag(), "BEGIN");
    s.simple_query("INSERT INTO t VALUES (2)").expect("insert in block");
    assert_eq!(s.simple_query("ROLLBACK").expect("rollback")[0].result.tag(), "ROLLBACK");
    match &s.simple_query("SELECT count(*) FROM t").expect("count")[0].result {
        StatementResult::Rows { rows, .. } => assert_eq!(rows, &vec![vec![Datum::Int8(1)]]),
        other => panic!("expected rows: {other:?}"),
    }
    let out = s.simple_query("BEGIN; INSERT INTO t VALUES (3); ROLLBACK").expect("multi-statement block");
    assert_eq!(out.last().unwrap().result.tag(), "ROLLBACK");
}

fn count_rows(s: &mut pgarbor::Session, sql: &str) -> usize {
    match &s.simple_query(sql).unwrap_or_else(|e| panic!("{sql}: {e:?}"))[0].result {
        StatementResult::Rows { rows, .. } => rows.len(),
        other => panic!("expected rows: {other:?}"),
    }
}

#[test]
fn update_delete_information_schema_and_subquery_filter() {
    let db = Database::in_memory().expect("create");
    let mut s = db.session("main", SessionOptions::default()).expect("session");
    s.simple_query("CREATE TABLE t(a int primary key, b text); INSERT INTO t VALUES (1,'x'),(2,'y')").expect("setup");
    assert_eq!(s.simple_query("UPDATE t SET b = b || '!' WHERE a = 1").expect("update")[0].result.tag(), "UPDATE 1");
    assert_eq!(count_rows(&mut s, "SELECT 1 FROM t WHERE b = 'x!'"), 1);
    assert_eq!(s.simple_query("DELETE FROM t WHERE a = 2").expect("delete")[0].result.tag(), "DELETE 1");
    assert_eq!(count_rows(&mut s, "SELECT 1 FROM t"), 1);
    // information_schema columns are builtin domains (sql_identifier) and the WHERE must filter.
    assert_eq!(count_rows(&mut s, "SELECT table_name FROM information_schema.tables WHERE table_schema='public'"), 1);
    // A WHERE over a FROM-subquery (what views expand to) must not be dropped.
    assert_eq!(count_rows(&mut s, "SELECT * FROM (SELECT a+1 AS b FROM t) q WHERE b = 99"), 0);
    assert_eq!(count_rows(&mut s, "SELECT * FROM (SELECT a+1 AS b FROM t) q WHERE b = 2"), 1);
}

#[test]
fn identity_sequence_name_option() {
    let db = Database::in_memory().expect("create");
    let mut s = db.session("main", SessionOptions::default()).expect("session");
    // SEQUENCE NAME may appear among the other options in any order (PG gram.y SeqOptList).
    s.simple_query(
        r#"CREATE TABLE t (id bigint PRIMARY KEY GENERATED ALWAYS AS IDENTITY (sequence name "t_id_seq" INCREMENT BY 1 START WITH 1 MINVALUE 1 MAXVALUE 9223372036854775807 CACHE 1), v int)"#,
    )
    .expect("identity with SEQUENCE NAME");
    s.simple_query("CREATE TABLE u (id int GENERATED BY DEFAULT AS IDENTITY (SEQUENCE NAME my_ids START WITH 40 INCREMENT 2))")
        .expect("explicit sequence name in a second table");
    s.simple_query("INSERT INTO u VALUES (DEFAULT)").expect("insert");
    // The identity sequence is the named one: its first value was 40, the next is 42.
    match &s.simple_query("SELECT nextval('my_ids')").expect("nextval")[0].result {
        StatementResult::Rows { rows, .. } => assert_eq!(rows, &vec![vec![Datum::Int8(42)]]),
        other => panic!("expected rows: {other:?}"),
    }
    // The name is taken: a second identity or sequence with it is 42P07.
    assert_eq!(
        s.simple_query("CREATE SEQUENCE my_ids").unwrap_err().sqlstate.code(),
        "42P07"
    );
    assert_eq!(
        s.simple_query("CREATE TABLE w (id int GENERATED ALWAYS AS IDENTITY (SEQUENCE NAME my_ids))")
            .unwrap_err()
            .sqlstate
            .code(),
        "42P07"
    );
    // ALTER ... ADD GENERATED takes the same option list.
    s.simple_query("CREATE TABLE t0 (a int NOT NULL, b int)").expect("create t0");
    s.simple_query("ALTER TABLE t0 ALTER COLUMN a ADD GENERATED ALWAYS AS IDENTITY (SEQUENCE NAME a_ids START 9)")
        .expect("add identity with SEQUENCE NAME");
    s.simple_query("INSERT INTO t0(b) VALUES (1)").expect("insert t0");
    assert_eq!(count_rows(&mut s, "SELECT 1 FROM t0 WHERE a = 9"), 1);
    match &s.simple_query("SELECT nextval('a_ids')").expect("nextval a_ids")[0].result {
        StatementResult::Rows { rows, .. } => assert_eq!(rows, &vec![vec![Datum::Int8(10)]]),
        other => panic!("expected rows: {other:?}"),
    }
    // Not a valid sequence option.
    assert_eq!(
        s.simple_query("CREATE TABLE x (id int GENERATED ALWAYS AS IDENTITY (SEQUENCE x))").unwrap_err().sqlstate.code(),
        "42601"
    );
}

#[test]
fn information_schema_columns_is_served_by_its_producer() {
    let db = Database::in_memory().expect("create");
    let mut s = db.session("main", SessionOptions::default()).expect("session");
    // Virtual system views are served by their producers, not by re-parsing the stored definition
    // (whose `a.*` function arguments are whole-row references).
    assert!(count_rows(&mut s, "SELECT table_name FROM information_schema.columns") > 0);
    match &s
        .simple_query(
            "SELECT table_name, column_name, data_type, ordinal_position FROM information_schema.columns \
             WHERE table_schema = 'pg_catalog' AND table_name = 'pg_class' ORDER BY ordinal_position LIMIT 2",
        )
        .expect("pg_class columns")[0]
        .result
    {
        StatementResult::Rows { rows, .. } => assert_eq!(
            rows,
            &vec![
                vec![Datum::Text("pg_class".into()), Datum::Text("oid".into()), Datum::Text("oid".into()), Datum::Int4(1)],
                vec![Datum::Text("pg_class".into()), Datum::Text("relname".into()), Datum::Text("name".into()), Datum::Int4(2)],
            ]
        ),
        other => panic!("expected rows: {other:?}"),
    }
}

#[test]
fn utility_and_transaction_control_can_be_first_on_fresh_session() {
    for sql in ["SHOW server_version", "BEGIN", "ROLLBACK", "SET application_name = 'first'", "SHOW ALL"] {
        let db = Database::in_memory().expect("database");
        let mut s = db.session("main", SessionOptions::default()).expect("fresh session");
        let out = s.simple_query_raw(sql);
        assert!(out.error.is_none(), "first statement {sql}: {:?}", out.error);
        assert_eq!(out.statements.len(), 1, "{sql}");
        if sql == "BEGIN" {
            assert_eq!(out.status, pgarbor::TxnStatus::InTransaction);
            assert_eq!(s.simple_query("ROLLBACK").expect("rollback")[0].result.tag(), "ROLLBACK");
        }
    }
}

#[test]
fn unsupported_security_and_deferred_constraints_fail_closed() {
    let db = Database::in_memory().expect("database");
    let mut s = db.session("main", SessionOptions::default()).expect("session");
    s.simple_query("CREATE TABLE t(id int primary key)").expect("create");
    for sql in ["ALTER TABLE t ENABLE ROW LEVEL SECURITY", "ALTER TABLE t FORCE ROW LEVEL SECURITY", "CREATE TABLE d(id int UNIQUE DEFERRABLE INITIALLY DEFERRED)"] {
        let e = s.simple_query(sql).expect_err("must reject unenforced semantics");
        assert_eq!(e.sqlstate.code(), "0A000", "{sql}: {e:?}");
        assert!(e.is_unimplemented());
    }
    s.simple_query("INSERT INTO t VALUES (1)").expect("transaction recovers");
}

#[test]
fn sql_function_pseudo_types_are_rejected_at_definition() {
    let db = Database::in_memory().expect("create");
    let mut s = db.session("main", SessionOptions::default()).expect("session");
    let e = s.simple_query("CREATE FUNCTION int_arg(internal) RETURNS int LANGUAGE sql AS 'SELECT 1'").unwrap_err();
    assert_eq!(e.sqlstate.code(), "42P13");
    assert_eq!(e.message, "SQL functions cannot have arguments of type internal");
    let e = s.simple_query("CREATE FUNCTION noargs() RETURNS anyelement LANGUAGE sql AS 'SELECT 1'").unwrap_err();
    assert_eq!(e.sqlstate.code(), "42P13");
    assert_eq!(e.message, "cannot determine result data type");
    let out = s.simple_query("SELECT count(*) FROM pg_proc WHERE proname IN ('int_arg', 'noargs')").expect("count");
    match &out[0].result {
        StatementResult::Rows { rows, .. } => assert_eq!(rows, &vec![vec![Datum::Int8(0)]]),
        other => panic!("expected rows: {other:?}"),
    }
}

#[test]
fn stored_config_settings_and_text_search_config_are_validated() {
    let db = Database::in_memory().expect("create");
    let mut s = db.session("main", SessionOptions::default()).expect("session");
    s.simple_query("CREATE FUNCTION cfg1(text) RETURNS text LANGUAGE sql AS 'SELECT $1'").expect("create");
    s.simple_query("CREATE ROLE cfg_role").expect("role");
    assert_eq!(s.simple_query("ALTER FUNCTION cfg1(text) SET nosuchparam = 1").unwrap_err().sqlstate.code(), "42704");
    assert_eq!(s.simple_query("ALTER FUNCTION cfg1(text) SET work_mem = 'bogus'").unwrap_err().sqlstate.code(), "22023");
    assert_eq!(s.simple_query("ALTER ROLE cfg_role SET no_such_param = 1").unwrap_err().sqlstate.code(), "42704");
    assert_eq!(s.simple_query("ALTER ROLE cfg_role SET maintenance_work_mem = 'abc'").unwrap_err().sqlstate.code(), "22023");
    s.simple_query("ALTER ROLE cfg_role SET maintenance_work_mem = '64MB'").expect("valid role setting");
    s.simple_query("ALTER FUNCTION cfg1(text) SET search_path = admin, pg_temp").expect("valid function setting");
    assert_eq!(s.simple_query("SET default_text_search_config = 'nosuch'").unwrap_err().sqlstate.code(), "22023");
    s.simple_query("SET default_text_search_config = 'English'").expect("builtin config");
    s.simple_query("SET default_text_search_config = '\"pg_catalog\".\"english\"'").expect("quoted lowercase");
    assert_eq!(s.simple_query("SET default_text_search_config = '\"English\"'").expect_err("quoted mixed case").sqlstate.code(), "22023");
    let out = s.simple_query("SHOW default_text_search_config").expect("show");
    match &out[0].result {
        StatementResult::Rows { rows, .. } => assert_eq!(rows, &vec![vec![Datum::Text("pg_catalog.english".to_string())]]),
        other => panic!("expected rows: {other:?}"),
    }
}
