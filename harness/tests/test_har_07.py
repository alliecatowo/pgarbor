"""Tests for HAR-07: corpus discovery, strict YAML loading, id uniqueness and filters."""

from __future__ import annotations

import hashlib
from pathlib import Path

import pytest
import yaml

from pgarbor_harness.cases.loader import (
    Corpus,
    InvalidCase,
    StrictLoader,
    load_corpus,
    load_file,
    select,
)
from pgarbor_harness.cases.model import Case
from pgarbor_harness.config import paths

VALID_SMOKE = """\
id: smoke.x
title: t
family: smoke
feature_ids: [select]
steps:
  - sql: |
      SELECT 1
"""


@pytest.fixture
def repo(tmp_path: Path) -> Path:
    (tmp_path / "tests" / "cases").mkdir(parents=True)
    return tmp_path


def _write(repo: Path, rel: str, content: str | bytes) -> Path:
    p = repo / rel
    p.parent.mkdir(parents=True, exist_ok=True)
    p.write_bytes(content.encode("utf-8") if isinstance(content, str) else content)
    return p


def _cases_root(repo: Path) -> Path:
    return repo / "tests" / "cases"


def _case_dict(cid: str, family: str = "smoke", tier: str | None = None,
               features: list[str] | None = None, quarantine: dict | None = None) -> dict:
    d: dict = {
        "id": cid,
        "title": "t",
        "family": family,
        "feature_ids": features if features is not None else ["select"],
        "steps": [{"sql": "SELECT 1"}],
    }
    if tier is not None:
        d["tier"] = tier
    if quarantine is not None:
        d["quarantine"] = quarantine
    return d


# ---------------------------------------------------------------------------------------------
# Strict loader
# ---------------------------------------------------------------------------------------------


def test_strict_loader_rejects_top_level_duplicate_key() -> None:
    with pytest.raises(yaml.constructor.ConstructorError, match="duplicate key"):
        yaml.load("a: 1\na: 2\n", Loader=StrictLoader)


def test_strict_loader_rejects_nested_duplicate_key() -> None:
    text = "steps:\n  - sql: x\n    sql: y\n"
    with pytest.raises(yaml.constructor.ConstructorError, match="duplicate key"):
        yaml.load(text, Loader=StrictLoader)


def test_strict_loader_accepts_distinct_keys() -> None:
    assert yaml.load("a: 1\nb: [1, 2]\n", Loader=StrictLoader) == {"a": 1, "b": [1, 2]}


# ---------------------------------------------------------------------------------------------
# Single-file loading
# ---------------------------------------------------------------------------------------------


def test_valid_file_loads_with_path_and_hash(repo: Path) -> None:
    p = _write(repo, "tests/cases/smoke/x.yaml", VALID_SMOKE)
    entry = load_file(p, repo)
    assert isinstance(entry, Case)
    assert entry.id == "smoke.x"
    assert entry.path == "tests/cases/smoke/x.yaml"
    assert entry.sha256 == hashlib.sha256(VALID_SMOKE.encode("utf-8")).hexdigest()


def test_duplicate_key_file_is_invalid(repo: Path) -> None:
    text = VALID_SMOKE + "title: again\n"
    p = _write(repo, "tests/cases/smoke/x.yaml", text)
    entry = load_file(p, repo)
    assert isinstance(entry, InvalidCase)
    assert entry.path == "tests/cases/smoke/x.yaml"
    assert any("duplicate key" in e for e in entry.errors)


def test_duplicate_key_corpus_reports_invalid_not_last_wins(repo: Path) -> None:
    _write(repo, "tests/cases/smoke/x.yaml", VALID_SMOKE + "title: again\n")
    corpus = load_corpus(_cases_root(repo), repo)
    assert corpus.cases == []
    assert [i.path for i in corpus.invalid] == ["tests/cases/smoke/x.yaml"]


def test_yml_extension_is_invalid(repo: Path) -> None:
    _write(repo, "tests/cases/smoke/x.yml", VALID_SMOKE)
    corpus = load_corpus(_cases_root(repo), repo)
    assert corpus.cases == []
    assert len(corpus.invalid) == 1
    assert "yml" in corpus.invalid[0].errors[0]


def test_bom_rejected(repo: Path) -> None:
    p = _write(repo, "tests/cases/smoke/x.yaml", b"\xef\xbb\xbf" + VALID_SMOKE.encode())
    entry = load_file(p, repo)
    assert isinstance(entry, InvalidCase)
    assert any("byte order mark" in e for e in entry.errors)


def test_non_utf8_rejected(repo: Path) -> None:
    p = _write(repo, "tests/cases/smoke/x.yaml", b"id: smoke.x\ntitle: \xff\xfe\n")
    entry = load_file(p, repo)
    assert isinstance(entry, InvalidCase)
    assert any("UTF-8" in e for e in entry.errors)


def test_multiple_documents_rejected(repo: Path) -> None:
    p = _write(repo, "tests/cases/smoke/x.yaml", VALID_SMOKE + "---\nid: smoke.y\n")
    entry = load_file(p, repo)
    assert isinstance(entry, InvalidCase)
    assert any("exactly one YAML document" in e for e in entry.errors)


def test_empty_file_rejected(repo: Path) -> None:
    p = _write(repo, "tests/cases/smoke/x.yaml", "# only a comment\n")
    entry = load_file(p, repo)
    assert isinstance(entry, InvalidCase)
    assert any("found 0" in e for e in entry.errors)


def test_non_mapping_root_rejected(repo: Path) -> None:
    p = _write(repo, "tests/cases/smoke/x.yaml", "- a\n- b\n")
    entry = load_file(p, repo)
    assert isinstance(entry, InvalidCase)
    assert any("root must be a mapping" in e for e in entry.errors)


def test_malformed_yaml_rejected(repo: Path) -> None:
    p = _write(repo, "tests/cases/smoke/x.yaml", "id: [unclosed\n")
    entry = load_file(p, repo)
    assert isinstance(entry, InvalidCase)
    assert any(e.startswith("YAML error") for e in entry.errors)


def test_schema_violation_is_invalid_with_case_id(repo: Path) -> None:
    _write(repo, "tests/cases/smoke/x.yaml", VALID_SMOKE.replace("title: t", "title: 5"))
    corpus = load_corpus(_cases_root(repo), repo)
    assert corpus.cases == []
    assert corpus.invalid[0].case_id == "smoke.x"
    assert corpus.invalid[0].errors


# ---------------------------------------------------------------------------------------------
# Corpus discovery and id uniqueness
# ---------------------------------------------------------------------------------------------


def test_duplicate_ids_across_files_are_both_invalid(repo: Path) -> None:
    _write(repo, "tests/cases/smoke/a.yaml", VALID_SMOKE)
    _write(repo, "tests/cases/smoke/b.yaml", VALID_SMOKE)
    corpus = load_corpus(_cases_root(repo), repo)
    assert corpus.cases == []
    assert [i.path for i in corpus.invalid] == [
        "tests/cases/smoke/a.yaml",
        "tests/cases/smoke/b.yaml",
    ]
    assert "duplicate id also defined in tests/cases/smoke/b.yaml" in corpus.invalid[0].errors
    assert "duplicate id also defined in tests/cases/smoke/a.yaml" in corpus.invalid[1].errors


def test_unique_ids_both_load_in_path_order(repo: Path) -> None:
    _write(repo, "tests/cases/smoke/b.yaml", VALID_SMOKE.replace("smoke.x", "smoke.b"))
    _write(repo, "tests/cases/smoke/a.yaml", VALID_SMOKE.replace("smoke.x", "smoke.a"))
    corpus = load_corpus(_cases_root(repo), repo)
    assert corpus.invalid == []
    assert [c.path for c in corpus.cases] == [
        "tests/cases/smoke/a.yaml",
        "tests/cases/smoke/b.yaml",
    ]
    assert set(corpus.by_id()) == {"smoke.a", "smoke.b"}


def test_readme_and_dotfiles_ignored(repo: Path) -> None:
    _write(repo, "tests/cases/README.md", "# not a case\n")
    _write(repo, "tests/cases/smoke/README.md", "# not a case\n")
    _write(repo, "tests/cases/smoke/.hidden.yaml", VALID_SMOKE)
    _write(repo, "tests/cases/smoke/notes.txt", "ignored\n")
    _write(repo, "tests/cases/smoke/x.yaml", VALID_SMOKE)
    corpus = load_corpus(_cases_root(repo), repo)
    assert [c.id for c in corpus.cases] == ["smoke.x"]
    assert corpus.invalid == []


def test_discovery_is_recursive(repo: Path) -> None:
    _write(repo, "tests/cases/smoke/sub/x.yaml", VALID_SMOKE)
    corpus = load_corpus(_cases_root(repo), repo)
    assert [c.path for c in corpus.cases] == ["tests/cases/smoke/sub/x.yaml"]


def test_real_corpus_accounts_for_every_yaml_file() -> None:
    root = paths().root
    cases_root = root / "tests" / "cases"
    expected = sum(
        1
        for p in cases_root.rglob("*")
        if p.is_file()
        and p.name != "README.md"
        and not any(part.startswith(".") for part in p.relative_to(cases_root).parts)
        and p.name.endswith((".yaml", ".yml"))
    )
    corpus = load_corpus(cases_root, root)
    assert len(corpus.cases) + len(corpus.invalid) == expected


# ---------------------------------------------------------------------------------------------
# Filters
# ---------------------------------------------------------------------------------------------


def _corpus() -> Corpus:
    entries = [
        _case_dict("smoke.b", tier="slice", features=["select", "insert"]),
        _case_dict("smoke.a", tier="core", features=["select"]),
        _case_dict("txn.c", family="txn", tier="core", features=["begin"]),
        _case_dict("txn.d", family="txn", tier="extended", features=["savepoint"],
                   quarantine={"reason": "flaky"}),
    ]
    cases = [Case.from_dict(d, path=f"tests/cases/{d['family']}/x.yaml") for d in entries]
    return Corpus(cases=cases, invalid=[])


def _ids(cases: list[Case]) -> list[str]:
    return [c.id for c in cases]


def test_select_no_filters_excludes_quarantined_and_sorts_by_id() -> None:
    assert _ids(select(_corpus())) == ["smoke.a", "smoke.b", "txn.c"]


def test_select_include_quarantined() -> None:
    assert _ids(select(_corpus(), include_quarantined=True)) == [
        "smoke.a", "smoke.b", "txn.c", "txn.d"]


def test_select_family_exact() -> None:
    assert _ids(select(_corpus(), families=["txn"])) == ["txn.c"]
    assert _ids(select(_corpus(), families=["tx"])) == []


def test_select_feature_any_of() -> None:
    assert _ids(select(_corpus(), features=["insert", "begin"])) == ["smoke.b", "txn.c"]


def test_select_tier() -> None:
    assert _ids(select(_corpus(), tiers=["slice"])) == ["smoke.b"]


def test_select_id_glob() -> None:
    assert _ids(select(_corpus(), id_glob="smoke.*")) == ["smoke.a", "smoke.b"]
    assert _ids(select(_corpus(), id_glob="*.c")) == ["txn.c"]


def test_select_filters_combine() -> None:
    result = select(_corpus(), families=["smoke"], features=["select"], tiers=["core"],
                    id_glob="smoke.?", include_quarantined=False)
    assert _ids(result) == ["smoke.a"]


def test_select_empty_glob_means_no_restriction() -> None:
    assert _ids(select(_corpus(), id_glob="")) == ["smoke.a", "smoke.b", "txn.c"]
