"""ORACLE-08: recording TCP proxy for real-client wire transcripts (psql, sqlx, node-postgres,
Drizzle, Prisma) — records what an UNMODIFIED client sends and what PG answers.

Lease HAR-24.

``RecordingProxy(listen_port, upstream_host, upstream_port, out_path)``: std ``socket`` +
one thread per direction per client connection (no asyncio). For each accepted connection
``c<N>`` (N from 1), bytes are forwarded unchanged and simultaneously decoded with
messages.StreamDecoder; each decoded message is appended to ``out_path`` (JSON Lines,
written+flushed under a lock) as::

    {"conn": "c1", "seq": 17, "dir": "F" | "B", "msg": {...decoded...}}

``seq`` is a global monotonically increasing counter (ordering across directions is the
order the proxy *read* the bytes, which is a legal interleaving). The proxy answers nothing
itself: SSLRequest is forwarded and the upstream answer ('N') recorded (the decoder is told to
expect a one-byte answer). Header line (first line of the file)::

    {"format": "pgarbor-wire-transcript/1", "client": "<label>", "upstream": "pg18-oracle",
     "image_digest": config.PG_IMAGE_DIGEST}

Auth: transcripts are recorded with trust auth; any PasswordMessage/SASL payload is replaced by
``{"redacted": true}`` before writing (never persist secrets).

``stop()`` closes the listener and all connections, joins threads (timeout 5 s).
"""

from __future__ import annotations

import json
import socket
import threading
import time
from pathlib import Path
from typing import Any, Optional

from .. import config
from .messages import StreamDecoder

#: First-line format tag of every transcript file.
TRANSCRIPT_FORMAT = "pgarbor-wire-transcript/1"
#: Constant ``upstream`` label written into the header.
UPSTREAM_LABEL = "pg18-oracle"

#: Frontend messages whose payload is a credential. Replaced by ``{"redacted": true}``.
#: CancelRequest carries the backend cancel secret key, so it is redacted too.
_REDACTED_TYPES = frozenset({
    "PasswordMessage",
    "SASLInitialResponse",
    "SASLResponse",
    "GSSResponse",
    "CancelRequest",
})
#: Frontend requests that the server answers with exactly one byte.
_ONE_BYTE_ANSWER_TYPES = frozenset({"SSLRequest", "GSSENCRequest"})

_RECV_BYTES = 65536
_ACCEPT_POLL_S = 0.25
_STOP_TIMEOUT_S = 5.0


def _redact(msg: dict[str, Any]) -> dict[str, Any]:
    """Return the value to persist for a decoded message (secrets replaced)."""
    if msg.get("type") in _REDACTED_TYPES:
        return {"redacted": True}
    return msg


def _json_default(obj: Any) -> Any:
    if isinstance(obj, (bytes, bytearray, memoryview)):
        return {"hex": bytes(obj).hex()}
    raise TypeError(f"not JSON serializable: {type(obj).__name__}")


def _close_quietly(sock: socket.socket) -> None:
    try:
        sock.close()
    except OSError:
        pass


def _shutdown_quietly(sock: socket.socket, how: int) -> None:
    try:
        sock.shutdown(how)
    except OSError:
        pass


class _Conn:
    """One accepted client connection and its upstream socket."""

    __slots__ = ("label", "client", "upstream", "live")

    def __init__(self, label: str, client: socket.socket, upstream: socket.socket):
        self.label = label
        self.client = client
        self.upstream = upstream
        self.live = 2  # two pump threads (F and B) still running


class RecordingProxy:
    def __init__(self, listen_port: int, upstream_host: str, upstream_port: int,
                 out_path: Path, client_label: str = "unknown"):
        self.listen_port = listen_port
        self.upstream_host = upstream_host
        self.upstream_port = upstream_port
        self.out_path = out_path
        self.client_label = client_label
        self._lock = threading.Lock()
        self._stop = threading.Event()
        self._threads: list[threading.Thread] = []
        self.bound_port: Optional[int] = None
        # private state; _fh, _seq and _conns are guarded by _lock
        self._listener: Optional[socket.socket] = None
        self._fh: Any = None
        self._seq = 0
        self._conns: set[_Conn] = set()

    def start(self) -> int:
        """Bind 127.0.0.1:listen_port (0 = ephemeral), write the header line, start the accept
        thread; return the bound port."""
        if self._listener is not None:
            raise RuntimeError("recording proxy already started")
        out = Path(self.out_path)
        out.parent.mkdir(parents=True, exist_ok=True)
        fh = open(out, "w", encoding="utf-8", newline="\n")
        try:
            header = {
                "format": TRANSCRIPT_FORMAT,
                "client": self.client_label,
                "upstream": UPSTREAM_LABEL,
                "image_digest": config.PG_IMAGE_DIGEST,
            }
            fh.write(json.dumps(header) + "\n")
            fh.flush()
            listener = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
            try:
                listener.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
                listener.bind(("127.0.0.1", self.listen_port))
                listener.listen(128)
                listener.settimeout(_ACCEPT_POLL_S)
            except OSError:
                _close_quietly(listener)
                raise
        except BaseException:
            fh.close()
            raise
        self._fh = fh
        self._listener = listener
        self.bound_port = listener.getsockname()[1]
        accept = threading.Thread(target=self._accept_loop, args=(listener,),
                                  name="wire-proxy-accept", daemon=True)
        self._threads.append(accept)
        accept.start()
        return self.bound_port

    def stop(self) -> None:
        """Close the listener and every open connection, then join threads (<= 5 s total)."""
        with self._lock:
            self._stop.set()
            conns = list(self._conns)
        # Wake every blocked recv() so the pump threads unwind and release their sockets.
        for conn in conns:
            _shutdown_quietly(conn.client, socket.SHUT_RDWR)
            _shutdown_quietly(conn.upstream, socket.SHUT_RDWR)
        deadline = time.monotonic() + _STOP_TIMEOUT_S
        for thread in list(self._threads):
            thread.join(max(0.0, deadline - time.monotonic()))
        # Anything still registered after the join budget is closed here (idempotent).
        with self._lock:
            leftovers = list(self._conns)
            self._conns.clear()
            fh = self._fh
            self._fh = None
        for conn in leftovers:
            _close_quietly(conn.client)
            _close_quietly(conn.upstream)
        if fh is not None:
            fh.close()

    # ------------------------------------------------------------------------------------
    # internals
    # ------------------------------------------------------------------------------------

    def _accept_loop(self, listener: socket.socket) -> None:
        counter = 0
        try:
            while not self._stop.is_set():
                try:
                    client, _addr = listener.accept()
                except socket.timeout:
                    continue
                except OSError:
                    break
                counter += 1
                label = f"c{counter}"
                try:
                    upstream = socket.create_connection(
                        (self.upstream_host, self.upstream_port),
                        timeout=config.CONNECT_TIMEOUT_S)
                except OSError:
                    # Upstream unreachable: dropping the client is the only honest outcome.
                    _close_quietly(client)
                    continue
                client.settimeout(None)
                upstream.settimeout(None)
                conn = _Conn(label, client, upstream)
                with self._lock:
                    if self._stop.is_set():
                        _close_quietly(client)
                        _close_quietly(upstream)
                        break
                    self._conns.add(conn)
                    f_dec = StreamDecoder(frontend=True)
                    b_dec = StreamDecoder(frontend=False)
                    fwd = threading.Thread(
                        target=self._pump,
                        args=(conn, client, upstream, "F", f_dec, b_dec),
                        name=f"wire-proxy-{label}-F", daemon=True)
                    bwd = threading.Thread(
                        target=self._pump,
                        args=(conn, upstream, client, "B", b_dec, None),
                        name=f"wire-proxy-{label}-B", daemon=True)
                    self._threads.extend((fwd, bwd))
                fwd.start()
                bwd.start()
        finally:
            _close_quietly(listener)

    def _pump(self, conn: _Conn, src: socket.socket, dst: socket.socket, direction: str,
              dec: Optional[StreamDecoder], peer_dec: Optional[StreamDecoder]) -> None:
        """Copy bytes src -> dst unchanged, recording each decoded message first.

        Recording happens before forwarding, so a server reply can never be sequenced before
        the client request that caused it.
        """
        try:
            while True:
                try:
                    data = src.recv(_RECV_BYTES)
                except OSError:
                    break
                if not data:
                    break
                if dec is not None:
                    try:
                        msgs = dec.feed(data)
                    except Exception as exc:  # decoder bug or malformed input: keep forwarding
                        msgs = [{"type": "DecodeError", "error": type(exc).__name__}]
                        dec = None
                    if (direction == "F" and peer_dec is not None
                            and any(m.get("type") in _ONE_BYTE_ANSWER_TYPES for m in msgs)):
                        # Armed before the bytes reach the server, which answers with a single
                        # byte that the B decoder has to expect.
                        peer_dec.expect_ssl_answer()
                    for msg in msgs:
                        self._record(conn.label, direction, msg)
                try:
                    dst.sendall(data)
                except OSError:
                    break
        finally:
            # Propagate end-of-stream to the peer; the other direction keeps draining.
            _shutdown_quietly(dst, socket.SHUT_WR)
            self._release(conn)

    def _record(self, label: str, direction: str, msg: dict[str, Any]) -> None:
        with self._lock:
            fh = self._fh
            if fh is None:
                return
            self._seq += 1
            line = {"conn": label, "seq": self._seq, "dir": direction, "msg": _redact(msg)}
            fh.write(json.dumps(line, default=_json_default) + "\n")
            fh.flush()

    def _release(self, conn: _Conn) -> None:
        with self._lock:
            conn.live -= 1
            done = conn.live == 0
            if done:
                self._conns.discard(conn)
        if done:
            _close_quietly(conn.client)
            _close_quietly(conn.upstream)
