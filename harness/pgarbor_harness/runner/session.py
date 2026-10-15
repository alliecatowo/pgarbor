"""Exact protocol-level session driver over libpq via ``psycopg.pq`` (low-level API).

Lease HAR-09 (sonnet: non-blocking libpq, cancel, notice capture).

Why the low-level API: we must observe what the server sent, not what a driver adapted —
raw text bytes, type OIDs, typmods, per-statement results of a multi-statement Query,
CommandComplete tags, every ErrorResponse field and NoticeResponse in order. psycopg's
``Cursor`` adapts values and merges results; ``psycopg.pq.PGconn``/``PGresult`` do not.

psycopg must be imported lazily *inside* functions (``from psycopg import pq``) so the rest of
the harness imports with the stdlib + PyYAML only. API used (psycopg >= 3.2, ``psycopg.pq``;
verify names against the installed package's ``psycopg/pq/abc.py`` after ``uv sync``):

  PGconn.connect(conninfo: bytes) -> PGconn;   .status (pq.ConnStatus.OK / BAD)
  .error_message (bytes) ; .socket (fd) ; .nonblocking = 1 ; .finish()
  .send_query(bytes) ; .send_query_params(cmd, values, param_types=, param_formats=,
      result_format=) ; .send_prepare(name, cmd, param_types=) ;
  .send_query_prepared(name, values, param_formats=, result_format=) ;
  .send_describe_prepared(name) ; .send_close_prepared(name)   (libpq >= 17)
  .consume_input() ; .is_busy() ; .get_result() -> PGresult | None ; .flush()
  .get_copy_data(async_: int) -> (nbytes, bytes) ; .put_copy_end(error: bytes | None)
  .transaction_status (pq.TransactionStatus IDLE/ACTIVE/INTRANS/INERROR/UNKNOWN)
  .notice_handler = callable(PGresult) ; .get_cancel().cancel()  (or PGcancelConn on 17+)
  PGresult: .status (pq.ExecStatus TUPLES_OK, COMMAND_OK, EMPTY_QUERY, COPY_OUT, COPY_IN,
      FATAL_ERROR, NONFATAL_ERROR, SINGLE_TUPLE, PIPELINE_SYNC...), .ntuples, .nfields,
      .fname(i), .ftype(i), .fmod(i), .fformat(i), .ftable(i), .ftablecol(i),
      .get_value(r, c) -> bytes | None, .command_status -> bytes | None,
      .error_field(pq.DiagnosticField.X) -> bytes | None, .nparams, .param_type(i)

Field mapping into model.ErrorInfo / model.Notice (DiagnosticField name -> attribute):
  SEVERITY_NONLOCALIZED->severity (fallback SEVERITY), SQLSTATE->sqlstate,
  MESSAGE_PRIMARY->message, MESSAGE_DETAIL->detail, MESSAGE_HINT->hint,
  STATEMENT_POSITION->position, INTERNAL_POSITION->internal_position,
  INTERNAL_QUERY->internal_query, CONTEXT->context, SCHEMA_NAME->schema_name,
  TABLE_NAME->table_name, COLUMN_NAME->column_name, DATATYPE_NAME->datatype_name,
  CONSTRAINT_NAME->constraint_name. All bytes decoded UTF-8 (errors="replace" — the decode
  must never raise).

PGresult -> model.StatementResult:
  TUPLES_OK -> kind "rows": columns from fname/ftype/fmod/fformat/ftable/ftablecol; values via
      get_value: None -> None; fformat 0 -> bytes.decode("utf-8") (on UnicodeDecodeError keep
      bytes); fformat 1 -> bytes. command_tag = command_status.decode().
  COMMAND_OK -> kind "command" (for a describe: kind "description" with columns and
      param_type_oids=[param_type(i) for i in range(nparams)]).
  EMPTY_QUERY -> kind "empty", command_tag None.
  FATAL_ERROR / NONFATAL_ERROR -> kind "error" with ErrorInfo.
  COPY_OUT -> loop get_copy_data(0) until nbytes == -1, collect each chunk as one row; then the
      next get_result() carries the COPY tag; produce ONE StatementResult(kind="copy_out").
  COPY_IN -> put_copy_end(b"pgarbor harness: COPY FROM STDIN data not supported"); the
      following error result is recorded normally.

Step-boundary rule: one ``run_*`` call = all results until get_result() returns None (for the
simple protocol, after ReadyForQuery). Notices are collected from the moment the step is sent
until the step's final result, in arrival order, into ``StepOutput.notices``. Notices that
arrive while no step is active (e.g. async NOTIFY, or after a step finished) are buffered and
attached to the NEXT step of that session.

Timeouts: every wait uses ``select.select([socket], [], [], remaining)`` + consume_input();
no blocking libpq call is ever made without a deadline. On deadline: send cancel, wait up to
2 s for the cancelled result, then ``finish()`` the connection and mark
``timed_out=True``; the session object reconnects lazily on next use.
Connection loss (status BAD, consume_input raising, socket EOF) -> ``connection_lost=True``,
results collected so far are kept.
"""

from __future__ import annotations

import select
import time
from dataclasses import dataclass, field
from typing import Any, Callable, Optional, Sequence

from ..model import (ColumnDesc, Endpoint, ErrorInfo, Notice, StatementResult, Value)


class HarnessSessionError(RuntimeError):
    """Harness-side failure (bad step, libpq feature missing). Becomes harness_error."""


@dataclass
class StepOutput:
    """Raw result of one protocol action on one session."""

    results: list[StatementResult] = field(default_factory=list)
    notices: list[Notice] = field(default_factory=list)
    txn_status: str = "U"
    timed_out: bool = False
    connection_lost: bool = False
    elapsed_ms: int = 0


# -- libpq constants (stable ABI; equal to psycopg.pq enum values) -------------------------------
_EMPTY_QUERY, _COMMAND_OK, _TUPLES_OK, _COPY_OUT, _COPY_IN = 0, 1, 2, 3, 4
_BAD_RESPONSE, _NONFATAL_ERROR, _FATAL_ERROR, _COPY_BOTH, _SINGLE_TUPLE = 5, 6, 7, 8, 9
_CONN_BAD = 1
_TXN = {0: "I", 1: "A", 2: "T", 3: "E", 4: "U"}

_F_SEVERITY = ord("S")
_F_SEVERITY_NL = ord("V")
_F_SQLSTATE = ord("C")
_F_MESSAGE = ord("M")
_F_DETAIL = ord("D")
_F_HINT = ord("H")
_F_POSITION = ord("P")
_F_INTERNAL_POSITION = ord("p")
_F_INTERNAL_QUERY = ord("q")
_F_CONTEXT = ord("W")
_F_SCHEMA = ord("s")
_F_TABLE = ord("t")
_F_COLUMN = ord("c")
_F_DATATYPE = ord("d")
_F_CONSTRAINT = ord("n")

_COPY_IN_MESSAGE = b"pgarbor harness: COPY FROM STDIN data not supported"
_CANCEL_GRACE_S = 2.0


def _dec(b: Any) -> Optional[str]:
    """bytes-like -> str (UTF-8, errors replaced); None stays None. Never raises."""
    if b is None:
        return None
    if isinstance(b, str):
        return b
    return bytes(b).decode("utf-8", errors="replace")


def _field(res: Any, code: int) -> Optional[str]:
    return _dec(res.error_field(code))


def _error_info(res: Any) -> ErrorInfo:
    severity = _field(res, _F_SEVERITY_NL) or _field(res, _F_SEVERITY) or ""
    message = _field(res, _F_MESSAGE)
    if message is None:
        # client-side failure (e.g. connection lost): libpq only has the error text
        message = (_dec(getattr(res, "error_message", None)) or "").strip()
    return ErrorInfo(
        severity=severity, sqlstate=_field(res, _F_SQLSTATE) or "", message=message,
        detail=_field(res, _F_DETAIL), hint=_field(res, _F_HINT),
        position=_field(res, _F_POSITION),
        internal_position=_field(res, _F_INTERNAL_POSITION),
        internal_query=_field(res, _F_INTERNAL_QUERY), context=_field(res, _F_CONTEXT),
        schema_name=_field(res, _F_SCHEMA), table_name=_field(res, _F_TABLE),
        column_name=_field(res, _F_COLUMN), datatype_name=_field(res, _F_DATATYPE),
        constraint_name=_field(res, _F_CONSTRAINT),
    )


def notice_from_result(res: Any) -> Notice:
    """PGresult of a NoticeResponse -> model.Notice."""
    return Notice(
        severity=_field(res, _F_SEVERITY_NL) or _field(res, _F_SEVERITY) or "",
        sqlstate=_field(res, _F_SQLSTATE) or "",
        message=_field(res, _F_MESSAGE) or "",
        detail=_field(res, _F_DETAIL), hint=_field(res, _F_HINT),
        context=_field(res, _F_CONTEXT),
    )


def _columns(res: Any) -> list[ColumnDesc]:
    return [
        ColumnDesc(
            name=_dec(res.fname(i)) or "", type_oid=int(res.ftype(i)), typmod=int(res.fmod(i)),
            format=int(res.fformat(i)), table_oid=int(res.ftable(i)),
            column_attnum=int(res.ftablecol(i)),
        )
        for i in range(res.nfields)
    ]


def _value(raw: Any, fmt: int) -> Value:
    if raw is None:
        return None
    b = bytes(raw)
    if fmt == 0:
        try:
            return b.decode("utf-8")
        except UnicodeDecodeError:
            return b
    return b


def _tag(res: Any) -> Optional[str]:
    st = getattr(res, "command_status", None)
    if not st:
        return None
    return _dec(st)


def result_from_pgresult(res: Any, describe: bool = False) -> StatementResult:
    """Convert a non-COPY PGresult into a StatementResult (pure; no I/O).

    ``describe`` selects the "description" form for the result of PQdescribePrepared.
    COPY_OUT / COPY_IN are handled by the session state machine, not here.
    """
    status = int(res.status)
    if status in (_FATAL_ERROR, _NONFATAL_ERROR, _BAD_RESPONSE):
        return StatementResult(kind="error", error=_error_info(res))
    if status == _EMPTY_QUERY:
        return StatementResult(kind="empty", command_tag=None)
    if status in (_TUPLES_OK, _COMMAND_OK, _SINGLE_TUPLE):
        cols = _columns(res)
        if describe:
            return StatementResult(
                kind="description", columns=cols,
                param_type_oids=[int(res.param_type(i)) for i in range(res.nparams)],
            )
        if status == _COMMAND_OK:
            return StatementResult(kind="command", columns=cols, command_tag=_tag(res))
        fmts = [c.format for c in cols]
        rows: list[list[Value]] = [
            [_value(res.get_value(r, c), fmts[c]) for c in range(len(cols))]
            for r in range(res.ntuples)
        ]
        return StatementResult(kind="rows", columns=cols, rows=rows, command_tag=_tag(res))
    raise HarnessSessionError(f"unexpected libpq result status {status}")


def _param_formats(params: Sequence[Optional[str]], param_formats: Sequence[str]) -> list[int]:
    n = len(params)
    codes: list[int] = []
    for f in param_formats:
        if f == "text":
            codes.append(0)
        elif f == "binary":
            codes.append(1)
        else:
            raise HarnessSessionError(f"bad param format {f!r}")
    if not codes:
        return [0] * n
    if len(codes) == 1:
        return codes * n
    if len(codes) != n:
        raise HarnessSessionError(
            f"{len(codes)} param formats for {n} params (must be 0, 1 or one per param)")
    return codes


def _encode_params(params: Sequence[Optional[str]],
                   param_formats: Sequence[str]) -> tuple[list[Optional[bytes]], list[int]]:
    fmts = _param_formats(params, param_formats)
    vals: list[Optional[bytes]] = []
    for p, f in zip(params, fmts):
        if p is None:
            vals.append(None)
        elif f == 0:
            vals.append(p.encode("utf-8"))
        else:
            try:
                vals.append(bytes.fromhex(p))
            except ValueError as e:
                raise HarnessSessionError(f"binary param is not hex: {p!r}") from e
    return vals, fmts


def _result_format(fmt: str) -> int:
    if fmt == "text":
        return 0
    if fmt == "binary":
        return 1
    raise HarnessSessionError(f"bad result format {fmt!r}")


def _sql_bytes(sql: str) -> bytes:
    if "\x00" in sql:
        raise HarnessSessionError("SQL text contains a NUL byte")
    return sql.encode("utf-8")


class Session:
    """One libpq connection = one case session. Not thread-safe (one owner thread).

    Lifecycle: ``Session(endpoint, name)`` -> ``connect()`` -> run_* / start_* + finish_pending
    -> ``close()``. ``pending`` is True between ``start_*`` and ``finish_pending``.
    """

    def __init__(self, endpoint: Endpoint, name: str, step_timeout_s: float = 10.0):
        self.endpoint = endpoint
        self.name = name
        self.step_timeout_s = step_timeout_s
        self.pending = False
        self._conn: Any = None
        self._notice_buffer: list[Notice] = []
        # state of the step in flight (sync or async)
        self._acc = StepOutput()
        self._mode = ""
        self._t0 = 0.0
        self._done = True
        self._need_flush = False
        self._in_copy_data = False
        self._copy_binary = False
        self._copy_res: Optional[StatementResult] = None
        self._deadline = 0.0

    # -- connection ---------------------------------------------------------------------------
    def connect(self) -> None:
        """PGconn.connect(endpoint.conninfo().encode()); status BAD -> raise
        ConnectionError(error_message). Install notice_handler; set nonblocking."""
        from psycopg import pq  # lazy: harness imports with stdlib + PyYAML only

        self.close()
        info = self.endpoint.conninfo()
        if "client_encoding" not in info:
            info += " client_encoding=UTF8"
        conn = pq.PGconn.connect(info.encode("utf-8"))
        if int(conn.status) != 0:
            msg = (_dec(conn.error_message) or "connection failed").strip()
            try:
                conn.finish()
            except Exception:
                pass
            raise ConnectionError(msg)
        buf = self._notice_buffer

        def _on_notice(res: Any) -> None:
            try:
                buf.append(notice_from_result(res))
            except Exception:
                pass  # a notice handler must never raise into libpq

        conn.notice_handler = _on_notice
        conn.nonblocking = 1
        self._conn = conn
        self.pending = False
        self._done = True
        self._in_copy_data = False
        self._copy_res = None

    def close(self) -> None:
        """finish() if connected; idempotent; never raises."""
        conn, self._conn = self._conn, None
        self.pending = False
        self._done = True
        if conn is not None:
            try:
                conn.finish()
            except Exception:
                pass

    def reconnect(self) -> None:
        """close() + connect(); prepared statements are lost (server side)."""
        self.close()
        self.connect()

    @property
    def txn_status(self) -> str:
        """Map PQtransactionStatus to "I"/"T"/"E"/"A"/"U" (implemented once _conn exists)."""
        if self._conn is None:
            return "U"
        try:
            return _TXN.get(int(self._conn.transaction_status), "U")
        except Exception:
            return "U"

    def _conn_bad(self) -> bool:
        if self._conn is None:
            return True
        try:
            return int(self._conn.status) == _CONN_BAD
        except Exception:
            return True

    def _ensure(self) -> None:
        if self._conn is None or self._conn_bad():
            self.connect()

    # -- low-level driving --------------------------------------------------------------------
    def _wait(self, deadline: float, write: bool = False) -> bool:
        """select() on the socket until ``deadline``; consume input when readable. Returns
        False when the deadline has passed without the socket becoming ready."""
        conn = self._conn
        remaining = deadline - time.monotonic()
        fd = conn.socket
        r, w, _ = select.select([fd], [fd] if write else [], [], max(0.0, remaining))
        if r:
            conn.consume_input()
            return True
        if w:
            return True
        return remaining > 0

    def _flush(self, deadline: float) -> bool:
        conn = self._conn
        while True:
            if conn.flush() == 0:
                return True
            if not self._wait(deadline, write=True) and time.monotonic() >= deadline:
                return False

    def _drive(self, deadline: float) -> bool:
        """Advance the in-flight step, accumulating into self._acc. True when the step is
        complete (ReadyForQuery seen, or connection lost); False when ``deadline`` passed."""
        if self._done:
            return True
        conn = self._conn
        acc = self._acc
        self._deadline = deadline
        try:
            if self._need_flush:
                if not self._flush(deadline):
                    return False
                self._need_flush = False
            while True:
                if self._in_copy_data:
                    n, data = conn.get_copy_data(1)
                    if n > 0:
                        chunk = bytes(data)
                        row: Value = chunk if self._copy_binary else _value(chunk, 0)
                        assert self._copy_res is not None
                        self._copy_res.rows.append([row])
                        continue
                    if n == 0:
                        if not self._wait(deadline) and time.monotonic() >= deadline:
                            return False
                        continue
                    if n == -1:
                        self._in_copy_data = False
                        continue
                    raise ConnectionError("COPY data read failed")
                if conn.is_busy():
                    if not self._wait(deadline) and time.monotonic() >= deadline:
                        return False
                    continue
                res = conn.get_result()
                if res is None:
                    self._done = True
                    return True
                self._handle(res)
        except HarnessSessionError:
            raise
        except Exception:
            # socket EOF / consume_input failure: keep what we have
            acc.connection_lost = True
            self._done = True
            self._in_copy_data = False
            return True

    def _handle(self, res: Any) -> None:
        acc = self._acc
        status = int(res.status)
        if status == _COPY_OUT:
            self._copy_binary = bool(getattr(res, "binary_tuples", 0))
            sr = StatementResult(kind="copy_out")
            acc.results.append(sr)
            self._copy_res = sr
            self._in_copy_data = True
            return
        if status == _COPY_IN:
            conn = self._conn
            while conn.put_copy_end(_COPY_IN_MESSAGE) == 0:
                if not self._flush(self._deadline):
                    raise ConnectionError("cannot send CopyFail")
            if not self._flush(self._deadline):
                raise ConnectionError("cannot flush CopyFail")
            return
        if status == _COPY_BOTH:
            raise HarnessSessionError("COPY BOTH (replication) is not supported")
        if self._copy_res is not None:
            cr, self._copy_res = self._copy_res, None
            if status == _COMMAND_OK:
                cr.command_tag = _tag(res)
                return
        acc.results.append(result_from_pgresult(res, describe=self._mode == "describe"))

    def _drain_notices(self) -> None:
        if self._notice_buffer:
            self._acc.notices.extend(self._notice_buffer)
            self._notice_buffer.clear()

    def _begin(self, mode: str) -> None:
        if self.pending:
            raise HarnessSessionError(f"session {self.name!r}: async action still pending")
        self._ensure()
        self._acc = StepOutput()
        self._mode = mode
        self._done = False
        self._need_flush = True
        self._in_copy_data = False
        self._copy_res = None
        self._t0 = time.monotonic()
        self._drain_notices()

    def _launch(self, mode: str, send: Callable[[Any], None]) -> None:
        self._begin(mode)
        try:
            send(self._conn)
        except Exception as e:
            if self._conn_bad():
                self._acc.connection_lost = True
                self._done = True
                return
            raise HarnessSessionError(f"send failed: {e}") from e

    def _on_timeout(self) -> None:
        acc = self._acc
        keep = len(acc.results)
        self.cancel()
        self._drive(time.monotonic() + _CANCEL_GRACE_S)
        del acc.results[keep:]
        acc.timed_out = True
        self._done = True
        self.close()  # reconnects lazily on next use

    def _finalize(self) -> StepOutput:
        acc = self._acc
        self._drain_notices()
        if self._conn is not None and self._conn_bad():
            acc.connection_lost = True
        acc.txn_status = self.txn_status
        acc.elapsed_ms = int((time.monotonic() - self._t0) * 1000)
        self.pending = False
        self._done = True
        return acc

    def _complete(self) -> StepOutput:
        if not self._done and not self._drive(self._t0 + self.step_timeout_s):
            self._on_timeout()
        return self._finalize()

    def _run(self, mode: str, send: Callable[[Any], None]) -> StepOutput:
        self._launch(mode, send)
        return self._complete()

    # -- synchronous actions (each = one case step) -------------------------------------------
    def run_simple(self, sql: str) -> StepOutput:
        """Simple Query protocol (send_query). One StatementResult per server result."""
        q = _sql_bytes(sql)
        return self._run("sql", lambda c: c.send_query(q))

    def run_prepare(self, name: str, sql: str, param_types: Sequence[int]) -> StepOutput:
        """send_prepare; result is a single "command" result (command_tag None) or "error"."""
        n, q = name.encode("utf-8"), _sql_bytes(sql)
        types = [int(t) for t in param_types] or None
        return self._run("prepare", lambda c: c.send_prepare(n, q, param_types=types))

    def run_describe(self, name: str) -> StepOutput:
        """send_describe_prepared -> one "description" (or "error") result."""
        n = name.encode("utf-8")
        return self._run("describe", lambda c: c.send_describe_prepared(n))

    def run_execute(self, name: str, params: Sequence[Optional[str]],
                    param_formats: Sequence[str], result_format: str) -> StepOutput:
        """send_query_prepared. ``params`` strings: text format -> UTF-8 bytes; binary format
        -> bytes.fromhex(s). ``param_formats`` per protocol rule: [] all text, [f] applies to
        all, else one per param. result_format "text"=0, "binary"=1."""
        send = self._execute_sender(name, params, param_formats, result_format)
        return self._run("execute", send)

    def run_exec_params(self, sql: str, params: Sequence[Optional[str]],
                        param_types: Sequence[int], param_formats: Sequence[str],
                        result_format: str) -> StepOutput:
        """send_query_params (unnamed statement). Same param encoding as run_execute; missing
        param_types entries are 0 (server infers)."""
        send = self._exec_params_sender(sql, params, param_types, param_formats, result_format)
        return self._run("exec_params", send)

    def run_close(self, name: str) -> StepOutput:
        """send_close_prepared (libpq >= 17). If the method is missing raise
        HarnessSessionError("libpq < 17: Close message unsupported") — never emulate with
        DEALLOCATE, that would change the protocol under test."""
        self._ensure()
        if not hasattr(self._conn, "send_close_prepared"):
            raise HarnessSessionError("libpq < 17: Close message unsupported")
        n = name.encode("utf-8")
        return self._run("close", lambda c: c.send_close_prepared(n))

    # -- sender builders (validate/encode before any state changes) ---------------------------
    @staticmethod
    def _execute_sender(name: str, params: Sequence[Optional[str]],
                        param_formats: Sequence[str],
                        result_format: str) -> Callable[[Any], None]:
        n = name.encode("utf-8")
        vals, fmts = _encode_params(params, param_formats)
        rf = _result_format(result_format)
        return lambda c: c.send_query_prepared(n, vals, param_formats=fmts, result_format=rf)

    @staticmethod
    def _exec_params_sender(sql: str, params: Sequence[Optional[str]],
                            param_types: Sequence[int], param_formats: Sequence[str],
                            result_format: str) -> Callable[[Any], None]:
        q = _sql_bytes(sql)
        vals, fmts = _encode_params(params, param_formats)
        if len(param_types) > len(vals):
            raise HarnessSessionError(
                f"{len(param_types)} param types for {len(vals)} params")
        types = [int(t) for t in param_types] + [0] * (len(vals) - len(param_types))
        rf = _result_format(result_format)
        return lambda c: c.send_query_params(
            q, vals, param_types=types, param_formats=fmts, result_format=rf)

    # -- asynchronous actions (``async: true`` steps) -----------------------------------------
    def start(self, kind: str, payload: dict[str, Any]) -> None:
        """Send the action for kind in ("sql", "execute", "exec_params") without waiting.
        Sets ``pending``. Raises HarnessSessionError if already pending."""
        if self.pending:
            raise HarnessSessionError(f"session {self.name!r}: async action already pending")
        send: Callable[[Any], None]
        if kind == "sql":
            q = _sql_bytes(payload["sql"])
            send = lambda c: c.send_query(q)  # noqa: E731
        elif kind == "execute":
            send = self._execute_sender(
                payload["statement"], payload.get("params", []),
                payload.get("param_formats", []), payload.get("result_format", "text"))
        elif kind == "exec_params":
            send = self._exec_params_sender(
                payload["sql"], payload.get("params", []), payload.get("param_types", []),
                payload.get("param_formats", []), payload.get("result_format", "text"))
        else:
            raise HarnessSessionError(f"cannot start async action of kind {kind!r}")
        self._launch(kind, send)
        self.pending = True

    def poll_done(self, wait_s: float) -> bool:
        """Wait up to wait_s for the pending action to have ALL results available
        (is_busy() False after consume_input). Returns True if done. Does not consume
        results.

        Results are parsed into the session's internal accumulator (so "all available" is
        decidable), but nothing is returned to the caller until ``finish_pending``."""
        if not self.pending:
            raise HarnessSessionError(f"session {self.name!r}: no pending action")
        return self._drive(time.monotonic() + max(0.0, wait_s))

    def finish_pending(self) -> StepOutput:
        """Collect all results of the pending action (with step_timeout_s deadline counted
        from this call), clear ``pending``."""
        if not self.pending:
            raise HarnessSessionError(f"session {self.name!r}: no pending action")
        if not self._done and not self._drive(time.monotonic() + self.step_timeout_s):
            self._on_timeout()
        return self._finalize()

    def cancel(self) -> None:
        """Send a CancelRequest for the in-flight query (best effort, never raises)."""
        conn = self._conn
        if conn is None:
            return
        try:
            c = conn.get_cancel()
            try:
                c.cancel()
            finally:
                try:
                    c.free()
                except Exception:
                    pass
        except Exception:
            pass
