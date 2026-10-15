"""Tests for pgarbor_harness.sqltext (lease HAR-08)."""

import doctest

from pgarbor_harness import sqltext
from pgarbor_harness.sqltext import (
    SqlStatement,
    first_keyword,
    has_top_level_order_by,
    message_template,
    split_statements,
    strip_comments,
)


def _texts(script):
    return [s.text for s in split_statements(script)]


def test_module_doctests():
    result = doctest.testmod(sqltext, verbose=False)
    assert result.failed == 0
    assert result.attempted > 0


def test_acceptance_three_statements():
    assert _texts("select 1; select ';' ; $$a;b$$;") == ["select 1", "select ';'", "$$a;b$$"]


def test_nested_block_comment_is_one_statement():
    assert _texts("select 1 /* a /* b */ c; */ ; select 2") == [
        "select 1 /* a /* b */ c; */",
        "select 2",
    ]


def test_e_string_backslash_quote():
    # SQL text: select E'\'' ; select 2
    assert _texts("select E'\\''; select 2") == ["select E'\\''", "select 2"]


def test_e_string_double_backslash_then_close():
    # SQL text: select E'\\'; select 2
    assert _texts("select E'\\\\'; select 2") == ["select E'\\\\'", "select 2"]


def test_standard_string_backslash_is_literal():
    # SQL text: select 'a\'; select 2  (the backslash does not escape in a standard string)
    assert _texts("select 'a\\'; select 2") == ["select 'a\\'", "select 2"]


def test_doubled_quotes_in_strings_and_identifiers():
    assert _texts("select 'it''s'; select \"a;b\"\"c\" from t; select 3") == [
        "select 'it''s'",
        'select "a;b""c" from t',
        "select 3",
    ]


def test_dollar_tag_quotes_and_params():
    assert _texts("select $t$a;$$b$t$; select $1; select 2") == [
        "select $t$a;$$b$t$",
        "select $1",
        "select 2",
    ]


def test_dollar_cannot_start_inside_identifier():
    assert _texts("select a$$b from t; select 2") == ["select a$$b from t", "select 2"]


def test_begin_atomic_body_is_not_split():
    script = (
        "create function f() returns int language sql "
        "begin atomic select 1; select 2; end; select 3"
    )
    assert _texts(script) == [
        "create function f() returns int language sql begin atomic select 1; select 2; end",
        "select 3",
    ]


def test_begin_atomic_case_end_nesting():
    script = (
        "create or replace function f() returns int language sql "
        "begin atomic select case when true then 1 else 2 end; end; select 3"
    )
    assert _texts(script) == [
        "create or replace function f() returns int language sql begin atomic "
        "select case when true then 1 else 2 end; end",
        "select 3",
    ]


def test_begin_outside_routine_splits():
    assert _texts("begin; select 1; end") == ["begin", "select 1", "end"]


def test_unbalanced_paren_does_not_split():
    assert _texts("select (1; select 2") == ["select (1; select 2"]


def test_comment_only_statements_dropped_and_offsets_exact():
    script = "-- hi\n; select 1; /* x */"
    stmts = split_statements(script)
    assert [s.text for s in stmts] == ["select 1"]
    assert stmts[0] == SqlStatement("select 1", 8, 16)
    assert script[stmts[0].start : stmts[0].end] == "select 1"


def test_offsets_match_text_for_each_statement():
    script = "  select 1 ;\n\n select 'x;y' ;  select 3  "
    for s in split_statements(script):
        assert script[s.start : s.end] == s.text
        assert s.text == s.text.strip()


def test_split_empty_and_whitespace():
    assert split_statements("") == []
    assert split_statements(" \n ;;  ") == []


def test_strip_comments_preserves_strings():
    assert strip_comments("select '--x' -- c") == "select '--x'  "
    assert strip_comments("select 1 /* a /* b */ c */ + 2") == "select 1   + 2"
    assert strip_comments("select 'a /* b */ c'") == "select 'a /* b */ c'"


def test_order_by_detection():
    assert has_top_level_order_by("select a from t order by a")
    assert has_top_level_order_by("select 1 order /* x */ by 1")
    assert has_top_level_order_by("with x as (select 1) select * from x order by 1")
    assert has_top_level_order_by("select 1 union all select 2 order by 1")
    assert has_top_level_order_by("values (1), (2) order by 1")
    assert not has_top_level_order_by("select row_number() over (order by a) from t")
    assert not has_top_level_order_by("select array_agg(x order by y) from t")
    assert not has_top_level_order_by("select string_agg(x, ',' order by y) from t")
    assert not has_top_level_order_by("select (select 1 order by 1)")
    assert not has_top_level_order_by("with x as (select 1 order by 1) select * from x")
    assert not has_top_level_order_by("select 'order by' from t")
    assert not has_top_level_order_by("select 1 /* order by 1 */")
    assert not has_top_level_order_by("values (1)")
    assert not has_top_level_order_by("show search_path")


def test_order_by_special_keywords():
    assert has_top_level_order_by("explain select 1")
    assert has_top_level_order_by("fetch next from c")
    assert has_top_level_order_by("move forward 1 from c")


def test_first_keyword():
    assert first_keyword("") == ""
    assert first_keyword("   ") == ""
    assert first_keyword("  /* c */ select 1") == "SELECT"
    assert first_keyword("-- c\ninsert into t values (1)") == "INSERT"
    assert first_keyword("with x as (select 1) insert into t select * from x") == "INSERT"
    assert first_keyword("WITH RECURSIVE r(n) AS (VALUES (1)) SELECT n FROM r") == "SELECT"
    assert first_keyword("with x(a) as materialized (select 1) update t set a = 1") == "UPDATE"
    assert first_keyword("with x as not materialized (select 1) delete from t") == "DELETE"
    assert first_keyword("select 1; drop table t") == "SELECT"
    assert first_keyword("explain (analyze) select 1") == "EXPLAIN"
    assert first_keyword("(select 1)") == "("


def test_message_template():
    assert message_template('relation "foo" does not exist') == 'relation "?" does not exist'
    assert (
        message_template("value too long for type character varying(5)")
        == "value too long for type character varying(N)"
    )
    assert (
        message_template('invalid input syntax for type integer: "abc"')
        == 'invalid input syntax for type integer: "?"'
    )
    assert message_template("value 'x' out of range 12.5") == "value '?' out of range N"
    assert message_template("there is no parameter $1") == "there is no parameter $N"
    assert (
        message_template('value "12.5" is out of range for type int4')
        == 'value "?" is out of range for type int4'
    )
