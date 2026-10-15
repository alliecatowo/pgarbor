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

import csv
import random
import re
from pathlib import Path
from typing import Any, Callable

import yaml

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

# ---------------------------------------------------------------------------------------------
# Private tuning (not part of the frozen contract; changing these changes generated output).
# ---------------------------------------------------------------------------------------------

_OPERATOR_MAX_PAIRS = 12
_DDL_CASES_PER_SEED = 8
_DDL_MAX_TABLES = 3
_DDL_MAX_COLUMNS = 6
_DDL_MIN_OPS = 4
_DDL_MAX_OPS = 10
_DDL_MAX_INSERT_ROWS = 3

#: Types with no default btree opclass: cannot be PRIMARY KEY / UNIQUE / ORDER BY keys.
_NO_BTREE: frozenset[int] = frozenset({114})
#: Types whose ORDER BY output is not uniquely determined by value (equal values print
#: differently: -0 vs 0, 1.10 vs 1.1, '1 day' vs '24:00:00', JSON key order). Such tables use
#: multiset comparison instead of an ORDER BY ``ordered`` comparison.
_ORDER_UNSAFE: frozenset[int] = frozenset({114, 700, 701, 1700, 1186})

#: Extra edge texts for ``io`` (overflow, malformed, whitespace, junk), per type oid.
_IO_EXTRA: dict[int, tuple[str, ...]] = {
    16: ("yes", "no", "t", "f", " on ", "maybe", "1", "0", "tru"),
    21: ("32768", "-32769", " 1 ", "1.5", "abc", "0x10", "+5"),
    23: ("2147483648", "-2147483649", "1e3", "", " 7\t", "12a"),
    20: ("9223372036854775808", "-9223372036854775809", "1_000", "99999999999999999999"),
    700: ("1e39", "-1e400", "1.5.5", "inf", "-inf", "nan", "1e-50"),
    701: ("1e309", "-1e400", "1.5.5", "inf", "nan", "NaN", "1e-400"),
    1700: ("1e1000", " 2 ", "1,5", "-NaN", "1e-16383", "1.2e+5"),
    25: ("abé", "  "),
    1043: ("  ", "ÿ"),
    1042: (" x", "abcdef"),
    17: ("\\xZZ", "\\x0", "\\x  ff", "abc", "\\\\x"),
    1082: ("2000-02-30", "2000-13-01", "1999-12-31 25:00", "2000-1-1", "today", "January 1 2000"),
    1083: ("24:00:01", "25:00:00", "12:60:00", "24:00:00", "1:2:3", "12:00:00+05"),
    1114: ("2000-01-01 25:00:00", "2000-02-30 00:00:00", "2000-01-01T00:00:00", "2000-01-01"),
    1184: ("2000-01-01 00:00:00+99", "2000-01-01 00:00:00 Mars", "2000-01-01 24:00:00+00",
           "2000-01-01 00:00:00+14:30:01"),
    1186: ("1 foo", "2147483648 years", "P1Y", "1:2:3:4", "00:00:01.5 ago"),
    2950: ("00000000-0000-0000-0000-00000000000g", "{00000000-0000-0000-0000-000000000000}",
           "a0eebc999c0b4ef8bb6d6bb9bd380a11", "abc", " 00000000-0000-0000-0000-000000000000 "),
    3802: ("{", "[1,]", "{'a':1}", " null ", "1 2", "tru", "NaN"),
    114: ("{", "[1,]", "{'a':1}", "NaN", "\"x\""),
}

#: Operator-symbol characters spelled out so operator slugs are readable and unique.
_OP_WORDS: dict[str, str] = {
    "+": "plus", "-": "minus", "*": "star", "/": "slash", "%": "mod", "^": "caret",
    "<": "lt", ">": "gt", "=": "eq", "!": "not", "~": "tilde", "@": "at", "#": "hash",
    "&": "and", "|": "or", "?": "q", "`": "tick",
}
_SLUG_NON_ALNUM = re.compile(r"[^a-z0-9]+")


# ---------------------------------------------------------------------------------------------
# Helpers
# ---------------------------------------------------------------------------------------------


def _slug(text: str) -> str:
    """Lowercase ``[a-z0-9-]`` slug; never contains ``.`` (ids are dot-separated)."""
    return _SLUG_NON_ALNUM.sub("-", text.lower()).strip("-")


def _sql_str(text: str) -> str:
    """SQL standard string literal (single quotes doubled)."""
    return "'" + text.replace("'", "''") + "'"


def _typed_literal(text: str | None, oid: int) -> str:
    """``'<text>'::<type>`` for a text literal, ``NULL::<type>`` for None."""
    if text is None:
        return f"NULL::{SCALAR_TYPES[oid]}"
    return f"{_sql_str(text)}::{SCALAR_TYPES[oid]}"


def _read_rows(path: Path) -> list[dict[str, str]]:
    with path.open(newline="", encoding="utf-8") as fh:
        return list(csv.DictReader(fh))


def _build_case(
    generator: str,
    slug: str,
    seed: int,
    title: str,
    feature_ids: list[str],
    steps: list[dict[str, Any]],
) -> dict[str, Any]:
    family = f"gen-{generator}"
    return {
        "id": f"{family}.{slug}-{seed}",
        "title": title[:200],
        "family": family,
        "feature_ids": feature_ids,
        "tier": "core",
        "provenance": {"source": "generated", "ref": f"seeds/{generator}@{seed}"},
        "seed": seed,
        "steps": steps,
    }


# ---------------------------------------------------------------------------------------------
# Generators
# ---------------------------------------------------------------------------------------------


def gen_operators(seed: int, ref_dir: Path) -> list[dict[str, Any]]:
    rng = random.Random(seed)
    rows = sorted(_read_rows(Path(ref_dir) / "pg_operator.csv"), key=lambda r: int(r["oid"]))
    cases: list[dict[str, Any]] = []
    for row in rows:
        if row["oprkind"] != "b":
            continue
        left, right = int(row["oprleft"]), int(row["oprright"])
        if left not in SCALAR_TYPES or right not in SCALAR_TYPES:
            continue
        op = row["oprname"]
        pairs: list[tuple[str | None, str | None]] = [
            (a, b) for a in LITERALS[left] for b in LITERALS[right]
        ]
        if len(pairs) > _OPERATOR_MAX_PAIRS:
            picked = sorted(rng.sample(range(len(pairs)), _OPERATOR_MAX_PAIRS))
            pairs = [pairs[i] for i in picked]
        steps = [
            {"sql": f"SELECT {_typed_literal(a, left)} {op} {_typed_literal(b, right)} AS r"}
            for a, b in pairs
        ]
        op_words = " ".join(_OP_WORDS.get(c, c) for c in op)
        slug = (
            f"o{row['oid']}-{_slug(op_words)}-"
            f"{_slug(SCALAR_TYPES[left])}-{_slug(SCALAR_TYPES[right])}"
        )
        title = f"operator {op} ({SCALAR_TYPES[left]}, {SCALAR_TYPES[right]})"
        cases.append(_build_case("operators", slug, seed, title, ["operators"], steps))
    return cases


def gen_casts(seed: int, ref_dir: Path) -> list[dict[str, Any]]:
    rows = sorted(_read_rows(Path(ref_dir) / "pg_cast.csv"), key=lambda r: int(r["oid"]))
    cases: list[dict[str, Any]] = []
    for row in rows:
        src, dst = int(row["castsource"]), int(row["casttarget"])
        if src not in SCALAR_TYPES or dst not in SCALAR_TYPES:
            continue
        context = row["castcontext"]
        dst_name = SCALAR_TYPES[dst]
        steps: list[dict[str, Any]] = []
        for lit in LITERALS[src]:
            source_expr = _typed_literal(lit, src)
            if context == "e":
                sql = f"SELECT CAST({source_expr} AS {dst_name}) AS r"
            else:
                sql = f"SELECT {source_expr}::{dst_name} AS r"
            steps.append({"sql": sql})
        slug = f"c{row['oid']}-{_slug(SCALAR_TYPES[src])}-to-{_slug(dst_name)}"
        title = f"cast {SCALAR_TYPES[src]} to {dst_name} (castcontext {context})"
        cases.append(_build_case("casts", slug, seed, title, ["casts"], steps))
    return cases


def gen_io(seed: int, ref_dir: Path) -> list[dict[str, Any]]:
    rng = random.Random(seed)
    cases: list[dict[str, Any]] = []
    for oid in sorted(SCALAR_TYPES):
        texts: set[str | None] = set(LITERALS[oid]) | set(_IO_EXTRA.get(oid, ()))
        ordered = sorted(texts, key=lambda t: (t is not None, t or ""))
        rng.shuffle(ordered)
        steps = [{"sql": f"SELECT {_typed_literal(t, oid)} AS r"} for t in ordered]
        slug = f"t{oid}-{_slug(SCALAR_TYPES[oid])}"
        title = f"type input edge literals for {SCALAR_TYPES[oid]}"
        cases.append(_build_case("io", slug, seed, title, ["type-io"], steps))
    return cases


def _ddl_column_def(col: dict[str, Any]) -> str:
    parts = [col["name"], SCALAR_TYPES[col["oid"]]]
    if col["default"] is not None:
        parts.append(f"DEFAULT {_typed_literal(col['default'], col['oid'])}")
    if col["not_null"] and not col["pk"]:
        parts.append("NOT NULL")
    if col["pk"]:
        parts.append("PRIMARY KEY")
    if col["unique"]:
        parts.append("UNIQUE")
    return " ".join(parts)


def _ddl_pick_value(rng: random.Random, col: dict[str, Any]) -> str | None:
    """A literal text for ``col``; never None for NOT NULL columns."""
    choices = LITERALS[col["oid"]]
    if col["not_null"]:
        choices = [t for t in choices if t is not None]
    return rng.choice(choices)


def _ddl_where(rng: random.Random, table: dict[str, Any]) -> str:
    col = rng.choice(table["cols"])
    negate = "NOT " if rng.random() < 0.5 else ""
    return f"{col['name']} IS {negate}NULL"


def _ddl_insert(rng: random.Random, table: dict[str, Any]) -> tuple[str, str | None]:
    cols = table["cols"]
    rows: list[str] = []
    for _ in range(rng.randint(1, _DDL_MAX_INSERT_ROWS)):
        values: list[str] = []
        for col in cols:
            may_default = col["default"] is not None or not col["not_null"]
            if may_default and rng.random() < 0.15:
                values.append("DEFAULT")
            else:
                values.append(_typed_literal(_ddl_pick_value(rng, col), col["oid"]))
        rows.append("(" + ", ".join(values) + ")")
    names = ", ".join(c["name"] for c in cols)
    sql = f"INSERT INTO {table['name']} ({names}) VALUES " + ", ".join(rows)
    if rng.random() < 0.3:
        return sql + " RETURNING *", "multiset"
    return sql, None


def _ddl_update(rng: random.Random, table: dict[str, Any]) -> tuple[str, str | None]:
    cols = table["cols"]
    targets = rng.sample(cols, rng.randint(1, min(2, len(cols))))
    sets = ", ".join(
        f"{c['name']} = {_typed_literal(_ddl_pick_value(rng, c), c['oid'])}" for c in targets
    )
    sql = f"UPDATE {table['name']} SET {sets} WHERE {_ddl_where(rng, table)}"
    if rng.random() < 0.3:
        return sql + " RETURNING *", "multiset"
    return sql, None


def _ddl_delete(rng: random.Random, table: dict[str, Any]) -> tuple[str, str | None]:
    sql = f"DELETE FROM {table['name']} WHERE {_ddl_where(rng, table)}"
    if rng.random() < 0.3:
        return sql + " RETURNING *", "multiset"
    return sql, None


def _ddl_select(rng: random.Random, table: dict[str, Any]) -> tuple[str, str | None]:
    cols = table["cols"]
    if rng.random() < 0.5:
        projection = "*"
    else:
        projection = ", ".join(c["name"] for c in cols[: rng.randint(1, len(cols))])
    sql = f"SELECT {projection} FROM {table['name']}"
    if rng.random() < 0.5:
        sql += f" WHERE {_ddl_where(rng, table)}"
    return sql, "multiset"


def _ddl_ordered_select(table: dict[str, Any]) -> tuple[str, str | None]:
    cols = table["cols"]
    if any(c["oid"] in _ORDER_UNSAFE for c in cols):
        return f"SELECT * FROM {table['name']}", "multiset"
    order_by = ", ".join(f"{c['name']} NULLS FIRST" for c in cols)
    return f"SELECT * FROM {table['name']} ORDER BY {order_by}", "ordered"


def _ddl_dml_case(rng: random.Random, index: int, seed: int) -> dict[str, Any]:
    oids = sorted(SCALAR_TYPES)
    tables: list[dict[str, Any]] = []
    for ti in range(rng.randint(1, _DDL_MAX_TABLES)):
        cols: list[dict[str, Any]] = []
        for ci in range(rng.randint(1, _DDL_MAX_COLUMNS)):
            oid = rng.choice(oids)
            keyable = oid not in _NO_BTREE
            pk = ci == 0 and keyable and rng.random() < 0.5
            unique = (not pk) and ci > 0 and keyable and rng.random() < 0.15
            not_null = pk or rng.random() < 0.25
            default: str | None = None
            if rng.random() < 0.3:
                default = rng.choice([t for t in LITERALS[oid] if t is not None])
            cols.append({
                "name": f"c{ci}", "oid": oid, "pk": pk, "unique": unique,
                "not_null": not_null, "default": default,
            })
        tables.append({"name": f"t{ti}", "cols": cols})

    stmts: list[tuple[str, str | None]] = []
    for table in tables:
        column_defs = ", ".join(_ddl_column_def(c) for c in table["cols"])
        stmts.append((f"CREATE TABLE {table['name']} ({column_defs})", None))
    for _ in range(rng.randint(_DDL_MIN_OPS, _DDL_MAX_OPS)):
        table = rng.choice(tables)
        roll = rng.random()
        if roll < 0.45:
            stmts.append(_ddl_insert(rng, table))
        elif roll < 0.60:
            stmts.append(_ddl_update(rng, table))
        elif roll < 0.70:
            stmts.append(_ddl_delete(rng, table))
        elif roll < 0.85:
            stmts.append(_ddl_select(rng, table))
        else:
            stmts.append(_ddl_ordered_select(table))
    for table in tables:
        stmts.append(_ddl_ordered_select(table))

    steps: list[dict[str, Any]] = []
    for sql, order in stmts:
        step: dict[str, Any] = {"sql": sql}
        if order is not None:
            step["order"] = order
        steps.append(step)
    title = f"random schema {index} with {len(tables)} table(s)"
    return _build_case("ddl-dml", f"s{index:02d}", seed, title, ["ddl", "dml"], steps)


def gen_ddl_dml(seed: int, ref_dir: Path) -> list[dict[str, Any]]:
    """Schemas and DML are drawn from SCALAR_TYPES only; ``ref_dir`` is not needed."""
    rng = random.Random(seed)
    return [_ddl_dml_case(rng, index, seed) for index in range(_DDL_CASES_PER_SEED)]


GENERATORS: dict[str, Callable[[int, Path], list[dict[str, Any]]]] = {
    "operators": gen_operators,
    "casts": gen_casts,
    "io": gen_io,
    "ddl-dml": gen_ddl_dml,
}


def write_cases(cases: list[dict[str, Any]], out_root: Path) -> list[str]:
    """Write per module docstring; return conflicting paths."""
    root = Path(out_root)
    conflicts: list[str] = []
    for case in cases:
        family = case["family"]
        rest = case["id"][len(family) + 1:]
        path = root / family / f"{rest}.yaml"
        text = yaml.safe_dump(case, sort_keys=False, allow_unicode=True, width=1000)
        path.parent.mkdir(parents=True, exist_ok=True)
        try:
            with path.open("x", encoding="utf-8", newline="\n") as fh:
                fh.write(text)
        except FileExistsError:
            if path.read_text(encoding="utf-8") != text:
                conflicts.append(str(path))
    return conflicts
