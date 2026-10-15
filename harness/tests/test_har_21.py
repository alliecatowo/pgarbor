"""Tests for the sqllogictest parser/writer (ORACLE-07, lease HAR-21)."""

import hashlib

import pytest

from pgarbor_harness.sqllogictest.parser import (
    SltRecord,
    SltSyntaxError,
    format_value,
    parse,
    render_values,
    write,
)

SAMPLE = """\
# PGArbor sample file
# comments are dropped by write()

statement ok
CREATE TABLE t(a int, b text)

statement ok
INSERT INTO t VALUES (1, 'x'), (2, NULL), (3, '')

statement error 23505
INSERT INTO t VALUES (1, 'dup')

query IT rowsort lbl1
SELECT a, b FROM t
----
1
x
2
NULL
3
(empty)

query T valuesort
SELECT b FROM t
----
x
NULL
(empty)

skipif pgarbor
query I nosort
SELECT 1
----
1

onlyif sqlite
statement ok
SELECT 2

hash-threshold 4

query II nosort
SELECT a, a FROM t
----
1
1
2
2
3
3
4
4
5
5
6
6
7
7
8
8
9
9
10
10
11
11

halt
"""


def _kinds(records):
    return [r.kind for r in records]


def test_parse_sample_structure():
    recs = parse(SAMPLE, "sample.slt")
    assert _kinds(recs) == [
        "statement", "statement", "statement",
        "query", "query",
        "query", "statement", "hash-threshold", "query", "halt",
    ]
    ok, ins, err = recs[0], recs[1], recs[2]
    assert ok.expect_ok is True and ok.sqlstate is None
    assert ok.sql == "CREATE TABLE t(a int, b text)"
    assert err.expect_ok is False and err.sqlstate == "23505"
    assert err.line == 10


def test_parse_query_fields_and_expected_values():
    recs = parse(SAMPLE)
    q = recs[3]
    assert q.kind == "query"
    assert q.types == "IT"
    assert q.sort == "rowsort"
    assert q.label == "lbl1"
    assert q.expected == ["1", "x", "2", "NULL", "3", "(empty)"]
    flat = recs[4]
    assert flat.sort == "valuesort" and flat.label is None


def test_parse_conditions_and_hash_threshold():
    recs = parse(SAMPLE)
    assert recs[5].conditions == [("skipif", "pgarbor")]
    assert recs[6].conditions == [("onlyif", "sqlite")]
    assert recs[7].hash_threshold == 4
    assert recs[7].conditions == []
    assert recs[9].kind == "halt"


def test_round_trip_sample():
    recs = parse(SAMPLE, "sample.slt")
    assert parse(write(recs), "sample.slt") == recs


def test_write_is_stable_after_one_pass():
    once = write(parse(SAMPLE))
    assert write(parse(once)) == once


def test_round_trip_unrecorded_query():
    recs = parse("query I\nSELECT 1\n")
    assert recs[0].expected is None
    assert parse(write(recs)) == recs
    assert "----" not in write(recs)


def test_round_trip_empty_expected_section():
    recs = parse("query I rowsort\nSELECT 1 WHERE false\n----\n")
    assert recs[0].expected == []
    assert parse(write(recs)) == recs


def test_statement_error_without_sqlstate():
    recs = parse("statement error\nSELECT bogus\n")
    assert recs[0].expect_ok is False
    assert recs[0].sqlstate is None
    assert parse(write(recs)) == recs


def test_crlf_input_is_accepted():
    recs = parse("statement ok\r\nSELECT 1\r\n\r\nhalt\r\n")
    assert _kinds(recs) == ["statement", "halt"]
    assert recs[0].sql == "SELECT 1"


def test_multiline_sql_round_trips():
    text = "statement ok\nCREATE TABLE x(\n  a int\n)\n"
    recs = parse(text)
    assert recs[0].sql == "CREATE TABLE x(\n  a int\n)"
    assert parse(write(recs)) == recs


def test_format_value_rules():
    assert format_value(None) == "NULL"
    assert format_value("") == "(empty)"
    assert format_value("NULL") == "NULL"
    assert format_value("a\nb") == "a\\nb"
    assert format_value("a\\b") == "a\\\\b"
    assert format_value("1.5") == "1.5"


def test_render_values_nosort_keeps_row_major_order():
    rows = [["2", None], ["1", ""]]
    assert render_values(rows, "nosort", 0) == ["2", "NULL", "1", "(empty)"]


def test_render_values_rowsort_sorts_rows_by_value_lists():
    rows = [["b", "1"], ["a", "2"], ["a", "10"]]
    assert render_values(rows, "rowsort", 0) == ["a", "10", "a", "2", "b", "1"]


def test_render_values_valuesort_sorts_flat():
    rows = [["b", "1"], ["a", "2"]]
    assert render_values(rows, "valuesort", 0) == ["1", "2", "a", "b"]


def test_render_values_unknown_sort_rejected():
    with pytest.raises(ValueError):
        render_values([["1"]], "sideways", 0)


def test_render_values_hash_threshold_applies_above_threshold_only():
    rows = [["1", "2"], ["3", "4"]]
    assert render_values(rows, "nosort", 4) == ["1", "2", "3", "4"]
    values = ["1", "2", "3", "4", "5"]
    expected_digest = hashlib.md5(
        "".join(v + "\n" for v in values).encode("utf-8")
    ).hexdigest()
    rows5 = [["1", "2", "3", "4", "5"]]
    assert render_values(rows5, "nosort", 4) == [f"5 values hashing to {expected_digest}"]


def test_render_values_hash_uses_final_sorted_order():
    rows = [["b", "2"], ["a", "1"], ["c", "3"]]
    rendered = render_values(rows, "valuesort", 2)
    ordered = ["1", "2", "3", "a", "b", "c"]
    digest = hashlib.md5("".join(v + "\n" for v in ordered).encode("utf-8")).hexdigest()
    assert rendered == [f"6 values hashing to {digest}"]


def test_render_values_zero_threshold_disables_hashing():
    rows = [["1", "2", "3"]]
    assert render_values(rows, "nosort", 0) == ["1", "2", "3"]


def test_hash_threshold_zero_is_parsed():
    recs = parse("hash-threshold 0\n")
    assert recs[0].hash_threshold == 0


def test_skipif_and_onlyif_attach_to_next_record_only():
    recs = parse("skipif pgarbor\nonlyif sqlite\nstatement ok\nSELECT 1\n\nhalt\n")
    assert recs[0].conditions == [("skipif", "pgarbor"), ("onlyif", "sqlite")]
    assert recs[1].conditions == []


def test_comment_lines_between_records_are_ignored():
    recs = parse("# one\n\n# two\nhalt\n")
    assert _kinds(recs) == ["halt"]
    assert recs[0].line == 4


def test_query_without_separator_has_no_expected_values():
    recs = parse("query I\nSELECT 1\n\nhalt\n")
    assert recs[0].expected is None
    assert recs[1].kind == "halt"


@pytest.mark.parametrize(
    "text, fragment",
    [
        ("statement maybe\nSELECT 1\n", "expects 'ok' or 'error'"),
        ("statement error 2300\nSELECT 1\n", "invalid SQLSTATE"),
        ("statement error 23a05\nSELECT 1\n", "invalid SQLSTATE"),
        ("statement ok\n", "no SQL"),
        ("query X\nSELECT 1\n", "invalid column types"),
        ("query\nSELECT 1\n", "needs a column type"),
        ("hash-threshold -1\n", "non-negative integer"),
        ("hash-threshold\n", "non-negative integer"),
        ("halt now\n", "takes no arguments"),
        ("frobnicate\n", "unknown record type"),
        ("skipif pgarbor\n", "not followed by a record"),
        ("skipif\nhalt\n", "exactly one engine name"),
    ],
)
def test_syntax_errors_carry_name_and_line(text, fragment):
    with pytest.raises(SltSyntaxError) as info:
        parse(text, "bad.slt")
    message = str(info.value)
    assert message.startswith("bad.slt:")
    assert fragment in message


def test_syntax_error_reports_record_line():
    with pytest.raises(SltSyntaxError, match=r"^bad\.slt:3: "):
        parse("halt\n\nfrobnicate\n", "bad.slt")


def test_write_rejects_unknown_kind():
    with pytest.raises(ValueError):
        write([SltRecord(kind="bogus", line=1)])


def test_write_empty_list_is_empty_text():
    assert write([]) == ""
