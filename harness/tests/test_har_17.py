"""HAR-17 tests: regress runner verdict rules, scheduling and recovery.

No podman, PostgreSQL or real psql is used. run_psql, run_admin_sql and compare_output are
replaced with fakes; the fake compare is a byte-exact line comparison that follows the
outdiff contract (identical to any candidate, else the candidate with the fewest changed
lines).
"""

from __future__ import annotations

import difflib
import json
from pathlib import Path
from typing import Callable, Optional

import pytest

from pgarbor_harness.model import Outcome
from pgarbor_harness.regress import runner
from pgarbor_harness.regress.manifest import RegressSchedule, RegressTest
from pgarbor_harness.regress.outdiff import OutDiff
from pgarbor_harness.regress.psql import PsqlRun


def fake_compare(results: Path, candidates: list[Path]) -> OutDiff:
    if not candidates:
        return OutDiff(False, None, None, -1, "no expected output")
    if not results.is_file():
        return OutDiff(False, None, None, -1, "no results file")
    got = results.read_text(encoding="utf-8").splitlines(keepends=True)
    best: Optional[Path] = None
    best_n = -1
    best_text = ""
    for cand in candidates:
        if not cand.is_file():
            continue
        exp = cand.read_text(encoding="utf-8").splitlines(keepends=True)
        if exp == got:
            return OutDiff(True, cand, None, 0, "")
        lines = list(difflib.unified_diff(exp, got, fromfile=str(cand), tofile=str(results)))
        n = sum(1 for ln in lines if ln[:1] in ("+", "-") and not ln.startswith(("+++", "---")))
        if best is None or n < best_n:
            best, best_n, best_text = cand, n, "".join(lines)
    if best is None:
        return OutDiff(False, None, None, -1, "no expected output")
    return OutDiff(False, None, best, best_n, best_text)


class FakePsql:
    """Stands in for regress.psql.run_psql; ``table`` maps test -> (text, exit, timed_out)."""

    def __init__(self, table: dict[str, tuple[str, Optional[int], bool]],
                 on_run: Optional[Callable[[str], None]] = None) -> None:
        self.table = table
        self.on_run = on_run
        self.calls: list[tuple[str, str]] = []

    def __call__(self, test, sql_file, host, port, db, regress_dir, out_dir, *args, **kwargs):
        self.calls.append((test, db))
        if self.on_run is not None:
            self.on_run(test)
        text, code, timed_out = self.table.get(test, ("", 0, False))
        out = out_dir / "results" / f"{test}.out"
        out.parent.mkdir(parents=True, exist_ok=True)
        out.write_text(text, encoding="utf-8")
        return PsqlRun(test=test, out_file=out, exit_code=code, timed_out=timed_out,
                       elapsed_ms=1)


class Env:
    def __init__(self, tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
        self.regress = tmp_path / "regress"
        self.out = tmp_path / "out"
        self.expected_root = tmp_path / "expected"
        self.baseline = self.expected_root / "regress"
        (self.regress / "expected").mkdir(parents=True)
        (self.regress / "sql").mkdir(parents=True)
        self.baseline.mkdir(parents=True)
        self.admin_calls: list[str] = []
        self.admin_result: tuple[int, str] = (0, "")
        monkeypatch.setattr(runner, "compare_output", fake_compare)
        monkeypatch.setattr(runner, "run_admin_sql", self._admin)

    def _admin(self, sql, host, port, db="postgres"):
        self.admin_calls.append(sql)
        return self.admin_result

    def expect(self, name: str, text: str) -> None:
        (self.regress / "expected" / f"{name}.out").write_text(text, encoding="utf-8")

    def expect_all(self, names: list[str], text: str) -> None:
        for n in names:
            self.expect(n, text)

    def base(self, name: str, text: str) -> None:
        (self.baseline / f"{name}.out").write_text(text, encoding="utf-8")

    def status(self, data: dict[str, str]) -> None:
        (self.baseline / "_official_status.json").write_text(json.dumps(data), encoding="utf-8")

    def psql(self, monkeypatch: pytest.MonkeyPatch, table, on_run=None) -> FakePsql:
        fake = FakePsql(table, on_run)
        monkeypatch.setattr(runner, "run_psql", fake)
        return fake


@pytest.fixture
def env(tmp_path, monkeypatch) -> Env:
    return Env(tmp_path, monkeypatch)


def make_schedule(groups: list[list[str]]) -> RegressSchedule:
    tests = {}
    for gi, group in enumerate(groups):
        for name in group:
            tests[name] = RegressTest(name=name, group=gi, sql_path=Path("sql") / f"{name}.sql",
                                      expected_paths=[Path("expected") / f"{name}.out"])
    return RegressSchedule(groups=[list(g) for g in groups],
                           group_comments=["" for _ in groups], tests=tests)


def run(env: Env, schedule: RegressSchedule, **kwargs):
    return runner.run_schedule(schedule, env.regress, "127.0.0.1", 54320, env.out,
                               env.baseline, **kwargs)


def ok(text: str = "ok\n") -> tuple[str, Optional[int], bool]:
    return (text, 0, False)


def test_identical_output_passes_both_families(env, monkeypatch):
    sched = make_schedule([["test_setup"], ["boolean"]])
    env.expect_all(["test_setup", "boolean"], "ok\n")
    env.psql(monkeypatch, {"test_setup": ok(), "boolean": ok()})
    results = run(env, sched)
    assert [r.test for r in results] == ["test_setup", "boolean"]
    for r in results:
        assert r.official is Outcome.PASS
        assert r.differential is Outcome.PASS
        assert r.reason == ""
    lines = (env.out / "regression.out").read_text(encoding="utf-8").splitlines()
    assert lines[1].startswith("ok 2 - boolean")
    summary = json.loads((env.out / "summary.json").read_text(encoding="utf-8"))
    assert [s["test"] for s in summary] == ["test_setup", "boolean"]


def test_oracle_env_blocks_official_when_oracle_fails(env, monkeypatch):
    sched = make_schedule([["test_setup"], ["boolean"]])
    env.expect_all(["test_setup", "boolean"], "ok\n")
    env.expect("boolean", "expected\n")
    env.base("boolean", "actual\n")
    env.base("test_setup", "ok\n")
    env.status({"boolean": "FAIL", "test_setup": "PASS"})
    env.psql(monkeypatch, {"test_setup": ok(), "boolean": ok("actual\n")})
    results = {r.test: r for r in run(env, sched)}
    assert results["boolean"].official is Outcome.BLOCKED
    assert results["boolean"].reason == "oracle_env"
    assert results["boolean"].differential is Outcome.PASS
    assert results["test_setup"].official is Outcome.PASS


def test_oracle_pass_keeps_official_mismatch(env, monkeypatch):
    sched = make_schedule([["boolean"]])
    env.expect("boolean", "expected\n")
    env.base("boolean", "actual\n")
    env.status({"boolean": "PASS"})
    env.psql(monkeypatch, {"boolean": ok("actual\n")})
    (r,) = run(env, sched)
    assert r.official is Outcome.MISMATCH
    assert r.differential is Outcome.PASS


def test_mismatch_in_both_families(env, monkeypatch):
    sched = make_schedule([["boolean"]])
    env.expect("boolean", "expected\n")
    env.base("boolean", "pg output\n")
    env.psql(monkeypatch, {"boolean": ok("got\n")})
    (r,) = run(env, sched)
    assert r.official is Outcome.MISMATCH
    assert r.differential is Outcome.MISMATCH
    assert r.reason == "mismatch"
    assert "boolean" in (env.out / "regression.diffs").read_text(encoding="utf-8")


def test_missing_baseline_blocks_differential_only(env, monkeypatch):
    sched = make_schedule([["boolean"]])
    env.expect("boolean", "expected\n")
    env.psql(monkeypatch, {"boolean": ok("got\n")})
    (r,) = run(env, sched)
    assert r.official is Outcome.MISMATCH
    assert r.differential is Outcome.BLOCKED
    assert r.extra["differential_reason"] == "reference_missing"


def test_unsupported_feature_message(env, monkeypatch):
    sched = make_schedule([["boolean"]])
    env.expect("boolean", "ok\n")
    env.base("boolean", "ok\n")
    env.psql(monkeypatch, {"boolean": ok("ERROR:  feature is not supported\n")})
    (r,) = run(env, sched)
    assert r.official is Outcome.UNSUPPORTED
    assert r.differential is Outcome.UNSUPPORTED


def test_corruption_text_is_corruption(env, monkeypatch):
    sched = make_schedule([["boolean"]])
    env.expect("boolean", "ok\n")
    env.psql(monkeypatch, {"boolean": ok('ERROR:  could not read block 0 in file "base/1/2"\n')})
    (r,) = run(env, sched)
    assert r.official is Outcome.CORRUPTION
    assert r.differential is Outcome.CORRUPTION


def test_timeout_is_hang(env, monkeypatch):
    sched = make_schedule([["boolean"]])
    env.expect("boolean", "ok\n")
    env.psql(monkeypatch, {"boolean": ("", None, True)})
    (r,) = run(env, sched)
    assert r.official is Outcome.HANG
    assert r.differential is Outcome.HANG


def test_crash_then_remaining_tests_are_target_down(env, monkeypatch):
    sched = make_schedule([["test_setup"], ["a", "b"], ["c", "d"]])
    names = ["test_setup", "a", "b", "c", "d"]
    env.expect_all(names, "ok\n")
    alive = {"up": True}

    def on_run(test: str) -> None:
        if test == "b":
            alive["up"] = False

    table = {n: ok() for n in names}
    table["b"] = ("server closed the connection unexpectedly\n", 2, False)
    fake = env.psql(monkeypatch, table, on_run)
    results = {r.test: r for r in run(env, sched, target_alive=lambda: alive["up"])}
    assert results["b"].official is Outcome.CRASH
    assert results["b"].differential is Outcome.CRASH
    assert results["a"].official is Outcome.PASS
    for n in ("c", "d"):
        assert results[n].official is Outcome.BLOCKED
        assert results[n].differential is Outcome.BLOCKED
        assert results[n].reason == "target_down"
    assert all(t not in ("c", "d") for t, _ in fake.calls)


def test_crash_with_restart_continues(env, monkeypatch):
    sched = make_schedule([["test_setup"], ["a", "b"], ["c"]])
    names = ["test_setup", "a", "b", "c"]
    env.expect_all(names, "ok\n")
    alive = {"up": True}
    restarts: list[int] = []

    def on_run(test: str) -> None:
        if test == "b":
            alive["up"] = False

    def restart() -> None:
        restarts.append(1)
        alive["up"] = True

    table = {n: ok() for n in names}
    table["b"] = ("server closed the connection unexpectedly\n", 2, False)
    env.psql(monkeypatch, table, on_run)
    results = {r.test: r for r in run(env, sched, target_alive=lambda: alive["up"],
                                      restart_target=restart)}
    assert restarts == [1]
    assert results["b"].official is Outcome.CRASH
    assert results["c"].official is Outcome.PASS


def test_only_always_runs_test_setup_first(env, monkeypatch):
    sched = make_schedule([["test_setup"], ["a", "b"], ["c"]])
    names = ["test_setup", "a", "b", "c"]
    env.expect_all(names, "ok\n")
    fake = env.psql(monkeypatch, {n: ok() for n in names})
    results = run(env, sched, only=["c"])
    assert [r.test for r in results] == ["test_setup", "c"]
    assert [t for t, _ in fake.calls] == ["test_setup", "c"]
    assert results[1].group == 2


def test_only_rejects_unknown_test(env, monkeypatch):
    sched = make_schedule([["test_setup"], ["boolean"]])
    env.psql(monkeypatch, {})
    with pytest.raises(ValueError):
        run(env, sched, only=["nope"])


def test_create_database_failure_blocks_family(env, monkeypatch):
    env.admin_result = (1, "ERROR:  permission denied to create database\n")
    sched = make_schedule([["test_setup"], ["boolean"]])
    fake = env.psql(monkeypatch, {})
    results = run(env, sched)
    assert fake.calls == []
    assert env.admin_calls[0].startswith("DROP DATABASE IF EXISTS")
    for r in results:
        assert r.official is Outcome.BLOCKED
        assert r.differential is Outcome.BLOCKED
        assert r.reason == "create_database"


def test_use_existing_skips_database_preparation(env, monkeypatch):
    env.admin_result = (1, "ERROR:  should not run\n")
    sched = make_schedule([["test_setup"]])
    env.expect("test_setup", "ok\n")
    fake = env.psql(monkeypatch, {"test_setup": ok()})
    (r,) = run(env, sched, use_existing=True)
    assert env.admin_calls == []
    assert fake.calls == [("test_setup", "regression")]
    assert r.official is Outcome.PASS


def test_prepare_database_statement_order(env):
    assert runner.prepare_database("127.0.0.1", 54320, False) is None
    assert env.admin_calls[0] == 'DROP DATABASE IF EXISTS "regression"'
    assert env.admin_calls[1] == 'CREATE DATABASE "regression" TEMPLATE=template0'
    assert len(env.admin_calls) == 2 + len(runner.config.REGRESS_DB_SETTINGS)


def test_record_baseline_requires_review_env(env, monkeypatch, tmp_path):
    monkeypatch.delenv("PGARBOR_ORACLE_REVIEW", raising=False)
    with pytest.raises(RuntimeError):
        runner.record_baseline(make_schedule([["test_setup"]]), env.regress, "127.0.0.1",
                               54319, tmp_path / "out", env.expected_root)


def test_record_baseline_writes_passing_outputs_and_status(env, monkeypatch, tmp_path):
    monkeypatch.setenv("PGARBOR_ORACLE_REVIEW", "1")
    sched = make_schedule([["test_setup"], ["boolean"]])
    env.expect("test_setup", "ok\n")
    env.expect("boolean", "ok\n")
    env.psql(monkeypatch, {"test_setup": ok(), "boolean": ("", None, True)})
    status = runner.record_baseline(sched, env.regress, "127.0.0.1", 54319, tmp_path / "out",
                                    env.expected_root)
    assert status == {"test_setup": "PASS", "boolean": "FAIL"}
    assert (env.baseline / "test_setup.out").read_text(encoding="utf-8") == "ok\n"
    assert not (env.baseline / "boolean.out").exists()
    saved = json.loads((env.baseline / "_official_status.json").read_text(encoding="utf-8"))
    assert saved == status


@pytest.mark.parametrize("text,expected", [
    ("ERROR:  could not read block 3 in file x\n", True),
    ("ERROR:  invalid page in block 0\n", True),
    ("NOTICE:  invalid page\n", False),
    ("ok\n", False),
])
def test_corruption_detection(text, expected):
    assert runner._corrupt(text) is expected


@pytest.mark.parametrize("unified,expected", [
    ("--- e\n+++ r\n@@ -1 +1 @@\n-ok\n+ERROR:  x is not implemented\n", True),
    ("--- e\n+++ r\n@@ -1 +1 @@\n-ok\n+ERROR:  syntax error\n", False),
    ("--- e\n+++ r\n@@ -1 +1 @@\n-ERROR:  not supported\n+ok\n", False),
    ("", False),
])
def test_unsupported_detection(unified, expected):
    assert runner._unsupported(unified) is expected
