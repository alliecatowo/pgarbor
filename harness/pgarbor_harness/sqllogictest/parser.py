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

from dataclasses import dataclass, field
from typing import Optional


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


def parse(text: str, name: str = "<slt>") -> list[SltRecord]:
    """Parse a file into records (grammar above)."""
    raise NotImplementedError("HAR-21: slt parser")


def format_value(v: Optional[str]) -> str:
    """NULL/empty/escape rules above (implemented: trivial)."""
    if v is None:
        return "NULL"
    if v == "":
        return "(empty)"
    return v.replace("\\", "\\\\").replace("\n", "\\n")


def render_values(rows: list[list[Optional[str]]], sort: str, hash_threshold: int) -> list[str]:
    """Format + sort + optionally hash the values per the dialect rules."""
    raise NotImplementedError("HAR-21: render result values")


def write(records: list[SltRecord]) -> str:
    """Serialize records back to text (round-trips parse(write(r)) == r except comments)."""
    raise NotImplementedError("HAR-21: slt writer")
