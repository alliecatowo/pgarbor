"""HAR-06 tests: hand-written pgarbor-case/1 validator (ORACLE-03).

Pure Python: no oracle container needed.
"""

from __future__ import annotations

import copy

import pytest
import yaml

from pgarbor_harness.cases import validate as V
from pgarbor_harness.config import paths

ROOT = paths().root
REL = "tests/cases/smoke/x.yaml"
EXAMPLES = [
    "tests/cases/smoke/create-insert-select.yaml",
    "tests/cases/txn/read-committed-blocking-update.yaml",
    "tests/cases/errors/unique-violation-extended.yaml",
]
SCHEMA = V.load_schema()
BASE = {
    "id": "smoke.x",
    "title": "t",
    "family": "smoke",
    "feature_ids": ["select"],
    "steps": [{"sql": "SELECT 1"}],
}


def mk(steps, **changes):
    d = copy.deepcopy(BASE)
    d["steps"] = steps
    d.update(changes)
    return d


def errs(doc, rel=REL):
    return V.validate(doc, rel)


def assert_has_path(error_list, prefix):
    assert any(e.startswith(prefix) for e in error_list), error_list


# ---------------------------------------------------------------------------------------------
# Example cases and the schema file itself
# ---------------------------------------------------------------------------------------------

@pytest.mark.parametrize("rel", EXAMPLES)
def test_example_cases_validate_clean(rel):
    data = yaml.safe_load((ROOT / rel).read_text(encoding="utf-8"))
    assert V.validate(data, rel) == []


def test_base_document_is_valid():
    assert errs(BASE) == []


def test_schema_uses_only_supported_keywords():
    V._check_schema(SCHEMA, "$schema")  # raises ValueError on any unsupported keyword
    assert V.schema_errors(BASE, SCHEMA) == []


def test_unsupported_keyword_raises():
    with pytest.raises(ValueError):
        V.schema_errors({}, {"type": "object", "minProperties": 1})


def test_unknown_top_level_key_rejected():
    d = mk([{"sql": "SELECT 1"}], bogus=1)
    e = errs(d)
    assert e, "expected an error for unknown top-level key"
    assert_has_path(e, "$.bogus")


def test_sql_and_parse_in_one_step_rejected():
    d = mk([{"sql": "SELECT 1", "parse": {"sql": "SELECT 1"}}])
    e = errs(d)
    assert e
    assert_has_path(e, "$.steps[0]")


def test_step_matching_no_form_rejected():
    e = errs(mk([{"session": "a"}]))
    assert_has_path(e, "$.steps[0]")


def test_wait_without_pending_async_exact_message():
    assert errs(mk([{"wait": True, "session": "a"}])) == [
        "$.steps[0]: wait on session 'a' without a pending async step"
    ]


@pytest.mark.parametrize("step", [
    {"execute": {"statement": "nope"}},
    {"describe": {"statement": "nope"}},
    {"close": {"statement": "nope"}},
])
def test_unknown_statement_name_rejected(step):
    e = errs(mk([step]))
    assert e
    assert_has_path(e, "$.steps[0]." + next(iter(step)) + ".statement")


def test_parse_then_execute_and_describe_ok():
    steps = [
        {"parse": {"name": "s1", "sql": "SELECT 1"}},
        {"describe": {"statement": "s1"}},
        {"execute": {"statement": "s1"}},
    ]
    assert errs(mk(steps)) == []


def test_unnamed_parse_defaults_to_empty_name():
    steps = [{"parse": {"sql": "SELECT 1"}}, {"execute": {"statement": ""}}]
    assert errs(mk(steps)) == []


def test_execute_after_close_rejected():
    steps = [
        {"parse": {"name": "s1", "sql": "SELECT 1"}},
        {"close": {"statement": "s1"}},
        {"execute": {"statement": "s1"}},
    ]
    assert_has_path(errs(mk(steps)), "$.steps[2].execute.statement")


def test_reconnect_forgets_prepared_statements():
    steps = [
        {"parse": {"name": "s1", "sql": "SELECT 1"}},
        {"reconnect": True},
        {"execute": {"statement": "s1"}},
    ]
    assert_has_path(errs(mk(steps)), "$.steps[2].execute.statement")


def test_restart_forgets_prepared_statements():
    steps = [
        {"parse": {"name": "s1", "sql": "SELECT 1"}},
        {"restart": "clean"},
        {"execute": {"statement": "s1"}},
    ]
    e = errs(mk(steps, required_capabilities=["server-restart"]))
    assert_has_path(e, "$.steps[2].execute.statement")


def test_exec_params_uses_unnamed_statement():
    steps = [{"exec_params": {"sql": "SELECT $1", "params": ["1"], "param_types": [25]}},
             {"execute": {"statement": ""}}]
    assert errs(mk(steps)) == []


def test_async_never_waited_rejected():
    e = errs(mk([{"sql": "SELECT 1", "async": True}]))
    assert_has_path(e, "$.steps[0]")
    assert any("never waited for" in x for x in e)


def test_busy_session_rejected():
    steps = [
        {"sql": "SELECT 1", "async": True},
        {"sql": "SELECT 2"},
        {"wait": True, "session": "a"},
    ]
    e = errs(mk(steps))
    assert_has_path(e, "$.steps[1]")


def test_async_wait_pair_valid_across_sessions():
    steps = [
        {"session": "a", "sql": "SELECT 1", "async": True, "expect_blocked": True},
        {"session": "b", "sql": "SELECT 2"},
        {"session": "a", "wait": True},
    ]
    assert errs(mk(steps, sessions=["a", "b"])) == []


def test_second_wait_rejected():
    steps = [
        {"session": "a", "sql": "SELECT 1", "async": True},
        {"session": "a", "wait": True},
        {"session": "a", "wait": True},
    ]
    e = errs(mk(steps))
    assert_has_path(e, "$.steps[2]")


def test_step_session_not_declared_rejected():
    e = errs(mk([{"session": "b", "sql": "SELECT 1"}]))
    assert_has_path(e, "$.steps[0].session")


def test_sqlstate_requires_expect_error():
    assert_has_path(errs(mk([{"sql": "SELECT 1", "sqlstate": "23505"}])), "$.steps[0].sqlstate")
    assert errs(mk([{"sql": "SELECT 1", "sqlstate": "23505", "expect": "error"}])) == []


def test_expect_blocked_requires_async():
    assert_has_path(errs(mk([{"sql": "SELECT 1", "expect_blocked": True}])),
                    "$.steps[0].expect_blocked")


def test_param_formats_length_checked():
    bad = [{"execute": {"statement": "", "params": ["a", "b"],
                        "param_formats": ["text", "text", "text"]}}]
    steps = [{"parse": {"sql": "SELECT $1, $2"}}] + bad
    assert_has_path(errs(mk(steps)), "$.steps[1].execute.param_formats")


def test_param_formats_single_entry_allowed():
    steps = [{"exec_params": {"sql": "SELECT $1, $2", "params": ["a", "b"],
                              "param_formats": ["text"]}}]
    assert errs(mk(steps)) == []


@pytest.mark.parametrize("value, ok", [("abcd", True), ("", True), (None, True),
                                       ("abc", False), ("ABCD", False), ("zz", False)])
def test_binary_params_must_be_lowercase_even_hex(value, ok):
    step = {"exec_params": {"sql": "SELECT $1", "params": [value],
                            "param_formats": ["binary"]}}
    e = errs(mk([step]))
    if ok:
        assert e == []
    else:
        assert_has_path(e, "$.steps[0].exec_params.params[0]")


def test_restart_requires_server_restart_capability():
    assert_has_path(errs(mk([{"restart": "clean"}])), "$.steps[0]")
    assert errs(mk([{"restart": "clean"}], required_capabilities=["server-restart"])) == []


def test_unknown_normalizer_rejected():
    assert_has_path(errs(mk([{"sql": "SELECT 1", "normalize": ["nope"]}])),
                    "$.steps[0].normalize[0]")
    assert errs(mk([{"sql": "SELECT 1", "normalize": ["timestamps"]}])) == []


def test_id_must_match_family():
    assert_has_path(errs(mk([{"sql": "SELECT 1"}], id="other.x")), "$.id")


def test_family_must_match_directory():
    assert_has_path(errs(BASE, "tests/cases/txn/x.yaml"), "$.family")


def test_sql_must_be_string_semantic_direct():
    d = mk([{"sql": True}])
    e = V.semantic_errors(d, REL)
    assert e == ["$.steps[0].sql: sql must be a string; use a block scalar (|)"]


def test_sql_must_be_string_schema_message():
    e = errs(mk([{"sql": True}]))
    assert any("block scalar" in x for x in e), e


def test_integer_rejects_bool_and_accepts_int():
    assert V.schema_errors(True, {"type": "integer"}) != []
    assert V.schema_errors(1, {"type": "integer"}) == []
    assert V.schema_errors(False, {"type": "number"}) != []
    assert V.schema_errors(1.5, {"type": "number"}) == []


def test_enum_const_do_not_confuse_bool_and_int():
    assert V.schema_errors(1, {"enum": [True]}) != []
    assert V.schema_errors(1, {"const": True}) != []


def test_ref_pointer_unescaping():
    schema = {
        "$defs": {"a/b": {"type": "string"}, "c~d": {"type": "integer"}},
        "properties": {"x": {"$ref": "#/$defs/a~1b"}, "y": {"$ref": "#/$defs/c~0d"}},
    }
    e = V.schema_errors({"x": 1, "y": 2}, schema)
    assert len(e) == 1 and e[0].startswith("$.x")


def test_schema_pattern_and_lengths():
    assert_has_path(errs(mk([{"sql": "SELECT 1"}], id="Smoke.x")), "$.id")
    assert_has_path(errs(mk([{"sql": "SELECT 1"}], title="")), "$.title")
    assert_has_path(errs(mk([{"sql": "SELECT 1"}], feature_ids=["a", "a"])), "$.feature_ids")


def test_oneop_reports_only_matching_branch():
    e = errs(mk([{"sql": 5}]))
    assert e and all(x.startswith("$.steps[0]") for x in e)
    assert_has_path(e, "$.steps[0].sql")


def test_not_const_excludes_setup_session_name():
    assert_has_path(errs(mk([{"session": "setup", "sql": "SELECT 1"}], sessions=["setup"])),
                    "$.sessions[0]")
