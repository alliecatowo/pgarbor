"""Targets: what the runner executes cases against (reference PG, PGArbor, any PG-wire DSN).

Lease HAR-11.

CONTRACT: the ``Target`` protocol and ``CaseEnv`` below are frozen; implementations are leased.

Isolation per case (every target gives each case a pristine database):
  * PostgresTarget: oracle.pool per-case database (CREATE DATABASE ... TEMPLATE).
  * PgarborTarget: a fresh ``.pga`` file per case under ``<work_dir>/<case_db_name>.pga``,
    served by its own ``pgarbor`` server process on a free loopback port; the process is
    stopped (SIGTERM, 5 s, then SIGKILL) and the file deleted on release (kept when
    ``keep_files`` for repro packages). Commands come from ``PgarborCommand`` templates; the
    defaults assume the pgarbor-cli contract in
    contracts/amendments/harness-02-cli-serve-contract.md until pgarbor-cli freezes its own.
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

import subprocess
from dataclasses import dataclass, field
from pathlib import Path
from typing import Optional, Protocol, runtime_checkable

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
        raise NotImplementedError("HAR-11: postgres target start")

    def provision(self, case_id: str, attempt: int = 1) -> CaseEnv:
        """pool.acquire -> CaseEnv(endpoint=db.endpoint, handle=db)."""
        raise NotImplementedError("HAR-11: postgres target provision")

    def release(self, env: CaseEnv, keep: bool = False) -> None:
        """pool.release(env.handle) (``keep`` ignored: PG dbs are cheap to recreate)."""
        raise NotImplementedError("HAR-11: postgres target release")

    def restart(self, env: CaseEnv, mode: str) -> None:
        """pool.container.restart(mode). Callers serialize restart cases (shared server)."""
        raise NotImplementedError("HAR-11: postgres target restart")

    def is_alive(self, env: CaseEnv) -> bool:
        """container status running."""
        raise NotImplementedError("HAR-11: postgres target liveness")

    def server_log_tail(self, env: CaseEnv, lines: int = 40) -> str:
        """pool.container.logs_tail(lines)."""
        raise NotImplementedError("HAR-11: postgres target logs")

    def stop(self) -> None:
        """pool.close() (container stays up, it is pooled)."""
        raise NotImplementedError("HAR-11: postgres target stop")


@dataclass
class PgarborCommand:
    """argv templates; ``{binary} {file} {host} {port} {branch}`` are substituted per item.

    Defaults per contracts/amendments/harness-02-cli-serve-contract.md:
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
        self._procs: dict[str, subprocess.Popen] = {}

    def capabilities(self) -> frozenset[str]:
        """``{"*"}`` when nothing is missing, else ALL_KNOWN_CAPABILITIES minus the missing
        ones (implemented)."""
        if not self.missing_capabilities:
            return frozenset({"*"})
        return ALL_KNOWN_CAPABILITIES - self.missing_capabilities

    def start(self) -> None:
        """mkdir work_dir; check the binary exists and is executable (TargetStartError)."""
        raise NotImplementedError("HAR-11: pgarbor target start")

    def provision(self, case_id: str, attempt: int = 1) -> CaseEnv:
        """create file, pick a free port (bind ('127.0.0.1', 0), read, close), spawn serve
        with stdout/stderr to the log file, wait ready, return CaseEnv (endpoint with
        config.DIFFERENTIAL_SESSION_GUCS as gucs). Retry once with a new port when the process
        exits with "address in use" in its stderr."""
        raise NotImplementedError("HAR-11: pgarbor target provision")

    def release(self, env: CaseEnv, keep: bool = False) -> None:
        """Stop the process (SIGTERM, 5 s, SIGKILL), delete .pga unless keep/keep_files."""
        raise NotImplementedError("HAR-11: pgarbor target release")

    def restart(self, env: CaseEnv, mode: str) -> None:
        """clean: SIGTERM + wait exit (must exit 0 within 10 s, else note warning) then serve
        again on the same file and port; kill: SIGKILL then serve again. Wait ready."""
        raise NotImplementedError("HAR-11: pgarbor target restart")

    def is_alive(self, env: CaseEnv) -> bool:
        raise NotImplementedError("HAR-11: pgarbor liveness")

    def server_log_tail(self, env: CaseEnv, lines: int = 40) -> str:
        raise NotImplementedError("HAR-11: pgarbor log tail")

    def stop(self) -> None:
        """Release all still-running processes."""
        raise NotImplementedError("HAR-11: pgarbor target stop")


class DsnTarget:
    """Externally managed PG-wire server (see module docstring)."""

    kind = "dsn"

    def __init__(self, endpoint: Endpoint, name: str = "dsn", create_databases: bool = True,
                 capabilities: frozenset[str] = frozenset({"*"})):
        self.endpoint = endpoint
        self.name = name
        self.create_databases = create_databases
        self._caps = capabilities

    def capabilities(self) -> frozenset[str]:
        return self._caps

    def start(self) -> None:
        raise NotImplementedError("HAR-11: dsn target start")

    def provision(self, case_id: str, attempt: int = 1) -> CaseEnv:
        raise NotImplementedError("HAR-11: dsn target provision")

    def release(self, env: CaseEnv, keep: bool = False) -> None:
        raise NotImplementedError("HAR-11: dsn target release")

    def restart(self, env: CaseEnv, mode: str) -> None:
        """Not supported: raise TargetStartError (cases needing restart are BLOCKED because
        DsnTarget never advertises 'server-restart' unless explicitly configured)."""
        raise NotImplementedError("HAR-11: dsn target restart")

    def is_alive(self, env: CaseEnv) -> bool:
        raise NotImplementedError("HAR-11: dsn liveness (TCP connect)")

    def server_log_tail(self, env: CaseEnv, lines: int = 40) -> str:
        return ""

    def stop(self) -> None:
        raise NotImplementedError("HAR-11: dsn target stop")


#: Every capability name used by the corpus. Adding one is a harness amendment.
ALL_KNOWN_CAPABILITIES: frozenset[str] = frozenset({
    "create-table", "insert", "select", "update", "delete", "transactions", "savepoints",
    "multi-session", "extended-protocol", "copy", "server-restart", "sequences", "indexes",
    "constraints", "views", "jsonb", "arrays", "numeric", "datetime", "catalog", "roles",
    "create-database", "prepared-statements", "cursors", "notify",
})
