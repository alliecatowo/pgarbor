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

import threading
from pathlib import Path
from typing import Optional


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

    def start(self) -> int:
        """Bind 127.0.0.1:listen_port (0 = ephemeral), write the header line, start the accept
        thread; return the bound port."""
        raise NotImplementedError("HAR-24: start recording proxy")

    def stop(self) -> None:
        raise NotImplementedError("HAR-24: stop recording proxy")
