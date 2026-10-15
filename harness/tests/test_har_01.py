"""HAR-01 unit tests: pinned PG18 container lifecycle, driven by a FakeRunner.

No podman, docker or postgres is needed: every podman call is recorded as an argv list and
answered with a canned CompletedProcess. Time is faked so readiness polling is instant.
"""

from __future__ import annotations

import json
import subprocess
from types import SimpleNamespace
from typing import Optional

import pytest

from pgarbor_harness import config
from pgarbor_harness.oracle import container as oc
from pgarbor_harness.oracle.container import (
    ContainerSpec,
    ContainerStatus,
    OracleContainer,
    OracleError,
)

NAME = config.HARNESS_CONTAINER_NAME
PORT = config.HARNESS_CONTAINER_PORT
Result = tuple[int, str, str]

OK: Result = (0, "", "")
NO_SUCH: Result = (125, "", f"Error: no such container {NAME}")
PG_READY_OK: Result = (0, "/var/run/postgresql:5432 - accepting connections\n", "")
SELECT1_OK: Result = (0, "1\n", "")


class FakeClock:
    def __init__(self) -> None:
        self.now = 0.0
        self.sleeps: list[float] = []

    def monotonic(self) -> float:
        return self.now

    def sleep(self, seconds: float) -> None:
        self.sleeps.append(seconds)
        self.now += seconds


@pytest.fixture
def clock(monkeypatch: pytest.MonkeyPatch) -> FakeClock:
    c = FakeClock()
    monkeypatch.setattr(oc, "time", SimpleNamespace(monotonic=c.monotonic, sleep=c.sleep))
    return c


class FakeRunner:
    """Records every argv (and stdin) and answers by longest-matching-prefix rule.

    A rule with several results consumes them in order and then keeps returning the last one.
    Unmatched calls succeed with empty output.
    """

    def __init__(self) -> None:
        self.calls: list[list[str]] = []
        self.inputs: list[Optional[str]] = []
        self._rules: list[tuple[tuple[str, ...], list[Result]]] = []

    def on(self, prefix: tuple[str, ...], *results: Result) -> "FakeRunner":
        self._rules.append((tuple(prefix), list(results)))
        return self

    def __call__(self, argv, input_text: Optional[str] = None):  # runner signature
        argv = list(argv)
        self.calls.append(argv)
        self.inputs.append(input_text)
        best: Optional[list[Result]] = None
        best_len = -1
        for prefix, results in self._rules:
            if tuple(argv[: len(prefix)]) == prefix and len(prefix) > best_len:
                best, best_len = results, len(prefix)
        if best is None:
            return subprocess.CompletedProcess(argv, 0, "", "")
        rc, out, err = best.pop(0) if len(best) > 1 else best[0]
        return subprocess.CompletedProcess(argv, rc, out, err)


class ExplodingRunner:
    """Simulates a host without podman on PATH."""

    def __call__(self, argv, input_text: Optional[str] = None):
        raise FileNotFoundError(2, "No such file or directory", argv[0])


def inspect_doc(*, digest: str = config.PG_IMAGE_DIGEST, port: int = PORT,
                running: bool = True, mounts: tuple[str, ...] = ()) -> str:
    return json.dumps([{
        "Name": NAME,
        "ImageDigest": digest,
        "State": {"Running": running, "Status": "running" if running else "exited"},
        "HostConfig": {"PortBindings": {"5432/tcp": [{"HostIp": "127.0.0.1",
                                                       "HostPort": str(port)}]}},
        "Mounts": [{"Type": "volume", "Destination": d, "Source": "/x"} for d in mounts],
    }])


def inspect_ok(**kw) -> Result:
    return (0, inspect_doc(**kw), "")


def ready_rules(fake: FakeRunner) -> FakeRunner:
    fake.on(("podman", "exec", NAME, "pg_isready"), PG_READY_OK)
    fake.on(("podman", "exec", NAME, "psql"), SELECT1_OK)
    return fake


def podman_calls(fake: FakeRunner, verb: str) -> list[list[str]]:
    return [a for a in fake.calls if a[:2] == ["podman", verb]]


# ---------------------------------------------------------------------------------------------
# ContainerSpec / run argv
# ---------------------------------------------------------------------------------------------


def test_run_argv_is_exact() -> None:
    spec = ContainerSpec(mounts=(("/r/regress", "/r/regress", "ro,Z"),
                                 ("/r/out", "/r/out", "rw,Z")))
    expected = [
        "podman", "run", "-d", "--name", "pgarbor-harness-pg18",
        "-p", "127.0.0.1:54319:5432",
        "-e", "POSTGRES_PASSWORD=oracle",
        "-e", "POSTGRES_HOST_AUTH_METHOD=trust",
        "-e", "POSTGRES_INITDB_ARGS=--encoding=UTF8 --locale-provider=builtin --locale=C.UTF-8",
        "-e", "TZ=UTC",
        "-v", "pgarbor-harness-pg18-data:/var/lib/postgresql",
        "-v", "/r/regress:/r/regress:ro,Z",
        "-v", "/r/out:/r/out:rw,Z",
        "--shm-size=256m", config.PG_IMAGE,
        "-c", "TimeZone=UTC",
        "-c", "DateStyle=ISO, MDY",
        "-c", "IntervalStyle=postgres",
        "-c", "extra_float_digits=1",
        "-c", "autovacuum=off",
        "-c", "lc_messages=C.UTF-8",
        "-c", "max_connections=200",
        "-c", "fsync=off",
        "-c", "synchronous_commit=off",
        "-c", "full_page_writes=off",
    ]
    assert spec.run_argv() == expected


def test_run_argv_prefix_and_digest_once() -> None:
    argv = ContainerSpec().run_argv()
    assert argv[:7] == ["podman", "run", "-d", "--name", "pgarbor-harness-pg18", "-p",
                        "127.0.0.1:54319:5432"]
    assert sum(config.PG_IMAGE_DIGEST in a for a in argv) == 1
    assert "pgarbor-oracle" not in " ".join(argv)


# ---------------------------------------------------------------------------------------------
# status / drift
# ---------------------------------------------------------------------------------------------


def test_status_parses_inspect(clock: FakeClock) -> None:
    fake = FakeRunner().on(("podman", "container", "inspect"),
                           inspect_ok(running=True, mounts=("/regress",)))
    st = OracleContainer(runner=fake).status()
    assert st.exists and st.running
    assert st.image_digest == config.PG_IMAGE_DIGEST
    assert st.host_port == PORT
    assert st.mounts == ("/regress",)
    assert st.raw_inspect is not None
    assert fake.calls[0] == ["podman", "container", "inspect", NAME]


def test_status_missing_container_is_not_an_error(clock: FakeClock) -> None:
    fake = FakeRunner().on(("podman", "container", "inspect"), NO_SUCH)
    st = OracleContainer(runner=fake).status()
    assert st == ContainerStatus(exists=False, running=False)


def test_status_podman_absent_raises_oracle_error() -> None:
    with pytest.raises(OracleError, match="podman"):
        OracleContainer(runner=ExplodingRunner()).status()


def test_status_rejects_unexpected_inspect_output(clock: FakeClock) -> None:
    fake = FakeRunner().on(("podman", "container", "inspect"), (0, "{}", ""))
    with pytest.raises(OracleError, match="unexpected"):
        OracleContainer(runner=fake).status()


def test_drift_reports_digest_mismatch() -> None:
    c = OracleContainer(runner=FakeRunner())
    st = ContainerStatus(exists=True, running=True, image_digest="sha256:" + "0" * 64,
                         host_port=PORT, mounts=())
    reasons = c.drift(st)
    assert len(reasons) == 1
    assert "digest" in reasons[0]


def test_drift_empty_when_matching_and_mounts_present() -> None:
    c = OracleContainer(ContainerSpec(mounts=(("/a", "/a", "ro,Z"),)), runner=FakeRunner())
    st = ContainerStatus(exists=True, running=True, image_digest=config.PG_IMAGE_DIGEST,
                         host_port=PORT, mounts=("/var/lib/postgresql", "/a"))
    assert c.drift(st) == []


def test_drift_reports_missing_mount_and_port() -> None:
    c = OracleContainer(ContainerSpec(mounts=(("/a", "/a", "ro,Z"),)), runner=FakeRunner())
    st = ContainerStatus(exists=True, running=True, image_digest=config.PG_IMAGE_DIGEST,
                         host_port=54320, mounts=())
    reasons = c.drift(st)
    assert any("/a" in r for r in reasons)
    assert any("port" in r for r in reasons)
    assert len(reasons) == 2


# ---------------------------------------------------------------------------------------------
# ensure
# ---------------------------------------------------------------------------------------------


def test_ensure_creates_when_missing(clock: FakeClock) -> None:
    fake = ready_rules(FakeRunner()
                       .on(("podman", "container", "inspect"), NO_SUCH)
                       .on(("podman", "image", "exists"), OK)
                       .on(("podman", "run"), OK))
    c = OracleContainer(runner=fake)
    ep = c.ensure()
    assert podman_calls(fake, "run") == [c.spec.run_argv()]
    assert podman_calls(fake, "pull") == []
    assert podman_calls(fake, "start") == []
    assert ep.host == "127.0.0.1"
    assert ep.port == PORT
    assert ep.dbname == "postgres"
    assert ep.user == config.PG_SUPERUSER


def test_ensure_pulls_image_when_absent(clock: FakeClock) -> None:
    fake = ready_rules(FakeRunner()
                       .on(("podman", "container", "inspect"), NO_SUCH)
                       .on(("podman", "image", "exists"), (1, "", ""))
                       .on(("podman", "pull"), OK)
                       .on(("podman", "run"), OK))
    c = OracleContainer(runner=fake)
    c.ensure()
    pull_idx = fake.calls.index(["podman", "pull", config.PG_IMAGE])
    run_idx = fake.calls.index(c.spec.run_argv())
    assert pull_idx < run_idx


def test_ensure_starts_stopped_container(clock: FakeClock) -> None:
    fake = ready_rules(FakeRunner()
                       .on(("podman", "container", "inspect"), inspect_ok(running=False))
                       .on(("podman", "start"), OK))
    OracleContainer(runner=fake).ensure()
    assert podman_calls(fake, "start") == [["podman", "start", NAME]]
    assert podman_calls(fake, "run") == []
    assert podman_calls(fake, "rm") == []


def test_ensure_running_matching_container_does_not_touch_it(clock: FakeClock) -> None:
    fake = ready_rules(FakeRunner().on(("podman", "container", "inspect"), inspect_ok()))
    OracleContainer(runner=fake).ensure()
    assert podman_calls(fake, "start") == []
    assert podman_calls(fake, "run") == []
    assert podman_calls(fake, "rm") == []


def test_ensure_raises_on_drift_without_recreate(clock: FakeClock) -> None:
    fake = ready_rules(FakeRunner().on(("podman", "container", "inspect"),
                                       inspect_ok(digest="sha256:" + "1" * 64)))
    with pytest.raises(OracleError, match="digest"):
        OracleContainer(runner=fake).ensure()
    assert podman_calls(fake, "run") == []
    assert podman_calls(fake, "rm") == []
    assert podman_calls(fake, "start") == []


def test_ensure_recreates_on_drift_when_asked(clock: FakeClock) -> None:
    fake = ready_rules(FakeRunner()
                       .on(("podman", "container", "inspect"),
                           inspect_ok(digest="sha256:" + "1" * 64))
                       .on(("podman", "rm"), OK)
                       .on(("podman", "image", "exists"), OK)
                       .on(("podman", "run"), OK))
    c = OracleContainer(runner=fake)
    c.ensure(recreate_on_drift=True)
    rm_idx = fake.calls.index(["podman", "rm", "-f", NAME])
    run_idx = fake.calls.index(c.spec.run_argv())
    assert rm_idx < run_idx
    assert podman_calls(fake, "start") == []


def test_ensure_never_touches_pgarbor_oracle(clock: FakeClock) -> None:
    fake = FakeRunner()
    c = OracleContainer(ContainerSpec(name="pgarbor-oracle"), runner=fake)
    with pytest.raises(OracleError, match="pgarbor-oracle"):
        c.ensure()
    assert fake.calls == []  # refused before podman is ever invoked


def test_every_harness_operation_avoids_pgarbor_oracle(clock: FakeClock) -> None:
    fake = ready_rules(FakeRunner().on(("podman", "container", "inspect"), inspect_ok()))
    c = OracleContainer(runner=fake)
    c.ensure()
    c.exec_psql("select 1")
    c.logs_tail()
    c.restart("clean")
    c.stop()
    c.remove(keep_volume=False)
    assert fake.calls
    for argv in fake.calls:
        assert all("pgarbor-oracle" not in tok for tok in argv), argv


# ---------------------------------------------------------------------------------------------
# wait_ready
# ---------------------------------------------------------------------------------------------


def test_wait_ready_needs_two_consecutive_successes(clock: FakeClock) -> None:
    fake = (FakeRunner()
            .on(("podman", "exec", NAME, "pg_isready"),
                (1, "", "no response"), PG_READY_OK, PG_READY_OK)
            .on(("podman", "exec", NAME, "psql"), SELECT1_OK))
    OracleContainer(runner=fake).wait_ready(timeout_s=30)
    # first probe failed (poll 0.25 s), second succeeded (1 s until the confirming probe)
    assert clock.sleeps == [0.25, 1.0]
    isready_calls = [a for a in fake.calls if a[3] == "pg_isready"]
    assert len(isready_calls) == 3


def test_wait_ready_timeout_includes_log_tail(clock: FakeClock) -> None:
    fake = (FakeRunner()
            .on(("podman", "exec", NAME, "pg_isready"), (1, "", ""))
            .on(("podman", "logs"), (0, "LOG:  database system is ready\n", "")))
    with pytest.raises(OracleError, match="not ready after 3 s") as info:
        OracleContainer(runner=fake).wait_ready(timeout_s=3)
    assert "database system is ready" in str(info.value)
    assert ["podman", "logs", "--tail", "40", NAME] in fake.calls


# ---------------------------------------------------------------------------------------------
# restart / stop / remove
# ---------------------------------------------------------------------------------------------


def test_restart_clean(clock: FakeClock) -> None:
    fake = ready_rules(FakeRunner().on(("podman", "restart"), OK))
    OracleContainer(runner=fake).restart("clean")
    assert fake.calls[0] == ["podman", "restart", "-t", "10", NAME]


def test_restart_kill_then_start(clock: FakeClock) -> None:
    fake = ready_rules(FakeRunner().on(("podman", "kill"), OK).on(("podman", "start"), OK))
    OracleContainer(runner=fake).restart("kill")
    assert fake.calls[0] == ["podman", "kill", "-s", "KILL", NAME]
    assert fake.calls[1] == ["podman", "start", NAME]
    assert any(a[3] == "pg_isready" for a in fake.calls[2:])


def test_restart_rejects_unknown_mode(clock: FakeClock) -> None:
    fake = FakeRunner()
    with pytest.raises(OracleError, match="restart mode"):
        OracleContainer(runner=fake).restart("hard")
    assert fake.calls == []


def test_stop_issues_stop_and_tolerates_missing(clock: FakeClock) -> None:
    fake = FakeRunner().on(("podman", "stop"), NO_SUCH)
    OracleContainer(runner=fake).stop()
    assert fake.calls == [["podman", "stop", "-t", "10", NAME]]


def test_remove_keeps_volume_by_default() -> None:
    fake = FakeRunner()
    OracleContainer(runner=fake).remove()
    assert fake.calls == [["podman", "rm", "-f", NAME]]


def test_remove_with_volume() -> None:
    fake = FakeRunner()
    OracleContainer(runner=fake).remove(keep_volume=False)
    assert fake.calls == [["podman", "rm", "-f", NAME],
                          ["podman", "volume", "rm", "pgarbor-harness-pg18-data"]]


# ---------------------------------------------------------------------------------------------
# exec_psql / logs_tail
# ---------------------------------------------------------------------------------------------


def test_exec_psql_argv_and_stdin() -> None:
    fake = FakeRunner().on(("podman", "exec", "-i"), (0, "18.6 (Debian 18.6-1.pgdg13+2)\n", ""))
    out = OracleContainer(runner=fake).exec_psql("SHOW server_version;")
    assert out == "18.6 (Debian 18.6-1.pgdg13+2)\n"
    assert fake.calls[0] == ["podman", "exec", "-i", NAME, "psql", "-U", "postgres",
                             "-X", "-v", "ON_ERROR_STOP=1", "-At", "-d", "postgres"]
    assert fake.inputs[0] == "SHOW server_version;"


def test_exec_psql_without_tuples_only_and_other_db() -> None:
    fake = FakeRunner()
    OracleContainer(runner=fake).exec_psql("select 1", dbname="regression", tuples_only=False)
    assert fake.calls[0] == ["podman", "exec", "-i", NAME, "psql", "-U", "postgres",
                             "-X", "-v", "ON_ERROR_STOP=1", "-d", "regression"]


def test_exec_psql_failure_carries_stderr() -> None:
    fake = FakeRunner().on(("podman", "exec", "-i"), (3, "", "ERROR:  syntax error at or near \"x\""))
    with pytest.raises(OracleError, match="syntax error"):
        OracleContainer(runner=fake).exec_psql("x")


def test_logs_tail_never_raises() -> None:
    assert "unavailable" in OracleContainer(runner=ExplodingRunner()).logs_tail()
