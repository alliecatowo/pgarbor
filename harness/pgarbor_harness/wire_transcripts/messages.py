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

Additional conventions (codec-internal, documented here so callers can rely on them):
  * ``Malformed``: a frame whose framing is intact but whose body does not parse, or a
    framing error in the stream. Shape ``{"type": "Malformed", "reason": str, "raw_hex": ...}``;
    ``raw_hex`` holds the exact original bytes, so encode() reproduces them.
  * ``Unknown`` frames from a startup packet use ``{"type": "Unknown", "startup_code": int,
    "body_hex": ...}``.
  * ``SSLResponse`` with an unexpected byte is a ``Malformed`` frame, never an SSLResponse.
  * Startup parameters are a list of ``[key, value]`` pairs (order and duplicates preserved).
  * Errors/notices: ``fields`` is a dict keyed by one-character field code; a duplicated
    field code makes the frame ``Malformed`` so the dict stays lossless.
  * PasswordMessage is context-free: the codec keeps the raw payload as ``body_hex``.
    SASLInitialResponse and SASLResponse are accepted by encode() only (for building client
    bytes); decode() returns them as PasswordMessage, which re-encodes byte-identically.
  * Strings are decoded as UTF-8 with surrogateescape, so any byte sequence round-trips.
  * OIDs are unsigned Int32; all other integers are signed, matching the protocol docs.
"""

from __future__ import annotations

import struct
from typing import Any, Callable, Optional

PROTOCOL_3_0 = 196608
PROTOCOL_3_2 = 196610
SSL_REQUEST_CODE = 80877103
GSSENC_REQUEST_CODE = 80877104
CANCEL_REQUEST_CODE = 80877102

# protocol.h MAX_STARTUP_PACKET_LENGTH; PQ_LARGE_MESSAGE_LIMIT (libpq/pqcomm.h).
MAX_STARTUP_PACKET_LENGTH = 10000
MAX_MESSAGE_LENGTH = 0x3FFFFFFF

_I8 = struct.Struct("!b")
_I16 = struct.Struct("!h")
_I32 = struct.Struct("!i")
_U32 = struct.Struct("!I")

# Authentication request codes with an empty body (0 Ok, 2 KerberosV5, 3 CleartextPassword,
# 6 SCMCredential, 7 GSS, 9 SSPI).
_AUTH_EMPTY = frozenset({0, 2, 3, 6, 7, 9})
_SSL_ANSWERS = (b"S", b"N", b"G")

Msg = dict[str, Any]


class _Malformed(Exception):
    """Internal: a body does not parse. Converted to a Malformed message, never raised out."""


# ---------------------------------------------------------------------------
# Low-level helpers
# ---------------------------------------------------------------------------


def _byte_key(b: int) -> str:
    """One protocol byte as a one-character string; bytes >= 0x80 become surrogate escapes."""
    return bytes((b,)).decode("utf-8", "surrogateescape")


def _key_byte(s: str) -> int:
    b = s.encode("utf-8", "surrogateescape")
    if len(b) != 1:
        raise ValueError(f"expected a one-byte protocol character, got {s!r}")
    return b[0]


def _malformed(raw: bytes, reason: str) -> Msg:
    return {"type": "Malformed", "reason": reason, "raw_hex": raw.hex()}


def _hx(b: Optional[bytes]) -> Optional[str]:
    return None if b is None else b.hex()


class _Reader:
    """Bounds-checked cursor over one message body."""

    __slots__ = ("_b", "_p")

    def __init__(self, body: bytes):
        self._b = body
        self._p = 0

    def _need(self, n: int) -> None:
        if n < 0 or self._p + n > len(self._b):
            raise _Malformed("truncated message body")

    def u8(self) -> int:
        self._need(1)
        v = self._b[self._p]
        self._p += 1
        return v

    def i8(self) -> int:
        self._need(1)
        (v,) = _I8.unpack_from(self._b, self._p)
        self._p += 1
        return v

    def i16(self) -> int:
        self._need(2)
        (v,) = _I16.unpack_from(self._b, self._p)
        self._p += 2
        return v

    def i32(self) -> int:
        self._need(4)
        (v,) = _I32.unpack_from(self._b, self._p)
        self._p += 4
        return v

    def u32(self) -> int:
        self._need(4)
        (v,) = _U32.unpack_from(self._b, self._p)
        self._p += 4
        return v

    def count(self) -> int:
        v = self.i16()
        if v < 0:
            raise _Malformed("negative Int16 count")
        return v

    def count32(self) -> int:
        v = self.i32()
        if v < 0:
            raise _Malformed("negative Int32 count")
        return v

    def take(self, n: int) -> bytes:
        self._need(n)
        v = self._b[self._p:self._p + n]
        self._p += n
        return v

    def cstr(self) -> str:
        end = self._b.find(b"\0", self._p)
        if end < 0:
            raise _Malformed("unterminated string")
        v = self._b[self._p:end].decode("utf-8", "surrogateescape")
        self._p = end + 1
        return v

    def opt(self) -> Optional[bytes]:
        """Int32 length (-1 = NULL) followed by that many bytes."""
        n = self.i32()
        if n == -1:
            return None
        if n < 0:
            raise _Malformed("negative Int32 length")
        return self.take(n)

    def rest(self) -> bytes:
        v = self._b[self._p:]
        self._p = len(self._b)
        return v

    def done(self) -> None:
        if self._p != len(self._b):
            raise _Malformed("trailing bytes in message body")


def _fin(r: _Reader, d: Msg) -> Msg:
    r.done()
    return d


def _no_body(r: _Reader) -> Msg:
    return _fin(r, {})


def _p8(v: int) -> bytes:
    return _I8.pack(v)


def _p16(v: int) -> bytes:
    return _I16.pack(v)


def _p32(v: int) -> bytes:
    return _I32.pack(v)


def _pu32(v: int) -> bytes:
    return _U32.pack(v)


def _cs(s: str) -> bytes:
    b = s.encode("utf-8", "surrogateescape")
    if b"\0" in b:
        raise ValueError("NUL byte in protocol string")
    return b + b"\0"


def _opt(h: Optional[str]) -> bytes:
    if h is None:
        return _p32(-1)
    b = bytes.fromhex(h)
    return _p32(len(b)) + b


def _fmts(xs: list[int]) -> bytes:
    return _p16(len(xs)) + b"".join(_p16(x) for x in xs)


def _one_of(s: str, allowed: str) -> bytes:
    if len(s) != 1 or s not in allowed:
        raise ValueError(f"expected one of {allowed!r}, got {s!r}")
    return s.encode("ascii")


# ---------------------------------------------------------------------------
# Decoders: handler(body_reader) -> fields dict (without "type")
# ---------------------------------------------------------------------------


def _kind(r: _Reader) -> str:
    k = _byte_key(r.u8())
    if k not in ("S", "P"):
        raise _Malformed("Describe/Close kind must be 'S' or 'P'")
    return k


def _d_query(r: _Reader) -> Msg:
    return _fin(r, {"sql": r.cstr()})


def _d_parse(r: _Reader) -> Msg:
    name = r.cstr()
    query = r.cstr()
    oids = [r.u32() for _ in range(r.count())]
    return _fin(r, {"name": name, "query": query, "param_oids": oids})


def _d_bind(r: _Reader) -> Msg:
    portal = r.cstr()
    statement = r.cstr()
    param_formats = [r.i16() for _ in range(r.count())]
    values = [r.opt() for _ in range(r.count())]
    result_formats = [r.i16() for _ in range(r.count())]
    return _fin(r, {
        "portal": portal,
        "statement": statement,
        "param_formats": param_formats,
        "param_values_hex": [_hx(v) for v in values],
        "result_formats": result_formats,
    })


def _d_describe(r: _Reader) -> Msg:
    kind = _kind(r)
    return _fin(r, {"kind": kind, "name": r.cstr()})


def _d_execute(r: _Reader) -> Msg:
    portal = r.cstr()
    return _fin(r, {"portal": portal, "max_rows": r.i32()})


def _d_pw(r: _Reader) -> Msg:
    return {"body_hex": r.rest().hex()}


def _d_data(r: _Reader) -> Msg:
    return {"data_hex": r.rest().hex()}


def _d_copyfail(r: _Reader) -> Msg:
    return _fin(r, {"message": r.cstr()})


def _d_funccall(r: _Reader) -> Msg:
    oid = r.u32()
    arg_formats = [r.i16() for _ in range(r.count())]
    args = [r.opt() for _ in range(r.count())]
    result_format = r.i16()
    return _fin(r, {
        "function_oid": oid,
        "arg_formats": arg_formats,
        "args_hex": [_hx(a) for a in args],
        "result_format": result_format,
    })


def _d_auth(r: _Reader) -> Msg:
    code = r.i32()
    if code in _AUTH_EMPTY:
        return _fin(r, {"code": code})
    if code == 5:
        return _fin(r, {"code": 5, "salt_hex": r.take(4).hex()})
    if code == 10:
        mechanisms: list[str] = []
        while True:
            m = r.cstr()
            if m == "":
                break
            mechanisms.append(m)
        return _fin(r, {"code": 10, "mechanisms": mechanisms})
    # 8 GSSContinue, 11 SASLContinue, 12 SASLFinal, and any code not listed above.
    return {"code": code, "data_hex": r.rest().hex()}


def _d_backend_key(r: _Reader) -> Msg:
    pid = r.i32()
    return {"pid": pid, "secret_key_hex": r.rest().hex()}


def _d_param_status(r: _Reader) -> Msg:
    name = r.cstr()
    return _fin(r, {"name": name, "value": r.cstr()})


def _d_ready(r: _Reader) -> Msg:
    status = _byte_key(r.u8())
    if status not in ("I", "T", "E"):
        raise _Malformed("ReadyForQuery status must be I, T or E")
    return _fin(r, {"status": status})


def _d_row_desc(r: _Reader) -> Msg:
    fields: list[Msg] = []
    for _ in range(r.count()):
        name = r.cstr()
        table_oid = r.u32()
        attnum = r.i16()
        type_oid = r.u32()
        type_size = r.i16()
        type_modifier = r.i32()
        format_code = r.i16()
        fields.append({
            "name": name,
            "table_oid": table_oid,
            "column_attnum": attnum,
            "type_oid": type_oid,
            "type_size": type_size,
            "type_modifier": type_modifier,
            "format_code": format_code,
        })
    return _fin(r, {"fields": fields})


def _d_data_row(r: _Reader) -> Msg:
    values = [r.opt() for _ in range(r.count())]
    return _fin(r, {"values_hex": [_hx(v) for v in values]})


def _d_command_complete(r: _Reader) -> Msg:
    return _fin(r, {"tag": r.cstr()})


def _d_fields(r: _Reader) -> Msg:
    fields: dict[str, str] = {}
    while True:
        code = r.u8()
        if code == 0:
            break
        key = _byte_key(code)
        if key in fields:
            raise _Malformed("duplicate ErrorResponse/NoticeResponse field code")
        fields[key] = r.cstr()
    return _fin(r, {"fields": fields})


def _d_param_desc(r: _Reader) -> Msg:
    oids = [r.u32() for _ in range(r.count())]
    return _fin(r, {"param_oids": oids})


def _d_copy_resp(r: _Reader) -> Msg:
    overall = r.i8()
    column_formats = [r.i16() for _ in range(r.count())]
    return _fin(r, {"overall_format": overall, "column_formats": column_formats})


def _d_notification(r: _Reader) -> Msg:
    pid = r.i32()
    channel = r.cstr()
    return _fin(r, {"pid": pid, "channel": channel, "payload": r.cstr()})


def _d_negotiate(r: _Reader) -> Msg:
    newest_minor = r.i32()
    options = [r.cstr() for _ in range(r.count32())]
    return _fin(r, {"newest_minor": newest_minor, "options": options})


def _d_fcr(r: _Reader) -> Msg:
    return _fin(r, {"value_hex": _hx(r.opt())})


_FRONTEND_TABLE: dict[int, tuple[str, Callable[[_Reader], Msg]]] = {
    ord("Q"): ("Query", _d_query),
    ord("P"): ("Parse", _d_parse),
    ord("B"): ("Bind", _d_bind),
    ord("D"): ("Describe", _d_describe),
    ord("E"): ("Execute", _d_execute),
    ord("S"): ("Sync", _no_body),
    ord("H"): ("Flush", _no_body),
    ord("C"): ("Close", _d_describe),
    ord("X"): ("Terminate", _no_body),
    ord("p"): ("PasswordMessage", _d_pw),
    ord("d"): ("CopyData", _d_data),
    ord("c"): ("CopyDone", _no_body),
    ord("f"): ("CopyFail", _d_copyfail),
    ord("F"): ("FunctionCall", _d_funccall),
}

_BACKEND_TABLE: dict[int, tuple[str, Callable[[_Reader], Msg]]] = {
    ord("R"): ("Authentication", _d_auth),
    ord("K"): ("BackendKeyData", _d_backend_key),
    ord("S"): ("ParameterStatus", _d_param_status),
    ord("Z"): ("ReadyForQuery", _d_ready),
    ord("T"): ("RowDescription", _d_row_desc),
    ord("D"): ("DataRow", _d_data_row),
    ord("C"): ("CommandComplete", _d_command_complete),
    ord("I"): ("EmptyQueryResponse", _no_body),
    ord("E"): ("ErrorResponse", _d_fields),
    ord("N"): ("NoticeResponse", _d_fields),
    ord("1"): ("ParseComplete", _no_body),
    ord("2"): ("BindComplete", _no_body),
    ord("3"): ("CloseComplete", _no_body),
    ord("n"): ("NoData", _no_body),
    ord("t"): ("ParameterDescription", _d_param_desc),
    ord("s"): ("PortalSuspended", _no_body),
    ord("G"): ("CopyInResponse", _d_copy_resp),
    ord("H"): ("CopyOutResponse", _d_copy_resp),
    ord("W"): ("CopyBothResponse", _d_copy_resp),
    ord("d"): ("CopyData", _d_data),
    ord("c"): ("CopyDone", _no_body),
    ord("A"): ("NotificationResponse", _d_notification),
    ord("v"): ("NegotiateProtocolVersion", _d_negotiate),
    ord("V"): ("FunctionCallResponse", _d_fcr),
}


def _decode_startup_packet(raw: bytes) -> Msg:
    if len(raw) < 8:
        return _malformed(raw, "startup packet shorter than 8 bytes")
    (declared,) = _I32.unpack_from(raw, 0)
    if declared != len(raw):
        return _malformed(raw, "startup packet length mismatch")
    (code,) = _I32.unpack_from(raw, 4)
    body = raw[8:]
    try:
        if code == PROTOCOL_3_0 or code == PROTOCOL_3_2:
            r = _Reader(body)
            params: list[list[str]] = []
            while True:
                key = r.cstr()
                if key == "":
                    break
                params.append([key, r.cstr()])
            r.done()
            return {"type": "Startup", "protocol": code, "params": params}
        if code == SSL_REQUEST_CODE or code == GSSENC_REQUEST_CODE:
            if body:
                raise _Malformed("SSLRequest/GSSENCRequest must have no body")
            name = "SSLRequest" if code == SSL_REQUEST_CODE else "GSSENCRequest"
            return {"type": name}
        if code == CANCEL_REQUEST_CODE:
            if len(body) < 4:
                raise _Malformed("CancelRequest without process id")
            (pid,) = _I32.unpack_from(body, 0)
            return {"type": "CancelRequest", "pid": pid, "secret_key_hex": body[4:].hex()}
    except _Malformed as e:
        return _malformed(raw, f"startup packet: {e}")
    return {"type": "Unknown", "startup_code": code, "body_hex": body.hex()}


def _decode_typed(raw: bytes, table: dict[int, tuple[str, Callable[[_Reader], Msg]]]) -> Msg:
    if len(raw) < 5:
        return _malformed(raw, "typed message shorter than 5 bytes")
    (declared,) = _I32.unpack_from(raw, 1)
    if declared != len(raw) - 1:
        return _malformed(raw, "message length mismatch")
    body = raw[5:]
    entry = table.get(raw[0])
    if entry is None:
        return {"type": "Unknown", "byte": _byte_key(raw[0]), "body_hex": body.hex()}
    name, handler = entry
    try:
        fields = handler(_Reader(body))
    except _Malformed as e:
        return _malformed(raw, f"{name}: {e}")
    return {"type": name, **fields}


def decode_frontend(raw: bytes, startup_phase: bool) -> dict[str, Any]:
    """Decode exactly one complete frontend message."""
    raw = bytes(raw)
    if startup_phase:
        return _decode_startup_packet(raw)
    return _decode_typed(raw, _FRONTEND_TABLE)


def decode_backend(raw: bytes) -> dict[str, Any]:
    """Decode exactly one complete backend message."""
    raw = bytes(raw)
    if len(raw) == 1:
        # A complete one-byte frame is never a typed message, so it can only be the SSL/GSSENC
        # answer to a request (see StreamDecoder.expect_ssl_answer).
        if raw in _SSL_ANSWERS:
            return {"type": "SSLResponse", "answer": raw.decode("ascii")}
        return _malformed(raw, "unexpected single-byte server answer")
    return _decode_typed(raw, _BACKEND_TABLE)


# ---------------------------------------------------------------------------
# Encoders: name -> (type byte, body builder). Startup-phase frames are built in encode().
# ---------------------------------------------------------------------------


def _e_empty(m: Msg) -> bytes:
    return b""


def _e_query(m: Msg) -> bytes:
    return _cs(m["sql"])


def _e_parse(m: Msg) -> bytes:
    oids = m["param_oids"]
    return _cs(m["name"]) + _cs(m["query"]) + _p16(len(oids)) + b"".join(_pu32(o) for o in oids)


def _e_bind(m: Msg) -> bytes:
    values = m["param_values_hex"]
    return (
        _cs(m["portal"]) + _cs(m["statement"])
        + _fmts(m["param_formats"])
        + _p16(len(values)) + b"".join(_opt(v) for v in values)
        + _fmts(m["result_formats"])
    )


def _e_describe(m: Msg) -> bytes:
    return _one_of(m["kind"], "SP") + _cs(m["name"])


def _e_execute(m: Msg) -> bytes:
    return _cs(m["portal"]) + _p32(m["max_rows"])


def _e_pw(m: Msg) -> bytes:
    return bytes.fromhex(m["body_hex"])


def _e_sasl_initial(m: Msg) -> bytes:
    return _cs(m["mechanism"]) + _opt(m.get("data_hex"))


def _e_data(m: Msg) -> bytes:
    return bytes.fromhex(m["data_hex"])


def _e_copyfail(m: Msg) -> bytes:
    return _cs(m["message"])


def _e_funccall(m: Msg) -> bytes:
    args = m["args_hex"]
    return (
        _pu32(m["function_oid"]) + _fmts(m["arg_formats"])
        + _p16(len(args)) + b"".join(_opt(a) for a in args)
        + _p16(m["result_format"])
    )


def _e_auth(m: Msg) -> bytes:
    code = m["code"]
    head = _p32(code)
    if code in _AUTH_EMPTY:
        return head
    if code == 5:
        salt = bytes.fromhex(m["salt_hex"])
        if len(salt) != 4:
            raise ValueError("MD5 password salt must be 4 bytes")
        return head + salt
    if code == 10:
        return head + b"".join(_cs(x) for x in m["mechanisms"]) + b"\0"
    return head + bytes.fromhex(m["data_hex"])


def _e_backend_key(m: Msg) -> bytes:
    return _p32(m["pid"]) + bytes.fromhex(m["secret_key_hex"])


def _e_param_status(m: Msg) -> bytes:
    return _cs(m["name"]) + _cs(m["value"])


def _e_ready(m: Msg) -> bytes:
    return _one_of(m["status"], "ITE")


def _e_row_desc(m: Msg) -> bytes:
    parts = [_p16(len(m["fields"]))]
    for f in m["fields"]:
        parts.append(_cs(f["name"]))
        parts.append(_pu32(f["table_oid"]))
        parts.append(_p16(f["column_attnum"]))
        parts.append(_pu32(f["type_oid"]))
        parts.append(_p16(f["type_size"]))
        parts.append(_p32(f["type_modifier"]))
        parts.append(_p16(f["format_code"]))
    return b"".join(parts)


def _e_data_row(m: Msg) -> bytes:
    values = m["values_hex"]
    return _p16(len(values)) + b"".join(_opt(v) for v in values)


def _e_command_complete(m: Msg) -> bytes:
    return _cs(m["tag"])


def _e_fields(m: Msg) -> bytes:
    parts = [bytes((_key_byte(k),)) + _cs(v) for k, v in m["fields"].items()]
    return b"".join(parts) + b"\0"


def _e_param_desc(m: Msg) -> bytes:
    oids = m["param_oids"]
    return _p16(len(oids)) + b"".join(_pu32(o) for o in oids)


def _e_copy_resp(m: Msg) -> bytes:
    return _p8(m["overall_format"]) + _fmts(m["column_formats"])


def _e_notification(m: Msg) -> bytes:
    return _p32(m["pid"]) + _cs(m["channel"]) + _cs(m["payload"])


def _e_negotiate(m: Msg) -> bytes:
    options = m["options"]
    return _p32(m["newest_minor"]) + _p32(len(options)) + b"".join(_cs(o) for o in options)


def _e_fcr(m: Msg) -> bytes:
    return _opt(m["value_hex"])


_ENCODERS: dict[str, tuple[int, Callable[[Msg], bytes]]] = {
    # frontend
    "Query": (ord("Q"), _e_query),
    "Parse": (ord("P"), _e_parse),
    "Bind": (ord("B"), _e_bind),
    "Describe": (ord("D"), _e_describe),
    "Execute": (ord("E"), _e_execute),
    "Sync": (ord("S"), _e_empty),
    "Flush": (ord("H"), _e_empty),
    "Close": (ord("C"), _e_describe),
    "Terminate": (ord("X"), _e_empty),
    "PasswordMessage": (ord("p"), _e_pw),
    "SASLInitialResponse": (ord("p"), _e_sasl_initial),  # encode-only, see module docstring
    "SASLResponse": (ord("p"), _e_data),  # encode-only, see module docstring
    "CopyFail": (ord("f"), _e_copyfail),
    "FunctionCall": (ord("F"), _e_funccall),
    # backend
    "Authentication": (ord("R"), _e_auth),
    "BackendKeyData": (ord("K"), _e_backend_key),
    "ParameterStatus": (ord("S"), _e_param_status),
    "ReadyForQuery": (ord("Z"), _e_ready),
    "RowDescription": (ord("T"), _e_row_desc),
    "DataRow": (ord("D"), _e_data_row),
    "CommandComplete": (ord("C"), _e_command_complete),
    "EmptyQueryResponse": (ord("I"), _e_empty),
    "ErrorResponse": (ord("E"), _e_fields),
    "NoticeResponse": (ord("N"), _e_fields),
    "ParseComplete": (ord("1"), _e_empty),
    "BindComplete": (ord("2"), _e_empty),
    "CloseComplete": (ord("3"), _e_empty),
    "NoData": (ord("n"), _e_empty),
    "ParameterDescription": (ord("t"), _e_param_desc),
    "PortalSuspended": (ord("s"), _e_empty),
    "CopyInResponse": (ord("G"), _e_copy_resp),
    "CopyOutResponse": (ord("H"), _e_copy_resp),
    "CopyBothResponse": (ord("W"), _e_copy_resp),
    "CopyData": (ord("d"), _e_data),
    "CopyDone": (ord("c"), _e_empty),
    "NotificationResponse": (ord("A"), _e_notification),
    "NegotiateProtocolVersion": (ord("v"), _e_negotiate),
    "FunctionCallResponse": (ord("V"), _e_fcr),
}


def encode(msg: dict[str, Any]) -> bytes:
    """Inverse of decode_* for every listed message type."""
    t = msg.get("type")
    if t == "Startup":
        body = _p32(msg["protocol"]) + b"".join(_cs(k) + _cs(v) for k, v in msg["params"]) + b"\0"
        return _p32(4 + len(body)) + body
    if t == "SSLRequest":
        return _p32(8) + _p32(SSL_REQUEST_CODE)
    if t == "GSSENCRequest":
        return _p32(8) + _p32(GSSENC_REQUEST_CODE)
    if t == "CancelRequest":
        body = _p32(msg["pid"]) + bytes.fromhex(msg["secret_key_hex"])
        return _p32(8 + len(body)) + _p32(CANCEL_REQUEST_CODE) + body
    if t == "SSLResponse":
        answer = msg["answer"]
        if answer not in ("S", "N", "G"):
            raise ValueError(f"SSLResponse answer must be S, N or G, got {answer!r}")
        return answer.encode("ascii")
    if t == "Malformed":
        return bytes.fromhex(msg["raw_hex"])
    if t == "Unknown":
        if "startup_code" in msg:
            body = bytes.fromhex(msg["body_hex"])
            return _p32(8 + len(body)) + _p32(msg["startup_code"]) + body
        body = bytes.fromhex(msg["body_hex"])
        return bytes((_key_byte(msg["byte"]),)) + _p32(4 + len(body)) + body
    entry = _ENCODERS.get(t)  # type: ignore[arg-type]
    if entry is None:
        raise ValueError(f"unknown protocol message type {t!r}")
    tag, fn = entry
    body = fn(msg)
    return bytes((tag,)) + _p32(4 + len(body)) + body


# ---------------------------------------------------------------------------
# Incremental decoder
# ---------------------------------------------------------------------------


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
        if data:
            self._buf += data
        buf = self._buf
        n = len(buf)
        pos = 0
        out: list[dict[str, Any]] = []
        while pos < n:
            if self._ssl_answer_pending:
                self._ssl_answer_pending = False
                out.append(decode_backend(bytes(buf[pos:pos + 1])))
                pos += 1
                continue
            if self.frontend and self.startup_phase:
                if n - pos < 4:
                    break
                (length,) = _I32.unpack_from(buf, pos)
                if length < 8 or length > MAX_STARTUP_PACKET_LENGTH:
                    out.append(_framing_error(buf, pos, "invalid startup packet length"))
                    pos = n
                    break
                total = length
            else:
                if n - pos < 5:
                    break
                (length,) = _I32.unpack_from(buf, pos + 1)
                if length < 4 or length > MAX_MESSAGE_LENGTH:
                    out.append(_framing_error(buf, pos, "invalid message length"))
                    pos = n
                    break
                total = 1 + length
            if n - pos < total:
                break
            raw = bytes(buf[pos:pos + total])
            pos += total
            if self.frontend:
                if self.startup_phase:
                    msg = decode_frontend(raw, True)
                    if msg.get("type") == "Startup":
                        self.startup_phase = False
                else:
                    msg = decode_frontend(raw, False)
            else:
                msg = decode_backend(raw)
            out.append(msg)
        del buf[:pos]
        return out


def _framing_error(buf: bytearray, pos: int, reason: str) -> Msg:
    # The stream cannot be re-synchronised after a bad length, so the rest of the buffer is
    # kept verbatim as one Malformed frame.
    return {"type": "Malformed", "reason": reason, "raw_hex": bytes(buf[pos:]).hex()}
