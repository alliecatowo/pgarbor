"""Unusable fixtures and unverified public writes must never publish references."""
from dataclasses import replace
import pytest
from pgarbor_harness.oracle import recorder
from pgarbor_harness.model import ErrorInfo, ReferenceIdentity, StatementResult, StepObservation, Transcript
from pgarbor_harness.cases.model import Case


def case():
    return Case.from_dict(dict(id="probe.fixture", title="fixture", family="probe", feature_ids=[],
                               setup="CREAT TABLE t(a int)",
                               steps=[dict(sql="select * from t", expect="error", sqlstate="42P01")]))


def error(state):
    return StatementResult("error", error=ErrorInfo("ERROR", state, "probe"))


@pytest.mark.parametrize("setup", [[], [StepObservation(0, "setup", "sql")],
                                    [StepObservation(0, "setup", "sql", results=[error("42601")])]])
def test_failed_or_missing_setup_never_publishes(tmp_path, monkeypatch, setup):
    c = case()
    tr = Transcript(c.id, "reference", setup=setup,
                    steps=[StepObservation(0, "a", "sql", results=[error("42P01")])])
    monkeypatch.setattr(recorder, "execute_case", lambda *a, **k: tr)
    result = recorder._record_verified_case(None, c, ReferenceIdentity("image", "version", "fp"), tmp_path)
    assert result.status is recorder.RecordStatus.ORACLE_ERROR
    assert list(tmp_path.rglob("*.json")) == []


def test_intended_step_error_with_valid_setup_is_recordable(tmp_path, monkeypatch):
    c = case()
    tr = Transcript(c.id, "reference", setup=[StepObservation(0, "setup", "sql", results=[
        StatementResult("command", command_tag="CREATE TABLE")])],
        steps=[StepObservation(0, "a", "sql", results=[error("42P01")])])
    monkeypatch.setattr(recorder, "execute_case", lambda *a, **k: tr)
    result = recorder._record_verified_case(None, c, ReferenceIdentity("image", "version", "fp"), tmp_path)
    assert result.status is recorder.RecordStatus.WRITTEN


def test_public_single_case_requires_review_before_access(tmp_path, monkeypatch):
    monkeypatch.delenv(recorder.REVIEW_ENV, raising=False)
    with pytest.raises(PermissionError):
        recorder.record_case(None, case(), ReferenceIdentity("image", "version", "fp"), tmp_path)
    assert list(tmp_path.iterdir()) == []


def test_public_single_case_rejects_forged_identity(tmp_path, monkeypatch):
    monkeypatch.setenv(recorder.REVIEW_ENV, "1")
    class Pool:
        container = object()
        def start(self): pass
    monkeypatch.setattr(recorder, "verify_live", lambda c: (
        {"image_digest": "verified", "server_version": "18.6"}, "fp"))
    with pytest.raises(ValueError, match="verified live oracle"):
        recorder.record_case(Pool(), case(), ReferenceIdentity("forged", "18.6", "fp"), tmp_path)
    assert list(tmp_path.iterdir()) == []
