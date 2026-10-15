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

from dataclasses import dataclass, field
from pathlib import Path
from typing import Any

MANIFEST_FORMAT = "pgarbor-regress-manifest/1"


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
    raise NotImplementedError("HAR-15: parse parallel_schedule")


def parse_resultmap(text: str) -> list[tuple[str, str, str, str]]:
    """``testname:output:platformpattern=expectedfile`` -> tuples; malformed -> ValueError."""
    raise NotImplementedError("HAR-15: parse resultmap")


def analyze_sql(sql_text: str) -> dict[str, Any]:
    """Static analysis fields listed in the module docstring."""
    raise NotImplementedError("HAR-15: analyze test sql")


def load(regress_dir: Path, schedule_name: str = "parallel_schedule") -> RegressSchedule:
    """Build the RegressSchedule from the PG source tree."""
    raise NotImplementedError("HAR-15: load regress suite")


def build_manifest(regress_dir: Path) -> dict[str, Any]:
    """RegressSchedule -> manifest document (module docstring). Paths relative to
    regress_dir, POSIX."""
    raise NotImplementedError("HAR-15: build manifest document")
