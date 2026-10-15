"""Lease HAR-22: sqllogictest record/run classification (no PG, no network).

A FakeSession returns canned StepOutput values, so the verdict rules are exercised directly.
"""

from __future__ import annotations

from pathlib import Path

import pytest

from pgarbor_harness.model import ColumnDesc, Endpoint, ErrorInfo, Outcome, StatementResult
from pgarbor_harness.runner.session import StepOutput
from pgarbor_harness.runner.targets import TargetStartError
from pgarbor_harness.sqllogictest import runner as slt_runner
from pgarbor_harness.sqllogictest.parser import SltRecord

ENDPOINT = Endpoint(host="127.0.0.1", port=5432, dbname="slt")


# ---------------------------------------------------------------------------------------------
# canned outputs
# ---------------------------------------------------------------------------------------------


def ok_out() -> StepOutput:
    return StepOutput(results=[StatementResult(kind="command", command_tag="CREATE TABLE")])


def err_out(sqlstate: str, message: str = "boom") -> StepOutput:
    return StepOutput(results=[StatementResult(
        kind="error",
        error=ErrorInfo(severity="ERROR", sqlstate=sqlstate, message=message),
    )])


def rows_out(rows: list[list], ncols: int = 1) -> StepOutput:
    cols = [ColumnDesc(name=f"c{i}", type_oid=25) for i in range(ncols)]
    return StepOutput(results=[StatementResult(
        kind="rows", columns=cols, rows=rows, command_tag=f"SELECT {len(rows)}")])


def timeout_out() -> StepOutput:
    return StepOutput(timed_out=True)


def lost_out() -> StepOutput:
    return StepOutput(connection_lost=True)


class FakeSession:
    """Returns the canned outputs in order; records the SQL it was asked to run."""

    def __init__(self, outputs: list[StepOutput]):
        self.outputs = list(outputs)
        self.sqls: list[str] = []
        self.closed = False

    def connect(self) -> None:
        pass

    def close(self) -> None:
        self.closed = True

    def run_simple(self, sql: str) -> StepOutput:
        self.sqls.append(sql)
        return self.outputs.pop(0)


def stmt(line: int, sql: str = "select 1", ok: bool = True, sqlstate=None,
         conditions=None) -> SltRecord:
    return SltRecord(kind="statement", line=line, sql=sql, expect_ok=ok, sqlstate=sqlstate,
                     conditions=list(conditions or []))


def query(line: int, expected, types: str = "I", sort: str = "nosort",
          conditions=None) -> SltRecord:
    return SltRecord(kind="query", line=line, sql="select 1", types=types, sort=sort,
                     expected=expected, conditions=list(conditions or []))


def run_records(records: list[SltRecord], outputs: list[StepOutput]) -> slt_runner.SltFileResult:
    result = slt_runner.SltFileResult("x.slt", Outcome.PASS)
    session = FakeSession(outputs)
    slt_runner._run_records(records, session, result)
    return slt_runner._finalize(result)


# ---------------------------------------------------------------------------------------------
# run classification
# ---------------------------------------------------------------------------------------------


def test_target_0a000_where_pg_succeeded_is_unsupported():
    res = run_records([stmt(1, ok=True)], [err_out("0A000", "not implemented")])
    assert res.outcome is Outcome.UNSUPPORTED
    assert res.first_failure_line == 1


def test_query_target_0a000_is_unsupported():
    res = run_records([query(4, ["1"])], [err_out("0A000")])
    assert res.outcome is Outcome.UNSUPPORTED
    assert res.first_failure_line == 4


def test_skipped_records_make_passing_file_blocked():
    records = [
        stmt(1, conditions=[("skipif", "pgarbor")]),
        query(3, ["1"]),
    ]
    res = run_records(records, [rows_out([["1"]])])
    assert res.outcome is Outcome.BLOCKED
    assert res.reason == "skipped_records"
    assert res.records_skipped == 1
    assert res.records_passed == 1


def test_onlyif_other_engine_is_skipped_not_run():
    records = [stmt(1, conditions=[("onlyif", "sqlite")]), stmt(2)]
    session = FakeSession([ok_out()])
    result = slt_runner.SltFileResult("x.slt", Outcome.PASS)
    slt_runner._run_records(records, session, result)
    assert session.sqls == ["select 1"]  # only the unconditioned record ran
    assert result.records_skipped == 1


def test_unexpected_acceptance_when_pg_errored():
    res = run_records([stmt(1, ok=False, sqlstate="42P01")], [ok_out()])
    assert res.outcome is Outcome.UNEXPECTED_ACCEPTANCE


def test_sqlstate_mismatch_is_mismatch():
    res = run_records([stmt(1, ok=False, sqlstate="42P01")], [err_out("42703")])
    assert res.outcome is Outcome.MISMATCH


def test_matching_statement_error_passes():
    res = run_records([stmt(1, ok=False, sqlstate="42P01")], [err_out("42P01")])
    assert res.outcome is Outcome.PASS
    assert res.records_passed == 1


def test_timeout_is_hang_and_stops_file():
    records = [stmt(1), stmt(2)]
    session = FakeSession([timeout_out(), ok_out()])
    result = slt_runner.SltFileResult("x.slt", Outcome.PASS)
    slt_runner._run_records(records, session, result)
    assert result.outcome is Outcome.HANG
    assert len(session.sqls) == 1


def test_connection_lost_is_crash():
    res = run_records([stmt(1)], [lost_out()])
    assert res.outcome is Outcome.CRASH


def test_query_values_match_passes():
    res = run_records([query(2, ["1", "NULL"], types="II")],
                      [rows_out([["1", None]], ncols=2)])
    assert res.outcome is Outcome.PASS


def test_query_value_mismatch_is_mismatch():
    res = run_records([query(2, ["1"])], [rows_out([["2"]])])
    assert res.outcome is Outcome.MISMATCH
    assert "expected '1'" in res.reason


def test_query_column_count_mismatch_is_mismatch():
    res = run_records([query(2, ["1"], types="I")], [rows_out([["1", "2"]], ncols=2)])
    assert res.outcome is Outcome.MISMATCH


def test_query_without_recorded_results_is_invalid_case():
    res = run_records([query(2, None)], [])
    assert res.outcome is Outcome.INVALID_CASE


def test_rowsort_applies_before_comparison():
    res = run_records([query(2, ["a", "b"], types="T", sort="rowsort")],
                      [rows_out([["b"], ["a"]])])
    assert res.outcome is Outcome.PASS


def test_halt_stops_the_file():
    records = [stmt(1), SltRecord(kind="halt", line=2), stmt(3)]
    session = FakeSession([ok_out()])
    result = slt_runner.SltFileResult("x.slt", Outcome.PASS)
    slt_runner._run_records(records, session, result)
    assert result.outcome is Outcome.PASS
    assert len(session.sqls) == 1


# ---------------------------------------------------------------------------------------------
# record path
# ---------------------------------------------------------------------------------------------


def test_record_refuses_without_review_gate(monkeypatch, tmp_path):
    monkeypatch.delenv("PGARBOR_ORACLE_REVIEW", raising=False)
    src = tmp_path / "in.slt"
    src.write_text("statement ok\nselect 1\n", encoding="utf-8")
    out = tmp_path / "out.slt"
    with pytest.raises(PermissionError):
        slt_runner.record(src, ENDPOINT, out)
    assert not out.exists()


def test_record_fills_query_expectations():
    records = [SltRecord(kind="query", line=1, sql="select 1", types="I", sort="nosort")]
    problems: list[str] = []
    slt_runner._record_records(records, FakeSession([rows_out([["1"], [None]])]), problems)
    assert problems == []
    assert records[0].expected == ["1", "NULL"]


def test_record_statement_ok_that_errors_is_problem():
    records = [stmt(1, sql="select 1/0", ok=True)]
    problems: list[str] = []
    slt_runner._record_records(records, FakeSession([err_out("22012", "division by zero")]),
                               problems)
    assert len(problems) == 1
    assert problems[0].startswith("line 1:")


def test_record_writes_nothing_when_authored_statement_ok_errors(monkeypatch, tmp_path):
    monkeypatch.setenv("PGARBOR_ORACLE_REVIEW", "1")
    src = tmp_path / "in.slt"
    src.write_text("statement ok\nselect 1/0\n", encoding="utf-8")
    out = tmp_path / "expected" / "out.slt"
    monkeypatch.setattr(slt_runner, "Session",
                        lambda endpoint, name, step_timeout_s=10.0: FakeSession(
                            [err_out("22012", "division by zero")]))
    problems = slt_runner.record(src, ENDPOINT, out)
    assert problems and problems[0].startswith("line 1:")
    assert not out.exists()


def test_record_writes_output_when_clean(monkeypatch, tmp_path):
    monkeypatch.setenv("PGARBOR_ORACLE_REVIEW", "1")
    src = tmp_path / "in.slt"
    src.write_text("statement ok\nselect 1\n", encoding="utf-8")
    out = tmp_path / "expected" / "out.slt"
    monkeypatch.setattr(slt_runner, "Session",
                        lambda endpoint, name, step_timeout_s=10.0: FakeSession([ok_out()]))
    assert slt_runner.record(src, ENDPOINT, out) == []
    assert out.exists()


# ---------------------------------------------------------------------------------------------
# tree runner
# ---------------------------------------------------------------------------------------------


class _FailingStartTarget:
    name = "fake"
    kind = "pgarbor"

    def start(self) -> None:
        pass

    def stop(self) -> None:
        pass

    def provision(self, case_id: str, attempt: int = 1):
        raise TargetStartError("binary exited immediately")

    def release(self, env, keep: bool = False) -> None:
        pass


def test_run_tree_target_start_error_is_crash(tmp_path: Path):
    (tmp_path / "a").mkdir()
    (tmp_path / "a" / "x.slt").write_text("statement ok\nselect 1\n", encoding="utf-8")
    results = slt_runner.run_tree(tmp_path, _FailingStartTarget(), jobs=1)
    assert len(results) == 1
    assert results[0].path == "a/x.slt"
    assert results[0].outcome is Outcome.CRASH
    assert "binary exited immediately" in results[0].reason
