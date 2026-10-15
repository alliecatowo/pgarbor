"""Tests for HAR-26 (ORACLE-10 minimizer). Lease HAR-26.

``test_script_*`` and the sqltext-dependent tests need HAR-08 (sqltext.split_statements).
"""

import pytest

from pgarbor_harness.cases.validate import validate
from pgarbor_harness.minimize.ddmin import (
    NotInterestingError,
    _step_groups,
    ddmin,
    minimize_case,
    minimize_script,
)


def test_ddmin_finds_units_3_and_7():
    result, runs, complete = ddmin(range(20), lambda u: 3 in u and 7 in u)
    assert result == [3, 7]
    assert complete is True
    assert runs >= 1


def test_ddmin_result_is_one_minimal():
    result, _runs, complete = ddmin(range(20), lambda u: 3 in u and 7 in u)
    assert complete is True
    for i in range(len(result)):
        assert not (3 in result[:i] + result[i + 1:] and 7 in result[:i] + result[i + 1:])


def test_ddmin_budget_exhaustion_is_incomplete():
    result, runs, complete = ddmin(range(20), lambda u: 3 in u and 7 in u, max_runs=3)
    assert complete is False
    assert runs <= 3
    # the returned list is still interesting (best found so far)
    assert 3 in result and 7 in result


def test_ddmin_memoizes_each_candidate_once():
    calls: list[tuple[int, ...]] = []

    def pred(u):
        calls.append(tuple(u))
        return 3 in u and 7 in u

    ddmin(range(20), pred)
    assert len(calls) == len(set(calls))


def test_ddmin_rejects_uninteresting_input():
    with pytest.raises(ValueError, match="original input is not interesting"):
        ddmin([1, 2, 3], lambda u: False)


def test_script_minimization_keeps_needed_statement():
    script = "select 1;\nselect 2;\nselect 3;\n"
    result = minimize_script(script, lambda s: "select 2" in s)
    assert result.units_after == 1
    assert "select 2" in result.script
    assert "select 1" not in result.script and "select 3" not in result.script


def test_step_groups_keep_async_with_wait_and_parse_with_execute():
    case = {
        "sessions": ["a", "b"],
        "steps": [
            {"sql": "select 1"},                                  # 0 alone
            {"sql": "select pg_sleep(0)", "async": True},         # 1 async a
            {"sql": "select 2", "session": "b"},                  # 2 alone (other session)
            {"wait": True},                                       # 3 wait a -> pairs with 1
            {"parse": {"name": "s", "sql": "select $1"}},         # 4 parse s
            {"describe": {"statement": "s"}},                     # 5 uses s
            {"execute": {"statement": "s", "params": ["1"]}},     # 6 uses s
            {"sql": "select 3"},                                  # 7 alone
        ],
    }
    groups = _step_groups(case)
    assert [1, 3] in groups
    assert [4, 5, 6] in groups
    assert [0] in groups and [2] in groups and [7] in groups
    flat = sorted(i for g in groups for i in g)
    assert flat == list(range(8))


def test_minimize_case_reduces_to_needed_step_and_keeps_candidates_valid():
    case = {
        "format": "pgarbor-case/1",
        "id": "smoke.demo",
        "title": "demo",
        "family": "smoke",
        "feature_ids": ["select"],
        "steps": [
            {"sql": "SELECT 1"},
            {"sql": "SELECT pg_sleep(0)", "async": True},
            {"wait": True, "session": "a"},
            {"sql": "SELECT 2"},
        ],
    }
    seen: list[dict] = []

    def same_failure(candidate):
        seen.append(candidate)
        return any(s.get("sql") == "SELECT 2" for s in candidate["steps"])

    result = minimize_case(case, same_failure, max_runs=400)
    assert result.complete is True
    assert result.case_dict["steps"] == [{"sql": "SELECT 2"}]
    assert result.case_dict["id"] == "smoke.min-demo"
    assert result.case_dict["provenance"] == {"source": "minimized", "ref": "smoke.demo"}
    # every candidate handed to the failure predicate is valid
    for cand in seen:
        assert validate(cand, "tests/cases/smoke/demo.yaml") == []
    # memoized: each distinct candidate is evaluated once
    keys = [repr(sorted(c.items(), key=lambda kv: kv[0])) for c in seen]
    assert len(keys) == len(set(keys))


def test_minimize_case_rejects_uninteresting_original():
    case = {
        "format": "pgarbor-case/1", "id": "smoke.demo", "title": "demo", "family": "smoke",
        "feature_ids": ["select"], "steps": [{"sql": "SELECT 1"}],
    }
    with pytest.raises(NotInterestingError):
        minimize_case(case, lambda c: False)


def test_minimize_case_budget_exhaustion_is_incomplete():
    case = {
        "format": "pgarbor-case/1", "id": "smoke.demo", "title": "demo", "family": "smoke",
        "feature_ids": ["select"],
        "steps": [{"sql": f"SELECT {i}"} for i in range(10)],
    }
    result = minimize_case(case, lambda c: len(c["steps"]) >= 1, max_runs=2)
    assert result.complete is False
