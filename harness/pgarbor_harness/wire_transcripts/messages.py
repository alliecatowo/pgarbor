"""ORACLE-08: PostgreSQL protocol 3.0/3.2 message codec (decode/encode to plain dicts).

Lease HAR-23.

Reference: PostgreSQL 18 docs "Message Formats" (protocol-message-formats.html) and
src/include/libpq/protocol.h. Pure Python (``struct``), no psycopg.

Framing:
  * untyped startup-phase frontend packets: Int32 length (incl. itself) + Int32 code:
    196608 (3.0) / 196610 (3.2) StartupMessage followed by NUL-terminated key/value pairs and
    a final NUL; 80877103 SSLRequest; 80877104 GSSENCRequest; 80877102 CancelRequest
    (pid Int32 + secret key bytes: 4 bytes in 3.0, up to 256 in 3.2 — length-derived).
  * everything else: Byte1 type + Int32 length (incl. itself) + body.
  * the single-byte server answer to SSLRequest/GSSENCRequest ('S'/'N'/'G') is a pseudo
    message ``{"type": "SSLResponse", "answer": "N"}``.

Decoded dict shape: ``{"type": <name>, ...fields}``; names (frontend): Startup, SSLRequest,
GSSENCRequest, CancelRequest, Query, Parse, Bind, Describe, Execute, Sync, Flush, Close,
Terminate, PasswordMessage (also SASLInitialResponse/SASLResponse by context), CopyData,
CopyDone, CopyFail, FunctionCall. Backend: Authentication (with ``code`` and sub-fields),
BackendKeyData, ParameterStatus, ReadyForQuery, RowDescription, DataRow, CommandComplete,
EmptyQueryResponse, ErrorResponse, NoticeResponse (fields as {code char: value}),
ParseComplete, BindComplete, CloseComplete, NoData, ParameterDescription, PortalSuspended,
CopyInResponse, CopyOutResponse, CopyBothResponse, CopyData, CopyDone,
NotificationResponse, NegotiateProtocolVersion, FunctionCallResponse.
Binary payloads (DataRow values, Bind params, CopyData) are lowercase hex strings under
``*_hex`` keys; NULL (-1 length) is None. Unknown type byte -> {"type": "Unknown", "byte":
"x", "body_hex": ...} (never raise on server bytes).

Round trip: ``encode(decode(raw)) == raw`` for every message type listed (unit tests).
"""

from __future__ import annotations

from typing import Any

PROTOCOL_3_0 = 196608
PROTOCOL_3_2 = 196610
SSL_REQUEST_CODE = 80877103
GSSENC_REQUEST_CODE = 80877104
CANCEL_REQUEST_CODE = 80877102


class StreamDecoder:
    """Incremental decoder for one direction of one connection.

    ``feed(data) -> list[dict]`` returns every complete message; ``frontend=True`` starts in
    startup phase (untyped packets) until a Startup message has been seen; SSL/GSSENC
    requests keep it in startup phase. For the backend direction, ``expect_ssl_answer()`` tells
    the decoder that the next single byte is an SSLResponse."""

    def __init__(self, frontend: bool):
        self.frontend = frontend
        self.startup_phase = frontend
        self._buf = bytearray()
        self._ssl_answer_pending = False

    def expect_ssl_answer(self) -> None:
        self._ssl_answer_pending = True

    def feed(self, data: bytes) -> list[dict[str, Any]]:
        raise NotImplementedError("HAR-23: incremental protocol decode")


def decode_frontend(raw: bytes, startup_phase: bool) -> dict[str, Any]:
    """Decode exactly one complete frontend message."""
    raise NotImplementedError("HAR-23: decode frontend message")


def decode_backend(raw: bytes) -> dict[str, Any]:
    """Decode exactly one complete backend message."""
    raise NotImplementedError("HAR-23: decode backend message")


def encode(msg: dict[str, Any]) -> bytes:
    """Inverse of decode_* for every listed message type."""
    raise NotImplementedError("HAR-23: encode protocol message")
