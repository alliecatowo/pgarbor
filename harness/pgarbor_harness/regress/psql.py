"""ORACLE-05: run ``psql`` exactly like pg_regress does, using psql FROM THE PINNED ORACLE IMAGE.

Lease HAR-16.

No local psql is required or used: psql 18.6 from ``config.PG_IMAGE`` runs in a throwaway
container on the host network so it can reach any loopback target (PG oracle on 54319 or a
PGArbor server on an ephemeral port).

pg_regress semantics reproduced (src/test/regress/pg_regress.c initialize_environment,
pg_regress_main.c psql_start_test; reference reading):
  * command: ``psql -X -a -q -d <db> -v HIDE_TABLEAM=on -v HIDE_TOAST_COMPRESSION=on``
    (config.REGRESS_PSQL_FLAGS) with the test's .sql on STDIN and stdout+stderr BOTH
    redirected to the SAME results file (``> out 2>&1``). The merge must happen INSIDE the
    container (podman/docker forward stdout/stderr as separate streams, which would reorder error
    lines relative to echoed queries), hence ``sh -c 'exec psql "$@" > "$OUT" 2>&1' sh ...``.
  * environment (config.REGRESS_ENV plus): ``PGAPPNAME=pg_regress/<testname>``,
    ``PG_ABS_SRCDIR=<regress_dir>``, ``PG_ABS_BUILDDIR=<out_dir>``, ``PG_LIBDIR=<regress_dir>``
    (no regress.so exists there; tests needing it fail identically on every target),
    ``PG_DLSUFFIX=.so``, ``PGHOST``, ``PGPORT``, ``PGUSER=postgres``, ``PGOPTIONS`` unset;
    LC_ALL/LANGUAGE unset.
  * mounts: identical-path mounts of regress_dir (ro) and out_dir (rw) — see
    oracle.container.regress_mounts — so ``\\getenv abs_srcdir`` paths are valid for psql's
    client-side ``\\copy`` AND for server-side ``COPY`` on both the oracle container (which has
    the same mounts) and a host-run PGArbor.

podman argv (exact order; unit-tested with a fake runner). ``podman`` is config.container_runtime()
and may be ``docker``; the flags are identical, so the example below shows podman::

    podman run --rm -i --network host
      -e K=V ... (sorted by K)
      -v <regress_dir>:<regress_dir>:ro,Z -v <out_dir>:<out_dir>:rw,Z
      -w <out_dir> --entrypoint sh <PG_IMAGE>
      -c 'exec psql "$@" > "$PGARBOR_OUT" 2>&1' sh
      -X -a -q -d <db> -v HIDE_TABLEAM=on -v HIDE_TOAST_COMPRESSION=on

with ``-e PGARBOR_OUT=<out_dir>/results/<test>.out``. Exit codes: psql returns 0 (normal; SQL
errors do not change it without ON_ERROR_STOP), 1 fatal psql error, 2 connection lost /
couldn't connect, 3 script error. The runner records the code; 2 is a CRASH candidate.
Timeout: config.REGRESS_TEST_TIMEOUT_S -> ``<runtime> kill`` the named container
(``--name pgarbor-psql-<test>-<pid>``; add ``--name`` right after ``-i``) and return
timed_out=True.
"""

from __future__ import annotations

import os
import re
import subprocess
import time
import uuid
from dataclasses import dataclass
from pathlib import Path
from typing import Callable, Optional, Sequence

from .. import config
from ..oracle.container import OracleError, regress_mounts, runtime

#: Result file names derive from the test name; container names must match this charset.
_UNSAFE_NAME_CHARS = re.compile(r"[^A-Za-z0-9_.-]")


@dataclass
class PsqlRun:
    test: str
    out_file: Path
    exit_code: Optional[int]
    timed_out: bool
    elapsed_ms: int
    stderr: str = ""     #: container runtime's own stderr (not psql's; psql's goes to out_file)


def psql_argv(test: str, host: str, port: int, db: str, regress_dir: Path, out_dir: Path,
              image: str = config.PG_IMAGE) -> list[str]:
    """Build the exact podman/docker argv from the module docstring."""
    src = str(regress_dir.resolve())
    out = str(out_dir.resolve())
    result_file = f"{out}/results/{test}.out"
    safe = _UNSAFE_NAME_CHARS.sub("_", test)
    name = f"pgarbor-psql-{safe}-{os.getpid()}"
    env = dict(config.REGRESS_ENV)
    env.update({
        "PGAPPNAME": f"pg_regress/{test}",
        "PGARBOR_OUT": result_file,
        "PG_ABS_SRCDIR": src,
        "PG_ABS_BUILDDIR": out,
        "PG_LIBDIR": src,
        "PG_DLSUFFIX": ".so",
        "PGHOST": host,
        "PGPORT": str(port),
        "PGUSER": config.PG_SUPERUSER,
    })
    argv: list[str] = [runtime(), "run", "--rm", "-i", "--name", name, "--network", "host"]
    for key in sorted(env):
        argv += ["-e", f"{key}={env[key]}"]
    for host_path, cont_path, opts in regress_mounts(regress_dir, out_dir):
        argv += ["-v", f"{host_path}:{cont_path}:{opts}"]
    argv += ["-w", out, "--entrypoint", "sh", image,
             "-c", 'exec psql "$@" > "$PGARBOR_OUT" 2>&1', "sh"]
    argv += list(config.REGRESS_PSQL_FLAGS[:3])  # -X -a -q
    argv += ["-d", db]
    argv += list(config.REGRESS_PSQL_FLAGS[3:])  # -v HIDE_TABLEAM=on -v HIDE_TOAST_COMPRESSION=on
    return argv


def _container_run(argv: Sequence[str], stdin: bytes, timeout: float) -> "subprocess.CompletedProcess[bytes]":
    """Run the container runtime with captured output. On timeout, ``<runtime> kill`` the named
    container and re-raise ``subprocess.TimeoutExpired``. A missing runtime binary is a harness error."""
    try:
        return subprocess.run(list(argv), input=stdin, capture_output=True,
                              timeout=timeout, check=False)
    except subprocess.TimeoutExpired:
        name = argv[argv.index("--name") + 1]
        subprocess.run([runtime(), "kill", name], capture_output=True, check=False)
        raise
    except OSError as e:
        raise OracleError(f"cannot run {argv[0]!r} (is podman or docker installed?): {e}") from e


def _default_runner(argv: Sequence[str], stdin: bytes, timeout: float) -> tuple[int, str]:
    proc = _container_run(argv, stdin, timeout)
    return proc.returncode, proc.stderr.decode("utf-8", errors="replace")


def run_psql(test: str, sql_file: Path, host: str, port: int, db: str, regress_dir: Path,
             out_dir: Path, timeout_s: float = config.REGRESS_TEST_TIMEOUT_S,
             runner: Optional[Callable[[Sequence[str], bytes, float], tuple[int, str]]] = None
             ) -> PsqlRun:
    """Run one test file. ``runner(argv, stdin_bytes, timeout) -> (exit_code, stderr)`` is
    injectable for tests; default uses subprocess.run(input=..., timeout=...). Creates
    ``out_dir/results`` first."""
    results_dir = out_dir.resolve() / "results"
    results_dir.mkdir(parents=True, exist_ok=True)
    out_file = results_dir / f"{test}.out"
    out_file.unlink(missing_ok=True)  # A previous launch must never supply this run's evidence.
    argv = psql_argv(test, host, port, db, regress_dir, out_dir)
    stdin = sql_file.read_bytes()
    run = runner if runner is not None else _default_runner
    start = time.monotonic()
    code: Optional[int]
    err = ""
    timed_out = False
    try:
        code, err = run(argv, stdin, timeout_s)
    except subprocess.TimeoutExpired:
        code, timed_out = None, True
        err = f"timed out after {timeout_s:g}s; kill issued"
    elapsed_ms = int((time.monotonic() - start) * 1000)
    return PsqlRun(test=test, out_file=out_file, exit_code=code, timed_out=timed_out,
                   elapsed_ms=elapsed_ms, stderr=err)


def run_admin_sql(sql: str, host: str, port: int, db: str = "postgres") -> tuple[int, str]:
    """Run admin SQL (e.g. CREATE DATABASE "regression") through the same image's psql with
    ``-X -q -v ON_ERROR_STOP=1 -c <sql>``; return (exit_code, combined output)."""
    name = f"pgarbor-psql-admin-{uuid.uuid4().hex[:12]}"
    argv = [runtime(), "run", "--rm", "-i", "--name", name, "--network", "host",
            "-e", f"PGHOST={host}", "-e", f"PGPORT={port}",
            "-e", f"PGUSER={config.PG_SUPERUSER}",
            "--entrypoint", "sh", config.PG_IMAGE,
            "-c", 'exec psql "$@" 2>&1', "sh",
            "-X", "-q", "-v", "ON_ERROR_STOP=1", "-d", db, "-c", sql]
    try:
        proc = _container_run(argv, b"", config.REGRESS_TEST_TIMEOUT_S)
    except subprocess.TimeoutExpired:
        # 124 follows coreutils ``timeout``: the statement never completed.
        return 124, f"admin sql timed out after {config.REGRESS_TEST_TIMEOUT_S:g}s"
    out = proc.stdout.decode("utf-8", errors="replace")
    err = proc.stderr.decode("utf-8", errors="replace")
    if err.strip():
        out += err
    return proc.returncode, out
