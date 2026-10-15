"""HAR-27: observation and comparator mutants (ORACLE-13).

Each observation mutant must make a concrete structural change to a suitable fixture, and must
not touch the input transcript. Comparator mutants must pre-mutate their inputs (never the
real comparator) and the kill relation must hold against the real comparator.
"""

from __future__ import annotations

import copy

import pytest

from pgarbor_harness.cases.model import Case
from pgarbor_harness.compare import typed
from pgarbor_harness.model import (
    ColumnDesc,
    ErrorInfo,
    Notice,
    Outcome,
    StatementResult,
    StepObservation,
    Transcript,
)
from pgarbor_harness.selftest import mutants


def _case() -> Case:
    return Case.from_dict({
        "id": "smoke.har27-fixture",
        "title": "HAR-27 fixture",
        "family": "smoke",
        "feature_ids": ["F-TEST"],
        "steps": [
            {"sql": "CREATE TABLE t (id int, name varchar(10))"},
            {"sql": "SELECT id, name FROM t ORDER BY id"},
            {"sql": "SELECT name FROM t"},
            {"sql": "SELECT 1/0"},
            {"sql": "INSERT INTO t VALUES (1, 'a')"},
            {"sql": "BEGIN"},
        ],
    })


def _transcript(endpoint: str = "reference") -> Transcript:
    id_col = ColumnDesc("id", 23, -1, 0, 16400, 1)
    name_col = ColumnDesc("name", 1043, 14, 0, 16400, 2)
    steps = [
        StepObservation(index=0, session="a", kind="sql", txn_status="I", results=[
            StatementResult(kind="command", command_tag="CREATE TABLE"),
        ]),
        StepObservation(index=1, session="a", kind="sql", txn_status="I", results=[
            StatementResult(kind="rows", columns=[id_col, name_col], command_tag="SELECT 4",
                            rows=[["1", "a"], ["2", None], ["3", ""], ["3", ""]]),
        ]),
        StepObservation(index=2, session="a", kind="sql", txn_status="I", results=[
            StatementResult(kind="rows", columns=[name_col], command_tag="SELECT 3",
                            rows=[["b"], ["a"], ["a"]]),
        ]),
        StepObservation(index=3, session="a", kind="sql", txn_status="I", results=[
            StatementResult(kind="error",
                            error=ErrorInfo(severity="ERROR", sqlstate="22012",
                                            message="division by zero")),
        ]),
        StepObservation(index=4, session="a", kind="sql", txn_status="I",
                        notices=[Notice("NOTICE", "00000", "note")], results=[
                            StatementResult(kind="command", command_tag="INSERT 0 1"),
                        ]),
        StepObservation(index=5, session="a", kind="sql", txn_status="T", results=[
            StatementResult(kind="command", command_tag="BEGIN"),
        ]),
    ]
    return Transcript(case_id="smoke.har27-fixture", endpoint=endpoint, setup=[], steps=steps)


def _mutate(name: str, t: Transcript | None = None, case: Case | None = None) -> Transcript:
    out = mutants.observation_mutants()[name](t or _transcript(), case)
    assert out is not None, f"{name} not applicable to the fixture"
    return out


def test_mutant_names_match_contract():
    assert set(mutants.observation_mutants()) == set(mutants.EXPECTED_OUTCOME)
    assert set(mutants.COMPARATOR_KILLS) == {
        "ignore_type_oids", "ignore_nulls", "always_multiset", "ignore_sqlstate",
        "ignore_command_tags",
    }
    assert len(mutants.EXPECTED_OUTCOME) == 18
    assert mutants.EXPECTED_OUTCOME["reorder_unordered"] is Outcome.PASS


@pytest.mark.parametrize("name", sorted(mutants.EXPECTED_OUTCOME))
def test_observation_mutant_applies_without_touching_input(name):
    case = _case()
    original = _transcript()
    pristine = copy.deepcopy(original)
    out = mutants.observation_mutants()[name](original, case)
    assert out is not None, f"{name} not applicable to the fixture"
    assert out is not original
    assert original == pristine, f"{name} mutated its input"


def test_change_value_appends_x_to_first_text_value():
    out = _mutate("change_value")
    assert out.steps[1].results[0].rows[0][0] == "1x"
    assert out.steps[1].results[0].rows[0][1] == "a"


def test_null_to_empty_replaces_first_null():
    out = _mutate("null_to_empty")
    assert out.steps[1].results[0].rows[1][1] == ""


def test_empty_to_null_replaces_first_empty_string():
    out = _mutate("empty_to_null")
    assert out.steps[1].results[0].rows[2][1] is None
    assert out.steps[1].results[0].rows[3][1] == ""


def test_change_type_oid_maps_23_to_20():
    out = _mutate("change_type_oid")
    assert out.steps[1].results[0].columns[0].type_oid == 20
    assert out.steps[1].results[0].columns[1].type_oid == 1043


def test_change_typmod_increments_first_constrained_column():
    out = _mutate("change_typmod")
    cols = out.steps[1].results[0].columns
    assert cols[0].typmod == -1
    assert cols[1].typmod == 15


def test_change_sqlstate_on_first_error():
    out = _mutate("change_sqlstate")
    assert out.steps[3].results[0].error.sqlstate == "42000"


def test_change_sqlstate_avoids_42000_when_already_42000():
    t = _transcript()
    t.steps[3].results[0].error.sqlstate = "42000"
    out = mutants.observation_mutants()["change_sqlstate"](t, None)
    assert out.steps[3].results[0].error.sqlstate == "42601"


def test_swallow_error_turns_error_into_accepted_command():
    out = _mutate("swallow_error")
    res = out.steps[3].results[0]
    assert res.kind == "command"
    assert res.command_tag == "SELECT 0"
    assert res.error is None


def test_unsupported_replaces_first_non_error_result():
    out = _mutate("unsupported")
    res = out.steps[0].results[0]
    assert res.kind == "error"
    assert res.error is not None
    assert res.error.sqlstate == "0A000"
    assert res.error.message == "not implemented"


def test_drop_duplicate_row_keeps_one_copy():
    out = _mutate("drop_duplicate_row")
    rows = out.steps[1].results[0].rows
    assert rows.count(["3", ""]) == 1
    assert len(rows) == 3


def test_drop_last_row_removes_last_row():
    out = _mutate("drop_last_row")
    assert out.steps[1].results[0].rows == [["1", "a"], ["2", None], ["3", ""]]


def test_reorder_ordered_reverses_order_by_result():
    out = _mutate("reorder_ordered", case=_case())
    assert out.steps[1].results[0].rows == [["3", ""], ["3", ""], ["2", None], ["1", "a"]]
    assert out.steps[2].results[0].rows == [["b"], ["a"], ["a"]]


def test_reorder_ordered_needs_the_case():
    assert mutants.observation_mutants()["reorder_ordered"](_transcript(), None) is None


def test_reorder_unordered_reverses_multiset_result():
    out = _mutate("reorder_unordered", case=_case())
    assert out.steps[2].results[0].rows == [["a"], ["a"], ["b"]]
    assert out.steps[1].results[0].rows[0] == ["1", "a"]


def test_change_command_tag_increments_insert_count():
    out = _mutate("change_command_tag")
    assert out.steps[4].results[0].command_tag == "INSERT 0 2"


def test_change_command_tag_appends_when_no_insert_tag():
    t = _transcript()
    t.steps[4].results[0].command_tag = "BEGIN"
    out = mutants.observation_mutants()["change_command_tag"](t, None)
    assert out.steps[0].results[0].command_tag == "CREATE TABLE 0"


def test_change_txn_status_flips_transaction_block_to_idle():
    out = _mutate("change_txn_status")
    assert out.steps[5].txn_status == "I"


def test_change_txn_status_flips_idle_to_transaction_when_no_t():
    t = _transcript()
    t.steps[5].txn_status = "I"
    out = mutants.observation_mutants()["change_txn_status"](t, None)
    assert out.steps[0].txn_status == "T"


def test_drop_notice_removes_first_notice():
    out = _mutate("drop_notice")
    assert out.steps[4].notices == []


def test_crash_midway_disconnects_last_step():
    out = _mutate("crash_midway")
    assert out.steps[-1].connection_lost is True
    assert out.steps[-1].results == []


def test_hang_midway_times_out_last_step():
    out = _mutate("hang_midway")
    assert out.steps[-1].timed_out is True
    assert out.steps[-1].results == []


def test_corrupt_replaces_first_result_with_xx001():
    out = _mutate("corrupt")
    res = out.steps[0].results[0]
    assert res.kind == "error"
    assert res.error.sqlstate == "XX001"
    assert res.error.message == "invalid page"


def test_observation_mutants_not_applicable_to_empty_transcript():
    empty = Transcript(case_id="x", endpoint="target", setup=[], steps=[])
    for name in ("crash_midway", "hang_midway", "change_value", "corrupt", "drop_notice"):
        assert mutants.observation_mutants()[name](empty, None) is None, name


def test_comparator_mutant_rejects_unknown_name():
    with pytest.raises(KeyError):
        mutants.comparator_mutant("no_such_mutant")


def test_comparator_pre_mutation_ignore_type_oids(monkeypatch):
    seen = {}

    def fake(case, ref, tgt):
        seen["args"] = (case, ref, tgt)
        return []

    monkeypatch.setattr(typed, "compare_transcripts", fake)
    case, ref = _case(), _transcript()
    tgt = _transcript("target")
    before = (copy.deepcopy(ref), copy.deepcopy(tgt))
    mutants.comparator_mutant("ignore_type_oids")(case, ref, tgt)
    _, r2, t2 = seen["args"]
    assert all(c.type_oid == 0 for c in r2.steps[1].results[0].columns)
    assert all(c.type_oid == 0 for c in t2.steps[1].results[0].columns)
    assert (ref, tgt) == before


def test_comparator_pre_mutation_always_multiset(monkeypatch):
    seen = {}

    def fake(c, r, t):
        seen["case"] = c
        return []

    monkeypatch.setattr(typed, "compare_transcripts", fake)
    case = _case()
    mutants.comparator_mutant("always_multiset")(case, _transcript(), _transcript("target"))
    assert all(s.order == "multiset" for s in seen["case"].steps)
    assert any(s.order == "auto" for s in case.steps)


@pytest.mark.parametrize("name", sorted(mutants.COMPARATOR_KILLS))
def test_comparator_kill_relation_against_real_comparator(name):
    case = _case()
    ref = _transcript()
    weak = mutants.comparator_mutant(name)
    for obs_name in mutants.COMPARATOR_KILLS[name]:
        tgt = _mutate(obs_name, case=case)
        assert typed.compare_transcripts(case, ref, tgt), f"real comparator misses {obs_name}"
        assert weak(case, ref, tgt) == [], f"{name} still detects {obs_name}"


def test_comparator_ignore_nulls_reports_no_diff_for_null_to_empty():
    case = _case()
    ref = _transcript()
    tgt = _mutate("null_to_empty")
    assert mutants.comparator_mutant("ignore_nulls")(case, ref, tgt) == []


def test_reorder_unordered_negative_control_passes_real_comparator():
    case = _case()
    ref = _transcript()
    tgt = _mutate("reorder_unordered", case=case)
    assert typed.compare_transcripts(case, ref, tgt) == []
