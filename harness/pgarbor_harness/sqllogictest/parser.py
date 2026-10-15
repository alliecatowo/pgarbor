"""ORACLE-07: sqllogictest (.slt) parser and writer — PGArbor dialect.

Lease HAR-21.

Record grammar (sqlite sqllogictest, as used by DuckDB/CockroachDB variants), records are
separated by one or more blank lines; ``#`` lines are comments:

    statement ok
    <sql, possibly multi-line>

    statement error [<sqlstate 5 chars>]
    <sql>

    query <types> [nosort|rowsort|valuesort] [label]
    <sql>
    ----
    <expected values, one value per line>

    hash-threshold <N>
    halt
    skipif <engine>   /  onlyif <engine>     (prefix lines of the next record)

``<types>``: one letter per column, ``T`` text, ``I`` integer, ``R`` real; must equal the
column count of the result.

PGArbor dialect (documented divergence from sqlite's formatting, because the oracle is PG):
  * values are PostgreSQL's exact TEXT output (no ``%.3f`` reformatting of ``R``);
  * NULL is ``NULL``; empty string is ``(empty)``; a value containing a newline is written
    with ``\\n`` escaped as two characters ``\\`` ``n`` and backslash as ``\\\\``;
  * sort modes act on the formatted values exactly like sqlite: ``rowsort`` sorts rows by
    their value lists (Python string ordering), ``valuesort`` sorts all values flat;
  * results with more than ``hash-threshold`` values are stored as one line
    ``<n> values hashing to <md5 hex>`` where md5 is over each value + "\\n" in final order
    (sqlite rule);
  * ``statement error <sqlstate>``: PGArbor extension; when present the SQLSTATE must match.
  * input files (tests/slt/**.slt, authored without results: ``query`` records may omit the
    ``----`` section) are completed by the recorder from PG into tests/expected/slt/**.slt.
"""

from __future__ import annotations

import hashlib
from dataclasses import dataclass, field
from typing import Optional

_SORT_MODES = ("nosort", "rowsort", "valuesort")
_TYPE_LETTERS = frozenset("TIR")
_SQLSTATE_CHARS = frozenset("0123456789ABCDEFGHIJKLMNOPQRSTUVWXYZ")
_RECORD_KINDS = ("statement", "query", "hash-threshold", "halt")


@dataclass
class SltRecord:
    kind: str                         #: "statement" | "query" | "hash-threshold" | "halt"
    line: int                         #: 1-based line of the record header
    sql: str = ""
    expect_ok: bool = True            #: statement ok / error
    sqlstate: Optional[str] = None
    types: str = ""
    sort: str = "nosort"
    label: Optional[str] = None
    expected: Optional[list[str]] = None   #: None = not yet recorded
    hash_threshold: int = 0
    conditions: list[tuple[str, str]] = field(default_factory=list)  #: ("skipif","pgarbor")


class SltSyntaxError(ValueError):
    """``<name>:<line>: <message>``."""


def _read_body(lines: list[str], i: int, stop_at_dashes: bool) -> tuple[str, int]:
    """Collect SQL lines from ``i`` up to a blank line (or ``----`` for queries)."""
    start = i
    n = len(lines)
    while i < n:
        s = lines[i].strip()
        if s == "" or (stop_at_dashes and s == "----"):
            break
        i += 1
    return "\n".join(lines[start:i]), i


def parse(text: str, name: str = "<slt>") -> list[SltRecord]:
    """Parse a file into records (grammar above)."""
    lines = [ln[:-1] if ln.endswith("\r") else ln for ln in text.split("\n")]
    n = len(lines)
    records: list[SltRecord] = []
    pending: list[tuple[str, str]] = []
    pending_line = 0
    i = 0

    def err(lineno: int, message: str) -> SltSyntaxError:
        return SltSyntaxError(f"{name}:{lineno}: {message}")

    while i < n:
        lineno = i + 1
        stripped = lines[i].strip()
        if stripped == "" or stripped.startswith("#"):
            i += 1
            continue
        tokens = stripped.split()
        head = tokens[0]

        if head in ("skipif", "onlyif"):
            if len(tokens) != 2:
                raise err(lineno, f"'{head}' takes exactly one engine name")
            if not pending:
                pending_line = lineno
            pending.append((head, tokens[1]))
            i += 1
            continue

        conditions = pending
        pending = []

        if head == "statement":
            if len(tokens) < 2:
                raise err(lineno, "'statement' needs 'ok' or 'error'")
            mode = tokens[1]
            sqlstate: Optional[str] = None
            if mode == "ok":
                if len(tokens) != 2:
                    raise err(lineno, "'statement ok' takes no further tokens")
                expect_ok = True
            elif mode == "error":
                if len(tokens) > 3:
                    raise err(lineno, "'statement error' takes at most one SQLSTATE")
                expect_ok = False
                if len(tokens) == 3:
                    code = tokens[2]
                    if len(code) != 5 or any(c not in _SQLSTATE_CHARS for c in code):
                        raise err(lineno, f"invalid SQLSTATE {code!r}")
                    sqlstate = code
            else:
                raise err(lineno, f"'statement' expects 'ok' or 'error', got {mode!r}")
            sql, i = _read_body(lines, i + 1, stop_at_dashes=False)
            if sql == "":
                raise err(lineno, "statement record has no SQL")
            records.append(SltRecord(kind="statement", line=lineno, sql=sql,
                                     expect_ok=expect_ok, sqlstate=sqlstate,
                                     conditions=conditions))

        elif head == "query":
            if len(tokens) < 2:
                raise err(lineno, "'query' needs a column type string")
            types = tokens[1]
            if any(c not in _TYPE_LETTERS for c in types):
                raise err(lineno, f"invalid column types {types!r} (expected T, I, R)")
            rest = tokens[2:]
            sort = "nosort"
            if rest and rest[0] in _SORT_MODES:
                sort = rest[0]
                rest = rest[1:]
            label = " ".join(rest) if rest else None
            sql, i = _read_body(lines, i + 1, stop_at_dashes=True)
            if sql == "":
                raise err(lineno, "query record has no SQL")
            expected: Optional[list[str]] = None
            if i < n and lines[i].strip() == "----":
                i += 1
                expected = []
                while i < n and lines[i].strip() != "":
                    expected.append(lines[i])
                    i += 1
            records.append(SltRecord(kind="query", line=lineno, sql=sql, types=types,
                                     sort=sort, label=label, expected=expected,
                                     conditions=conditions))

        elif head == "hash-threshold":
            if len(tokens) != 2 or not (tokens[1].isascii() and tokens[1].isdigit()):
                raise err(lineno, "'hash-threshold' takes one non-negative integer")
            records.append(SltRecord(kind="hash-threshold", line=lineno,
                                     hash_threshold=int(tokens[1]), conditions=conditions))
            i += 1

        elif head == "halt":
            if len(tokens) != 1:
                raise err(lineno, "'halt' takes no arguments")
            records.append(SltRecord(kind="halt", line=lineno, conditions=conditions))
            i += 1

        else:
            raise err(lineno, f"unknown record type {head!r}")

    if pending:
        raise err(pending_line, "skipif/onlyif not followed by a record")
    return records


def format_value(v: Optional[str]) -> str:
    """NULL/empty/escape rules above (implemented: trivial)."""
    if v is None:
        return "NULL"
    if v == "":
        return "(empty)"
    return v.replace("\\", "\\\\").replace("\n", "\\n")


def render_values(rows: list[list[Optional[str]]], sort: str, hash_threshold: int) -> list[str]:
    """Format + sort + optionally hash the values per the dialect rules.

    A ``hash_threshold`` <= 0 disables hashing (sqlite behaviour); otherwise results with
    more values than the threshold are replaced by one hash line.
    """
    if sort not in _SORT_MODES:
        raise ValueError(f"unknown sort mode {sort!r}")
    formatted = [[format_value(v) for v in row] for row in rows]
    if sort == "rowsort":
        formatted.sort()
        values = [v for row in formatted for v in row]
    elif sort == "valuesort":
        values = sorted(v for row in formatted for v in row)
    else:
        values = [v for row in formatted for v in row]
    if hash_threshold > 0 and len(values) > hash_threshold:
        payload = "".join(v + "\n" for v in values).encode("utf-8", "surrogatepass")
        digest = hashlib.md5(payload, usedforsecurity=False).hexdigest()
        return [f"{len(values)} values hashing to {digest}"]
    return values


def write(records: list[SltRecord]) -> str:
    """Serialize records back to text (round-trips parse(write(r)) == r except comments).

    Each record header is written on its own ``line`` (comments and other layout are replaced by
    blank lines), so ``parse(write(r))`` keeps every ``line`` and diagnostics stay aligned with
    the source file. Records that cannot be placed at their line are written just after the
    previous one, separated by one blank line.
    """
    lines: list[str] = []
    for r in records:
        if r.kind not in _RECORD_KINDS:
            raise ValueError(f"unknown record kind {r.kind!r}")
        out: list[str] = [f"{kind} {engine}" for kind, engine in r.conditions]
        # 1-based line where this block must start so that its header lands on r.line.
        target = r.line - len(out)
        nxt = len(lines) + 1
        if lines:
            lines.extend([""] * max(1, target - nxt))
        else:
            lines.extend([""] * max(0, target - nxt))
        if r.kind == "statement":
            if r.expect_ok:
                out.append("statement ok")
            elif r.sqlstate:
                out.append(f"statement error {r.sqlstate}")
            else:
                out.append("statement error")
            out.append(r.sql)
        elif r.kind == "query":
            head = f"query {r.types} {r.sort}"
            if r.label:
                head += f" {r.label}"
            out.append(head)
            out.append(r.sql)
            if r.expected is not None:
                out.append("----")
                out.extend(r.expected)
        elif r.kind == "hash-threshold":
            out.append(f"hash-threshold {r.hash_threshold}")
        else:
            out.append("halt")
        lines.extend(out)
    if not lines:
        return ""
    return "\n".join(lines) + "\n"
