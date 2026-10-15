"""Lease HAR-04 tests: intent checking and the trusted recorder (no live oracle needed).

The recorder's execute_case and verify_live are replaced with fakes, so these tests cover the
recorder's own logic: intent rules, nondeterminism, write/unchanged/would-change statuses,
quarantine, the review gate and the dry run.
"""

from __future__ import annotations

import pytest

from pgarbor_harness.cases.model import Case
from pgarbor_harness.model import (
    ErrorInfo,
    Notice,
    ReferenceIdentity,
    StatementResult,
    StepObservation,
    Transcript,
    expected_path,
)
from pgarbor_harness.oracle import recorder
from pgarbor_harness.oracle.recorder import RecordStatus, check_intent, _record_verified_case as record_case, record_corpus


def _case(steps, cid="smoke.one", quarantine=None):
    d = {"id": cid, "title": "t", "family": cid.split(".", 1)[0], "feature_ids": [],
         "steps": steps}
    if quarantine is not None:
        d["quarantine"] = quarantine
    return Case.from_dict(d, path=f"tests/cases/smoke/{cid}.yaml", sha256="abc123")


def _res(kind, sqlstate=None):
    error = ErrorInfo(severity="ERROR", sqlstate=sqlstate, message="boom") if kind == "error" else None
    tag = "INSERT 0 1" if kind == "command" else None
    return StatementResult(kind=kind, error=error, command_tag=tag)


def _obs(index, session="a", kind="sql", results=(), notices=(), blocked=None, **kw):
    return StepObservation(index=index, session=session, kind=kind, results=list(results),
                           notices=list(notices), blocked_observed=blocked, **kw)


def _tr(*steps, cid="smoke.one"):
    return Transcript(cid, "reference", [], list(steps))


def _identity():
    return ReferenceIdentity("sha256:" + "0" * 64, "18.6 (Debian 18.6-1.pgdg13+2)", "f" * 64)


# ---------------------------------------------------------------------------------------------
# check_intent
# ---------------------------------------------------------------------------------------------


def test_any_is_never_judged():
    case = _case([{"sql": "select 1"}])
    assert check_intent(case, _tr(_obs(0, results=[_res("error", "42601")]))) == []


def test_ok_flags_error_result():
    case = _case([{"sql": "x", "expect": "ok"}])
    violations = check_intent(case, _tr(_obs(0, results=[_res("error", "23505")])))
    assert len(violations) == 1
    assert violations[0].startswith("step 0: expect=ok")


def test_ok_accepts_rows_and_command():
    case = _case([{"sql": "x", "expect": "ok"}])
    assert check_intent(case, _tr(_obs(0, results=[_res("rows")]))) == []
    assert check_intent(case, _tr(_obs(0, results=[_res("command")]))) == []


def test_error_without_error_result_reports_kind():
    case = _case([{"sql": "insert", "expect": "error", "sqlstate": "23505"}])
    violations = check_intent(case, _tr(_obs(0, results=[_res("command")])))
    assert violations == [
        "step 0: expect=error sqlstate=23505 but reference returned kind=command"]


def test_error_sqlstate_must_match():
    case = _case([{"sql": "insert", "expect": "error", "sqlstate": "23505"}])
    assert check_intent(case, _tr(_obs(0, results=[_res("error", "23505")]))) == []
    violations = check_intent(case, _tr(_obs(0, results=[_res("error", "42P01")])))
    assert len(violations) == 1
    assert "sqlstate=23505" in violations[0] and "42P01" in violations[0]


def test_error_without_sqlstate_accepts_any_error():
    case = _case([{"sql": "x", "expect": "error"}])
    assert check_intent(case, _tr(_obs(0, results=[_res("error", "22012")]))) == []


def test_rows_requires_last_result_to_be_rows():
    case = _case([{"sql": "x", "expect": "rows"}])
    assert check_intent(case, _tr(_obs(0, results=[_res("command")]))) != []
    assert check_intent(case, _tr(_obs(0, results=[_res("rows"), _res("command")]))) != []
    assert check_intent(case, _tr(_obs(0, results=[_res("command"), _res("rows")]))) == []


def test_notice_requires_a_notice():
    case = _case([{"sql": "x", "expect": "notice"}])
    assert len(check_intent(case, _tr(_obs(0, results=[_res("command")])))) == 1
    notice = Notice(severity="NOTICE", sqlstate="00000", message="hi")
    assert check_intent(case, _tr(_obs(0, results=[_res("command")], notices=[notice]))) == []


def test_async_step_is_judged_on_matching_wait():
    case = _case([
        {"sql": "update t set v = 1", "async": True, "session": "b", "expect": "rows",
         "expect_blocked": True},
        {"wait": True, "session": "b"},
    ])
    launch = _obs(0, session="b", kind="sql", blocked=True)
    ok_wait = _obs(1, session="b", kind="wait", results=[_res("rows")])
    assert check_intent(case, _tr(launch, ok_wait)) == []

    bad_wait = _obs(1, session="b", kind="wait", results=[_res("command")])
    violations = check_intent(case, _tr(launch, bad_wait))
    assert violations == ["step 0: expect=rows but reference returned kind=command"]


def test_async_step_without_wait_is_a_violation():
    case = _case([{"sql": "x", "async": True, "session": "b", "expect": "ok"}])
    violations = check_intent(case, _tr(_obs(0, session="b", blocked=False)))
    assert len(violations) == 1 and "no matching wait" in violations[0]


def test_expect_blocked_equals_blocked_observed():
    case = _case([
        {"sql": "lock", "async": True, "session": "b", "expect_blocked": True},
        {"wait": True, "session": "b"},
    ])
    assert check_intent(case, _tr(_obs(0, session="b", blocked=True),
                                  _obs(1, session="b", kind="wait", results=[_res("command")]))) == []
    violations = check_intent(case, _tr(_obs(0, session="b", blocked=False),
                                        _obs(1, session="b", kind="wait", results=[_res("command")])))
    assert violations == ["step 0: expect_blocked=true but reference blocked_observed=false"]


def test_faulted_observation_is_not_judged_here():
    case = _case([{"sql": "x", "expect": "ok"}])
    faulted = _obs(0, results=[_res("error", "23505")], harness_error="boom")
    assert check_intent(case, _tr(faulted)) == []


# ---------------------------------------------------------------------------------------------
# record_case / record_corpus with a fake executor
# ---------------------------------------------------------------------------------------------


def _fake_execute(monkeypatch, per_attempt):
    """per_attempt(attempt) -> list of StepObservation for the single step."""
    calls = []

    def fake(case, target, endpoint_label="target", attempt=1, **kw):
        calls.append(attempt)
        return Transcript(case.id, endpoint_label, [], per_attempt(attempt))

    monkeypatch.setattr(recorder, "execute_case", fake)
    return calls


def test_nondeterministic_writes_nothing(tmp_path, monkeypatch):
    case = _case([{"sql": "select 1"}])
    _fake_execute(monkeypatch, lambda a: [_obs(0, results=[_res("rows" if a == 1 else "command")])])
    result = record_case(None, case, _identity(), tmp_path)
    assert result.status is RecordStatus.NONDETERMINISTIC
    assert not expected_path(tmp_path, case.id).exists()


def test_intent_mismatch_writes_nothing(tmp_path, monkeypatch):
    case = _case([{"sql": "insert", "expect": "error", "sqlstate": "23505"}])
    _fake_execute(monkeypatch, lambda a: [_obs(0, results=[_res("command")])])
    result = record_case(None, case, _identity(), tmp_path)
    assert result.status is RecordStatus.INTENT_MISMATCH
    assert not expected_path(tmp_path, case.id).exists()


def test_harness_error_is_oracle_error(tmp_path, monkeypatch):
    case = _case([{"sql": "select 1"}])
    _fake_execute(monkeypatch, lambda a: [_obs(0, results=[_res("rows")], harness_error="boom")])
    result = record_case(None, case, _identity(), tmp_path)
    assert result.status is RecordStatus.ORACLE_ERROR
    assert not expected_path(tmp_path, case.id).exists()


def test_write_unchanged_and_would_change(tmp_path, monkeypatch):
    case = _case([{"sql": "select 1"}])
    path = expected_path(tmp_path, case.id)

    _fake_execute(monkeypatch, lambda a: [_obs(0, results=[_res("rows")])])
    first = record_case(None, case, _identity(), tmp_path)
    assert first.status is RecordStatus.WRITTEN
    assert path.exists()
    original = path.read_bytes()

    second = record_case(None, case, _identity(), tmp_path)
    assert second.status is RecordStatus.UNCHANGED

    notice = Notice(severity="NOTICE", sqlstate="00000", message="changed")
    _fake_execute(monkeypatch, lambda a: [_obs(0, results=[_res("rows")], notices=[notice])])
    changed = record_case(None, case, _identity(), tmp_path)
    assert changed.status is RecordStatus.WOULD_CHANGE
    assert changed.messages
    assert path.read_bytes() == original

    overwritten = record_case(None, case, _identity(), tmp_path, allow_overwrite=True)
    assert overwritten.status is RecordStatus.WRITTEN
    assert path.read_bytes() != original


def test_quarantined_case_is_skipped(tmp_path, monkeypatch):
    case = _case([{"sql": "select 1"}], quarantine={"reason": "known gap"})

    def boom(*args, **kwargs):
        raise AssertionError("quarantined case must not run")

    monkeypatch.setattr(recorder, "execute_case", boom)
    result = record_case(None, case, _identity(), tmp_path)
    assert result.status is RecordStatus.SKIPPED_QUARANTINED
    assert not expected_path(tmp_path, case.id).exists()


def test_record_corpus_refuses_without_review_env(tmp_path, monkeypatch):
    monkeypatch.delenv(recorder.REVIEW_ENV, raising=False)
    with pytest.raises(PermissionError):
        record_corpus(object(), [], tmp_path)


def test_dry_run_reports_without_writing(tmp_path, monkeypatch):
    monkeypatch.delenv(recorder.REVIEW_ENV, raising=False)
    case = _case([{"sql": "select 1"}])
    _fake_execute(monkeypatch, lambda a: [_obs(0, results=[_res("rows")])])
    monkeypatch.setattr(recorder, "verify_live",
                        lambda container: ({"image_digest": "sha256:" + "0" * 64,
                                            "server_version": "18.6"}, "f" * 64))

    class FakePool:
        container = None

        def start(self):
            pass

    results = record_corpus(FakePool(), [case], tmp_path, dry_run=True, jobs=1)
    assert [r.status for r in results] == [RecordStatus.WOULD_CHANGE]
    assert not expected_path(tmp_path, case.id).exists()
