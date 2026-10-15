"""HAR-15 (ORACLE-05): tests for the official regress suite manifest.

Inline parsing tests run everywhere. The live test reads the PostgreSQL 18 source tree at
``config.paths().pg_regress_dir`` and is skipped when that tree is absent.
"""

from __future__ import annotations

import json
from pathlib import Path

import pytest

from pgarbor_harness import config
from pgarbor_harness.regress import manifest as M

SAMPLE_SCHEDULE = """# ----------
# header
#
# more header
# ----------

# required setup steps
test: test_setup

# ----------
# The first group of parallel tests
# ----------
test: boolean char
test: name
"""


def test_parse_schedule_groups_and_comments() -> None:
    groups, comments = M.parse_schedule(SAMPLE_SCHEDULE)
    assert groups == [["test_setup"], ["boolean", "char"], ["name"]]
    assert comments == [
        "required setup steps",
        "The first group of parallel tests",
        "",
    ]


def test_parse_schedule_multi_line_comment_is_kept() -> None:
    text = "# geometry depends on point\n# horology depends on date\ntest: geometry\n"
    groups, comments = M.parse_schedule(text)
    assert groups == [["geometry"]]
    assert comments == ["geometry depends on point\nhorology depends on date"]


@pytest.mark.parametrize("text", ["ignore: foo\n", "test:\n", "bogus line\n"])
def test_parse_schedule_rejects_other_directives(text: str) -> None:
    with pytest.raises(ValueError):
        M.parse_schedule(text)


def test_parse_resultmap_entry() -> None:
    entries = M.parse_resultmap("float4:out:.*-.*-cygwin.*=float4-misrounded-input.out\n")
    assert entries == [("float4", "out", ".*-.*-cygwin.*", "float4-misrounded-input.out")]


@pytest.mark.parametrize("line", ["float4:out=x.out", "float4:out:plat", "float4::p=x.out"])
def test_parse_resultmap_malformed(line: str) -> None:
    with pytest.raises(ValueError):
        M.parse_resultmap(line + "\n")


def test_platform_matches_pg_regress_rules() -> None:
    assert M.platform_matches("axb", "a.b")
    assert not M.platform_matches("ab", "a.b")
    assert M.platform_matches("x86_64-pc-linux-gnu", "x86")  # implicit ^, prefix match
    assert not M.platform_matches("aarch64-linux", "x86")
    assert M.platform_matches("x86_64-pc-linux-gnu", "x86_64-.*-linux.*")


def test_resultmap_substitute_last_match_wins() -> None:
    entries = M.parse_resultmap("t:out:x86_64-.*-linux.*=t_alt.out\nt:out:.*=t_last.out\n")
    assert M.resultmap_substitute(entries, "t", "x86_64-pc-linux-gnu") == "t_last.out"
    only_linux = M.parse_resultmap("t:out:x86_64-.*-linux.*=t_alt.out\n")
    assert M.resultmap_substitute(only_linux, "t", "x86_64-pc-linux-gnu") == "t_alt.out"
    assert M.resultmap_substitute(only_linux, "t", "aarch64-apple-darwin") is None
    assert M.resultmap_substitute(only_linux, "other", "x86_64-pc-linux-gnu") is None


def test_analyze_sql_fields() -> None:
    sql = """\\getenv abs_srcdir PG_ABS_SRCDIR
\\set regresslib :libdir '/regress' :dlsuffix
LOAD :'regresslib';
CREATE TABLESPACE regress_tblspace LOCATION '';
COPY onek FROM :'abs_srcdir' '/data/onek.data';
CREATE ROLE regress_x;
SELECT E'\\\\d' ; -- \\pset not real
\\d+ onek
"""
    fields = M.analyze_sql(sql)
    assert fields["meta_commands"] == ["\\d+", "\\getenv", "\\set"]
    assert fields["uses_regresslib"] is True
    assert fields["data_files"] == ["/data/onek.data"]
    assert fields["uses_tablespace"] is True
    assert fields["uses_create_role"] is True
    assert fields["uses_create_database"] is False


def test_analyze_sql_plain() -> None:
    fields = M.analyze_sql("SELECT 1;\n")
    assert fields == {
        "meta_commands": [],
        "uses_regresslib": False,
        "data_files": [],
        "uses_tablespace": False,
        "uses_create_role": False,
        "uses_create_database": False,
    }


def _write(path: Path, text: str) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text(text, encoding="utf-8")


def _make_tree(src: Path) -> Path:
    """Synthetic ``src/test/regress`` tree with ``src/include/pg_config.h``."""
    regress = src / "test" / "regress"
    _write(
        regress / "parallel_schedule",
        "# required setup steps\ntest: test_setup\n\n# group two\ntest: char missing\n",
    )
    _write(regress / "sql" / "test_setup.sql", "SELECT 1;\n")
    _write(regress / "sql" / "char.sql", "\\pset format aligned\nSELECT 'x';\n")
    _write(regress / "expected" / "test_setup.out", "")
    for name in ["char.out", "char_1.out", "char_2.out", "char_9.out", "char_10.out"]:
        _write(regress / "expected" / name, "out\n")
    _write(regress / "resultmap", "char:out:.*=char_1.out\n")
    _write(src / "include" / "pg_config.h", '#define PG_VERSION "18.6"\n')
    return regress


def test_load_synthetic_tree(tmp_path: Path) -> None:
    regress = _make_tree(tmp_path / "src")
    schedule = M.load(regress)
    assert schedule.groups == [["test_setup"], ["char", "missing"]]
    assert schedule.group_comments == ["required setup steps", "group two"]
    assert "missing" not in schedule.tests
    assert any("missing" in err for err in schedule.errors)
    char = schedule.tests["char"]
    # base first, then _0.._9 that exist; _10 is not a pg_regress variant
    assert [p.name for p in char.expected_paths] == [
        "char.out",
        "char_1.out",
        "char_2.out",
        "char_9.out",
    ]
    assert char.meta_commands == ["\\pset"]
    assert schedule.tests["test_setup"].uses_regresslib is False


def test_build_manifest_synthetic_tree(tmp_path: Path) -> None:
    regress = _make_tree(tmp_path / "src")
    manifest = M.build_manifest(regress)
    assert manifest["format"] == M.MANIFEST_FORMAT
    assert manifest["pg_source_version"] == "18.6"
    assert manifest["groups"][0] == {
        "index": 0,
        "tests": ["test_setup"],
        "group_comment": "required setup steps",
    }
    assert manifest["groups"][1]["tests"] == ["char", "missing"]
    char = manifest["tests"]["char"]
    assert char["group"] == 1
    assert char["sql"] == "sql/char.sql"
    assert char["expected"][0]["path"] == "expected/char.out"
    assert len(char["expected"][0]["sha256"]) == 64
    assert char["resultmap"] == [{"output": "out", "platform": ".*", "expected": "char_1.out"}]
    assert any("missing" in err for err in manifest["errors"])
    json.dumps(manifest)  # must be serializable


def test_build_manifest_version_unknown_without_pg_config(tmp_path: Path) -> None:
    regress = tmp_path / "src" / "test" / "regress"
    _write(regress / "parallel_schedule", "test: a\n")
    _write(regress / "sql" / "a.sql", "SELECT 1;\n")
    _write(regress / "expected" / "a.out", "")
    manifest = M.build_manifest(regress)
    assert manifest["pg_source_version"] == "unknown"
    assert manifest["errors"] == []


PG_REGRESS = config.paths().pg_regress_dir


@pytest.mark.skipif(
    not (PG_REGRESS / "parallel_schedule").is_file(),
    reason="PostgreSQL 18 source tree not present",
)
def test_live_pg18_schedule() -> None:
    schedule = M.load(PG_REGRESS)
    assert schedule.groups[0] == ["test_setup"]
    assert schedule.errors == []
    assert [p.name for p in schedule.tests["char"].expected_paths][:3] == [
        "char.out",
        "char_1.out",
        "char_2.out",
    ]
    assert schedule.tests["test_setup"].uses_regresslib is True
    for name in schedule.ordered_tests():
        assert name in schedule.tests, name
        assert schedule.tests[name].sql_path.is_file(), name


@pytest.mark.skipif(
    not (PG_REGRESS / "parallel_schedule").is_file(),
    reason="PostgreSQL 18 source tree not present",
)
def test_live_pg18_manifest_is_serializable() -> None:
    manifest = M.build_manifest(PG_REGRESS)
    assert manifest["format"] == M.MANIFEST_FORMAT
    assert manifest["groups"][0]["tests"] == ["test_setup"]
    assert manifest["tests"]["test_setup"]["uses_regresslib"] is True
    json.dumps(manifest)
