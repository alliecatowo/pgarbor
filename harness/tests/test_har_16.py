"""HAR-16 tests: psql podman argv, pg_regress environment, runner timeout path, admin SQL.

Unit tests use fake runners and monkeypatched subprocess; no podman is executed. The live
test is marked ``oracle`` and runs only with PGARBOR_ORACLE=1.
"""

from __future__ import annotations

import subprocess
from pathlib import Path

import pytest

from pgarbor_harness import config
from pgarbor_harness.regress import psql

SRC = Path("/srv/regress")
OUT = Path("/srv/out")


@pytest.fixture(autouse=True)
def _pin_podman(monkeypatch: pytest.MonkeyPatch):
    """Argv assertions are written for podman; pin the runtime so they are deterministic."""
    monkeypatch.setenv("PGARBOR_CONTAINER_RUNTIME", "podman")
    config.container_runtime.cache_clear()
    yield
    config.container_runtime.cache_clear()


def _argv(test: str = "boolean", db: str = "regression") -> list[str]:
    return psql.psql_argv(test, "127.0.0.1", 54319, db, SRC, OUT)


def _expected_argv(name: str) -> list[str]:
    env = {
        "LANG": "C",
        "LC_MESSAGES": "C",
        "PGAPPNAME": "pg_regress/boolean",
        "PGARBOR_OUT": "/srv/out/results/boolean.out",
        "PGDATESTYLE": "Postgres, MDY",
        "PGHOST": "127.0.0.1",
        "PGPORT": "54319",
        "PGTZ": "America/Los_Angeles",
        "PGUSER": "postgres",
        "PG_ABS_BUILDDIR": "/srv/out",
        "PG_ABS_SRCDIR": "/srv/regress",
        "PG_DLSUFFIX": ".so",
        "PG_LIBDIR": "/srv/regress",
    }
    env_args: list[str] = []
    for key in sorted(env):
        env_args += ["-e", f"{key}={env[key]}"]
    return [
        "podman", "run", "--rm", "-i", "--name", name, "--network", "host",
        *env_args,
        "-v", "/srv/regress:/srv/regress:ro,Z",
        "-v", "/srv/out:/srv/out:rw,Z",
        "-w", "/srv/out", "--entrypoint", "sh", config.PG_IMAGE,
        "-c", 'exec psql "$@" > "$PGARBOR_OUT" 2>&1', "sh",
        "-X", "-a", "-q", "-d", "regression",
        "-v", "HIDE_TABLEAM=on", "-v", "HIDE_TOAST_COMPRESSION=on",
    ]


def test_psql_argv_exact():
    argv = _argv()
    name = argv[argv.index("--name") + 1]
    assert name.startswith("pgarbor-psql-boolean-")
    assert argv == _expected_argv(name)


def test_psql_argv_flags_after_image():
    argv = _argv()
    tail = argv[argv.index(config.PG_IMAGE) + 1:]
    assert tail[:2] == ["-c", 'exec psql "$@" > "$PGARBOR_OUT" 2>&1']
    assert tail[-9:] == ["-X", "-a", "-q", "-d", "regression",
                         "-v", "HIDE_TABLEAM=on", "-v", "HIDE_TOAST_COMPRESSION=on"]


def test_psql_env_contains_regress_profile():
    argv = _argv()
    pairs = [argv[i + 1] for i, a in enumerate(argv) if a == "-e"]
    envs = dict(p.split("=", 1) for p in pairs)
    assert envs["PGTZ"] == "America/Los_Angeles"
    assert envs["PGDATESTYLE"] == "Postgres, MDY"
    assert "PGOPTIONS" not in envs
    keys = [p.split("=", 1)[0] for p in pairs]
    assert keys == sorted(keys)


def test_run_psql_passes_sql_on_stdin_and_creates_results_dir(tmp_path):
    sql = tmp_path / "boolean.sql"
    sql.write_bytes(b"SELECT 1;\n")
    out_dir = tmp_path / "out"
    seen: dict = {}

    def fake(argv, stdin, timeout):
        seen["argv"], seen["stdin"], seen["timeout"] = list(argv), stdin, timeout
        return 0, ""

    run = psql.run_psql("boolean", sql, "127.0.0.1", 54319, "regression",
                        SRC, out_dir, timeout_s=12.5, runner=fake)
    assert seen["stdin"] == b"SELECT 1;\n"
    assert seen["timeout"] == 12.5
    assert seen["argv"] == psql.psql_argv("boolean", "127.0.0.1", 54319, "regression",
                                          SRC, out_dir)
    assert (out_dir / "results").is_dir()
    assert run.exit_code == 0
    assert run.timed_out is False
    assert run.out_file == (out_dir / "results" / "boolean.out").resolve()
    assert run.test == "boolean"


def test_run_psql_fake_runner_timeout_path(tmp_path):
    sql = tmp_path / "t.sql"
    sql.write_bytes(b"")

    def fake(argv, stdin, timeout):
        raise subprocess.TimeoutExpired(list(argv), timeout)

    run = psql.run_psql("t", sql, "127.0.0.1", 1, "regression", SRC, tmp_path / "o",
                        timeout_s=3.0, runner=fake)
    assert run.timed_out is True
    assert run.exit_code is None
    assert "timed out" in run.stderr


def test_run_psql_reports_podman_exit_code(tmp_path):
    sql = tmp_path / "t.sql"
    sql.write_bytes(b"\\c x\n")

    run = psql.run_psql("t", sql, "127.0.0.1", 1, "regression", SRC, tmp_path / "o",
                        runner=lambda argv, stdin, timeout: (2, "podman warning"))
    assert run.exit_code == 2
    assert run.timed_out is False
    assert run.stderr == "podman warning"


def test_default_runner_timeout_kills_named_container(monkeypatch):
    calls: list[list[str]] = []

    def fake_run(argv, **kwargs):
        calls.append(list(argv))
        if argv[0] == "podman" and argv[1] == "run":
            raise subprocess.TimeoutExpired(list(argv), kwargs.get("timeout"))
        return subprocess.CompletedProcess(argv, 0, b"", b"")

    monkeypatch.setattr(psql.subprocess, "run", fake_run)
    argv = _argv("boolean")
    name = argv[argv.index("--name") + 1]
    with pytest.raises(subprocess.TimeoutExpired):
        psql._default_runner(argv, b"select 1", 5.0)
    kill = [c for c in calls if c[:2] == ["podman", "kill"]]
    assert len(kill) == 1
    assert kill[0][2] == name


def test_run_admin_sql_argv_and_output(monkeypatch):
    seen: dict = {}

    def fake_run(argv, **kwargs):
        seen["argv"] = list(argv)
        seen["input"] = kwargs.get("input")
        return subprocess.CompletedProcess(argv, 0, b"CREATE DATABASE\n", b"")

    monkeypatch.setattr(psql.subprocess, "run", fake_run)
    code, out = psql.run_admin_sql('CREATE DATABASE "regression" TEMPLATE=template0',
                                   "127.0.0.1", 54319)
    assert code == 0
    assert out == "CREATE DATABASE\n"
    argv = seen["argv"]
    assert seen["input"] == b""
    assert argv[-4:] == ["-d", "postgres", "-c", 'CREATE DATABASE "regression" TEMPLATE=template0']
    assert "ON_ERROR_STOP=1" in argv
    assert config.PG_IMAGE in argv


@pytest.mark.oracle
def test_live_boolean_matches_expected(tmp_path):
    from pgarbor_harness.config import paths
    from pgarbor_harness.oracle.container import OracleContainer

    ep = OracleContainer().endpoint()
    regress = paths().pg_regress_dir
    code, _ = psql.run_admin_sql('DROP DATABASE IF EXISTS "regression"', ep.host, ep.port)
    assert code == 0
    code, text = psql.run_admin_sql('CREATE DATABASE "regression" TEMPLATE=template0',
                                    ep.host, ep.port)
    assert code == 0, text
    for stmt in config.REGRESS_DB_SETTINGS:
        code, text = psql.run_admin_sql(stmt.format(db="regression"), ep.host, ep.port)
        assert code == 0, text

    run = psql.run_psql("boolean", regress / "sql" / "boolean.sql", ep.host, ep.port,
                        "regression", regress, tmp_path / "out")
    assert run.timed_out is False
    assert run.exit_code == 0
    expected = (regress / "expected" / "boolean.out").read_bytes()
    assert run.out_file.read_bytes() == expected
