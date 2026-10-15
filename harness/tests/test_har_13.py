"""HAR-13 tests: exact typed transcript comparison."""

import copy

from pgarbor_harness.cases.model import Case, Step
from pgarbor_harness.compare import typed
from pgarbor_harness.compare.typed import (
    DIFF_KINDS, compare_results, compare_steps, compare_transcripts, render_diff,
    resolve_order,
)
from pgarbor_harness.model import (
    ColumnDesc, Diff, ErrorInfo, Notice, StatementResult, StepObservation, Transcript,
)


def col(name="a", oid=23, **kw):
    return ColumnDesc(name, oid, **kw)


def rows_res(rows, cols=None, tag=None):
    cols = cols if cols is not None else [col()]
    return StatementResult("rows", columns=cols, rows=[list(r) for r in rows],
                           command_tag=tag or f"SELECT {len(rows)}")


def err_res(sqlstate="42P01", message="m", **kw):
    return StatementResult("error", error=ErrorInfo("ERROR", sqlstate, message, **kw))


def step_obs(index=0, results=None, **kw):
    return StepObservation(index, "a", "sql", results or [], **kw)


def make_case(*steps):
    return Case(id="t.x", title="t", family="t", feature_ids=["f"], steps=list(steps))


def sql_step(i, sql, **kw):
    return Step(i, "sql", "a", {"sql": sql}, **kw)


def cmp_res(ref, tgt, order="ordered", cm=True):
    return compare_results("r", ref, tgt, order, cm)


def kinds(diffs):
    return [d.kind for d in diffs]


def transcripts(case_sql, ref_results, tgt_results, **case_kw):
    case = make_case(sql_step(0, case_sql, **case_kw))
    r = Transcript("t.x", "reference", steps=[step_obs(0, ref_results, txn_status="I")])
    t = Transcript("t.x", "target", steps=[step_obs(0, tgt_results, txn_status="I")])
    return case, r, t


# --- one test per DIFF_KINDS entry --------------------------------------------------------


def test_missing_step():
    case = make_case(sql_step(0, "select 1"))
    r = Transcript("t.x", "reference", steps=[step_obs(0, [])])
    t = Transcript("t.x", "target", steps=[])
    d = compare_transcripts(case, r, t)
    assert kinds(d) == ["missing_step"] and d[0].path == "steps[0]"


def test_timeout_connection_blocked_txn_status_result_count():
    case = make_case(sql_step(0, "select 1"))
    ref = step_obs(0, [rows_res([["1"]])], txn_status="I", blocked_observed=False)
    tgt = step_obs(0, [], txn_status="E", blocked_observed=True, timed_out=True,
                   connection_lost=True)
    d = compare_steps("steps[0]", case, 0, ref, tgt)
    assert kinds(d) == ["timeout", "connection", "blocked", "txn_status", "result_count"]


def test_each_step_level_kind_alone():
    case = make_case(sql_step(0, "select 1"))
    base = step_obs(0, [], txn_status="I")
    for field, val, kind in (("timed_out", True, "timeout"),
                             ("connection_lost", True, "connection"),
                             ("blocked_observed", True, "blocked"),
                             ("txn_status", "E", "txn_status")):
        tgt = copy.deepcopy(base)
        setattr(tgt, field, val)
        assert kinds(compare_steps("s", case, 0, base, tgt)) == [kind]


def test_result_kind():
    d = cmp_res(rows_res([["1"]]), err_res("22012"))
    assert kinds(d) == ["result_kind"]
    assert "22012" in d[0].note


def test_sqlstate_and_error_message_and_fields():
    assert kinds(cmp_res(err_res("42P01"), err_res("42703"))) == ["sqlstate"]
    assert kinds(cmp_res(err_res(message="a"), err_res(message="b"))) == ["error_message"]
    assert cmp_res(err_res(message="a"), err_res(message="b"), cm=False) == []
    assert kinds(cmp_res(err_res(position="3"), err_res(position="4"))) == ["error_field"]
    assert kinds(cmp_res(err_res(detail="x"), err_res(detail="y"))) == ["error_field"]
    assert cmp_res(err_res(detail="x", hint="h", context="c"),
                   err_res(detail="y"), cm=False) == []
    assert kinds(cmp_res(err_res(constraint_name="c1"), err_res(constraint_name="c2"))) \
        == ["error_field"]
    a, b = err_res(), err_res()
    b.error.severity = "FATAL"
    assert kinds(cmp_res(a, b)) == ["error_field"]
    # sqlstate is compared even without messages
    assert kinds(cmp_res(err_res("42P01"), err_res("42703"), cm=False)) == ["sqlstate"]


def test_column_kinds():
    ref = rows_res([["1"]], cols=[col("a", 23, typmod=-1, format=0, table_oid=5,
                                      column_attnum=1)])
    assert kinds(cmp_res(ref, rows_res([["1"]], cols=[col("a", 23), col("b")]))) == \
        ["column_count", "column_origin", "column_origin"]
    for kw, kind in ((dict(name="z"), "column_name"), (dict(oid=25), "column_type"),
                     (dict(typmod=7), "column_typmod"), (dict(format=1), "column_format"),
                     (dict(table_oid=6), "column_origin"),
                     (dict(column_attnum=2), "column_origin")):
        base = dict(name="a", oid=23, typmod=-1, format=0, table_oid=5, column_attnum=1)
        base.update(kw)
        tgt = rows_res([["1"]], cols=[ColumnDesc(base["name"], base["oid"], base["typmod"],
                                                 base["format"], base["table_oid"],
                                                 base["column_attnum"])])
        assert kinds(cmp_res(ref, tgt)) == [kind], kw


def test_command_tag_exact():
    a = StatementResult("command", command_tag="INSERT 0 1")
    b = StatementResult("command", command_tag="INSERT 1")
    assert kinds(cmp_res(a, b)) == ["command_tag"]


def test_param_types():
    a = StatementResult("description", param_type_oids=[23])
    b = StatementResult("description", param_type_oids=[25])
    assert kinds(cmp_res(a, b)) == ["param_types"]


def test_row_order_and_row_count_and_value():
    d = cmp_res(rows_res([["1"], ["2"]]), rows_res([["2"], ["1"]]))
    assert kinds(d) == ["row_order"]
    assert kinds(cmp_res(rows_res([["1"], ["1"]]), rows_res([["1"]]))) == ["row_count"]
    d = cmp_res(rows_res([["1"], ["2"]]), rows_res([["1"], ["3"]]))
    assert kinds(d) == ["value"] and d[0].path == "r.rows[1][0]"
    assert (d[0].reference, d[0].target) == ("2", "3")


def test_row_multiset():
    d = cmp_res(rows_res([["1"], ["1"]]), rows_res([["1"]]), order="multiset")
    assert kinds(d) == ["row_multiset"]
    assert d[0].reference == [["1"]] and d[0].target == []
    assert cmp_res(rows_res([["1"], ["2"]]), rows_res([["2"], ["1"]]), order="multiset") == []


def test_notice():
    n1 = Notice("NOTICE", "00000", "a")
    n2 = Notice("NOTICE", "00000", "b")
    case = make_case(sql_step(0, "select 1"))
    ref = step_obs(0, [], txn_status="I", notices=[n1])
    assert kinds(compare_steps("s", case, 0, ref, step_obs(0, [], txn_status="I",
                                                           notices=[n2]))) == ["notice"]
    assert kinds(compare_steps("s", case, 0, ref, step_obs(0, [], txn_status="I"))) == \
        ["notice"]
    case.steps[0].compare_message = False
    assert compare_steps("s", case, 0, ref, step_obs(0, [], txn_status="I",
                                                     notices=[n2])) == []


def test_every_diff_kind_is_covered_by_this_file():
    produced = {
        "missing_step", "timeout", "connection", "blocked", "txn_status", "result_count",
        "result_kind", "sqlstate", "error_message", "error_field", "column_count",
        "column_name", "column_type", "column_typmod", "column_format", "column_origin",
        "command_tag", "param_types", "row_order", "row_count", "value", "row_multiset",
        "notice",
    }
    assert produced == set(DIFF_KINDS)


# --- extra required behaviors ---------------------------------------------------------------


def test_identical_transcripts_no_diff():
    case, r, t = transcripts("select a from t order by a", [rows_res([["1"], ["2"]])],
                             [rows_res([["1"], ["2"]])])
    assert compare_transcripts(case, r, t) == []


def test_null_vs_empty_string_detected():
    d = cmp_res(rows_res([[None]]), rows_res([[""]]))
    assert kinds(d) == ["value"]
    d = cmp_res(rows_res([[None]]), rows_res([[""]]), order="multiset")
    assert kinds(d) == ["row_multiset"]


def test_str_vs_bytes_and_precision_exact():
    assert kinds(cmp_res(rows_res([["1"]]), rows_res([[b"1"]]))) == ["value"]
    assert kinds(cmp_res(rows_res([["1.0"]]), rows_res([["1"]]))) == ["value"]


def test_duplicates_preserved_in_multiset():
    d = cmp_res(rows_res([["1"], ["1"], ["2"]]), rows_res([["1"], ["2"], ["2"]]),
                order="multiset")
    assert kinds(d) == ["row_multiset"]
    assert d[0].reference == [["1"]] and d[0].target == [["2"]]


def test_multiset_sort_and_truncation():
    ref = rows_res([[str(i)] for i in range(30)])
    d = cmp_res(ref, rows_res([]), order="multiset")
    assert len(d[0].reference) == 20
    mixed = rows_res([["b"], [None], [b"x"], ["a"]])
    d = cmp_res(mixed, rows_res([]), order="multiset")
    assert d[0].reference == [[None], [b"x"], ["a"], ["b"]]


def test_auto_order_without_order_by_ignores_reorder():
    case, r, t = transcripts("select a from t", [rows_res([["1"], ["2"]])],
                             [rows_res([["2"], ["1"]])])
    assert compare_transcripts(case, r, t) == []


def test_auto_order_with_order_by_reports_row_order():
    case, r, t = transcripts("select a from t order by a", [rows_res([["1"], ["2"]])],
                             [rows_res([["2"], ["1"]])])
    d = compare_transcripts(case, r, t)
    assert kinds(d) == ["row_order"] and d[0].path == "steps[0].results[0].rows"


def test_explicit_order_wins():
    case, r, t = transcripts("select a from t", [rows_res([["1"], ["2"]])],
                             [rows_res([["2"], ["1"]])], order="ordered")
    assert kinds(compare_transcripts(case, r, t)) == ["row_order"]
    case, r, t = transcripts("select a from t order by a", [rows_res([["1"], ["2"]])],
                             [rows_res([["2"], ["1"]])], order="multiset")
    assert compare_transcripts(case, r, t) == []


def test_duplicate_rows_diff_in_ordered_and_multiset_modes():
    case, r, t = transcripts("select a from t order by a", [rows_res([["1"], ["1"]])],
                             [rows_res([["1"]])])
    assert kinds(compare_transcripts(case, r, t)) == ["row_count"]
    case, r, t = transcripts("select a from t", [rows_res([["1"], ["1"]])],
                             [rows_res([["1"]])])
    assert kinds(compare_transcripts(case, r, t)) == ["row_multiset"]


def test_resolve_order_per_statement_and_execute_and_wait():
    case = make_case(
        sql_step(0, "select 1 order by 1; select 2"),
        Step(1, "parse", "a", {"name": "p", "sql": "select a from t order by a",
                               "param_types": []}),
        Step(2, "execute", "a", {"statement": "p", "params": [], "param_formats": [],
                                 "result_format": "text"}),
        Step(3, "exec_params", "a", {"sql": "select a from t", "params": [],
                                     "param_types": [], "param_formats": [],
                                     "result_format": "text"}),
        Step(4, "sql", "b", {"sql": "select a from t order by a"}, is_async=True),
        Step(5, "wait", "b", {}),
    )
    assert resolve_order(case, 0, 0, 2) == "ordered"
    assert resolve_order(case, 0, 1, 2) == "multiset"
    assert resolve_order(case, 0, 0, 1) == "ordered"  # count mismatch -> any ORDER BY
    assert resolve_order(case, 2, 0, 1) == "ordered"
    assert resolve_order(case, 3, 0, 1) == "multiset"
    assert resolve_order(case, 5, 0, 1) == "ordered"


def test_diffs_in_transcript_order_and_setup_first():
    case = make_case(sql_step(0, "select 1"), sql_step(1, "select 2"))
    r = Transcript("t.x", "reference",
                   setup=[step_obs(0, [StatementResult("command", command_tag="CREATE TABLE")],
                                   txn_status="I")],
                   steps=[step_obs(0, [], txn_status="I"), step_obs(1, [], txn_status="I")])
    t = copy.deepcopy(r)
    t.steps[1].txn_status = "E"
    t.steps[0].txn_status = "T"
    t.setup[0].results[0].command_tag = "CREATE VIEW"
    paths = [d.path for d in compare_transcripts(case, r, t)]
    assert paths == ["setup[0].results[0].command_tag", "steps[0].txn_status",
                     "steps[1].txn_status"]


def test_inputs_not_mutated():
    case, r, t = transcripts("select a from t", [rows_res([["1"]])], [rows_res([["2"]])])
    r0, t0 = copy.deepcopy(r), copy.deepcopy(t)
    compare_transcripts(case, r, t)
    assert r == r0 and t == t0


def test_elapsed_ms_not_compared():
    case = make_case(sql_step(0, "select 1"))
    a = step_obs(0, [], txn_status="I", elapsed_ms=1)
    b = step_obs(0, [], txn_status="I", elapsed_ms=999)
    assert compare_steps("s", case, 0, a, b) == []


def test_render_diff():
    d = [Diff("steps[2].results[0].rows[1][0]", "value", "2", "3"),
         Diff("steps[3]", "missing_step", None, "x" * 500, "note")]
    out = typed.render_diff(d).splitlines()
    assert out[0] == "steps[2].results[0].rows[1][0]: value: reference='2' target='3'"
    assert out[1].startswith("steps[3]: missing_step: reference=NULL target='xxx")
    assert len(out[1]) < 330 and out[1].endswith("(note)")
    many = [Diff(f"p{i}", "value", "a", "b") for i in range(5)]
    assert len(render_diff(many, limit=2).splitlines()) == 3
