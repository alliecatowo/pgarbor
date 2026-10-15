"""Tests for HAR-02 (oracle.pool: pooled per-case databases on the single oracle).

Unit tests use a fake container and a fake admin channel (monkeypatched), so they need no
podman or PostgreSQL. The live test is marked ``oracle`` and is skipped unless PGARBOR_ORACLE=1.
"""

from __future__ import annotations

import hashlib
import os
import re
import subprocess
import sys
import threading
from concurrent.futures import ThreadPoolExecutor

import pytest

from pgarbor_harness import config
from pgarbor_harness.model import Endpoint
from pgarbor_harness.oracle import pool as pool_mod
from pgarbor_harness.oracle.container import OracleError
from pgarbor_harness.oracle.pool import OraclePool

NAME_RE = re.compile(r"^pgarbor_case_[0-9]+_[0-9a-f]{10}$")


class FakeDbError(Exception):
    """Stands in for a psycopg error: carries an SQLSTATE like psycopg.Error.sqlstate."""

    def __init__(self, sqlstate: str, message: str = "fake"):
        super().__init__(message)
        self.sqlstate = sqlstate


class FakeContainer:
    def ensure(self, recreate_on_drift: bool = False) -> Endpoint:
        return self.endpoint("postgres")

    def endpoint(self, dbname: str = "postgres") -> Endpoint:
        return Endpoint(host="127.0.0.1", port=1, dbname=dbname, user="postgres", password="x")


# ---------------------------------------------------------------------------------------------
# case_db_name
# ---------------------------------------------------------------------------------------------


def test_case_db_name_is_stable_and_well_formed():
    name = OraclePool.case_db_name("smoke.x", 1, 123)
    expected_hex = hashlib.sha256(b"smoke.x:1:123").hexdigest()[:10]
    assert name == f"pgarbor_case_123_{expected_hex}"
    assert NAME_RE.fullmatch(name)
    # Deterministic across calls and sensitive to every input.
    assert OraclePool.case_db_name("smoke.x", 1, 123) == name
    assert OraclePool.case_db_name("smoke.x", 2, 123) != name
    assert OraclePool.case_db_name("smoke.x", 1, 124) != name
    assert OraclePool.case_db_name("smoke.y", 1, 123) != name


# ---------------------------------------------------------------------------------------------
# semaphore bound, acquire/release hygiene (fake admin)
# ---------------------------------------------------------------------------------------------


def test_acquire_blocks_when_max_parallel_outstanding(monkeypatch):
    pool = OraclePool(container=FakeContainer(), max_parallel=2)
    created: list[str] = []
    monkeypatch.setattr(pool, "admin_execute", lambda sql: created.append(sql))

    a = pool.acquire("a")
    b = pool.acquire("b")
    assert len(created) == 2

    got = threading.Event()

    def third():
        db = pool.acquire("c")
        got.set()
        pool.release(db)

    t = threading.Thread(target=third, daemon=True)
    t.start()
    assert not got.wait(0.3), "third acquire must block while two databases are outstanding"

    pool.release(a)
    assert got.wait(5), "third acquire must proceed once a permit is released"
    t.join(5)
    pool.release(b)
    assert not t.is_alive()


def test_release_releases_semaphore_even_when_drop_raises(monkeypatch):
    pool = OraclePool(container=FakeContainer(), max_parallel=1)

    def admin(sql: str):
        if sql.startswith("DROP DATABASE"):
            raise FakeDbError("55006", "database is being accessed")

    monkeypatch.setattr(pool, "admin_execute", admin)
    db = pool.acquire("drop-fails")
    pool.release(db)  # must not raise

    assert any("drop of" in w for w in db.warnings)
    # The single permit is back: a second acquire completes.
    done = threading.Event()

    def again():
        pool.release(pool.acquire("after"))
        done.set()

    t = threading.Thread(target=again, daemon=True)
    t.start()
    assert done.wait(5)
    t.join(5)


def test_acquire_retries_object_in_use_then_succeeds(monkeypatch):
    pool = OraclePool(container=FakeContainer(), max_parallel=1)
    attempts = {"n": 0}
    sleeps: list[float] = []

    def admin(sql: str):
        if sql.startswith("CREATE DATABASE"):
            attempts["n"] += 1
            if attempts["n"] <= 2:
                raise FakeDbError("55006", "source database is being accessed")

    monkeypatch.setattr(pool, "admin_execute", admin)
    monkeypatch.setattr(pool_mod.time, "sleep", lambda s: sleeps.append(s))
    db = pool.acquire("busy")
    assert attempts["n"] == 3
    assert sleeps == [pool_mod._CREATE_BACKOFF_S, pool_mod._CREATE_BACKOFF_S]
    assert db.endpoint.gucs == tuple(sorted(config.DIFFERENTIAL_SESSION_GUCS.items()))
    pool.release(db)


def test_acquire_failure_releases_semaphore_before_raising(monkeypatch):
    pool = OraclePool(container=FakeContainer(), max_parallel=1)

    def admin(sql: str):
        if sql.startswith("CREATE DATABASE"):
            raise FakeDbError("42P04", "database already exists")

    monkeypatch.setattr(pool, "admin_execute", admin)
    with pytest.raises(OracleError):
        pool.acquire("dup")
    # Permit must have been returned, otherwise this would block forever.
    assert pool._sem.acquire(timeout=1)
    pool._sem.release()


def test_check_globals_is_noop_before_start(monkeypatch):
    pool = OraclePool(container=FakeContainer(), max_parallel=1)

    def admin(sql: str):
        raise AssertionError(f"no admin call expected before start: {sql}")

    monkeypatch.setattr(pool, "admin_execute", admin)
    monkeypatch.setattr(pool, "_admin_query", lambda sql: [("postgres",), ("intruder",)])
    assert pool.check_globals() == []


def test_check_globals_drops_only_new_non_predefined_roles(monkeypatch):
    pool = OraclePool(container=FakeContainer(), max_parallel=1)
    pool._started = True
    pool._baseline_roles = ["postgres"]
    dropped: list[str] = []

    def admin(sql: str):
        if sql.startswith("DROP ROLE"):
            dropped.append(sql)

    monkeypatch.setattr(pool, "admin_execute", admin)
    monkeypatch.setattr(pool, "_admin_query",
                        lambda sql: [("pg_monitor",), ("postgres",), ('we"ird',),
                                     ("pgarbor_case_role_x",)])
    leaked = pool.check_globals()
    assert leaked == ['we"ird', "pgarbor_case_role_x"]
    assert dropped == ['DROP ROLE IF EXISTS "we""ird"',
                       'DROP ROLE IF EXISTS "pgarbor_case_role_x"']


# ---------------------------------------------------------------------------------------------
# start(): stale database garbage collection never touches live processes or the template
# ---------------------------------------------------------------------------------------------


def test_start_drops_only_dead_pid_case_databases(monkeypatch):
    # A pid that certainly no longer exists: a child that has exited and been reaped.
    child = subprocess.Popen([sys.executable, "-c", "pass"])
    child.wait()
    dead_pid = child.pid

    live_name = f"pgarbor_case_{os.getpid()}_0123456789"
    dead_name = f"pgarbor_case_{dead_pid}_abcdef0123"
    template = config.CASE_TEMPLATE_DB
    listing = [(live_name,), (dead_name,), (template,), ("pgarbor_case_notapid",)]
    drops: list[str] = []

    pool = OraclePool(container=FakeContainer(), max_parallel=2)

    def query(sql: str):
        if "datname = " in sql:           # template existence check
            return [(1,)]
        if "LIKE" in sql:                 # stale case database listing
            return list(listing)
        if "pg_roles" in sql:
            return [("postgres",)]
        raise AssertionError(f"unexpected query: {sql}")

    def admin(sql: str):
        if sql.startswith("DROP DATABASE"):
            drops.append(sql)

    monkeypatch.setattr(pool_mod, "verify_live", lambda container: ({}, "sha"))
    monkeypatch.setattr(pool, "_admin_query", query)
    monkeypatch.setattr(pool, "admin_execute", admin)

    pool.start()
    assert drops == [f'DROP DATABASE "{dead_name}" WITH (FORCE)']
    assert pool._baseline_roles == ["postgres"]

    # Idempotent: a second start does nothing.
    drops.clear()
    pool.start()
    assert drops == []


# ---------------------------------------------------------------------------------------------
# live (PG18 container required)
# ---------------------------------------------------------------------------------------------


@pytest.mark.oracle
def test_live_concurrent_cycles_leave_no_case_databases():
    pool = OraclePool(max_parallel=8)
    pool.start()
    try:
        def cycle(i: int) -> str:
            with pool.case_database(f"live.{i}") as db:
                return db.name

        with ThreadPoolExecutor(max_workers=8) as ex:
            names = list(ex.map(cycle, range(8)))
        assert len(set(names)) == 8

        rows = pool._admin_query(r"SELECT datname FROM pg_database WHERE datname LIKE 'pgarbor\_case\_%'")
        leftover = {str(r[0]) for r in rows} - {config.CASE_TEMPLATE_DB}
        assert leftover == set()
    finally:
        pool.close()
