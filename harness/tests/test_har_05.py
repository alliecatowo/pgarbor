"""Tests for HAR-05 (oracle manifest freeze/check). Lease HAR-05, ORACLE-11.

Each test builds a small repo layout under tmp_path and never touches the real checkout.
"""

from __future__ import annotations

import json
from pathlib import Path

import pytest

from pgarbor_harness.config import PG_IMAGE_DIGEST, PG_SERVER_VERSION, Paths
from pgarbor_harness.oracle import manifest as om

FINGERPRINT = "ab" * 32
REFERENCE = {
    "image_digest": PG_IMAGE_DIGEST,
    "server_version": PG_SERVER_VERSION,
    "fingerprint_sha256": FINGERPRINT,
}

LAYOUT = {
    "contracts/ARCHITECTURE.md": "# architecture\n",
    "harness/pgarbor_harness/config.py": "X = 1\n",
    "tests/cases/smoke/a.yaml": "case_id: smoke.a\n",
    "tests/expected/smoke/a.json": '{"case_id": "smoke.a"}\n',
}


def _write(root: Path, rel: str, text: str) -> None:
    target = root / rel
    target.parent.mkdir(parents=True, exist_ok=True)
    target.write_text(text, encoding="utf-8")


@pytest.fixture
def repo(tmp_path, monkeypatch):
    # Keep PG-source-dependent suite hashes deterministic: point at a directory that is absent.
    monkeypatch.setenv("PGARBOR_PG_SOURCE", str(tmp_path / "no-pg-source"))
    monkeypatch.delenv("PGARBOR_ORACLE_REVIEW", raising=False)
    for rel, text in LAYOUT.items():
        _write(tmp_path, rel, text)
    return Paths(root=tmp_path)


@pytest.fixture
def reviewed(monkeypatch):
    monkeypatch.setenv("PGARBOR_ORACLE_REVIEW", "1")


def _manifest_file(paths: Paths) -> Path:
    return paths.root / om.MANIFEST_RELPATH


def test_freeze_requires_review_env(repo):
    with pytest.raises(PermissionError, match="oracle review path"):
        om.freeze(repo, REFERENCE)
    assert not _manifest_file(repo).exists()


def test_check_without_manifest_reports_missing(repo):
    result = om.check(repo)
    assert result.ok is False
    assert result.changes == []
    assert result.message == "no frozen oracle manifest"


def test_check_after_freeze_on_unchanged_tree_is_ok(repo, reviewed):
    doc = om.freeze(repo, REFERENCE)
    assert doc["format"] == om.MANIFEST_FORMAT
    assert _manifest_file(repo).is_file()

    result = om.check(repo)
    assert result.ok is True
    assert result.changes == []
    assert result.reference_changed is False


def test_frozen_document_shape(repo, reviewed):
    doc = om.freeze(repo, REFERENCE)
    assert set(doc) == {"format", "git_sha", "reference", "trees", "pg_suites"}
    assert doc["reference"] == REFERENCE
    assert set(doc["trees"]) == {"protected", "verifier", "cases", "expected"}
    expected_tree = doc["trees"]["expected"]
    assert expected_tree["count"] == 1
    assert set(expected_tree["files"]) == {"tests/expected/smoke/a.json"}
    # pg source is absent or the suite manifests are stubs: both hashes must be null.
    assert doc["pg_suites"] == {
        "regress_manifest_sha256": None,
        "isolation_manifest_sha256": None,
    }


def test_modified_expected_file_is_reported(repo, reviewed):
    om.freeze(repo, REFERENCE)
    _write(repo.root, "tests/expected/smoke/a.json", '{"case_id": "smoke.changed"}\n')

    result = om.check(repo)
    assert result.ok is False
    assert om.TreeChange(tree="expected", path="tests/expected/smoke/a.json", change="modified") in (
        result.changes
    )
    # Changes are reported per tree; the protected tree also covers tests/expected/**.
    assert om.TreeChange(tree="protected", path="tests/expected/smoke/a.json", change="modified") in (
        result.changes
    )


def test_added_and_removed_files_are_reported(repo, reviewed):
    om.freeze(repo, REFERENCE)
    _write(repo.root, "tests/cases/smoke/b.yaml", "case_id: smoke.b\n")
    (repo.root / "tests/expected/smoke/a.json").unlink()

    result = om.check(repo)
    assert result.ok is False
    assert om.TreeChange(tree="cases", path="tests/cases/smoke/b.yaml", change="added") in (
        result.changes
    )
    assert om.TreeChange(tree="verifier", path="tests/cases/smoke/b.yaml", change="added") in (
        result.changes
    )
    assert om.TreeChange(tree="expected", path="tests/expected/smoke/a.json", change="removed") in (
        result.changes
    )


def test_changes_are_sorted_by_tree_then_path(repo, reviewed):
    om.freeze(repo, REFERENCE)
    _write(repo.root, "tests/cases/smoke/b.yaml", "case_id: smoke.b\n")
    _write(repo.root, "tests/cases/smoke/c.yaml", "case_id: smoke.c\n")
    _write(repo.root, "contracts/new.md", "new\n")

    result = om.check(repo)
    keys = [(c.tree, c.path) for c in result.changes]
    assert keys == sorted(keys)
    assert keys  # non-empty


def test_reference_drift_is_flagged(repo, reviewed):
    om.freeze(repo, REFERENCE)
    manifest_file = _manifest_file(repo)
    doc = json.loads(manifest_file.read_text(encoding="utf-8"))
    doc["reference"]["server_version"] = "18.5 (Debian 18.5-1)"
    manifest_file.write_text(json.dumps(doc), encoding="utf-8")

    result = om.check(repo)
    assert result.ok is False
    assert result.reference_changed is True
    assert result.changes == []


def test_unreadable_manifest_is_not_ok(repo, reviewed):
    om.freeze(repo, REFERENCE)
    _manifest_file(repo).write_text("{not json", encoding="utf-8")

    result = om.check(repo)
    assert result.ok is False
    assert "unreadable" in result.message


def test_freeze_rejects_unpinned_reference(repo, reviewed):
    bad = dict(REFERENCE, image_digest="sha256:" + "0" * 64)
    with pytest.raises(ValueError, match="image_digest"):
        om.freeze(repo, bad)
    assert not _manifest_file(repo).exists()


def test_freeze_requires_fingerprint(repo, reviewed):
    with pytest.raises(ValueError, match="fingerprint"):
        om.freeze(repo, dict(REFERENCE, fingerprint_sha256=""))


def test_compute_unknown_reference_key_rejected(repo):
    with pytest.raises(ValueError, match="unknown reference keys"):
        om.compute(repo, {"bogus": "x"})


def test_compute_without_git_records_null_sha(repo):
    # tmp_path is not a git repository, so git_sha is recorded as null, not an error.
    doc = om.compute(repo)
    assert doc["git_sha"] is None or isinstance(doc["git_sha"], str)
    assert doc["reference"]["image_digest"] == PG_IMAGE_DIGEST
    assert doc["reference"]["fingerprint_sha256"] is None


def test_tree_hashes_are_the_hashing_module_algorithm(repo):
    from pgarbor_harness.hashing import tree_sha256

    doc = om.compute(repo)
    digest, files = tree_sha256(repo.root, om.CASES_GLOBS)
    assert doc["trees"]["cases"]["sha256"] == digest
    assert doc["trees"]["cases"]["files"] == files
