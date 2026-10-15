"""ORACLE-08: replay a recorded client transcript against a target and compare server messages.

Lease HAR-25.

Inputs: a recorded transcript (proxy.py format) from ``tests/expected/wire/<name>.jsonl``
(recorded from the PG oracle through the oracle review path).

Replay per connection ``cN`` (connections replayed sequentially in first-seen order; a
transcript with overlapping connections replays them concurrently, one thread each, keeping
per-connection order):
  * open a TCP socket to the target; send each frontend message (messages.encode) in order;
  * after sending a message that the recorded stream shows was followed by backend messages
    before the next frontend message, read backend messages until the same COUNT of messages
    has been received or a ReadyForQuery is seen when the recording also ended that run with
    ReadyForQuery (timeout config.STEP_TIMEOUT_S -> HANG);
  * the CancelRequest connection: rewrite pid/secret with the values of the replayed
    connection's BackendKeyData.

Comparison of backend messages (recorded PG vs target), in order, with these documented
normalizations ONLY:
  * BackendKeyData: pid and secret ignored (random per connection);
  * ParameterStatus: compared exactly for ``server_encoding``, ``client_encoding``,
    ``DateStyle``, ``IntervalStyle``, ``TimeZone``, ``integer_datetimes``,
    ``standard_conforming_strings``, ``is_superuser``, ``session_authorization``,
    ``application_name``, ``default_transaction_read_only``, ``in_hot_standby``,
    ``scram_iterations``, ``search_path``; ``server_version`` compared exactly too (clients
    parse it; PGArbor must report a PG18-compatible version string — the expected value is
    the oracle's ``18.6 (...)``; a difference is a MISMATCH that the wire lease owner must
    resolve by amendment, not a normalization). The ORDER of ParameterStatus messages within
    the startup burst is NOT compared (PG sends them in GUC-table order; clients must not
    depend on it) — compare as a set keyed by name;
  * RowDescription: table OID and type OIDs >= 16384 canonicalized like
    compare.normalize ``user_oids`` (bijective per transcript);
  * ErrorResponse/NoticeResponse: fields ``F`` (file), ``L`` (line), ``R`` (routine) ignored
    (source locations of the C implementation); all other fields exact.
Everything else (message types, order, CommandComplete tags, DataRow bytes, ReadyForQuery
status) must be identical.

Outcome: PASS / MISMATCH (first differing message index + both decoded messages) / CRASH
(connection reset before the recorded end) / HANG.

Implementation notes (HAR-25):
  * frontend messages are sent in the recorded global ``seq`` order across all connections
    (a connection waits for its turn), so a CancelRequest is never sent before the query it
    cancels and the first-appearance order of user OIDs is deterministic;
  * the message index reported in a MISMATCH is the 0-based index among the backend messages
    of that connection (the recorded stream's order); ``connection`` names the connection;
  * the ParameterStatus run order is normalized by sorting each contiguous run of
    ParameterStatus messages by their canonical JSON, so the comparison is a multiset
    comparison keyed by (name, value);
  * decoded field names are assumed from messages.py: ``pid`` / ``process_id`` / ``secret*``
    are the BackendKeyData identity fields, ``F``/``L``/``R`` the notice field codes (either
    top level or under ``fields``), and user OIDs are integer values under keys containing
    ``oid``.
"""

from __future__ import annotations

import copy
import json
import socket
import threading
import time
from collections import deque
from dataclasses import dataclass, field
from pathlib import Path
from typing import Any, Optional

from .. import config
from ..model import Outcome
from . import messages

TRANSCRIPT_FORMAT = "pgarbor-wire-transcript/1"

#: Object identifiers >= FirstNormalObjectId (16384) are allocation-dependent.
_FIRST_NORMAL_OID = 16384
#: Canonical OID numbering base (same as compare.normalize ``user_oids``).
_CANON_OID_BASE = 1_000_000_000
_RECV_CHUNK = 65536
_SSL_TYPES = frozenset({"SSLRequest", "GSSENCRequest"})
_NOTICE_FIELDS_IGNORED = frozenset({"F", "L", "R"})
_BKD_IGNORED_KEYS = frozenset({"pid", "process_id"})
_RFQ = "ReadyForQuery"
_PARAM_STATUS = "ParameterStatus"
_BKD = "BackendKeyData"


@dataclass
class WireReplayResult:
    name: str
    outcome: Outcome
    reason: str = ""
    connection: Optional[str] = None
    message_index: Optional[int] = None
    expected: Optional[dict[str, Any]] = None
    actual: Optional[dict[str, Any]] = None
    notes: list[str] = field(default_factory=list)


def load_transcript(path: Path) -> tuple[dict[str, Any], list[dict[str, Any]]]:
    """(header, records) from a JSONL transcript; validates the header format."""
    text = Path(path).read_text(encoding="utf-8", errors="surrogateescape")
    lines = [line for line in text.splitlines() if line.strip()]
    if not lines:
        raise ValueError(f"{path}: empty transcript (no header line)")
    try:
        header = json.loads(lines[0])
    except json.JSONDecodeError as exc:
        raise ValueError(f"{path}: header line is not JSON: {exc}") from None
    if not isinstance(header, dict) or header.get("format") != TRANSCRIPT_FORMAT:
        raise ValueError(f"{path}: header format is not {TRANSCRIPT_FORMAT!r}")
    records: list[dict[str, Any]] = []
    for lineno, line in enumerate(lines[1:], start=2):
        try:
            rec = json.loads(line)
        except json.JSONDecodeError as exc:
            raise ValueError(f"{path}:{lineno}: invalid JSON: {exc}") from None
        if (not isinstance(rec, dict) or not isinstance(rec.get("conn"), str)
                or type(rec.get("seq")) is not int or rec.get("dir") not in ("F", "B")
                or not isinstance(rec.get("msg"), dict)):
            raise ValueError(f"{path}:{lineno}: malformed transcript record")
        records.append(rec)
    records.sort(key=lambda r: r["seq"])
    return header, records


def _is_bkd_ignored_key(key: Any) -> bool:
    return isinstance(key, str) and (key in _BKD_IGNORED_KEYS or key.startswith("secret"))


def _canon_oid(value: int, oid_map: dict[int, int]) -> int:
    canon = oid_map.get(value)
    if canon is None:
        canon = _CANON_OID_BASE + len(oid_map) + 1
        oid_map[value] = canon
    return canon


def _canonicalize_oids(obj: Any, oid_map: dict[int, int]) -> Any:
    """Copy ``obj``, replacing integer values under ``*oid*`` keys that are >= 16384 by their
    canonical number, numbering distinct OIDs in first-appearance (walk) order."""
    if isinstance(obj, dict):
        out: dict[Any, Any] = {}
        for key, value in obj.items():
            if (isinstance(key, str) and "oid" in key.lower() and type(value) is int
                    and value >= _FIRST_NORMAL_OID):
                out[key] = _canon_oid(value, oid_map)
            else:
                out[key] = _canonicalize_oids(value, oid_map)
        return out
    if isinstance(obj, list):
        return [_canonicalize_oids(item, oid_map) for item in obj]
    return copy.deepcopy(obj)


def _normalize_notice(msg: dict[str, Any]) -> dict[str, Any]:
    out: dict[str, Any] = {}
    for key, value in msg.items():
        if key in _NOTICE_FIELDS_IGNORED:
            continue
        if key == "fields" and isinstance(value, dict):
            out[key] = {fk: copy.deepcopy(fv) for fk, fv in value.items()
                        if fk not in _NOTICE_FIELDS_IGNORED}
        else:
            out[key] = copy.deepcopy(value)
    return out


def normalize_backend(msg: dict[str, Any], oid_map: dict[int, int]) -> dict[str, Any]:
    """Apply the documented normalizations to one decoded backend message (returns a copy)."""
    kind = msg.get("type")
    if kind == _BKD:
        return {k: copy.deepcopy(v) for k, v in msg.items() if not _is_bkd_ignored_key(k)}
    if kind in ("ErrorResponse", "NoticeResponse"):
        return _normalize_notice(msg)
    if kind == "RowDescription":
        return _canonicalize_oids(msg, oid_map)
    return copy.deepcopy(msg)


# ---------------------------------------------------------------------------------------------
# Transcript structure
# ---------------------------------------------------------------------------------------------

_Pair = tuple[dict[str, Any], dict[str, Any]]  # (normalized, raw)


@dataclass
class _Group:
    """One frontend message and the backend messages recorded before the next frontend one."""

    seq: int
    frontend: dict[str, Any]
    backend: list[_Pair] = field(default_factory=list)


@dataclass
class _Conn:
    name: str
    groups: list[_Group] = field(default_factory=list)


def _build_connections(records: list[dict[str, Any]]) -> dict[str, _Conn]:
    """Group records per connection (first-seen order). Expected backend messages are
    normalized here, in global seq order, so the recorded OID numbering is deterministic."""
    conns: dict[str, _Conn] = {}
    expected_oids: dict[int, int] = {}
    for rec in records:
        name = rec["conn"]
        seq = rec["seq"]
        msg = rec["msg"]
        conn = conns.get(name)
        if conn is None:
            conn = _Conn(name=name)
            conns[name] = conn
        if rec["dir"] == "F":
            if "redacted" in msg:
                raise ValueError(f"{name}: frontend message at seq {seq} is redacted; "
                                 "the transcript is not replayable")
            conn.groups.append(_Group(seq=seq, frontend=msg))
        else:
            if not conn.groups:
                raise ValueError(f"{name}: backend message at seq {seq} precedes any "
                                 "frontend message")
            conn.groups[-1].backend.append((normalize_backend(msg, expected_oids), msg))
    return conns


# ---------------------------------------------------------------------------------------------
# Shared replay state
# ---------------------------------------------------------------------------------------------


@dataclass
class _ConnOutcome:
    conn: str
    outcome: Outcome
    reason: str = ""
    index: Optional[int] = None
    expected: Optional[dict[str, Any]] = None
    actual: Optional[dict[str, Any]] = None
    compared: int = 0


class _Shared:
    """State shared by the connection threads, guarded by one condition variable."""

    def __init__(self, conns: dict[str, _Conn]):
        self.cond = threading.Condition()
        #: target-side OID numbering (per transcript, first-appearance order)
        self.actual_oids: dict[int, int] = {}
        #: recorded BackendKeyData pid -> name of the connection that reported it
        self.owner_by_pid: dict[Any, str] = {}
        #: target BackendKeyData per connection name
        self._keys: dict[str, dict[str, Any]] = {}
        #: frontend seqs in global send order, and the index of the next one to send
        self._order = sorted(g.seq for c in conns.values() for g in c.groups)
        self._pos = 0
        self.aborted = False
        self.first_failure: Optional[_ConnOutcome] = None
        for name, conn in conns.items():
            for group in conn.groups:
                for _norm, raw in group.backend:
                    if raw.get("type") == _BKD:
                        pid = raw.get("pid")
                        if pid is not None:
                            self.owner_by_pid.setdefault(pid, name)

    def wait_turn(self, seq: int) -> bool:
        """Block until every frontend message with a smaller seq has been sent."""
        with self.cond:
            ok = self.cond.wait_for(lambda: self.aborted or self._order[self._pos] == seq,
                                    timeout=config.CASE_TIMEOUT_S)
            return bool(ok) and not self.aborted

    def advance(self) -> None:
        with self.cond:
            self._pos += 1
            self.cond.notify_all()

    def publish_key(self, name: str, raw: dict[str, Any]) -> None:
        with self.cond:
            self._keys.setdefault(name, raw)
            self.cond.notify_all()

    def key_for(self, name: str) -> Optional[dict[str, Any]]:
        with self.cond:
            self.cond.wait_for(lambda: name in self._keys or self.aborted,
                               timeout=config.STEP_TIMEOUT_S)
            return self._keys.get(name)

    def fail(self, outcome: _ConnOutcome) -> None:
        """Record the first failure and abort the other connections."""
        with self.cond:
            if self.first_failure is None:
                self.first_failure = outcome
            self.aborted = True
            self.cond.notify_all()


# ---------------------------------------------------------------------------------------------
# Per-connection replay
# ---------------------------------------------------------------------------------------------


class _Fail(Exception):
    def __init__(self, outcome: Outcome, reason: str, index: Optional[int] = None,
                 expected: Optional[dict[str, Any]] = None,
                 actual: Optional[dict[str, Any]] = None):
        super().__init__(reason)
        self.outcome = outcome
        self.reason = reason
        self.index = index
        self.expected = expected
        self.actual = actual


def _pair_key(pair: _Pair) -> str:
    return json.dumps(pair[0], sort_keys=True, default=repr)


def _canonical_order(pairs: list[_Pair]) -> list[_Pair]:
    """Sort each contiguous run of ParameterStatus messages (startup order is not compared)."""
    out: list[_Pair] = []
    run: list[_Pair] = []
    for pair in pairs:
        if pair[0].get("type") == _PARAM_STATUS:
            run.append(pair)
            continue
        if run:
            run.sort(key=_pair_key)
            out.extend(run)
            run = []
        out.append(pair)
    if run:
        run.sort(key=_pair_key)
        out.extend(run)
    return out


def _first_difference(expected: list[_Pair], actual: list[_Pair], base_index: int,
                      ) -> Optional[tuple[int, Optional[dict[str, Any]], Optional[dict[str, Any]]]]:
    exp = _canonical_order(expected)
    act = _canonical_order(actual)
    for i in range(max(len(exp), len(act))):
        e = exp[i] if i < len(exp) else None
        a = act[i] if i < len(act) else None
        if (e[0] if e else None) != (a[0] if a else None):
            return base_index + i, (e[1] if e else None), (a[1] if a else None)
    return None


class _Session:
    def __init__(self, sock: socket.socket, shared: _Shared, conn: _Conn):
        self.sock = sock
        self.shared = shared
        self.conn = conn
        self.decoder = messages.StreamDecoder(frontend=False)
        self.pending: deque[dict[str, Any]] = deque()
        #: backend messages compared so far on this connection
        self.index = 0

    def run(self) -> _ConnOutcome:
        try:
            self._play()
        except _Fail as fail:
            return _ConnOutcome(self.conn.name, fail.outcome, fail.reason, fail.index,
                                fail.expected, fail.actual, self.index)
        return _ConnOutcome(self.conn.name, Outcome.PASS, compared=self.index)

    def _play(self) -> None:
        for group in self.conn.groups:
            frame = self._frame(group.frontend)
            if group.frontend.get("type") in _SSL_TYPES:
                self.decoder.expect_ssl_answer()
            if not self.shared.wait_turn(group.seq):
                raise _Fail(Outcome.HANG, "replay aborted or an earlier frontend message "
                            f"(seq < {group.seq}, another connection) was never sent")
            try:
                self.sock.sendall(frame)
            except OSError as exc:
                raise _Fail(Outcome.CRASH, "target reset the connection before the recorded "
                            f"end (send failed: {exc})") from None
            self.shared.advance()
            if group.backend:
                self._expect(group)
        # Recorded end: a close is fine, anything more from the target is a difference.
        self._settle()
        if self.pending:
            raise _Fail(Outcome.MISMATCH,
                        "target sent a backend message the recording does not contain",
                        index=self.index, expected=None, actual=self.pending[0])

    def _frame(self, frontend: dict[str, Any]) -> bytes:
        msg = frontend
        if msg.get("type") == "CancelRequest":
            owner = self.shared.owner_by_pid.get(msg.get("pid"))
            if owner is None:
                raise _Fail(Outcome.INVALID_CASE, f"CancelRequest pid {msg.get('pid')!r} "
                            "matches no recorded BackendKeyData")
            key = self.shared.key_for(owner)
            if key is None:
                raise _Fail(Outcome.HANG, f"{owner} reported no BackendKeyData within "
                            f"{config.STEP_TIMEOUT_S}s, so its CancelRequest cannot be replayed")
            msg = dict(msg)
            for k, v in key.items():
                if k != "type":
                    msg[k] = v
        try:
            return messages.encode(msg)
        except Exception as exc:  # file data: report, never crash the harness
            raise _Fail(Outcome.INVALID_CASE, f"cannot encode recorded "
                        f"{frontend.get('type')!r} message: {exc}") from None

    def _recv_chunk(self, wanted: int) -> None:
        try:
            data = self.sock.recv(_RECV_CHUNK)
        except socket.timeout:
            raise _Fail(Outcome.HANG, f"no backend message within {config.STEP_TIMEOUT_S}s "
                        f"(recording expects {wanted} more)", index=self.index) from None
        except OSError as exc:
            raise _Fail(Outcome.CRASH, "connection reset by target before the recorded end: "
                        f"{exc}") from None
        if not data:
            raise _Fail(Outcome.CRASH, "target closed the connection before the recorded end "
                        f"(recording expects {wanted} more backend message(s))", index=self.index)
        self.pending.extend(self.decoder.feed(data))

    def _settle(self) -> None:
        """Give the target config.BLOCK_SETTLE_S to send more after the recorded end. Data is
        queued in ``pending``; a close or reset here is not a failure."""
        deadline = time.monotonic() + config.BLOCK_SETTLE_S
        try:
            while True:
                remaining = deadline - time.monotonic()
                if remaining <= 0:
                    return
                self.sock.settimeout(remaining)
                try:
                    data = self.sock.recv(_RECV_CHUNK)
                except OSError:  # includes socket.timeout
                    return
                if not data:
                    return
                self.pending.extend(self.decoder.feed(data))
        finally:
            self.sock.settimeout(config.STEP_TIMEOUT_S)

    def _expect(self, group: _Group) -> None:
        want = len(group.backend)
        rfq_expected = sum(1 for _norm, raw in group.backend if raw.get("type") == _RFQ)
        stop_on_rfq = group.backend[-1][1].get("type") == _RFQ
        rfq_seen = 0
        got: list[dict[str, Any]] = []
        while len(got) < want:
            if not self.pending:
                self._recv_chunk(want - len(got))
                continue
            msg = self.pending.popleft()
            got.append(msg)
            if msg.get("type") == _RFQ:
                rfq_seen += 1
            if stop_on_rfq and rfq_seen >= rfq_expected:
                break
        for msg in got:
            if msg.get("type") == _BKD:
                self.shared.publish_key(self.conn.name, msg)
                break
        with self.shared.cond:
            actual = [(normalize_backend(m, self.shared.actual_oids), m) for m in got]
        diff = _first_difference(group.backend, actual, self.index)
        if diff is not None:
            idx, exp, act = diff
            raise _Fail(Outcome.MISMATCH, f"backend message {idx} differs from the recording",
                        index=idx, expected=exp, actual=act)
        self.index += want


def _run_connection(conn: _Conn, host: str, port: int, shared: _Shared) -> _ConnOutcome:
    try:
        sock = socket.create_connection((host, port), timeout=config.CONNECT_TIMEOUT_S)
    except OSError as exc:
        return _ConnOutcome(conn.name, Outcome.CRASH, f"cannot connect to {host}:{port}: {exc}")
    try:
        sock.settimeout(config.STEP_TIMEOUT_S)
        return _Session(sock, shared, conn).run()
    except Exception as exc:  # internal error: surface it, never masquerade as a target result
        return _ConnOutcome(conn.name, Outcome.INVALID_CASE, f"internal replay error: {exc!r}")
    finally:
        sock.close()


def _run_all(conns: list[_Conn], host: str, port: int,
             shared: _Shared) -> list[_ConnOutcome]:
    results: list[Optional[_ConnOutcome]] = [None] * len(conns)

    def worker(i: int, conn: _Conn) -> None:
        outcome = _run_connection(conn, host, port, shared)
        results[i] = outcome
        if outcome.outcome is not Outcome.PASS:
            shared.fail(outcome)

    threads = [threading.Thread(target=worker, args=(i, c), daemon=True)
               for i, c in enumerate(conns)]
    for thread in threads:
        thread.start()
    for thread in threads:
        thread.join()
    return [r if r is not None else _ConnOutcome(conns[i].name, Outcome.INVALID_CASE,
                                                 "replay worker produced no result")
            for i, r in enumerate(results)]


def replay(path: Path, host: str, port: int) -> WireReplayResult:
    """Replay one transcript against host:port and compare."""
    path = Path(path)
    name = path.stem
    try:
        _header, records = load_transcript(path)
        conns = _build_connections(records)
    except (OSError, ValueError) as exc:
        return WireReplayResult(name=name, outcome=Outcome.INVALID_CASE,
                                reason=f"unreadable transcript: {exc}")
    if not conns:
        return WireReplayResult(name=name, outcome=Outcome.INVALID_CASE,
                                reason="transcript has no connections")
    shared = _Shared(conns)
    outcomes = _run_all(list(conns.values()), host, port, shared)
    notes = [f"{len(outcomes)} connection(s) replayed"]
    failure = shared.first_failure
    if failure is not None:
        return WireReplayResult(name=name, outcome=failure.outcome,
                                reason=f"{failure.conn}: {failure.reason}",
                                connection=failure.conn, message_index=failure.index,
                                expected=failure.expected, actual=failure.actual, notes=notes)
    compared = sum(o.compared for o in outcomes)
    return WireReplayResult(name=name, outcome=Outcome.PASS,
                            notes=notes + [f"{compared} backend message(s) compared"])
