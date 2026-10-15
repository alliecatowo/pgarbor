"""HAR-24: recording TCP proxy (wire transcripts). Runs centrally; needs HAR-23 codec.

Uses a fake upstream on localhost that speaks a canned exchange (cleartext auth, one simple
query, terminate). No PostgreSQL container is needed.
"""

from __future__ import annotations

import json
import socket
import struct
import threading
import time
from pathlib import Path
from typing import Callable

from pgarbor_harness import config
from pgarbor_harness.wire_transcripts.proxy import RecordingProxy, _redact

PASSWORD = b"hunter2-secret"


# ---------------------------------------------------------------------------------------------
# raw byte helpers (independent of the codec under test)
# ---------------------------------------------------------------------------------------------


def _typed(tag: bytes, body: bytes) -> bytes:
    return tag + struct.pack("!i", len(body) + 4) + body


def _startup() -> bytes:
    body = (struct.pack("!i", 196608) + b"user\0postgres\0database\0postgres\0\0")
    return struct.pack("!i", len(body) + 4) + body


def _recv_exact(sock: socket.socket, n: int) -> bytes:
    buf = bytearray()
    while len(buf) < n:
        chunk = sock.recv(n - len(buf))
        if not chunk:
            raise EOFError("peer closed the connection")
        buf += chunk
    return bytes(buf)


def _recv_typed(sock: socket.socket) -> tuple[bytes, bytes]:
    tag = _recv_exact(sock, 1)
    length = struct.unpack("!i", _recv_exact(sock, 4))[0]
    return tag, _recv_exact(sock, length - 4)


def _recv_untyped(sock: socket.socket) -> bytes:
    length = struct.unpack("!i", _recv_exact(sock, 4))[0]
    return _recv_exact(sock, length - 4)


def _recv_until_ready(sock: socket.socket) -> None:
    while True:
        tag, _body = _recv_typed(sock)
        if tag == b"Z":
            return


class _FakeUpstream:
    """One-shot fake server: runs ``handler`` on the first accepted connection."""

    def __init__(self, handler: Callable[[socket.socket], None]):
        self._handler = handler
        self.error: BaseException | None = None
        self._sock = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
        self._sock.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
        self._sock.bind(("127.0.0.1", 0))
        self._sock.listen(4)
        self.port = self._sock.getsockname()[1]
        self._thread = threading.Thread(target=self._run, name="fake-upstream", daemon=True)
        self._thread.start()

    def _run(self) -> None:
        try:
            conn, _ = self._sock.accept()
            with conn:
                self._handler(conn)
        except BaseException as exc:  # surfaced by join()
            self.error = exc
        finally:
            self._sock.close()

    def join(self, timeout: float = 10.0) -> None:
        self._thread.join(timeout)
        assert not self._thread.is_alive(), "fake upstream did not finish"
        assert self.error is None, f"fake upstream failed: {self.error!r}"


def _read_records(out: Path) -> tuple[dict, list[dict]]:
    lines = out.read_text(encoding="utf-8").splitlines()
    header = json.loads(lines[0])
    records = [json.loads(line) for line in lines[1:]]
    return header, records


# ---------------------------------------------------------------------------------------------
# tests
# ---------------------------------------------------------------------------------------------


def _cleartext_upstream(sock: socket.socket) -> None:
    _recv_untyped(sock)  # StartupMessage
    sock.sendall(_typed(b"R", struct.pack("!i", 3)))  # AuthenticationCleartextPassword
    tag, body = _recv_typed(sock)
    assert tag == b"p", tag
    assert body == PASSWORD + b"\0", body
    sock.sendall(_typed(b"R", struct.pack("!i", 0)) + _typed(b"Z", b"I"))
    tag, body = _recv_typed(sock)  # Query
    assert tag == b"Q", tag
    assert body == b"select 1\0", body
    sock.sendall(_typed(b"C", b"SELECT 1\0") + _typed(b"Z", b"I"))
    tag, _body = _recv_typed(sock)  # Terminate
    assert tag == b"X", tag


def test_header_jsonl_and_password_redaction(tmp_path: Path) -> None:
    out = tmp_path / "transcript.jsonl"
    upstream = _FakeUpstream(_cleartext_upstream)
    proxy = RecordingProxy(0, "127.0.0.1", upstream.port, out, client_label="test-client")
    port = proxy.start()
    try:
        client = socket.create_connection(("127.0.0.1", port), timeout=5)
        try:
            client.sendall(_startup())
            tag, _ = _recv_typed(client)
            assert tag == b"R"
            pw = _typed(b"p", PASSWORD + b"\0")
            client.sendall(pw[:3])  # split across writes on purpose
            client.sendall(pw[3:])
            _recv_until_ready(client)
            client.sendall(_typed(b"Q", b"select 1\0"))
            _recv_until_ready(client)
            client.sendall(_typed(b"X", b""))
        finally:
            client.close()
        upstream.join()  # server saw Terminate, so every record has been written
    finally:
        proxy.stop()

    raw = out.read_bytes()
    assert PASSWORD not in raw, "PasswordMessage content leaked into the transcript"

    header, records = _read_records(out)
    assert out.read_text(encoding="utf-8").splitlines()[0].startswith('{"format": "pgarbor-wire-transcript/1"')
    assert header == {
        "format": "pgarbor-wire-transcript/1",
        "client": "test-client",
        "upstream": "pg18-oracle",
        "image_digest": config.PG_IMAGE_DIGEST,
    }

    seqs = [r["seq"] for r in records]
    assert seqs == list(range(1, len(records) + 1))
    assert all(r["conn"] == "c1" for r in records)
    assert all(r["dir"] in ("F", "B") for r in records)

    pairs = [(r["dir"], r["msg"]["type"]) for r in records]
    assert ("F", "Startup") in pairs
    assert ("B", "Authentication") in pairs
    assert ("F", "Query") in pairs
    assert ("B", "CommandComplete") in pairs
    assert pairs.count(("B", "ReadyForQuery")) == 2
    assert pairs[-1] == ("F", "Terminate")

    redacted = [r for r in records if r["msg"] == {"redacted": True}]
    assert len(redacted) == 1
    assert redacted[0]["dir"] == "F"


def test_stop_returns_quickly_with_open_connection(tmp_path: Path) -> None:
    out = tmp_path / "open.jsonl"

    def hold_open(sock: socket.socket) -> None:
        _recv_untyped(sock)
        while sock.recv(4096):  # returns b"" once the proxy shuts the upstream down
            pass

    upstream = _FakeUpstream(hold_open)
    proxy = RecordingProxy(0, "127.0.0.1", upstream.port, out, client_label="open")
    port = proxy.start()
    client = socket.create_connection(("127.0.0.1", port), timeout=5)
    try:
        client.sendall(_startup())
        # Wait until the Startup has been recorded, so the connection is fully established.
        deadline = time.monotonic() + 5
        while time.monotonic() < deadline and out.read_text(encoding="utf-8").count("\n") < 2:
            time.sleep(0.01)
        begin = time.monotonic()
        proxy.stop()
        elapsed = time.monotonic() - begin
        assert elapsed < 5.0, f"stop() took {elapsed:.2f}s with an open connection"
    finally:
        client.close()
    upstream.join()

    header, records = _read_records(out)
    assert header["client"] == "open"
    assert [(r["dir"], r["msg"]["type"]) for r in records] == [("F", "Startup")]


def _ssl_upstream(sock: socket.socket) -> None:
    ssl_request = _recv_exact(sock, 8)
    assert ssl_request == struct.pack("!ii", 8, 80877103), ssl_request
    sock.sendall(b"N")
    _recv_untyped(sock)  # StartupMessage after the refusal
    # returning closes the connection and lets the proxy finish the B direction


def test_ssl_request_is_forwarded_and_answer_recorded(tmp_path: Path) -> None:
    out = tmp_path / "ssl.jsonl"
    upstream = _FakeUpstream(_ssl_upstream)
    proxy = RecordingProxy(0, "127.0.0.1", upstream.port, out, client_label="ssl")
    port = proxy.start()
    try:
        client = socket.create_connection(("127.0.0.1", port), timeout=5)
        try:
            client.sendall(struct.pack("!ii", 8, 80877103))
            assert _recv_exact(client, 1) == b"N"
            client.sendall(_startup())
        finally:
            client.close()
        upstream.join()
    finally:
        proxy.stop()

    _header, records = _read_records(out)
    pairs = [(r["dir"], r["msg"]["type"]) for r in records]
    assert pairs == [("F", "SSLRequest"), ("B", "SSLResponse"), ("F", "Startup")]
    assert records[1]["msg"]["answer"] == "N"
    assert [r["seq"] for r in records] == [1, 2, 3]


def test_redaction_covers_credential_messages_only() -> None:
    assert _redact({"type": "PasswordMessage", "password_hex": "00"}) == {"redacted": True}
    assert _redact({"type": "SASLInitialResponse", "data_hex": "00"}) == {"redacted": True}
    assert _redact({"type": "SASLResponse", "data_hex": "00"}) == {"redacted": True}
    query = {"type": "Query", "sql": "select 1"}
    assert _redact(query) is query
    ready = {"type": "ReadyForQuery", "status": "I"}
    assert _redact(ready) is ready
