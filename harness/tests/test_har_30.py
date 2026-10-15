"""Tests for HAR-30: the differential driver (runner.differential).

The executor is replaced by a fake through monkeypatch on ``runner.executor.execute_case``, so
no PostgreSQL or PGArbor process is needed.
"""

from __future__ import annotations

import json
import time
from pathlib import Path
from typing import Callable, Optional

import pytest

from pgarbor_harness.cases.loader import InvalidCase
from pgarbor_harness.cases.model import Case
from pgarbor_harness.model import (
    ExpectedFile,
    Outcome,
    ReferenceIdentity,
    StatementResult,
    StepObservation,
    Transcript,
    expected_path,
)
from pgarbor_harness.runner import differential, executor

REF = ReferenceIdentity(
    image_digest="sha256:" + "a" * 64,
    server_version="18.6 (test)",
    fingerprint_sha256="f" * 64,
)


class FakeTarget:
    """A target that keeps no state; start/stop are counted."""

    kind = "dsn"

    def __init__(self, caps: frozenset[str] = frozenset({"*"})) -> None:
        self.name = "fake"
        self._caps = caps
        self.started = 0
        self.stopped = 0

    def capabilities(self) -> frozenset[str]:
        return self._caps

    def start(self) -> None:
        self.started += 1

    def provision(self, case_id: str, attempt: int = 1) -> None:
        return None

    def release(self, env: object, keep: bool = False) -> None:
        return None

    def restart(self, env: object, mode: str) -> None:
        return None

    def is_alive(self, env: object) -> bool:
        return True

    def server_log_tail(self, env: object, lines: int = 40) -> str:
        return ""

    def stop(self) -> None:
        self.stopped += 1


def make_case(case_id: str = "smoke.one", sha: str = "c" * 64,
              required: Optional[list[str]] = None,
              quarantine: Optional[dict[str, str]] = None) -> Case:
    d = {
        "id": case_id,
        "title": "test case",
        "family": case_id.split(".", 1)[0],
        "feature_ids": ["F-1"],
        "steps": [{"sql": "SELECT 1"}],
    }
    if required:
        d["required_capabilities"] = required
    family = case_id.split(".", 1)[0]
    case = Case.from_dict(d, path=f"tests/cases/{family}/{case_id}.yaml", sha256=sha)
    case.quarantine = quarantine
    return case


def make_transcript(case_id: str, endpoint: str = "target", txn: str = "I",
                    tag: str = "SELECT 1") -> Transcript:
    step = StepObservation(
        index=0,
        session="a",
        kind="sql",
        results=[StatementResult(kind="command", command_tag=tag)],
        txn_status=txn,
    )
    return Transcript(case_id=case_id, endpoint=endpoint, setup=[], steps=[step])


def write_expected(root: Path, case: Case, txn: str = "I", case_sha: Optional[str] = None,
                   reference: ReferenceIdentity = REF) -> None:
    ExpectedFile(
        case_id=case.id,
        case_sha256=case.sha256 if case_sha is None else case_sha,
        reference=reference,
        transcript=make_transcript(case.id, endpoint="reference", txn=txn),
    ).save(expected_path(root, case.id))


def install_executor(monkeypatch: pytest.MonkeyPatch,
                     fn: Callable[[Case, int], Transcript]) -> list[tuple[str, int]]:
    """Replace execute_case; returns the list of (case id, attempt) calls it received."""
    calls: list[tuple[str, int]] = []

    def fake(case: Case, target: object, endpoint_label: str = "target", attempt: int = 1,
             **kwargs: object) -> Transcript:
        calls.append((case.id, attempt))
        return fn(case, attempt)

    monkeypatch.setattr(executor, "execute_case", fake)
    return calls


def read_jsonl(path: Path) -> list[dict]:
    return [json.loads(line) for line in path.read_text(encoding="utf-8").splitlines()]


def test_missing_reference_is_blocked_and_not_executed(tmp_path: Path,
                                                        monkeypatch: pytest.MonkeyPatch) -> None:
    calls = install_executor(monkeypatch, lambda c, a: make_transcript(c.id))
    case = make_case()
    result = differential.run_case(case, FakeTarget(), tmp_path / "expected",
                                   tmp_path / "out")
    assert result.outcome is Outcome.BLOCKED
    assert result.reason == "reference_missing"
    assert calls == []
    rows = read_jsonl(tmp_path / "out" / "results.jsonl")
    assert [r["outcome"] for r in rows] == ["BLOCKED"]


def test_stale_reference_by_case_sha_is_blocked(tmp_path: Path,
                                                monkeypatch: pytest.MonkeyPatch) -> None:
    calls = install_executor(monkeypatch, lambda c, a: make_transcript(c.id))
    case = make_case(sha="n" * 64)
    write_expected(tmp_path / "expected", case, case_sha="o" * 64)
    result = differential.run_case(case, FakeTarget(), tmp_path / "expected",
                                   tmp_path / "out")
    assert result.outcome is Outcome.BLOCKED
    assert result.reason == "reference_stale"
    assert calls == []


def test_stale_reference_by_identity_is_blocked(tmp_path: Path,
                                                monkeypatch: pytest.MonkeyPatch) -> None:
    calls = install_executor(monkeypatch, lambda c, a: make_transcript(c.id))
    case = make_case()
    write_expected(tmp_path / "expected", case)
    live = ReferenceIdentity(image_digest=REF.image_digest, server_version=REF.server_version,
                             fingerprint_sha256="0" * 64)
    result = differential.run_case(case, FakeTarget(), tmp_path / "expected",
                                   tmp_path / "out", live_reference=live)
    assert result.outcome is Outcome.BLOCKED
    assert result.reason == "reference_stale"
    assert calls == []


def test_missing_capability_is_blocked(tmp_path: Path,
                                       monkeypatch: pytest.MonkeyPatch) -> None:
    calls = install_executor(monkeypatch, lambda c, a: make_transcript(c.id))
    case = make_case(required=["jsonb", "btree"])
    write_expected(tmp_path / "expected", case)
    target = FakeTarget(caps=frozenset({"btree"}))
    result = differential.run_case(case, target, tmp_path / "expected", tmp_path / "out")
    assert result.outcome is Outcome.BLOCKED
    assert result.reason == "capability:jsonb"
    assert calls == []


def test_quarantined_case_is_invalid_and_not_executed(tmp_path: Path,
                                                      monkeypatch: pytest.MonkeyPatch) -> None:
    calls = install_executor(monkeypatch, lambda c, a: make_transcript(c.id))
    case = make_case(quarantine={"reason": "upstream bug"})
    write_expected(tmp_path / "expected", case)
    result = differential.run_case(case, FakeTarget(), tmp_path / "expected",
                                   tmp_path / "out")
    assert result.outcome is Outcome.INVALID_CASE
    assert result.reason == "quarantined"
    assert calls == []


def test_identical_transcript_passes_once(tmp_path: Path,
                                          monkeypatch: pytest.MonkeyPatch) -> None:
    calls = install_executor(monkeypatch, lambda c, a: make_transcript(c.id))
    case = make_case()
    write_expected(tmp_path / "expected", case)
    out = tmp_path / "out"
    result = differential.run_case(case, FakeTarget(), tmp_path / "expected", out)
    assert result.outcome is Outcome.PASS
    assert result.attempts == 1
    assert calls == [("smoke.one", 1)]
    assert result.transcript_path is not None
    assert result.transcript_path.endswith("transcripts/smoke.one.json")
    written = json.loads((out / "transcripts" / "smoke.one.json").read_text(encoding="utf-8"))
    assert written["case_id"] == "smoke.one"
    rows = read_jsonl(out / "results.jsonl")
    assert len(rows) == 1 and rows[0]["outcome"] == "PASS"


def test_two_differing_runs_of_failing_case_are_flaky(tmp_path: Path,
                                                      monkeypatch: pytest.MonkeyPatch) -> None:
    # Expected is idle ("I"); the target reports a different txn status on every run.
    txns = iter(["T", "E"])
    calls = install_executor(monkeypatch,
                             lambda c, a: make_transcript(c.id, txn=next(txns)))
    case = make_case()
    write_expected(tmp_path / "expected", case)
    result = differential.run_case(case, FakeTarget(), tmp_path / "expected",
                                   tmp_path / "out", reruns=1)
    assert result.outcome is Outcome.FLAKY
    assert result.reason == "target_nondeterministic"
    assert result.attempts == 2
    assert result.first_divergence is not None
    assert calls == [("smoke.one", 1), ("smoke.one", 2)]


def test_deterministic_failure_is_rerun_but_not_flaky(tmp_path: Path,
                                                      monkeypatch: pytest.MonkeyPatch) -> None:
    calls = install_executor(monkeypatch, lambda c, a: make_transcript(c.id, txn="T"))
    case = make_case()
    write_expected(tmp_path / "expected", case)
    result = differential.run_case(case, FakeTarget(), tmp_path / "expected",
                                   tmp_path / "out", reruns=1)
    assert result.outcome is Outcome.MISMATCH
    assert result.attempts == 2
    assert calls == [("smoke.one", 1), ("smoke.one", 2)]


def test_no_rerun_when_reruns_is_zero(tmp_path: Path,
                                      monkeypatch: pytest.MonkeyPatch) -> None:
    calls = install_executor(monkeypatch, lambda c, a: make_transcript(c.id, txn="T"))
    case = make_case()
    write_expected(tmp_path / "expected", case)
    result = differential.run_case(case, FakeTarget(), tmp_path / "expected",
                                   tmp_path / "out", reruns=0)
    assert result.outcome is Outcome.MISMATCH
    assert result.attempts == 1
    assert calls == [("smoke.one", 1)]


def test_exception_becomes_harness_error_and_run_continues(tmp_path: Path,
                                                           monkeypatch: pytest.MonkeyPatch
                                                           ) -> None:
    def behave(case: Case, attempt: int) -> Transcript:
        if case.id == "smoke.bad":
            raise RuntimeError("boom")
        return make_transcript(case.id)

    install_executor(monkeypatch, behave)
    bad = make_case("smoke.bad")
    good = make_case("smoke.good")
    exp_root = tmp_path / "expected"
    write_expected(exp_root, bad)
    write_expected(exp_root, good)
    target = FakeTarget()
    results = differential.run_cases([bad, good], target, exp_root, tmp_path / "out", jobs=1)
    by_id = {r.case_id: r for r in results}
    assert by_id["smoke.bad"].outcome is Outcome.HARNESS_ERROR
    assert by_id["smoke.bad"].reason == "exception:RuntimeError"
    assert by_id["smoke.good"].outcome is Outcome.PASS
    assert target.started == 1 and target.stopped == 1


def test_results_sorted_by_case_id_regardless_of_completion(tmp_path: Path,
                                                            monkeypatch: pytest.MonkeyPatch
                                                            ) -> None:
    delays = {"smoke.a": 0.2, "smoke.b": 0.1, "smoke.c": 0.0}

    def behave(case: Case, attempt: int) -> Transcript:
        time.sleep(delays[case.id])
        return make_transcript(case.id)

    install_executor(monkeypatch, behave)
    exp_root = tmp_path / "expected"
    cases = [make_case("smoke.c"), make_case("smoke.a"), make_case("smoke.b")]
    for case in cases:
        write_expected(exp_root, case)
    target = FakeTarget()
    results = differential.run_cases(cases, target, exp_root, tmp_path / "out", jobs=3)
    assert [r.case_id for r in results] == ["smoke.a", "smoke.b", "smoke.c"]
    assert all(r.outcome is Outcome.PASS for r in results)
    rows = read_jsonl(tmp_path / "out" / "results.jsonl")
    assert sorted(r["case_id"] for r in rows) == ["smoke.a", "smoke.b", "smoke.c"]


def test_invalid_results_map_schema_errors(tmp_path: Path) -> None:
    invalid = [
        InvalidCase(path="tests/cases/smoke/bad.yaml", case_id=None,
                    errors=["steps: required"]),
        InvalidCase(path="tests/cases/perf/x.yaml", case_id="perf.x",
                    errors=["id: bad", "title: missing"]),
    ]
    results = differential.invalid_results(invalid)
    by_id = {r.case_id: r for r in results}
    assert set(by_id) == {"tests/cases/smoke/bad.yaml", "perf.x"}
    bad = by_id["tests/cases/smoke/bad.yaml"]
    assert bad.outcome is Outcome.INVALID_CASE
    assert bad.reason == "schema:steps: required"
    assert bad.family == "smoke"
    perf = by_id["perf.x"]
    assert perf.reason == "schema:id: bad"
    assert perf.family == "perf"
