"""Minimal DB-API-like ctypes binding over the PGArbor C ABI (include/pgarbor.h).

    import pgarbor
    conn = pgarbor.connect("dev.pga")
    cur = conn.execute("SELECT 1 AS one")
    cur.fetchall()   # [('1',)]   (values are PG text-format strings; NULL is None)

Handles are bound to the creating thread by the engine: use a connection from one thread only.
"""
from __future__ import annotations

import ctypes
import json
import os
import sys
from pathlib import Path

__all__ = ["connect", "Connection", "Cursor", "Error", "version"]

PGA_OPEN_CREATE = 1
PGA_OPEN_READONLY = 2
PGA_OPEN_MEMORY = 4


class Error(Exception):
    """Database error. ``sqlstate`` is the 5-character SQLSTATE."""

    def __init__(self, sqlstate: str, message: str, detail: str | None = None, hint: str | None = None):
        super().__init__(f"{sqlstate}: {message}")
        self.sqlstate = sqlstate
        self.pgcode = sqlstate
        self.message = message
        self.detail = detail
        self.hint = hint


def _lib_name() -> str:
    if sys.platform == "darwin":
        return "libpgarbor_ffi.dylib"
    if sys.platform == "win32":
        return "pgarbor_ffi.dll"
    return "libpgarbor_ffi.so"


def _find_lib() -> str:
    env = os.environ.get("PGARBOR_LIB")
    if env:
        return env
    here = Path(__file__).resolve()
    for parent in here.parents:
        for profile in ("release", "debug"):
            cand = parent / "target" / profile / _lib_name()
            if cand.exists():
                return str(cand)
    return _lib_name()  # rely on the system loader


_lib = None


def _load():
    global _lib
    if _lib is not None:
        return _lib
    lib = ctypes.CDLL(_find_lib())
    c_str = ctypes.c_char_p
    u64 = ctypes.c_uint64
    lib.pga_version.restype = c_str
    for fn in ("pga_errcode", "pga_errmsg", "pga_errdetail", "pga_errhint"):
        getattr(lib, fn).restype = c_str
        getattr(lib, fn).argtypes = []
    lib.pga_open.argtypes = [c_str, ctypes.c_uint32, ctypes.POINTER(u64)]
    lib.pga_close.argtypes = [u64]
    lib.pga_session_open.argtypes = [u64, c_str, c_str, c_str, ctypes.POINTER(u64)]
    lib.pga_session_close.argtypes = [u64]
    lib.pga_exec_json.argtypes = [u64, c_str]
    lib.pga_exec_json.restype = ctypes.c_void_p  # keep the raw pointer so we can free it
    lib.pga_string_free.argtypes = [ctypes.c_void_p]
    lib.pga_txn_status.argtypes = [u64]
    lib.pga_branch_create.argtypes = [u64, c_str, c_str]
    lib.pga_branch_delete.argtypes = [u64, c_str]
    lib.pga_branch_list.argtypes = [u64]
    lib.pga_branch_list.restype = c_str
    _lib = lib
    return lib


def _last_error() -> Error:
    lib = _load()
    dec = lambda b: b.decode("utf-8", "replace") if b else None  # noqa: E731
    return Error(dec(lib.pga_errcode()) or "XX000", dec(lib.pga_errmsg()) or "unknown error",
                 dec(lib.pga_errdetail()), dec(lib.pga_errhint()))


def version() -> str:
    return _load().pga_version().decode()


class Cursor:
    def __init__(self, conn: "Connection"):
        self._conn = conn
        self.description = None
        self.rowcount = -1
        self.statusmessage = None
        self._rows: list[tuple] = []
        self._pos = 0

    def execute(self, sql: str) -> "Cursor":
        lib = _load()
        conn = self._conn
        if conn._session == 0:
            raise Error("08003", "connection is closed")
        ptr = lib.pga_exec_json(conn._session, sql.encode("utf-8"))
        if not ptr:
            raise _last_error()
        try:
            doc = json.loads(ctypes.string_at(ptr).decode("utf-8"))
        finally:
            lib.pga_string_free(ptr)
        # Multi-statement strings: the cursor exposes the last statement's result.
        last = doc[-1] if doc else {"columns": [], "rows": [], "tag": ""}
        self.description = [(c["name"], c["type_oid"], None, None, None, None, None) for c in last["columns"]] or None
        self._rows = [tuple(r) for r in last["rows"]]
        self._pos = 0
        self.statusmessage = last["tag"]
        tag = last["tag"].split()
        self.rowcount = int(tag[-1]) if tag and tag[-1].isdigit() else -1
        return self

    def fetchone(self):
        if self._pos >= len(self._rows):
            return None
        row = self._rows[self._pos]
        self._pos += 1
        return row

    def fetchall(self):
        rows = self._rows[self._pos:]
        self._pos = len(self._rows)
        return rows

    def __iter__(self):
        return iter(self.fetchall())

    def close(self):
        self._rows = []


class Connection:
    def __init__(self, db: int, session: int):
        self._db = db
        self._session = session

    def cursor(self) -> Cursor:
        return Cursor(self)

    def execute(self, sql: str) -> Cursor:
        return self.cursor().execute(sql)

    def create_branch(self, name: str, from_branch: str = "main") -> None:
        if _load().pga_branch_create(self._db, name.encode(), from_branch.encode()) != 0:
            raise _last_error()

    def delete_branch(self, name: str) -> None:
        if _load().pga_branch_delete(self._db, name.encode()) != 0:
            raise _last_error()

    def branches(self) -> list[str]:
        out = _load().pga_branch_list(self._db)
        if out is None:
            raise _last_error()
        return [b for b in out.decode().split("\n") if b]

    def close(self) -> None:
        lib = _load()
        if self._session:
            lib.pga_session_close(self._session)
            self._session = 0
        if self._db:
            rc = lib.pga_close(self._db)
            self._db = 0
            if rc != 0:
                raise _last_error()

    def __enter__(self):
        return self

    def __exit__(self, *exc):
        self.close()


def connect(path: str, *, create: bool = True, read_only: bool = False, memory: bool = False,
            branch: str = "main") -> Connection:
    lib = _load()
    flags = (PGA_OPEN_CREATE if create else 0) | (PGA_OPEN_READONLY if read_only else 0) | \
            (PGA_OPEN_MEMORY if memory else 0)
    db = ctypes.c_uint64(0)
    if lib.pga_open(os.fsencode(path), flags, ctypes.byref(db)) != 0:
        raise _last_error()
    sess = ctypes.c_uint64(0)
    if lib.pga_session_open(db.value, branch.encode(), None, None, ctypes.byref(sess)) != 0:
        err = _last_error()
        lib.pga_close(db.value)
        raise err
    return Connection(db.value, sess.value)
