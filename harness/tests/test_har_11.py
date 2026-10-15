"""HAR-11 tests: target adapters.

No PostgreSQL wire is spoken here. PgarborTarget is exercised against a fake ``pgarbor``
binary (a Python script written to tmp_path) that listens on the requested port and answers
nothing; the readiness SQL probe is replaced by a TCP-only probe for these tests.
"""

from __future__ import annotations

import os
import signal
import socket
import sys
import time
from pathlib import Path
from types import SimpleNamespace

import pytest

from pgarbor_harness.model import Endpoint
from pgarbor_harness.runner import targets as T
from pgarbor_harness.runner.targets import (
    CaseEnv,
    DsnTarget,
    PgarborCommand,
    PgarborTarget,
    PostgresTarget,
    TargetStartError,
    has_capabilities,
)

# Fake server. Modes come from FAKE_PGARBOR_MODE:
#   listen       serve normally until SIGTERM
#   ignore-term  ignore SIGTERM (release must escalate to SIGKILL)
#   exit         print a corruption-style message to stderr and exit 3 before listening
#   busy-once    first start exits 2 with "address in use"; later starts listen
FAKE_PGARBOR = """#!@PYTHON@
import os, signal, socket, sys, time

args = sys.argv[1:]
cmd = args[0]
if cmd == "create":
    open(args[1], "wb").close()
    sys.exit(0)
if cmd != "serve":
    sys.stderr.write("unknown command " + cmd + "\\n")
    sys.exit(64)

path = args[1]
host = args[args.index("--host") + 1]
port = int(args[args.index("--port") + 1])
mode = os.environ.get("FAKE_PGARBOR_MODE", "listen")

if mode == "exit":
    sys.stderr.write("fatal: corrupt page header in " + path + "\\n")
    sys.exit(3)
if mode == "busy-once":
    marker = path + ".busy"
    if not os.path.exists(marker):
        open(marker, "wb").close()
        sys.stderr.write("bind: address in use\\n")
        sys.exit(2)

if mode == "ignore-term":
    signal.signal(signal.SIGTERM, signal.SIG_IGN)

srv = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
srv.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
srv.bind((host, port))
srv.listen(16)
print("pgarbor: listening on " + host + ":" + str(port), flush=True)
while True:
    time.sleep(0.2)
"""


@pytest.fixture(autouse=True)
def _tcp_only_readiness(monkeypatch):
    """The fake server speaks no PG wire: readiness is the TCP connect only."""
    monkeypatch.setattr(T, "_probe_sql", lambda endpoint, timeout_s: None)


@pytest.fixture
def fake_binary(tmp_path: Path) -> Path:
    path = tmp_path / "fake-pgarbor"
    path.write_text(FAKE_PGARBOR.replace("@PYTHON@", sys.executable), encoding="utf-8")
    path.chmod(0o755)
    return path


def _pgarbor(tmp_path: Path, fake: Path, **kwargs) -> PgarborTarget:
    return PgarborTarget(work_dir=tmp_path / "work",
                         command=PgarborCommand(binary=str(fake)), **kwargs)


# ---------------------------------------------------------------------------------------------
# Capabilities
# ---------------------------------------------------------------------------------------------


def test_postgres_target_grants_everything():
    assert has_capabilities(PostgresTarget(pool=object()), ["jsonb"]) == []


def test_pgarbor_missing_restart_blocks_restart_cases(tmp_path):
    target = PgarborTarget(work_dir=tmp_path,
                           missing_capabilities=frozenset({"server-restart"}))
    assert has_capabilities(target, ["server-restart"]) == ["server-restart"]
    assert has_capabilities(target, ["select", "jsonb"]) == []
    default = PgarborTarget(work_dir=tmp_path)
    assert has_capabilities(default, ["server-restart"]) == []


def test_dsn_target_does_not_advertise_restart_by_default():
    dsn = DsnTarget(Endpoint("127.0.0.1", 1, "postgres"))
    assert has_capabilities(dsn, ["server-restart"]) == ["server-restart"]
    assert has_capabilities(dsn, ["select", "copy"]) == []


# ---------------------------------------------------------------------------------------------
# PostgresTarget delegation (fake pool)
# ---------------------------------------------------------------------------------------------


class _FakeContainer:
    def __init__(self, calls: list):
        self.calls = calls

    def status(self):
        return SimpleNamespace(running=True)

    def restart(self, mode: str) -> None:
        self.calls.append(("restart", mode))

    def logs_tail(self, lines: int) -> str:
        return f"tail {lines}"


class _FakePool:
    def __init__(self):
        self.calls: list = []
        self.container = _FakeContainer(self.calls)

    def start(self) -> None:
        self.calls.append("start")

    def acquire(self, case_id: str, attempt: int = 1):
        self.calls.append(("acquire", case_id, attempt))
        return SimpleNamespace(name="db", endpoint=Endpoint("127.0.0.1", 5, "db"),
                               warnings=["w"])

    def release(self, db) -> None:
        self.calls.append(("release", db.name))

    def close(self) -> None:
        self.calls.append("close")


def test_postgres_target_delegates_to_pool():
    pool = _FakePool()
    target = PostgresTarget(pool)
    target.start()
    env = target.provision("c1", 2)
    assert env.endpoint.dbname == "db"
    assert env.warnings == ["w"]
    assert target.is_alive(env) is True
    assert target.server_log_tail(env, 5) == "tail 5"
    target.restart(env, "kill")
    target.release(env)
    target.stop()
    assert pool.calls == ["start", ("acquire", "c1", 2), ("restart", "kill"),
                          ("release", "db"), "close"]


# ---------------------------------------------------------------------------------------------
# PgarborTarget lifecycle against the fake binary
# ---------------------------------------------------------------------------------------------


def test_provision_and_release_remove_case_files(tmp_path, fake_binary, monkeypatch):
    monkeypatch.setenv("FAKE_PGARBOR_MODE", "listen")
    target = _pgarbor(tmp_path, fake_binary)
    env = target.provision("case.one", 1)
    pga, log = Path(env.work_files[0]), Path(env.work_files[1])
    try:
        assert target.is_alive(env) is True
        assert pga.is_file()
        assert env.endpoint.port > 0
        assert env.endpoint.dbname == "postgres"
        assert dict(env.endpoint.gucs)["TimeZone"] == "UTC"
    finally:
        target.release(env)
    assert target.is_alive(env) is False
    assert not pga.exists()
    assert not log.exists()
    target.release(env)  # idempotent


def test_release_kills_group_that_ignores_sigterm(tmp_path, fake_binary, monkeypatch):
    monkeypatch.setenv("FAKE_PGARBOR_MODE", "ignore-term")
    target = _pgarbor(tmp_path, fake_binary)
    env = target.provision("case.stubborn", 1)
    proc = env.handle.proc
    started = time.monotonic()
    target.release(env)
    elapsed = time.monotonic() - started
    assert elapsed < 6.0, f"release took {elapsed:.2f} s"
    assert proc.poll() is not None
    assert proc.returncode == -signal.SIGKILL


def test_provision_raises_with_stderr_tail_when_binary_exits(tmp_path, fake_binary, monkeypatch):
    monkeypatch.setenv("FAKE_PGARBOR_MODE", "exit")
    target = _pgarbor(tmp_path, fake_binary)
    with pytest.raises(TargetStartError, match="corrupt page header"):
        target.provision("case.bad", 1)
    assert list((tmp_path / "work").glob("*.pga")) == []


def test_provision_retries_once_on_address_in_use(tmp_path, fake_binary, monkeypatch):
    monkeypatch.setenv("FAKE_PGARBOR_MODE", "busy-once")
    target = _pgarbor(tmp_path, fake_binary)
    env = target.provision("case.busy", 1)
    try:
        assert target.is_alive(env) is True
    finally:
        target.release(env)


def test_restart_kill_replaces_server_process(tmp_path, fake_binary, monkeypatch):
    monkeypatch.setenv("FAKE_PGARBOR_MODE", "listen")
    target = _pgarbor(tmp_path, fake_binary)
    env = target.provision("case.restart", 1)
    try:
        old_pid = env.handle.proc.pid
        target.restart(env, "kill")
        assert env.handle.proc.pid != old_pid
        assert target.is_alive(env) is True
    finally:
        target.release(env)


def test_start_rejects_missing_binary(tmp_path):
    target = PgarborTarget(work_dir=tmp_path / "work",
                           command=PgarborCommand(binary=str(tmp_path / "no-such-binary")))
    with pytest.raises(TargetStartError, match="not found"):
        target.start()


def test_stop_releases_every_live_case(tmp_path, fake_binary, monkeypatch):
    monkeypatch.setenv("FAKE_PGARBOR_MODE", "listen")
    target = _pgarbor(tmp_path, fake_binary)
    envs = [target.provision(f"case.{i}", 1) for i in range(2)]
    procs = [env.handle.proc for env in envs]
    target.stop()
    assert all(proc.poll() is not None for proc in procs)
    assert target._live == {}


# ---------------------------------------------------------------------------------------------
# DsnTarget
# ---------------------------------------------------------------------------------------------


def test_dsn_liveness_tracks_listening_socket():
    server = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
    server.bind(("127.0.0.1", 0))
    server.listen(4)
    port = server.getsockname()[1]
    dsn = DsnTarget(Endpoint("127.0.0.1", port, "postgres"), create_databases=False)
    env = dsn.provision("case.dsn", 1)
    try:
        assert dsn.is_alive(env) is True
        assert env.warnings and "shared" in env.warnings[0]
        assert dict(env.endpoint.gucs)["DateStyle"] == "ISO, MDY"
        dsn.release(env)  # shared database: nothing to drop
    finally:
        server.close()
    assert dsn.is_alive(env) is False


def test_dsn_restart_is_refused():
    dsn = DsnTarget(Endpoint("127.0.0.1", 1, "postgres"), create_databases=False)
    env = CaseEnv(case_id="case.restart", attempt=1, endpoint=dsn.endpoint,
                  handle=None)
    with pytest.raises(TargetStartError, match="not supported"):
        dsn.restart(env, "clean")
