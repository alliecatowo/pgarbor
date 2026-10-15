"""HAR-25 tests: wire transcript replay (ORACLE-08).

Each test replays a hand-built transcript against an in-process fake target that answers with
recorded backend bytes. Wire bytes are built from the protocol layout and decoded with the
public codec (messages.decode_*), so these tests do not depend on decoded field names.
"""

from __future__ import annotations

import json
import socket
import struct
import threading
from pathlib import Path
from typing import Any

import pytest

from pgarbor_harness import config
from pgarbor_harness.model import Outcome
from pgarbor_harness.wire_transcripts import messages
from pgarbor_harness.wire_transcripts import replay as rp


@pytest.fixture(autouse=True)
def _fast_timeouts(monkeypatch):
    monkeypatch.setattr(config, "BLOCK_SETTLE_S", 0.05)
    monkeypatch.setattr(config, "STEP_TIMEOUT_S", 3.0)


# ---------------------------------------------------------------------------------------------
# Raw wire builders (protocol-message-formats layout)
# ---------------------------------------------------------------------------------------------


def _typed(tag: bytes, body: bytes) -> bytes:
    return tag + struct.pack("!i", len(body) + 4) + body


def _startup() -> bytes:
    body = struct.pack("!i", 196608) + b"user\x00pgarbor\x00database\x00pgarbor\x00\x00"
    return struct.pack("!i", len(body) + 4) + body


def _query(sql: str) -> bytes:
    return _typed(b"Q", sql.encode() + b"\x00")


def _auth_ok() -> bytes:
    return _typed(b"R", struct.pack("!i", 0))


def _bkd(pid: int, secret: int) -> bytes:
    return _typed(b"K", struct.pack("!ii", pid, secret))


def _param(name: str, value: str) -> bytes:
    return _typed(b"S", name.encode() + b"\x00" + value.encode() + b"\x00")


def _cc(tag: str) -> bytes:
    return _typed(b"C", tag.encode() + b"\x00")


def _rfq(status: bytes = b"I") -> bytes:
    return _typed(b"Z", status)


def _exchange(conn: str, front_raw: bytes, back_raws: list[bytes],
              startup: bool = False) -> list[tuple[str, str, dict[str, Any]]]:
    """Transcript steps for one frontend message and the backend messages recorded after it."""
    front = messages.decode_frontend(front_raw, startup)
    steps: list[tuple[str, str, dict[str, Any]]] = [(conn, "F", front)]
    steps += [(conn, "B", messages.decode_backend(raw)) for raw in back_raws]
    return steps


def _write_transcript(path: Path, steps: list[tuple[str, str, dict[str, Any]]]) -> Path:
    lines = [json.dumps({"format": "pgarbor-wire-transcript/1", "client": "test",
                         "upstream": "pg18-oracle",
                         "image_digest": config.PG_IMAGE_DIGEST})]
    for seq, (conn, direction, msg) in enumerate(steps, start=1):
        lines.append(json.dumps({"conn": conn, "seq": seq, "dir": direction, "msg": msg}))
    path.write_text("\n".join(lines) + "\n", encoding="utf-8")
    return path


class _FakeTarget:
    """Accepts one connection. ``script`` is a list of (frame, reply) steps: the fake reads
    exactly len(frame) bytes (the frontend message the replay is expected to send), then sends
    ``reply`` (raw bytes). With close_after=k it closes right after step k's reply."""

    def __init__(self, script: list[tuple[bytes, bytes]], close_after: int | None = None):
        self._script = script
        self._close_after = close_after
        self._listener = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
        self._listener.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
        self._listener.bind(("127.0.0.1", 0))
        self._listener.listen(1)
        self._listener.settimeout(5.0)
        self.port = self._listener.getsockname()[1]
        self._thread = threading.Thread(target=self._serve, daemon=True)

    def __enter__(self) -> "_FakeTarget":
        self._thread.start()
        return self

    def __exit__(self, *exc_info: object) -> None:
        self._thread.join(timeout=10)
        self._listener.close()

    def _serve(self) -> None:
        try:
            conn, _ = self._listener.accept()
        except OSError:
            return
        with conn:
            conn.settimeout(5.0)
            try:
                for index, (frame, reply) in enumerate(self._script):
                    got = 0
                    while got < len(frame):
                        data = conn.recv(len(frame) - got)
                        if not data:
                            return
                        got += len(data)
                    conn.sendall(reply)
                    if self._close_after == index:
                        return
                while conn.recv(65536):
                    pass
            except OSError:
                return


# ---------------------------------------------------------------------------------------------
# Replay scenarios
# ---------------------------------------------------------------------------------------------


def test_identical_exchange_passes(tmp_path: Path) -> None:
    path = _write_transcript(tmp_path / "simple.jsonl",
                             _exchange("c1", _query("SELECT 1"), [_cc("SELECT 1"), _rfq()]))
    with _FakeTarget([(_query("SELECT 1"), _cc("SELECT 1") + _rfq())]) as target:
        result = rp.replay(path, "127.0.0.1", target.port)
    assert result.outcome is Outcome.PASS, result.reason


def test_changed_command_complete_tag_mismatches_at_that_index(tmp_path: Path) -> None:
    path = _write_transcript(tmp_path / "tag.jsonl",
                             _exchange("c1", _query("SELECT 1"), [_cc("SELECT 1"), _rfq()]))
    with _FakeTarget([(_query("SELECT 1"), _cc("SELECT 2") + _rfq())]) as target:
        result = rp.replay(path, "127.0.0.1", target.port)
    assert result.outcome is Outcome.MISMATCH, result.reason
    assert result.connection == "c1"
    assert result.message_index == 0
    assert result.expected is not None and result.expected["type"] == "CommandComplete"
    assert result.actual is not None and result.actual["type"] == "CommandComplete"


def test_different_backend_key_data_still_passes(tmp_path: Path) -> None:
    recorded = [_auth_ok(), _bkd(1234, 555),
                _param("server_version", "18.6 (Debian 18.6-1.pgdg13+2)"),
                _param("TimeZone", "UTC"), _rfq()]
    path = _write_transcript(tmp_path / "startup.jsonl",
                             _exchange("c1", _startup(), recorded, startup=True))
    # Target: different pid/secret and the two ParameterStatus messages in the other order.
    actual = [_auth_ok(), _bkd(9999, 42), _param("TimeZone", "UTC"),
              _param("server_version", "18.6 (Debian 18.6-1.pgdg13+2)"), _rfq()]
    with _FakeTarget([(_startup(), b"".join(actual))]) as target:
        result = rp.replay(path, "127.0.0.1", target.port)
    assert result.outcome is Outcome.PASS, result.reason


def test_parameter_status_value_change_mismatches(tmp_path: Path) -> None:
    recorded = [_auth_ok(), _param("TimeZone", "UTC"), _rfq()]
    path = _write_transcript(tmp_path / "tz.jsonl",
                             _exchange("c1", _startup(), recorded, startup=True))
    actual = [_auth_ok(), _param("TimeZone", "America/New_York"), _rfq()]
    with _FakeTarget([(_startup(), b"".join(actual))]) as target:
        result = rp.replay(path, "127.0.0.1", target.port)
    assert result.outcome is Outcome.MISMATCH, result.reason
    assert result.message_index == 1


def test_reset_before_recorded_end_crashes(tmp_path: Path) -> None:
    steps = (_exchange("c1", _query("SELECT 1"), [_cc("SELECT 1"), _rfq()])
             + _exchange("c1", _query("SELECT 2"), [_cc("SELECT 2"), _rfq()]))
    path = _write_transcript(tmp_path / "reset.jsonl", steps)
    script = [(_query("SELECT 1"), _cc("SELECT 1") + _rfq()),
              (_query("SELECT 2"), _cc("SELECT 2") + _rfq())]
    with _FakeTarget(script, close_after=0) as target:
        result = rp.replay(path, "127.0.0.1", target.port)
    assert result.outcome is Outcome.CRASH, result.reason


def test_silent_target_hangs(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
    monkeypatch.setattr(config, "STEP_TIMEOUT_S", 0.5)
    path = _write_transcript(tmp_path / "hang.jsonl",
                             _exchange("c1", _query("SELECT 1"), [_cc("SELECT 1"), _rfq()]))
    with _FakeTarget([(_query("SELECT 1"), b"")]) as target:
        result = rp.replay(path, "127.0.0.1", target.port)
    assert result.outcome is Outcome.HANG, result.reason


def test_extra_backend_message_is_a_mismatch(tmp_path: Path) -> None:
    path = _write_transcript(tmp_path / "extra.jsonl",
                             _exchange("c1", _query("SELECT 1"), [_cc("SELECT 1"), _rfq()]))
    with _FakeTarget([(_query("SELECT 1"), _cc("SELECT 1") + _rfq() + _cc("SELECT 1"))]) as target:
        result = rp.replay(path, "127.0.0.1", target.port)
    assert result.outcome is Outcome.MISMATCH, result.reason
    assert result.expected is None


def test_pipelined_queries_with_two_ready_for_query_pass(tmp_path: Path) -> None:
    # Client writes Q1 and Q2 in one burst; the recording holds all replies after Q2.
    steps = (_exchange("c1", _query("SELECT 1"), [])
             + _exchange("c1", _query("SELECT 2"),
                         [_cc("SELECT 1"), _rfq(), _cc("SELECT 2"), _rfq()]))
    path = _write_transcript(tmp_path / "pipelined.jsonl", steps)
    script = [(_query("SELECT 1"), b""),
              (_query("SELECT 2"), _cc("SELECT 1") + _rfq() + _cc("SELECT 2") + _rfq())]
    with _FakeTarget(script) as target:
        result = rp.replay(path, "127.0.0.1", target.port)
    assert result.outcome is Outcome.PASS, result.reason


def test_redacted_secret_is_not_replayable(tmp_path: Path) -> None:
    path = _write_transcript(tmp_path / "redacted.jsonl", [("c1", "F", {"redacted": True})])
    result = rp.replay(path, "127.0.0.1", 1)
    assert result.outcome is Outcome.INVALID_CASE, result.reason


def test_bad_header_is_invalid_case(tmp_path: Path) -> None:
    path = tmp_path / "bad.jsonl"
    path.write_text('{"format": "something-else/9"}\n', encoding="utf-8")
    with pytest.raises(ValueError):
        rp.load_transcript(path)
    result = rp.replay(path, "127.0.0.1", 1)
    assert result.outcome is Outcome.INVALID_CASE, result.reason


# ---------------------------------------------------------------------------------------------
# Normalization units
# ---------------------------------------------------------------------------------------------


def test_backend_key_data_normalizes_pid_and_secret_away() -> None:
    a = rp.normalize_backend(messages.decode_backend(_bkd(1, 2)), {})
    b = rp.normalize_backend(messages.decode_backend(_bkd(3, 4)), {})
    assert a == b


def test_row_description_oids_are_canonicalized_per_transcript() -> None:
    row_a = {"type": "RowDescription",
             "fields": [{"name": "a", "table_oid": 16500, "type_oid": 16600}]}
    row_b = {"type": "RowDescription",
             "fields": [{"name": "a", "table_oid": 20001, "type_oid": 20002}]}
    assert rp.normalize_backend(row_a, {}) == rp.normalize_backend(row_b, {})


def test_builtin_oids_are_not_canonicalized() -> None:
    row = {"type": "RowDescription", "fields": [{"name": "a", "type_oid": 23}]}
    assert rp.normalize_backend(row, {})["fields"][0]["type_oid"] == 23
