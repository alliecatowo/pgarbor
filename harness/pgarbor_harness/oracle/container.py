"""ORACLE-01 / ORACLE-12: lifecycle of the ONE pinned PostgreSQL 18.6 container the harness owns.

Lease HAR-01. Uses the ``podman`` or ``docker`` CLI (``config.container_runtime()``) via
``subprocess`` (no Python container bindings). The examples below show podman; docker takes
the same flags.

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

Errors are raised as ``OracleError`` with the podman/docker stderr attached. Callers classify
oracle failures as HARNESS_ERROR, never as target failures.
"""

from __future__ import annotations

import json
import subprocess
import time
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

#: The architects' developer container. The harness never passes this name to podman or docker.
FORBIDDEN_CONTAINER_NAME = "pgarbor-oracle"

#: podman/docker stderr fragments meaning "no such container" / "no such volume".
_NO_SUCH_CONTAINER: tuple[str, ...] = ("no such container", "no container with name or id")
_NO_SUCH_VOLUME: tuple[str, ...] = ("no such volume", "no volume with name")


class OracleError(RuntimeError):
    """Oracle infrastructure failure (container runtime missing, wrong digest, not ready, ...)."""



def runtime() -> str:
    """``config.container_runtime()`` ('podman' or 'docker'), raised as OracleError when absent."""
    try:
        return config.container_runtime()
    except RuntimeError as e:
        raise OracleError(str(e)) from e

#: Signature of the command runner (injectable for tests): argv -> CompletedProcess.
#: The optional ``input_text`` keyword carries stdin and is passed only by ``exec_psql``.
Runner = Callable[..., "subprocess.CompletedProcess[str]"]


def default_runner(argv: Sequence[str], input_text: Optional[str] = None) -> "subprocess.CompletedProcess[str]":
    """Run argv with captured text output, no shell, no check (callers inspect returncode).
    ``input_text`` (optional) is written to the process stdin."""
    return subprocess.run(list(argv), input=input_text, capture_output=True, text=True, check=False)


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
        """Exact ``podman run`` (or docker run) argv per the module docstring (implemented: frozen)."""
        argv = [runtime(), "run", "-d", "--name", self.name,
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

    # -- private plumbing ----------------------------------------------------------------

    def _run(self, argv: Sequence[str], input_text: Optional[str] = None) -> "subprocess.CompletedProcess[str]":
        """Every podman/docker call goes through here: argv list only, and the developer container
        name is refused before the runner is ever invoked."""
        argv = list(argv)
        f = FORBIDDEN_CONTAINER_NAME
        for tok in argv:
            if tok == f or tok.startswith(f + ":") or tok.endswith("=" + f):
                raise OracleError(f"refusing to run the container runtime against {f!r}: that container belongs "
                                  "to the architects and is never managed by the harness")
        try:
            if input_text is None:
                return self.runner(argv)
            return self.runner(argv, input_text=input_text)
        except OSError as e:
            raise OracleError(f"cannot run {argv[0]!r} (is podman or docker installed?): {e}") from e

    @staticmethod
    def _require_ok(proc: "subprocess.CompletedProcess[str]", what: str) -> "subprocess.CompletedProcess[str]":
        if proc.returncode != 0:
            raise OracleError(f"{what} failed (exit {proc.returncode}): {(proc.stderr or '').strip()}")
        return proc

    @staticmethod
    def _stderr_has(proc: "subprocess.CompletedProcess[str]", needles: tuple[str, ...]) -> bool:
        err = (proc.stderr or "").lower()
        return any(n in err for n in needles)

    def _create(self) -> None:
        """Pull the pinned image if it is absent locally, then ``podman run`` the spec."""
        if self._run([runtime(), "image", "exists", self.spec.image]).returncode != 0:
            self._require_ok(self._run([runtime(), "pull", self.spec.image]), "podman pull")
        self._require_ok(self._run(self.spec.run_argv()), "podman run")

    def _probe(self) -> bool:
        """One readiness probe: pg_isready over TCP, then SELECT 1 over TCP, both inside the
        container. During the docker-entrypoint temporary server TCP is not listening, so the
        probe fails until the real server is up."""
        name, user = self.spec.name, config.PG_SUPERUSER
        ready = self._run([runtime(), "exec", name, "pg_isready", "-U", user, "-h", "127.0.0.1"])
        if ready.returncode != 0:
            return False
        q = self._run([runtime(), "exec", name, "psql", "-h", "127.0.0.1", "-U", user,
                       "-X", "-At", "-v", "ON_ERROR_STOP=1", "-c", "select 1", "-d", "postgres"])
        return q.returncode == 0 and q.stdout.strip() == "1"

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
        proc = self._run([runtime(), "container", "inspect", self.spec.name])
        if proc.returncode != 0:
            if self._stderr_has(proc, _NO_SUCH_CONTAINER):
                return ContainerStatus(exists=False, running=False)
            raise OracleError(f"podman container inspect failed (exit {proc.returncode}): "
                              f"{(proc.stderr or '').strip()}")
        try:
            data = json.loads(proc.stdout or "")
        except json.JSONDecodeError as e:
            raise OracleError(f"podman container inspect returned invalid JSON: {e}") from e
        if not isinstance(data, list) or not data or not isinstance(data[0], dict):
            raise OracleError("podman container inspect returned an unexpected document "
                              "(expected a one-element list)")
        obj = data[0]
        state = obj.get("State") or {}
        bindings = ((obj.get("HostConfig") or {}).get("PortBindings") or {}).get("5432/tcp") or []
        host_port: Optional[int] = None
        if bindings and isinstance(bindings[0], dict):
            try:
                host_port = int(bindings[0].get("HostPort") or "")
            except (TypeError, ValueError):
                host_port = None
        mounts = tuple(m["Destination"] for m in (obj.get("Mounts") or [])
                       if isinstance(m, dict) and isinstance(m.get("Destination"), str))
        digest = obj.get("ImageDigest")
        return ContainerStatus(
            exists=True,
            running=bool(state.get("Running")),
            image_digest=digest if isinstance(digest, str) else None,
            host_port=host_port,
            mounts=mounts,
            raw_inspect=obj,
        )

    def drift(self, st: ContainerStatus) -> list[str]:
        """Human-readable reasons the existing container does not match ``self.spec``:
        digest differs, port differs, a required mount destination missing. Empty = OK."""
        if not st.exists:
            return ["container does not exist"]
        reasons: list[str] = []
        if st.image_digest != config.PG_IMAGE_DIGEST:
            reasons.append(f"image digest {st.image_digest or '<none>'} != pinned "
                           f"{config.PG_IMAGE_DIGEST}")
        if st.host_port != self.spec.host_port:
            reasons.append(f"host port {st.host_port} != {self.spec.host_port}")
        present = set(st.mounts)
        for _host, cont, _opts in self.spec.mounts:
            if cont not in present:
                reasons.append(f"mount {cont} missing from container Mounts")
        return reasons

    def ensure(self, recreate_on_drift: bool = False) -> Endpoint:
        """Idempotently make the container exist, run, match the spec and be ready.

        * missing -> ``podman pull <image>`` if ``podman image exists`` fails, then run_argv();
        * exists but stopped -> ``podman start``;
        * drift -> raise OracleError listing drift unless ``recreate_on_drift`` (then
          ``podman rm -f`` and recreate; the data volume is kept, initdb args are only applied
          on first init, so ALSO verify via fingerprint afterwards);
        * then wait_ready(). Returns ``endpoint("postgres")``.
        """
        name = self.spec.name
        st = self.status()
        if st.exists:
            reasons = self.drift(st)
            if reasons:
                detail = "; ".join(reasons)
                if not recreate_on_drift:
                    raise OracleError(
                        f"container {name} does not match the pinned spec: {detail} "
                        "(pass recreate_on_drift=True to recreate it; the data volume is kept)")
                # The data volume is not removed; initdb args only apply on first init, so the
                # caller must verify the fingerprint after recreation (oracle.fingerprint).
                self._require_ok(self._run([runtime(), "rm", "-f", name]),
                                 f"podman rm -f {name} (drift recreate)")
                st = ContainerStatus(exists=False, running=False)
        if not st.exists:
            self._create()
        elif not st.running:
            self._require_ok(self._run([runtime(), "start", name]), "podman start")
        self.wait_ready()
        return self.endpoint("postgres")

    def wait_ready(self, timeout_s: float = config.CONTAINER_READY_TIMEOUT_S) -> None:
        """Poll per module docstring; raise OracleError("oracle not ready after N s: <log tail>")
        including the last 40 lines of ``podman logs``."""
        deadline = time.monotonic() + timeout_s
        streak = 0
        while True:
            if self._probe():
                streak += 1
                if streak >= 2:
                    return
                time.sleep(1.0)  # the second success must come 1 s after the first
                continue
            streak = 0
            if time.monotonic() >= deadline:
                raise OracleError(f"oracle not ready after {timeout_s:g} s: {self.logs_tail(40)}")
            time.sleep(0.25)

    def stop(self) -> None:
        """``podman stop -t 10 <name>``; no error if not running."""
        proc = self._run([runtime(), "stop", "-t", "10", self.spec.name])
        if proc.returncode == 0 or self._stderr_has(proc, _NO_SUCH_CONTAINER) \
                or self._stderr_has(proc, ("is not running",)):
            return
        self._require_ok(proc, "podman stop")

    def restart(self, mode: str = "clean") -> None:
        """Server restart for ``restart`` case steps. ``clean``: ``podman restart -t 10``;
        ``kill``: ``podman kill -s KILL`` then ``podman start`` (exercises crash recovery).
        Always wait_ready() afterwards."""
        if mode not in ("clean", "kill"):
            raise OracleError(f"unknown restart mode {mode!r} (expected 'clean' or 'kill')")
        name = self.spec.name
        if mode == "clean":
            self._require_ok(self._run([runtime(), "restart", "-t", "10", name]), "podman restart")
        else:
            self._require_ok(self._run([runtime(), "kill", "-s", "KILL", name]), "podman kill")
            self._require_ok(self._run([runtime(), "start", name]), "podman start")
        self.wait_ready()

    def remove(self, keep_volume: bool = True) -> None:
        """``podman rm -f <name>``; also ``podman volume rm`` unless keep_volume."""
        proc = self._run([runtime(), "rm", "-f", self.spec.name])
        if proc.returncode != 0 and not self._stderr_has(proc, _NO_SUCH_CONTAINER):
            self._require_ok(proc, "podman rm -f")
        if not keep_volume:
            vproc = self._run([runtime(), "volume", "rm", self.spec.data_volume])
            if vproc.returncode != 0 and not self._stderr_has(vproc, _NO_SUCH_VOLUME):
                self._require_ok(vproc, "podman volume rm")

    def exec_psql(self, sql: str, dbname: str = "postgres", tuples_only: bool = True) -> str:
        """Run ``podman exec -i <name> psql -U postgres -X -v ON_ERROR_STOP=1 [-At] -d db``
        with ``sql`` on stdin; return stdout. Non-zero exit -> OracleError(stderr).
        Used only for administration/fingerprinting, never for case execution."""
        argv = [runtime(), "exec", "-i", self.spec.name, "psql", "-U", config.PG_SUPERUSER,
                "-X", "-v", "ON_ERROR_STOP=1"]
        if tuples_only:
            argv.append("-At")
        argv += ["-d", dbname]
        proc = self._run(argv, input_text=sql)
        if proc.returncode != 0:
            raise OracleError(f"psql in {self.spec.name} failed (exit {proc.returncode}): "
                              f"{(proc.stderr or '').strip()}")
        return proc.stdout or ""

    def logs_tail(self, lines: int = 40) -> str:
        """``podman logs --tail N <name>`` stdout+stderr (best effort, never raises)."""
        try:
            proc = self._run([runtime(), "logs", "--tail", str(lines), self.spec.name])
            return ((proc.stdout or "") + (proc.stderr or "")).rstrip("\n")
        except Exception as e:  # best effort by contract: diagnostics must not mask the error
            return f"<podman logs unavailable: {e}>"
