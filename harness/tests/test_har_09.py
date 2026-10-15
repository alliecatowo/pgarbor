"""HAR-09 tests: libpq session driver. Unit tests use fake PGresult objects (no psycopg
needed); ``@pytest.mark.oracle`` tests need the PG18 oracle container and psycopg."""

import os
import time

import pytest

from pgarbor_harness.model import Endpoint
from pgarbor_harness.runner import session as S
from pgarbor_harness.runner.session import (
    HarnessSessionError, Session, notice_from_result, result_from_pgresult,
)


class FakeResult:
    def __init__(self, status, fields=None, cols=(), rows=(), tag=None, params=(), msg=None):
        self.status = status
        self._fields = fields or {}
        self._cols = list(cols)  # (name, oid, mod, fmt, table, tablecol)
        self._rows = [list(r) for r in rows]
        self.command_status = tag
        self._params = list(params)
        self.error_message = msg

    @property
    def nfields(self):
        return len(self._cols)

    @property
    def ntuples(self):
        return len(self._rows)

    @property
    def nparams(self):
        return len(self._params)

    def fname(self, i): return self._cols[i][0].encode()
    def ftype(self, i): return self._cols[i][1]
    def fmod(self, i): return self._cols[i][2]
    def fformat(self, i): return self._cols[i][3]
    def ftable(self, i): return self._cols[i][4]
    def ftablecol(self, i): return self._cols[i][5]
    def param_type(self, i): return self._params[i]
    def get_value(self, r, c): return self._rows[r][c]

    def error_field(self, code):
        v = self._fields.get(chr(code))
        return None if v is None else v.encode()


def test_rows_text_and_null():
    res = FakeResult(S._TUPLES_OK, cols=[("a", 25, -1, 0, 0, 0), ("b", 25, -1, 0, 0, 0)],
                     rows=[[None, b""]], tag=b"SELECT 1")
    r = result_from_pgresult(res)
    assert r.kind == "rows"
    assert r.rows == [[None, ""]]
    assert r.command_tag == "SELECT 1"
    assert [c.name for c in r.columns] == ["a", "b"]


def test_rows_binary_and_bad_utf8():
    res = FakeResult(S._TUPLES_OK, cols=[("a", 17, -1, 1, 0, 0), ("b", 25, -1, 0, 16385, 2)],
                     rows=[[b"\x00\x01", b"\xff\xfe"]], tag=b"SELECT 1")
    r = result_from_pgresult(res)
    assert r.rows == [[b"\x00\x01", b"\xff\xfe"]]
    assert r.columns[1].table_oid == 16385 and r.columns[1].column_attnum == 2


def test_command_empty_error():
    assert result_from_pgresult(FakeResult(S._COMMAND_OK, tag=b"CREATE TABLE")).command_tag \
        == "CREATE TABLE"
    assert result_from_pgresult(FakeResult(S._COMMAND_OK, tag=b"")).command_tag is None
    assert result_from_pgresult(FakeResult(S._EMPTY_QUERY)).kind == "empty"
    err = result_from_pgresult(FakeResult(S._FATAL_ERROR, fields={
        "V": "ERROR", "S": "FEHLER", "C": "42601", "M": "syntax error", "P": "8"}))
    assert err.kind == "error"
    assert (err.error.severity, err.error.sqlstate, err.error.position) == ("ERROR", "42601", "8")
    assert err.error.detail is None


def test_error_without_server_fields_uses_error_message():
    err = result_from_pgresult(FakeResult(S._FATAL_ERROR, msg=b"server closed the connection\n"))
    assert err.error.message == "server closed the connection"
    assert err.error.sqlstate == ""


def test_description():
    res = FakeResult(S._COMMAND_OK, cols=[("x", 23, -1, 0, 0, 0)], params=[23, 25])
    r = result_from_pgresult(res, describe=True)
    assert r.kind == "description" and r.param_type_oids == [23, 25]
    assert r.columns[0].type_oid == 23


def test_notice():
    n = notice_from_result(FakeResult(S._NONFATAL_ERROR, fields={
        "V": "NOTICE", "C": "00000", "M": "hello", "W": "PL/pgSQL function inline_code_block"}))
    assert (n.severity, n.sqlstate, n.message) == ("NOTICE", "00000", "hello")
    assert n.context.startswith("PL/pgSQL")


def test_param_encoding():
    vals, fmts = S._encode_params(["a", None, "é"], [])
    assert vals == [b"a", None, "é".encode()] and fmts == [0, 0, 0]
    vals, fmts = S._encode_params(["00ff", "x"], ["binary", "text"])
    assert vals == [b"\x00\xff", b"x"] and fmts == [1, 0]
    vals, fmts = S._encode_params(["0a", "0b"], ["binary"])
    assert vals == [b"\n", b"\x0b"] and fmts == [1, 1]
    with pytest.raises(HarnessSessionError):
        S._encode_params(["zz"], ["binary"])
    with pytest.raises(HarnessSessionError):
        S._encode_params(["a", "b", "c"], ["text", "text"])


def test_unconnected_session():
    s = Session(Endpoint("127.0.0.1", 1, "x"), "a")
    assert s.txn_status == "U"
    s.close()
    s.close()
    with pytest.raises(HarnessSessionError):
        s.finish_pending()
    with pytest.raises(HarnessSessionError):
        s.start("bogus", {})
    s.cancel()


# -- live ------------------------------------------------------------------------------------
def _endpoint():
    return Endpoint(os.environ.get("PGARBOR_ORACLE_HOST", "127.0.0.1"),
                    int(os.environ.get("PGARBOR_ORACLE_PORT", "54319")),
                    os.environ.get("PGARBOR_ORACLE_DB", "postgres"),
                    os.environ.get("PGARBOR_ORACLE_USER", "postgres"),
                    os.environ.get("PGARBOR_ORACLE_PASSWORD"))


@pytest.fixture
def sess():
    s = Session(_endpoint(), "t", step_timeout_s=5.0)
    s.connect()
    yield s
    s.close()


@pytest.mark.oracle
def test_live_multi_statement(sess):
    out = sess.run_simple("select 1; select null::text, ''::text")
    assert [r.kind for r in out.results] == ["rows", "rows"]
    assert out.results[0].rows == [["1"]]
    assert out.results[1].rows == [[None, ""]]
    assert out.txn_status == "I"


@pytest.mark.oracle
def test_live_error_and_txn_status(sess):
    sess.run_simple("begin")
    out = sess.run_simple("select 1/0")
    assert out.results[0].kind == "error"
    assert out.results[0].error.sqlstate == "22012"
    assert out.txn_status == "E"
    sess.run_simple("rollback")


@pytest.mark.oracle
def test_live_copy_out(sess):
    out = sess.run_simple("copy (select 1) to stdout")
    assert len(out.results) == 1
    r = out.results[0]
    assert r.kind == "copy_out" and r.rows == [["1\n"]] and r.command_tag == "COPY 1"


@pytest.mark.oracle
def test_live_copy_in_fails_deterministically(sess):
    sess.run_simple("create temp table ci(a int)")
    out = sess.run_simple("copy ci from stdin")
    assert out.results[-1].kind == "error"
    assert out.results[-1].error.sqlstate == "57014"


@pytest.mark.oracle
def test_live_notice(sess):
    out = sess.run_simple("do $$ begin raise notice 'hi'; end $$")
    assert [(n.severity, n.sqlstate, n.message) for n in out.notices] == \
        [("NOTICE", "00000", "hi")]


@pytest.mark.oracle
def test_live_prepare_describe_execute(sess):
    p = sess.run_prepare("s1", "select $1::int + 1 as x", [])
    assert p.results[0].kind == "command" and p.results[0].command_tag is None
    d = sess.run_describe("s1")
    assert d.results[0].kind == "description"
    assert d.results[0].param_type_oids == [23]
    assert d.results[0].columns[0].name == "x"
    e = sess.run_execute("s1", ["41"], [], "text")
    assert e.results[0].rows == [["42"]]
    e = sess.run_execute("s1", ["0000002a"], ["binary"], "binary")
    assert e.results[0].rows == [[bytes.fromhex("0000002b")]]
    e = sess.run_exec_params("select $1::text", ["x"], [], [], "text")
    assert e.results[0].rows == [["x"]]


@pytest.mark.oracle
def test_live_async(sess):
    sess.start("sql", {"sql": "select pg_sleep(0.3), 1"})
    assert sess.pending
    assert sess.txn_status == "A"
    assert not sess.poll_done(0.0)
    assert sess.poll_done(3.0)
    out = sess.finish_pending()
    assert not sess.pending
    assert out.results[0].rows[0][1] == "1"


@pytest.mark.oracle
def test_live_timeout_reconnects():
    s = Session(_endpoint(), "t", step_timeout_s=0.5)
    s.connect()
    try:
        t0 = time.monotonic()
        out = s.run_simple("select pg_sleep(30)")
        assert out.timed_out
        assert time.monotonic() - t0 < 3.5
        assert s.run_simple("select 1").results[0].rows == [["1"]]
    finally:
        s.close()
