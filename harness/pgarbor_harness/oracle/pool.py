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

import threading
from contextlib import contextmanager
from dataclasses import dataclass, field
from typing import Iterator, Optional

from .. import config
from ..model import Endpoint
from .container import OracleContainer


@dataclass
class CaseDatabase:
    """A leased per-case database."""

    name: str
    endpoint: Endpoint
    warnings: list[str] = field(default_factory=list)


class OraclePool:
    """Pool of per-case databases on the single harness oracle container."""

    def __init__(self, container: Optional[OracleContainer] = None, max_parallel: int = 8):
        self.container = container or OracleContainer()
        self.max_parallel = max_parallel
        self._sem = threading.BoundedSemaphore(max_parallel)
        self._lock = threading.Lock()
        self._baseline_roles: list[str] = []
        self._started = False

    @staticmethod
    def case_db_name(case_id: str, attempt: int, pid: int) -> str:
        """``pgarbor_case_<pid>_<10 lowercase hex>`` per module docstring (implemented)."""
        import hashlib

        h = hashlib.sha256(f"{case_id}:{attempt}:{pid}".encode()).hexdigest()[:10]
        return f"{config.CASE_DB_PREFIX}{pid}_{h}"

    def start(self) -> None:
        """container.ensure(); verify fingerprint (oracle.fingerprint.verify_live);
        drop stale case databases of DEAD pids only (``WITH (FORCE)``, see module docstring);
        create the template if missing; snapshot baseline roles. Idempotent."""
        raise NotImplementedError("HAR-02: pool start")

    def acquire(self, case_id: str, attempt: int = 1) -> CaseDatabase:
        """Block on the semaphore, CREATE DATABASE from the template, return its endpoint
        (``container.endpoint(name)`` with gucs=config.DIFFERENTIAL_SESSION_GUCS items).
        If CREATE DATABASE fails with 55006 (source database being accessed) retry up to 5
        times with 0.2 s backoff."""
        raise NotImplementedError("HAR-02: create per-case database")

    def release(self, db: CaseDatabase) -> None:
        """``DROP DATABASE "<name>" WITH (FORCE)``; check_globals(); release semaphore (always,
        even if the drop fails: then append a warning)."""
        raise NotImplementedError("HAR-02: drop per-case database")

    @contextmanager
    def case_database(self, case_id: str, attempt: int = 1) -> Iterator[CaseDatabase]:
        """``with pool.case_database(id) as db:`` = acquire + guaranteed release (implemented)."""
        db = self.acquire(case_id, attempt)
        try:
            yield db
        finally:
            self.release(db)

    def check_globals(self) -> list[str]:
        """Drop roles not in the baseline snapshot; return their names (see docstring)."""
        raise NotImplementedError("HAR-02: global object hygiene")

    def admin_execute(self, sql: str) -> None:
        """Run one statement on an autocommit admin connection to ``postgres``.
        Lazily ``import psycopg``; one cached admin connection guarded by ``self._lock``;
        reconnect once on OperationalError."""
        raise NotImplementedError("HAR-02: admin connection")

    def close(self) -> None:
        """Close the admin connection. Does NOT stop the container (pooled across runs)."""
        raise NotImplementedError("HAR-02: close")
