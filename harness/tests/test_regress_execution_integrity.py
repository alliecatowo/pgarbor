"""Fresh output and process success precede any comparison verdict."""
import subprocess
import pytest
from pgarbor_harness.model import Outcome
from pgarbor_harness.regress import psql, runner
from pgarbor_harness.regress.manifest import RegressTest


@pytest.mark.parametrize("code", [125, 126, 127, 1, 2])
def test_failed_launch_cannot_reuse_stale_artifact(tmp_path, monkeypatch, code):
    src = tmp_path / "src"
    (src / "sql").mkdir(parents=True)
    (src / "expected").mkdir()
    (src / "sql" / "probe.sql").write_text("SELECT 1;")
    (src / "expected" / "probe.out").write_text("matching stale output\n")
    out = tmp_path / "out"
    (out / "results").mkdir(parents=True)
    artifact = out / "results" / "probe.out"
    artifact.write_text("matching stale output\n")
    def fake(*args, **kwargs):
        kwargs["runner"] = lambda *a: (code, "launcher failed")
        return psql.run_psql(*args, **kwargs)
    monkeypatch.setattr(runner, "run_psql", fake)
    test = RegressTest(name="probe", group=0, sql_path="sql/probe.sql", expected_paths=["expected/probe.out"])
    result, _ = runner._judge("probe", 0, test, src, "localhost", 1, out, None, {}, None)
    assert not artifact.exists()
    assert result.official is not Outcome.PASS
    assert result.differential is not Outcome.PASS


def test_timeout_removes_old_output(tmp_path):
    src = tmp_path / "probe.sql"
    src.write_text("select 1")
    (tmp_path / "results").mkdir()
    artifact = tmp_path / "results" / "probe.out"
    artifact.write_text("stale")
    def timeout(*a): raise subprocess.TimeoutExpired("probe", 1)
    result = psql.run_psql("probe", src, "localhost", 1, "postgres", tmp_path, tmp_path, runner=timeout)
    assert result.timed_out
    assert not artifact.exists()


@pytest.mark.parametrize("fresh,baseline,official,differential", [
    (False, False, Outcome.HARNESS_ERROR, Outcome.HARNESS_ERROR),
    (True, False, Outcome.PASS, Outcome.BLOCKED),
    (True, True, Outcome.PASS, Outcome.PASS),
])
def test_successful_launch_needs_output_and_independent_baseline(tmp_path, monkeypatch,
                                                               fresh, baseline, official, differential):
    src = tmp_path / "src"
    (src / "sql").mkdir(parents=True)
    (src / "expected").mkdir()
    (src / "sql" / "probe.sql").write_text("SELECT 1/0;")
    text = "SELECT 1/0;\nERROR:  division by zero\n"
    (src / "expected" / "probe.out").write_text(text)
    bases = tmp_path / "baseline"
    bases.mkdir()
    if baseline: (bases / "probe.out").write_text(text)
    out = tmp_path / "out"
    def launch(argv, stdin, timeout):
        if fresh:
            (out / "results" / "probe.out").write_text(text)
        return 0, ""
    def fake(*args, **kwargs):
        return psql.run_psql(*args, **kwargs, runner=launch)
    monkeypatch.setattr(runner, "run_psql", fake)
    test = RegressTest(name="probe", group=0, sql_path="sql/probe.sql",
                       expected_paths=["expected/probe.out"])
    result, _ = runner._judge("probe", 0, test, src, "localhost", 1, out, bases, {}, None)
    assert result.official is official
    assert result.differential is differential
