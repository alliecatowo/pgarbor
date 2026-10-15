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

from dataclasses import dataclass, field
from typing import Any, Optional, Sequence

from ..model import Endpoint, Notice, StatementResult


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

    # -- connection ---------------------------------------------------------------------------
    def connect(self) -> None:
        """PGconn.connect(endpoint.conninfo().encode()); status BAD -> raise
        ConnectionError(error_message). Install notice_handler; set nonblocking."""
        raise NotImplementedError("HAR-09: connect")

    def close(self) -> None:
        """finish() if connected; idempotent; never raises."""
        raise NotImplementedError("HAR-09: close")

    def reconnect(self) -> None:
        """close() + connect(); prepared statements are lost (server side)."""
        raise NotImplementedError("HAR-09: reconnect")

    @property
    def txn_status(self) -> str:
        """Map PQtransactionStatus to "I"/"T"/"E"/"A"/"U" (implemented once _conn exists)."""
        raise NotImplementedError("HAR-09: transaction status")

    # -- synchronous actions (each = one case step) -------------------------------------------
    def run_simple(self, sql: str) -> StepOutput:
        """Simple Query protocol (send_query). One StatementResult per server result."""
        raise NotImplementedError("HAR-09: simple query")

    def run_prepare(self, name: str, sql: str, param_types: Sequence[int]) -> StepOutput:
        """send_prepare; result is a single "command" result (command_tag None) or "error"."""
        raise NotImplementedError("HAR-09: prepare")

    def run_describe(self, name: str) -> StepOutput:
        """send_describe_prepared -> one "description" (or "error") result."""
        raise NotImplementedError("HAR-09: describe prepared")

    def run_execute(self, name: str, params: Sequence[Optional[str]],
                    param_formats: Sequence[str], result_format: str) -> StepOutput:
        """send_query_prepared. ``params`` strings: text format -> UTF-8 bytes; binary format
        -> bytes.fromhex(s). ``param_formats`` per protocol rule: [] all text, [f] applies to
        all, else one per param. result_format "text"=0, "binary"=1."""
        raise NotImplementedError("HAR-09: execute prepared")

    def run_exec_params(self, sql: str, params: Sequence[Optional[str]],
                        param_types: Sequence[int], param_formats: Sequence[str],
                        result_format: str) -> StepOutput:
        """send_query_params (unnamed statement). Same param encoding as run_execute; missing
        param_types entries are 0 (server infers)."""
        raise NotImplementedError("HAR-09: exec params")

    def run_close(self, name: str) -> StepOutput:
        """send_close_prepared (libpq >= 17). If the method is missing raise
        HarnessSessionError("libpq < 17: Close message unsupported") — never emulate with
        DEALLOCATE, that would change the protocol under test."""
        raise NotImplementedError("HAR-09: close prepared")

    # -- asynchronous actions (``async: true`` steps) -----------------------------------------
    def start(self, kind: str, payload: dict[str, Any]) -> None:
        """Send the action for kind in ("sql", "execute", "exec_params") without waiting.
        Sets ``pending``. Raises HarnessSessionError if already pending."""
        raise NotImplementedError("HAR-09: start async action")

    def poll_done(self, wait_s: float) -> bool:
        """Wait up to wait_s for the pending action to have ALL results available
        (is_busy() False after consume_input). Returns True if done. Does not consume
        results."""
        raise NotImplementedError("HAR-09: poll pending")

    def finish_pending(self) -> StepOutput:
        """Collect all results of the pending action (with step_timeout_s deadline counted
        from this call), clear ``pending``."""
        raise NotImplementedError("HAR-09: finish pending")

    def cancel(self) -> None:
        """Send a CancelRequest for the in-flight query (best effort, never raises)."""
        raise NotImplementedError("HAR-09: cancel")
