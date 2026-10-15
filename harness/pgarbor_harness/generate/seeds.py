"""ORACLE-09: deterministic grammar/type seed corpus generator.

Lease HAR-31.

Generates case dicts (pgarbor-case/1) from the ground-truth catalog CSVs in
``contracts/reference/pg18/`` — never from guesses. Every generated case has
``provenance: {source: generated, ref: "seeds/<generator>@<seed>"}``, ``tier: core``,
family ``gen-<generator>`` and an id ``gen-<generator>.<stable slug>-<seed>``. Output is fully
determined by (generator name, seed, reference CSV bytes): no wall clock, no unordered set
iteration (sort everything), ``random.Random(seed)`` only.

Generators (``GENERATORS``):
  * ``operators``: for each binary operator row in pg_operator.csv (oprkind ``b``) whose left
    and right types are both in SCALAR_TYPES, one case per (operator) with a single sql step
    ``SELECT <lit_l>::<ltype> <op> <lit_r>::<rtype> AS r`` for every literal pair from
    LITERALS[ltype] x LITERALS[rtype] (capped at 12 pairs, chosen by the Random), batched as
    one ``sql`` step per pair (so each pair is one result). Includes NULL operands. Errors
    (overflow 22003, division by zero 22012) are legitimate expectations recorded from PG.
  * ``casts``: for each pg_cast row with both types in SCALAR_TYPES: ``SELECT <lit>::<src>::<dst>``
    for each literal of the source type; plus the explicit-only casts via CAST(... AS ...).
  * ``io``: type input/output round trip: ``SELECT '<text>'::<type>`` for edge literals
    (min/max/overflow/whitespace/invalid) of each SCALAR_TYPES type — exercises
    22P02/22003/22007/22008 errors with exact messages.
  * ``ddl-dml``: random small schemas (1-3 tables, 1-6 columns of SCALAR_TYPES, optional
    PRIMARY KEY/UNIQUE/NOT NULL/DEFAULT) + INSERT/UPDATE/DELETE/SELECT sequences with
    ``order: multiset`` selects and an ORDER BY variant; multi-row VALUES; RETURNING.

``SCALAR_TYPES`` and ``LITERALS`` are defined here (frozen); literal strings are written as
SQL string literals with ``'`` doubled.

``write_cases(cases, out_root)`` writes YAML (``yaml.safe_dump(sort_keys=False,
allow_unicode=True, width=1000)``; SQL as block scalars is not guaranteed by safe_dump, which
is fine since values are always str) to ``out_root/<family>/<id without family>.yaml`` and
never overwrites an existing different file (returns the list of conflicts instead).
"""

from __future__ import annotations

from pathlib import Path
from typing import Any, Callable

#: type oid -> SQL type name. CONTRACT (adding types = new generated ids, fine).
SCALAR_TYPES: dict[int, str] = {
    16: "boolean", 21: "smallint", 23: "integer", 20: "bigint", 700: "real",
    701: "double precision", 1700: "numeric", 25: "text", 1043: "varchar", 1042: "char",
    17: "bytea", 1082: "date", 1083: "time", 1114: "timestamp", 1184: "timestamptz",
    1186: "interval", 2950: "uuid", 3802: "jsonb", 114: "json",
}

#: type oid -> literal texts (None = SQL NULL). CONTRACT.
LITERALS: dict[int, list[str | None]] = {
    16: ["true", "false", None],
    21: ["0", "1", "-1", "32767", "-32768", None],
    23: ["0", "1", "-1", "2147483647", "-2147483648", None],
    20: ["0", "1", "-1", "9223372036854775807", "-9223372036854775808", None],
    700: ["0", "1.5", "-0", "3.4028235e+38", "NaN", "Infinity", "-Infinity", None],
    701: ["0", "0.1", "-0", "1e308", "NaN", "Infinity", "-Infinity", None],
    1700: ["0", "1.10", "-0.000001", "123456789012345678901234567890.5", "NaN", None],
    25: ["", "a", "abc", "Ünïcødé", " padded ", None],
    1043: ["", "x", "hello", None],
    1042: ["", "a", "ab  ", None],
    17: ["\\x", "\\x00ff", "\\xdeadbeef", None],
    1082: ["2000-01-01", "1999-12-31", "infinity", "-infinity", "0001-01-01 BC", None],
    1083: ["00:00:00", "23:59:59.999999", "12:34:56.5", None],
    1114: ["2000-01-01 00:00:00", "1970-01-01 00:00:00.123456", "infinity", None],
    1184: ["2000-01-01 00:00:00+00", "2024-02-29 12:00:00-08", "-infinity", None],
    1186: ["0", "1 day", "-1 mon 2 days 03:04:05", "1 year 2 months", None],
    2950: ["00000000-0000-0000-0000-000000000000", "a0eebc99-9c0b-4ef8-bb6d-6bb9bd380a11",
           None],
    3802: ["{}", "[]", "{\"a\": [1, 2]}", "null", "1.0", None],
    114: ["{}", "[1, 2]", "{\"a\": 1, \"a\": 2}", None],
}


def gen_operators(seed: int, ref_dir: Path) -> list[dict[str, Any]]:
    raise NotImplementedError("HAR-31: operator seed generator")


def gen_casts(seed: int, ref_dir: Path) -> list[dict[str, Any]]:
    raise NotImplementedError("HAR-31: cast seed generator")


def gen_io(seed: int, ref_dir: Path) -> list[dict[str, Any]]:
    raise NotImplementedError("HAR-31: type io seed generator")


def gen_ddl_dml(seed: int, ref_dir: Path) -> list[dict[str, Any]]:
    raise NotImplementedError("HAR-31: ddl/dml seed generator")


GENERATORS: dict[str, Callable[[int, Path], list[dict[str, Any]]]] = {
    "operators": gen_operators,
    "casts": gen_casts,
    "io": gen_io,
    "ddl-dml": gen_ddl_dml,
}


def write_cases(cases: list[dict[str, Any]], out_root: Path) -> list[str]:
    """Write per module docstring; return conflicting paths."""
    raise NotImplementedError("HAR-31: write generated cases")
