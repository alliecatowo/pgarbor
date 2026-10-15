"""Targets: what the runner executes cases against (reference PG, PGArbor, any PG-wire DSN).

Lease HAR-11.

CONTRACT: the ``Target`` protocol and ``CaseEnv`` below are frozen; implementations are leased.

Isolation per case (every target gives each case a pristine database):
  * PostgresTarget: oracle.pool per-case database (CREATE DATABASE ... TEMPLATE).
  * PgarborTarget: a fresh ``.pga`` file per case under ``<work_dir>/<case_db_name>.pga``,
    served by its own ``pgarbor`` server process on a free loopback port; the process is
    stopped (SIGTERM, 5 s, then SIGKILL) and the file deleted on release (kept when
    ``keep_files`` for repro packages). Commands come from ``PgarborCommand`` templates; the
    defaults match the frozen contracts/crates/pgarbor-cli.md grammar and the accepted
    contracts/amendments/harness-02-cli-serve-contract.md.
    Readiness = TCP connect + successful ``SELECT 1`` within 10 s; process exit before
    readiness -> provision raises TargetStartError (the case becomes CRASH if the reference
    ran fine, because the engine could not even start).
  * DsnTarget: an externally managed server; per-case isolation via ``CREATE DATABASE`` on an
    admin connection if ``create_databases`` else all cases share the one database
    (sequential only, jobs forced to 1; reports mark ``isolation: shared``).

Crash detection: ``is_alive(env)`` for PgarborTarget = server process still running
(``Popen.poll() is None``); the executor calls it after any connection loss to distinguish
CRASH (process died) from a server-initiated disconnect (FATAL). Stderr of the server process
is captured to ``<work_dir>/<db>.stderr.log`` and its tail attached to CRASH reports; a line
containing ``panicked at`` is a CRASH even if the process survived.

Capabilities: PostgresTarget has ``{"*"}`` (everything). PgarborTarget defaults to ``{"*"}``
minus ``missing_capabilities`` (e.g. {"server-restart"} until the adapter supports restart).
A case whose ``required_capabilities`` are not all present => BLOCKED(capability:<name>)
without execution. Capabilities are never inferred from failures.
"""

from __future__ import annotations

import contextlib
import os
import shlex
import shutil
import signal
import socket
import subprocess
import threading
import time
import uuid
from collections import deque
from dataclasses import dataclass, field, replace
from pathlib import Path
from typing import Any, Optional, Protocol, runtime_checkable

from .. import config
from ..model import Endpoint


class TargetStartError(RuntimeError):
    """The target server could not be started/provisioned for a case."""


@dataclass
class CaseEnv:
    """A provisioned, isolated environment for one case attempt."""

    case_id: str
    attempt: int
    endpoint: Endpoint
    handle: object = None          #: target-private (CaseDatabase, Popen, ...)
    work_files: list[str] = field(default_factory=list)
    warnings: list[str] = field(default_factory=list)


@runtime_checkable
class Target(Protocol):  # CONTRACT
    name: str                       #: e.g. "pg18-oracle", "pgarbor@<sha>"
    kind: str                       #: "postgres" | "pgarbor" | "dsn"

    def capabilities(self) -> frozenset[str]: ...
    def start(self) -> None: ...
    def provision(self, case_id: str, attempt: int = 1) -> CaseEnv: ...
    def release(self, env: CaseEnv, keep: bool = False) -> None: ...
    def restart(self, env: CaseEnv, mode: str) -> None: ...
    def is_alive(self, env: CaseEnv) -> bool: ...
    def server_log_tail(self, env: CaseEnv, lines: int = 40) -> str: ...
    def stop(self) -> None: ...


def has_capabilities(target: Target, required: list[str]) -> list[str]:
    """Return the required capabilities the target lacks ("*" grants all) (implemented)."""
    caps = target.capabilities()
    if "*" in caps:
        return []
    return [c for c in required if c not in caps]


# ---------------------------------------------------------------------------------------------
# Shared helpers (private)
# ---------------------------------------------------------------------------------------------

_READY_TIMEOUT_S = 10.0         #: readiness deadline per server start (spec: 10 s)
_READY_TCP_TIMEOUT_S = 0.2      #: TCP connect timeout per readiness probe
_READY_SQL_TIMEOUT_S = 2.0      #: psycopg connect timeout per readiness probe
_READY_POLL_S = 0.05
_TERM_GRACE_S = 5.0             #: release: SIGTERM -> SIGKILL after this long
_CLEAN_RESTART_EXIT_S = 10.0    #: clean restart: server must exit within this long
_CREATE_TIMEOUT_S = 60.0        #: ``pgarbor create`` budget
_LOG_TAIL_LINES = 40
_DSN_LIVENESS_S = 1.0
_ADDRESS_IN_USE = "address in use"


def _session_endpoint(host: str, port: int, dbname: str, user: str) -> Endpoint:
    """Endpoint carrying the differential session GUCs (every session of every case)."""
    return Endpoint(host=host, port=port, dbname=dbname, user=user, gucs=_session_gucs())


def _with_session_gucs(endpoint: Endpoint) -> Endpoint:
    """Copy of ``endpoint`` whose GUCs are the differential profile, overridden by the
    endpoint's own explicit GUCs."""
    merged: dict[str, str] = dict(config.DIFFERENTIAL_SESSION_GUCS)
    merged.update(dict(endpoint.gucs))
    return replace(endpoint, gucs=tuple(merged.items()))


def _session_gucs() -> tuple[tuple[str, str], ...]:
    return tuple(config.DIFFERENTIAL_SESSION_GUCS.items())


def _new_case_db_name() -> str:
    """``pgarbor_case_<pid>_<10 lowercase hex>``: same shape as oracle.pool case databases,
    with a random suffix so repeated attempts never collide."""
    return f"{config.CASE_DB_PREFIX}{os.getpid()}_{uuid.uuid4().hex[:10]}"


def _tail_text(path: Path, lines: int) -> str:
    """Last ``lines`` lines of a log file (empty string when the file is missing)."""
    try:
        with path.open("r", encoding="utf-8", errors="replace") as fh:
            return "".join(deque(fh, maxlen=max(lines, 0)))
    except FileNotFoundError:
        return ""


def _last_lines(text: str, lines: int) -> str:
    return "\n".join(text.splitlines()[-lines:])


def _unlink_quiet(path: Path) -> None:
    with contextlib.suppress(FileNotFoundError):
        path.unlink()


def _probe_sql(endpoint: Endpoint, timeout_s: float) -> None:
    """Readiness probe: psycopg connect + ``select 1``. Raises on any failure."""
    import psycopg  # lazy: this module must import with the stdlib alone

    with psycopg.connect(endpoint.conninfo(), connect_timeout=max(1, int(timeout_s))) as conn:
        row = conn.execute("select 1").fetchone()
    if row is None or row[0] != 1:
        raise RuntimeError(f"select 1 returned {row!r}")


def _pick_free_port(host: str) -> int:
    """Bind ('host', 0), read the kernel-chosen port, release it. Racy by nature; callers
    retry once on "address in use"."""
    with socket.socket(socket.AF_INET, socket.SOCK_STREAM) as sock:
        sock.bind((host, 0))
        return int(sock.getsockname()[1])


def _signal_group(proc: subprocess.Popen, sig: int) -> None:
    """Signal the server's process group (the server runs in its own session, so its pgid is
    its pid). A missing group is not an error."""
    with contextlib.suppress(ProcessLookupError):
        os.killpg(proc.pid, sig)


def _wait_quiet(proc: subprocess.Popen, timeout_s: float) -> bool:
    """True when ``proc`` exited within ``timeout_s`` (reaps it)."""
    try:
        proc.wait(timeout=timeout_s)
        return True
    except subprocess.TimeoutExpired:
        return False


def _kill_now(proc: subprocess.Popen) -> None:
    """SIGKILL the whole group and reap the leader."""
    _signal_group(proc, signal.SIGKILL)
    proc.wait()


def _stop_group(proc: subprocess.Popen, grace_s: float) -> None:
    """SIGTERM the group, SIGKILL after ``grace_s``, then sweep the group so no straggler
    outlives the leader. A pgid stays allocated while any member lives, so the sweep
    cannot hit an unrelated process."""
    if proc.poll() is None:
        _signal_group(proc, signal.SIGTERM)
        if not _wait_quiet(proc, grace_s):
            _signal_group(proc, signal.SIGKILL)
            proc.wait()  # SIGKILL cannot be ignored
    _signal_group(proc, signal.SIGKILL)


def _wait_ready(proc: subprocess.Popen, endpoint: Endpoint, log_path: Path, what: str) -> None:
    """Poll until TCP connects and ``select 1`` succeeds. Raises TargetStartError when the
    process exits first (with the log tail) or the deadline passes. The caller kills the
    process on error."""
    deadline = time.monotonic() + _READY_TIMEOUT_S
    last = "no probe completed"
    while True:
        rc = proc.poll()
        if rc is not None:
            tail = _tail_text(log_path, _LOG_TAIL_LINES).rstrip()
            msg = f"{what} exited with status {rc} before it was ready"
            raise TargetStartError(msg + (f"; log tail:\n{tail}" if tail else " (no output)"))
        try:
            with socket.create_connection((endpoint.host, endpoint.port),
                                          timeout=_READY_TCP_TIMEOUT_S):
                pass
            _probe_sql(endpoint, _READY_SQL_TIMEOUT_S)
            return
        except Exception as exc:  # any probe failure means "not ready yet"; retried below
            last = f"{type(exc).__name__}: {exc}"
        if time.monotonic() >= deadline:
            tail = _tail_text(log_path, _LOG_TAIL_LINES).rstrip()
            msg = f"{what} not ready within {_READY_TIMEOUT_S:g} s (last probe: {last})"
            raise TargetStartError(msg + (f"; log tail:\n{tail}" if tail else ""))
        time.sleep(_READY_POLL_S)


# ---------------------------------------------------------------------------------------------
# PostgresTarget
# ---------------------------------------------------------------------------------------------


class PostgresTarget:
    """The pinned PG18 oracle as a target (used by the recorder and by PG-vs-PG selftests)."""

    kind = "postgres"

    def __init__(self, pool: "object", name: str = "pg18-oracle"):
        self.pool = pool  # oracle.pool.OraclePool (typed loosely to avoid import cycles)
        self.name = name

    def capabilities(self) -> frozenset[str]:
        return frozenset({"*"})

    def start(self) -> None:
        """pool.start()."""
        pool: Any = self.pool
        try:
            pool.start()
        except Exception as exc:
            raise TargetStartError(f"{self.name}: oracle pool did not start: {exc}") from exc

    def provision(self, case_id: str, attempt: int = 1) -> CaseEnv:
        """pool.acquire -> CaseEnv(endpoint=db.endpoint, handle=db)."""
        pool: Any = self.pool
        db = pool.acquire(case_id, attempt)
        return CaseEnv(case_id=case_id, attempt=attempt, endpoint=db.endpoint, handle=db,
                       warnings=list(db.warnings))

    def release(self, env: CaseEnv, keep: bool = False) -> None:
        """pool.release(env.handle) (``keep`` ignored: PG dbs are cheap to recreate)."""
        pool: Any = self.pool
        pool.release(env.handle)

    def restart(self, env: CaseEnv, mode: str) -> None:
        """pool.container.restart(mode). Callers serialize restart cases (shared server)."""
        pool: Any = self.pool
        pool.container.restart(mode)

    def is_alive(self, env: CaseEnv) -> bool:
        """container status running."""
        pool: Any = self.pool
        return bool(pool.container.status().running)

    def server_log_tail(self, env: CaseEnv, lines: int = 40) -> str:
        """pool.container.logs_tail(lines)."""
        pool: Any = self.pool
        return str(pool.container.logs_tail(lines))

    def stop(self) -> None:
        """pool.close() (container stays up, it is pooled)."""
        pool: Any = self.pool
        pool.close()


# ---------------------------------------------------------------------------------------------
# PgarborTarget
# ---------------------------------------------------------------------------------------------


@dataclass
class PgarborCommand:
    """argv templates; ``{binary} {file} {host} {port} {branch}`` are substituted per item.

    Defaults per contracts/crates/pgarbor-cli.md (accepted harness-02 amendment):
      create: ``{binary} create {file}``
      serve:  ``{binary} serve {file} --host {host} --port {port} --branch {branch}``
    """

    binary: str = "target/release/pgarbor"
    create: tuple[str, ...] = ("{binary}", "create", "{file}")
    serve: tuple[str, ...] = ("{binary}", "serve", "{file}", "--host", "{host}",
                              "--port", "{port}", "--branch", "{branch}")
    branch: str = "main"
    host: str = "127.0.0.1"
    dbname: str = "postgres"
    user: str = "postgres"


@dataclass
class _PgarborHandle:
    """Target-private state of one PGArbor case (CaseEnv.handle)."""

    proc: subprocess.Popen
    name: str
    file_path: Path
    log_path: Path
    port: int
    released: bool = False


def _pgarbor_handle(env: CaseEnv) -> _PgarborHandle:
    handle = env.handle
    if not isinstance(handle, _PgarborHandle):
        raise TypeError(f"not a PGArbor case environment: {type(handle).__name__}")
    return handle


class PgarborTarget:
    """PGArbor server process per case (see module docstring)."""

    kind = "pgarbor"

    def __init__(self, work_dir: Path, command: Optional[PgarborCommand] = None,
                 name: str = "pgarbor", missing_capabilities: frozenset[str] = frozenset(),
                 keep_files: bool = False):
        self.work_dir = work_dir
        self.command = command or PgarborCommand()
        self.name = name
        self.missing_capabilities = missing_capabilities
        self.keep_files = keep_files
        self._binary: Optional[str] = None
        self._started = False
        self._live: dict[str, CaseEnv] = {}   #: unreleased cases, keyed by case db name

    def capabilities(self) -> frozenset[str]:
        """``{"*"}`` when nothing is missing, else ALL_KNOWN_CAPABILITIES minus the missing
        ones (implemented)."""
        if not self.missing_capabilities:
            return frozenset({"*"})
        return ALL_KNOWN_CAPABILITIES - self.missing_capabilities

    def start(self) -> None:
        """mkdir work_dir; check the binary exists and is executable (TargetStartError)."""
        try:
            self.work_dir.mkdir(parents=True, exist_ok=True)
        except OSError as exc:
            raise TargetStartError(
                f"{self.name}: cannot create work dir {self.work_dir}: {exc}") from exc
        name = self.command.binary
        if os.sep in name or (os.altsep is not None and os.altsep in name):
            candidate = Path(name).absolute()
            if not candidate.is_file():
                raise TargetStartError(f"{self.name}: pgarbor binary not found: {candidate}")
            resolved = str(candidate)
        else:
            found = shutil.which(name)
            if found is None:
                raise TargetStartError(f"{self.name}: pgarbor binary {name!r} not found on PATH")
            resolved = found
        if not os.access(resolved, os.X_OK):
            raise TargetStartError(f"{self.name}: pgarbor binary is not executable: {resolved}")
        self._binary = resolved
        self._started = True

    def provision(self, case_id: str, attempt: int = 1) -> CaseEnv:
        """create file, pick a free port (bind ('127.0.0.1', 0), read, close), spawn serve
        with stdout/stderr to the log file, wait ready, return CaseEnv (endpoint with
        config.DIFFERENTIAL_SESSION_GUCS as gucs). Retry once with a new port when the process
        exits with "address in use" in its stderr."""
        if not self._started:
            self.start()
        name = _new_case_db_name()
        file_path = self.work_dir / f"{name}.pga"
        log_path = self.work_dir / f"{name}.stderr.log"
        self._create_file(file_path)
        try:
            proc, port = self._start_ready(file_path, log_path,
                                           what=f"pgarbor serve for case {case_id!r}")
        except BaseException:
            if not self.keep_files:
                _unlink_quiet(file_path)
            raise
        handle = _PgarborHandle(proc=proc, name=name, file_path=file_path, log_path=log_path,
                                port=port)
        env = CaseEnv(case_id=case_id, attempt=attempt,
                      endpoint=self._endpoint(port), handle=handle,
                      work_files=[str(file_path), str(log_path)])
        self._live[name] = env
        return env

    def release(self, env: CaseEnv, keep: bool = False) -> None:
        """Stop the process (SIGTERM, 5 s, SIGKILL), delete .pga unless keep/keep_files."""
        handle = _pgarbor_handle(env)
        if handle.released:
            return
        _stop_group(handle.proc, _TERM_GRACE_S)
        handle.released = True
        self._live.pop(handle.name, None)
        if not (keep or self.keep_files):
            _unlink_quiet(handle.file_path)
            _unlink_quiet(handle.log_path)

    def restart(self, env: CaseEnv, mode: str) -> None:
        """clean: SIGTERM + wait exit (must exit 0 within 10 s, else note warning) then serve
        again on the same file and port; kill: SIGKILL then serve again. Wait ready."""
        handle = _pgarbor_handle(env)
        if handle.released:
            raise RuntimeError(f"{self.name}: cannot restart a released case ({env.case_id})")
        if mode not in ("clean", "kill"):
            raise ValueError(f"{self.name}: unknown restart mode {mode!r} (clean | kill)")
        proc = handle.proc
        if mode == "clean" and proc.poll() is None:
            _signal_group(proc, signal.SIGTERM)
            if not _wait_quiet(proc, _CLEAN_RESTART_EXIT_S):
                env.warnings.append(
                    f"server did not exit within {_CLEAN_RESTART_EXIT_S:g} s of SIGTERM; killed")
            elif proc.returncode != 0:
                env.warnings.append(
                    f"server exited with status {proc.returncode} on clean restart")
        _kill_now(proc)  # kill: the server; clean: sweeps stragglers, no-op otherwise
        new_proc = self._spawn_serve(handle.file_path, handle.port, handle.log_path, append=True)
        try:
            _wait_ready(new_proc, self._endpoint(handle.port), handle.log_path,
                        what=f"pgarbor serve after {mode} restart for case {env.case_id!r}")
        except TargetStartError:
            _kill_now(new_proc)
            raise
        handle.proc = new_proc

    def is_alive(self, env: CaseEnv) -> bool:
        return _pgarbor_handle(env).proc.poll() is None

    def server_log_tail(self, env: CaseEnv, lines: int = 40) -> str:
        return _tail_text(_pgarbor_handle(env).log_path, lines)

    def stop(self) -> None:
        """Release all still-running processes."""
        first: Optional[BaseException] = None
        for env in list(self._live.values()):
            try:
                self.release(env)
            except Exception as exc:  # keep releasing the rest; re-raise the first failure
                if first is None:
                    first = exc
        if first is not None:
            raise first

    # -- private ------------------------------------------------------------------------------

    def _endpoint(self, port: int) -> Endpoint:
        return _session_endpoint(self.command.host, port, self.command.dbname, self.command.user)

    def _argv(self, template: tuple[str, ...], file_path: Path, port: Optional[int]) -> list[str]:
        fields = {
            "binary": self._binary or self.command.binary,
            "file": str(file_path),
            "host": self.command.host,
            "port": "" if port is None else str(port),
            "branch": self.command.branch,
        }
        argv: list[str] = []
        for part in template:
            try:
                argv.append(part.format(**fields))
            except (KeyError, IndexError, ValueError) as exc:
                raise TargetStartError(
                    f"{self.name}: invalid command template {part!r}: {exc!r}") from exc
        return argv

    def _create_file(self, file_path: Path) -> None:
        argv = self._argv(self.command.create, file_path, None)
        try:
            res = subprocess.run(argv, stdin=subprocess.DEVNULL, capture_output=True,
                                 timeout=_CREATE_TIMEOUT_S, check=False)
        except subprocess.TimeoutExpired as exc:
            raise TargetStartError(
                f"{self.name}: `{shlex.join(argv)}` timed out after {_CREATE_TIMEOUT_S:g} s"
            ) from exc
        except OSError as exc:
            raise TargetStartError(f"{self.name}: cannot run `{shlex.join(argv)}`: {exc}") from exc
        if res.returncode != 0 or not file_path.is_file():
            err = _last_lines(res.stderr.decode("utf-8", "replace"), _LOG_TAIL_LINES)
            msg = f"{self.name}: create failed (status {res.returncode}) for {file_path.name}"
            raise TargetStartError(msg + (f"; stderr tail:\n{err}" if err else ""))

    def _spawn_serve(self, file_path: Path, port: int, log_path: Path,
                     *, append: bool) -> subprocess.Popen:
        argv = self._argv(self.command.serve, file_path, port)
        try:
            with log_path.open("ab" if append else "wb") as log:
                # The child keeps its own copy of the descriptor; ours closes with the block.
                return subprocess.Popen(argv, stdin=subprocess.DEVNULL, stdout=log, stderr=log,
                                        start_new_session=True)
        except OSError as exc:
            raise TargetStartError(f"{self.name}: cannot start `{shlex.join(argv)}`: {exc}") from exc

    def _start_ready(self, file_path: Path, log_path: Path,
                     what: str) -> tuple[subprocess.Popen, int]:
        """Spawn serve on a free port and wait ready; retry once on a new port when the first
        attempt reports "address in use"."""
        tries = 0
        while True:
            port = _pick_free_port(self.command.host)
            proc = self._spawn_serve(file_path, port, log_path, append=False)
            try:
                _wait_ready(proc, self._endpoint(port), log_path, what)
                return proc, port
            except TargetStartError:
                _kill_now(proc)
                tries += 1
                if tries >= 2 or _ADDRESS_IN_USE not in _tail_text(log_path, _LOG_TAIL_LINES).lower():
                    raise


# ---------------------------------------------------------------------------------------------
# DsnTarget
# ---------------------------------------------------------------------------------------------


@dataclass
class _DsnHandle:
    """Target-private state of one DSN case (CaseEnv.handle)."""

    name: Optional[str]      #: per-case database this target created; None when shared
    dropped: bool = False


def _dsn_handle(env: CaseEnv) -> _DsnHandle:
    handle = env.handle
    if not isinstance(handle, _DsnHandle):
        raise TypeError(f"not a DSN case environment: {type(handle).__name__}")
    return handle


class DsnTarget:
    """Externally managed PG-wire server (see module docstring)."""

    kind = "dsn"

    def __init__(self, endpoint: Endpoint, name: str = "dsn", create_databases: bool = True,
                 capabilities: frozenset[str] = frozenset({"*"})):
        self.endpoint = endpoint
        self.name = name
        self.create_databases = create_databases
        self._caps = capabilities
        self._admin: Any = None                 #: cached autocommit admin connection
        self._lock = threading.Lock()
        self._started = False

    def capabilities(self) -> frozenset[str]:
        """The configured capabilities. ``"*"`` expands to every known capability except
        ``server-restart``: this adapter cannot restart the server, so restart cases must be
        BLOCKED unless the caller lists the capability explicitly."""
        if "*" in self._caps:
            return ALL_KNOWN_CAPABILITIES - {"server-restart"}
        return self._caps

    def start(self) -> None:
        """Verify the server answers ``select 1`` (through the admin database when
        ``create_databases``). Idempotent."""
        if self._started:
            return
        probe = self._admin_endpoint() if self.create_databases else self.endpoint
        try:
            _probe_sql(probe, float(config.CONNECT_TIMEOUT_S))
        except Exception as exc:
            raise TargetStartError(
                f"{self.name}: cannot reach {probe.host}:{probe.port} (db {probe.dbname}): {exc}"
            ) from exc
        self._started = True

    def provision(self, case_id: str, attempt: int = 1) -> CaseEnv:
        """A fresh database via CREATE DATABASE when ``create_databases``; otherwise the one
        shared database (warning attached)."""
        if not self._started:
            self.start()
        base = _with_session_gucs(self.endpoint)
        if not self.create_databases:
            return CaseEnv(case_id=case_id, attempt=attempt, endpoint=base,
                           handle=_DsnHandle(name=None),
                           warnings=["isolation: shared (all cases use one database)"])
        db_name = _new_case_db_name()
        try:
            self._admin_ddl("CREATE DATABASE {}", db_name)
        except Exception as exc:
            raise TargetStartError(f"{self.name}: CREATE DATABASE {db_name} failed: {exc}") from exc
        return CaseEnv(case_id=case_id, attempt=attempt, endpoint=replace(base, dbname=db_name),
                       handle=_DsnHandle(name=db_name))

    def release(self, env: CaseEnv, keep: bool = False) -> None:
        """DROP DATABASE ... WITH (FORCE) for a database this target created (kept when
        ``keep``, for repro packages)."""
        handle = _dsn_handle(env)
        if handle.name is None or handle.dropped or keep:
            return
        try:
            self._admin_ddl("DROP DATABASE {} WITH (FORCE)", handle.name)
        except Exception as exc:
            raise RuntimeError(f"{self.name}: DROP DATABASE {handle.name} failed: {exc}") from exc
        handle.dropped = True

    def restart(self, env: CaseEnv, mode: str) -> None:
        """Not supported: raise TargetStartError (cases needing restart are BLOCKED because
        DsnTarget never advertises 'server-restart' unless explicitly configured)."""
        raise TargetStartError(
            f"{self.name}: restart is not supported by a DSN target (mode {mode!r})")

    def is_alive(self, env: CaseEnv) -> bool:
        """TCP connect to the server."""
        try:
            with socket.create_connection((env.endpoint.host, env.endpoint.port),
                                          timeout=_DSN_LIVENESS_S):
                return True
        except OSError:
            return False

    def server_log_tail(self, env: CaseEnv, lines: int = 40) -> str:
        return ""

    def stop(self) -> None:
        """Close the admin connection. Databases still registered to live cases are not
        dropped here; each case's release drops its own."""
        with self._lock:
            self._close_admin()
        self._started = False

    # -- private ------------------------------------------------------------------------------

    def _admin_endpoint(self) -> Endpoint:
        return self.endpoint.with_db("postgres")

    def _close_admin(self) -> None:
        conn, self._admin = self._admin, None
        if conn is not None:
            with contextlib.suppress(Exception):
                conn.close()

    def _admin_ddl(self, template: str, db_name: str) -> None:
        """Run one DDL statement, with the identifier quoted, on the cached autocommit admin
        connection (reconnect once on connection loss)."""
        import psycopg  # lazy: this module must import with the stdlib alone
        from psycopg import sql

        query = sql.SQL(template).format(sql.Identifier(db_name))
        with self._lock:
            for attempt in (1, 2):
                try:
                    if self._admin is None:
                        self._admin = psycopg.connect(
                            self._admin_endpoint().conninfo(), autocommit=True,
                            connect_timeout=config.CONNECT_TIMEOUT_S)
                    self._admin.execute(query)
                    return
                except psycopg.OperationalError:
                    self._close_admin()
                    if attempt == 2:
                        raise


#: Every capability name used by the corpus. Adding one is a harness amendment.
ALL_KNOWN_CAPABILITIES: frozenset[str] = frozenset({
    "create-table", "insert", "select", "update", "delete", "transactions", "savepoints",
    "multi-session", "extended-protocol", "copy", "server-restart", "sequences", "indexes",
    "constraints", "views", "jsonb", "arrays", "numeric", "datetime", "catalog", "roles",
    "create-database", "prepared-statements", "cursors", "notify",
})
