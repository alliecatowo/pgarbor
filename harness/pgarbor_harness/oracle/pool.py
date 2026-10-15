"""ORACLE-12: pooled single oracle instance with per-case database isolation.

Lease HAR-02.

Design
------
One container (oracle.container) serves a whole campaign. Isolation per case is a fresh
database cloned from a template::

    -- once per pool start (idempotent):
    CREATE DATABASE pgarbor_case_template TEMPLATE template0
        ENCODING 'UTF8' LOCALE_PROVIDER builtin BUILTIN_LOCALE 'C.UTF-8';
    -- (no objects inside; the template exists so future fixtures can be preloaded and so that
    --  'template0' is never modified)
    -- per case:
    CREATE DATABASE "pgarbor_case_<12 hex>" TEMPLATE pgarbor_case_template;
    ...run case sessions against that db...
    DROP DATABASE "pgarbor_case_<12 hex>" WITH (FORCE);

Database names: ``case_db_name`` = ``pgarbor_case_<pid>_<10 hex>`` where pid is the creating
process id and the hex is ``sha256(f"{case_id}:{attempt}:{pid}")[:10]``. The pid lets
concurrent harness processes (a campaign plus an ad-hoc recorder/minimizer) share the oracle:
each only garbage-collects databases of DEAD processes. ``CREATE DATABASE`` cannot run inside a transaction block, so admin
statements use an autocommit admin connection to db ``postgres`` (psycopg imported lazily
inside methods; this module must import with the stdlib only).

Global-object hygiene: roles, tablespaces and database-level settings are cluster-global and
are NOT isolated by per-case databases. After each case ``check_globals`` compares
``SELECT rolname FROM pg_roles ORDER BY 1`` and ``SELECT datname FROM pg_database`` against
the snapshot taken at pool start; leaked roles created by a case are dropped
(``DROP OWNED BY r CASCADE`` in the case db before dropping it is not possible after the drop,
so: drop the case db first, then ``DROP ROLE``) and reported as a warning attached to the
case (``globals_leaked``). Cases that need roles should create them with names prefixed
``pgarbor_case_role_``; leaked roles with other names are still dropped but the case is
flagged INVALID_CASE(reason="global_object_leak") by the differential driver.

Concurrency: ``max_parallel`` databases may exist at once (default 8); ``acquire`` blocks on a
``threading.BoundedSemaphore``. Thread-safe. ``start`` drops stale case databases left by
crashed runs: only names matching ``^pgarbor_case_(\\d+)_[0-9a-f]{10}$`` whose pid is not a
live process (``os.kill(pid, 0)`` raises ProcessLookupError) — never the template, never a
live process's databases.
"""

from __future__ import annotations

import dataclasses
import os
import re
import threading
import time
from contextlib import contextmanager
from dataclasses import dataclass, field
from typing import Any, Iterator, Optional

from .. import config
from ..model import Endpoint
from .container import OracleContainer, OracleError
from .fingerprint import verify_live

#: Retries of CREATE DATABASE ... TEMPLATE when the template is busy (SQLSTATE 55006).
_CREATE_RETRIES = 5
#: Backoff between CREATE DATABASE retries, seconds.
_CREATE_BACKOFF_S = 0.2
#: SQLSTATE: object_in_use / "source database is being accessed by other users".
_SQLSTATE_OBJECT_IN_USE = "55006"
#: SQLSTATE: duplicate_database.
_SQLSTATE_DUPLICATE_DATABASE = "42P04"

#: Matches a case database name and captures the creating pid.
_CASE_NAME_RE = re.compile(r"^pgarbor_case_([0-9]+)_([0-9a-f]{10})$")
#: Identifier charset for generated database names.
_IDENT_RE = re.compile(r"^[a-z0-9_]+$")

_STALE_LIST_SQL = r"SELECT datname FROM pg_database WHERE datname LIKE 'pgarbor\_case\_%'"
_ROLES_SQL = "SELECT rolname FROM pg_roles ORDER BY 1"

#: Session GUCs every case endpoint carries (sorted, as a hashable tuple).
_CASE_GUCS: tuple[tuple[str, str], ...] = tuple(sorted(config.DIFFERENTIAL_SESSION_GUCS.items()))


@dataclass
class CaseDatabase:
    """A leased per-case database."""

    name: str
    endpoint: Endpoint
    warnings: list[str] = field(default_factory=list)


def _check_ident(name: str) -> None:
    """Refuse anything that is not a generated [a-z0-9_] identifier (defence in depth)."""
    if not _IDENT_RE.fullmatch(name):
        raise ValueError(f"unsafe database identifier: {name!r}")


def _quote_ident(name: str) -> str:
    """Double-quote an SQL identifier, doubling embedded quotes."""
    return '"' + name.replace('"', '""') + '"'


def _sqlstate(exc: BaseException) -> Optional[str]:
    """SQLSTATE carried by a psycopg error, or None for anything else."""
    value = getattr(exc, "sqlstate", None)
    return value if isinstance(value, str) else None


def _describe(exc: BaseException) -> str:
    return f"{type(exc).__name__}: {exc}"


def _owner_pid(name: str) -> Optional[int]:
    """The creating pid embedded in a case database name, or None if not a case name."""
    m = _CASE_NAME_RE.fullmatch(name)
    return int(m.group(1)) if m else None


def _pid_alive(pid: int) -> bool:
    """True unless the pid is provably dead. Errors on the side of 'alive' (never drop)."""
    if pid <= 0:
        return True
    try:
        os.kill(pid, 0)
    except ProcessLookupError:
        return False
    except (PermissionError, OSError, OverflowError, ValueError):
        return True
    return True


class OraclePool:
    """Pool of per-case databases on the single harness oracle container."""

    def __init__(self, container: Optional[OracleContainer] = None, max_parallel: int = 8):
        self.container = container or OracleContainer()
        self.max_parallel = max_parallel
        self._sem = threading.BoundedSemaphore(max_parallel)
        self._lock = threading.Lock()
        self._start_lock = threading.Lock()
        self._baseline_roles: list[str] = []
        self._started = False
        self._outstanding = 0
        self._admin_conn: Any = None

    @staticmethod
    def case_db_name(case_id: str, attempt: int, pid: int) -> str:
        """``pgarbor_case_<pid>_<10 lowercase hex>`` per module docstring (implemented)."""
        import hashlib

        h = hashlib.sha256(f"{case_id}:{attempt}:{pid}".encode()).hexdigest()[:10]
        return f"{config.CASE_DB_PREFIX}{pid}_{h}"

    def start(self) -> None:
        """container.ensure(); verify fingerprint (oracle.fingerprint.verify_live);
        drop stale case databases of DEAD pids only (``WITH (FORCE)``, see module docstring);
        create the template if missing; snapshot baseline roles. Idempotent.

        Order note: the template is created before the fingerprint is verified because the
        fingerprint describes the template database; a missing template would otherwise make
        every fresh oracle look drifted."""
        with self._start_lock:
            if self._started:
                return
            self.container.ensure()
            self._create_template_if_missing()
            verify_live(self.container)
            self._drop_stale_case_databases()
            roles = self._admin_query(_ROLES_SQL)
            self._baseline_roles = [str(r[0]) for r in roles]
            self._started = True

    def acquire(self, case_id: str, attempt: int = 1) -> CaseDatabase:
        """Block on the semaphore, CREATE DATABASE from the template, return its endpoint
        (``container.endpoint(name)`` with gucs=config.DIFFERENTIAL_SESSION_GUCS items).
        If CREATE DATABASE fails with 55006 (source database being accessed) retry up to 5
        times with 0.2 s backoff."""
        name = self.case_db_name(case_id, attempt, os.getpid())
        _check_ident(name)
        endpoint = dataclasses.replace(self.container.endpoint(name), gucs=_CASE_GUCS)
        self._sem.acquire()
        try:
            self._create_case_database(name)
        except BaseException:
            self._sem.release()
            raise
        with self._lock:
            self._outstanding += 1
        return CaseDatabase(name=name, endpoint=endpoint)

    def release(self, db: CaseDatabase) -> None:
        """``DROP DATABASE "<name>" WITH (FORCE)``; check_globals(); release semaphore (always,
        even if the drop fails: then append a warning).

        check_globals runs only when no other case is outstanding: a role created by a
        concurrently running case must not be dropped out from under it. Roles leaked while
        cases overlap are swept at the next quiescent release."""
        try:
            with self._lock:
                self._outstanding -= 1
                quiescent = self._outstanding == 0
            try:
                _check_ident(db.name)
                self.admin_execute(f"DROP DATABASE {_quote_ident(db.name)} WITH (FORCE)")
            except Exception as exc:  # noqa: BLE001 - any failure is a warning, never a raise
                db.warnings.append(f"drop of {db.name} failed: {_describe(exc)}")
            if quiescent:
                try:
                    leaked = self.check_globals()
                except Exception as exc:  # noqa: BLE001
                    db.warnings.append(f"check_globals failed: {_describe(exc)}")
                else:
                    if leaked:
                        db.warnings.append("globals_leaked: " + ", ".join(leaked))
        finally:
            self._sem.release()

    @contextmanager
    def case_database(self, case_id: str, attempt: int = 1) -> Iterator[CaseDatabase]:
        """``with pool.case_database(id) as db:`` = acquire + guaranteed release (implemented)."""
        db = self.acquire(case_id, attempt)
        try:
            yield db
        finally:
            self.release(db)

    def check_globals(self) -> list[str]:
        """Drop roles not in the baseline snapshot; return their names (see docstring).

        Only roles are compared (tablespaces and cluster-level settings are not expected to
        change under a case). Does nothing before start() has taken the baseline, so it can
        never drop roles it cannot prove were leaked. ``pg_*`` predefined roles are never
        touched. A role that cannot be dropped raises OracleError after the others are tried."""
        if not self._started:
            return []
        baseline = set(self._baseline_roles)
        rows = self._admin_query(_ROLES_SQL)
        leaked = [str(r[0]) for r in rows
                  if str(r[0]) not in baseline and not str(r[0]).startswith("pg_")]
        failures: list[str] = []
        for role in leaked:
            try:
                self.admin_execute(f"DROP ROLE IF EXISTS {_quote_ident(role)}")
            except Exception as exc:  # noqa: BLE001
                failures.append(f"{role} ({_describe(exc)})")
        if failures:
            raise OracleError("could not drop leaked roles: " + "; ".join(failures))
        return leaked

    def admin_execute(self, sql: str) -> None:
        """Run one statement on an autocommit admin connection to ``postgres``.
        Lazily ``import psycopg``; one cached admin connection guarded by ``self._lock``;
        reconnect once on OperationalError."""
        self._admin_run(sql, fetch=False)

    def close(self) -> None:
        """Close the admin connection. Does NOT stop the container (pooled across runs)."""
        with self._lock:
            self._close_admin_locked()

    # ------------------------------------------------------------------------------------
    # private helpers
    # ------------------------------------------------------------------------------------

    def _admin_query(self, sql: str) -> list[tuple[Any, ...]]:
        """Run one statement on the admin connection and return all rows."""
        return self._admin_run(sql, fetch=True)

    def _admin_run(self, sql: str, fetch: bool) -> list[tuple[Any, ...]]:
        import psycopg

        with self._lock:
            for attempt in range(2):
                try:
                    if self._admin_conn is None:
                        self._admin_conn = self._connect()
                    with self._admin_conn.cursor() as cur:
                        cur.execute(sql)
                        return list(cur.fetchall()) if fetch else []
                except psycopg.OperationalError:
                    self._close_admin_locked()
                    if attempt == 1:
                        raise
        return []  # unreachable: the loop returns or re-raises

    def _connect(self) -> Any:
        import psycopg

        info = (self.container.endpoint("postgres").conninfo()
                + f" connect_timeout={int(config.CONNECT_TIMEOUT_S)}")
        return psycopg.connect(info, autocommit=True)

    def _close_admin_locked(self) -> None:
        conn, self._admin_conn = self._admin_conn, None
        if conn is not None:
            try:
                conn.close()
            except Exception:  # noqa: BLE001 - closing a dead connection is best effort
                pass

    def _create_template_if_missing(self) -> None:
        template = config.CASE_TEMPLATE_DB
        _check_ident(template)
        rows = self._admin_query(
            f"SELECT 1 FROM pg_database WHERE datname = '{template}'")
        if rows:
            return
        try:
            self.admin_execute(
                f"CREATE DATABASE {template} TEMPLATE template0 ENCODING 'UTF8' "
                "LOCALE_PROVIDER builtin BUILTIN_LOCALE 'C.UTF-8'")
        except Exception as exc:  # noqa: BLE001
            if _sqlstate(exc) == _SQLSTATE_DUPLICATE_DATABASE:
                return  # another process created it first
            raise OracleError(f"create template {template} failed: {_describe(exc)}") from exc

    def _drop_stale_case_databases(self) -> None:
        for (name,) in self._admin_query(_STALE_LIST_SQL):
            name = str(name)
            if name == config.CASE_TEMPLATE_DB:
                continue
            pid = _owner_pid(name)
            if pid is None or _pid_alive(pid):
                continue
            try:
                _check_ident(name)
                self.admin_execute(f"DROP DATABASE {_quote_ident(name)} WITH (FORCE)")
            except Exception as exc:  # noqa: BLE001
                raise OracleError(f"drop stale case database {name} failed: "
                                  f"{_describe(exc)}") from exc

    def _create_case_database(self, name: str) -> None:
        sql = f"CREATE DATABASE {_quote_ident(name)} TEMPLATE {_quote_ident(config.CASE_TEMPLATE_DB)}"
        for attempt in range(_CREATE_RETRIES + 1):
            try:
                self.admin_execute(sql)
                return
            except Exception as exc:  # noqa: BLE001
                if _sqlstate(exc) == _SQLSTATE_OBJECT_IN_USE and attempt < _CREATE_RETRIES:
                    time.sleep(_CREATE_BACKOFF_S)
                    continue
                raise OracleError(f"create case database {name} failed: "
                                  f"{_describe(exc)}") from exc
        raise OracleError(f"create case database {name} failed")  # unreachable
