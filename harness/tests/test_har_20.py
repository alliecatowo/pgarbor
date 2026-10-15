"""HAR-20 tests: isolationtester-compatible runner (PQprint formatting, control flow, outcomes)."""

from __future__ import annotations

import time
from pathlib import Path

import pytest

from pgarbor_harness.isolation import tester
from pgarbor_harness.isolation.spec_parser import (IsolationSpec, Marker, PermutationStep,
                                                   SpecSession, SpecStep)
from pgarbor_harness.model import (ColumnDesc, Endpoint, ErrorInfo, Notice, Outcome,
                                   StatementResult)
from pgarbor_harness.runner.session import StepOutput


def rows(cols, data):
    return StatementResult(kind="rows", columns=[ColumnDesc(c, 25) for c in cols],
                           rows=[list(r) for r in data], command_tag=f"SELECT {len(data)}")


# ---------------------------------------------------------------------------------------------
# print_result_set
# ---------------------------------------------------------------------------------------------


def test_single_int_column():
    assert tester.print_result_set(rows(["i"], [["42"]])) == " i\n--\n42\n(1 row)\n\n"


def test_zero_rows_plural_and_header_only():
    assert tester.print_result_set(rows(["a", "b"], [])) == "a|b\n-+-\n(0 rows)\n\n"


def test_numeric_right_text_left_mixed():
    out = tester.print_result_set(rows(["id", "val"], [["1", "x"], ["22", "hello"]]))
    assert out == ("id|val  \n" "--+-----\n" " 1|x    \n" "22|hello\n" "(2 rows)\n\n")


def test_text_column_header_left_aligned_wide_header():
    out = tester.print_result_set(rows(["name"], [["a"]]))
    assert out == "name\n----\na   \n(1 row)\n\n"


def test_numeric_detection_rules():
    # last char must be a digit; leading E/e is not numeric; '+' is not numeric
    assert tester.print_result_set(rows(["c"], [["1."]])).splitlines()[0] == "c "
    assert tester.print_result_set(rows(["col"], [["e5"]])).splitlines()[2] == "e5 "
    assert tester.print_result_set(rows(["col"], [["+5"]])).splitlines()[2] == "+5 "
    assert tester.print_result_set(rows(["col"], [["1e5"]])).splitlines()[2] == "1e5"
    assert tester.print_result_set(rows(["col"], [["-1.5"]])).splitlines()[2] == "-1.5"


def test_null_and_empty_do_not_affect_width_or_numeric():
    out = tester.print_result_set(rows(["n"], [[None], ["7"], [""]]))
    assert out == "n\n-\n \n7\n \n(3 rows)\n\n"


def test_width_counts_bytes():
    out = tester.print_result_set(rows(["t"], [["é"], ["abc"]]))
    lines = out.splitlines()
    assert lines[2] == "é "      # 2 bytes + 1 pad = width 3
    assert lines[3] == "abc"


def test_no_columns_prints_nothing():
    assert tester.print_result_set(StatementResult(kind="rows")) == ""


def test_read_write_unique_snippet():
    out = tester.print_result_set(rows(["i"], [["42"]]))
    assert "step r2: SELECT * FROM test;\n" + out == (
        "step r2: SELECT * FROM test;\n i\n--\n42\n(1 row)\n\n")


def test_error_line_and_notice_text():
    e = ErrorInfo("ERROR", "40001", "could not serialize", detail="d", hint="h")
    assert tester._error_line(e) == "ERROR:  could not serialize\n"
    n = Notice("NOTICE", "00000", "hi", detail="dd")
    assert tester._notice_text(n) == "NOTICE:  hi\nDETAIL:  dd\n"


# ---------------------------------------------------------------------------------------------
# Fake sessions for control-flow tests
# ---------------------------------------------------------------------------------------------


class World:
    nofunc = False
    pid_seq = 100
    blocked: dict = {}        # pid -> True while blocked
    sessions: dict = {}
    log: list = []


class FakeSession:
    """Behaviour by SQL text: 'LOCK' steps block until 'UNLOCK' runs on another session."""

    def __init__(self, endpoint, name, step_timeout_s=10.0):
        self.name = name
        self.pid = None
        self.pending_sql = None
        self.waiting = False
        self.pending = False

    def connect(self):
        World.pid_seq += 1
        self.pid = str(World.pid_seq)
        World.sessions[self.pid] = self

    def close(self):
        pass

    def cancel(self):
        pass

    def run_simple(self, sql):
        World.log.append((self.name, sql))
        if sql == "SELECT pg_backend_pid()":
            return StepOutput(results=[rows(["pg_backend_pid"], [[self.pid]])])
        if sql.startswith("BAD"):
            return StepOutput(results=[StatementResult(
                kind="error", error=ErrorInfo("ERROR", "42601", "syntax error"))])
        if sql.startswith("SEL"):
            return StepOutput(results=[rows(["x"], [["1"]])])
        return StepOutput(results=[StatementResult(kind="command", command_tag="OK")])

    def run_exec_params(self, sql, params, types, fmts, rfmt):
        if World.nofunc:
            return StepOutput(results=[StatementResult(
                kind="error", error=ErrorInfo("ERROR", "42883", "function does not exist"))])
        s = World.sessions[params[0]]
        return StepOutput(results=[rows(["b"], [["t" if s.waiting else "f"]])])

    def start(self, kind, payload):
        self.pending = True
        self.pending_sql = payload["sql"]
        self.waiting = payload["sql"].startswith("LOCK")
        World.log.append((self.name, "start " + self.pending_sql))

    def poll_done(self, wait_s):
        if self.waiting:
            time.sleep(min(wait_s, 0.001))
            return False
        return True

    def finish_pending(self):
        self.pending = False
        sql = self.pending_sql
        if sql.startswith("UNLOCK"):
            for s in World.sessions.values():
                s.waiting = False
        if sql.startswith("SEL"):
            return StepOutput(results=[rows(["x"], [["1"]])])
        if sql.startswith("ERR"):
            return StepOutput(results=[StatementResult(
                kind="error", error=ErrorInfo("ERROR", "40001", "could not serialize",
                                              detail="x"))])
        if sql.startswith("NOTE"):
            return StepOutput(results=[StatementResult(kind="command")],
                              notices=[Notice("NOTICE", "00000", "hello")])
        return StepOutput(results=[StatementResult(kind="command")])


@pytest.fixture
def fake(monkeypatch):
    World.nofunc = False
    World.pid_seq = 100
    World.sessions = {}
    World.log = []
    monkeypatch.setattr(tester, "Session", FakeSession)
    monkeypatch.setattr(tester.config, "BLOCK_SETTLE_S", 0.02)
    return World


def mkspec(steps_by_session, perms, setup=None, teardown=None, ssetup=None):
    sessions = []
    for name, steps in steps_by_session:
        sessions.append(SpecSession(name, ssetup, [SpecStep(n, q, name) for n, q in steps],
                                    None))
    return IsolationSpec("t", setup or [], teardown, sessions, perms)


EP = Endpoint("127.0.0.1", 1, "db")


def P(name, *markers):
    return PermutationStep(name, list(markers))


def test_basic_output(fake):
    spec = mkspec([("s1", [("a", "SEL 1;")]), ("s2", [("b", "ERR;")])],
                  [[P("a"), P("b")]], setup=["SEL setup"])
    text, mode = tester.run_spec(spec, EP)
    assert mode == "function"
    assert text == (
        "Parsed test spec with 2 sessions\n"
        "\nstarting permutation: a b\n"
        "x\n-\n1\n(1 row)\n\n"          # setup result set
        "step a: SEL 1;\n x\n-\n1\n(1 row)\n\n"
        "step b: ERR;\nERROR:  could not serialize\n")


def test_waiting_step_and_completion(fake):
    spec = mkspec([("s1", [("lock", "LOCK;")]), ("s2", [("c1", "UNLOCK;")])],
                  [[P("lock"), P("c1")]])
    text, _ = tester.run_spec(spec, EP)
    assert text == (
        "Parsed test spec with 2 sessions\n"
        "\nstarting permutation: lock c1\n"
        "step lock: LOCK; <waiting ...>\n"
        "step c1: UNLOCK;\n"
        "step lock: <... completed>\n")


def test_session_waits_for_own_active_step(fake):
    # s1 steps: lock (blocks), then x on the same session must wait for completion
    spec = mkspec([("s1", [("lock", "LOCK;"), ("x", "SEL 2;")]), ("s2", [("c", "UNLOCK;")])],
                  [[P("lock"), P("c"), P("x")]])
    text, _ = tester.run_spec(spec, EP)
    assert "step lock: LOCK; <waiting ...>\n" in text
    assert text.index("step lock: <... completed>") < text.index("step x: SEL 2;")


def test_star_marker_forces_wait(fake):
    spec = mkspec([("s1", [("a", "SEL 1;")]), ("s2", [("b", "SEL 2;")])],
                  [[P("a", Marker("star")), P("b")]])
    text, _ = tester.run_spec(spec, EP)
    assert "step a: SEL 1; <waiting ...>\n" in text
    assert text.endswith("step a: <... completed>\n x\n-\n1\n(1 row)\n\n")


def test_step_blocker_marker(fake):
    spec = mkspec([("s1", [("lock", "LOCK;")]), ("s2", [("b", "SEL 2;")]),
                   ("s3", [("c1", "UNLOCK;")])],
                  [[P("lock"), P("b", Marker("step", "lock")), P("c1")]])
    text, _ = tester.run_spec(spec, EP)
    assert "step b: SEL 2; <waiting ...>\n" in text
    assert text.index("step lock: <... completed>") < text.index("step b: <... completed>")


def test_notices_printed_with_session_prefix(fake):
    spec = mkspec([("s1", [("n", "NOTE;")])], [[P("n")]])
    text, _ = tester.run_spec(spec, EP)
    assert "s1: NOTICE:  hello\nstep n: NOTE;\n" in text


def test_setup_failure_aborts(fake):
    spec = mkspec([("s1", [("a", "SEL 1;")])], [[P("a")]], setup=["BAD stmt"])
    with pytest.raises(tester.IsolationAbort) as ei:
        tester.run_spec(spec, EP)
    assert ei.value.kind == "setup"
    assert "setup failed: ERROR:  syntax error" in ei.value.message
    assert ei.value.output.startswith("Parsed test spec with 1 sessions\n")


def test_blocker_on_own_session_rejected(fake):
    spec = mkspec([("s1", [("a", "SEL 1;"), ("b", "SEL 2;")])],
                  [[P("a"), P("b", Marker("step", "a"))]])
    with pytest.raises(tester.IsolationAbort):
        tester.run_spec(spec, EP)


def test_heuristic_mode_detects_wait_after_settle(fake):
    World.nofunc = True
    spec = mkspec([("s1", [("lock", "LOCK;")]), ("s2", [("c1", "UNLOCK;")])],
                  [[P("lock"), P("c1")]])
    text, mode = tester.run_spec(spec, EP)
    assert mode == "heuristic"
    assert "step lock: LOCK; <waiting ...>\n" in text
    assert text.endswith("step lock: <... completed>\n")


def test_never_completing_step_times_out(fake, monkeypatch):
    monkeypatch.setattr(tester.config, "STEP_TIMEOUT_S", 0.02)
    spec = mkspec([("s1", [("lock", "LOCK;")])], [[P("lock")]])
    with pytest.raises(tester.IsolationAbort) as ei:
        tester.run_spec(spec, EP)
    assert ei.value.kind == "hang"
    assert "isolationtester: canceling step lock after" in ei.value.output


# ---------------------------------------------------------------------------------------------
# Suite classification
# ---------------------------------------------------------------------------------------------


class FakeEnv:
    endpoint = EP


class FakeTarget:
    def provision(self, case_id, attempt=1):
        return FakeEnv()

    def release(self, env, keep=False):
        pass

    def is_alive(self, env):
        return True


def make_suite(tmp_path: Path, expected: str, spec_text="x") -> Path:
    d = tmp_path / "iso"
    (d / "specs").mkdir(parents=True)
    (d / "expected").mkdir()
    (d / "isolation_schedule").write_text("test: t1\n")
    (d / "specs" / "t1.spec").write_text(spec_text)
    (d / "expected" / "t1.out").write_text(expected)
    return d


def patch_pipeline(monkeypatch, text, mode):
    spec = mkspec([("s1", [("a", "SEL 1;")])], [[P("a")]])
    monkeypatch.setattr(tester, "parse_spec", lambda t, n="": spec)
    monkeypatch.setattr(tester, "parse_schedule", lambda t: ["t1"])
    monkeypatch.setattr(tester, "run_spec", lambda s, e: (text, mode))


def test_suite_pass(tmp_path, monkeypatch):
    d = make_suite(tmp_path, "abc\n")
    patch_pipeline(monkeypatch, "abc\n", "function")
    [r] = tester.run_suite(d, FakeTarget(), tmp_path / "out", jobs=1)
    assert r.outcome is Outcome.PASS and r.wait_detection == "function"
    assert (tmp_path / "out" / "results" / "t1.out").read_text() == "abc\n"


def test_suite_heuristic_identical_is_blocked_never_pass(tmp_path, monkeypatch):
    d = make_suite(tmp_path, "abc\n")
    patch_pipeline(monkeypatch, "abc\n", "heuristic")
    [r] = tester.run_suite(d, FakeTarget(), tmp_path / "out", jobs=1)
    assert r.outcome is Outcome.BLOCKED and r.reason == "heuristic_wait_detection"


def test_suite_heuristic_wait_line_diff_is_flaky(tmp_path, monkeypatch):
    d = make_suite(tmp_path, "step a: SEL; <waiting ...>\nstep a: <... completed>\n")
    patch_pipeline(monkeypatch, "step a: SEL;\n", "heuristic")
    [r] = tester.run_suite(d, FakeTarget(), tmp_path / "out", jobs=1)
    assert r.outcome is Outcome.FLAKY


def test_suite_heuristic_other_diff_is_mismatch(tmp_path, monkeypatch):
    d = make_suite(tmp_path, "step a: SEL;\nERROR:  boom\n")
    patch_pipeline(monkeypatch, "step a: SEL;\nERROR:  other\n", "heuristic")
    [r] = tester.run_suite(d, FakeTarget(), tmp_path / "out", jobs=1)
    assert r.outcome is Outcome.MISMATCH


def test_suite_function_diff_is_mismatch_and_variant_accepted(tmp_path, monkeypatch):
    d = make_suite(tmp_path, "one\n")
    (d / "expected" / "t1_1.out").write_text("two\n")
    patch_pipeline(monkeypatch, "two\n", "function")
    [r] = tester.run_suite(d, FakeTarget(), tmp_path / "out", jobs=1)
    assert r.outcome is Outcome.PASS
    patch_pipeline(monkeypatch, "three\n", "function")
    [r] = tester.run_suite(d, FakeTarget(), tmp_path / "out", jobs=1)
    assert r.outcome is Outcome.MISMATCH


def test_suite_abort_kinds(tmp_path, monkeypatch):
    d = make_suite(tmp_path, "x\n")
    patch_pipeline(monkeypatch, "", "function")

    def boom(kind):
        def f(s, e):
            raise tester.IsolationAbort(kind, "msg", "partial\n", "function")
        return f
    for kind, outcome in (("hang", Outcome.HANG), ("setup", Outcome.HARNESS_ERROR),
                          ("conn_lost", Outcome.MISMATCH)):
        monkeypatch.setattr(tester, "run_spec", boom(kind))
        [r] = tester.run_suite(d, FakeTarget(), tmp_path / "out", jobs=1)
        assert r.outcome is outcome
        assert (tmp_path / "out" / "results" / "t1.out").read_text() == "partial\n"


def test_suite_spec_parse_error_is_harness_error(tmp_path, monkeypatch):
    d = make_suite(tmp_path, "x\n")
    patch_pipeline(monkeypatch, "x\n", "function")

    def bad(t, n=""):
        raise tester.SpecSyntaxError("bad:1: oops")
    monkeypatch.setattr(tester, "parse_spec", bad)
    [r] = tester.run_suite(d, FakeTarget(), tmp_path / "out", jobs=1)
    assert r.outcome is Outcome.HARNESS_ERROR


@pytest.mark.oracle
def test_live_deadlock_simple_and_read_write_unique():
    pytest.skip("live oracle run is driven centrally (needs PG18 container and Target)")
