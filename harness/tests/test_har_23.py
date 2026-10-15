"""HAR-23 (ORACLE-08): PG v3 wire message codec tests.

Every sample is hand-built from the protocol message formats. Each must satisfy
``encode(decode(raw)) == raw``. Streams must decode identically whether fed whole or one byte
at a time.
"""

from __future__ import annotations

import struct

import pytest

from pgarbor_harness.wire_transcripts.messages import (
    CANCEL_REQUEST_CODE,
    GSSENC_REQUEST_CODE,
    PROTOCOL_3_0,
    SSL_REQUEST_CODE,
    StreamDecoder,
    decode_backend,
    decode_frontend,
    encode,
)


def _typed(tag: bytes, body: bytes = b"") -> bytes:
    return tag + struct.pack("!i", 4 + len(body)) + body


def _startup(code: int, body: bytes = b"") -> bytes:
    return struct.pack("!ii", 8 + len(body), code) + body


# ---------------------------------------------------------------------------
# Frontend samples: (expected decoded type, raw bytes, startup_phase)
# ---------------------------------------------------------------------------

FRONTEND_SAMPLES = [
    ("Startup", _startup(PROTOCOL_3_0, b"user\0alice\0database\0postgres\0\0"), True),
    ("SSLRequest", _startup(SSL_REQUEST_CODE), True),
    ("GSSENCRequest", _startup(GSSENC_REQUEST_CODE), True),
    ("CancelRequest", _startup(CANCEL_REQUEST_CODE, struct.pack("!i", 1234) + b"\x01\x02\x03\x04"), True),
    ("Query", _typed(b"Q", b"select 1\0"), False),
    ("Parse", _typed(b"P", b"s1\0select $1\0" + struct.pack("!hI", 1, 23)), False),
    (
        "Bind",
        _typed(
            b"B",
            b"p\0s1\0"
            + struct.pack("!hh", 1, 0)
            + struct.pack("!h", 2)
            + struct.pack("!i", 1) + b"x"
            + struct.pack("!i", -1)
            + struct.pack("!h", 1) + struct.pack("!h", 0),
        ),
        False,
    ),
    ("Describe", _typed(b"D", b"Sstmt\0"), False),
    ("Execute", _typed(b"E", b"\0" + struct.pack("!i", 0)), False),
    ("Sync", _typed(b"S"), False),
    ("Flush", _typed(b"H"), False),
    ("Close", _typed(b"C", b"Pportal\0"), False),
    ("Terminate", _typed(b"X"), False),
    ("PasswordMessage", _typed(b"p", b"md5abc\0"), False),
    ("CopyData", _typed(b"d", b"1\t2\n"), False),
    ("CopyDone", _typed(b"c"), False),
    ("CopyFail", _typed(b"f", b"oops\0"), False),
    (
        "FunctionCall",
        _typed(
            b"F",
            struct.pack("!I", 44)
            + struct.pack("!h", 0)
            + struct.pack("!h", 1)
            + struct.pack("!i", 2) + b"ab"
            + struct.pack("!h", 0),
        ),
        False,
    ),
]

# Backend samples: (expected decoded type, raw bytes)
BACKEND_SAMPLES = [
    ("Authentication", _typed(b"R", struct.pack("!i", 0))),
    ("Authentication", _typed(b"R", struct.pack("!i", 5) + b"\x01\x02\x03\x04")),
    ("Authentication", _typed(b"R", struct.pack("!i", 10) + b"SCRAM-SHA-256\0\0")),
    ("Authentication", _typed(b"R", struct.pack("!i", 11) + b"r=abc")),
    ("Authentication", _typed(b"R", struct.pack("!i", 12) + b"v=xyz")),
    ("BackendKeyData", _typed(b"K", struct.pack("!ii", 4242, 99))),
    ("ParameterStatus", _typed(b"S", b"server_version\x0018.6\0")),
    ("ReadyForQuery", _typed(b"Z", b"I")),
    (
        "RowDescription",
        _typed(
            b"T",
            struct.pack("!h", 1) + b"a\0" + struct.pack("!IhIhih", 0, 0, 23, 4, -1, 0),
        ),
    ),
    (
        "DataRow",
        _typed(b"D", struct.pack("!h", 2) + struct.pack("!i", 1) + b"x" + struct.pack("!i", -1)),
    ),
    ("CommandComplete", _typed(b"C", b"SELECT 1\0")),
    ("EmptyQueryResponse", _typed(b"I")),
    ("ErrorResponse", _typed(b"E", b"SERROR\0C42601\0Msyntax error\0\0")),
    ("NoticeResponse", _typed(b"N", b"SNOTICE\0Mhi\0\0")),
    ("ParseComplete", _typed(b"1")),
    ("BindComplete", _typed(b"2")),
    ("CloseComplete", _typed(b"3")),
    ("NoData", _typed(b"n")),
    ("ParameterDescription", _typed(b"t", struct.pack("!h", 1) + struct.pack("!I", 23))),
    ("PortalSuspended", _typed(b"s")),
    ("CopyInResponse", _typed(b"G", struct.pack("!bh", 0, 1) + struct.pack("!h", 0))),
    ("CopyOutResponse", _typed(b"H", struct.pack("!bh", 0, 1) + struct.pack("!h", 0))),
    ("CopyBothResponse", _typed(b"W", struct.pack("!bh", 1, 1) + struct.pack("!h", 1))),
    ("CopyData", _typed(b"d", b"row\n")),
    ("CopyDone", _typed(b"c")),
    ("NotificationResponse", _typed(b"A", struct.pack("!i", 7) + b"chan\0" + b"payload\0")),
    ("NegotiateProtocolVersion", _typed(b"v", struct.pack("!ii", 0, 1) + b"_pq_.x\0")),
    ("FunctionCallResponse", _typed(b"V", struct.pack("!i", 2) + b"ok")),
    ("FunctionCallResponse", _typed(b"V", struct.pack("!i", -1))),
]


@pytest.mark.parametrize("expected_type,raw,startup", FRONTEND_SAMPLES)
def test_frontend_round_trip(expected_type, raw, startup):
    msg = decode_frontend(raw, startup)
    assert msg["type"] == expected_type, msg
    assert encode(msg) == raw


@pytest.mark.parametrize("expected_type,raw", BACKEND_SAMPLES)
def test_backend_round_trip(expected_type, raw):
    msg = decode_backend(raw)
    assert msg["type"] == expected_type, msg
    assert encode(msg) == raw


def test_ssl_response_round_trip():
    for answer in (b"S", b"N", b"G"):
        msg = decode_backend(answer)
        assert msg == {"type": "SSLResponse", "answer": answer.decode("ascii")}
        assert encode(msg) == answer


def test_sasl_frames_decode_as_password_message_and_round_trip():
    raw = _typed(b"p", b"SCRAM-SHA-256\0" + struct.pack("!i", 2) + b"n,")
    assert encode({"type": "SASLInitialResponse", "mechanism": "SCRAM-SHA-256",
                   "data_hex": "6e2c"}) == raw
    msg = decode_frontend(raw, False)
    assert msg == {"type": "PasswordMessage", "body_hex": raw[5:].hex()}
    assert encode(msg) == raw
    raw_resp = _typed(b"p", b"c=biws")
    assert encode({"type": "SASLResponse", "data_hex": b"c=biws".hex()}) == raw_resp


def test_startup_params_preserve_order_and_duplicates():
    raw = _startup(PROTOCOL_3_0, b"user\0alice\0user\0bob\0\0")
    msg = decode_frontend(raw, True)
    assert msg["params"] == [["user", "alice"], ["user", "bob"]]
    assert msg["protocol"] == PROTOCOL_3_0


def test_error_fields_decode_to_dict():
    msg = decode_backend(_typed(b"E", b"SERROR\0C42601\0Msyntax error\0\0"))
    assert msg["fields"] == {"S": "ERROR", "C": "42601", "M": "syntax error"}


def test_datarow_null_and_value():
    raw = _typed(b"D", struct.pack("!h", 2) + struct.pack("!i", 1) + b"x" + struct.pack("!i", -1))
    assert decode_backend(raw)["values_hex"] == ["78", None]


def test_unknown_type_byte_does_not_raise():
    raw = _typed(b"\x7f", b"abc")
    msg = decode_backend(raw)
    assert msg == {"type": "Unknown", "byte": "\x7f", "body_hex": "616263"}
    assert encode(msg) == raw
    fmsg = decode_frontend(_typed(b"?", b""), False)
    assert fmsg["type"] == "Unknown"
    assert encode(fmsg) == _typed(b"?", b"")


def test_high_byte_unknown_type_round_trips():
    raw = _typed(b"\xe9", b"")
    msg = decode_backend(raw)
    assert msg["type"] == "Unknown"
    assert encode(msg) == raw


def test_non_utf8_string_bytes_round_trip():
    raw = _typed(b"Q", b"select '\xff'\0")
    msg = decode_frontend(raw, False)
    assert msg["type"] == "Query"
    assert encode(msg) == raw


def test_truncated_body_is_malformed_not_raised():
    raw = _typed(b"Z", b"")
    msg = decode_backend(raw)
    assert msg["type"] == "Malformed"
    assert encode(msg) == raw


def test_unexpected_single_byte_answer_is_malformed():
    msg = decode_backend(b"x")
    assert msg["type"] == "Malformed"
    assert encode(msg) == b"x"


def test_startup_stream_byte_by_byte_equals_whole():
    stream = (
        _startup(SSL_REQUEST_CODE)
        + _startup(PROTOCOL_3_0, b"user\0alice\0database\0postgres\0\0")
        + _typed(b"Q", b"select 1\0")
        + _typed(b"X")
    )
    whole = StreamDecoder(frontend=True).feed(stream)
    assert [m["type"] for m in whole] == ["SSLRequest", "Startup", "Query", "Terminate"]

    dec = StreamDecoder(frontend=True)
    split: list[dict] = []
    for i in range(len(stream)):
        split.extend(dec.feed(stream[i:i + 1]))
    assert split == whole
    assert dec.startup_phase is False

    reencoded = b"".join(encode(m) for m in whole)
    assert reencoded == stream


def test_backend_stream_with_ssl_answer_byte_by_byte_equals_whole():
    stream = (
        b"N"
        + _typed(b"R", struct.pack("!i", 0))
        + _typed(b"S", b"client_encoding\0UTF8\0")
        + _typed(b"K", struct.pack("!ii", 1, 2))
        + _typed(b"Z", b"I")
    )
    whole_dec = StreamDecoder(frontend=False)
    whole_dec.expect_ssl_answer()
    whole = whole_dec.feed(stream)
    assert [m["type"] for m in whole] == [
        "SSLResponse", "Authentication", "ParameterStatus", "BackendKeyData", "ReadyForQuery",
    ]

    dec = StreamDecoder(frontend=False)
    dec.expect_ssl_answer()
    split: list[dict] = []
    for i in range(len(stream)):
        split.extend(dec.feed(stream[i:i + 1]))
    assert split == whole


def test_no_ssl_answer_pending_treats_n_as_message_type():
    dec = StreamDecoder(frontend=False)
    # Without expect_ssl_answer(), 'N' starts a NoticeResponse frame and waits for more bytes.
    assert dec.feed(b"N") == []
    out = dec.feed(_typed(b"N", b"SNOTICE\0Mhi\0\0")[1:])
    assert out[0]["type"] == "NoticeResponse"


def test_framing_error_is_emitted_not_raised():
    dec = StreamDecoder(frontend=False)
    out = dec.feed(b"Z\x00\x00\x00\x01")  # length 1 is below the minimum of 4
    assert [m["type"] for m in out] == ["Malformed"]
    assert encode(out[0]) == b"Z\x00\x00\x00\x01"


def test_startup_length_cap_is_enforced():
    dec = StreamDecoder(frontend=True)
    out = dec.feed(struct.pack("!i", 20000) + b"\x00" * 4)
    assert [m["type"] for m in out] == ["Malformed"]
