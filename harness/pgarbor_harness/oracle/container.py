"""ORACLE-01 / ORACLE-12: lifecycle of the ONE pinned PostgreSQL 18.6 container the harness owns.

Lease HAR-01. Uses the ``podman`` CLI via ``subprocess`` (no Python podman bindings).

Container contract
------------------
* name ``config.HARNESS_CONTAINER_NAME`` (``pgarbor-harness-pg18``); image ``config.PG_IMAGE``
  (by digest, never a tag). The developer container ``pgarbor-oracle`` is NEVER started,
  stopped, removed or modified by the harness.
* create command (exact argv order, tested by unit test with a fake runner)::

    podman run -d --name pgarbor-harness-pg18
      -p 127.0.0.1:54319:5432
      -e POSTGRES_PASSWORD=oracle -e POSTGRES_HOST_AUTH_METHOD=trust
      -e "POSTGRES_INITDB_ARGS=--encoding=UTF8 --locale-provider=builtin --locale=C.UTF-8"
      -e TZ=UTC
      -v pgarbor-harness-pg18-data:/var/lib/postgresql
      -v <regress_dir>:<regress_dir>:ro,Z           (identical host/container path, see below)
      -v <regress_out>:<regress_out>:rw,Z
      --shm-size=256m
      docker.io/library/postgres@sha256:7493...
      -c TimeZone=UTC -c DateStyle=ISO,\\ MDY ... (one ``-c k=v`` per config.PG_SERVER_GUCS,
         passed as separate argv items ``-c`` and ``k=v``; argv needs no shell quoting)
      -c max_connections=200 -c fsync=off -c synchronous_commit=off -c full_page_writes=off

  ``fsync=off`` etc. are allowed because the oracle's durability is irrelevant to the
  behaviour under test (no case may SHOW these; fingerprint records them).
* identical-path mounts: the regress suite's server-side ``COPY ... FROM :'abs_srcdir'/data/x``
  must resolve to the same absolute path on the oracle server, on a host-run PGArbor server and
  inside the psql client container (regress.psql). Therefore regress dirs are mounted at their
  host absolute paths.
* readiness: poll ``podman exec <name> pg_isready -U postgres -h 127.0.0.1`` every 0.25 s up to
  ``config.CONTAINER_READY_TIMEOUT_S``; then run ``SELECT 1`` through a real TCP connection
  (pg_isready can succeed while the docker-entrypoint temporary server is still up during
  first init; therefore require two consecutive successes 1 s apart).

Errors are raised as ``OracleError`` with the podman stderr attached. Callers classify
oracle failures as HARNESS_ERROR, never as target failures.
"""

from __future__ import annotations

import subprocess
from dataclasses import dataclass, field
from pathlib import Path
from typing import Callable, Optional, Sequence

from .. import config
from ..model import Endpoint

#: Extra server flags beyond config.PG_SERVER_GUCS (performance only; never semantics).
PERF_SERVER_FLAGS: tuple[tuple[str, str], ...] = (
    ("max_connections", "200"),
    ("fsync", "off"),
    ("synchronous_commit", "off"),
    ("full_page_writes", "off"),
)

DATA_VOLUME = f"{config.HARNESS_CONTAINER_NAME}-data"


class OracleError(RuntimeError):
    """Oracle infrastructure failure (podman missing, wrong digest, not ready, ...)."""


#: Signature of the command runner (injectable for tests): argv -> CompletedProcess.
Runner = Callable[[Sequence[str]], "subprocess.CompletedProcess[str]"]


def default_runner(argv: Sequence[str]) -> "subprocess.CompletedProcess[str]":
    """Run argv with captured text output, no shell, no check (callers inspect returncode)."""
    return subprocess.run(list(argv), capture_output=True, text=True, check=False)


@dataclass
class ContainerSpec:
    """Everything that determines the container's identity. Two specs that differ in any field
    require recreating the container (``ensure`` does that when ``recreate_on_drift``)."""

    name: str = config.HARNESS_CONTAINER_NAME
    image: str = config.PG_IMAGE
    host_port: int = config.HARNESS_CONTAINER_PORT
    initdb_args: str = config.PG_INITDB_ARGS
    server_gucs: dict[str, str] = field(default_factory=lambda: dict(config.PG_SERVER_GUCS))
    mounts: tuple[tuple[str, str, str], ...] = ()  #: (host_path, container_path, options)
    data_volume: str = DATA_VOLUME

    def run_argv(self) -> list[str]:
        """Exact ``podman run`` argv per the module docstring (implemented: frozen)."""
        argv = ["podman", "run", "-d", "--name", self.name,
                "-p", f"127.0.0.1:{self.host_port}:5432",
                "-e", f"POSTGRES_PASSWORD={config.PG_SUPERUSER_PASSWORD}",
                "-e", "POSTGRES_HOST_AUTH_METHOD=trust",
                "-e", f"POSTGRES_INITDB_ARGS={self.initdb_args}",
                "-e", "TZ=UTC",
                "-v", f"{self.data_volume}:/var/lib/postgresql"]
        for host, cont, opts in self.mounts:
            argv += ["-v", f"{host}:{cont}:{opts}"]
        argv += ["--shm-size=256m", self.image]
        for k, v in [*self.server_gucs.items(), *PERF_SERVER_FLAGS]:
            argv += ["-c", f"{k}={v}"]
        return argv


def regress_mounts(regress_dir: Path, regress_out: Path) -> tuple[tuple[str, str, str], ...]:
    """Identical-path mounts for the regress suite (implemented)."""
    rd, ro = str(regress_dir.resolve()), str(regress_out.resolve())
    return ((rd, rd, "ro,Z"), (ro, ro, "rw,Z"))


@dataclass
class ContainerStatus:
    exists: bool
    running: bool
    image_digest: Optional[str] = None
    host_port: Optional[int] = None
    mounts: tuple[str, ...] = ()
    raw_inspect: Optional[dict] = None


class OracleContainer:
    """Manage the harness's pinned PG18 container. One instance per process."""

    def __init__(self, spec: Optional[ContainerSpec] = None, runner: Runner = default_runner):
        self.spec = spec or ContainerSpec()
        self.runner = runner

    def endpoint(self, dbname: str = "postgres") -> Endpoint:
        """TCP endpoint of the container (implemented)."""
        return Endpoint(host="127.0.0.1", port=self.spec.host_port, dbname=dbname,
                        user=config.PG_SUPERUSER, password=config.PG_SUPERUSER_PASSWORD)

    def status(self) -> ContainerStatus:
        """``podman container inspect <name>`` -> ContainerStatus.

        Not found (exit code 125 / "no such container") -> ``ContainerStatus(False, False)``.
        Read ``.State.Running``, ``.ImageDigest`` (compare to config.PG_IMAGE_DIGEST),
        ``.HostConfig.PortBindings["5432/tcp"][0].HostPort``, ``.Mounts[*].Destination``.
        """
        raise NotImplementedError("HAR-01: inspect container")

    def drift(self, st: ContainerStatus) -> list[str]:
        """Human-readable reasons the existing container does not match ``self.spec``:
        digest differs, port differs, a required mount destination missing. Empty = OK."""
        raise NotImplementedError("HAR-01: compare inspect vs spec")

    def ensure(self, recreate_on_drift: bool = False) -> Endpoint:
        """Idempotently make the container exist, run, match the spec and be ready.

        * missing -> ``podman pull <image>`` if ``podman image exists`` fails, then run_argv();
        * exists but stopped -> ``podman start``;
        * drift -> raise OracleError listing drift unless ``recreate_on_drift`` (then
          ``podman rm -f`` and recreate; the data volume is kept, initdb args are only applied
          on first init, so ALSO verify via fingerprint afterwards);
        * then wait_ready(). Returns ``endpoint("postgres")``.
        """
        raise NotImplementedError("HAR-01: ensure container running")

    def wait_ready(self, timeout_s: float = config.CONTAINER_READY_TIMEOUT_S) -> None:
        """Poll per module docstring; raise OracleError("oracle not ready after N s: <log tail>")
        including the last 40 lines of ``podman logs``."""
        raise NotImplementedError("HAR-01: readiness wait")

    def stop(self) -> None:
        """``podman stop -t 10 <name>``; no error if not running."""
        raise NotImplementedError("HAR-01: stop")

    def restart(self, mode: str = "clean") -> None:
        """Server restart for ``restart`` case steps. ``clean``: ``podman restart -t 10``;
        ``kill``: ``podman kill -s KILL`` then ``podman start`` (exercises crash recovery).
        Always wait_ready() afterwards."""
        raise NotImplementedError("HAR-01: restart")

    def remove(self, keep_volume: bool = True) -> None:
        """``podman rm -f <name>``; also ``podman volume rm`` unless keep_volume."""
        raise NotImplementedError("HAR-01: remove")

    def exec_psql(self, sql: str, dbname: str = "postgres", tuples_only: bool = True) -> str:
        """Run ``podman exec -i <name> psql -U postgres -X -v ON_ERROR_STOP=1 [-At] -d db``
        with ``sql`` on stdin; return stdout. Non-zero exit -> OracleError(stderr).
        Used only for administration/fingerprinting, never for case execution."""
        raise NotImplementedError("HAR-01: psql via podman exec")

    def logs_tail(self, lines: int = 40) -> str:
        """``podman logs --tail N <name>`` stdout+stderr (best effort, never raises)."""
        raise NotImplementedError("HAR-01: logs tail")
