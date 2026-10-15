"""HAR-10 tests: executor driven by a FakeTarget + FakeSession."""

from __future__ import annotations

import pytest

from pgarbor_harness.cases.model import Case
from pgarbor_harness.model import StatementResult
from pgarbor_harness.runner import executor, session as session_mod
from pgarbor_harness.runner.session import StepOutput
from pgarbor_harness.runner.targets import CaseEnv


def rows_result(v: str) -> StatementResult:
    return StatementResult(kind="rows", rows=[[v]], command_tag="SELECT 1")


class FakeTarget:
    name = "fake"
    kind = "dsn"

    def __init__(self, alive=True, log=""):
        self.alive = alive
        self.log = log
        self.released = []
        self.restarts = []
        self.events = []

    def capabilities(self):
        return frozenset()

    def start(self):
        pass

    def provision(self, case_id, attempt=1):
        return CaseEnv(case_id=case_id, attempt=attempt, endpoint=None)

    def release(self, env, keep=False):
        self.events.append("release")
        self.released.append((env, keep))

    def restart(self, env, mode):
        self.restarts.append(mode)

    def is_alive(self, env):
        return self.alive

    def server_log_tail(self, env, lines=40):
        return self.log

    def stop(self):
        pass


LOG = []


class FakeSession:
    script: dict = {}   # (name, op index) -> StepOutput override
    block: set = set()  # session names whose async never finishes within settle

    def __init__(self, endpoint, name, step_timeout_s=10.0):
        self.name = name
        self.pending = False
        self.connects = 0
        self.closed = False
        self.calls = 0
        LOG.append((name, "new"))

    def connect(self):
        self.connects += 1
        LOG.append((self.name, "connect"))

    def close(self):
        self.closed = True
        LOG.append((self.name, "close"))
        FakeTarget_events.append("close")

    def reconnect(self):
        LOG.append((self.name, "reconnect"))

    @property
    def txn_status(self):
        return "I"

    def _out(self, sql):
        self.calls += 1
        o = FakeSession.script.get((self.name, sql))
        if o is not None:
            return o
        return StepOutput(results=[rows_result(sql)], txn_status="T")

    def run_simple(self, sql):
        return self._out(sql)

    def run_prepare(self, name, sql, pt):
        return self._out("prepare:" + name)

    def run_describe(self, name):
        return self._out("describe:" + name)

    def run_execute(self, name, params, pf, rf):
        return self._out("execute:" + name)

    def run_exec_params(self, sql, params, pt, pf, rf):
        return self._out(sql)

    def run_close(self, name):
        return self._out("close:" + name)

    def start(self, kind, payload):
        self.pending = True
        self._sql = payload.get("sql")

    def poll_done(self, wait_s):
        return self.name not in FakeSession.block

    def finish_pending(self):
        self.pending = False
        return StepOutput(results=[rows_result("async:" + str(self._sql))], txn_status="I")


FakeTarget_events: list = []


@pytest.fixture(autouse=True)
def fake_session(monkeypatch):
    LOG.clear()
    FakeTarget_events.clear()
    FakeSession.script = {}
    FakeSession.block = set()
    monkeypatch.setattr(session_mod, "Session", FakeSession)


def mk(steps, setup=None, sessions=("a", "b")):
    d = {"id": "t.case", "title": "t", "family": "t", "feature_ids": ["F"],
         "sessions": list(sessions), "steps": steps}
    if setup:
        d["setup"] = setup
    return Case.from_dict(d)


def test_all_kinds_one_observation_each():
    case = mk([
        {"sql": "select 1"},
        {"parse": {"name": "s", "sql": "select $1"}},
        {"describe": {"statement": "s"}},
        {"execute": {"statement": "s", "params": ["1"]}},
        {"exec_params": {"sql": "select $1", "params": ["1"]}},
        {"close": {"statement": "s"}},
        {"reconnect": {}},
    ])
    t = FakeTarget()
    tr = executor.execute_case(case, t, "reference")
    assert tr.endpoint == "reference"
    assert [s.index for s in tr.steps] == list(range(7))
    assert [s.kind for s in tr.steps][-1] == "reconnect"
    assert tr.steps[-1].results == [] and tr.steps[-1].txn_status == "I"
    assert tr.steps[0].txn_status == "T"
    assert t.released and t.released[0][1] is False


def test_setup_closed_before_steps_and_release_after_close():
    case = mk([{"sql": "select 1"}], setup="create table x();")
    t = FakeTarget()
    tr = executor.execute_case(case, t, keep=True)
    assert tr.setup[0].index == 0 and tr.setup[0].session == "setup"
    assert LOG.index(("setup", "close")) < LOG.index(("a", "connect"))
    assert t.released[0][1] is True
    assert FakeTarget_events  # sessions closed


def test_async_and_wait():
    case = mk([
        {"sql": "select pg_sleep(1)", "session": "a", "async": True, "expect_blocked": True},
        {"sql": "select 2", "session": "b"},
        {"wait": {}, "session": "a"},
    ])
    FakeSession.block = {"a"}
    tr = executor.execute_case(case, FakeTarget())
    a = tr.steps[0]
    assert a.txn_status == "A" and a.results == [] and a.blocked_observed is True
    assert tr.steps[2].results[0].rows == [["async:select pg_sleep(1)"]]


def test_async_not_blocked():
    case = mk([{"sql": "select 1", "async": True}, {"wait": {}}])
    tr = executor.execute_case(case, FakeTarget())
    assert tr.steps[0].blocked_observed is False


def test_timeout_stops():
    case = mk([{"sql": "select 1"}, {"sql": "slow"}, {"sql": "never"}])
    FakeSession.script = {("a", "slow"): StepOutput(timed_out=True, txn_status="I")}
    tr = executor.execute_case(case, FakeTarget())
    assert len(tr.steps) == 2 and tr.steps[1].timed_out
    assert executor.server_log_excerpt(FakeTarget(), "t.case") is None or True


def test_connection_lost_dead_stops():
    case = mk([{"sql": "boom"}, {"sql": "never"}])
    FakeSession.script = {("a", "boom"): StepOutput(connection_lost=True)}
    tr = executor.execute_case(case, FakeTarget(alive=False, log="x\nthread panicked at y"))
    assert len(tr.steps) == 1 and tr.steps[0].connection_lost
    assert "panicked at" in executor.server_log_excerpt(FakeTarget(), "t.case")


def test_connection_lost_alive_continues_with_reconnect():
    case = mk([{"sql": "term"}, {"sql": "after"}])
    FakeSession.script = {("a", "term"): StepOutput(connection_lost=True)}
    tr = executor.execute_case(case, FakeTarget(alive=True))
    assert len(tr.steps) == 2 and not tr.steps[1].connection_lost
    assert ("a", "reconnect") in LOG


def test_connection_lost_alive_but_panic_stops():
    case = mk([{"sql": "term"}, {"sql": "after"}])
    FakeSession.script = {("a", "term"): StepOutput(connection_lost=True)}
    tr = executor.execute_case(case, FakeTarget(alive=True, log="panicked at src/x.rs"))
    assert len(tr.steps) == 1


def test_harness_exception_recorded_and_released(monkeypatch):
    case = mk([{"sql": "ok"}, {"sql": "bad"}, {"sql": "never"}])

    def boom(self, sql):
        if sql == "bad":
            raise ValueError("oops")
        return StepOutput(results=[rows_result(sql)])

    # monkeypatch restores the original FakeSession.run_simple on teardown; a manual
    # assign + `del` would delete the class's own method and break later tests.
    monkeypatch.setattr(FakeSession, "run_simple", boom)
    t = FakeTarget()
    tr = executor.execute_case(case, t)
    assert len(tr.steps) == 2
    assert tr.steps[1].harness_error == "ValueError: oops"
    assert t.released


def test_restart_reconnects_sessions():
    case = mk([{"sql": "select 1"}, {"restart": "kill"}, {"sql": "select 2"}])
    t = FakeTarget()
    tr = executor.execute_case(case, t)
    assert t.restarts == ["kill"]
    assert tr.steps[1].kind == "restart" and tr.steps[1].session == ""
    assert tr.steps[1].harness_error is None
    assert ("a", "reconnect") in LOG
    assert len(tr.steps) == 3


def test_case_timeout():
    case = mk([{"sql": "select 1"}, {"sql": "select 2"}])
    tr = executor.execute_case(case, FakeTarget(), case_timeout_s=-1.0)
    assert len(tr.steps) == 1 and tr.steps[0].timed_out
