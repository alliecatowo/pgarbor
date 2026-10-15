"""The socket-free transport must retain strict comparison and execution accounting."""
from pathlib import Path
from types import SimpleNamespace
import subprocess
from pgarbor_harness.cases.model import Case, Step
from pgarbor_harness.model import ColumnDesc, StatementResult, StepObservation, Transcript
from pgarbor_harness.runner import embedded


def fixture(tmp_path, monkeypatch):
    case = Case(id="smoke.value", title="value", family="smoke", feature_ids=[],
                steps=[Step(0, "sql", "a", {"sql": "SELECT 20000::oid"})], sha256="case-hash")
    ref = Transcript(case_id=case.id, endpoint="reference", steps=[
        StepObservation(0, "a", "sql", results=[StatementResult("rows", [ColumnDesc("oid", 26)],
                         [["20000"]], "SELECT 1")], txn_status="I")])
    file = embedded.expected_path(tmp_path / "tests/expected", case.id)
    file.parent.mkdir(parents=True); file.write_text("{}")
    monkeypatch.setattr(embedded.ExpectedFile, "load", lambda p: SimpleNamespace(
        case_sha256=case.sha256, transcript=ref))
    out = tmp_path / "out"
    (out / "transcripts").mkdir(parents=True); (out / "errors").mkdir()
    return case, ref, out


def test_wrong_oid_literal_cannot_pass(tmp_path, monkeypatch):
    import json
    case, ref, out = fixture(tmp_path, monkeypatch)
    observed = ref.to_json(); observed["endpoint"] = "target"
    observed["steps"][0]["results"][0]["rows"] = [["30000"]]
    monkeypatch.setattr(embedded.subprocess, "run", lambda *a, **kw:
        SimpleNamespace(returncode=0, stdout=json.dumps({"transcript": observed}), stderr=""))
    result = embedded.run_case(case, Path("binary"), tmp_path, out, 1)
    assert result["outcome"] == "MISMATCH" and result["executed"]


def test_async_is_blocked_before_process_launch(tmp_path, monkeypatch):
    case, _, out = fixture(tmp_path, monkeypatch); case.steps[0].is_async = True
    monkeypatch.setattr(embedded.subprocess, "run", lambda *a, **kw: (_ for _ in ()).throw(AssertionError("launched")))
    result = embedded.run_case(case, Path("binary"), tmp_path, out, 1)
    assert result["outcome"] == "BLOCKED" and not result["executed"]


def test_timeout_never_counts_as_pass(tmp_path, monkeypatch):
    case, _, out = fixture(tmp_path, monkeypatch)
    def timeout(*a, **kw):
        raise subprocess.TimeoutExpired(a[0], kw["timeout"])
    monkeypatch.setattr(embedded.subprocess, "run", timeout)
    result = embedded.run_case(case, Path("binary"), tmp_path, out, 1)
    assert result["outcome"] == "HANG" and result["executed"]


def test_process_abort_retains_stderr(tmp_path, monkeypatch):
    case, _, out = fixture(tmp_path, monkeypatch)
    monkeypatch.setattr(embedded.subprocess, "run", lambda *a, **kw:
        SimpleNamespace(returncode=-6, stdout="", stderr="stack overflow"))
    result = embedded.run_case(case, Path("binary"), tmp_path, out, 1)
    assert result["outcome"] == "CRASH"
    assert (out / "errors" / (case.id + ".stderr")).read_text() == "stack overflow"
