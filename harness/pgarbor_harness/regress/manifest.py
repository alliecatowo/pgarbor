"""ORACLE-05: manifest of the official PostgreSQL 18 regression suite.

Lease HAR-15.

Source: ``config.paths().pg_regress_dir`` = ``~/develop/reference/postgres-18/src/test/regress``
(PostgreSQL license; we READ these files and run them unmodified, never copy them into the repo).

Inputs parsed:
  * ``parallel_schedule``: lines ``test: name1 name2 ...`` form one parallel group each, in
    file order; ``#`` starts a comment line; blank lines ignored; any other directive ->
    ValueError (PG18 uses only ``test:``). The comment block immediately preceding a group
    (consecutive ``#`` lines, stripped of ``# `` and of separator lines of dashes) is kept
    verbatim as ``group_comment`` (human prerequisite notes like "geometry depends on point,
    lseg, ...").
  * ``sql/<name>.sql`` (must exist for every scheduled test; missing -> manifest error entry,
    never silently skipped), ``expected/<name>.out`` and variants ``expected/<name>_<N>.out``
    for N in 0..9 (pg_regress tries them all), and ``resultmap`` lines
    ``testname:output:platformpattern=expectedfile`` (record them; on Linux none apply in PG18
    but keep the mechanism: a matching entry substitutes the base expected file).

Per-test static analysis of the .sql text (lexical, via simple regexes on lines):
  * ``meta_commands``: sorted set of psql backslash commands used (``\\getenv``, ``\\set``,
    ``\\copy``, ``\\gset``, ``\\if``, ``\\d+``, ...; record the command word only)
  * ``uses_regresslib``: references ``:'regresslib'`` or ``regress.so`` — needs the C test
    library that the stock postgres image does NOT ship (``test_setup`` itself creates
    functions from it), so PG-in-container cannot pass these exactly: their official status on
    the oracle is expected BLOCKED(env), and the differential baseline (regress.runner) is what
    makes them useful.
  * ``data_files``: ``abs_srcdir``-relative paths mentioned (``/data/onek.data`` ...).
  * ``uses_tablespace``, ``uses_create_role``, ``uses_create_database``: substring checks for
    ``TABLESPACE``, ``CREATE ROLE``/``CREATE USER``, ``CREATE DATABASE`` (case-insensitive).

Manifest document (canonical JSON; written to runs/state/regress_manifest.json)::

    {"format": "pgarbor-regress-manifest/1", "pg_source_version": "<contents of
     src/include/pg_config.h PG_VERSION if present else 'unknown'>",
     "groups": [{"index": 0, "tests": ["test_setup"], "group_comment": "required setup steps"}],
     "tests": {"boolean": {"group": 1, "sql": "sql/boolean.sql", "sql_sha256": "...",
                "expected": [{"path": "expected/boolean.out", "sha256": "..."}],
                "meta_commands": ["\\\\pset"], "uses_regresslib": false, "data_files": [],
                "uses_tablespace": false, "uses_create_role": false,
                "uses_create_database": false}},
     "errors": []}
"""

from __future__ import annotations

import re
from dataclasses import dataclass, field
from pathlib import Path
from typing import Any

from pgarbor_harness.hashing import sha256_file

MANIFEST_FORMAT = "pgarbor-regress-manifest/1"

#: Highest ``_N`` expected-output variant pg_regress considers (``expected/<name>_N.out``).
_MAX_VARIANT = 9
#: A single-quoted SQL string literal on one line (``''`` is an escaped quote).
_QUOTED_RE = re.compile(r"'(?:[^']|'')*'")
#: psql meta-command word at a token boundary (``\d+``, ``\getenv``, ``\?``).
_META_RE = re.compile(r"(?:^|[\s;])(\\(?:[A-Za-z]+\+?|\?))")
#: ``abs_srcdir``-relative data file references: ``:'abs_srcdir' '/data/onek.data'``.
_DATA_RE = re.compile(r"abs_srcdir'?[ \t]*'(/[^'\n]+)'")
_REGRESSLIB_RE = re.compile(r":'?regresslib\b")
_TABLESPACE_RE = re.compile(r"TABLESPACE", re.IGNORECASE)
_CREATE_ROLE_RE = re.compile(r"CREATE\s+(?:ROLE|USER)\b", re.IGNORECASE)
_CREATE_DATABASE_RE = re.compile(r"CREATE\s+DATABASE\b", re.IGNORECASE)
_PG_VERSION_RE = re.compile(r'^#define\s+PG_VERSION\s+"([^"]*)"', re.MULTILINE)


@dataclass
class RegressTest:
    name: str
    group: int
    sql_path: Path
    expected_paths: list[Path] = field(default_factory=list)  #: base first, then _0.._9
    meta_commands: list[str] = field(default_factory=list)
    uses_regresslib: bool = False
    data_files: list[str] = field(default_factory=list)
    uses_tablespace: bool = False
    uses_create_role: bool = False
    uses_create_database: bool = False


@dataclass
class RegressSchedule:
    groups: list[list[str]]
    group_comments: list[str]
    tests: dict[str, RegressTest]
    errors: list[str] = field(default_factory=list)

    def ordered_tests(self) -> list[str]:
        """All test names in schedule order (implemented)."""
        return [t for g in self.groups for t in g]


def parse_schedule(text: str) -> tuple[list[list[str]], list[str]]:
    """Parse a schedule file into (groups, group_comments) per module docstring."""
    groups: list[list[str]] = []
    group_comments: list[str] = []
    # ``block`` is the comment block being read; ``pending`` is the most recent block that
    # has not yet been claimed by a ``test:`` line (blank lines do not discard it).
    block: list[str] = []
    pending: list[str] | None = None
    in_block = False
    for lineno, raw in enumerate(text.splitlines(), start=1):
        line = raw.strip()
        if not line:
            in_block = False
            continue
        if line.startswith("#"):
            if not in_block:
                block = []
                pending = block
                in_block = True
            body = line[1:].strip()
            # Separator lines of dashes and empty comment lines carry no information.
            if body and set(body) != {"-"}:
                block.append(body)
            continue
        in_block = False
        if not line.startswith("test:"):
            raise ValueError(
                f"parallel_schedule line {lineno}: unsupported directive {line!r} "
                "(only 'test:' lines are allowed)"
            )
        names = line[len("test:"):].split()
        if not names:
            raise ValueError(f"parallel_schedule line {lineno}: 'test:' names no tests")
        groups.append(names)
        group_comments.append("\n".join(pending) if pending else "")
        pending = None
    return groups, group_comments


def parse_resultmap(text: str) -> list[tuple[str, str, str, str]]:
    """``testname:output:platformpattern=expectedfile`` -> tuples; malformed -> ValueError."""
    entries: list[tuple[str, str, str, str]] = []
    for lineno, raw in enumerate(text.splitlines(), start=1):
        line = raw.rstrip()
        if not line:
            continue
        # Same splitting as pg_regress load_resultmap(): first ':', second ':', first '='.
        testname, sep1, rest = line.partition(":")
        output, sep2, rest = rest.partition(":")
        platform, sep3, expected = rest.partition("=")
        if not (sep1 and sep2 and sep3) or not (testname and output and platform and expected):
            raise ValueError(
                f"resultmap line {lineno}: malformed entry {line!r} "
                "(expected testname:output:platformpattern=expectedfile)"
            )
        entries.append((testname, output, platform, expected))
    return entries


def platform_matches(host_platform: str, pattern: str) -> bool:
    """pg_regress ``string_matches_pattern``: implicit leading ``^``, ``.`` matches any one
    character, ``.*`` any run, and the pattern need not consume the whole platform string."""
    parts: list[str] = []
    i = 0
    while i < len(pattern):
        if pattern[i] == "." and pattern[i + 1 : i + 2] == "*":
            parts.append(".*")
            i += 2
        elif pattern[i] == ".":
            parts.append(".")
            i += 1
        else:
            parts.append(re.escape(pattern[i]))
            i += 1
    return re.match("".join(parts), host_platform, re.DOTALL) is not None


def resultmap_substitute(
    entries: list[tuple[str, str, str, str]],
    testname: str,
    host_platform: str,
    output: str = "out",
) -> str | None:
    """Expected-file name that replaces the base ``expected/<testname>.<output>`` on
    ``host_platform``, or None. As in pg_regress, the last matching entry wins."""
    found: str | None = None
    for name, out, platform, expected in entries:
        if name == testname and out == output and platform_matches(host_platform, platform):
            found = expected
    return found


def analyze_sql(sql_text: str) -> dict[str, Any]:
    """Static analysis fields listed in the module docstring."""
    meta: set[str] = set()
    for raw in sql_text.splitlines():
        # Drop string literals and line comments so backslashes inside them are ignored.
        line = _QUOTED_RE.sub("''", raw).split("--", 1)[0]
        for match in _META_RE.finditer(line):
            meta.add(match.group(1))
    return {
        "meta_commands": sorted(meta),
        "uses_regresslib": _REGRESSLIB_RE.search(sql_text) is not None
        or "regress.so" in sql_text,
        "data_files": sorted({m.group(1) for m in _DATA_RE.finditer(sql_text)}),
        "uses_tablespace": _TABLESPACE_RE.search(sql_text) is not None,
        "uses_create_role": _CREATE_ROLE_RE.search(sql_text) is not None,
        "uses_create_database": _CREATE_DATABASE_RE.search(sql_text) is not None,
    }


def _expected_candidates(expected_dir: Path, name: str) -> list[Path]:
    """Existing expected files for ``name``: base first, then ``_0.._9`` in numeric order."""
    candidates: list[Path] = []
    base = expected_dir / f"{name}.out"
    if base.is_file():
        candidates.append(base)
    for n in range(_MAX_VARIANT + 1):
        variant = expected_dir / f"{name}_{n}.out"
        if variant.is_file():
            candidates.append(variant)
    return candidates


def load(regress_dir: Path, schedule_name: str = "parallel_schedule") -> RegressSchedule:
    """Build the RegressSchedule from the PG source tree."""
    schedule_text = (regress_dir / schedule_name).read_text(encoding="utf-8")
    groups, group_comments = parse_schedule(schedule_text)
    errors: list[str] = []
    tests: dict[str, RegressTest] = {}
    first_group: dict[str, int] = {}
    expected_dir = regress_dir / "expected"
    for group_index, names in enumerate(groups):
        for name in names:
            if name in first_group:
                errors.append(
                    f"{schedule_name}: test {name!r} scheduled in groups "
                    f"{first_group[name]} and {group_index}; kept the first"
                )
                continue
            first_group[name] = group_index
            sql_path = regress_dir / "sql" / f"{name}.sql"
            if not sql_path.is_file():
                errors.append(
                    f"test {name!r} (group {group_index}): missing sql/{name}.sql"
                )
                continue
            expected = _expected_candidates(expected_dir, name)
            if not expected:
                errors.append(
                    f"test {name!r} (group {group_index}): no expected/{name}.out "
                    f"or {name}_N.out variant"
                )
            fields = analyze_sql(sql_path.read_text(encoding="utf-8", errors="replace"))
            tests[name] = RegressTest(
                name=name,
                group=group_index,
                sql_path=sql_path,
                expected_paths=expected,
                meta_commands=fields["meta_commands"],
                uses_regresslib=fields["uses_regresslib"],
                data_files=fields["data_files"],
                uses_tablespace=fields["uses_tablespace"],
                uses_create_role=fields["uses_create_role"],
                uses_create_database=fields["uses_create_database"],
            )
    return RegressSchedule(
        groups=groups, group_comments=group_comments, tests=tests, errors=errors
    )


def _pg_source_version(regress_dir: Path) -> str:
    """``PG_VERSION`` from ``src/include/pg_config.h`` (two levels above regress_dir)."""
    config_h = regress_dir.parent.parent / "include" / "pg_config.h"
    try:
        text = config_h.read_text(encoding="utf-8", errors="replace")
    except OSError:
        return "unknown"
    match = _PG_VERSION_RE.search(text)
    return match.group(1) if match else "unknown"


def build_manifest(regress_dir: Path) -> dict[str, Any]:
    """RegressSchedule -> manifest document (module docstring). Paths relative to
    regress_dir, POSIX."""
    schedule = load(regress_dir)
    errors = list(schedule.errors)

    # ``resultmap`` entries are recorded per test (``resultmap`` key); substitution for a
    # concrete host is done by ``resultmap_substitute`` at run time.
    resultmap_entries: list[tuple[str, str, str, str]] = []
    resultmap_path = regress_dir / "resultmap"
    if resultmap_path.is_file():
        try:
            resultmap_entries = parse_resultmap(resultmap_path.read_text(encoding="utf-8"))
        except ValueError as exc:
            errors.append(f"resultmap: {exc}")
    for name, output, platform, expected in resultmap_entries:
        if not (regress_dir / "expected" / expected).is_file():
            errors.append(
                f"resultmap: {name}:{output}:{platform} substitutes missing expected/{expected}"
            )

    groups = [
        {"index": index, "tests": names, "group_comment": comment}
        for index, (names, comment) in enumerate(zip(schedule.groups, schedule.group_comments))
    ]
    tests: dict[str, Any] = {}
    for name, test in schedule.tests.items():
        tests[name] = {
            "group": test.group,
            "sql": _relative(test.sql_path, regress_dir),
            "sql_sha256": sha256_file(test.sql_path),
            "expected": [
                {"path": _relative(path, regress_dir), "sha256": sha256_file(path)}
                for path in test.expected_paths
            ],
            "meta_commands": list(test.meta_commands),
            "uses_regresslib": test.uses_regresslib,
            "data_files": list(test.data_files),
            "uses_tablespace": test.uses_tablespace,
            "uses_create_role": test.uses_create_role,
            "uses_create_database": test.uses_create_database,
            "resultmap": [
                {"output": output, "platform": platform, "expected": expected}
                for entry_name, output, platform, expected in resultmap_entries
                if entry_name == name
            ],
        }
    return {
        "format": MANIFEST_FORMAT,
        "pg_source_version": _pg_source_version(regress_dir),
        "groups": groups,
        "tests": tests,
        "errors": errors,
    }


def _relative(path: Path, base: Path) -> str:
    return path.relative_to(base).as_posix()
