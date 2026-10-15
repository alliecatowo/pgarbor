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
    container (podman forwards stdout/stderr as separate streams, which would reorder error
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

podman argv (exact order; unit-tested with a fake runner)::

    podman run --rm -i --network host
      -e K=V ... (sorted by K)
      -v <regress_dir>:<regress_dir>:ro,Z -v <out_dir>:<out_dir>:rw,Z
      -w <out_dir> --entrypoint sh <PG_IMAGE>
      -c 'exec psql "$@" > "$PGARBOR_OUT" 2>&1' sh
      -X -a -q -d <db> -v HIDE_TABLEAM=on -v HIDE_TOAST_COMPRESSION=on

with ``-e PGARBOR_OUT=<out_dir>/results/<test>.out``. Exit codes: psql returns 0 (normal; SQL
errors do not change it without ON_ERROR_STOP), 1 fatal psql error, 2 connection lost /
couldn't connect, 3 script error. The runner records the code; 2 is a CRASH candidate.
Timeout: config.REGRESS_TEST_TIMEOUT_S -> ``podman kill`` the named container
(``--name pgarbor-psql-<test>-<pid>``; add ``--name`` right after ``-i``) and return
timed_out=True.
"""

from __future__ import annotations

from dataclasses import dataclass
from pathlib import Path
from typing import Callable, Optional, Sequence

from .. import config


@dataclass
class PsqlRun:
    test: str
    out_file: Path
    exit_code: Optional[int]
    timed_out: bool
    elapsed_ms: int
    stderr: str = ""     #: podman's own stderr (not psql's; psql's goes to out_file)


def psql_argv(test: str, host: str, port: int, db: str, regress_dir: Path, out_dir: Path,
              image: str = config.PG_IMAGE) -> list[str]:
    """Build the exact podman argv from the module docstring."""
    raise NotImplementedError("HAR-16: psql podman argv")


def run_psql(test: str, sql_file: Path, host: str, port: int, db: str, regress_dir: Path,
             out_dir: Path, timeout_s: float = config.REGRESS_TEST_TIMEOUT_S,
             runner: Optional[Callable[[Sequence[str], bytes, float], tuple[int, str]]] = None
             ) -> PsqlRun:
    """Run one test file. ``runner(argv, stdin_bytes, timeout) -> (exit_code, stderr)`` is
    injectable for tests; default uses subprocess.run(input=..., timeout=...). Creates
    ``out_dir/results`` first."""
    raise NotImplementedError("HAR-16: run psql in oracle image")


def run_admin_sql(sql: str, host: str, port: int, db: str = "postgres") -> tuple[int, str]:
    """Run admin SQL (e.g. CREATE DATABASE "regression") through the same image's psql with
    ``-X -q -v ON_ERROR_STOP=1 -c <sql>``; return (exit_code, combined output)."""
    raise NotImplementedError("HAR-16: admin sql via image psql")
